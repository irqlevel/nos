//! The extended page table: Intel's name for the second translation, from a
//! guest's physical addresses to the host's, which the CPU walks on every
//! guest memory access the TLB does not already have. AMD's is the [`crate::npt`];
//! this is the same idea in a different entry format.
//!
//! Four levels of 512 eight-byte entries, as the host's own page table has,
//! but the bits are EPT's: read (bit 0), write (bit 1), execute (bit 2), and
//! -- in a leaf -- the memory type in bits 5:3, write-back for plain RAM.
//! There is no present bit; an entry with no read, write or execute is the
//! one with no memory behind it, which is what an EPT violation on an
//! unmapped page finds.
//!
//! As the nested table on the AMD side, it is built from the host only,
//! walked by software only through its own indices, and only grows: an entry
//! is written once where there was none. So a translation the TLB holds is
//! one the table would still make, and a guest's entries keep their meaning
//! from one exit to the next.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use hvarch::{Error, Result};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;

use crate::memory::MAX_GPA;

const ENTRIES: usize = 512;
const ENTRY_BYTES: usize = 8;

const READ: u64 = 1 << 0;
const WRITE: u64 = 1 << 1;
const EXECUTE: u64 = 1 << 2;
/// Memory type in bits 5:3; 6 is write-back, the only type plain RAM wants.
const MEMTYPE_WB: u64 = 6 << 3;
/// Bits 12-51: the next table's, or the page's, physical address.
const ADDRESS: u64 = 0x000F_FFFF_FFFF_F000;

/// An entry that points at the next level down: readable, writable and
/// executable, so the leaf's own bits are what decide the access.
const TABLE: u64 = READ | WRITE | EXECUTE;
/// A 4 KiB page of guest memory: read, write, execute, write-back.
const PAGE: u64 = READ | WRITE | EXECUTE | MEMTYPE_WB;
/// The same page, read-only: a write to it is an EPT violation.
const PAGE_RO: u64 = READ | MEMTYPE_WB;

/// The EPT pointer's own bits, below the PML4 address: write-back (2:0 = 6),
/// a page-walk length of four (5:3 = 3), and nothing else -- no accessed or
/// dirty flags, so the CPU writes nothing back into the table.
const EPTP_BITS: u64 = 6 | (3 << 3);

/// No table below this entry yet.
const NONE: u32 = u32::MAX;

/// Levels from the top: which bits of an address index each one.
const SHIFTS: [u32; 4] = [39, 30, 21, 12];

struct Table {
    page: DmaBuffer,
    /// For each entry of a table above the last level, the index in
    /// `Ept::tables` of the table it points at, or `NONE`.
    next: Vec<u32>,
}

impl Table {
    fn new(last_level: bool) -> Result<Self> {
        let mut page = DmaBuffer::new(1).ok_or(Error::NoMemory)?;
        page.as_mut_slice().fill(0);
        let next = if last_level {
            Vec::new()
        } else {
            let mut next = Vec::new();
            next.try_reserve_exact(ENTRIES).map_err(|_| Error::NoMemory)?;
            next.resize(ENTRIES, NONE);
            next
        };
        Ok(Self { page, next })
    }
}

pub struct Ept {
    /// Every table, the top level first. An arena: a table is found by its
    /// index here, never by the address in an entry.
    tables: Vec<Table>,
    /// Its identity, as the nested table's is on the AMD side -- unused by
    /// VMX, whose VPID is the guest's for life rather than the AMD side's
    /// per-CPU tag over a table, but the shape the entry point takes is one
    /// for both.
    id: u64,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn index(gpa: u64, level: usize) -> usize {
    ((gpa >> SHIFTS[level]) as usize) & (ENTRIES - 1)
}

impl Ept {
    pub fn new() -> Result<Self> {
        let mut tables = Vec::new();
        tables.try_reserve(1).map_err(|_| Error::NoMemory)?;
        tables.push(Table::new(false)?);
        Ok(Self { tables, id: NEXT_ID.fetch_add(1, Ordering::Relaxed) })
    }

    /// The EPT pointer, with its memory type and walk length, and the id.
    pub fn nested(&self) -> hvarch::x86::svm::Nested {
        hvarch::x86::svm::Nested { root: self.tables[0].page.phys() | EPTP_BITS, id: self.id }
    }

    /// The last-level table that maps `gpa`, making the levels above it as
    /// needed when `make` says so.
    fn last_level(&mut self, gpa: u64, make: bool) -> Result<usize> {
        if gpa >= MAX_GPA {
            return Err(Error::BadAddress);
        }
        let mut table = 0usize;
        for level in 0..SHIFTS.len() - 1 {
            let i = index(gpa, level);
            let next = self.tables[table].next[i];
            table = if next != NONE {
                next as usize
            } else if make {
                let child = Table::new(level + 1 == SHIFTS.len() - 1)?;
                let phys = child.page.phys();
                if phys & !ADDRESS != 0 {
                    return Err(Error::BadAddress);
                }
                self.tables.try_reserve(1).map_err(|_| Error::NoMemory)?;
                let id = self.tables.len();
                self.tables.push(child);
                /* The child is zeroed and in the arena before the entry that
                 * makes it reachable is written. */
                if !self.tables[table].page.store::<u64>(i * ENTRY_BYTES, phys | TABLE) {
                    return Err(Error::BadAddress);
                }
                self.tables[table].next[i] = id as u32;
                id
            } else {
                return Err(Error::Unmapped);
            };
        }
        Ok(table)
    }

    /// Make every table `[gpa, gpa + size)` will need, so that mapping its
    /// pages allocates nothing and cannot fail for want of memory halfway.
    pub fn prepare(&mut self, gpa: u64, size: u64) -> Result<()> {
        let end = gpa.checked_add(size).ok_or(Error::BadAddress)?;
        let span = 1u64 << SHIFTS[SHIFTS.len() - 2];
        let mut at = gpa & !(span - 1);
        while at < end {
            self.last_level(at, true)?;
            at += span;
        }
        Ok(())
    }

    /// Map the page at `gpa` to the host page at `hpa`; already-mapped is
    /// refused rather than moved, the old page being still in some TLB.
    pub fn set(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        self.set_entry(gpa, hpa, PAGE)
    }

    /// As [`set`](Self::set), but read-only for the guest.
    pub fn set_ro(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        self.set_entry(gpa, hpa, PAGE_RO)
    }

    fn set_entry(&mut self, gpa: u64, hpa: u64, bits: u64) -> Result<()> {
        if gpa % PAGE_SIZE as u64 != 0 || hpa & !ADDRESS != 0 {
            return Err(Error::BadAddress);
        }
        let table = self.last_level(gpa, false)?;
        let offset = index(gpa, SHIFTS.len() - 1) * ENTRY_BYTES;
        let page = &mut self.tables[table].page;
        match page.load::<u64>(offset) {
            Some(0) => {}
            _ => return Err(Error::BadAddress),
        }
        if !page.store::<u64>(offset, hpa | bits) {
            return Err(Error::BadAddress);
        }
        Ok(())
    }
}
