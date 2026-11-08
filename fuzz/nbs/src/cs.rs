//! The `cs` target: NBS's chunk server engine (src/crates/common/storage/
//! nbscs) over disks with a write cache, through random histories of
//! fences, writes, reads, flushes, closes and frees -- and power cuts, each
//! write not yet flushed kept, lost or torn at a sector's edge, on its own.
//! The fuzzer is the cs's clients and its host's mds: it decides every
//! chunk's incarnation and epoch, and every volume's generation.
//!
//! Held to, after every cut and the cs opened again:
//! - every fence answered there, at least as new;
//! - every chunk written since its last close open, at a generation at
//!   least the newest answered;
//! - a chunk closed reading exactly what it held when it was closed;
//! - every sector reading its value at the last flush answered, or one
//!   written since;
//! - a chunk never written, freed, or taken again by a new incarnation,
//!   reading zeros where its new chunk was not written;
//! - no record lost: one copy of a page is ever in flight.
//!
//! And between cuts: every read exactly the last write, every refusal the
//! one the request deserves. Before each run: a disk with anything at all
//! past a superblock's bytes in its first two pages -- a boot sector, a
//! filesystem, a partition table -- is never taken for blank, nor written;
//! a cs's of another identity is told apart.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use nbscs::{ChunkRef, Device, Epoch, Error, Identity, IoError, OpenError, SlotState, Store, PAGE, SLOT};

use crate::rng::{mix, Rng};

/// A write the cache holds, in order.
enum Op {
    Write { at: u64, data: Vec<u8> },
    Zero { at: u64, len: u64 },
}

/// A disk: what is durable, and the writes since the last flush.
pub(crate) struct Disk {
    sector: u64,
    bytes: u64,
    /// Where the cs's own ends -- its superblock, fences and table -- and
    /// the chunks' slots begin.
    own_end: u64,
    /// Durable sectors, by number: absent ones zeros.
    durable: BTreeMap<u64, Vec<u8>>,
    pending: Vec<Op>,
    /// The power fails at this many more writes and flushes: the one that
    /// blows it reaches the cache, or not, and fails; every request after
    /// it fails until the cut.
    fuse: Option<u64>,
    dead: bool,
    /// The next cut tears a write of the cs's own inside, where it changes
    /// bytes, whatever the dice say.
    tear_next: bool,
    /// Whether the last cut found a write of the cs's own in the cache: one
    /// copy of a record in flight, which rot must leave be -- a record with
    /// both copies hurt at once is lost whatever a cs does.
    own_cut: bool,
    /// Where copy A of the fences and of the table are, and how long each.
    copy_a: [(u64, u64); 2],
}

impl Disk {
    pub(crate) fn new(sector: u64, bytes: u64) -> Disk {
        let l = nbscs::layout(bytes);
        let own_end = l.map_or(bytes, |l| l.data);
        let table = l.map_or((0, 0), |l| (l.table[0], l.table[1] - l.table[0]));
        /* The fences' copy A: slot 0's pages 2 to 34 */
        let copy_a = [(2 * PAGE, 32 * PAGE), table];
        Disk { sector, bytes, own_end, durable: BTreeMap::new(), pending: Vec::new(), fuse: None, dead: false,
               tear_next: false, own_cut: false, copy_a }
    }

    fn apply(durable: &mut BTreeMap<u64, Vec<u8>>, sector: u64, op: &Op, keep: &dyn Fn(u64) -> bool) {
        match op {
            Op::Write { at, data } => {
                for (k, chunk) in data.chunks(sector as usize).enumerate() {
                    let n = at / sector + k as u64;
                    if keep(n) {
                        durable.insert(n, chunk.to_vec());
                    }
                }
            }
            Op::Zero { at, len } => {
                let (first, last) = (at / sector, (at + len) / sector);
                let gone: Vec<u64> = durable.range(first..last).map(|(&n, _)| n).filter(|&n| keep(n)).collect();
                for n in gone {
                    durable.remove(&n);
                }
            }
        }
    }

    fn sector_now(&self, n: u64) -> Vec<u8> {
        let s = self.sector;
        let mut v = self.durable.get(&n).cloned().unwrap_or_else(|| vec![0; s as usize]);
        for op in &self.pending {
            match op {
                Op::Write { at, data } => {
                    let first = at / s;
                    let count = data.len() as u64 / s;
                    if n >= first && n < first + count {
                        let k = (n - first) as usize * s as usize;
                        v.copy_from_slice(&data[k..k + s as usize]);
                    }
                }
                Op::Zero { at, len } => {
                    if n >= at / s && n < (at + len) / s {
                        v.fill(0);
                    }
                }
            }
        }
        v
    }

