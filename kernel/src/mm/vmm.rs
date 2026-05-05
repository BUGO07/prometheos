use core::sync::atomic::{AtomicU64, Ordering};

use crate::{mm::pmm, utils::Singleton};

use x86::{
    controlregs,
    current::paging::{
        BASE_PAGE_SIZE, HUGE_PAGE_SIZE, LARGE_PAGE_SIZE, PAGE_SIZE_ENTRIES, PAddr, PD, PDEntry,
        PDFlags, PDPT, PDPTEntry, PDPTFlags, PML4, PML4Entry, PML4Flags, PT, PTEntry, PTFlags,
        VAddr, pd_index, pdpt_index, pml4_index, pt_index,
    },
    tlb,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PageSize {
    Base,
    Large,
    Huge,
}

impl PageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            Self::Base => BASE_PAGE_SIZE as u64,
            Self::Large => LARGE_PAGE_SIZE as u64,
            Self::Huge => HUGE_PAGE_SIZE as u64,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MapError {
    AlreadyMapped,
    OutOfMemory,
    NonCanonical,
    Misaligned,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum UnmapError {
    NotMapped,
    NonCanonical,
    Misaligned,
}

pub static VMM: Singleton<Vmm> = Singleton::new();
pub static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

pub struct Vmm {
    top_level: PAddr,
}

impl Vmm {
    pub fn new(top_level: PAddr) -> Self {
        Self { top_level }
    }

    pub fn translate(&self, virt: VAddr) -> Option<(PAddr, PTFlags)> {
        if !is_canonical(virt) {
            return None;
        }
        unsafe {
            let hhdm = HHDM_OFFSET.load(Ordering::Relaxed);

            let pml4 = table_mut::<PML4>(self.top_level, hhdm);
            let pml4e = (*pml4)[pml4_index(virt)];
            if !pml4e.is_present() {
                return None;
            }

            let pdpt_phys = PAddr(pml4e.address().as_u64());
            let pdpt = table_mut::<PDPT>(pdpt_phys, hhdm);
            let pdpte = (*pdpt)[pdpt_index(virt)];
            if !pdpte.is_present() {
                return None;
            }
            if pdpte.is_page() {
                let phys = pdpte.address().as_u64() + (virt.as_u64() & (HUGE_PAGE_SIZE as u64 - 1));
                return Some((PAddr(phys), PTFlags::from_bits_truncate(pdpte.0)));
            }

            let pd_phys = PAddr(pdpte.address().as_u64());
            let pd = table_mut::<PD>(pd_phys, hhdm);
            let pde = (*pd)[pd_index(virt)];
            if !pde.is_present() {
                return None;
            }
            if pde.is_page() {
                let phys = pde.address().as_u64() + (virt.as_u64() & (LARGE_PAGE_SIZE as u64 - 1));
                return Some((PAddr(phys), PTFlags::from_bits_truncate(pde.0)));
            }

            let pt_phys = PAddr(pde.address().as_u64());
            let pt = table_mut::<PT>(pt_phys, hhdm);
            let pte = (*pt)[pt_index(virt)];
            if !pte.is_present() {
                return None;
            }
            let phys = pte.address().as_u64() + (virt.as_u64() & (BASE_PAGE_SIZE as u64 - 1));
            Some((PAddr(phys), pte.flags()))
        }
    }

    pub fn map(
        &mut self,
        virt: VAddr,
        phys: PAddr,
        flags: PTFlags,
        size: PageSize,
    ) -> Result<(), MapError> {
        if !is_canonical(virt) {
            return Err(MapError::NonCanonical);
        }
        let align = size.bytes();
        if !virt.as_u64().is_multiple_of(align) || !phys.as_u64().is_multiple_of(align) {
            return Err(MapError::Misaligned);
        }

        unsafe {
            let hhdm = HHDM_OFFSET.load(Ordering::Relaxed);

            let pml4 = table_mut::<PML4>(self.top_level, hhdm);
            let pml4_slot = &mut (*pml4)[pml4_index(virt)];
            let mut new_pdpt = false;
            let pdpt_phys = if pml4_slot.is_present() {
                PAddr(pml4_slot.address().as_u64())
            } else {
                let frame = pmm::alloc_frame().ok_or(MapError::OutOfMemory)?;
                zero_frame(frame, hhdm);
                *pml4_slot = PML4Entry::new(frame, pml4_parent());
                new_pdpt = true;
                frame
            };

            let pdpt = table_mut::<PDPT>(pdpt_phys, hhdm);
            let pdpt_slot = &mut (*pdpt)[pdpt_index(virt)];

            if size == PageSize::Huge {
                if pdpt_slot.is_present() {
                    return Err(MapError::AlreadyMapped);
                }
                let huge = pdpt_leaf(flags);
                *pdpt_slot = PDPTEntry::new(phys, huge);
                tlb::flush(virt.as_u64() as usize);
                return Ok(());
            }

            if pdpt_slot.is_present() && pdpt_slot.is_page() {
                return Err(MapError::AlreadyMapped);
            }
            let mut new_pd = false;
            let pd_phys = if pdpt_slot.is_present() {
                PAddr(pdpt_slot.address().as_u64())
            } else {
                let frame = match pmm::alloc_frame() {
                    Some(f) => f,
                    None => {
                        if new_pdpt {
                            *pml4_slot = PML4Entry(0);
                            pmm::free_frame(pdpt_phys);
                        }
                        return Err(MapError::OutOfMemory);
                    }
                };
                zero_frame(frame, hhdm);
                *pdpt_slot = PDPTEntry::new(frame, pdpt_parent());
                new_pd = true;
                frame
            };

            let pd = table_mut::<PD>(pd_phys, hhdm);
            let pd_slot = &mut (*pd)[pd_index(virt)];

            if size == PageSize::Large {
                if pd_slot.is_present() {
                    return Err(MapError::AlreadyMapped);
                }
                let large = pd_leaf(flags);
                *pd_slot = PDEntry::new(phys, large);
                tlb::flush(virt.as_u64() as usize);
                return Ok(());
            }

            if pd_slot.is_present() && pd_slot.is_page() {
                return Err(MapError::AlreadyMapped);
            }
            let pt_phys = if pd_slot.is_present() {
                PAddr(pd_slot.address().as_u64())
            } else {
                let frame = match pmm::alloc_frame() {
                    Some(f) => f,
                    None => {
                        if new_pd {
                            *pdpt_slot = PDPTEntry(0);
                            pmm::free_frame(pd_phys);
                        }
                        if new_pdpt {
                            *pml4_slot = PML4Entry(0);
                            pmm::free_frame(pdpt_phys);
                        }
                        return Err(MapError::OutOfMemory);
                    }
                };
                zero_frame(frame, hhdm);
                *pd_slot = PDEntry::new(frame, pd_parent());
                frame
            };

            let pt = table_mut::<PT>(pt_phys, hhdm);
            let pt_slot = &mut (*pt)[pt_index(virt)];
            if pt_slot.is_present() {
                return Err(MapError::AlreadyMapped);
            }
            *pt_slot = PTEntry::new(phys, flags | PTFlags::P);
            tlb::flush(virt.as_u64() as usize);
            Ok(())
        }
    }

    pub fn unmap(&mut self, virt: VAddr, size: PageSize) -> Result<(), UnmapError> {
        if !is_canonical(virt) {
            return Err(UnmapError::NonCanonical);
        }
        if !virt.as_u64().is_multiple_of(size.bytes()) {
            return Err(UnmapError::Misaligned);
        }

        unsafe {
            let hhdm = HHDM_OFFSET.load(Ordering::Relaxed);

            let pml4 = table_mut::<PML4>(self.top_level, hhdm);
            let pml4_slot = &mut (*pml4)[pml4_index(virt)];
            if !pml4_slot.is_present() {
                return Err(UnmapError::NotMapped);
            }
            let pdpt_phys = PAddr(pml4_slot.address().as_u64());

            let pdpt = table_mut::<PDPT>(pdpt_phys, hhdm);
            let pdpt_slot = &mut (*pdpt)[pdpt_index(virt)];

            if size == PageSize::Huge {
                if !pdpt_slot.is_present() || !pdpt_slot.is_page() {
                    return Err(UnmapError::NotMapped);
                }
                *pdpt_slot = PDPTEntry(0);
                tlb::flush(virt.as_u64() as usize);
                reclaim_pdpt(pml4_slot, pdpt_phys, hhdm);
                return Ok(());
            }

            if !pdpt_slot.is_present() || pdpt_slot.is_page() {
                return Err(UnmapError::NotMapped);
            }
            let pd_phys = PAddr(pdpt_slot.address().as_u64());

            let pd = table_mut::<PD>(pd_phys, hhdm);
            let pd_slot = &mut (*pd)[pd_index(virt)];

            if size == PageSize::Large {
                if !pd_slot.is_present() || !pd_slot.is_page() {
                    return Err(UnmapError::NotMapped);
                }
                *pd_slot = PDEntry(0);
                tlb::flush(virt.as_u64() as usize);
                reclaim_pd(pdpt_slot, pd_phys, hhdm);
                reclaim_pdpt(pml4_slot, pdpt_phys, hhdm);
                return Ok(());
            }

            if !pd_slot.is_present() || pd_slot.is_page() {
                return Err(UnmapError::NotMapped);
            }
            let pt_phys = PAddr(pd_slot.address().as_u64());

            let pt = table_mut::<PT>(pt_phys, hhdm);
            let pt_slot = &mut (*pt)[pt_index(virt)];
            if !pt_slot.is_present() {
                return Err(UnmapError::NotMapped);
            }
            *pt_slot = PTEntry(0);
            tlb::flush(virt.as_u64() as usize);
            reclaim_pt(pd_slot, pt_phys, hhdm);
            reclaim_pd(pdpt_slot, pd_phys, hhdm);
            reclaim_pdpt(pml4_slot, pdpt_phys, hhdm);
            Ok(())
        }
    }

    pub fn map_range(
        &mut self,
        virt: VAddr,
        phys: PAddr,
        pages: u64,
        flags: PTFlags,
        size: PageSize,
    ) -> Result<(), MapError> {
        let stride = size.bytes();
        for i in 0..pages {
            let step = i * stride;
            if let Err(e) = self.map(
                VAddr(virt.as_u64() + step),
                PAddr(phys.as_u64() + step),
                flags,
                size,
            ) {
                for j in 0..i {
                    let back = j * stride;
                    let _ = self.unmap(VAddr(virt.as_u64() + back), size);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    pub fn unmap_range(
        &mut self,
        virt: VAddr,
        pages: u64,
        size: PageSize,
    ) -> Result<(), UnmapError> {
        let stride = size.bytes();
        for i in 0..pages {
            self.unmap(VAddr(virt.as_u64() + i * stride), size)?;
        }
        Ok(())
    }
}
pub fn translate(virt: VAddr) -> Option<(PAddr, PTFlags)> {
    VMM.with(|vmm| vmm.translate(virt))
}

pub fn map(virt: VAddr, phys: PAddr, flags: PTFlags, size: PageSize) -> Result<(), MapError> {
    VMM.with(|vmm| vmm.map(virt, phys, flags, size))
}

pub fn map_hhdm(phys: PAddr, flags: PTFlags, size: PageSize) -> Result<VAddr, MapError> {
    let virt = VAddr(phys.as_u64() + HHDM_OFFSET.load(Ordering::Relaxed));
    map(virt, phys, flags, size)?;
    Ok(virt)
}

pub fn unmap(virt: VAddr, size: PageSize) -> Result<(), UnmapError> {
    VMM.with(|vmm| vmm.unmap(virt, size))
}

pub fn map_range(
    virt: VAddr,
    phys: PAddr,
    pages: u64,
    flags: PTFlags,
    size: PageSize,
) -> Result<(), MapError> {
    VMM.with(|vmm| vmm.map_range(virt, phys, pages, flags, size))
}

pub fn unmap_range(virt: VAddr, pages: u64, size: PageSize) -> Result<(), UnmapError> {
    VMM.with(|vmm| vmm.unmap_range(virt, pages, size))
}

fn is_canonical(virt: VAddr) -> bool {
    let hi = virt.as_u64() >> 47;
    hi == 0 || hi == 0x1_ffff
}

fn table_mut<T>(phys: PAddr, hhdm_offset: u64) -> *mut T {
    (phys.as_u64() + hhdm_offset) as *mut T
}

fn zero_frame(phys: PAddr, hhdm_offset: u64) {
    let ptr = (phys.as_u64() + hhdm_offset) as *mut u8;
    unsafe { core::ptr::write_bytes(ptr, 0, BASE_PAGE_SIZE) };
}

fn pml4_parent() -> PML4Flags {
    PML4Flags::P | PML4Flags::RW | PML4Flags::US
}

fn pdpt_parent() -> PDPTFlags {
    PDPTFlags::P | PDPTFlags::RW | PDPTFlags::US
}

fn pd_parent() -> PDFlags {
    PDFlags::P | PDFlags::RW | PDFlags::US
}

fn pdpt_leaf(leaf: PTFlags) -> PDPTFlags {
    let common = leaf.bits()
        & (PTFlags::RW | PTFlags::US | PTFlags::PWT | PTFlags::PCD | PTFlags::G | PTFlags::XD)
            .bits();
    PDPTFlags::from_bits_truncate(common) | PDPTFlags::P | PDPTFlags::PS
}

fn pd_leaf(leaf: PTFlags) -> PDFlags {
    let common = leaf.bits()
        & (PTFlags::RW | PTFlags::US | PTFlags::PWT | PTFlags::PCD | PTFlags::G | PTFlags::XD)
            .bits();
    PDFlags::from_bits_truncate(common) | PDFlags::P | PDFlags::PS
}

fn is_empty(phys: PAddr, hhdm_offset: u64) -> bool {
    let entries = table_mut::<[u64; PAGE_SIZE_ENTRIES]>(phys, hhdm_offset);
    unsafe { (*entries).iter().all(|e| (*e) & 1 == 0) }
}

fn reclaim_pt(parent: &mut PDEntry, pt_phys: PAddr, hhdm: u64) {
    if parent.is_page() {
        return;
    }
    if is_empty(pt_phys, hhdm) {
        *parent = PDEntry(0);
        pmm::free_frame(pt_phys);
        unsafe { tlb::flush_all() };
    }
}

fn reclaim_pd(parent: &mut PDPTEntry, pd_phys: PAddr, hhdm: u64) {
    if parent.is_page() {
        return;
    }
    if is_empty(pd_phys, hhdm) {
        *parent = PDPTEntry(0);
        pmm::free_frame(pd_phys);
        unsafe { tlb::flush_all() };
    }
}

fn reclaim_pdpt(parent: &mut PML4Entry, pdpt_phys: PAddr, hhdm: u64) {
    if is_empty(pdpt_phys, hhdm) {
        *parent = PML4Entry(0);
        pmm::free_frame(pdpt_phys);
        unsafe { tlb::flush_all() };
    }
}

const ENTRY_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

fn clone_table(src_phys: PAddr, hhdm: u64, level: u8) -> Option<PAddr> {
    let dst_frame = pmm::alloc_frame()?;
    zero_frame(dst_frame, hhdm);

    let src = (src_phys.as_u64() + hhdm) as *const [u64; PAGE_SIZE_ENTRIES];
    let dst = (dst_frame.as_u64() + hhdm) as *mut [u64; PAGE_SIZE_ENTRIES];

    for i in 0..PAGE_SIZE_ENTRIES {
        let entry = unsafe { (*src)[i] };
        if entry & 1 == 0 {
            continue;
        }
        let is_leaf = level == 0 || (level <= 2 && (entry & (1 << 7)) != 0);
        if is_leaf {
            unsafe { (*dst)[i] = entry };
        } else {
            let child_phys = PAddr(entry & ENTRY_ADDR_MASK);
            let new_child = match clone_table(child_phys, hhdm, level - 1) {
                Some(c) => c,
                None => {
                    free_cloned_table(dst_frame, hhdm, level);
                    return None;
                }
            };
            let flags = entry & !ENTRY_ADDR_MASK;
            unsafe { (*dst)[i] = (new_child.as_u64() & ENTRY_ADDR_MASK) | flags };
        }
    }

    Some(dst_frame)
}

fn free_cloned_table(phys: PAddr, hhdm: u64, level: u8) {
    let table = (phys.as_u64() + hhdm) as *const [u64; PAGE_SIZE_ENTRIES];
    for i in 0..PAGE_SIZE_ENTRIES {
        let entry = unsafe { (*table)[i] };
        if entry & 1 == 0 {
            continue;
        }
        let is_leaf = level == 0 || (level <= 2 && (entry & (1 << 7)) != 0);
        if !is_leaf {
            free_cloned_table(PAddr(entry & ENTRY_ADDR_MASK), hhdm, level - 1);
        }
    }
    pmm::free_frame(phys);
}

pub fn take_ownership(hhdm_offset: u64) -> PAddr {
    let old_cr3 = unsafe { controlregs::cr3() };
    let old_pml4 = PAddr(old_cr3 & ENTRY_ADDR_MASK);
    let new_pml4 = clone_table(old_pml4, hhdm_offset, 3)
        .expect("clone_table: out of frames during take_ownership");
    let new_cr3 = (new_pml4.as_u64() & ENTRY_ADDR_MASK) | (old_cr3 & !ENTRY_ADDR_MASK);
    unsafe { controlregs::cr3_write(new_cr3) };
    new_pml4
}
