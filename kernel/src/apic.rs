use x86::{
    apic::{ioapic::IoApic, x2apic::X2APIC},
    current::paging::{PAddr, PTFlags},
    irq,
    msr::{IA32_X2APIC_APICID, IA32_X2APIC_EOI, rdmsr, wrmsr},
};

use crate::{
    idt::install_handler,
    mm::vmm::{self, MapError, PageSize},
    println,
};

pub fn init() -> Result<(), ApicError> {
    println!("init");

    println!("mask legacy pic");
    unsafe {
        x86::io::outb(0x21, 0xFF);
        x86::io::outb(0xA1, 0xFF);
    }

    println!("init lapic");
    let mut lapic = X2APIC::new();
    lapic.attach();
    let bsp_id = unsafe { rdmsr(IA32_X2APIC_APICID) } as u32;
    println!("bsp id - {bsp_id}");

    install_handler(0x21, |_| {
        let scancode = unsafe { x86::io::inb(0x60) };
        println!("keyboard scancode: {scancode:#04x}");
        unsafe { wrmsr(IA32_X2APIC_EOI, 0) };
    });

    let (phys, base_gsi) = find_ioapic()?;
    let kb_gsi = resolve_isa_irq(1);
    assert!(
        kb_gsi >= base_gsi,
        "keyboard gsi {kb_gsi:#x} below ioapic base {base_gsi:#x}"
    );
    println!("found ioapic {phys:#x}, keyboard gsi={kb_gsi:#x}");

    let virt = vmm::map_hhdm(
        phys,
        PTFlags::P | PTFlags::RW | PTFlags::XD | PTFlags::PCD | PTFlags::PWT,
        PageSize::Base,
    )
    .map_err(ApicError::CouldNotMap)?;
    let mut ioapic = unsafe { IoApic::new(virt.as_usize()) };
    ioapic.enable((kb_gsi - base_gsi) as u8, bsp_id as u8);

    unsafe { irq::enable() };
    println!("done");
    Ok(())
}

#[derive(Debug)]
pub enum ApicError {
    AcpiTableNotFound,
    IoApicNotFound,
    #[allow(dead_code)]
    CouldNotMap(MapError),
}

fn find_ioapic() -> Result<(PAddr, u32), ApicError> {
    let mut table = uacpi_sys::uacpi_table::default();
    if unsafe { uacpi_sys::uacpi_table_find_by_signature(c"APIC".as_ptr(), &mut table) }
        != uacpi_sys::UACPI_STATUS_OK
    {
        return Err(ApicError::AcpiTableNotFound);
    }

    let mut out = None;
    unsafe {
        uacpi_sys::uacpi_for_each_subtable(
            table.__bindgen_anon_1.hdr,
            size_of::<uacpi_sys::acpi_madt>(),
            Some(ioapic_cb),
            &raw mut out as _,
        )
    };
    let _ = unsafe { uacpi_sys::uacpi_table_unref(&mut table) };
    out.ok_or(ApicError::IoApicNotFound)
}

unsafe extern "C" fn ioapic_cb(
    user: uacpi_sys::uacpi_handle,
    hdr: *mut uacpi_sys::acpi_entry_hdr,
) -> uacpi_sys::uacpi_iteration_decision {
    if unsafe { (*hdr).type_ } as u32 == uacpi_sys::ACPI_MADT_ENTRY_TYPE_IOAPIC {
        let ioapic = unsafe { &*(hdr as *const uacpi_sys::acpi_madt_ioapic) };
        let out = unsafe { &mut *(user as *mut Option<(PAddr, u32)>) };
        *out = Some((PAddr(ioapic.address as u64), ioapic.gsi_base));
        return uacpi_sys::UACPI_ITERATION_DECISION_BREAK;
    }
    uacpi_sys::UACPI_ITERATION_DECISION_CONTINUE
}

struct IsoCtx {
    source: u8,
    gsi: u32,
}

fn resolve_isa_irq(source: u8) -> u32 {
    let mut table = uacpi_sys::uacpi_table::default();
    if unsafe { uacpi_sys::uacpi_table_find_by_signature(c"APIC".as_ptr(), &mut table) }
        != uacpi_sys::UACPI_STATUS_OK
    {
        return source as u32;
    }

    let mut ctx = IsoCtx {
        source,
        gsi: source as u32,
    };

    unsafe {
        uacpi_sys::uacpi_for_each_subtable(
            table.__bindgen_anon_1.hdr,
            size_of::<uacpi_sys::acpi_madt>(),
            Some(iso_cb),
            &raw mut ctx as _,
        )
    };
    let _ = unsafe { uacpi_sys::uacpi_table_unref(&mut table) };
    ctx.gsi
}

unsafe extern "C" fn iso_cb(
    user: uacpi_sys::uacpi_handle,
    hdr: *mut uacpi_sys::acpi_entry_hdr,
) -> uacpi_sys::uacpi_iteration_decision {
    if unsafe { (*hdr).type_ } as u32 == uacpi_sys::ACPI_MADT_ENTRY_TYPE_INTERRUPT_SOURCE_OVERRIDE {
        let iso = unsafe { &*(hdr as *const uacpi_sys::acpi_madt_interrupt_source_override) };
        let ctx = unsafe { &mut *(user as *mut IsoCtx) };
        if iso.source == ctx.source {
            ctx.gsi = iso.gsi;
            return uacpi_sys::UACPI_ITERATION_DECISION_BREAK;
        }
    }
    uacpi_sys::UACPI_ITERATION_DECISION_CONTINUE
}
