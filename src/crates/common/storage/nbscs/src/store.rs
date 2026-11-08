//! A cs's devices and the chunks on them: the engine nbs-cs.ko runs, each
//! request checked and done against the devices of `format.rs`.
//!
//! What it holds to:
//! - **A fence is durable before it is answered**, on every device of the
//!   cs: a request of an older generation of the volume is refused, after a
//!   crash too.
//! - **An open mark is durable before the first write of a generation** to
//!   a chunk: after a crash, every chunk whose replicas may differ says
//!   open.
//! - **A chunk is held by its number and its incarnation**: a slot of the
//!   chunk its number was before is never written or read for the chunk it
//!   is now, and is freed at the new one's first write.
//! - **A slot is zeros before a chunk takes it**: what was freed is never
//!   read through another chunk.
//! - **A close is durable only after the chunk's data is**: it flushes
//!   first.
//! - **A record whose two copies are both bad is lost**, its slot never
//!   used again, and said.
//! - **A pull moves the chunk's epoch on, durable, before it reads** (N9):
//!   a write of the epoch before is refused here from then on.
//! - **A copy writes no sector a client has** since the join of its set
//!   began: those are marked first, and a copy into a set not joined is
//!   refused.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use nbsmeta::Pieces;

use crate::device::Device;
use crate::format::*;
use crate::{GRANULE, PAGE, SECTOR, SLOT};

/// Why a request was not done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The volume is fenced at a newer generation: this attach is not the
    /// newest.
    Fenced { generation: u64 },
    /// Not fenced at this generation yet -- at `generation`, 0 for none: the
    /// client fences first.
    NotFenced { generation: u64 },
    /// The chunk's number is a newer chunk's here: the request is of the
    /// chunk it was.
    StaleChunk { incarnation: u32 },
    /// The cs knows the chunk at another epoch: one to be checked with its
    /// host's mds.
    Epoch { known: Epoch },
    /// A chunk this cs does not hold: its first write is checked with its
    /// host's mds first.
    Unverified,
    /// Another volume's chunk.
    NotTheVolumes,
    /// Every device that takes new chunks is full.
    NoRoom,
    /// Past the chunk, not whole sectors of the device, empty, or a volume
    /// or generation of 0.
    Bad,
    /// The device the chunk is on failed, or its slot's record was lost.
    Failed,
    /// The device failed under this request: every request of its from now
    /// on is refused.
    Io,
    /// A chunk the cs does not hold: a pull's, or a sum's (N9).
    NotHeld,
    /// No memory for what the request needs: asked again.
    NoMemory,
    /// As many fences as a cs keeps.
    TooManyFences,
}

/// A chunk as a request names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub chunk: u32,
    pub incarnation: u32,
    pub epoch: Epoch,
}

/// Why a device was not taken into the cs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// No superblock, and zeros in its place: to be formatted.
    Blank,
    /// An NBS device, but of another cluster, cs or device number.
    Foreign(Identity),
    /// Something that is not NBS's, or NBS's damaged.
    Corrupt(&'static str),
    /// Too small for a slot of chunks.
    Small,
    /// A request of the device's failed.
    Io,
}

/// A chunk the cs holds, as `holdings` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Holding {
    pub chunk: u32,
    pub incarnation: u32,
    pub state: SlotState,
    pub volume: u32,
    pub generation: u64,
    pub device: u32,
    /// Its slot on the device.
    pub slot: u32,
    /// The epoch the cs knows it at, and the one it was last written at.
    pub known: Epoch,
    pub written: Epoch,
}

/// A device of the cs's, as `devices` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device: u32,
    pub slots: u32,
    pub free: u32,
    pub lost: u32,
    pub failed: bool,
    pub active: bool,
}

/// What opening found that was not as the engine leaves it: said, and put
/// right where it can be.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Repairs {
    /// Pages of which one copy was stale or torn, written again from the
    /// other.
    pub pages: u32,
    /// Records neither copy of which is good: their slots never used again.
    pub lost: u32,
    /// Slots of a chunk another slot holds at a newer incarnation: freed.
    pub stale: u32,
}

struct Dev<D> {
    d: D,
    layout: Layout,
    sup: Super,
    records: Pieces<Record>,
    /// Free slots, the lowest on top.
    free: Pieces<u32>,
    lost: u32,
    dirty: bool,
    failed: bool,
    active: bool,
}

/// A cs: its devices, every chunk on them, every volume's fence.
pub struct Store<D: Device> {
    devs: Vec<Dev<D>>,
    /// Each chunk held: on which device, in which slot.
    at: BTreeMap<u32, (usize, u32)>,
    /// The fence table, the same on every device: a place a volume.
    fences: Vec<Fence>,
    fence_of: BTreeMap<u32, u32>,
    /// The table's places no volume has, the lowest on top.
    fence_free: Vec<u32>,
    /// The chunks found open when their device was taken in -- each at its
    /// incarnation and the generation that wrote it last -- and not written
    /// again since: what this cs held of them in its device's write cache
    /// may be gone (`found_open`).
    found: BTreeMap<u32, (u32, u64)>,
    /// The sets this cs joins (N9), each with its join's `since`: their
    /// clients' writes marked, and a copy into them written only where no
    /// write is.
    joining: BTreeMap<u32, u32>,
    marks: BTreeMap<u32, Marks>,
}

/// The sectors of a chunk that clients have written since the join of its
/// set began (N9): a bit a sector, in memory alone -- a joiner started
/// again gives its joins up, so nothing of a copy outlives its run.
struct Marks {
    set: u32,
    incarnation: u32,
    bits: Vec<u64>,
}

/// A chunk's marks: a bit for each sector of the smallest kind.
const MARK_WORDS: usize = (SLOT / SECTOR) as usize / 64;

impl<D: Device> Default for Store<D> {
    fn default() -> Self {
        Store::new()
    }
}

