use core::sync::atomic::Ordering;

use limine::{
    memmap::MEMMAP_USABLE,
    request::{HhdmRequest, MemmapRequest},
};

use crate::{
    mm::{
        pmm::{FRAME_SHIFT, FRAME_SIZE, Pmm},
        vmm::{HHDM_OFFSET, VMM, Vmm},
    },
    println,
};

pub mod heap;
pub mod pmm;
pub mod vmm;

#[used]
#[unsafe(link_section = ".requests")]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[derive(Debug)]
pub enum MmError {
    VmmError(vmm::VmmError),
    HeapError(heap::HeapError),
}

pub fn init() -> Result<(), MmError> {
    println!("init");

    let memmap = MEMMAP_REQUEST.response().unwrap().entries();
    let hhdm_offset = HHDM_REQUEST.response().unwrap().offset;

    println!("hhdm offset = {:#x}", hhdm_offset);

    let mut highest = 0;
    for entry in memmap {
        let end = entry.base + entry.length;
        if end > highest {
            highest = end;
        }
    }

    let total_frames = align_up(highest, FRAME_SIZE) >> FRAME_SHIFT;
    let order_bytes = align_up(total_frames, FRAME_SIZE);

    println!(
        "highest phys = {:#x}, frames = {}, frame_order = {} KiB",
        highest,
        total_frames,
        order_bytes / 1024
    );

    let mut order_phys = u64::MAX;
    for entry in memmap {
        if entry.type_ == MEMMAP_USABLE && entry.length >= order_bytes {
            order_phys = entry.base;
            break;
        }
    }
    assert!(
        order_phys != u64::MAX,
        "no usable region large enough for frame_order"
    );

    let frame_order = unsafe {
        core::slice::from_raw_parts_mut(
            (order_phys + hhdm_offset) as *mut u8,
            total_frames as usize,
        )
    };

    let mut pmm = Pmm::new(frame_order, total_frames, hhdm_offset);

    let order_first = order_phys >> FRAME_SHIFT;
    let order_last = order_first + (order_bytes >> FRAME_SHIFT);

    for entry in memmap {
        if entry.type_ != MEMMAP_USABLE {
            continue;
        }
        let entry_first = entry.base >> FRAME_SHIFT;
        let entry_last = (entry.base + entry.length) >> FRAME_SHIFT;

        // skip the slice owned by frame_order
        let pieces = [
            (entry_first, entry_last.min(order_first)),
            (entry_first.max(order_last), entry_last),
        ];
        for (mut s, e) in pieces {
            if e <= s {
                continue;
            }
            // never hand out frame 0 (null)
            if s == 0 {
                s = 1;
            }
            if e > s {
                pmm.add_range(s, e - s);
            }
        }
    }

    let free = pmm.free_frames_count();
    pmm::install(pmm);

    println!(
        "free = {} / {} frames ({} MiB)",
        free,
        total_frames,
        free * FRAME_SIZE / 1024 / 1024
    );

    let top_level = vmm::take_ownership(hhdm_offset).map_err(MmError::VmmError)?;
    println!("installing vmm ({:#x})", top_level.as_u64());
    HHDM_OFFSET.store(hhdm_offset, Ordering::Relaxed);
    VMM.install(Vmm::new(top_level));

    println!("setting up heap");
    heap::init().map_err(MmError::HeapError)?;
    println!("done");

    Ok(())
}

#[inline(always)]
pub const fn align_up(addr: u64, align: u64) -> u64 {
    assert!(align.is_power_of_two());
    (addr + align - 1) & !(align - 1)
}

#[inline(always)]
pub const fn align_down(addr: u64, align: u64) -> u64 {
    assert!(align.is_power_of_two());
    addr & !(align - 1)
}
