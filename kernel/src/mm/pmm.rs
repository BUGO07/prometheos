use limine::memmap::{Entry, MEMMAP_BOOTLOADER_RECLAIMABLE};
use x86::current::paging::PAddr;

use crate::{println, utils::Singleton};

pub const FRAME_SHIFT: u32 = 12;
pub const FRAME_SIZE: u64 = 1 << FRAME_SHIFT;

fn frame_index(addr: PAddr) -> u64 {
    debug_assert!(
        addr.as_u64().is_multiple_of(FRAME_SIZE),
        "frame_index called on unaligned PAddr {:#x}",
        addr.as_u64()
    );
    addr.as_u64() >> FRAME_SHIFT
}

const fn from_frame(idx: u64) -> PAddr {
    PAddr(idx << FRAME_SHIFT)
}

pub struct Bitmap {
    pub buf: &'static mut [u8],
}

impl Bitmap {
    pub fn new(addr: *mut u8, len: u64) -> Self {
        assert!(
            len.is_multiple_of(8),
            "bitmap length must be a multiple of 8"
        );
        assert!(
            (addr as usize).is_multiple_of(8),
            "bitmap must be 8-byte aligned"
        );
        Self {
            buf: unsafe { core::slice::from_raw_parts_mut(addr, len as usize) },
        }
    }

    fn words(&self) -> &[u64] {
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr().cast(), self.buf.len() / 8) }
    }

    fn words_mut(&mut self) -> &mut [u64] {
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast(), self.buf.len() / 8) }
    }

    pub fn set(&mut self, i: u64) {
        let byte = (i / 8) as usize;
        let bit = (i % 8) as u8;
        assert!(byte < self.buf.len(), "bitmap index out of range: {}", i);
        self.buf[byte] |= 1 << bit;
    }

    pub fn clear(&mut self, i: u64) {
        let byte = (i / 8) as usize;
        let bit = (i % 8) as u8;
        assert!(byte < self.buf.len(), "bitmap index out of range: {}", i);
        self.buf[byte] &= !(1 << bit);
    }

    pub fn read(&self, i: u64) -> bool {
        let byte = (i / 8) as usize;
        let bit = (i % 8) as u8;
        assert!(byte < self.buf.len(), "bitmap index out of range: {}", i);
        self.buf[byte] & (1 << bit) != 0
    }

    pub fn find_clear_from(&self, start: u64, limit: u64) -> Option<u64> {
        if start >= limit {
            return None;
        }
        let words = self.words();
        let start_word = (start / 64) as usize;
        let start_bit = start % 64;

        if start_word < words.len() {
            let mask = (1u64 << start_bit).wrapping_sub(1);
            let word = words[start_word] | mask;
            if word != u64::MAX {
                let idx = start_word as u64 * 64 + word.trailing_ones() as u64;
                if idx < limit {
                    return Some(idx);
                }
                // give up early
                return None;
            }
        }

        for (i, &word) in words.iter().enumerate().skip(start_word + 1) {
            if word != u64::MAX {
                let idx = i as u64 * 64 + word.trailing_ones() as u64;
                if idx < limit {
                    return Some(idx);
                }
                return None;
            }
        }
        None
    }

    pub fn set_range(&mut self, start: u64, count: u64) {
        self.apply_range(start, count, true);
    }

    pub fn clear_range(&mut self, start: u64, count: u64) {
        self.apply_range(start, count, false);
    }

    fn apply_range(&mut self, start: u64, count: u64, value: bool) {
        if count == 0 {
            return;
        }
        let end = start + count;
        assert!(
            end <= (self.buf.len() as u64) * 8,
            "range out of bounds: {}..{}",
            start,
            end
        );

        let mut i = start;
        while i < end && !i.is_multiple_of(64) {
            if value {
                self.set(i);
            } else {
                self.clear(i);
            }
            i += 1;
        }
        if i + 64 <= end {
            let fill = if value { u64::MAX } else { 0 };
            let words = self.words_mut();
            let word_start = (i / 64) as usize;
            let word_end = (end / 64) as usize;
            for slot in &mut words[word_start..word_end] {
                *slot = fill;
            }
            i = (word_end as u64) * 64;
        }
        while i < end {
            if value {
                self.set(i);
            } else {
                self.clear(i);
            }
            i += 1;
        }
    }
}

pub struct Pmm {
    bitmap: Bitmap,
    total_frames: u64,
    free_frames: u64,
    last_index: u64,
}

