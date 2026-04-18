core::arch::global_asm!(include_str!("isr.S"));

use core::sync::atomic::{AtomicPtr, Ordering};

use x86::{
    Ring,
    bits64::segmentation::Descriptor64,
    debugregs::{Dr6, dr6_write},
    dtables::{DescriptorTablePointer, lidt},
    segmentation::{BuildDescriptor, DescriptorBuilder, GateDescriptorBuilder, SegmentSelector},
};

use crate::{println, utils::Singleton};

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
            println!("{}: {:#x?}", x86::irq::EXCEPTIONS[vec], frame);
            unsafe { dr6_write(Dr6::empty()) };
        }
        2 => {}
        3 => {
            println!("{}: {:#x?}", x86::irq::EXCEPTIONS[vec], frame);
        }
        4..32 => {
            panic!("{}: {:#x?}", x86::irq::EXCEPTIONS[vec], frame);
        }
        0x80 => {
            println!("syscall: {:#x?}", frame);
        }
        _ => {
            let ptr = HANDLERS[vec].load(Ordering::Acquire);
            if ptr.is_null() {
                println!("unhandled interrupt: {:#x?}", frame);
            } else {
                let handler: fn(&mut InterruptFrame) = unsafe { core::mem::transmute(ptr) };
                handler(frame);
            }
        }
    }
}

static IDT: Singleton<[Descriptor64; 256]> = Singleton::new();
static HANDLERS: [AtomicPtr<()>; 256] = [const { AtomicPtr::new(core::ptr::null_mut()) }; 256];

const DOUBLE_FAULT_IST: u8 = 1;

pub fn init() {
    println!("initializing");
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
    println!("loading");
    unsafe { lidt(&idt_ptr) };
    println!("initialized");
}

pub fn install_handler(vector: u8, handler: fn(&mut InterruptFrame)) {
    HANDLERS[vector as usize].store(handler as *const () as *mut (), Ordering::Release);
}

pub fn clear_handler(vector: u8) {
    HANDLERS[vector as usize].store(core::ptr::null_mut(), Ordering::Release);
}