    /// The power cut: each write the cache held kept whole, lost, or torn
    /// -- some of its sectors kept -- and now and then, in the cs's own
    /// pages, one sector torn inside: no device should, some do, and a
    /// record's CRC is there for it. A chunk's data has no CRC of the cs's:
    /// its sectors are taken to be written whole, as NVMe's atomic write
    /// unit of at least a block promises.
    /// A write or a flush counted down the fuse: whether this one blows it.
    fn burn(&mut self) -> bool {
        match self.fuse {
            Some(n) if n <= 1 => {
                self.fuse = None;
                self.dead = true;
                true
            }
            Some(n) => {
                self.fuse = Some(n - 1);
                false
            }
            None => false,
        }
    }

    pub(crate) fn cut(&mut self, r: &mut Rng) {
        self.fuse = None;
        self.dead = false;
        let pending = std::mem::take(&mut self.pending);
        self.own_cut = pending.iter().any(|op| matches!(op, Op::Write { at, .. } if *at < self.own_end));
        let s = self.sector as usize;
        for op in &pending {
            /* One of the cs's own sectors the write changes, torn inside,
               between its first and last changed byte: new up to the tear, as
               it was past it -- a record that spans it is garbage */
            let mut torn: Option<(u64, Vec<u8>)> = None;
            if let Op::Write { at, data } = op {
                let forced = self.tear_next && *at < self.own_end;
                if *at < self.own_end && (forced || r.chance(25)) {
                    let old_of = |n: u64| self.durable.get(&n).cloned().unwrap_or_else(|| vec![0; s]);
                    let changed: Vec<usize> = (0..data.len() / s)
                        .filter(|&k| old_of(at / self.sector + k as u64) != data[k * s..(k + 1) * s]).collect();
                    if let Some(k) = r.pick(&changed) {
                        let n = at / self.sector + k as u64;
                        let old = old_of(n);
                        let new = &data[k * s..(k + 1) * s];
                        let first = (0..s).find(|&i| old[i] != new[i]).unwrap_or(0);
                        let last = (0..s).rfind(|&i| old[i] != new[i]).unwrap_or(0);
                        if last > first {
                            let cut_at = first + 1 + r.below((last - first) as u64) as usize;
                            let mut v = old.clone();
                            v[..cut_at].copy_from_slice(&new[..cut_at]);
                            torn = Some((n, v));
                            self.tear_next = false;
                        }
                    }
                }
            }
            match r.below(10) {
                0..=5 => Disk::apply(&mut self.durable, self.sector, op, &|_| true),
                6..=7 => {}
                _ => {
                    let seed = r.below(u64::MAX);
                    Disk::apply(&mut self.durable, self.sector, op, &|n| mix(seed, n) % 2 == 0);
                }
            }
            if let Some((n, v)) = torn {
                /* Torn, and so its fate is the tear's: a torn sector is never
                   also kept whole */
                self.durable.insert(n, v);
            }
        }
        self.tear_next = false;
    }

    /// Bit rot after a cut: one bit of a durable sector of copy A of the
    /// cs's own flipped -- only where the cut found nothing of the cs's own
    /// in flight, so that copy B is the good one the record is. Whether a
    /// sector was there to rot.
    fn rot(&mut self, r: &mut Rng) -> bool {
        if self.own_cut || !self.pending.is_empty() {
            return false;
        }
        let (start, len) = self.copy_a[r.below(2) as usize];
        let (first, last) = (start / self.sector, (start + len) / self.sector);
        let held: Vec<u64> = self.durable.range(first..last).map(|(&n, _)| n).collect();
        let Some(n) = r.pick(&held) else { return false };
        let bit = r.below(self.sector * 8);
        if let Some(v) = self.durable.get_mut(&n) {
            v[(bit / 8) as usize] ^= 1 << (bit % 8);
        }
        true
    }

    /// The power to fail at the `n`th write or flush from now (the `join`
    /// target's); and whether it has.
    pub(crate) fn arm(&mut self, n: u64) {
        self.fuse = Some(n.max(1));
    }

    pub(crate) fn dead(&self) -> bool {
        self.dead
    }

    /// One bit of sector `n` flipped as it is now, its cache flushed first:
    /// a device that rots what it was given (the `join` target's).
    pub(crate) fn flip(&mut self, n: u64, bit: u64) {
        self.flush();
        let s = self.sector as usize;
        let v = self.durable.entry(n).or_insert_with(|| vec![0; s]);
        v[(bit / 8) as usize % s] ^= 1 << (bit % 8);
    }

    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for op in &pending {
            Disk::apply(&mut self.durable, self.sector, op, &|_| true);
        }
    }
}

