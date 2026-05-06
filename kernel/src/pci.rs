use core::sync::atomic::{AtomicU64, Ordering};

use alloc::vec::Vec;
use x86::{
    current::paging::{PAddr, PTFlags, VAddr},
    io,
};

use crate::{
    acpi::uacpi_status_to_result,
    mm::vmm::{self, HHDM_OFFSET, MapError, PageSize},
    println,
};

#[derive(Clone, Copy)]
pub struct PciAddress {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

pub static MCFG_ADDRESS: AtomicU64 = AtomicU64::new(0);

impl PciAddress {
    fn io_address(&self, offset: usize) -> u32 {
        let bus = self.bus as u32;
        let device = self.device as u32;
        let function = self.function as u32;
        let offset = offset as u32 & 0xFC;
        0x8000_0000 | (bus << 16) | (device << 11) | (function << 8) | offset
    }

    fn mmio_address(&self, offset: usize, mcfg: u64) -> *mut u32 {
        let bus = self.bus as u64;
        let device = self.device as u64;
        let function = self.function as u64;
        (mcfg + (bus << 20) + (device << 15) + (function << 12) + offset as u64) as _
    }
}

pub fn pci_read<T: TryFrom<u32>>(addr: PciAddress, offset: usize) -> T {
    assert!(matches!(size_of::<T>(), 1 | 2 | 4), "unsupported size");
    let mcfg = MCFG_ADDRESS.load(Ordering::Relaxed);
    if mcfg != 0 {
        unsafe { core::ptr::read_volatile(addr.mmio_address(offset, mcfg) as *const T) }
    } else {
        unsafe { io::outl(0xCF8, addr.io_address(offset)) };
        let port = 0xCFC + (offset as u16 & 3);
        let value: u32 = match size_of::<T>() {
            1 => unsafe { io::inb(port) as u32 },
            2 => unsafe { io::inw(port) as u32 },
            4 => unsafe { io::inl(port) },
            _ => unreachable!(),
        };
        T::try_from(value).ok().unwrap()
    }
}

pub fn pci_write<T: Into<u32>>(addr: PciAddress, offset: usize, value: T) {
    assert!(matches!(size_of::<T>(), 1 | 2 | 4), "unsupported size");
    let mcfg = MCFG_ADDRESS.load(Ordering::Relaxed);
    if mcfg != 0 {
        unsafe { core::ptr::write_volatile(addr.mmio_address(offset, mcfg) as *mut T, value) }
    } else {
        unsafe { io::outl(0xCF8, addr.io_address(offset)) };
        let port = 0xCFC + (offset as u16 & 3);
        match size_of::<T>() {
            1 => unsafe { io::outb(port, value.into() as u8) },
            2 => unsafe { io::outw(port, value.into() as u16) },
            4 => unsafe { io::outl(port, value.into()) },
            _ => unreachable!(),
        }
    }
}

pub fn enum_device(bus: u8, device: u8, funcs: &mut Vec<PciAddress>) {
    for function in 0..8 {
        let pciaddr = PciAddress {
            bus,
            device,
            function,
        };

        let vendor_device = pci_read::<u32>(pciaddr, 0x00);
        let vendor_id = (vendor_device & 0xFFFF) as u16;
        if vendor_id == 0xFFFF {
            if function == 0 {
                return;
            } else {
                continue;
            }
        }
        let device_id = ((vendor_device >> 16) & 0xFFFF) as u16;

        let class_info = pci_read::<u32>(pciaddr, 0x08);
        let class_code = ((class_info >> 24) & 0xFF) as u8;
        let subclass = ((class_info >> 16) & 0xFF) as u8;
        let prog_if = ((class_info >> 8) & 0xFF) as u8;

        let header_reg = pci_read::<u32>(pciaddr, 0x0C);
        let header_byte = ((header_reg >> 16) & 0xFF) as u8;
        let multifunction = header_byte & 0x80 != 0;
        let header_type = header_byte & 0x7F;

        let name = match (class_code, subclass, prog_if) {
            (0x01, 0x06, 0x01) => "AHCI storage controller",
            (0x01, 0x08, 0x02) => "NVMe storage device",
            (0x01, 0x01, _) => "IDE storage controller",
            (0x02, 0x00, _) => "Ethernet controller",
            (0x03, 0x00, _) => "VGA-compatible device",
            (0x03, 0x80, _) => "Other display device",
            (0x04, 0x03, _) => "Audio device",
            (0x06, _, _) => "Bridge device",
            (0x0C, 0x03, _) => "USB Controller",
            _ => "PCI device",
        };

        println!(
            "{:02x}:{:02x}.{} [{:04x}:{:04x}] class {:02x}:{:02x}:{:02x} {}",
            bus, device, function, vendor_id, device_id, class_code, subclass, prog_if, name
        );

        funcs.push(pciaddr);

        if class_code == 0x06 && matches!(subclass, 0x04 | 0x09) && header_type == 1 {
            let secondary_bus = (pci_read::<u32>(pciaddr, 0x18) >> 8) as u8;
            enum_bus(secondary_bus, funcs);
        }

        if function == 0 && !multifunction {
            break;
        }
    }
}

pub fn enum_bus(bus: u8, devices: &mut Vec<PciAddress>) {
    for device in 0..32 {
        enum_device(bus, device, devices);
    }
}

#[derive(Debug)]
pub enum PciError {
    #[allow(dead_code)]
    McfgNotFound(&'static str),
    #[allow(dead_code)]
    CouldNotMap(MapError),
}

pub fn init() -> Result<Vec<PciAddress>, PciError> {
    let mut table = uacpi_sys::uacpi_table::default();
    uacpi_status_to_result(unsafe {
        uacpi_sys::uacpi_table_find_by_signature(c"MCFG".as_ptr(), &mut table)
    })
    .map_err(PciError::McfgNotFound)?;

    let mcfg = unsafe { &*(table.__bindgen_anon_1.hdr as *const uacpi_sys::acpi_mcfg) };
    let entry = unsafe { &*mcfg.entries.as_ptr() };
    let phys = entry.address;
    let start_bus = entry.start_bus;
    let end_bus = entry.end_bus;

    let bus_count = end_bus as u64 - start_bus as u64 + 1;
    let bytes = bus_count << 20;
    let stride = PageSize::Large.bytes();
    let pages = bytes.div_ceil(stride);
    println!("found mcfg {phys:#x}, mapping {pages} pages");
    let hhdm = HHDM_OFFSET.load(Ordering::Relaxed);
    let virt = VAddr(phys + hhdm);
    vmm::map_range(
        virt,
        PAddr(phys),
        pages,
        PTFlags::P | PTFlags::RW | PTFlags::XD | PTFlags::PCD | PTFlags::PWT,
        PageSize::Large,
    )
    .map_err(PciError::CouldNotMap)?;

    MCFG_ADDRESS.store(virt.as_u64(), Ordering::Relaxed);
    let _ = unsafe { uacpi_sys::uacpi_table_unref(&mut table) };

    let mut devices = Vec::new();
    enum_bus(start_bus, &mut devices);
    Ok(devices)
}
