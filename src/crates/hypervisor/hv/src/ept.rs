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
//! walked by software only through its own indices, and only grows -- but
//! for a migration's dirty log: an entry is written once where there was
//! none. So a translation the TLB holds is one the table would still make,
//! and a guest's entries keep their meaning from one exit to the next. The
//! dirty log narrows entries -- a page read-only, a 2 MiB leaf split -- and
//! gives the table a new id with each round of them (`renew`), which every
//! CPU flushes the table's translations for before the guest enters there
//! again (INVEPT, `hvarch::x86::vmx`).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use hvarch::{Error, Result};
use kcore::consts::PAGE_SIZE;
use kcore::chunked::Chunked;
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
/// An entry of the level above the last that is a page itself, 2 MiB of
/// guest memory (bit 7): read, write, execute, write-back, as a page is.
/// Only where the CPU says it walks them (`IA32_VMX_EPT_VPID_CAP` bit 16,
/// `hvarch::x86::Caps::nested_2m`).
const PAGE_SIZE_BIT: u64 = 1 << 7;
const HUGE_PAGE: u64 = PAGE | PAGE_SIZE_BIT;
/// A page of a device's: read and write, uncacheable (type 0 in bits 5:3),
/// IPAT clear -- the guest's PAT then decides with it, and UC wins over
/// every type it may give -- and never executed.
const MEMTYPE_UC: u64 = 0 << 3;
const DEVICE_PAGE: u64 = READ | WRITE | MEMTYPE_UC;

/// The EPT pointer's own bits, below the PML4 address: write-back (2:0 = 6),
/// a page-walk length of four (5:3 = 3), and nothing else -- no accessed or
/// dirty flags, so the CPU writes nothing back into the table.
const EPTP_BITS: u64 = 6 | (3 << 3);

