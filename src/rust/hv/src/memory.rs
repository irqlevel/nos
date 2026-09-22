//! Guest physical memory: the host pages a guest's RAM is made of, the
//! nested table that is the guest's only way to them, and the host's only
//! way to read and write them.

use alloc::vec::Vec;

use hvarch::{Error, Result};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::pod::{self, Pod};

#[cfg(target_arch = "x86_64")]
use crate::npt::Npt;

/// The most pages the page allocator hands out in one physically contiguous
/// run. Guest memory does not need to be contiguous -- the nested table
/// translates it page by page -- so a region is as many of these as it
/// takes.
const CHUNK_PAGES: usize = 128;
const CHUNK_BYTES: u64 = (CHUNK_PAGES * PAGE_SIZE) as u64;
const PAGE: u64 = PAGE_SIZE as u64;

/// Where a guest's physical address space ends: four levels of nested table
/// translate 48 bits, and an address above that faults whatever is mapped.
pub const MAX_GPA: u64 = 1 << 48;

/// A guest's memory.
///
/// The guest writes it whenever it runs, on whatever CPU it runs on, so a
/// Rust reference into it would be a promise the guest does not keep --
/// and a reference that changes under the compiler is undefined behaviour
/// however it is used. So nothing here hands one out. Every access is a
/// copy, made with volatile loads and stores, into or out of the host's own
/// memory, and bounds-checked against the region it falls in: an address
/// the guest gave, whatever it is, is at worst an `Unmapped`.
///
/// The nested table is inside, and that is the point of it. A guest reaches
/// exactly what its nested table maps, and the table here maps nothing but
/// pages this value owns -- each one owned before it is mapped, and freed
/// only with the table -- so "the guest can reach host memory it was not
/// given" is not a thing a caller can get wrong.
pub struct GuestMemory {
    regions: Vec<Region>,
    #[cfg(target_arch = "x86_64")]
    npt: Npt,
}

/// One run of guest physical addresses with memory behind it.
struct Region {
    base: u64,
    size: u64,
    /// `CHUNK_BYTES` each, in order; the last may be larger than what is
    /// left of the region, since the allocator rounds up to a power of two,
    /// and what is past the region is neither mapped nor reachable.
    chunks: Vec<DmaBuffer>,
}

impl Region {
    fn contains(&self, gpa: u64, len: u64) -> bool {
        gpa >= self.base && len <= self.size && gpa - self.base <= self.size - len
    }

    /// The chunk `gpa` is in, and where in it: `gpa` is inside the region.
    fn locate(&self, gpa: u64) -> (usize, usize) {
        let off = gpa - self.base;
        ((off / CHUNK_BYTES) as usize, (off % CHUNK_BYTES) as usize)
    }
}

impl GuestMemory {
    /// No memory at all, and a nested table that maps nothing.
    pub fn new() -> Result<Self> {
        Ok(Self {
            regions: Vec::new(),
            #[cfg(target_arch = "x86_64")]
            npt: Npt::new()?,
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

        let mut chunks = Vec::new();
        let mut left = size;
        while left > 0 {
            let bytes = left.min(CHUNK_BYTES);
            let mut chunk = DmaBuffer::new((bytes / PAGE) as usize).ok_or(Error::NoMemory)?;
            /* The page allocator zeroes what it hands out; this says so
             * where it matters most. What the guest finds in its memory is
             * what it was given, never what the host left there. */
            chunk.as_mut_slice().fill(0);
            chunks.try_reserve(1).map_err(|_| Error::NoMemory)?;
            chunks.push(chunk);
            left -= bytes;
        }
        /* The tables first, which is all that can fail for want of memory:
         * a failure there leaves empty tables and no page mapped. */
        #[cfg(target_arch = "x86_64")]
        self.npt.prepare(base, size)?;

        self.regions.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.regions.push(Region { base, size, chunks });

        /* Owned first, mapped after, so every page the table points at is
         * this value's from before the moment it is reachable. */
        #[cfg(target_arch = "x86_64")]
        {
            let region = self.regions.last().expect("just pushed");
            let mut gpa = base;
            for chunk in &region.chunks {
                let mut off = 0u64;
                while off < CHUNK_BYTES && gpa < base + size {
                    self.npt.set(gpa, chunk.phys() + off)?;
                    off += PAGE;
                    gpa += PAGE;
                }
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
            let (chunk, off) = region.locate(gpa + done as u64);
            let n = (buf.len() - done).min(CHUNK_BYTES as usize - off);
            copy_out(&region.chunks[chunk], off, &mut buf[done..done + n])?;
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
            let (chunk, off) = region.locate(gpa + done as u64);
            let n = (data.len() - done).min(CHUNK_BYTES as usize - off);
            copy_in(&mut region.chunks[chunk], off, &data[done..done + n])?;
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

/* The copies themselves: eight bytes at a time where the guest's side is
 * aligned for it, a byte at a time elsewhere, every access volatile and
 * bounds-checked by the buffer. `false` from a buffer is a range the checks
 * above should have refused, and is said as one. */

fn copy_out(chunk: &DmaBuffer, off: usize, dst: &mut [u8]) -> Result<()> {
    let mut i = 0usize;
    while i < dst.len() {
        if (off + i) % 8 == 0 && dst.len() - i >= 8 {
            let word = chunk.load::<u64>(off + i).ok_or(Error::Unmapped)?;
            dst[i..i + 8].copy_from_slice(&word.to_le_bytes());
            i += 8;
        } else {
            dst[i] = chunk.load::<u8>(off + i).ok_or(Error::Unmapped)?;
            i += 1;
        }
    }
    Ok(())
}

fn copy_in(chunk: &mut DmaBuffer, off: usize, src: &[u8]) -> Result<()> {
    let mut i = 0usize;
    while i < src.len() {
        let (n, stored) = if (off + i) % 8 == 0 && src.len() - i >= 8 {
            let word = u64::from_le_bytes(src[i..i + 8].try_into().expect("eight bytes"));
            (8, chunk.store::<u64>(off + i, word))
        } else {
            (1, chunk.store::<u8>(off + i, src[i]))
        };
        if !stored {
            return Err(Error::Unmapped);
        }
        i += n;
    }
    Ok(())
}