/// The engine's handle on a disk the fuzzer keeps too.
pub(crate) struct Handle(pub(crate) Rc<RefCell<Disk>>);

impl Device for Handle {
    fn bytes(&self) -> u64 {
        self.0.borrow().bytes
    }

    fn sector(&self) -> u64 {
        self.0.borrow().sector
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let d = self.0.borrow();
        if d.dead {
            return Err(IoError);
        }
        let s = d.sector;
        assert!(offset % s == 0 && buf.len() as u64 % s == 0 && offset + buf.len() as u64 <= d.bytes,
                "a read the device cannot do: {} bytes at {}", buf.len(), offset);
        for (k, out) in buf.chunks_mut(s as usize).enumerate() {
            out.copy_from_slice(&d.sector_now(offset / s + k as u64));
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), IoError> {
        let mut d = self.0.borrow_mut();
        let s = d.sector;
        assert!(offset % s == 0 && data.len() as u64 % s == 0 && offset + data.len() as u64 <= d.bytes,
                "a write the device cannot do: {} bytes at {}", data.len(), offset);
        if d.dead {
            return Err(IoError);
        }
        d.pending.push(Op::Write { at: offset, data: data.to_vec() });
        if d.burn() { Err(IoError) } else { Ok(()) }
    }

    fn flush(&mut self) -> Result<(), IoError> {
        let mut d = self.0.borrow_mut();
        if d.dead || d.burn() {
            return Err(IoError);
        }
        d.flush();
        Ok(())
    }

    fn zero(&mut self, offset: u64, len: u64) -> Result<(), IoError> {
        let mut d = self.0.borrow_mut();
        assert!(offset % d.sector == 0 && len % d.sector == 0 && offset + len <= d.bytes, "a zero past the device");
        if d.dead {
            return Err(IoError);
        }
        d.pending.push(Op::Zero { at: offset, len });
        if d.burn() { Err(IoError) } else { Ok(()) }
    }
}

/// A sector of a chunk, as the model has it: its value at the last flush
/// answered, and every value written since -- each a byte the whole sector
/// is, 0 for zeros.
#[derive(Clone, Default)]
struct MSector {
    durable: u8,
    since: Vec<u8>,
}

impl MSector {
    fn now(&self) -> u8 {
        self.since.last().copied().unwrap_or(self.durable)
    }
}

/// A chunk number, as the fuzzer -- its mds -- has it.
struct MChunk {
    incarnation: u32,
    epoch: Epoch,
    volume: u32,
    /// The sectors the run wrote, by number in the chunk.
    sectors: BTreeMap<u32, MSector>,
    /// The newest generation a write was answered at since the last close.
    open: Option<u64>,
    /// The generation of a write cut by the power before it was answered:
    /// its open mark may be durable, or not.
    maybe_open: Option<u64>,
    /// Closed since its last write: its every sector durable.
    closed: bool,
    /// What its sectors held at the last close asked -- answered or cut --
    /// since which no write was answered: a record that says closed after a
    /// cut says the chunk is exactly this.
    at_close: Option<BTreeMap<u32, u8>>,
    /// Whether the cs was told the number's chunk before was freed.
    told_free: bool,
}

const VOLUMES: u32 = 4;
const CHUNKS: u32 = 12;
/// The sectors of a chunk a run writes: a few, from each end and between.
const SECTORS: [u32; 6] = [0, 1, 2, 7, 4096, 8191];

struct Run {
    r: Rng,
    disks: Vec<Rc<RefCell<Disk>>>,
    identities: Vec<Identity>,
    store: Store<Handle>,
    /// Each volume's generation now, and the fence answered.
    generation: [u64; VOLUMES as usize + 1],
    fenced: [u64; VOLUMES as usize + 1],
    chunks: Vec<MChunk>,
    sector: u64,
    trace: bool,
    /// What the run did, by kind: NBS_CS_STATS=1 prints it.
    stats: BTreeMap<&'static str, u64>,
    /// The chunk last written: half the writes and closes go to it, so that
    /// a close finds its chunk's data still in the cache.
    last: Option<u32>,
}

fn sectors_per_chunk(sector: u64) -> u32 {
    (SLOT / sector) as u32
}

impl Run {
    fn cref(&self, c: u32) -> ChunkRef {
        let m = &self.chunks[c as usize];
        ChunkRef { chunk: c, incarnation: m.incarnation, epoch: m.epoch }
    }