/// Whether epoch `e` is one before `known`, a chunk's as the cs knows it:
/// refused, vouched for or not. The mds vouches for the epoch that is the
/// set's now, but the cs keeps what it vouched for, and a write in flight
/// comes after the epoch has moved on -- a pull's for a joiner, a replica
/// taken out (found by fuzz/nbs's join target: such a write taken moved the
/// chunk's epoch back, and a writer of the map before the join wrote past
/// the joiner).
fn older(e: Epoch, known: Epoch) -> bool {
    e.set != known.set || e.version < known.version
}

/// A slot's place in its table's pages.
fn page_of(slot: u32) -> u64 {
    u64::from(slot) / RECORDS_PER_PAGE as u64
}

impl<D: Device> Store<D> {
    pub fn new() -> Store<D> {
        Store {
            devs: Vec::new(),
            at: BTreeMap::new(),
            fences: vec![Fence::default(); FENCES_MAX],
            fence_of: BTreeMap::new(),
            fence_free: (0..FENCES_MAX as u32).rev().collect(),
            found: BTreeMap::new(),
            joining: BTreeMap::new(),
            marks: BTreeMap::new(),
        }
    }

    /* ---- the devices ---- */

    /// Device `d`, formatted as `identity`'s, its format's id `format`, and
    /// taken in: whatever was on it is gone. Its fences are the cs's.
    pub fn format_device(&mut self, mut d: D, identity: Identity, format: u64) -> Result<usize, (D, Error)> {
        let Some(layout) = layout(d.bytes()) else { return Err((d, Error::Bad)) };
        if !(d.sector() == SECTOR || d.sector() == PAGE) || format == 0 {
            return Err((d, Error::Bad));
        }
        let sup = Super { identity, sector: d.sector() as u32, slots: layout.slots, records: layout.records, format,
                          seq: 1 };
        let pages = u64::from(layout.slots).div_ceil(RECORDS_PER_PAGE as u64);
        /* The superblock's pages zeros first: a format cut short leaves a
           device that is blank, never one half another's */
        let wiped = d.zero(0, 2 * PAGE).and_then(|_| d.flush())
            .and_then(|_| d.zero(FENCE_AT[0] * PAGE, 2 * FENCE_PAGES * PAGE))
            .and_then(|_| d.zero(layout.table[0], pages * PAGE))
            .and_then(|_| d.zero(layout.table[1], pages * PAGE));
        if wiped.is_err() {
            return Err((d, Error::Io));
        }
        let mut dev = Dev { d, layout, sup, records: Pieces::new(), free: Pieces::new(), lost: 0, dirty: false,
                            failed: false, active: true };
        for _ in 0..layout.slots {
            dev.records.push(Record::default());
        }
        for s in (0..layout.slots).rev() {
            dev.free.push(s);
        }
        /* The cs's fences onto it, then the superblock: A, then B */
        let mut ok = (0..FENCE_PAGES).all(|p| write_fence_page(&mut dev, &self.fences, p, 0).is_ok()
                                            && write_fence_page(&mut dev, &self.fences, p, 1).is_ok());
        ok = ok && dev.d.flush().is_ok();
        let mut page = vec![0u8; PAGE as usize];
        sup.encode(&mut page);
        for copy in SUPER_PAGES {
            ok = ok && dev.d.write(copy * PAGE, &page).is_ok() && dev.d.flush().is_ok();
        }
        if !ok {
            return Err((dev.d, Error::Io));
        }
        self.devs.push(dev);
        Ok(self.devs.len() - 1)
    }

