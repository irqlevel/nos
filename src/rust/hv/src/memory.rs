//! Guest physical memory: the host pages a guest's RAM is made of, the
//! nested table that is the guest's only way to them, and the host's only
//! way to read and write them.

use alloc::vec::Vec;

use hvarch::{Error, Result};
use kcore::consts::PAGE_SIZE;
use kcore::frame::Frame;
use kcore::pod::{self, Pod};
use kcore::sync::Mutex;

#[cfg(target_arch = "x86_64")]
use crate::ept::Ept;
#[cfg(target_arch = "x86_64")]
use crate::npt::Npt;
#[cfg(target_arch = "x86_64")]
use hvarch::Vendor;

/// The second-level translation, whichever the machine has: AMD's nested
/// page table or Intel's extended one. Both are built the same way -- from
/// the host, only growing -- and offer the same handful of operations, so
/// the memory above them is one set of code over an enum, not two.
#[cfg(target_arch = "x86_64")]
enum SecondLevel {
    Npt(Npt),
    Ept(Ept),
}

#[cfg(target_arch = "x86_64")]
impl SecondLevel {
    fn prepare(&mut self, gpa: u64, size: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.prepare(gpa, size),
            SecondLevel::Ept(t) => t.prepare(gpa, size),
        }
    }
    fn set(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set(gpa, hpa),
            SecondLevel::Ept(t) => t.set(gpa, hpa),
        }
    }
    fn set_ro(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set_ro(gpa, hpa),
            SecondLevel::Ept(t) => t.set_ro(gpa, hpa),
        }
    }
    fn nested(&self) -> hvarch::x86::svm::Nested {
        match self {
            SecondLevel::Npt(t) => t.nested(),
            SecondLevel::Ept(t) => t.nested(),
        }
    }
}

const PAGE: u64 = PAGE_SIZE as u64;
/// Frames are kept in runs of this many, so that no one allocation of the
/// table of them outgrows a page however large the guest is.
const RUN_FRAMES: usize = PAGE_SIZE / core::mem::size_of::<Frame>();

/// Where a guest's physical address space ends: four levels of nested table
/// translate 48 bits, and an address above that faults whatever is mapped.
pub const MAX_GPA: u64 = 1 << 48;

/// The platform's MMIO window below 4 GiB, where a PC keeps its chipset's
/// registers (the IOAPIC, the HPET, AMD's FCH, the local APIC, the firmware
/// flash): what [`GuestMemory::map_absent`] answers reads of.
#[cfg(target_arch = "x86_64")]
const MMIO_WINDOW: core::ops::Range<u64> = 0xC000_0000..0x1_0000_0000;
/// The most pages of it a guest may have answered that way: a guest that
/// walks the whole window is not probing for a device.
#[cfg(target_arch = "x86_64")]
const MAX_ABSENT_PAGES: usize = 64;
/// The local APIC's page in xAPIC mode. A guest's APIC is an x2APIC, reached
/// through MSRs, and never this page: a guest that reads it has left x2APIC
/// mode, and is better stopped at the read, naming the page, than answered
/// with an APIC whose every register is all ones.
#[cfg(target_arch = "x86_64")]
pub const XAPIC_PAGE: u64 = 0xFEE0_0000;

/// A guest's memory.
///
/// Its pages are [`Frame`]s: RAM the kernel handed out by address and mapped
/// nowhere, so that the guest's nested table is the only mapping of it there
/// is, and a guest of any size costs pages and no kernel address space. The
/// guest writes it whenever it runs, on whatever CPU it runs on, so nothing
/// here hands out a reference into it -- a reference that changes under the
/// compiler is undefined behaviour however it is used. Every access is a
/// copy, bounds-checked against the region it falls in, made by the kernel
/// through its temporary window onto the page: an address the guest gave,
/// whatever it is, is at worst an `Unmapped`.
///
/// The nested table is inside, and that is the point of it. A guest reaches
/// exactly what its nested table maps, and the table here maps nothing but
/// pages this value owns -- each one owned before it is mapped, and freed
/// only with the table -- so "the guest can reach host memory it was not
/// given" is not a thing a caller can get wrong.
///
/// A guest of several CPUs shares one: every vCPU's task copies in and out
/// of it (`read` and `write` take `&self`, each copy a CPU's own), and any
/// of them may find an absent device (`map_absent`), which grows the table
/// under a lock. The regions are fixed once the guest is built.
pub struct GuestMemory {
    /* Before the regions and the absent page, so that it is dropped first:
     * the table goes before the pages it maps are back on the free list. */
    #[cfg(target_arch = "x86_64")]
    table: Mutex<SecondLevel>,
    /// The table's top level and identity, as an entry names them: neither
    /// changes for as long as the table lives, so an entry reads them here
    /// without the table's lock.
    #[cfg(target_arch = "x86_64")]
    nested: hvarch::x86::svm::Nested,
    regions: Vec<Region>,
    /// The page of all ones a guest's reads of an absent device find, and
    /// where it is mapped.
    absent: Mutex<Absent>,
}