    /// A sector's number in the device's sectors of the chunk.
    fn sector_index(&self, k: u32) -> u32 {
        let per = sectors_per_chunk(self.sector);
        SECTORS[k as usize % SECTORS.len()].min(per - 1)
    }

    fn count(&mut self, what: &'static str) {
        *self.stats.entry(what).or_default() += 1;
    }

    /// Every slot of every device free, held by a chunk, or lost -- none
    /// leaked, none twice.
    fn accounted(&self) -> Result<(), String> {
        let held = self.store.holdings();
        for d in self.store.devices() {
            let on = held.iter().filter(|h| h.device == d.device).count() as u32;
            if d.free + on + d.lost != d.slots {
                return Err(format!("device {}: {} free, {} held, {} lost, of {} slots", d.device, d.free, on, d.lost,
                                   d.slots));
            }
        }
        Ok(())
    }

    fn reopen(&mut self) -> Result<(), String> {
        let mut store = Store::new();
        let mut order: Vec<usize> = (0..self.disks.len()).collect();
        if self.r.chance(50) {
            order.reverse();
        }
        for i in order {
            match store.open_device(Handle(self.disks[i].clone()), self.identities[i]) {
                Ok((_, rep)) => {
                    if self.trace {
                        eprintln!("  reopen disk {}: {:?}", i, rep);
                    }
                    if rep.pages != 0 {
                        *self.stats.entry("repaired pages").or_default() += u64::from(rep.pages);
                    }
                    if rep.lost != 0 {
                        return Err(format!("disk {}: {} records lost -- both copies of a page were torn", i,
                                           rep.lost));
                    }
                }
                Err((_, e)) => return Err(format!("disk {} not opened again: {:?}", i, e)),
            }
        }
        self.store = store;
        Ok(())
    }

    /// The power cut, the cs opened again, and everything held to the model.
    fn cut(&mut self) -> Result<(), String> {
        self.count("cuts");
        let pending: usize = self.disks.iter().map(|d| d.borrow().pending.len()).sum();
        if pending != 0 {
            self.count("cuts with writes cached");
        }
        for d in &self.disks {
            d.borrow_mut().cut(&mut self.r);
        }
        if self.r.chance(30) {
            let i = self.r.below(self.disks.len() as u64) as usize;
            if self.disks[i].borrow_mut().rot(&mut self.r) {
                self.count("bits rotted");
            }
        }
        self.reopen()?;
        for v in 1..=VOLUMES {
            let f = self.store.fence_of(v);
            if f < self.fenced[v as usize] {
                return Err(format!("volume {} fenced at {} after the cut, {} answered", v, f,
                                   self.fenced[v as usize]));
            }
            /* What the cs has now is what the run's clients go on from */
            self.fenced[v as usize] = f;
            self.generation[v as usize] = self.generation[v as usize].max(f);
        }
        for c in 0..CHUNKS {
            let held = self.store.holding(c);
            let m = &self.chunks[c as usize];
            if let Some(g) = m.open {
                match held {
                    Some(h) if h.incarnation == m.incarnation && h.state == SlotState::Open && h.generation >= g => {}
                    other => {
                        return Err(format!("chunk {} written at generation {} since its close, after the cut: {:?}",
                                           c, g, other));
                    }
                }
            }
            if m.closed {
                if let Some(h) = held.filter(|h| h.incarnation == m.incarnation) {
                    if h.state != SlotState::Written {
                        return Err(format!("chunk {} closed, after the cut {:?}", c, h.state));
                    }
                }
            }
        }
        /* A chunk whose record says closed: exactly what it held at that close */
        for c in 0..CHUNKS {
            let m = &self.chunks[c as usize];
            let closed = self.store.holding(c)
                .is_some_and(|h| h.incarnation == m.incarnation && h.state == SlotState::Written);
            let Some(at_close) = m.at_close.clone().filter(|_| closed) else { continue };
            let cref = self.cref(c);
            let volume = m.volume;
            for (k, want) in at_close {
                let got = self.read_sector(volume, cref, k)?;
                if got != want {
                    return Err(format!("chunk {} closed, after the cut: sector {} reads {}, {} at the close", c, k,
                                       got, want));
                }
            }
        }
        /* Every sector one of the values it may have, and from now on that */
        for c in 0..CHUNKS {
            let cref = self.cref(c);
            let volume = self.chunks[c as usize].volume;
            let keys: Vec<u32> = self.chunks[c as usize].sectors.keys().copied().collect();
            for k in keys {
                let got = self.read_sector(volume, cref, k)?;
                let m = &mut self.chunks[c as usize];
                let s = m.sectors.get_mut(&k).unwrap();
                let closed = m.closed;
                if got != s.durable && !s.since.contains(&got) {
                    return Err(format!("chunk {} sector {} reads {} after the cut: {} at the last flush, {:?} since",
                                       c, k, got, s.durable, s.since));
                }
                if closed && got != s.now() {
                    return Err(format!("chunk {} sector {}, closed at {}, reads {} after the cut", c, k, s.now(), got));
                }
                s.durable = got;
                s.since.clear();
            }
            let m = &mut self.chunks[c as usize];
            if m.open.is_some() {
                m.closed = false;
            }
        }
        Ok(())
    }