    /// Device `d`, which says it is `expect`'s, read and taken in: its
    /// records, each the good one of its copies, the stale copies written
    /// again; its fences joined with the cs's.
    pub fn open_device(&mut self, mut d: D, expect: Identity) -> Result<(usize, Repairs), (D, OpenError)> {
        let Some(layout) = layout(d.bytes()) else { return Err((d, OpenError::Small)) };
        let mut a = vec![0u8; PAGE as usize];
        let mut b = vec![0u8; PAGE as usize];
        if d.read(SUPER_PAGES[0] * PAGE, &mut a).is_err() || d.read(SUPER_PAGES[1] * PAGE, &mut b).is_err() {
            return Err((d, OpenError::Io));
        }
        let sup = match holds(&a, &b) {
            Holds::Nbs(s) => s,
            /* Zeros where the superblock goes: a blank device, or one whose
               format was cut short */
            Holds::Nothing => return Err((d, OpenError::Blank)),
            Holds::Other => {
                return Err((d, OpenError::Corrupt("its first pages are neither an NBS superblock nor zeros: \
                                                   something else's")));
            }
        };
        if sup.identity != expect {
            return Err((d, OpenError::Foreign(sup.identity)));
        }
        if u64::from(sup.sector) != d.sector() || sup.slots != layout.slots || sup.records != layout.records {
            return Err((d, OpenError::Corrupt("formatted for a device of another size or sector")));
        }
        let mut repairs = Repairs::default();
        let mut dev = Dev { d, layout, sup, records: Pieces::new(), free: Pieces::new(), lost: 0, dirty: false,
                            failed: false, active: true };

        /* Its fences; a page of which one copy is stale or torn written again
           below, as the table's are -- left as it was, a cut in the next
           write of its other copy would leave neither good (found by
           fuzz/nbs's cs target) */
        let mut fences = vec![Fence::default(); FENCES_MAX];
        let mut fences_stale = false;
        for p in 0..FENCE_PAGES {
            if dev.d.read((FENCE_AT[0] + p) * PAGE, &mut a).is_err()
                || dev.d.read((FENCE_AT[1] + p) * PAGE, &mut b).is_err()
            {
                return Err((dev.d, OpenError::Io));
            }
            for k in 0..FENCES_PER_PAGE {
                let at = k * FENCE;
                let fa = Fence::decode(&a[at..], sup.format);
                let fb = Fence::decode(&b[at..], sup.format);
                match choose(fa, fb, |f: &Fence| f.seq) {
                    Some(Decoded::Valid(f)) if f.volume != 0 => fences[p as usize * FENCES_PER_PAGE + k] = f,
                    Some(Decoded::Empty) => {}
                    /* A fence lost is a stale writer let in: not opened */
                    _ => return Err((dev.d, OpenError::Corrupt("a fence of which neither copy is good"))),
                }
            }
            if a != b {
                fences_stale = true;
            }
        }

        /* Its table of slots */
        let pages = u64::from(layout.slots).div_ceil(RECORDS_PER_PAGE as u64);
        let mut stale_pages = Vec::new();
        for p in 0..pages {
            if dev.d.read(layout.table[0] + p * PAGE, &mut a).is_err()
                || dev.d.read(layout.table[1] + p * PAGE, &mut b).is_err()
            {
                return Err((dev.d, OpenError::Io));
            }
            for k in 0..RECORDS_PER_PAGE {
                let slot = p * RECORDS_PER_PAGE as u64 + k as u64;
                if slot >= u64::from(layout.slots) {
                    break;
                }
                let at = k * RECORD;
                let r = match choose(Record::decode(&a[at..], sup.format), Record::decode(&b[at..], sup.format),
                                     |r: &Record| r.seq) {
                    Some(Decoded::Valid(r)) => r,
                    Some(Decoded::Empty) => Record::default(),
                    _ => {
                        dev.lost += 1;
                        repairs.lost += 1;
                        Record { state: SlotState::Lost, ..Record::default() }
                    }
                };
                dev.records.push(r);
            }
            if a != b {
                stale_pages.push(p);
            }
        }
        for s in (0..layout.slots).rev() {
            if dev.records.get(s as usize).is_some_and(|r| r.state == SlotState::Free) {
                dev.free.push(s);
            }
        }
        /* The stale copies made the good ones again, a copy at a time */
        for &p in &stale_pages {
            for copy in 0..2 {
                if write_table_page(&mut dev, p, copy).is_err() || dev.d.flush().is_err() {
                    return Err((dev.d, OpenError::Io));
                }
            }
            repairs.pages += 1;
        }

        let index = self.devs.len();
        self.devs.push(dev);
        /* Its chunks: one held twice is kept at its newer incarnation */
        let held: Vec<(u32, Record)> = {
            let dev = &self.devs[index];
            (0..dev.layout.slots).filter_map(|s| dev.records.get(s as usize).map(|r| (s, *r)))
                .filter(|(_, r)| matches!(r.state, SlotState::Open | SlotState::Written)).collect()
        };
        for (slot, r) in held {
            match self.at.get(&r.chunk).copied() {
                Some((dv, s)) if self.record(dv, s).incarnation >= r.incarnation => {
                    let _ = self.release(index, slot);
                    repairs.stale += 1;
                }
                Some((dv, s)) => {
                    let _ = self.release(dv, s);
                    repairs.stale += 1;
                    self.at.insert(r.chunk, (index, slot));
                }
                None => {
                    self.at.insert(r.chunk, (index, slot));
                }
            }
        }
        /* The chunks it holds open, as it was when it stopped */
        let open: Vec<(u32, u32, u64)> = self.at.iter().filter(|(_, &(dv, _))| dv == index)
            .map(|(&c, &(dv, s))| (c, self.record(dv, s))).filter(|(_, r)| r.state == SlotState::Open)
            .map(|(c, r)| (c, r.incarnation, r.generation)).collect();
        for (c, incarnation, generation) in open {
            self.found.insert(c, (incarnation, generation));
        }
        /* Its fences joined with the cs's: the newer generation of each
           volume, in the cs's place for it. The devices taken in before all
           hold the table as it was; the table written again onto them if
           the join changed it, and onto this one if its own differs */
        let before = self.fences.clone();
        for f in fences.iter().filter(|f| f.volume != 0) {
            match self.fence_of.get(&f.volume).copied() {
                Some(i) => {
                    let cur = &mut self.fences[i as usize];
                    cur.generation = cur.generation.max(f.generation);
                    cur.seq = cur.seq.max(f.seq);
                }
                None => {
                    let Some(i) = self.fence_free.pop() else {
                        let dev = self.devs.remove(index);
                        return Err((dev.d, OpenError::Corrupt("more fences than a cs keeps")));
                    };
                    self.fences[i as usize] = *f;
                    self.fence_of.insert(f.volume, i);
                }
            }
        }
        let others = self.fences != before;
        let this = fences_stale || fences != self.fences;
        for dv in 0..self.devs.len() {
            if (dv == index && !this) || (dv != index && !others) || self.devs[dv].failed {
                continue;
            }
            let dev = &mut self.devs[dv];
            for copy in 0..2 {
                let ok = (0..FENCE_PAGES).all(|p| write_fence_page(dev, &self.fences, p, copy).is_ok())
                    && dev.d.flush().is_ok();
                if !ok {
                    dev.failed = true;
                    break;
                }
            }
            repairs.pages += 1;
        }
        Ok((index, repairs))
    }

    /// Whether the device takes new chunks: false to drain it.
    pub fn set_active(&mut self, dv: usize, active: bool) {
        if let Some(d) = self.devs.get_mut(dv) {
            d.active = active;
        }
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.devs.iter().map(|d| DeviceInfo { device: d.sup.identity.device, slots: d.layout.slots,
                                               free: d.free.len() as u32, lost: d.lost, failed: d.failed,
                                               active: d.active }).collect()
    }

    /// The device of the cs's that is the metadata's device `device`.
    pub fn device_index(&self, device: u32) -> Option<usize> {
        self.devs.iter().position(|d| d.sup.identity.device == device)
    }

    /* ---- fences ---- */

