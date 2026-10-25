//! A guest's memory as the fuzzer keeps it: regions added as the real one
//! adds them -- whole pages, clear of each other, below `MAX_GPA` -- each
//! kept as the pages written so far, a page never written reading as
//! zeroes; every access inside one region or refused, as the real one's is;
//! the MMIO window's absent pages, which read as all ones once mapped; and
//! a device's rings mapped (`map`), with the real mapping's bounds and
//! alignment, or refused when a target says so.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::pod::{self, Pod};
use crate::{Error, Result, Vendor};

const PAGE: u64 = 4096;
/// A block of the huge pool, as kcore::frame has it.
const HUGE: u64 = 2 << 20;
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
    /// The huge pool's (`add_huge`).
    huge: bool,
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

/// What the memory and every mapping of it share: the pages, and the racer
/// that watches reads of them. Its reads and writes are inside the memory's
/// regions, which whoever calls them has checked.
struct Shared {
    pages: Mutex<Pages>,
    racer: Mutex<Option<Racer>>,
}

pub struct GuestMemory {
    regions: Vec<Region>,
    shared: Arc<Shared>,
    absent: Mutex<Vec<u64>>,
    /// `map` refuses, as the real one does with no address left.
    refuse_maps: AtomicBool,
    /// Mappings made: each one undone is an address the real kernel shoots
    /// down on every CPU, which a device makes only so many of (`held_to`).
    maps: AtomicUsize,
}

impl GuestMemory {
    pub fn new(_vendor: Vendor) -> Result<GuestMemory> {
        Ok(GuestMemory {
            regions: Vec::new(),
            shared: Arc::new(Shared { pages: Mutex::new(Pages::default()), racer: Mutex::new(None) }),
            absent: Mutex::new(Vec::new()),
            refuse_maps: AtomicBool::new(false),
            maps: AtomicUsize::new(0),
        })
    }

    /// Race the device on the ring whose available index is at `idx`, its
    /// entries from `ring` and its used index at `used`, posting `head`
    /// again and again (`Racer`), `most` times at most -- until `stop_race`.
    pub fn race(&self, idx: u64, ring: u64, used: u64, size: u16, head: u16, most: usize) {
        *self.shared.racer.lock().unwrap() = Some(Racer { idx, ring, used, size, head, posts: 0, left: most });
    }

    /// The race over: how many chains the racer posted.
    pub fn stop_race(&self) -> usize {
        self.shared.racer.lock().unwrap().take().map_or(0, |r| r.posts)
    }

    /// From now on `map` refuses, or maps again: what a device does with its
    /// rings copied instead, the kernel having had no address for them.
    pub fn refuse_maps(&self, refuse: bool) {
        self.refuse_maps.store(refuse, Ordering::Relaxed);
    }

    /// As the real one maps them: the pages from `gpa` that `len` bytes
    /// reach, four at most, all in one region -- reached by offset with the
    /// real mapping's bounds and alignment held (`Mapping`).
    pub fn map(&self, gpa: u64, len: usize) -> Result<Mapping> {
        if gpa % PAGE != 0 || len == 0 {
            return Err(Error::BadAddress);
        }
        let pages = len.div_ceil(PAGE as usize);
        if pages > MAPPED_MAX {
            return Err(Error::BadAddress);
        }
        self.inside(gpa, pages * PAGE as usize)?;
        if self.refuse_maps.load(Ordering::Relaxed) {
            return Err(Error::NoMemory);
        }
        self.maps.fetch_add(1, Ordering::Relaxed);
        Ok(Mapping { shared: self.shared.clone(), base: gpa, bytes: pages * PAGE as usize })
    }

    /// The mappings its one device made since `since`, on the fuzzer's
    /// clock, held to what the device may make: a burst of eight, and one a
    /// second after (`virtio::Maps`) -- each a host-wide shootdown when it
    /// is undone, which a guest setting its queues up over and over must not
    /// have at will.
    pub fn held_to_map_budget(&self, since: u64) {
        let made = self.maps.load(Ordering::Relaxed) as u64;
        let earned = crate::time::boot_time_ns().saturating_sub(since) / 1_000_000_000;
        invariant!(made <= MAPS_BURST + earned,
                   "{} ring mappings made in {} s, past the {} and one a second a device may make", made, earned, MAPS_BURST);
    }
}

/// As `virtio::MAPS_BURST`.
const MAPS_BURST: u64 = 8;

/// As kcore's `frame::MAPPED_MAX`.
const MAPPED_MAX: usize = 4;

/// Pages mapped (`GuestMemory::map`), as kcore's `frame::Mapped` is reached:
/// a value at an offset, inside and aligned for its type or refused, the
/// racer seeing every read as it sees the memory's own.
pub struct Mapping {
    shared: Arc<Shared>,
    base: u64,
    bytes: usize,
}

impl Mapping {
    fn holds<T>(&self, offset: usize) -> bool {
        let size = core::mem::size_of::<T>();
        offset % core::mem::align_of::<T>() == 0 && offset <= self.bytes && size <= self.bytes - offset
    }