/// No table below this entry yet.
const NONE: u32 = u32::MAX;
/// No table below this entry ever: it is a 2 MiB page itself.
const LEAF: u32 = u32::MAX - 1;
/// The level whose entries are 2 MiB pages, or the last-level tables that
/// split them into 4 KiB ones.
const DIRECTORY: usize = 2;
/// What a 2 MiB page's address must be a multiple of.
const HUGE_BYTES: u64 = 1 << 21;

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
    /// index here, never by the address in an entry. Kept in page-sized
    /// chunks: one `Vec`, doubled as it grew, was past the heap's largest
    /// block at 16384 tables -- a guest of 4 KiB pages stopped short of 16
    /// GiB, "out of memory" on a machine with it free.
    tables: Chunked<Table>,
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
        let mut tables = Chunked::new();
        tables.try_push(Table::new(false)?).map_err(|_| Error::NoMemory)?;
        Ok(Self { tables, id: NEXT_ID.fetch_add(1, Ordering::Relaxed) })
    }

    /// The EPT pointer, with its memory type and walk length, and the id.
    pub fn nested(&self) -> hvarch::x86::svm::Nested {
        hvarch::x86::svm::Nested { root: self.tables[0].page.phys() | EPTP_BITS, id: self.id }
    }

    /// The last-level table that maps `gpa`, making the levels above it as
    /// needed when `make` says so.
    fn last_level(&mut self, gpa: u64, make: bool) -> Result<usize> {
        self.table_at(gpa, SHIFTS.len() - 1, make)
    }

    /// The table of level `depth` (0 the top) on the way to `gpa`, making
    /// the levels above it as needed when `make` says so. An entry on the
    /// way that is a 2 MiB page has no table below it, and refuses.
    fn table_at(&mut self, gpa: u64, depth: usize, make: bool) -> Result<usize> {
        if gpa >= MAX_GPA {
            return Err(Error::BadAddress);
        }
        let mut table = 0usize;
        for level in 0..depth {
            let i = index(gpa, level);
            let next = self.tables[table].next[i];
            table = if next == LEAF {
                return Err(Error::BadAddress);
            } else if next != NONE {
                next as usize
            } else if make {
                let child = Table::new(level + 1 == SHIFTS.len() - 1)?;
                let phys = child.page.phys();
                if phys & !ADDRESS != 0 {
                    return Err(Error::BadAddress);
                }
                let id = self.tables.try_push(child).map_err(|_| Error::NoMemory)?;
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

    /// Make every table `[gpa, gpa + size)` will need to be mapped 2 MiB at
    /// a time ([`set_huge`](Self::set_huge)) -- the levels down to the one
    /// whose entries are its pages, and none below: whole 2 MiB, and none
    /// of it split into 4 KiB pages already.
    pub fn prepare_huge(&mut self, gpa: u64, size: u64) -> Result<()> {
        let end = gpa.checked_add(size).ok_or(Error::BadAddress)?;
        if gpa % HUGE_BYTES != 0 || size % HUGE_BYTES != 0 {
            return Err(Error::BadAddress);
        }
        let mut at = gpa;
        while at < end {
            let dir = self.table_at(at, DIRECTORY, true)?;
            if self.tables[dir].next[index(at, DIRECTORY)] != NONE {
                return Err(Error::BadAddress);
            }
            at += HUGE_BYTES;
        }
        Ok(())
    }

    /// Map the 2 MiB at `gpa` to the host's 2 MiB at `hpa`, both aligned to
    /// it, with one entry. The tables have been made by `prepare_huge`; an
    /// entry already there -- a page, or a table of 4 KiB ones -- is refused
    /// rather than replaced, as [`set`](Self::set) refuses.
    pub fn set_huge(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        self.set_huge_with(gpa, hpa, HUGE_PAGE)
    }

    /// A page of a device's BAR at `gpa`, the host's `hpa` -- not RAM, not
    /// cached: uncacheable whatever the guest's PAT says, as a device's
    /// registers are to be reached, and no instruction fetched from it.
    pub fn set_device(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        self.set_entry(gpa, hpa, DEVICE_PAGE)
    }

    /// As [`set_device`](Self::set_device), 2 MiB at a time, prepared as
    /// [`set_huge`](Self::set_huge)'s are.
    pub fn set_huge_device(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        self.set_huge_with(gpa, hpa, DEVICE_PAGE | PAGE_SIZE_BIT)
    }

    /// What maps `gpa` taken out -- a 2 MiB leaf whole, or one page: a BAR
    /// the guest moved. The TLBs are the caller's (`renew`).
    pub fn clear(&mut self, gpa: u64) -> Result<()> {
        let dir = match self.table_at(gpa, DIRECTORY, false) {
            Ok(dir) => dir,
            /* No table on the way: nothing is mapped there */
            Err(Error::Unmapped) => return Ok(()),
            Err(e) => return Err(e),
        };
        let i = index(gpa, DIRECTORY);
        if self.tables[dir].next[i] == LEAF {
            if !self.tables[dir].page.store::<u64>(i * ENTRY_BYTES, 0) {
                return Err(Error::BadAddress);
            }
            self.tables[dir].next[i] = NONE;
            return Ok(());
        }
        if self.tables[dir].next[i] == NONE {
            return Ok(());
        }
        let table = self.last_level(gpa, false)?;
        let offset = index(gpa, SHIFTS.len() - 1) * ENTRY_BYTES;
        if !self.tables[table].page.store::<u64>(offset, 0) {
            return Err(Error::BadAddress);
        }
        Ok(())
    }

    fn set_huge_with(&mut self, gpa: u64, hpa: u64, bits: u64) -> Result<()> {
        if gpa % HUGE_BYTES != 0 || hpa % HUGE_BYTES != 0 || hpa & !ADDRESS != 0 {
            return Err(Error::BadAddress);
        }
        let dir = self.table_at(gpa, DIRECTORY, false)?;
        let i = index(gpa, DIRECTORY);
        if self.tables[dir].next[i] != NONE {
            return Err(Error::BadAddress);
        }
        let page = &mut self.tables[dir].page;
        match page.load::<u64>(i * ENTRY_BYTES) {
            Some(0) => {}
            _ => return Err(Error::BadAddress),
        }
        if !page.store::<u64>(i * ENTRY_BYTES, hpa | bits) {
            return Err(Error::BadAddress);
        }
        self.tables[dir].next[i] = LEAF;
        Ok(())
    }

    /// Its identity for the TLB now (`renew`).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// A new identity: what the table translated has narrowed -- a page
    /// made read-only, a 2 MiB leaf split (`protect`) -- so every CPU drops
    /// what it cached through the table as the guest next enters there
    /// (INVEPT, `hvarch::x86::vmx`).
    pub fn renew(&mut self) {
        self.id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    }

    /// The 2 MiB leaf `gpa` is in, if it is in one, made a last-level table
    /// of the same 512 pages, each as the leaf had it: what a page of it is
    /// protected through (`protect`). The TLBs are the caller's (`renew`).
    fn split(&mut self, gpa: u64) -> Result<()> {
        let dir = self.table_at(gpa, DIRECTORY, false)?;
        let i = index(gpa, DIRECTORY);
        if self.tables[dir].next[i] != LEAF {
            return Ok(());
        }
        let leaf = self.tables[dir].page.load::<u64>(i * ENTRY_BYTES).ok_or(Error::BadAddress)?;
        let hpa = leaf & ADDRESS & !(HUGE_BYTES - 1);
        let mut child = Table::new(true)?;
        let phys = child.page.phys();
        if phys & !ADDRESS != 0 {
            return Err(Error::BadAddress);
        }
        for k in 0..ENTRIES {
            if !child.page.store::<u64>(k * ENTRY_BYTES, (hpa + (k * PAGE_SIZE) as u64) | PAGE) {
                return Err(Error::BadAddress);
            }
        }
        let id = self.tables.try_push(child).map_err(|_| Error::NoMemory)?;
        /* The table filled and in the arena before the entry that makes it
         * reachable takes the leaf's place. */
        if !self.tables[dir].page.store::<u64>(i * ENTRY_BYTES, phys | TABLE) {
            return Err(Error::BadAddress);
        }
        self.tables[dir].next[i] = id as u32;
        Ok(())
    }

    /// The page at `gpa`, mapped, made read-only to the guest -- its 2 MiB
    /// leaf split first where it is in one (`split`): a write to it is an
    /// EPT violation from the TLBs' next flush on (`renew`).
    pub fn protect(&mut self, gpa: u64) -> Result<()> {
        self.split(gpa)?;
        self.change(gpa, |e| e & !WRITE)
    }

    /// The page at `gpa`, mapped, writable to the guest again, as `set`
    /// mapped it. Nothing to flush: a TLB keeps no translation wider than
    /// the table's.
    pub fn unprotect(&mut self, gpa: u64) -> Result<()> {
        self.change(gpa, |e| e | WRITE)
    }

    /// The last-level entry of the page at `gpa` -- one there, readable --
    /// made what `f` makes of it.
    fn change(&mut self, gpa: u64, f: fn(u64) -> u64) -> Result<()> {
        if gpa % PAGE_SIZE as u64 != 0 {
            return Err(Error::BadAddress);
        }
        let table = self.last_level(gpa, false)?;
        let offset = index(gpa, SHIFTS.len() - 1) * ENTRY_BYTES;
        let page = &mut self.tables[table].page;
        let e = page.load::<u64>(offset).ok_or(Error::BadAddress)?;
        if e & READ == 0 {
            return Err(Error::Unmapped);
        }
        if !page.store::<u64>(offset, f(e)) {
            return Err(Error::BadAddress);
        }
        Ok(())
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
