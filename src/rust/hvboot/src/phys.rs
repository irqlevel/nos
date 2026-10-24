//! Guest physical memory, which the loader runs identity-mapped over the
//! first 4 GiB of: the start info read out of it, and the ranges a kernel
//! and an initrd are read into lent as slices -- only where the memory map
//! says RAM, away from the loader's own image, and never twice.

use alloc::string::String;
use alloc::vec::Vec;

use crate::start::{MemEntry, E820_RAM};

/// The identity map's reach (`pvh_entry` maps 4 GiB).
const MAPPED: u64 = 1 << 32;

/// Copy `buf.len()` bytes at physical address `addr` out: the start info,
/// its memory map and command line, where hv put them.
pub fn read(addr: u64, buf: &mut [u8]) -> Result<(), String> {
    let end = addr.checked_add(buf.len() as u64).filter(|&e| e <= MAPPED);
    if addr == 0 || end.is_none() {
        return Err(alloc::format!("the start info points at {:#x}, outside the memory mapped", addr));
    }
    // SAFETY: the range is inside the identity map, which is all readable
    // memory -- RAM, or a device page reading as ones -- and nothing of
    // Rust's aliases it as `&mut`: only the loans below hand out RAM, and
    // they are made after the start info is read.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), buf.len()) };
    Ok(())
}

/// The RAM the memory map gives the guest, less the loader's image, and
/// what of it has been lent.
pub struct Ram {
    ram: Vec<(u64, u64)>,
    image: (u64, u64),
    lent: Vec<(u64, u64)>,
}

impl Ram {
    pub fn new(map: &[MemEntry], image: (u64, u64)) -> Ram {
        let ram = map.iter().filter(|e| e.kind == E820_RAM).filter_map(|e| Some((e.addr, e.addr.checked_add(e.size)?)))
            .collect();
        Ram { ram, image, lent: Vec::new() }
    }

    /// Whether `[addr, addr + len)` is RAM, below 4 GiB, clear of the
    /// loader's image and of what has been lent.
    pub fn free(&self, addr: u64, len: u64) -> bool {
        let Some(end) = addr.checked_add(len).filter(|&e| e <= MAPPED) else { return false };
        let in_ram = self.ram.iter().any(|&(s, e)| addr >= s && end <= e);
        let apart = |&(s, e): &(u64, u64)| end <= s || addr >= e;
        len != 0 && in_ram && apart(&self.image) && self.lent.iter().all(apart)
    }

    /// The highest RAM below `ceiling` this could lend `len` bytes of, at
    /// `align`, above `floor`: where an initrd goes.
    pub fn highest(&self, len: u64, align: u64, floor: u64, ceiling: u64) -> Option<u64> {
        let mut best = None;
        for &(s, e) in &self.ram {
            let top = e.min(ceiling).min(MAPPED);
            let Some(addr) = top.checked_sub(len).map(|a| a & !(align - 1)) else { continue };
            if addr < s.max(floor) || !self.free(addr, len) {
                continue;
            }
            best = Some(best.map_or(addr, |b: u64| b.max(addr)));
        }
        best
    }

    /// Lend `[addr, addr + len)` to be written: once, and only if it is
    /// [`Ram::free`].
    pub fn lend(&mut self, addr: u64, len: u64) -> Result<&'static mut [u8], String> {
        if !self.free(addr, len) {
            return Err(alloc::format!("no RAM free for {:#x} bytes at {:#x}", len, addr));
        }
        let n = usize::try_from(len).map_err(|_| String::from("a range too long"))?;
        self.lent.push((addr, addr + len));
        // SAFETY: the range is RAM inside the identity map, outside this
        // program's image -- its code, data, BSS, stack and heap -- and lent
        // no other time, so this slice is the only reference to it.
        Ok(unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, n) })
    }
}