    /// The volume's fence: 0 for none.
    pub fn fence_of(&self, volume: u32) -> u64 {
        self.fence_of.get(&volume).map_or(0, |&i| self.fences[i as usize].generation)
    }

    /// Volume `volume` fenced at `generation`: written to every device and
    /// flushed before it is answered -- with the fence there is now.
    pub fn fence(&mut self, volume: u32, generation: u64) -> Result<u64, Error> {
        if volume == 0 || generation == 0 {
            return Err(Error::Bad);
        }
        let cur = self.fence_of(volume);
        if cur > generation {
            return Err(Error::Fenced { generation: cur });
        }
        if cur == generation {
            return Ok(cur);
        }
        let index = match self.fence_of.get(&volume) {
            Some(&i) => i,
            None => self.fence_free.pop().ok_or(Error::TooManyFences)?,
        };
        let seq = self.fences[index as usize].seq + 1;
        self.fences[index as usize] = Fence { volume, generation, seq };
        self.fence_of.insert(volume, index);
        self.write_fence_page_everywhere(u64::from(index) / FENCES_PER_PAGE as u64)?;
        Ok(generation)
    }

    /// Volume `volume`'s fence taken off: the volume is gone.
    pub fn drop_fence(&mut self, volume: u32) -> Result<(), Error> {
        let Some(index) = self.fence_of.remove(&volume) else { return Ok(()) };
        let seq = self.fences[index as usize].seq + 1;
        self.fences[index as usize] = Fence { volume: 0, generation: 0, seq };
        self.fence_free.push(index);
        self.write_fence_page_everywhere(u64::from(index) / FENCES_PER_PAGE as u64)
    }

    /// The volumes fenced.
    pub fn fenced_volumes(&self) -> Vec<(u32, u64)> {
        self.fence_of.iter().map(|(&v, &i)| (v, self.fences[i as usize].generation)).collect()
    }

    /// A request of `volume` at `generation`: the newest attach's.
    fn check_fence(&self, volume: u32, generation: u64) -> Result<(), Error> {
        if volume == 0 || generation == 0 {
            return Err(Error::Bad);
        }
        let cur = self.fence_of(volume);
        if cur > generation {
            return Err(Error::Fenced { generation: cur });
        }
        if cur < generation {
            return Err(Error::NotFenced { generation: cur });
        }
        Ok(())
    }

    /// The fence table's page `p` in copy A of every device, flushed, then
    /// in copy B: a device whose write fails is taken as failed; none left
    /// that took it, an error.
    fn write_fence_page_everywhere(&mut self, p: u64) -> Result<(), Error> {
        for copy in 0..2 {
            let mut any = false;
            for dev in self.devs.iter_mut().filter(|d| !d.failed) {
                if write_fence_page(dev, &self.fences, p, copy).is_ok() && dev.d.flush().is_ok() {
                    any = true;
                } else {
                    dev.failed = true;
                }
            }
            if !any && !self.devs.is_empty() {
                return Err(Error::Io);
            }
        }
        Ok(())
    }

    /* ---- chunks ---- */

    fn record(&self, dv: usize, slot: u32) -> Record {
        self.devs.get(dv).and_then(|d| d.records.get(slot as usize)).copied().unwrap_or_default()
    }

    /// Slot `slot` of device `dv` holding `r`, its page written in copy A,
    /// flushed, then in copy B, flushed.
    fn put(&mut self, dv: usize, slot: u32, r: Record) -> Result<(), Error> {
        /* Written again in this run: no longer as it was found -- the chunk
           the slot held, and the one it holds now */
        let old = self.record(dv, slot);
        if old.state != SlotState::Free {
            self.found.remove(&old.chunk);
        }
        if r.state != SlotState::Free {
            self.found.remove(&r.chunk);
        }
        let dev = &mut self.devs[dv];
        if let Some(cur) = dev.records.get_mut(slot as usize) {
            *cur = r;
        }
        let p = page_of(slot);
        for copy in 0..2 {
            if write_table_page(dev, p, copy).is_err() || dev.d.flush().is_err() {
                dev.failed = true;
                return Err(Error::Io);
            }
        }
        Ok(())
    }

    /// A slot given back: its record free, the slot on the free list.
    fn release(&mut self, dv: usize, slot: u32) -> Result<(), Error> {
        let old = self.record(dv, slot);
        if old.state == SlotState::Lost {
            return Ok(());
        }
        self.put(dv, slot, Record { seq: old.seq + 1, ..Record::default() })?;
        self.devs[dv].free.push(slot);
        Ok(())
    }

    /// Where the chunk is, its record as a request finds it.
    fn find(&self, c: &ChunkRef) -> Result<Option<(usize, u32, Record)>, Error> {
        let Some((dv, slot)) = self.at.get(&c.chunk).copied() else { return Ok(None) };
        let r = self.record(dv, slot);
        if r.incarnation > c.incarnation {
            return Err(Error::StaleChunk { incarnation: r.incarnation });
        }
        if r.incarnation < c.incarnation {
            /* The chunk its number was: to be freed */
            return Ok(None);
        }
        if self.devs[dv].failed || r.state == SlotState::Lost {
            return Err(Error::Failed);
        }
        Ok(Some((dv, slot, r)))
    }

    /// `offset` and `len` inside a chunk, whole sectors of the device.
    fn span(&self, dv: Option<usize>, offset: u32, len: usize) -> Result<(), Error> {
        let sector = dv.and_then(|i| self.devs.get(i)).map_or(SECTOR, |d| d.d.sector());
        let end = u64::from(offset) + len as u64;
        if len == 0 || end > SLOT || u64::from(offset) % sector != 0 || len as u64 % sector != 0 {
            return Err(Error::Bad);
        }
        Ok(())
    }

    /// The device a new chunk goes onto: one that takes new chunks, with
    /// the most slots free.
    fn pick(&self) -> Option<usize> {
        self.devs.iter().enumerate().filter(|(_, d)| d.active && !d.failed && !d.free.is_empty())
            .max_by_key(|(i, d)| (d.free.len(), usize::MAX - i)).map(|(i, _)| i)
    }