    /// One sector of chunk `c` read back, by volume `volume`'s read-only
    /// view (generation 0), as a byte: the whole sector must be that byte.
    fn read_sector(&mut self, volume: u32, cref: ChunkRef, k: u32) -> Result<u8, String> {
        let mut buf = vec![0u8; self.sector as usize];
        let offset = k * self.sector as u32;
        match self.store.read(volume, 0, cref, offset, &mut buf, true) {
            Ok(()) => {}
            Err(e) => return Err(format!("chunk {} sector {} not read: {:?}", cref.chunk, k, e)),
        }
        let b = buf[0];
        if buf.iter().any(|&x| x != b) {
            return Err(format!("chunk {} sector {} not one value: it starts {:?}", cref.chunk, k, &buf[..8]));
        }
        Ok(b)
    }

    /// A fence's copy B torn by a cut, the cs opened again, and the same
    /// volume's next fence cut in its copy A: a fence page left with one
    /// copy torn would have neither good, and the cs not open.
    fn fence_tears(&mut self) -> Result<(), String> {
        let v = 1 + self.r.below(u64::from(VOLUMES)) as u32;
        for fuse in [3, 1] {
            self.generation[v as usize] = self.generation[v as usize].max(self.fenced[v as usize]) + 1;
            let g = self.generation[v as usize];
            {
                let mut d = self.disks[0].borrow_mut();
                d.fuse = Some(fuse);
                d.tear_next = true;
            }
            match self.store.fence(v, g) {
                Err(Error::Io) if self.dead() => {}
                Ok(f) => self.fenced[v as usize] = f,
                Err(e) => return Err(format!("fence of volume {} at {} in the tears: {:?}", v, g, e)),
            }
            self.count("fence tears");
            self.cut()?;
        }
        Ok(())
    }

    /// Whether a disk's power has failed under the step.
    fn dead(&self) -> bool {
        self.disks.iter().any(|d| d.borrow().dead)
    }

    fn step(&mut self, step: u64) -> Result<(), String> {
        /* Now and then the power fails within the step's next few writes */
        if self.r.chance(12) {
            let i = self.r.below(self.disks.len() as u64) as usize;
            self.disks[i].borrow_mut().fuse = Some(1 + self.r.below(6));
        }
        self.op(step)?;
        if self.dead() {
            /* The cut it was: what the step did not get answered may be
               there or not */
            self.count("cuts in a request");
            return self.cut();
        }
        for d in &self.disks {
            d.borrow_mut().fuse = None;
        }
        self.accounted().map_err(|e| format!("step {}: {}", step, e))
    }

