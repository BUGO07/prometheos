#![no_std]
#![no_main]

use core::arch::asm;

use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker, request::FramebufferRequest};

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

    if let Some(framebuffer_response) = FRAMEBUFFER_REQUEST.response()
        && let Some(framebuffer) = framebuffer_response.framebuffers().iter().next()
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
fn rust_panic(_info: &core::panic::PanicInfo) -> ! {
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