    /// Chunk `c`'s data written: `data` at `offset` in it, by `volume`'s
    /// attach at `generation`. `verified`: the chunk, its incarnation and
    /// epoch checked with the host's mds -- needed for a chunk the cs does
    /// not hold yet, and for an epoch other than the one it knows.
    pub fn write(&mut self, volume: u32, generation: u64, c: ChunkRef, offset: u32, data: &[u8], verified: bool)
        -> Result<(), Error> {
        self.check_fence(volume, generation)?;
        /* A joiner takes no write of an epoch before its join: one from a
           view that old -- a writer taken over since among them, which the
           members fenced against it refuse -- would be marked, and the copy
           keep it over the set's (found by fuzz/nbs's cluster target, N9d).
           Its writer reads its map again, and writes at the join's epoch */
        if let Some(since) = self.joining.get(&c.epoch.set).copied() {
            if c.epoch.version < since {
                return Err(Error::Epoch { known: Epoch { set: c.epoch.set, version: since } });
            }
        }
        let found = self.find(&c)?;
        let (dv, slot) = match found {
            Some((dv, slot, r)) => {
                self.span(Some(dv), offset, data.len())?;
                if r.volume != volume {
                    return Err(Error::NotTheVolumes);
                }
                if r.known != c.epoch && (!verified || older(c.epoch, r.known)) {
                    return Err(Error::Epoch { known: r.known });
                }
                /* The first write of this generation or epoch: the open mark
                   durable before it */
                if r.state != SlotState::Open || r.generation != generation || r.written != c.epoch
                    || r.known != c.epoch
                {
                    let open = Record { state: SlotState::Open, known: c.epoch, written: c.epoch, generation,
                                        seq: r.seq + 1, ..r };
                    self.put(dv, slot, open)?;
                }
                (dv, slot)
            }
            None => {
                if !verified {
                    return Err(Error::Unverified);
                }
                self.take_slot(volume, generation, &c, offset, data.len())?
            }
        };
        /* Marked before it is written, where its set is joined: a copy must
           not write over it, and a mark that cannot be kept refuses it */
        self.mark(&c, offset, data.len())?;
        let dev = &mut self.devs[dv];
        let at = dev.layout.data + u64::from(slot) * SLOT + u64::from(offset);
        if dev.d.write(at, data).is_err() {
            dev.failed = true;
            return Err(Error::Io);
        }
        dev.dirty = true;
        Ok(())
    }

    /// A slot taken for chunk `c`, which the cs does not hold, open for
    /// `volume`'s `generation` at `c`'s epoch -- a client's first write's,
    /// or a copy's (N9).
    fn take_slot(&mut self, volume: u32, generation: u64, c: &ChunkRef, offset: u32, len: usize)
        -> Result<(usize, u32), Error> {
        /* The slot of the chunk its number was -- the mds says this is the
           number's chunk now -- given back first */
        if let Some((odv, oslot)) = self.at.get(&c.chunk).copied() {
            self.release(odv, oslot)?;
            self.at.remove(&c.chunk);
        }
        let dv = self.pick().ok_or(Error::NoRoom)?;
        self.span(Some(dv), offset, len)?;
        let slot = self.devs[dv].free.pop().ok_or(Error::NoRoom)?;
        /* Zeros, flushed before the record that takes the slot is written: a
           device keeps or loses what it holds unflushed in any order, and a
           record kept with its zeros lost would read what the slot held
           through this chunk -- another volume's data. (Found by fuzz/nbs's
           cs target.) */
        let base = self.devs[dv].layout.data + u64::from(slot) * SLOT;
        if self.devs[dv].d.zero(base, SLOT).and_then(|_| self.devs[dv].d.flush()).is_err() {
            self.devs[dv].failed = true;
            return Err(Error::Io);
        }
        let old = self.record(dv, slot);
        let open = Record { state: SlotState::Open, chunk: c.chunk, incarnation: c.incarnation, known: c.epoch,
                            written: c.epoch, volume, generation, seq: old.seq + 1 };
        self.put(dv, slot, open)?;
        self.at.insert(c.chunk, (dv, slot));
        Ok((dv, slot))
    }

    /// Chunk `c`'s data at `offset`, into `buf`: zeros where nothing was
    /// written. `generation` 0 for a read-only attach, which no fence holds
    /// to.
    pub fn read(&mut self, volume: u32, generation: u64, c: ChunkRef, offset: u32, buf: &mut [u8], verified: bool)
        -> Result<(), Error> {
        if generation != 0 {
            self.check_fence(volume, generation)?;
        }
        let Some((dv, slot, r)) = self.find(&c)? else {
            self.span(None, offset, buf.len())?;
            buf.fill(0);
            return Ok(());
        };
        self.span(Some(dv), offset, buf.len())?;
        /* Nothing of one volume's is read through another's */
        if r.volume != volume {
            return Err(Error::NotTheVolumes);
        }
        if r.known != c.epoch && (!verified || older(c.epoch, r.known)) {
            return Err(Error::Epoch { known: r.known });
        }
        self.read_at(dv, slot, offset, buf)
    }

    /// Slot `slot` of device `dv` read at `offset` into `buf`.
    fn read_at(&mut self, dv: usize, slot: u32, offset: u32, buf: &mut [u8]) -> Result<(), Error> {
        let dev = &mut self.devs[dv];
        let at = dev.layout.data + u64::from(slot) * SLOT + u64::from(offset);
        if dev.d.read(at, buf).is_err() {
            dev.failed = true;
            return Err(Error::Io);
        }
        Ok(())
    }

