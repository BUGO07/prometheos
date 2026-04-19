use core::{alloc::Layout, ptr::NonNull};

use talc::{
    TalcLock,
    base::{Talc, binning::Binning},
    source::Source,
};
use x86::current::paging::{BASE_PAGE_SIZE, PTFlags, VAddr};

use crate::{
    mm::{
        pmm,
        vmm::{self, PageSize},
    },
    utils::IntLock,
};

pub const HEAP_BASE: usize = 0xFFFF_9000_0000_0000;
pub const INITIAL_HEAP_SIZE: usize = 64 * 1024 * 1024;
pub const HEAP_LIMIT: usize = 1024 * 1024 * 1024;
pub const GROW_CHUNK: usize = 4 * 1024 * 1024;

#[global_allocator]
static ALLOCATOR: TalcLock<IntLock, HeapSource> = TalcLock::new(HeapSource::new());

pub fn init() {
    map_range(HEAP_BASE, INITIAL_HEAP_SIZE).unwrap();

    let mut talc = ALLOCATOR.lock();
    let end = unsafe { talc.claim(HEAP_BASE as *mut u8, INITIAL_HEAP_SIZE).unwrap() };
    talc.source.heap_end = Some(end);
}

#[derive(Debug)]
pub struct HeapSource {
    next: usize,
    heap_end: Option<NonNull<u8>>,
}

unsafe impl Send for HeapSource {}

impl HeapSource {
    pub const fn new() -> Self {
        Self {
            next: HEAP_BASE + INITIAL_HEAP_SIZE,
            heap_end: None,
        }
    }
}

unsafe impl Source for HeapSource {
    fn acquire<B: Binning>(talc: &mut Talc<Self, B>, layout: Layout) -> Result<(), ()> {
        let start = talc.source.next;
        let heap_end = talc.source.heap_end.ok_or(())?;

        let grow = layout
            .size()
            .max(GROW_CHUNK)
            .next_multiple_of(BASE_PAGE_SIZE);
        let end = start.checked_add(grow).ok_or(())?;
        if end > HEAP_BASE + HEAP_LIMIT {
            return Err(());
        }

        map_range(start, grow)?;

        let new_end = end as *mut u8;
        let updated = unsafe { talc.extend(heap_end, new_end) };
        debug_assert_eq!(updated.as_ptr(), new_end);

        talc.source.next = end;
        talc.source.heap_end = Some(updated);
        Ok(())
    }
}

fn rollback(start: usize, mapped: usize) {
    for off in (0..mapped).step_by(BASE_PAGE_SIZE) {
        let virt = VAddr::from_usize(start + off);
        if let Some((phys, _)) = vmm::translate(virt)
            && vmm::unmap(virt, PageSize::Base).is_ok()
        {
            pmm::free_frame(phys);
        }
    }
}

fn map_range(start: usize, size: usize) -> Result<(), ()> {
    let mut mapped = 0usize;
    while mapped < size {
        let Some(frame) = pmm::alloc_frame() else {
            rollback(start, mapped);
            return Err(());
        };
        if vmm::map(
            VAddr::from_usize(start + mapped),
            frame,
            PTFlags::RW | PTFlags::XD,
            PageSize::Base,
        )
        .is_err()
        {
            pmm::free_frame(frame);
            rollback(start, mapped);
            return Err(());
        }
        mapped += BASE_PAGE_SIZE;
    }
    Ok(())
}
