//! The nested page table: AMD's name for the second translation, from a
//! guest's physical addresses to the host's, which the CPU walks on every
//! guest memory access the TLB does not already have.
//!
//! Its format is the host's own long-mode page table, four levels of 512
//! eight-byte entries, with one difference that matters: a nested walk is a
//! user-mode access at every level, so every entry has U/S set or the guest
//! faults on memory it was given.
//!
//! This table is built from the host's side only and walked by software
//! only through its own indices: an entry the CPU reads is written here and
//! never read back to find the next level. The CPU sets accessed and dirty
//! bits in entries as it walks; they are set from the start, so that it has
//! nothing to write.

use alloc::vec::Vec;

use hvarch::{Error, Result};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;

use crate::memory::MAX_GPA;

const ENTRIES: usize = 512;
const ENTRY_BYTES: usize = 8;

const PRESENT: u64 = 1 << 0;
const WRITE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const ACCESSED: u64 = 1 << 5;
const DIRTY: u64 = 1 << 6;
/// Bits 12-51: the next table's, or the page's, physical address.
const ADDRESS: u64 = 0x000F_FFFF_FFFF_F000;

/// An entry that points at the next level down.
const TABLE: u64 = PRESENT | WRITE | USER | ACCESSED;
/// A 4 KiB page of guest memory: readable, writable and executable by the
/// guest, write-back -- PAT, PCD and PWT clear pick the host's PAT entry 0.
const PAGE: u64 = PRESENT | WRITE | USER | ACCESSED | DIRTY;
/// The same page, read-only: a write to it is a nested page fault.
const PAGE_RO: u64 = PRESENT | USER | ACCESSED;

/// No table below this entry yet.
const NONE: u32 = u32::MAX;

/// Levels from the top: which bits of an address index each one.
const SHIFTS: [u32; 4] = [39, 30, 21, 12];

struct Table {
    page: DmaBuffer,
    /// For each entry of a table above the last level, the index in
    /// `Npt::tables` of the table it points at, or `NONE`. Empty for a last
    /// level table, whose entries are pages.
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

pub struct Npt {
    /// Every table, the top level first. An arena: a table is found by its
    /// index here, never by the address in an entry.
    tables: Vec<Table>,
}

fn index(gpa: u64, level: usize) -> usize {
    ((gpa >> SHIFTS[level]) as usize) & (ENTRIES - 1)
}

impl Npt {
    pub fn new() -> Result<Self> {
        let mut tables = Vec::new();
        tables.try_reserve(1).map_err(|_| Error::NoMemory)?;
        tables.push(Table::new(false)?);
        Ok(Self { tables })
    }

    /// The top level's physical address: `nCR3`.
    pub fn root(&self) -> u64 {
        self.tables[0].page.phys()
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
        /* One last-level table covers 2 MiB. */
        let span = 1u64 << SHIFTS[SHIFTS.len() - 2];
        let mut at = gpa & !(span - 1);
        while at < end {
            self.last_level(at, true)?;
            at += span;
        }
        Ok(())
    }

    /// Map the page at `gpa` to the host page at `hpa`. The tables have been
    /// made by `prepare`; a page already mapped is refused rather than
    /// quietly moved -- the old page would still be in a TLB somewhere.
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