/// What [`GuestMemory::map_absent`] has made so far.
struct Absent {
    /// A page of all ones: made the first time one is needed, mapped
    /// read-only wherever it is.
    #[cfg(target_arch = "x86_64")]
    frame: Option<Frame>,
    /// Where it is mapped, page-aligned, in the order the guest found them.
    at: Vec<u64>,
}

/// One run of guest physical addresses with memory behind it.
struct Region {
    base: u64,
    size: u64,
    /// One frame a page, in order, `RUN_FRAMES` to a run.
    runs: Vec<Vec<Frame>>,
}

impl Region {
    fn contains(&self, gpa: u64, len: u64) -> bool {
        gpa >= self.base && len <= self.size && gpa - self.base <= self.size - len
    }

    /// The frame `gpa` is in, and where in it: `gpa` is inside the region.
    fn frame(&self, gpa: u64) -> (&Frame, usize) {
        let page = ((gpa - self.base) / PAGE) as usize;
        (&self.runs[page / RUN_FRAMES][page % RUN_FRAMES], (gpa % PAGE) as usize)
    }
}

impl GuestMemory {
    /// No memory at all, and a second-level table -- of the kind the
    /// machine's `vendor` calls for -- that maps nothing.
    #[cfg(target_arch = "x86_64")]
    pub fn new(vendor: Vendor) -> Result<Self> {
        let table = match vendor {
            Vendor::Svm => SecondLevel::Npt(Npt::new()?),
            Vendor::Vmx => SecondLevel::Ept(Ept::new()?),
            _ => return Err(Error::NotImplemented),
        };
        let nested = table.nested();
        Ok(Self {
            table: Mutex::new(table).ok_or(Error::NoMemory)?,
            nested,
            regions: Vec::new(),
            absent: Mutex::new(Absent { frame: None, at: Vec::new() }).ok_or(Error::NoMemory)?,
        })
    }

    /// No memory, and no second-level table: this architecture runs no
    /// guests yet.
    #[cfg(not(target_arch = "x86_64"))]
    pub fn new() -> Result<Self> {
        Ok(Self {
            regions: Vec::new(),
            absent: Mutex::new(Absent { at: Vec::new() }).ok_or(Error::NoMemory)?,
        })
    }

    /// Give the guest `size` bytes of zeroed memory at `base`. Both are whole
    /// pages, and the range is clear of every region already given.
    pub fn add(&mut self, base: u64, size: u64) -> Result<()> {
        if size == 0 || base % PAGE != 0 || size % PAGE != 0 {
            return Err(Error::BadAddress);
        }
        match base.checked_add(size) {
            Some(end) if end <= MAX_GPA => {}
            _ => return Err(Error::BadAddress),
        }
        if self.regions.iter().any(|r| base < r.base + r.size && r.base < base + size) {
            return Err(Error::BadAddress);
        }

        /* The kernel zeroes every frame it hands out: what the guest finds
         * in its memory is what it was given, never what the host left
         * there. A failure part way drops what was taken. */
        let pages = (size / PAGE) as usize;
        let mut runs: Vec<Vec<Frame>> = Vec::new();
        runs.try_reserve_exact(pages.div_ceil(RUN_FRAMES)).map_err(|_| Error::NoMemory)?;
        let mut left = pages;
        while left > 0 {
            let n = left.min(RUN_FRAMES);
            let mut run = Vec::new();
            run.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
            for _ in 0..n {
                run.push(Frame::new().ok_or(Error::NoMemory)?);
            }
            runs.push(run);
            left -= n;
        }
        /* The tables next, which is all that can fail for want of memory:
         * a failure there leaves empty tables and no page mapped. */
        #[cfg(target_arch = "x86_64")]
        let mut table = self.table.lock();
        #[cfg(target_arch = "x86_64")]
        table.prepare(base, size)?;

        self.regions.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.regions.push(Region { base, size, runs });

        /* Owned first, mapped after, so every page the table points at is
         * this value's from before the moment it is reachable. */
        #[cfg(target_arch = "x86_64")]
        {
            let region = self.regions.last().expect("just pushed");
            let mut gpa = base;
            for frame in region.runs.iter().flatten() {
                table.set(gpa, frame.phys())?;
                gpa += PAGE;
            }
        }
        Ok(())
    }

