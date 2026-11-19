//! A guest's memory as the fuzzer keeps it: regions added as the real one
//! adds them -- whole pages, clear of each other, below `MAX_GPA` -- each
//! kept as the pages written so far, a page never written reading as
//! zeroes; every access inside one region or refused, as the real one's is;
//! the MMIO window's absent pages, which read as all ones once mapped; and
//! a device's rings mapped (`map`), with the real mapping's bounds and
//! alignment, or refused when a target says so.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::pod::{self, Pod};
use crate::{Error, Result, Vendor};

const PAGE: u64 = 4096;
/// A block of the huge pool, as kcore::frame has it.
const HUGE: u64 = 2 << 20;
/// As hypervisor/hv/src/memory.rs has them.
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

/// Another CPU of the guest's making one chain available while the device
/// works -- after the device's `after`th read of the available index from
/// when it is armed -- and reading the device's `avail_event` right after,
/// as a driver does to decide whether to kick: the moment EVENT_IDX's ask
/// has to survive, a chain published between the device's last look at the
/// ring and its ask for a kick, by a driver that saw no ask yet.
struct LatePost {
    idx: u64,
    ring: u64,
    avail_event: u64,
    size: u16,
    head: u16,
    after: usize,
    /// Once posted: the `avail_event` the driver read.
    seen: Option<u16>,
}

/// What the memory and every mapping of it share: the pages, and the racers
/// that watch reads of them. Their reads and writes are inside the memory's
/// regions, which whoever calls them has checked.
struct Shared {
    pages: Mutex<Pages>,
    racer: Mutex<Option<Racer>>,
    late: Mutex<Option<LatePost>>,
}

/// The real one's `hvarch::x86::svm::Nested`: what an entry names its
/// table by.
pub struct Nested {
    pub root: u64,
    pub id: u64,
}

/// The nested table as the dirty log sees it: which pages of RAM are
/// read-only to the guest.
#[derive(Default)]
pub struct Protected {
    ram: Vec<(u64, u64)>,
    ro: std::collections::BTreeSet<u64>,
}

impl crate::dirty::Table for Protected {
    fn protect(&mut self, gpa: u64) -> bool {
        if !self.ram.iter().any(|&(b, s)| gpa >= b && gpa - b < s) {
            return false;
        }
        self.ro.insert(gpa);
        true
    }
    fn unprotect(&mut self, gpa: u64) -> bool {
        if !self.ram.iter().any(|&(b, s)| gpa >= b && gpa - b < s) {
            return false;
        }
        self.ro.remove(&gpa);
        true
    }
}

pub struct GuestMemory {
    regions: Vec<Region>,
    /// The dirty log, as the real one keeps it (`crate::dirty`), over a
    /// table of which pages are read-only to the guest; and the table's
    /// identity for the TLBs, new with each narrowing.
    log: crate::dirty::Log,
    table: Mutex<Protected>,
    id: AtomicU64,
    shared: Arc<Shared>,
    absent: Mutex<Vec<u64>>,
    /// A host's function's BARs mapped (`map_device`): base and size.
    devices: Mutex<Vec<(u64, u64)>>,
    /// `map` refuses, as the real one does with no address left.
    refuse_maps: AtomicBool,
    /// Mappings made: each one undone is an address the real kernel shoots
    /// down on every CPU, which a device makes only so many of (`held_to`).
    maps: AtomicUsize,
    /// Bytes read and written by `read` and `write`, which a device's data
    /// goes through: what a disk's pass moved, under the platform's lock
    /// (`moved`).
    moved: AtomicU64,
}

