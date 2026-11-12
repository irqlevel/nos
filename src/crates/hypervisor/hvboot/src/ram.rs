//! Guest RAM as the boot places things in it: what the memory map says is
//! RAM, below the identity map's 4 GiB, less the loader's own image -- and
//! the ranges taken from it, the kernel's and the initrds', each once. No
//! platform in it: a range taken is a [`Loan`], which only [`Ram::take`]
//! makes, and which the loader's `phys::lend` makes the bytes of -- the
//! host's harness a buffer of its own.

use alloc::string::String;
use alloc::vec::Vec;

/// The identity map's reach (`pvh_entry` maps 4 GiB): nothing is placed
/// past it.
pub const MAPPED: u64 = 1 << 32;

/// An entry of the memory map, as e820 has them -- and PVH's start info,
/// and the zero page the kernel is handed (`bzimage`).
pub use bzimage::{E820Entry, E820_RAM};

/// A range taken by [`Ram::take`]: RAM the memory map has, below 4 GiB,
/// clear of the loader's image and of every range taken before it. It is
/// neither `Clone` nor made anywhere else, so the bytes made of it --
/// `phys::lend`, which consumes it -- are no one else's.
#[derive(Debug)]
pub struct Loan {
    addr: u64,
    len: u64,
}

impl Loan {
    pub fn addr(&self) -> u64 {
        self.addr
    }
    pub fn len(&self) -> u64 {
        self.len
    }
}

/// The RAM the memory map gives the guest, the loader's image, and what has
/// been taken.
pub struct Ram {
    ram: Vec<(u64, u64)>,
    image: (u64, u64),
    taken: Vec<(u64, u64)>,
}

impl Ram {
    pub fn new(map: &[E820Entry], image: (u64, u64)) -> Ram {
        let ram = map.iter().filter(|e| e.kind == E820_RAM).filter_map(|e| Some((e.addr, e.addr.checked_add(e.size)?)))
            .collect();
        Ram { ram, image, taken: Vec::new() }
    }

    /// Whether `[addr, addr + len)` is RAM, below 4 GiB, clear of the
    /// loader's image and of what has been taken.
    pub fn free(&self, addr: u64, len: u64) -> bool {
        let Some(end) = addr.checked_add(len).filter(|&e| e <= MAPPED) else { return false };
        let in_ram = self.ram.iter().any(|&(s, e)| addr >= s && end <= e);
        let apart = |&(s, e): &(u64, u64)| end <= s || addr >= e;
        len != 0 && in_ram && apart(&self.image) && self.taken.iter().all(apart)
    }

    /// The highest RAM below `ceiling` that `len` bytes at `align` could be
    /// taken from, above `floor`: where an initrd goes.
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

    /// Take `[addr, addr + len)` to be written: once, and only if it is
    /// [`Ram::free`].
    pub fn take(&mut self, addr: u64, len: u64) -> Result<Loan, String> {
        if !self.free(addr, len) {
            return Err(alloc::format!("no RAM free for {:#x} bytes at {:#x}", len, addr));
        }
        self.taken.push((addr, addr + len));
        Ok(Loan { addr, len })
    }
}
