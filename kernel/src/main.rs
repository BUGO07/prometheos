#![no_std]
#![no_main]

extern crate alloc;

use core::arch::asm;

use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker, request::FramebufferRequest};

mod acpi;
mod gdt;
mod idt;
mod mm;
mod serial;
mod utils;

#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests")]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".requests_start_marker")]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    assert!(BASE_REVISION.is_supported());

    serial::init();

    print!("\x1b[H\x1b[2J"); // clear screen
    println!("booting");

    gdt::init();
    idt::init();

    mm::init();

    acpi::init().unwrap();

    if let Some(framebuffer) = FRAMEBUFFER_REQUEST
        .response()
        .and_then(|res| res.framebuffers().first())
    {
        let fb = unsafe {
            core::slice::from_raw_parts_mut(
                framebuffer.address() as *mut u32,
                (framebuffer.width * framebuffer.height) as usize,
            )
        };
        for i in 0..100 {
            let pixel_offset = i * framebuffer.width as usize + i;
            fb[pixel_offset] = 0xFFFFFFFF;
        }
    }

    hcf();
}

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    println!("kernel panic: {}", info);
    hcf();
}

fn hcf() -> ! {
    loop {
        unsafe {
            #[cfg(target_arch = "x86_64")]
            asm!("hlt");
            #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
            asm!("wfi");
            #[cfg(target_arch = "loongarch64")]
            asm!("idle 0");
        }
    }
}