    /// Answer the guest's read of `gpa` -- in the platform's MMIO window and
    /// backed by no memory -- as an empty bus answers one: with all ones. A
    /// page of them is mapped there, read-only, and the guest's instruction
    /// runs again and reads it. What makes this necessary is Linux on a Zen
    /// CPU reading the reset-status register of AMD's FCH at a fixed address,
    /// on hardware that has one and in a guest that does not; all ones is
    /// what it takes for "no such device" and moves on from.
    ///
    /// A write is not answered this way, nor a read anywhere else: discarding
    /// a write needs the instruction's length, which is a decoder this
    /// hypervisor does not have, and a read of an address outside the window
    /// is a guest's mistake, not a probe. So a write to the page is still a
    /// nested fault, and stops the guest; so is one read past its RAM. And
    /// not the local APIC's own page (`XAPIC_PAGE`): a guest's APIC is an
    /// x2APIC, and one that reads it there has left x2APIC mode.
    #[cfg(target_arch = "x86_64")]
    pub fn map_absent(&self, gpa: u64) -> Result<()> {
        let page = gpa & !(PAGE - 1);
        if !MMIO_WINDOW.contains(&page) || page == XAPIC_PAGE || self.region(page, PAGE_SIZE).is_ok() {
            return Err(Error::BadAddress);
        }
        /* The absent pages' lock, then the table's: the one order anything
         * here takes both in. */
        let mut absent = self.absent.lock();
        if absent.at.contains(&page) {
            /* Another of the guest's CPUs read it first, and it is mapped
             * already: this one's read runs again and finds it. */
            return Ok(());
        }
        if absent.at.len() >= MAX_ABSENT_PAGES {
            return Err(Error::NoMemory);
        }
        if absent.frame.is_none() {
            /* Filled a piece at a time: a page of it on the task's stack is
             * a page the stack may not have. */
            const PIECE: usize = 512;
            let frame = Frame::new().ok_or(Error::NoMemory)?;
            for at in (0..PAGE_SIZE).step_by(PIECE) {
                if !frame.write(at, &[0xFF; PIECE]) {
                    return Err(Error::NoMemory);
                }
            }
            absent.frame = Some(frame);
        }
        let hpa = absent.frame.as_ref().map(|f| f.phys()).ok_or(Error::NoMemory)?;
        absent.at.try_reserve(1).map_err(|_| Error::NoMemory)?;
        {
            let mut table = self.table.lock();
            table.prepare(page, PAGE)?;
            table.set_ro(page, hpa)?;
        }
        absent.at.push(page);
        Ok(())
    }

    /// Where the guest has read an absent device, for a report: empty when
    /// there is no memory to say it in.
    pub fn absent_pages(&self) -> Vec<u64> {
        let absent = self.absent.lock();
        let mut pages = Vec::new();
        if pages.try_reserve_exact(absent.at.len()).is_ok() {
            pages.extend_from_slice(&absent.at);
        }
        pages
    }

    /// How much memory the guest has, over every region.
    pub fn size(&self) -> u64 {
        self.regions.iter().map(|r| r.size).sum()
    }

    /// The region `[gpa, gpa + len)` lies whole in.
    fn region(&self, gpa: u64, len: usize) -> Result<usize> {
        let len = len as u64;
        self.regions.iter().position(|r| r.contains(gpa, len)).ok_or(Error::Unmapped)
    }

    /// Copy out `buf.len()` bytes from `gpa`.
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        let region = &self.regions[self.region(gpa, buf.len())?];
        let mut done = 0usize;
        while done < buf.len() {
            let (frame, off) = region.frame(gpa + done as u64);
            let n = (buf.len() - done).min(PAGE_SIZE - off);
            if !frame.read(off, &mut buf[done..done + n]) {
                return Err(Error::Unmapped);
            }
            done += n;
        }
        Ok(())
    }

    /// Copy `data` in at `gpa`. Through `&self`, as `read` is: a copy into
    /// the guest's memory is one more writer of it beside the guest's own
    /// CPUs, which write it with no reference of the host's in sight.
    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let region = &self.regions[self.region(gpa, data.len())?];
        let mut done = 0usize;
        while done < data.len() {
            let (frame, off) = region.frame(gpa + done as u64);
            let n = (data.len() - done).min(PAGE_SIZE - off);
            if !frame.write(off, &data[done..done + n]) {
                return Err(Error::Unmapped);
            }
            done += n;
        }
        Ok(())
    }

    /// The `T` at `gpa`, as it is this instant.
    pub fn read_obj<T: Pod>(&self, gpa: u64) -> Result<T> {
        let mut value = pod::zeroed::<T>();
        self.read(gpa, pod::bytes_of_mut(&mut value))?;
        Ok(value)
    }

    pub fn write_obj<T: Pod>(&self, gpa: u64, value: &T) -> Result<()> {
        self.write(gpa, pod::bytes_of(value))
    }

    /// The nested table, for the VMCB: what `vmrun` translates every guest
    /// physical address through, and what the TLB may keep of it.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn nested(&self) -> hvarch::x86::svm::Nested {
        self.nested
    }
}
