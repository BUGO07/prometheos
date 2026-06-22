use core::ptr::NonNull;

use limine::memmap::{Entry, MEMMAP_BOOTLOADER_RECLAIMABLE};
use lock_api::{Mutex, RawMutex};
use x86::current::paging::PAddr;

use crate::{println, utils::IntLock};

pub const FRAME_SHIFT: u32 = 12;
pub const FRAME_SIZE: u64 = 1 << FRAME_SHIFT;
pub const MAX_ORDER: usize = 11; // 2MiB
const NUM_ORDERS: usize = MAX_ORDER + 1;
const ORDER_NONE: u8 = 0xFF;

#[inline]
fn frame_index(addr: PAddr) -> u64 {
    let raw = addr.as_u64();
    assert!(
        raw.is_multiple_of(FRAME_SIZE),
        "physical address is not frame-aligned: {:#x}",
        raw
    );
    raw >> FRAME_SHIFT
}

struct FreeBlock {
    prev: Option<NonNull<FreeBlock>>,
    next: Option<NonNull<FreeBlock>>,
}

pub struct Pmm {
    free_lists: [Option<NonNull<FreeBlock>>; NUM_ORDERS],
    frame_order: &'static mut [u8],
    total_frames: u64,
    free_frames: u64,
    hhdm_offset: u64,
}

unsafe impl Send for Pmm {}

static PMM: Mutex<IntLock, Option<Pmm>> = Mutex::const_new(IntLock::INIT, None);

pub fn install(pmm: Pmm) {
    let mut guard = PMM.lock();
    assert!(guard.is_none(), "PMM already installed");
    *guard = Some(pmm);
}

#[inline]
pub fn alloc_frame() -> Option<PAddr> {
    let mut g = PMM.lock();
    g.as_mut().unwrap().alloc_pages(0)
}

#[inline]
pub fn free_frame(addr: PAddr) {
    let mut g = PMM.lock();
    g.as_mut().unwrap().free_pages(addr, 0);
}

#[inline]
pub fn alloc_pages(order: usize) -> Option<PAddr> {
    let mut g = PMM.lock();
    g.as_mut().unwrap().alloc_pages(order)
}

#[inline]
pub fn free_pages(addr: PAddr, order: usize) {
    let mut g = PMM.lock();
    g.as_mut().unwrap().free_pages(addr, order);
}

pub fn stats() -> (u64, u64) {
    let g = PMM.lock();
    let p = g.as_ref().unwrap();
    (p.free_frames, p.total_frames)
}

pub fn reclaim_bootloader(memmap: &[&Entry]) {
    let mut g = PMM.lock();
    let pmm = g.as_mut().unwrap();
    {
        let mut reclaimed: u64 = 0;
        for entry in memmap {
            if entry.type_ != MEMMAP_BOOTLOADER_RECLAIMABLE {
                continue;
            }
            let first = entry.base >> FRAME_SHIFT;
            let count = entry.length >> FRAME_SHIFT;
            pmm.add_range(first, count);
            reclaimed += count;
        }
        println!(
            "reclaimed {} frames ({} KiB) from bootloader",
            reclaimed,
            reclaimed * FRAME_SIZE / 1024
        );
    }
}

impl Pmm {
    pub fn new(frame_order: &'static mut [u8], total_frames: u64, hhdm_offset: u64) -> Self {
        assert!(
            total_frames as usize == frame_order.len(),
            "frame_order metadata length must match total frame count"
        );
        frame_order.fill(ORDER_NONE);
        Self {
            free_lists: [None; NUM_ORDERS],
            frame_order,
            total_frames,
            free_frames: 0,
            hhdm_offset,
        }
    }

    #[inline]
    fn frame_to_block(&self, frame: u64) -> NonNull<FreeBlock> {
        let virt = (frame << FRAME_SHIFT) + self.hhdm_offset;
        unsafe { NonNull::new_unchecked(virt as *mut FreeBlock) }
    }

    #[inline]
    fn block_to_frame(&self, block: NonNull<FreeBlock>) -> u64 {
        (block.as_ptr() as u64 - self.hhdm_offset) >> FRAME_SHIFT
    }