    fn op(&mut self, step: u64) -> Result<(), String> {
        let c = match self.last {
            Some(l) if self.r.chance(50) => l,
            _ => self.r.below(u64::from(CHUNKS)) as u32,
        };
        let v = 1 + self.r.below(u64::from(VOLUMES)) as u32;
        let what = self.r.below(100);
        if self.trace {
            let fuse: Vec<Option<u64>> = self.disks.iter().map(|d| d.borrow().fuse).collect();
            eprintln!("step {}: op {} chunk {} volume {} fuses {:?}", step, what, c, v, fuse);
        }
        match what {
            /* A fence: the volume's newest attach, or now and then an older
               one's */
            0..=9 => {
                if self.r.chance(70) {
                    self.generation[v as usize] += 1;
                }
                let g = self.generation[v as usize].saturating_sub(u64::from(self.r.chance(15)));
                let got = self.store.fence(v, g.max(1));
                if got == Err(Error::Io) && self.dead() {
                    return Ok(());
                }
                let want = if g.max(1) < self.fenced[v as usize] {
                    Err(Error::Fenced { generation: self.fenced[v as usize] })
                } else {
                    Ok(g.max(1))
                };
                if got != want {
                    return Err(format!("step {}: fence of volume {} at {}: {:?}, {:?} expected", step, v, g, got, want));
                }
                if let Ok(g) = got {
                    self.fenced[v as usize] = g;
                }
            }
            /* A write: of the chunk's volume, at its fence */
            10..=49 => {
                let m = &self.chunks[c as usize];
                let v = m.volume;
                let fence = self.fenced[v as usize];
                /* Now and then an attach older than the fence, or newer than
                   the cs was told of */
                let g = match self.r.below(100) {
                    0..=7 => fence.saturating_sub(1),
                    8..=11 => fence + 1,
                    _ => fence,
                };
                let first = self.r.below(SECTORS.len() as u64) as u32;
                let count = 1 + self.r.below(3) as u32;
                let start = self.sector_index(first);
                let per = sectors_per_chunk(self.sector);
                let count = count.min(per - start);
                let vals: Vec<u8> = (0..count).map(|_| 1 + self.r.below(255) as u8).collect();
                let mut data = Vec::with_capacity(count as usize * self.sector as usize);
                for &b in &vals {
                    data.extend(std::iter::repeat(b).take(self.sector as usize));
                }
                let cref = self.cref(c);
                let verified = self.r.chance(60);
                let mut got = self.store.write(v, g, cref, start * self.sector as u32, &data, verified);
                let held = self.store.holding(c);
                if !verified && matches!(got, Err(Error::Unverified) | Err(Error::Epoch { .. })) {
                    /* As the module does: asked of the mds, which vouches */
                    match got {
                        Err(Error::Unverified) if held.is_some_and(|h| h.incarnation >= cref.incarnation) => {
                            return Err(format!("step {}: chunk {} held at {:?}, and its write unverified", step, c,
                                               held));
                        }
                        _ => {}
                    }
                    got = self.store.write(v, g, cref, start * self.sector as u32, &data, true);
                }
                if got == Err(Error::Io) && self.dead() {
                    /* Cut in the write: its sectors may hold its data, and
                       the chunk may be open again */
                    let m = &mut self.chunks[c as usize];
                    for (i, &b) in vals.iter().enumerate() {
                        m.sectors.entry(start + i as u32).or_default().since.push(b);
                    }
                    m.closed = false;
                    m.maybe_open = Some(g);
                    return Ok(());
                }
                let want_err = if g == 0 {
                    Some(Error::Bad)
                } else if g < fence {
                    Some(Error::Fenced { generation: fence })
                } else if g > fence || fence == 0 {
                    Some(Error::NotFenced { generation: fence })
                } else {
                    None
                };
                let kind = match &got {
                    Ok(()) => "writes",
                    Err(Error::Fenced { .. }) => "writes fenced",
                    Err(Error::NotFenced { .. }) => "writes not fenced",
                    Err(Error::Io) => "writes cut",
                    Err(Error::NoRoom) => "writes with no room",
                    Err(Error::Bad) => "writes bad",
                    Err(_) => "writes refused otherwise",
                };
                self.count(kind);
                if !verified {
                    self.count("writes unverified at first");
                }
                match (got, want_err) {
                    (Ok(()), None) => {
                        let m = &mut self.chunks[c as usize];
                        for (i, &b) in vals.iter().enumerate() {
                            m.sectors.entry(start + i as u32).or_default().since.push(b);
                        }
                        m.open = Some(m.open.map_or(g, |o| o.max(g)));
                        m.closed = false;
                        m.at_close = None;
                        self.last = Some(c);
                    }
                    (Err(e), Some(w)) if e == w => {}
                    (Err(Error::NoRoom), None) => {}
                    (got, want) => {
                        return Err(format!("step {}: write of chunk {} by volume {} at {} (fenced {}): {:?}, {:?} \
                                            expected", step, c, v, g, fence, got, want));
                    }
                }
            }
            /* A read, by the writer at its fence: exactly what is there now */
            50..=69 => {
                let m = &self.chunks[c as usize];
                let v = m.volume;
                if self.fenced[v as usize] == 0 {
                    return Ok(());
                }
                let pick = self.r.below(SECTORS.len() as u64) as u32;
                let k = self.sector_index(pick);
                let m = &self.chunks[c as usize];
                let want = m.sectors.get(&k).map_or(0, |s| s.now());
                let mut buf = vec![0u8; self.sector as usize];
                let cref = self.cref(c);
                /* Now and then by another volume's read-only attach: refused
                   where the cs holds the chunk -- nothing of one volume's
                   is read through another's -- zeros where it does not */
                if self.r.chance(10) {
                    let other = 1 + v % VOLUMES;
                    let held = self.store.holding(c).is_some_and(|h| h.incarnation == cref.incarnation);
                    match self.store.read(other, 0, cref, k * self.sector as u32, &mut buf, true) {
                        Err(Error::NotTheVolumes) if held => {}
                        Ok(()) if !held && buf.iter().all(|&x| x == 0) => {}
                        got => {
                            return Err(format!("step {}: chunk {} of volume {} read by volume {} (held {}): {:?}",
                                               step, c, v, other, held, got));
                        }
                    }
                    self.count("reads by another volume");
                    return Ok(());
                }
                let g = self.fenced[v as usize];
                match self.store.read(v, g, cref, k * self.sector as u32, &mut buf, true) {
                    Err(Error::Io) if self.dead() => {}
                    Ok(()) if buf.iter().all(|&x| x == want) => {}
                    Ok(()) => {
                        return Err(format!("step {}: chunk {} sector {} reads {:?}..., {} was last written", step, c,
                                           k, &buf[..4], want));
                    }
                    Err(e) => return Err(format!("step {}: chunk {} sector {} not read: {:?}", step, c, k, e)),
                }
            }
            /* A flush: everything written durable */
            70..=79 => {
                match self.store.flush() {
                    Err(Error::Io) if self.dead() => return Ok(()),
                    r => r.map_err(|e| format!("step {}: flush: {:?}", step, e))?,
                }
                for m in &mut self.chunks {
                    for s in m.sectors.values_mut() {
                        s.durable = s.now();
                        s.since.clear();
                    }
                }
            }
            /* A close: its data flushed, then the chunk no longer open */
            80..=87 => {
                let m = &self.chunks[c as usize];
                let v = m.volume;
                let g = self.fenced[v as usize];
                if g == 0 {
                    return Ok(());
                }
                let cref = self.cref(c);
                let held = self.store.holding(c).is_some_and(|h| h.incarnation == cref.incarnation);
                let was_open = m.open == Some(g) && held;
                let may = m.maybe_open == Some(g) && held;
                let now: BTreeMap<u32, u8> = m.sectors.iter().map(|(&k, s)| (k, s.now())).collect();
                let got = match self.store.close(v, g, cref) {
                    Err(Error::Io) if self.dead() => {
                        /* Cut in the close: open still, or closed */
                        let m = &mut self.chunks[c as usize];
                        m.at_close = Some(now);
                        if m.open.is_some() {
                            m.maybe_open = Some(g);
                        }
                        m.open = None;
                        m.closed = false;
                        return Ok(());
                    }
                    r => r.map_err(|e| format!("step {}: close of chunk {}: {:?}", step, c, e))?,
                };
                if got != was_open && !may {
                    return Err(format!("step {}: close of chunk {} said {}, open at {:?} (fence {})", step, c, got,
                                       m.open, g));
                }
                self.chunks[c as usize].maybe_open = None;
                if got {
                    self.count("closes");
                    let m = &mut self.chunks[c as usize];
                    m.at_close = Some(now);
                    for s in m.sectors.values_mut() {
                        s.durable = s.now();
                        s.since.clear();
                    }
                    m.open = None;
                    m.closed = true;
                }
            }
            /* The chunk freed in the metadata and its number taken again by
               a new incarnation, of another volume: the cs told of the free,
               or not -- a cs that missed it */
            88..=93 => {
                let tell = self.r.chance(60);
                self.count(if tell { "frees told" } else { "frees missed" });
                let m = &self.chunks[c as usize];
                if tell {
                    match self.store.free(c, m.incarnation) {
                        Err(Error::Io) if self.dead() => {}
                        r => {
                            r.map_err(|e| format!("step {}: free of {}: {:?}", step, c, e))?;
                        }
                    }
                }
                let m = &mut self.chunks[c as usize];
                m.incarnation += 1;
                m.volume = v;
                m.epoch = Epoch { set: m.epoch.set + 1, version: 1 };
                m.sectors.clear();
                m.open = None;
                m.maybe_open = None;
                m.closed = false;
                m.at_close = None;
                m.told_free = tell;
            }
            /* The chunk's set changed: its version up */
            94..=95 => {
                self.chunks[c as usize].epoch.version += 1;
            }
            /* A stale request: the number's chunk before, which the cs holds
               the new one of */
            96..=97 => {
                let m = &self.chunks[c as usize];
                let held = self.store.holding(c);
                if m.incarnation > 1 && held.is_some_and(|h| h.incarnation == m.incarnation) {
                    let mut stale = self.cref(c);
                    stale.incarnation -= 1;
                    let mut buf = vec![0u8; self.sector as usize];
                    let got = self.store.read(m.volume, 0, stale, 0, &mut buf, true);
                    *self.stats.entry("stale reads").or_default() += 1;
                    if got == Err(Error::Io) && self.dead() {
                        return Ok(());
                    }
                    if got != Err(Error::StaleChunk { incarnation: m.incarnation }) {
                        return Err(format!("step {}: chunk {}'s incarnation before read: {:?}", step, c, got));
                    }
                }
            }
            /* The power cut, or a fence's copies torn in turn */
            98 if self.r.chance(50) => self.fence_tears()?,
            _ => self.cut()?,
        }
        if self.trace {
            eprintln!("step {}: ok", step);
        }
        Ok(())
    }
}

/// What a disk that is not a cs's of this identity opens as: blank where it
/// is zeros, refused and left as it is where it has anything else in its
/// first two pages, told apart where it is another cs's.
fn strangers(r: &mut Rng, sector: u64) -> Result<(), String> {
    let id = Identity { cluster: [7; 16], cs: 3, device: 99 };
    let d = Rc::new(RefCell::new(Disk::new(sector, 12 * SLOT)));
    match Store::new().open_device(Handle(d.clone()), id) {
        Err((_, OpenError::Blank)) => {}
        other => return Err(format!("a blank disk opened as {:?}", other.map(|(_, rep)| rep).map_err(|(_, e)| e))),
    }
    /* One byte past a superblock's: a boot sector's signature, a GPT
       header, ext's superblock at 1 KiB, or anywhere in the two pages */
    let at = match r.below(4) {
        0 => 510,
        1 => 512,
        2 => 1024 + 56,
        _ => 128 + r.below(2 * PAGE - 128),
    };
    {
        let mut disk = d.borrow_mut();
        let mut v = vec![0u8; sector as usize];
        v[(at % sector) as usize] = 1 + r.below(255) as u8;
        disk.durable.insert(at / sector, v);
    }
    let before = d.borrow().durable.clone();
    match Store::new().open_device(Handle(d.clone()), id) {
        Err((_, OpenError::Corrupt(_))) => {}
        other => {
            return Err(format!("a disk with a byte at {} of its first pages opened as {:?}", at,
                               other.map(|(_, rep)| rep).map_err(|(_, e)| e)));
        }
    }
    if d.borrow().durable != before || !d.borrow().pending.is_empty() {
        return Err(format!("a disk with a byte at {} of its first pages was written by its open", at));
    }
    /* Another cs's: formatted as its, opened as this one's */
    let other = Rc::new(RefCell::new(Disk::new(sector, 12 * SLOT)));
    let theirs = Identity { cluster: [7; 16], cs: 4, device: 99 };
    if let Err((_, e)) = Store::new().format_device(Handle(other.clone()), theirs, 5) {
        return Err(format!("a disk not formatted: {:?}", e));
    }
    match Store::new().open_device(Handle(other.clone()), id) {
        Err((_, OpenError::Foreign(i))) if i == theirs => Ok(()),
        got => Err(format!("another cs's disk opened as {:?}", got.map(|(_, rep)| rep).map_err(|(_, e)| e))),
    }
}

pub fn run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x6373));
    let sector = if r.chance(30) { PAGE } else { 512 };
    strangers(&mut r, sector)?;
    let disks = 1 + r.below(3) as usize;
    let mut identities = Vec::new();
    let mut handles = Vec::new();
    for i in 0..disks {
        /* Small disks, so that a run fills them: 12 to 40 slots */
        let slots = 12 + r.below(29);
        handles.push(Rc::new(RefCell::new(Disk::new(sector, slots * SLOT))));
        identities.push(Identity { cluster: [7; 16], cs: 3, device: 10 + i as u32 });
    }
    let mut store = Store::new();
    for (i, d) in handles.iter().enumerate() {
        if let Err((_, e)) = store.format_device(Handle(d.clone()), identities[i], 1 + r.below(u64::MAX - 1)) {
            return Err(format!("disk {} not formatted: {:?}", i, e));
        }
    }
    let chunks = (0..CHUNKS).map(|c| MChunk {
        incarnation: 1,
        epoch: Epoch { set: c * 7, version: 1 },
        volume: 1 + c % VOLUMES,
        sectors: BTreeMap::new(),
        open: None,
        maybe_open: None,
        closed: false,
        at_close: None,
        told_free: false,
    }).collect();
    let mut run = Run { r, disks: handles, identities, store, generation: [0; VOLUMES as usize + 1],
                        fenced: [0; VOLUMES as usize + 1], chunks, sector, trace, stats: BTreeMap::new(),
                        last: None };
    for step in 1..=steps {
        run.step(step)?;
    }
    /* And a last cut, everything held once more */
    run.cut()?;
    if std::env::var_os("NBS_CS_STATS").is_some() {
        eprintln!("cs seed {}: sector {}, {} disks: {:?}", seed, sector, disks, run.stats);
    }
    Ok(())
}
