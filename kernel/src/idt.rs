core::arch::global_asm!(include_str!("isr.S"));

use alloc::boxed::Box;
use lock_api::{Mutex, RawMutex};
use x86::{
    Ring, controlregs,
    current::segmentation::Descriptor64,
    debugregs::{Dr6, dr6_write},
    dtables::{DescriptorTablePointer, lidt},
    irq,
    segmentation::{BuildDescriptor, DescriptorBuilder, GateDescriptorBuilder, SegmentSelector},
};

use crate::{
    println,
    utils::{Singleton, SpinLock},
};

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

unsafe extern "C" {
    static isr_table: [unsafe extern "C" fn(); 256];
}

#[unsafe(no_mangle)]
pub extern "C" fn isr_handler(frame: &mut InterruptFrame) {
    let vec = frame.vector as usize;
    match vec {
        1 => {
            println!("{}: {:#x?}", irq::EXCEPTIONS[vec], frame);
            unsafe { dr6_write(Dr6::from_bits_truncate(0xFFFF_0FF0)) };
        }
        2 => {}
        3 => {
            println!("{}: {:#x?}", irq::EXCEPTIONS[vec], frame);
        }
        14 => {
            let cr2 = unsafe { controlregs::cr2() };
            panic!("{} at cr2={:#x}: {:#x?}", irq::EXCEPTIONS[vec], cr2, frame);
        }
        0 | 4..32 => {
            panic!("{}: {:#x?}", irq::EXCEPTIONS[vec], frame);
        }
        0x80 => {
            println!("syscall: {:#x?}", frame);
        }
        _ => {
            let mut slot = HANDLERS[vec].lock();
            if let Some(handler) = slot.as_mut() {
                handler(frame);
            } else {
                println!("unhandled interrupt: {:#x?}", frame);
            }
        }
    }
}

type Handler = Box<dyn FnMut(&mut InterruptFrame) + Send>;

static IDT: Singleton<[Descriptor64; 256]> = Singleton::new();
static HANDLERS: [Mutex<SpinLock, Option<Handler>>; 256] =
    [const { Mutex::const_new(SpinLock::INIT, None) }; 256];

const DOUBLE_FAULT_IST: u8 = 1;

pub fn init() {
    println!("init");
    let mut idt = [Descriptor64::NULL; 256];
    for i in 0..256 {
        idt[i] = <DescriptorBuilder as GateDescriptorBuilder<u64>>::interrupt_descriptor(
            SegmentSelector::new(1, Ring::Ring0),
            unsafe { isr_table[i] } as *const () as u64,
        )
        .dpl(if i == 0x80 { Ring::Ring3 } else { Ring::Ring0 })
        .ist(if i == 8 { DOUBLE_FAULT_IST } else { 0 })
        .present()
        .finish();
    }
    IDT.install(idt);
    let idt_ptr = IDT.with(|idt| DescriptorTablePointer::new(idt));
    println!("lidt");
    unsafe { lidt(&idt_ptr) };
    println!("done");
}

pub fn is_handler_installed(vector: u8) -> bool {
    HANDLERS[vector as usize].lock().is_some()
}

pub fn install_handler<F>(vector: u8, handler: F)
where
    F: FnMut(&mut InterruptFrame) + Send + 'static,
{
    let handler: Handler = Box::new(handler);
    *HANDLERS[vector as usize].lock() = Some(handler);
}

pub fn clear_handler(vector: u8) {
    *HANDLERS[vector as usize].lock() = None;
}
