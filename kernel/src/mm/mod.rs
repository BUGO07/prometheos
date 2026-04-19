use limine::{
    memmap::MEMMAP_USABLE,
    request::{HhdmRequest, MemmapRequest},
};

use crate::{
    mm::{
        pmm::{Bitmap, FRAME_SHIFT, FRAME_SIZE, PMM, Pmm},
        vmm::{VMM, Vmm},
    },
    println,
};

pub mod pmm;
pub mod vmm;

#[used]
#[unsafe(link_section = ".requests")]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

pub fn init() {
    println!("init");

    let memmap = MEMMAP_REQUEST
        .response()
        .expect("no memmap response")
        .entries();
    let hhdm_offset = HHDM_REQUEST.response().expect("no hhdm response").offset;

    println!("hhdm offset = {:#x}", hhdm_offset);

    let mut highest = 0;
    for entry in memmap {
        let end = entry.base + entry.length;
        if end > highest {
            highest = end;
        }
    }

    let total_frames = (highest + FRAME_SIZE - 1) >> FRAME_SHIFT;
    let bitmap_bytes = total_frames.div_ceil(8).next_multiple_of(FRAME_SIZE);

    println!(
        "highest phys = {:#x}, frames = {}, bitmap = {} KiB",
        highest,
        total_frames,
        bitmap_bytes / 1024
    );

    let mut bitmap_phys = u64::MAX;
    for entry in memmap {
        if entry.type_ == MEMMAP_USABLE && entry.length >= bitmap_bytes {
            bitmap_phys = entry.base;
            break;
        }
    }
    assert!(
        bitmap_phys != u64::MAX,
        "no usable region large enough for bitmap"
    );

    let mut bitmap = Bitmap::new((bitmap_phys + hhdm_offset) as *mut u8, bitmap_bytes);
    bitmap.buf.fill(0xFF);

    let mut free_frames = 0;
    for entry in memmap {
        if entry.type_ != MEMMAP_USABLE {
            continue;
        }
        let first = entry.base >> FRAME_SHIFT;
        let count = entry.length >> FRAME_SHIFT;
        bitmap.clear_range(first, count);
        free_frames += count;
    }

    let bitmap_first = bitmap_phys >> FRAME_SHIFT;
    let bitmap_frames = bitmap_bytes.div_ceil(FRAME_SIZE);
    bitmap.set_range(bitmap_first, bitmap_frames);
    free_frames -= bitmap_frames;

    if !bitmap.read(0) {
        bitmap.set(0);
        free_frames -= 1;
    }

    PMM.install(Pmm::new(bitmap, total_frames, free_frames));

    println!(
        "free = {} frames ({} MiB)",
        free_frames,
        free_frames * FRAME_SIZE / 1024 / 1024
    );

    let top_level = vmm::take_ownership(hhdm_offset);
    println!("installing vmm ({:#x})", top_level.as_u64());
    VMM.install(Vmm::new(top_level, hhdm_offset));
    println!("done");
}
