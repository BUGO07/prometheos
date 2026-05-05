use core::fmt::{self, Write};

use x86::io::{inb, outb};

use crate::utils::critical_section;

const COM1: u16 = 0x3F8;

#[allow(clippy::identity_op)]
pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // disable interrupts
        outb(COM1 + 3, 0x80); // enable DLAB
        outb(COM1 + 0, 0x03); // divisor low  (38400 baud)
        outb(COM1 + 1, 0x00); // divisor high
        outb(COM1 + 3, 0x03); // 8 bits, no parity, 1 stop
        outb(COM1 + 2, 0xC7); // enable + clear FIFO, 14-byte threshold
        outb(COM1 + 4, 0x0B); // IRQs enabled, RTS/DSR set
    }
}

fn transmit_ready() -> bool {
    unsafe { inb(COM1 + 5) & 0x20 != 0 }
}

fn write_byte(b: u8) {
    while !transmit_ready() {}
    unsafe { outb(COM1, b) };
}

pub struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        critical_section(|| {
            for b in s.bytes() {
                if b == b'\n' {
                    write_byte(b'\r');
                }
                write_byte(b);
            }
        });
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = core::write!($crate::serial::Serial, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}: {}\n", module_path!().split("::").last().unwrap(), core::format_args!($($arg)*)));
}
