use core::{
    alloc::{GlobalAlloc, Layout},
    ptr::NonNull,
    sync::atomic::Ordering,
};

use lock_api::{Mutex, RawMutex};
use x86::current::paging::PAddr;

use crate::{
    mm::{
        pmm::{self, FRAME_SIZE, MAX_ORDER},
        vmm::HHDM_OFFSET,
    },
    utils::IntLock,
};

const MIN_BUCKET_SHIFT: usize = 4; // 16 B
const MAX_BUCKET_SHIFT: usize = 10; // 1024 B
const NUM_BUCKETS: usize = MAX_BUCKET_SHIFT - MIN_BUCKET_SHIFT + 1;

const SLAB_FRAME_SIZE: usize = FRAME_SIZE as usize;
const SLAB_MASK: usize = !(SLAB_FRAME_SIZE - 1);

const fn make_obj_sizes() -> [u32; NUM_BUCKETS] {
    let mut arr = [0u32; NUM_BUCKETS];
    let mut i = 0;
    while i < NUM_BUCKETS {
        arr[i] = 1u32 << (MIN_BUCKET_SHIFT + i);
        i += 1;
    }
    arr
}
const OBJ_SIZE: [u32; NUM_BUCKETS] = make_obj_sizes();

#[global_allocator]
pub static ALLOCATOR: Heap = Heap::new();

#[derive(Debug)]
pub enum HeapError {}

pub fn init() -> Result<(), HeapError> {
    debug_assert!(HHDM_OFFSET.load(Ordering::Relaxed) != 0);
    Ok(())
}

#[repr(C)]
struct FreeObj {
    next: Option<NonNull<FreeObj>>,
}

#[repr(C)]
struct Slab {
    next: Option<NonNull<Slab>>,
    free: Option<NonNull<FreeObj>>,
}

struct HeapInner {
    active: [Option<NonNull<Slab>>; NUM_BUCKETS],
    partial: [Option<NonNull<Slab>>; NUM_BUCKETS],
}

unsafe impl Send for HeapInner {}

pub struct Heap {
    inner: Mutex<IntLock, HeapInner>,
}

impl Heap {
    const fn new() -> Self {
        Self {
            inner: Mutex::const_new(
                <IntLock as RawMutex>::INIT,
                HeapInner {
                    active: [None; NUM_BUCKETS],
                    partial: [None; NUM_BUCKETS],
                },
            ),
        }
    }
}

unsafe impl GlobalAlloc for Heap {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(bucket) = bucket_for(layout) else {
            return alloc_large(layout);
        };

        self.alloc_small(bucket)
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let Some(bucket) = bucket_for(layout) else {
            return dealloc_large(ptr, layout);
        };

        let slab_ptr = ((ptr as usize) & SLAB_MASK) as *mut Slab;
        self.dealloc_small(bucket, slab_ptr, ptr);
    }
}

impl Heap {
    fn alloc_small(&self, bucket: usize) -> *mut u8 {
        let mut inner = self.inner.lock();

        unsafe {
            if let Some(active) = inner.active[bucket]
                && let Some(obj) = (*active.as_ptr()).free
            {
                (*active.as_ptr()).free = (*obj.as_ptr()).next;
                return obj.as_ptr() as *mut u8;
            }
        }

        inner.active[bucket] = None;

        let slab_nn = match inner.take_partial(bucket) {
            Some(s) => Some(s),
            None => refill(bucket),
        };

        unsafe {
            match slab_nn {
                Some(slab_nn) => {
                    inner.active[bucket] = Some(slab_nn);
                    let slab = slab_nn.as_ptr();
                    let obj = (*slab).free.unwrap_unchecked();
                    (*slab).free = (*obj.as_ptr()).next;
                    obj.as_ptr() as *mut u8
                }
                None => core::ptr::null_mut(),
            }
        }
    }

    fn dealloc_small(&self, bucket: usize, slab_ptr: *mut Slab, ptr: *mut u8) {
        let mut inner = self.inner.lock();
        unsafe {
            let was_full = (*slab_ptr).free.is_none();
            let obj = ptr as *mut FreeObj;
            (*obj).next = (*slab_ptr).free;
            (*slab_ptr).free = Some(NonNull::new_unchecked(obj));

            if inner.active[bucket].is_some_and(|active| active.as_ptr() == slab_ptr) {
                return;
            }

            if was_full {
                (*slab_ptr).next = inner.partial[bucket];
                inner.partial[bucket] = Some(NonNull::new_unchecked(slab_ptr));
            }
        }
    }
}

impl HeapInner {
    #[inline]
    fn take_partial(&mut self, bucket: usize) -> Option<NonNull<Slab>> {
        let slab = self.partial[bucket]?;
        unsafe {
            self.partial[bucket] = (*slab.as_ptr()).next;
            (*slab.as_ptr()).next = None;
        }
        Some(slab)
    }
}

#[cold]
fn refill(bucket: usize) -> Option<NonNull<Slab>> {
    let obj_size = OBJ_SIZE[bucket] as usize;
    let phys = pmm::alloc_pages(0)?;
    let base = phys_to_virt(phys);

    let header_size = core::mem::size_of::<Slab>();
    let data_off = header_size.next_multiple_of(obj_size);
    let count = (SLAB_FRAME_SIZE - data_off) / obj_size;
    debug_assert!(count > 0);

    let slab = base as *mut Slab;
    let mut head: Option<NonNull<FreeObj>> = None;
    unsafe {
        for i in (0..count).rev() {
            let obj_ptr = base.add(data_off + i * obj_size) as *mut FreeObj;
            (*obj_ptr).next = head;
            head = Some(NonNull::new_unchecked(obj_ptr));
        }
        (*slab).next = None;
        (*slab).free = head;
        Some(NonNull::new_unchecked(slab))
    }
}

#[cold]
fn alloc_large(layout: Layout) -> *mut u8 {
    let Some(order) = order_for_large(layout) else {
        return core::ptr::null_mut();
    };
    match pmm::alloc_pages(order) {
        Some(phys) => phys_to_virt(phys),
        None => core::ptr::null_mut(),
    }
}

#[cold]
fn dealloc_large(ptr: *mut u8, layout: Layout) {
    if let Some(order) = order_for_large(layout) {
        let phys = virt_to_phys(ptr);
        pmm::free_pages(phys, order);
    }
}

#[inline(always)]
fn bucket_for(layout: Layout) -> Option<usize> {
    let need = layout.size().max(layout.align()).max(1 << MIN_BUCKET_SHIFT);
    if need > 1 << MAX_BUCKET_SHIFT {
        return None;
    }
    let pow = need.next_power_of_two();
    Some((pow.trailing_zeros() as usize) - MIN_BUCKET_SHIFT)
}

#[inline(always)]
fn order_for_large(layout: Layout) -> Option<usize> {
    let need = layout.size().max(layout.align());
    let frames = need.div_ceil(SLAB_FRAME_SIZE).next_power_of_two();
    let order = frames.trailing_zeros() as usize;
    if order > MAX_ORDER { None } else { Some(order) }
}

#[inline(always)]
fn hhdm() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

#[inline(always)]
fn phys_to_virt(p: PAddr) -> *mut u8 {
    (p.as_u64() + hhdm()) as *mut u8
}

#[inline(always)]
fn virt_to_phys(v: *mut u8) -> PAddr {
    PAddr(v as u64 - hhdm())
}