    fn set_block_order(&mut self, frame: u64, order: usize, value: u8) {
        let pages = 1u64 << order;
        for i in 0..pages {
            self.frame_order[(frame + i) as usize] = value;
        }
    }

    fn assert_block_unused(&self, frame: u64, order: usize) {
        let pages = 1u64 << order;
        for i in 0..pages {
            assert_eq!(
                self.frame_order[(frame + i) as usize],
                ORDER_NONE,
                "free overlaps an existing free block at frame {}",
                frame + i
            );
        }
    }

    unsafe fn list_push(&mut self, frame: u64, order: usize) {
        debug_assert!(frame < self.total_frames);
        debug_assert!(frame + (1u64 << order) <= self.total_frames);
        self.assert_block_unused(frame, order);
        let block = self.frame_to_block(frame);
        let head = self.free_lists[order];
        unsafe {
            (*block.as_ptr()).prev = None;
            (*block.as_ptr()).next = head;
        }
        if let Some(h) = head {
            unsafe { (*h.as_ptr()).prev = Some(block) };
        }
        self.free_lists[order] = Some(block);
        self.set_block_order(frame, order, order as u8);
    }

    unsafe fn list_remove(&mut self, frame: u64, order: usize) {
        debug_assert!(frame < self.total_frames);
        debug_assert_eq!(self.frame_order[frame as usize], order as u8);
        let block = self.frame_to_block(frame);
        let prev = unsafe { (*block.as_ptr()).prev };
        let next = unsafe { (*block.as_ptr()).next };
        match prev {
            Some(p) => unsafe { (*p.as_ptr()).next = next },
            None => self.free_lists[order] = next,
        }
        if let Some(n) = next {
            unsafe { (*n.as_ptr()).prev = prev };
        }
        self.set_block_order(frame, order, ORDER_NONE);
    }

    pub fn free_frames_count(&self) -> u64 {
        self.free_frames
    }

    pub fn alloc_pages(&mut self, order: usize) -> Option<PAddr> {
        assert!(order <= MAX_ORDER, "order out of range: {}", order);

        let mut current = order;
        while current <= MAX_ORDER {
            if let Some(head) = self.free_lists[current] {
                let frame = self.block_to_frame(head);
                unsafe { self.list_remove(frame, current) };

                // split surplus halves down to requested order
                let mut o = current;
                while o > order {
                    o -= 1;
                    let buddy = frame + (1u64 << o);
                    unsafe { self.list_push(buddy, o) };
                }

                self.free_frames -= 1u64 << order;
                return Some(PAddr(frame << FRAME_SHIFT));
            }
            current += 1;
        }
        None
    }

    pub fn free_pages(&mut self, addr: PAddr, order: usize) {
        assert!(order <= MAX_ORDER, "order out of range: {}", order);
        let frame = frame_index(addr);
        assert!(
            frame + (1u64 << order) <= self.total_frames,
            "free out of range: {:#x}, order {}",
            addr.as_u64(),
            order
        );
        assert!(
            frame.is_multiple_of(1u64 << order),
            "free_pages address {:#x} not aligned to order {}",
            addr.as_u64(),
            order
        );
        assert_eq!(
            self.frame_order[frame as usize], ORDER_NONE,
            "double free at frame {}",
            frame
        );
        self.assert_block_unused(frame, order);

        let mut frame = frame;
        let mut o = order;
        self.free_frames += 1u64 << order;

        while o < MAX_ORDER {
            let buddy = frame ^ (1u64 << o);
            if buddy >= self.total_frames || self.frame_order[buddy as usize] != o as u8 {
                break;
            }
            unsafe { self.list_remove(buddy, o) };
            frame &= !(1u64 << o);
            o += 1;
        }
        unsafe { self.list_push(frame, o) };
    }

    pub fn add_range(&mut self, mut start: u64, mut count: u64) {
        while count > 0 {
            let align_order = if start == 0 {
                MAX_ORDER as u32
            } else {
                start.trailing_zeros()
            };
            let size_order = 63 - count.leading_zeros();
            let order = (align_order.min(size_order) as usize).min(MAX_ORDER);

            self.free_pages(PAddr(start << FRAME_SHIFT), order);
            let block = 1u64 << order;
            start += block;
            count -= block;
        }
    }
}