    /// Everything written so far durable, for `volume`'s attach at
    /// `generation`: refused to an older one the fence has taken the volume
    /// from -- its flush done would tell its writer that what it wrote is
    /// durable where a newer writer's reconcile may have put other data
    /// (found by fuzz/nbs's cluster target).
    pub fn flush_for(&mut self, volume: u32, generation: u64) -> Result<(), Error> {
        self.check_fence(volume, generation)?;
        self.flush()
    }

    /// Everything written so far, durable.
    pub fn flush(&mut self) -> Result<(), Error> {
        let mut failed = false;
        for dev in self.devs.iter_mut().filter(|d| d.dirty && !d.failed) {
            if dev.d.flush().is_ok() {
                dev.dirty = false;
            } else {
                dev.failed = true;
                failed = true;
            }
        }
        if failed { Err(Error::Io) } else { Ok(()) }
    }

    /// Chunk `c` closed by `volume`'s attach at `generation`: its data
    /// flushed, then its record no longer open. Whether it was open.
    pub fn close(&mut self, volume: u32, generation: u64, c: ChunkRef) -> Result<bool, Error> {
        self.check_fence(volume, generation)?;
        let Some((dv, slot, r)) = self.find(&c)? else { return Ok(false) };
        if r.state != SlotState::Open || r.volume != volume || r.generation != generation {
            return Ok(false);
        }
        let dev = &mut self.devs[dv];
        if dev.dirty {
            if dev.d.flush().is_err() {
                dev.failed = true;
                return Err(Error::Io);
            }
            dev.dirty = false;
        }
        self.put(dv, slot, Record { state: SlotState::Written, seq: r.seq + 1, ..r })?;
        Ok(true)
    }

    /// Chunk `chunk`'s slot given back, the chunk freed in the metadata at
    /// `incarnation`: whether it held one -- at that incarnation or older.
    pub fn free(&mut self, chunk: u32, incarnation: u32) -> Result<bool, Error> {
        let Some((dv, slot)) = self.at.get(&chunk).copied() else { return Ok(false) };
        if self.record(dv, slot).incarnation > incarnation || self.devs[dv].failed {
            return Ok(false);
        }
        self.release(dv, slot)?;
        self.at.remove(&chunk);
        Ok(true)
    }

    /* ---- joins (N9) ---- */

    /// Clients' writes to set `set`'s chunks marked from now on, as this cs
    /// joins it -- the join begun at `since`: a copy into the set writes only
    /// where no client has. The marks of a join before it dropped: written
    /// while this cs was the joiner of none, the source may have newer
    /// data than they kept.
    pub fn join(&mut self, set: u32, since: u32) {
        if self.joining.get(&set) != Some(&since) {
            self.leave(set);
            self.joining.insert(set, since);
        }
    }

    /// The join of set `set` over -- done, or given up: its marks dropped.
    pub fn leave(&mut self, set: u32) {
        self.joining.remove(&set);
        self.marks.retain(|_, m| m.set != set);
    }

    /// The join of set `set` this cs marks for: its `since`.
    pub fn joins(&self, set: u32) -> Option<u32> {
        self.joining.get(&set).copied()
    }

    /// The sets this cs marks for, each with its join's `since`.
    pub fn joined(&self) -> Vec<(u32, u32)> {
        self.joining.iter().map(|(&s, &since)| (s, since)).collect()
    }

    /// Whether a client has written a sector of chunk `c` from `offset` for
    /// `len` bytes since the join of its set began: what a copy leaves as
    /// it is, and its check leaves out.
    pub fn marked(&self, c: &ChunkRef, offset: u32, len: u32) -> bool {
        let Some(m) = self.marks.get(&c.chunk).filter(|m| m.incarnation == c.incarnation) else { return false };
        let first = offset as usize / SECTOR as usize;
        (first..first + len as usize / SECTOR as usize)
            .any(|s| m.bits.get(s / 64).is_some_and(|w| w & (1 << (s % 64)) != 0))
    }

    /// A client's write of `len` bytes at `offset` in chunk `c` marked,
    /// where its set is joined.
    fn mark(&mut self, c: &ChunkRef, offset: u32, len: usize) -> Result<(), Error> {
        if !self.joining.contains_key(&c.epoch.set) {
            return Ok(());
        }
        if !self.marks.get(&c.chunk).is_some_and(|m| m.incarnation == c.incarnation) {
            let mut bits = Vec::new();
            bits.try_reserve_exact(MARK_WORDS).map_err(|_| Error::NoMemory)?;
            bits.resize(MARK_WORDS, 0);
            self.marks.insert(c.chunk, Marks { set: c.epoch.set, incarnation: c.incarnation, bits });
        }
        let Some(m) = self.marks.get_mut(&c.chunk) else { return Err(Error::NoMemory) };
        let first = offset as usize / SECTOR as usize;
        for s in first..first + len / SECTOR as usize {
            if let Some(w) = m.bits.get_mut(s / 64) {
                *w |= 1 << (s % 64);
            }
        }
        Ok(())
    }

    /// Chunk `c`'s known epoch moved on to `c`'s -- durable -- before a pull
    /// or a sum reads it: a write of the epoch before is refused here from
    /// then on, and its writer, its map read again, writes the joiner too.
    /// An epoch older than the one the cs knows is refused, as a request's
    /// is.
    fn advance(&mut self, c: &ChunkRef) -> Result<(usize, u32), Error> {
        let Some((dv, slot, r)) = self.find(c)? else { return Err(Error::NotHeld) };
        if r.known.set != c.epoch.set || r.known.version > c.epoch.version {
            return Err(Error::Epoch { known: r.known });
        }
        if r.known != c.epoch {
            /* A chunk found open never comes here (`not_found_open`): the
               record written would take its mark off (found by fuzz/nbs's
               cluster target, N9d: a joiner's pull did, and the writer went
               on with a replica short of a write it had done) */
            self.put(dv, slot, Record { known: c.epoch, seq: r.seq + 1, ..r })?;
        }
        Ok((dv, slot))
    }