    pub fn load<T: Pod>(&self, offset: usize) -> Option<T> {
        if !self.holds::<T>(offset) {
            return None;
        }
        let mut value = pod::zeroed::<T>();
        self.shared.read(self.base + offset as u64, pod::bytes_of_mut(&mut value));
        Some(value)
    }

    pub fn store<T: Pod>(&self, offset: usize, value: T) -> bool {
        if !self.holds::<T>(offset) {
            return false;
        }
        self.shared.write(self.base + offset as u64, pod::bytes_of(&value));
        true
    }

    pub fn load_u16(&self, offset: usize) -> Option<u16> {
        self.load::<u16>(offset)
    }

    pub fn store_u16(&self, offset: usize, value: u16) -> bool {
        self.store::<u16>(offset, value)
    }
}

impl Shared {
    fn read(&self, gpa: u64, buf: &mut [u8]) {
        self.read_unraced(gpa, buf);
        self.race_after_read(gpa, buf.len());
    }

    fn read_unraced(&self, gpa: u64, buf: &mut [u8]) {
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
    }

    fn write(&self, gpa: u64, data: &[u8]) {
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
    }

    /// A read the device made, as the racer sees it: one of the available
    /// index has it publish the next chain, when the ring has room for one.
    /// The racer's ring is one a target laid out inside the memory.
    fn race_after_read(&self, gpa: u64, len: usize) {
        let mut racer = self.racer.lock().unwrap();
        let Some(r) = racer.as_mut() else { return };
        if !(gpa < r.idx.saturating_add(2) && r.idx < gpa.saturating_add(len as u64)) {
            return;
        }
        let mut b = [0u8; 2];
        self.read_unraced(r.idx, &mut b);
        let idx = u16::from_le_bytes(b);
        self.read_unraced(r.used, &mut b);
        if idx.wrapping_sub(u16::from_le_bytes(b)) >= r.size {
            return;
        }
        invariant!(r.left > 0,
                   "a device took {} chains in one call while another CPU kept its ring topped up: a pass with no end",
                   r.posts);
        r.left -= 1;
        r.posts += 1;
        let slot = u64::from(idx % r.size);
        self.write(r.ring + 2 * slot, &r.head.to_le_bytes());
        self.write(r.idx, &idx.wrapping_add(1).to_le_bytes());
    }
}

impl GuestMemory {
    /// `size` bytes from 0: what the device targets run over.
    pub fn with_size(size: u64) -> GuestMemory {
        let mut m = GuestMemory::new(Vendor::Svm).expect("memory");
        m.add(0, size).expect("a region");
        m
    }

    /// A guest's RAM of `mem_bytes`, as `LinuxGuest::new` lays it out
    /// (`linux::Ram`): None for one no region could hold.
    pub fn with_ram(mem_bytes: u64) -> Option<GuestMemory> {
        let mut m = GuestMemory::new(Vendor::Svm).expect("memory");
        for (base, size) in crate::linux::Ram::of(mem_bytes).regions() {
            m.add(base, size).ok()?;
        }
        Some(m)
    }

    pub fn add(&mut self, base: u64, size: u64) -> Result<()> {
        self.add_region(base, size, PAGE, false)
    }

    /// As the real one gives the huge pool's: whole 2 MiB blocks, clear of
    /// every other region -- kept as any region is, a page at a time.
    pub fn add_huge(&mut self, base: u64, size: u64, _leaves_2m: bool) -> Result<()> {
        self.add_region(base, size, HUGE, true)
    }

    fn add_region(&mut self, base: u64, size: u64, unit: u64, huge: bool) -> Result<()> {
        if size == 0 || base % unit != 0 || size % unit != 0 {
            return Err(Error::BadAddress);
        }
        match base.checked_add(size) {
            Some(end) if end <= MAX_GPA => {}
            _ => return Err(Error::BadAddress),
        }
        if self.regions.iter().any(|r| base < r.base + r.size && r.base < base + size) {
            return Err(Error::BadAddress);
        }
        self.regions.push(Region { base, size, huge });
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.regions.iter().map(|r| r.size).sum()
    }

    pub fn huge_size(&self) -> u64 {
        self.regions.iter().filter(|r| r.huge).map(|r| r.size).sum()
    }

    /// Whether `[gpa, gpa + len)` is in one region: what the targets hold
    /// a memory map's RAM to.
    pub fn backs(&self, gpa: u64, len: u64) -> bool {
        self.regions.iter().any(|r| r.contains(gpa, len))
    }

    fn inside(&self, gpa: u64, len: usize) -> Result<()> {
        if self.regions.iter().any(|r| r.contains(gpa, len as u64)) { Ok(()) } else { Err(Error::Unmapped) }
    }

    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        self.inside(gpa, buf.len())?;
        self.shared.read(gpa, buf);
        Ok(())
    }

    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        self.inside(gpa, data.len())?;
        self.shared.write(gpa, data);
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