impl GuestMemory {
    pub fn new(_vendor: Vendor) -> Result<GuestMemory> {
        Ok(GuestMemory {
            regions: Vec::new(),
            log: crate::dirty::Log::new(),
            table: Mutex::new(Protected::default()),
            id: AtomicU64::new(1),
            shared: Arc::new(Shared { pages: Mutex::new(Pages::default()), racer: Mutex::new(None), late: Mutex::new(None) }),
            absent: Mutex::new(Vec::new()),
            devices: Mutex::new(Vec::new()),
            refuse_maps: AtomicBool::new(false),
            maps: AtomicUsize::new(0),
            moved: AtomicU64::new(0),
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

    /// Have another CPU make the chain at `head` available on the ring
    /// whose index is at `idx` and entries from `ring`, after the device's
    /// `after`th read of the index from now (`LatePost`), and read the
    /// device's word at `avail_event` right after.
    pub fn post_late(&self, idx: u64, ring: u64, avail_event: u64, size: u16, head: u16, after: usize) {
        *self.shared.late.lock().unwrap() = Some(LatePost { idx, ring, avail_event, size, head, after, seen: None });
    }

    /// The late post, once made: the `avail_event` its driver read, the
    /// post over. None while it waits for its read.
    pub fn late_posted(&self) -> Option<u16> {
        let mut late = self.shared.late.lock().unwrap();
        let seen = late.as_ref()?.seen?;
        *late = None;
        Some(seen)
    }

    /// The late post called off before it was made: whether it was still
    /// waiting.
    pub fn cancel_late(&self) -> bool {
        self.shared.late.lock().unwrap().take().is_some_and(|l| l.seen.is_none())
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

    /// The mappings its one device of `queues` queues made since `since`, on
    /// the fuzzer's clock, held to what the device may make: a burst of two
    /// boots' worth of its queues, eight at the least, and one a second after
    /// (`virtio::Maps`) -- each a host-wide shootdown when it is undone,
    /// which a guest setting its queues up over and over must not have at
    /// will.
    pub fn held_to_map_budget(&self, since: u64, queues: usize) {
        let made = self.maps.load(Ordering::Relaxed) as u64;
        let earned = crate::time::boot_time_ns().saturating_sub(since) / 1_000_000_000;
        let burst = MAPS_BURST.max(2 * queues as u64);
        invariant!(made <= burst + earned,
                   "{} ring mappings made in {} s, past the {} and one a second a device may make", made, earned, burst);
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
        self.post_after_read(gpa, len);
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

    /// A read the device made, as the late poster sees it: its chain made
    /// available after the read it waits for, and the device's ask read.
    fn post_after_read(&self, gpa: u64, len: usize) {
        let mut late = self.late.lock().unwrap();
        let Some(l) = late.as_mut() else { return };
        if l.seen.is_some() || !(gpa < l.idx.saturating_add(2) && l.idx < gpa.saturating_add(len as u64)) {
            return;
        }
        if l.after != 0 {
            l.after -= 1;
            return;
        }
        let mut b = [0u8; 2];
        self.read_unraced(l.idx, &mut b);
        let idx = u16::from_le_bytes(b);
        self.write(l.ring + 2 * u64::from(idx % l.size), &l.head.to_le_bytes());
        self.write(l.idx, &idx.wrapping_add(1).to_le_bytes());
        self.read_unraced(l.avail_event, &mut b);
        l.seen = Some(u16::from_le_bytes(b));
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
    /// (`linux::Ram`), its RAM below 4 GiB ending at `low_end` -- below the
    /// devices' pages, or as `new_holed` lays it out, below the hole a host
    /// function's BARs go in (`linux::Ram::of_below`): None for one no
    /// region could hold.
    pub fn with_ram_below(mem_bytes: u64, low_end: u64) -> Option<GuestMemory> {
        let mut m = GuestMemory::new(Vendor::Svm).expect("memory");
        for (base, size) in crate::linux::Ram::of_below(mem_bytes, low_end).regions() {
            m.add(base, size).ok()?;
        }
        Some(m)
    }

    /// The NUMA node the kernel's takes RAM from first: nothing to place
    /// on the host.
    pub fn place_on(&mut self, _node: usize) {}

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
        if !self.log.add(base, size) {
            return Err(Error::NoMemory);
        }
        self.table.lock().unwrap().ram.push((base, size));
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

    /// The real one's: whether `[gpa, gpa + len)` is RAM of one region,
    /// whole.
    pub fn is_ram(&self, gpa: u64, len: u64) -> bool {
        len != 0 && self.backs(gpa, len)
    }

    /// The real one's: the regions, base and size.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.regions.iter().map(|r| (r.base, r.size))
    }

    fn inside(&self, gpa: u64, len: usize) -> Result<()> {
        if self.regions.iter().any(|r| r.contains(gpa, len as u64)) { Ok(()) } else { Err(Error::Unmapped) }
    }

    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        self.inside(gpa, buf.len())?;
        self.shared.read(gpa, buf);
        self.moved.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        self.inside(gpa, data.len())?;
        self.shared.write(gpa, data);
        self.moved.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.log.wrote(gpa, data.len() as u64);
        Ok(())
    }

    /// The real one's: the table's top level and its identity now.
    pub fn nested(&self) -> Nested {
        Nested { root: 1, id: self.id.load(Ordering::Acquire) }
    }

    pub fn table_root(&self) -> u64 {
        1
    }

    /// The real one's steps, over the model table.
    pub fn write_fault(&self, gpa: u64) -> bool {
        if self.inside(gpa & !(PAGE - 1), PAGE as usize).is_err() {
            return false;
        }
        let mut t = self.table.lock().unwrap();
        self.log.fault(&mut *t, gpa)
    }

    fn renew(&self) -> u64 {
        self.id.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn log_start(&self) -> Result<u64> {
        let mut t = self.table.lock().unwrap();
        if !self.log.start(&mut *t) {
            self.log.release(&mut *t);
            self.renew();
            return Err(Error::NoMemory);
        }
        Ok(self.renew())
    }

    pub fn log_take(&self, out: &mut dyn FnMut(u64)) -> Result<(u64, u64)> {
        let mut t = self.table.lock().unwrap();
        let n = self.log.take(Some(&mut *t), out);
        let id = self.renew();
        n.map(|n| (n, id)).ok_or(Error::NoMemory)
    }

    pub fn log_last(&self, out: &mut dyn FnMut(u64)) -> u64 {
        self.log.take(None, out).unwrap_or(0)
    }

    pub fn log_marked(&self) -> u64 {
        self.log.marked()
    }

    pub fn log_stop(&self) {
        let mut t = self.table.lock().unwrap();
        self.log.release(&mut *t);
    }

    /// Whether the page at `gpa` is read-only to the guest now.
    pub fn protected(&self, gpa: u64) -> bool {
        self.table.lock().unwrap().ro.contains(&(gpa & !(PAGE - 1)))
    }

    /// The bytes `read` and `write` have moved so far.
    pub fn moved(&self) -> u64 {
        self.moved.load(Ordering::Relaxed)
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
    /// region's, no BAR's and not the local APIC's, up to 64 of them -- the
    /// BARs looked at under the absent pages' lock, as the real one does.
    pub fn map_absent(&self, gpa: u64) -> Result<()> {
        let page = gpa & !(PAGE - 1);
        if !MMIO_WINDOW.contains(&page) || page == XAPIC_PAGE || self.inside(page, PAGE as usize).is_ok() {
            return Err(Error::BadAddress);
        }
        let mut absent = self.absent.lock().unwrap();
        if self.is_device(page) {
            return Err(Error::BadAddress);
        }
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

    /// As the real one decides it: whole pages, below `MAX_GPA`, clear of
    /// RAM, the local APIC's page, every page of all ones made and every
    /// BAR mapped -- or refused, and nothing mapped.
    pub fn map_device(&self, gpa: u64, hpa: u64, size: u64, _holes: &[u64]) -> Result<()> {
        let end = gpa.checked_add(size).ok_or(Error::BadAddress)?;
        if size == 0 || gpa % PAGE != 0 || hpa % PAGE != 0 || size % PAGE != 0 || end > MAX_GPA
            || self.regions.iter().any(|r| gpa < r.base + r.size && r.base < end)
            || (gpa..end).contains(&XAPIC_PAGE)
        {
            return Err(Error::BadAddress);
        }
        let absent = self.absent.lock().unwrap();
        if absent.iter().any(|&p| p >= gpa && p < end) {
            return Err(Error::BadAddress);
        }
        let mut devices = self.devices.lock().unwrap();
        if devices.iter().any(|&(b, s)| gpa < b + s && b < end) {
            return Err(Error::BadAddress);
        }
        devices.push((gpa, size));
        Ok(())
    }

    /// The BAR at `gpa`, `size` long, unmapped: the table's identity anew.
    pub fn unmap_device(&self, gpa: u64, size: u64) -> Result<u64> {
        let mut devices = self.devices.lock().unwrap();
        let Some(i) = devices.iter().position(|&d| d == (gpa, size)) else { return Err(Error::Unmapped) };
        devices.swap_remove(i);
        Ok(self.id.fetch_add(1, Ordering::AcqRel) + 1)
    }

    pub fn is_device(&self, gpa: u64) -> bool {
        self.devices.lock().unwrap().iter().any(|&(b, s)| gpa >= b && gpa - b < s)
    }
}
