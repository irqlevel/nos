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

/// Another CPU of the guest's, keeping a ring topped up while a device takes
/// from it -- by plain writes to memory, with no exit to stop it: each time
/// the device reads the available index, one more chain is published, the
/// chain at `head` again, while the used index says the ring has room for
/// it. A device that follows that for as long as it goes on holds its CPU in
/// the host for good, and the racer says so after `left` posts.
struct Racer {
    /// The available ring's index, and its first entry.
    idx: u64,
    ring: u64,
    /// The used ring's index: what the device has given back.
    used: u64,
    size: u16,
    head: u16,
    posts: usize,
    left: usize,
}

pub struct GuestMemory {
    regions: Vec<Region>,
    pages: Mutex<Pages>,
    absent: Mutex<Vec<u64>>,
    racer: Mutex<Option<Racer>>,
}

impl GuestMemory {
    pub fn new(_vendor: Vendor) -> Result<GuestMemory> {
        Ok(GuestMemory {
            regions: Vec::new(),
            pages: Mutex::new(Pages::default()),
            absent: Mutex::new(Vec::new()),
            racer: Mutex::new(None),
        })
    }

    /// Race the device on the ring whose available index is at `idx`, its
    /// entries from `ring` and its used index at `used`, posting `head`
    /// again and again (`Racer`), `most` times at most -- until `stop_race`.
    pub fn race(&self, idx: u64, ring: u64, used: u64, size: u16, head: u16, most: usize) {
        *self.racer.lock().unwrap() = Some(Racer { idx, ring, used, size, head, posts: 0, left: most });
    }

    /// The race over: how many chains the racer posted.
    pub fn stop_race(&self) -> usize {
        self.racer.lock().unwrap().take().map_or(0, |r| r.posts)
    }

    /// A read the device made, as the racer sees it: one of the available
    /// index has it publish the next chain, when the ring has room for one.
    fn race_after_read(&self, gpa: u64, len: usize) {
        let mut racer = self.racer.lock().unwrap();
        let Some(r) = racer.as_mut() else { return };
        if !(gpa < r.idx.saturating_add(2) && r.idx < gpa.saturating_add(len as u64)) {
            return;
        }
        let mut b = [0u8; 2];
        if self.read_unraced(r.idx, &mut b).is_err() {
            return;
        }
        let idx = u16::from_le_bytes(b);
        if self.read_unraced(r.used, &mut b).is_err() {
            return;
        }
        if idx.wrapping_sub(u16::from_le_bytes(b)) >= r.size {
            return;
        }
        invariant!(r.left > 0,
                   "a device took {} chains in one call while another CPU kept its ring topped up: a pass with no end",
                   r.posts);
        r.left -= 1;
        r.posts += 1;
        let slot = u64::from(idx % r.size);
        let _ = self.write(r.ring + 2 * slot, &r.head.to_le_bytes());
        let _ = self.write(r.idx, &idx.wrapping_add(1).to_le_bytes());
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
        self.read_unraced(gpa, buf)?;
        self.race_after_read(gpa, buf.len());
        Ok(())
    }

    fn read_unraced(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
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

    /// A word read whole, as the real one reads it (one access); refused at
    /// an odd address, as there. Through `read`, so a racer sees it.
    pub fn read_u16(&self, gpa: u64) -> Result<u16> {
        if gpa % 2 != 0 {
            return Err(Error::BadAddress);
        }
        let mut b = [0u8; 2];
        self.read(gpa, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn write_u16(&self, gpa: u64, value: u16) -> Result<()> {
        if gpa % 2 != 0 {
            return Err(Error::BadAddress);
        }
        self.write(gpa, &value.to_le_bytes())
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