    /// Chunk `c`'s data at `offset`, into `buf`, for its set's joiner: its
    /// epoch moved on first (`advance`). The caller has had the joiner
    /// vouched for by its host's mds.
    pub fn pull(&mut self, c: ChunkRef, offset: u32, buf: &mut [u8]) -> Result<(), Error> {
        self.not_found_open(&c)?;
        let (dv, slot) = self.advance(&c)?;
        self.span(Some(dv), offset, buf.len())?;
        self.read_at(dv, slot, offset, buf)
    }

    /// The CRC-32C of each `GRANULE` of chunk `c` from `offset` for `len`
    /// bytes -- the last shorter where `len` ends inside one -- for its set's
    /// joiner, its epoch moved on first as a pull's is.
    pub fn sums(&mut self, c: ChunkRef, offset: u32, len: u32) -> Result<Vec<u32>, Error> {
        self.not_found_open(&c)?;
        let (dv, slot) = self.advance(&c)?;
        self.sums_at(dv, slot, offset, len)
    }

    /// Chunk `c` no source of a copy where it was found open: it may lack
    /// what its writer wrote since its last flush, which a joiner would take
    /// as the set's (found by fuzz/nbs's cluster target, N9d: a joiner
    /// copied from such a replica, and once it was active held a write done
    /// as it was before). Refused as failed: the copy takes another source,
    /// or waits for the writer to take this one out, or for a writer's
    /// reconcile to make the chunk the same, which ends the mark.
    fn not_found_open(&self, c: &ChunkRef) -> Result<(), Error> {
        if self.found_open(c.chunk, c.incarnation).is_some() {
            return Err(Error::Failed);
        }
        Ok(())
    }

    /// The same of this cs's own copy of chunk `c`: what a joiner holds its
    /// copy to its source's by.
    pub fn own_sums(&mut self, c: ChunkRef, offset: u32, len: u32) -> Result<Vec<u32>, Error> {
        let Some((dv, slot, _)) = self.find(&c)? else { return Err(Error::NotHeld) };
        self.sums_at(dv, slot, offset, len)
    }

    /// This cs's own copy of chunk `c` at `offset`, into `buf`: what a
    /// joiner holds a granule that differs to its source's by, a sector at a
    /// time.
    pub fn own_read(&mut self, c: ChunkRef, offset: u32, buf: &mut [u8]) -> Result<(), Error> {
        let Some((dv, slot, _)) = self.find(&c)? else { return Err(Error::NotHeld) };
        self.span(Some(dv), offset, buf.len())?;
        self.read_at(dv, slot, offset, buf)
    }

    fn sums_at(&mut self, dv: usize, slot: u32, offset: u32, len: u32) -> Result<Vec<u32>, Error> {
        self.span(Some(dv), offset, len as usize)?;
        let mut sums = Vec::new();
        sums.try_reserve_exact(len.div_ceil(GRANULE) as usize).map_err(|_| Error::NoMemory)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(GRANULE.min(len) as usize).map_err(|_| Error::NoMemory)?;
        let end = offset + len;
        let mut at = offset;
        while at < end {
            let n = GRANULE.min(end - at);
            buf.resize(n as usize, 0);
            self.read_at(dv, slot, at, &mut buf)?;
            sums.push(crc::crc32c::checksum(&buf));
            at += n;
        }
        Ok(sums)
    }

    /// `data` copied into chunk `c` at `offset` for the join of its set,
    /// held as `volume`'s: written where no client's write has marked a
    /// sector -- the client's is newer, and the source has it or will. A
    /// chunk the cs does not hold taken in as a first write takes it, open
    /// at no writer's generation; one it holds opened at the copy's epoch.
    /// Refused where the set is not joined: with no marks kept, a copy could
    /// write over a client's write.
    pub fn copy_in(&mut self, volume: u32, c: ChunkRef, offset: u32, data: &[u8]) -> Result<(), Error> {
        if !self.joining.contains_key(&c.epoch.set) || volume == 0 {
            return Err(Error::Bad);
        }
        let (dv, slot) = match self.find(&c)? {
            Some((dv, slot, r)) => {
                self.span(Some(dv), offset, data.len())?;
                if r.volume != volume {
                    return Err(Error::NotTheVolumes);
                }
                if r.known.set != c.epoch.set || r.known.version > c.epoch.version {
                    return Err(Error::Epoch { known: r.known });
                }
                if r.state != SlotState::Open || r.known != c.epoch {
                    /* A client's open mark kept at its generation, for
                       `close_copy` to leave or close; a closed chunk opened
                       at none, for `close_copy` to close */
                    let generation = if r.state == SlotState::Open { r.generation } else { 0 };
                    self.put(dv, slot, Record { state: SlotState::Open, known: c.epoch, written: c.epoch, generation,
                                                seq: r.seq + 1, ..r })?;
                }
                (dv, slot)
            }
            None => self.take_slot(volume, 0, &c, offset, data.len())?,
        };
        /* The runs of sectors no client has written */
        let sector = SECTOR as usize;
        let first = offset as usize / sector;
        let n = data.len() / sector;
        let marks = self.marks.get(&c.chunk).filter(|m| m.incarnation == c.incarnation);
        let marked = |s: usize| marks.is_some_and(|m| m.bits.get(s / 64).is_some_and(|w| w & (1 << (s % 64)) != 0));
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut k = 0;
        while k < n {
            if marked(first + k) {
                k += 1;
                continue;
            }
            let start = k;
            while k < n && !marked(first + k) {
                k += 1;
            }
            runs.try_reserve(1).map_err(|_| Error::NoMemory)?;
            runs.push((start, k));
        }
        let dev = &mut self.devs[dv];
        for (from, to) in runs {
            let at = dev.layout.data + u64::from(slot) * SLOT + u64::from(offset) + (from * sector) as u64;
            if dev.d.write(at, &data[from * sector..to * sector]).is_err() {
                dev.failed = true;
                return Err(Error::Io);
            }
            dev.dirty = true;
        }
        Ok(())
    }

