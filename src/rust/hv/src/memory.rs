//! Guest physical memory: the host pages a guest's RAM is made of, the
//! nested table that is the guest's only way to them, and the host's only
//! way to read and write them.

use alloc::vec::Vec;

use hvarch::{Error, Result};
use kcore::consts::PAGE_SIZE;
use kcore::frame::Frame;
use kcore::pod::{self, Pod};

#[cfg(target_arch = "x86_64")]
use crate::npt::Npt;

const PAGE: u64 = PAGE_SIZE as u64;
/// Frames are kept in runs of this many, so that no one allocation of the
/// table of them outgrows a page however large the guest is.
const RUN_FRAMES: usize = PAGE_SIZE / core::mem::size_of::<Frame>();

/// Where a guest's physical address space ends: four levels of nested table
/// translate 48 bits, and an address above that faults whatever is mapped.
pub const MAX_GPA: u64 = 1 << 48;

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
pub struct GuestMemory {
    /* Before the regions, so that it is dropped first: the table goes
     * before the pages it maps are back on the free list. */
    #[cfg(target_arch = "x86_64")]
    npt: Npt,
    regions: Vec<Region>,
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

    fn frame_mut(&mut self, gpa: u64) -> (&mut Frame, usize) {
        let page = ((gpa - self.base) / PAGE) as usize;
        (&mut self.runs[page / RUN_FRAMES][page % RUN_FRAMES], (gpa % PAGE) as usize)
    }
}

impl GuestMemory {
    /// No memory at all, and a nested table that maps nothing.
    pub fn new() -> Result<Self> {
        Ok(Self {
            #[cfg(target_arch = "x86_64")]
            npt: Npt::new()?,
            regions: Vec::new(),
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
        self.npt.prepare(base, size)?;

        self.regions.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.regions.push(Region { base, size, runs });

        /* Owned first, mapped after, so every page the table points at is
         * this value's from before the moment it is reachable. */
        #[cfg(target_arch = "x86_64")]
        {
            let region = self.regions.last().expect("just pushed");
            let mut gpa = base;
            for frame in region.runs.iter().flatten() {
                self.npt.set(gpa, frame.phys())?;
                gpa += PAGE;
            }
        }
        Ok(())
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

    /// Copy `data` in at `gpa`.
    pub fn write(&mut self, gpa: u64, data: &[u8]) -> Result<()> {
        let index = self.region(gpa, data.len())?;
        let region = &mut self.regions[index];
        let mut done = 0usize;
        while done < data.len() {
            let (frame, off) = region.frame_mut(gpa + done as u64);
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

    pub fn write_obj<T: Pod>(&mut self, gpa: u64, value: &T) -> Result<()> {
        self.write(gpa, pod::bytes_of(value))
    }

    /// The top of the nested table, for the VMCB: what `vmrun` translates
    /// every guest physical address through.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn nested_root(&self) -> u64 {
        self.npt.root()
    }
}