unsafe impl Send for Pmm {}

pub static PMM: Singleton<Pmm> = Singleton::new();

pub fn alloc_frame() -> Option<PAddr> {
    PMM.with(|pmm| pmm.alloc_frame())
}

pub fn free_frame(addr: PAddr) {
    PMM.with(|pmm| pmm.free_frame(addr));
}

pub fn alloc_contiguous(count: u64, align: u64, below_frame: u64) -> Option<PAddr> {
    PMM.with(|pmm| pmm.alloc_contiguous(count, align, below_frame))
}

pub fn free_contiguous(base: PAddr, count: u64) {
    PMM.with(|pmm| pmm.free_contiguous(base, count));
}

pub fn stats() -> (u64, u64) {
    PMM.with(|pmm| (pmm.free_frames, pmm.total_frames))
}

pub fn reclaim_bootloader(memmap: &[&Entry]) {
    PMM.with(|pmm: &mut Pmm| {
        let mut reclaimed: u64 = 0;
        for entry in memmap {
            if entry.type_ != MEMMAP_BOOTLOADER_RECLAIMABLE {
                continue;
            }
            let first = entry.base >> FRAME_SHIFT;
            let count = entry.length >> FRAME_SHIFT;
            pmm.bitmap.clear_range(first, count);
            pmm.free_frames += count;
            if first < pmm.last_index {
                pmm.last_index = first;
            }
            reclaimed += count;
        }
        println!(
            "reclaimed {} frames ({} KiB) from bootloader",
            reclaimed,
            reclaimed * FRAME_SIZE / 1024
        );
    });
}

impl Pmm {
    pub fn new(bitmap: Bitmap, total_frames: u64, free_frames: u64) -> Self {
        Self {
            bitmap,
            total_frames,
            free_frames,
            last_index: 1, // skip frame 0
        }
    }

    fn alloc_frame(&mut self) -> Option<PAddr> {
        let idx = self
            .bitmap
            .find_clear_from(self.last_index, self.total_frames)
            .or_else(|| self.bitmap.find_clear_from(0, self.last_index))?;
        self.bitmap.set(idx);
        self.free_frames -= 1;
        self.last_index = idx + 1;
        Some(from_frame(idx))
    }

    fn free_frame(&mut self, addr: PAddr) {
        let idx = frame_index(addr);
        assert!(
            idx < self.total_frames,
            "free out of range: {:#x}",
            addr.as_u64()
        );
        assert!(self.bitmap.read(idx), "double free at {:#x}", addr.as_u64());
        self.bitmap.clear(idx);
        self.free_frames += 1;
        if idx < self.last_index {
            self.last_index = idx;
        }
    }

    fn scan_contiguous(&self, from: u64, limit: u64, count: u64, align_mask: u64) -> Option<u64> {
        let mut base = (from + align_mask) & !align_mask;
        let mut run = 0u64;
        while base + count <= limit {
            let probe = base + run;
            if self.bitmap.read(probe) {
                // collision, restart at the next aligned slot past the blocker
                base = (probe + 1 + align_mask) & !align_mask;
                run = 0;
            } else {
                run += 1;
                if run == count {
                    return Some(base);
                }
            }
        }
        None
    }

    fn alloc_contiguous(&mut self, count: u64, align: u64, below_frame: u64) -> Option<PAddr> {
        assert!(align.is_power_of_two(), "align must be a power of two");
        assert!(count > 0, "count must be positive");

        if count > self.free_frames {
            return None;
        }

        let limit = self.total_frames.min(below_frame);
        let align_mask = align - 1;

        let base = self
            .scan_contiguous(self.last_index, limit, count, align_mask)
            .or_else(|| self.scan_contiguous(0, self.last_index, count, align_mask))?;

        self.bitmap.set_range(base, count);
        self.free_frames -= count;
        self.last_index = base + count;
        Some(from_frame(base))
    }

    fn free_contiguous(&mut self, base: PAddr, count: u64) {
        let first = frame_index(base);
        assert!(
            first + count <= self.total_frames,
            "free_contiguous out of range: {:#x} + {}",
            base.as_u64(),
            count
        );
        for i in 0..count {
            let idx = first + i;
            assert!(self.bitmap.read(idx), "double free at frame {}", idx);
            self.bitmap.clear(idx);
        }
        self.free_frames += count;
        if first < self.last_index {
            self.last_index = first;
        }
    }
}