    /// Chunk `c`'s copy done: its data flushed, then its record closed where
    /// no client has written it since the join began -- one a client has,
    /// at its generation, is that client's to close.
    pub fn close_copy(&mut self, c: ChunkRef) -> Result<(), Error> {
        let Some((dv, slot, r)) = self.find(&c)? else { return Ok(()) };
        /* What the copy took in durable, whatever the record: one a client
           wrote meanwhile stays open at the writer's generation, as any
           chunk it writes, but the join's end makes this cs a member, and a
           power cut after must not take the copy's sectors with it (found
           by fuzz/nbs's cluster target, N9d: the record left open, the
           flush was left out with it) */
        let dev = &mut self.devs[dv];
        if dev.dirty {
            if dev.d.flush().is_err() {
                dev.failed = true;
                return Err(Error::Io);
            }
            dev.dirty = false;
        }
        /* Open at a writer's generation, the chunk that client's to close
           where a client has written it since the join began. A mark from
           before the join -- this cs a member then, taken out with the chunk
           open, its writer closing it on the members it had left -- is
           nobody's but the copy's, whether the copy wrote the chunk or found
           it the source's already (found by fuzz/nbs's cluster target, N9d:
           kept, the joiner's crash made the chunk found open, and as its
           set's last member it was no source for the joiner after it) */
        let written = self.marks.get(&c.chunk).is_some_and(|m| m.incarnation == c.incarnation);
        if r.state != SlotState::Open || (r.generation != 0 && written) {
            return Ok(());
        }
        self.put(dv, slot, Record { state: SlotState::Written, seq: r.seq + 1, ..r })
    }

    /// The chunks of set `set` the cs holds, from chunk number `from` on,
    /// `max` at most: each's number, incarnation, volume and the version it
    /// was last written at -- what its joiner copies.
    pub fn holdings_in(&self, set: u32, from: u32, max: usize) -> Vec<(u32, u32, u32, u32)> {
        self.at.range(from..).filter_map(|(&chunk, &(dv, slot))| {
            let r = self.record(dv, slot);
            (r.known.set == set && r.state != SlotState::Lost && !self.devs[dv].failed)
                .then_some((chunk, r.incarnation, r.volume, r.written.version))
        }).take(max).collect()
    }

    /// Every chunk the cs holds.
    pub fn holdings(&self) -> Vec<Holding> {
        self.at.iter().map(|(&chunk, &(dv, slot))| {
            let r = self.record(dv, slot);
            Holding { chunk, incarnation: r.incarnation, state: r.state, volume: r.volume,
                      generation: r.generation, device: self.devs[dv].sup.identity.device, slot, known: r.known,
                      written: r.written }
        }).collect()
    }

    /// The generation of chunk `chunk`'s writer at incarnation `incarnation`
    /// where its record was open when its device was taken in, and has not
    /// been written since. Its data written but not flushed when the cs
    /// stopped may be gone where the replicas that stayed up have it: a
    /// request of that same writer's is not to be done with it (nbswire's
    /// `serve` refuses it as `Failed`); a newer writer's reconcile makes the
    /// replicas the same.
    pub fn found_open(&self, chunk: u32, incarnation: u32) -> Option<u64> {
        self.found.get(&chunk).filter(|(i, _)| *i == incarnation).map(|&(_, g)| g)
    }

    /// Whether some chunk of `volume`'s is found open at `generation` (as
    /// `found_open`): that writer's flush cannot make durable what may be
    /// gone here.
    pub fn found_open_in(&self, volume: u32, generation: u64) -> bool {
        self.found.iter().any(|(&chunk, &(incarnation, g))| {
            g == generation && self.at.get(&chunk).is_some_and(|&(dv, slot)| {
                let r = self.record(dv, slot);
                r.volume == volume && r.incarnation == incarnation
            })
        })
    }

    /// The chunk's record, where the cs holds it at this incarnation.
    pub fn holding(&self, chunk: u32) -> Option<Holding> {
        let (dv, slot) = self.at.get(&chunk).copied()?;
        let r = self.record(dv, slot);
        Some(Holding { chunk, incarnation: r.incarnation, state: r.state, volume: r.volume,
                       generation: r.generation, device: self.devs[dv].sup.identity.device, slot, known: r.known,
                       written: r.written })
    }
}

/// Device `dev`'s table page `p`, in copy `copy`, as its records are now: a
/// lost record written so that it stays lost.
fn write_table_page<D: Device>(dev: &mut Dev<D>, p: u64, copy: usize) -> Result<(), crate::device::IoError> {
    let mut page = vec![0u8; PAGE as usize];
    for k in 0..RECORDS_PER_PAGE {
        let slot = p * RECORDS_PER_PAGE as u64 + k as u64;
        let Some(r) = dev.records.get(slot as usize) else { break };
        let at = k * RECORD;
        if r.state == SlotState::Lost {
            page[at..at + RECORD].fill(0xFF);
        } else if *r != Record::default() {
            r.encode(dev.sup.format, &mut page[at..]);
        }
    }
    dev.d.write(dev.layout.table[copy] + p * PAGE, &page)
}

/// Device `dev`'s fence page `p`, in copy `copy`, from `fences`.
fn write_fence_page<D: Device>(dev: &mut Dev<D>, fences: &[Fence], p: u64, copy: usize)
    -> Result<(), crate::device::IoError> {
    let mut page = vec![0u8; PAGE as usize];
    for k in 0..FENCES_PER_PAGE {
        let f = fences[p as usize * FENCES_PER_PAGE + k];
        if f.volume != 0 {
            f.encode(dev.sup.format, &mut page[k * FENCE..]);
        }
    }
    dev.d.write((FENCE_AT[copy] + p) * PAGE, &page)
}
