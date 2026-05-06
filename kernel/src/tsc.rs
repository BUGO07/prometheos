use core::sync::atomic::{AtomicU64, Ordering};

use x86::{
    cpuid::CpuId,
    io::{inb, outb},
    time::rdtsc,
};

use crate::println;

const PIT_FREQ: u64 = 1_193_182;

pub static TSC_FREQUENCY: AtomicU64 = AtomicU64::new(0);
static BOOT_TSC: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum TscError {
    NotInvariant,
}

pub fn init() -> Result<(), TscError> {
    println!("init");

    let cpuid = CpuId::new();
    if !cpuid
        .get_advanced_power_mgmt_info()
        .is_some_and(|info| info.has_invariant_tsc())
    {
        return Err(TscError::NotInvariant);
    }

    let freq = cpuid
        .get_tsc_info()
        .and_then(|info| info.tsc_frequency())
        .filter(|&h| h > 0)
        .unwrap_or_else(calibrate_with_pit);
    TSC_FREQUENCY.store(freq, Ordering::Release);
    BOOT_TSC.store(unsafe { rdtsc() }, Ordering::Release);

    println!("done ({freq}hz)");
    Ok(())
}

fn calibrate_with_pit() -> u64 {
    const RELOAD: u16 = 0xFFFF;

    unsafe {
        let p61 = inb(0x61) & !0x03;
        outb(0x61, p61);

        outb(0x43, 0b1011_0000);
        outb(0x42, RELOAD as u8);
        outb(0x42, (RELOAD >> 8) as u8);

        let start = rdtsc();
        outb(0x61, p61 | 0x01);

        while inb(0x61) & 0x20 == 0 {}
        let end = rdtsc();

        outb(0x61, p61);

        (end - start) * PIT_FREQ / RELOAD as u64
    }
}

pub fn current_time_ns() -> u64 {
    let tsc = unsafe { rdtsc() };
    let boot = BOOT_TSC.load(Ordering::Relaxed);
    let hz = TSC_FREQUENCY.load(Ordering::Relaxed);
    ((tsc - boot) as u128 * 1_000_000_000 / hz as u128) as u64
}
