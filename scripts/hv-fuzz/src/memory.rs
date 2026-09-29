//! A guest's memory as the fuzzer keeps it: regions added as the real one
//! adds them -- whole pages, clear of each other, below `MAX_GPA` -- each
//! kept as the pages written so far, a page never written reading as
//! zeroes; every access inside one region or refused, as the real one's is;
//! and the MMIO window's absent pages, which read as all ones once mapped.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Mutex;

use crate::{Error, Result, Vendor};

const PAGE: u64 = 4096;
/// As hv/src/memory.rs has them.
pub const MAX_GPA: u64 = 1 << 48;
const MMIO_WINDOW: core::ops::Range<u64> = 0xC000_0000..0x1_0000_0000;
const MAX_ABSENT_PAGES: usize = 64;
pub const XAPIC_PAGE: u64 = 0xFEE0_0000;

/// A page number's hash: one multiply. The map's default, SipHash, was what
/// a run of the whole machine spent most on after the vCPUs' threads -- the
/// loader and the devices reach memory a few bytes at a time -- and a page
/// number needs no defence against a chosen key.
#[derive(Default)]
struct PageHash(u64);

impl Hasher for PageHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01B3);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type Pages = HashMap<u64, Box<[u8; PAGE as usize]>, BuildHasherDefault<PageHash>>;

struct Region {
    base: u64,
    size: u64,
}

impl Region {
    fn contains(&self, gpa: u64, len: u64) -> bool {
        gpa >= self.base && len <= self.size && gpa - self.base <= self.size - len
    }
}

pub struct GuestMemory {
    regions: Vec<Region>,
    pages: Mutex<Pages>,
    absent: Mutex<Vec<u64>>,
}

impl GuestMemory {
    pub fn new(_vendor: Vendor) -> Result<GuestMemory> {
        Ok(GuestMemory { regions: Vec::new(), pages: Mutex::new(Pages::default()), absent: Mutex::new(Vec::new()) })
    }

    /// `size` bytes from 0: what the device targets run over.
    pub fn with_size(size: u64) -> GuestMemory {
        let mut m = GuestMemory::new(Vendor::Svm).expect("memory");
        m.add(0, size).expect("a region");
        m
    }

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
        self.regions.push(Region { base, size });
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.regions.iter().map(|r| r.size).sum()
    }

    fn inside(&self, gpa: u64, len: usize) -> Result<()> {
        if self.regions.iter().any(|r| r.contains(gpa, len as u64)) { Ok(()) } else { Err(Error::Unmapped) }
    }

    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        self.inside(gpa, buf.len())?;
        let pages = self.pages.lock().unwrap();
        let mut done = 0usize;
        while done < buf.len() {
            let at = gpa + done as u64;
            let off = (at % PAGE) as usize;
            let n = (buf.len() - done).min(PAGE as usize - off);
            match pages.get(&(at / PAGE)) {
                Some(p) => buf[done..done + n].copy_from_slice(&p[off..off + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        self.inside(gpa, data.len())?;
        let mut pages = self.pages.lock().unwrap();
        let mut done = 0usize;
        while done < data.len() {
            let at = gpa + done as u64;
            let off = (at % PAGE) as usize;
            let n = (data.len() - done).min(PAGE as usize - off);
            let p = pages.entry(at / PAGE).or_insert_with(|| Box::new([0; PAGE as usize]));
            p[off..off + n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        Ok(())
    }

    /// As the real one decides it: a page of the MMIO window that is no
    /// region's and not the local APIC's, up to 64 of them.
    pub fn map_absent(&self, gpa: u64) -> Result<()> {
        let page = gpa & !(PAGE - 1);
        if !MMIO_WINDOW.contains(&page) || page == XAPIC_PAGE || self.inside(page, PAGE as usize).is_ok() {
            return Err(Error::BadAddress);
        }
        let mut absent = self.absent.lock().unwrap();
        if absent.contains(&page) {
            return Ok(());
        }
        if absent.len() >= MAX_ABSENT_PAGES {
            return Err(Error::NoMemory);
        }
        absent.push(page);
        Ok(())
    }

    pub fn absent_pages(&self) -> Vec<u64> {
        self.absent.lock().unwrap().clone()
    }
}
