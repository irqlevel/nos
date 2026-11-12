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
//! - **Another volume's chunk is read or written only once the host's mds
//!   vouches** that the volume's map names it (N10): a snapshot's or a
//!   clone's map shares the chunks of what it was made of. Written, it is
//!   the writer's volume's from then on, as a reconcile looks for it.
//! - **A chunk the cs does not hold is read as zeros only once the host's
//!   mds vouches** that the volume's map names it (N10): one freed since --
//!   a copy-up done, the old chunk named by no map -- is not a chunk never
//!   written, and a read of it on its way from before is refused.
//! - **A copy-up's chunk is durable before it is answered** (N10), its data
//!   before its record, and never written over once held: what a client
//!   wrote to it since is kept.
//!
//! Its operations are `async fn`s over an asynchronous device (N12): each
//! read, write and flush a future of the device's, the store's tables in
//! cells borrowed between two awaits and never across one, so that the
//! operations of many requests may be in flight at once on one task. Over
//! a device that answers at once, an operation ends at its first poll
//! (`now`). Those in flight together are held to these:
//! - **Turns** (`turns.rs`): a chunk's requests one at a time, in the order
//!   they came; a volume's together, and its fence alone -- after every
//!   request of the volume before it, before every one after, so that no
//!   request of an older generation is done once a newer one's fence is
//!   answered.
//! - **Commit rounds**: a record or a fence changed marks its page; a round
//!   writes copy A of every page marked, on every device, flushes, then
//!   copy B, flushed -- never both copies of a page in flight -- and every
//!   request whose page it took is answered once it is done: the opens,
//!   closes and frees that came together made durable by one write of each
//!   copy and two flushes.
//! - **Counted flushes**: a flush covers what was done before it began --
//!   each device counts its data writes done, and a flush the count it
//!   began at -- so a close, a copy's end or a client's flush waits for a
//!   flush begun after the writes it needs, joining one in flight that did.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::future::poll_fn;
use core::task::Poll;

use nbsmeta::Pieces;

use crate::device::{Device, IoError};
use crate::format::*;
use crate::turns::Turns;
use crate::{COPY_PIECE, GRANULE, PAGE, SECTOR, SLOT};

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
    /// A chunk this cs does not hold, or another volume's (N10): checked
    /// with its host's mds first.
    Unverified,
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


/// A device of the cs's: the device, where things are on it and what its
/// superblock says -- none of which changes once it is taken in -- and what
/// does, in a cell of its own.
struct Dev<D> {
    d: D,
    layout: Layout,
    sup: Super,
    st: RefCell<DevState>,
}

struct DevState {
    records: Pieces<Record>,
    /// Free slots, the lowest on top.
    free: Pieces<u32>,
    lost: u32,
    /// Data writes done on it, counted; how many of them the flushes done
    /// cover -- those done before a flush began; and the flush in flight,
    /// with the count it began at.
    writes: u64,
    flushed: u64,
    flushing: Option<u64>,
    /// Flushes ended, counted: what a wait for one is polled again for.
    ended: u64,
    failed: bool,
    active: bool,
}

impl<D> Dev<D> {
    fn new(d: D, layout: Layout, sup: Super, records: Pieces<Record>, free: Pieces<u32>, lost: u32) -> Dev<D> {
        Dev { d, layout, sup, st: RefCell::new(DevState { records, free, lost, writes: 0, flushed: 0, flushing: None,
                                                          ended: 0, failed: false, active: true }) }
    }

    fn record(&self, slot: u32) -> Record {
        self.st.borrow().records.get(slot as usize).copied().unwrap_or_default()
    }

    fn failed(&self) -> bool {
        self.st.borrow().failed
    }

    fn fail(&self) {
        self.st.borrow_mut().failed = true;
    }

    /// Where slot `slot`'s data begins, plus `offset`.
    fn data_at(&self, slot: u32, offset: u64) -> u64 {
        self.layout.data + u64::from(slot) * SLOT + offset
    }

    /// Its table page `p` as its records are now: a lost record written so
    /// that it stays lost.
    fn table_page(&self, p: u64) -> Vec<u8> {
        let mut page = vec![0u8; PAGE as usize];
        let st = self.st.borrow();
        for k in 0..RECORDS_PER_PAGE {
            let slot = p * RECORDS_PER_PAGE as u64 + k as u64;
            let Some(r) = st.records.get(slot as usize) else { break };
            let at = k * RECORD;
            if r.state == SlotState::Lost {
                page[at..at + RECORD].fill(0xFF);
            } else if *r != Record::default() {
                r.encode(self.sup.format, &mut page[at..]);
            }
        }
        page
    }
}

/// A flush in flight on a device, its mark taken off however it ends.
struct Flushing<'a> {
    st: &'a RefCell<DevState>,
}

impl Drop for Flushing<'_> {
    fn drop(&mut self) {
        let mut st = self.st.borrow_mut();
        st.flushing = None;
        st.ended = st.ended.wrapping_add(1);
    }
}

impl<D: Device> Dev<D> {
    /// A data write -- a chunk's, a slot's zeros -- counted once done: what
    /// a flush begun after it covers.
    async fn write_data(&self, at: u64, data: &[u8]) -> Result<(), IoError> {
        let done = self.d.write(at, data).await;
        if done.is_ok() {
            self.st.borrow_mut().writes += 1;
        }
        done
    }

    /// Zeros over `len` bytes from `at`, counted as one data write.
    async fn zero_data(&self, at: u64, len: u64) -> Result<(), IoError> {
        let done = self.d.zero(at, len).await;
        if done.is_ok() {
            self.st.borrow_mut().writes += 1;
        }
        done
    }

    /// A flush of the device, one at a time: the data writes done as it
    /// begins covered once it is done. Failed, the device fails.
    async fn flush_counted(&self) -> Result<(), IoError> {
        poll_fn(|_| if self.st.borrow().flushing.is_none() { Poll::Ready(()) } else { Poll::Pending }).await;
        let mark = {
            let mut st = self.st.borrow_mut();
            st.flushing = Some(st.writes);
            st.writes
        };
        let held = Flushing { st: &self.st };
        let done = self.d.flush().await;
        drop(held);
        let mut st = self.st.borrow_mut();
        match done {
            Ok(()) => st.flushed = st.flushed.max(mark),
            Err(_) => st.failed = true,
        }
        done
    }

    /// Every data write done on the device before now durable: a flush in
    /// flight that began after them waited for, or one begun now -- after
    /// the one in flight, where that began before them.
    async fn durable(&self) -> Result<(), IoError> {
        let need = self.st.borrow().writes;
        loop {
            let flushing = {
                let st = self.st.borrow();
                if st.failed {
                    return Err(IoError);
                }
                if st.flushed >= need {
                    return Ok(());
                }
                st.flushing
            };
            match flushing {
                Some(mark) => {
                    poll_fn(|_| if self.st.borrow().flushing != Some(mark) { Poll::Ready(()) } else { Poll::Pending })
                        .await
                }
                None => self.flush_counted().await?,
            }
        }
    }

    /// Its table page `p`, in copy `copy`, as its records are now.
    async fn write_table_page(&self, p: u64, copy: usize) -> Result<(), IoError> {
        let page = self.table_page(p);
        self.d.write(self.layout.table[copy] + p * PAGE, &page).await
    }

    /// Its fence page `p`, in copy `copy`, from `fences`.
    async fn write_fence_page(&self, fences: &[Fence], p: u64, copy: usize) -> Result<(), IoError> {
        let page = fence_page(fences, self.sup.format, p);
        self.d.write((FENCE_AT[copy] + p) * PAGE, &page).await
    }
}

/// Fence page `p` of `fences`, as a device of format `format` holds it.
fn fence_page(fences: &[Fence], format: u64, p: u64) -> Vec<u8> {
    let mut page = vec![0u8; PAGE as usize];
    for k in 0..FENCES_PER_PAGE {
        let f = fences[p as usize * FENCES_PER_PAGE + k];
        if f.volume != 0 {
            f.encode(format, &mut page[k * FENCE..]);
        }
    }
    page
}

/// A cs: its devices, every chunk on them, every volume's fence.
pub struct Store<D: Device> {
    devs: RefCell<Vec<Rc<Dev<D>>>>,
    s: RefCell<State>,
    /// A chunk's turn, by its number; a volume's, by its number.
    chunk_turns: Turns,
    volume_turns: Turns,
    commits: RefCell<Commits>,
}

/// The records and fences on their way to the devices: the pages marked
/// since the round running began, and the rounds -- the next to begin,
/// and the last done.
struct Commits {
    table: BTreeMap<usize, BTreeSet<u64>>,
    fences: BTreeSet<u64>,
    next: u64,
    done: u64,
    running: bool,
    /// Rounds ended, done or not, counted.
    ended: u64,
}

/// A round running: its pages marked again and the round let go, where it
/// ends before it is done -- dropped mid-way.
struct Round<'a> {
    commits: &'a RefCell<Commits>,
    round: u64,
    table: BTreeMap<usize, BTreeSet<u64>>,
    fences: BTreeSet<u64>,
    finished: bool,
}

impl Drop for Round<'_> {
    fn drop(&mut self) {
        let mut c = self.commits.borrow_mut();
        c.running = false;
        c.ended = c.ended.wrapping_add(1);
        if self.finished {
            c.done = c.done.max(self.round);
        } else {
            for (dv, pages) in core::mem::take(&mut self.table) {
                c.table.entry(dv).or_default().extend(pages);
            }
            c.fences.extend(core::mem::take(&mut self.fences));
        }
    }
}

struct State {
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

/// A slot taken off a device's free list for a copy-up: given back where
/// the copy ends before its record is written.
struct Held<'a, D> {
    dev: &'a Dev<D>,
    slot: Option<u32>,
}

impl<D> Drop for Held<'_, D> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.dev.st.borrow_mut().free.push(slot);
        }
    }
}

/// A device taken back out of an `Rc` no one else holds: the device the
/// caller handed in, given back with a refusal.
fn unwrap_dev<D>(dev: Rc<Dev<D>>) -> D {
    match Rc::try_unwrap(dev) {
        Ok(dev) => dev.d,
        Err(_) => panic!("invariant: a device given back is held elsewhere"),
    }
}

impl<D: Device> Store<D> {
    pub fn new() -> Store<D> {
        Store {
            devs: RefCell::new(Vec::new()),
            s: RefCell::new(State {
                at: BTreeMap::new(),
                fences: vec![Fence::default(); FENCES_MAX],
                fence_of: BTreeMap::new(),
                fence_free: (0..FENCES_MAX as u32).rev().collect(),
                found: BTreeMap::new(),
                joining: BTreeMap::new(),
                marks: BTreeMap::new(),
            }),
            chunk_turns: Turns::default(),
            volume_turns: Turns::default(),
            commits: RefCell::new(Commits { table: BTreeMap::new(), fences: BTreeSet::new(), next: 1, done: 0,
                                            running: false, ended: 0 }),
        }
    }

    /// Turns held or waited for, and rounds running: none once every
    /// operation is over.
    pub fn busy(&self) -> usize {
        self.chunk_turns.in_use() + self.volume_turns.in_use() + usize::from(self.commits.borrow().running)
    }

    /// What has moved in the store, counted: a turn let go, a round ended,
    /// a flush ended -- all that an operation waiting in the store waits
    /// for. An executor that polls its operations again until this and its
    /// devices' own moves stay put has polled every one that could go on.
    pub fn moves(&self) -> u64 {
        let devices = self.devs.borrow().iter().fold(0u64, |n, d| n.wrapping_add(d.st.borrow().ended));
        self.chunk_turns.released().wrapping_add(self.volume_turns.released())
            .wrapping_add(self.commits.borrow().ended).wrapping_add(devices)
    }

    /// What is held, in words: for a report of a wait that never ends.
    pub fn held(&self) -> alloc::string::String {
        let c = self.commits.borrow();
        let flushing: Vec<Option<u64>> = self.devs.borrow().iter().map(|d| d.st.borrow().flushing).collect();
        alloc::format!("chunk turns {}, volume turns {}, rounds next {} done {} running {}, pages marked {:?} \
                        fences {:?}, flushes in flight {:?}", self.chunk_turns.held(), self.volume_turns.held(), c.next,
                       c.done, c.running, c.table, c.fences, flushing)
    }

    /* ---- commit rounds ---- */

    /// Until round `round` is done: run here where no round runs, and
    /// waited for where one does.
    async fn committed(&self, round: u64) {
        loop {
            let (done, running) = {
                let c = self.commits.borrow();
                (c.done, c.running)
            };
            if done >= round {
                return;
            }
            if running {
                poll_fn(|_| {
                    let c = self.commits.borrow();
                    if c.done >= round || !c.running { Poll::Ready(()) } else { Poll::Pending }
                }).await;
            } else {
                self.commit_round().await;
            }
        }
    }

    /// A round: copy A of every page marked, on every device there was as
    /// it began, each device flushed; then copy B, flushed. A device whose
    /// write or flush fails is taken as failed.
    async fn commit_round(&self) {
        let mut round = {
            let mut c = self.commits.borrow_mut();
            c.running = true;
            let round = c.next;
            c.next += 1;
            Round { commits: &self.commits, round, table: core::mem::take(&mut c.table),
                    fences: core::mem::take(&mut c.fences), finished: false }
        };
        let devs = self.dev_count();
        for copy in 0..2 {
            for dv in 0..devs {
                let dev = self.dev(dv);
                let pages: Vec<u64> = round.table.get(&dv).map(|p| p.iter().copied().collect()).unwrap_or_default();
                if dev.failed() || (pages.is_empty() && round.fences.is_empty()) {
                    continue;
                }
                let mut ok = true;
                for p in pages {
                    ok = ok && dev.write_table_page(p, copy).await.is_ok();
                }
                let fences: Vec<u64> = round.fences.iter().copied().collect();
                for p in fences {
                    let page = fence_page(&self.s.borrow().fences, dev.sup.format, p);
                    ok = ok && dev.d.write((FENCE_AT[copy] + p) * PAGE, &page).await.is_ok();
                }
                ok = ok && dev.flush_counted().await.is_ok();
                if !ok {
                    dev.fail();
                }
            }
        }
        round.finished = true;
    }

    /// Device `dv`.
    fn dev(&self, dv: usize) -> Rc<Dev<D>> {
        self.devs.borrow()[dv].clone()
    }

    fn dev_count(&self) -> usize {
        self.devs.borrow().len()
    }

    /* ---- the devices ---- */

    /// Device `d`, formatted as `identity`'s, its format's id `format`, and
    /// taken in: whatever was on it is gone. Its fences are the cs's.
    pub async fn format_device(&self, d: D, identity: Identity, format: u64) -> Result<usize, (D, Error)> {
        let Some(layout) = layout(d.bytes()) else { return Err((d, Error::Bad)) };
        if !(d.sector() == SECTOR || d.sector() == PAGE) || format == 0 {
            return Err((d, Error::Bad));
        }
        let sup = Super { identity, sector: d.sector() as u32, slots: layout.slots, records: layout.records, format,
                          seq: 1 };
        let pages = u64::from(layout.slots).div_ceil(RECORDS_PER_PAGE as u64);
        /* The superblock's pages zeros first: a format cut short leaves a
           device that is blank, never one half another's */
        let mut wiped = d.zero(0, 2 * PAGE).await;
        if wiped.is_ok() {
            wiped = d.flush().await;
        }
        if wiped.is_ok() {
            wiped = d.zero(FENCE_AT[0] * PAGE, 2 * FENCE_PAGES * PAGE).await;
        }
        if wiped.is_ok() {
            wiped = d.zero(layout.table[0], pages * PAGE).await;
        }
        if wiped.is_ok() {
            wiped = d.zero(layout.table[1], pages * PAGE).await;
        }
        if wiped.is_err() {
            return Err((d, Error::Io));
        }
        let mut records = Pieces::new();
        for _ in 0..layout.slots {
            records.push(Record::default());
        }
        let mut free = Pieces::new();
        for s in (0..layout.slots).rev() {
            free.push(s);
        }
        let dev = Dev::new(d, layout, sup, records, free, 0);
        /* The cs's fences onto it, then the superblock: A, then B */
        let fences = self.s.borrow().fences.clone();
        let mut ok = true;
        for p in 0..FENCE_PAGES {
            ok = ok && dev.write_fence_page(&fences, p, 0).await.is_ok() && dev.write_fence_page(&fences, p, 1).await.is_ok();
        }
        ok = ok && dev.d.flush().await.is_ok();
        let mut page = vec![0u8; PAGE as usize];
        sup.encode(&mut page);
        for copy in SUPER_PAGES {
            ok = ok && dev.d.write(copy * PAGE, &page).await.is_ok() && dev.d.flush().await.is_ok();
        }
        if !ok {
            return Err((dev.d, Error::Io));
        }
        let mut devs = self.devs.borrow_mut();
        devs.push(Rc::new(dev));
        Ok(devs.len() - 1)
    }

    /// Device `d`, which says it is `expect`'s, read and taken in: its
    /// records, each the good one of its copies, the stale copies written
    /// again; its fences joined with the cs's.
    pub async fn open_device(&self, d: D, expect: Identity) -> Result<(usize, Repairs), (D, OpenError)> {
        let Some(layout) = layout(d.bytes()) else { return Err((d, OpenError::Small)) };
        let mut a = vec![0u8; PAGE as usize];
        let mut b = vec![0u8; PAGE as usize];
        if d.read(SUPER_PAGES[0] * PAGE, &mut a).await.is_err() || d.read(SUPER_PAGES[1] * PAGE, &mut b).await.is_err()
        {
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

        /* Its fences; a page of which one copy is stale or torn written again
           below, as the table's are -- left as it was, a cut in the next
           write of its other copy would leave neither good (found by
           fuzz/nbs's cs target) */
        let mut fences = vec![Fence::default(); FENCES_MAX];
        let mut fences_stale = false;
        for p in 0..FENCE_PAGES {
            if d.read((FENCE_AT[0] + p) * PAGE, &mut a).await.is_err()
                || d.read((FENCE_AT[1] + p) * PAGE, &mut b).await.is_err()
            {
                return Err((d, OpenError::Io));
            }
            for k in 0..FENCES_PER_PAGE {
                let at = k * FENCE;
                let fa = Fence::decode(&a[at..], sup.format);
                let fb = Fence::decode(&b[at..], sup.format);
                match choose(fa, fb, |f: &Fence| f.seq) {
                    Some(Decoded::Valid(f)) if f.volume != 0 => fences[p as usize * FENCES_PER_PAGE + k] = f,
                    Some(Decoded::Empty) => {}
                    /* A fence lost is a stale writer let in: not opened */
                    _ => return Err((d, OpenError::Corrupt("a fence of which neither copy is good"))),
                }
            }
            if a != b {
                fences_stale = true;
            }
        }

        /* Its table of slots */
        let pages = u64::from(layout.slots).div_ceil(RECORDS_PER_PAGE as u64);
        let mut stale_pages = Vec::new();
        let mut records = Pieces::new();
        let mut lost = 0;
        for p in 0..pages {
            if d.read(layout.table[0] + p * PAGE, &mut a).await.is_err()
                || d.read(layout.table[1] + p * PAGE, &mut b).await.is_err()
            {
                return Err((d, OpenError::Io));
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
                        lost += 1;
                        repairs.lost += 1;
                        Record { state: SlotState::Lost, ..Record::default() }
                    }
                };
                records.push(r);
            }
            if a != b {
                stale_pages.push(p);
            }
        }
        let mut free = Pieces::new();
        for s in (0..layout.slots).rev() {
            if records.get(s as usize).is_some_and(|r| r.state == SlotState::Free) {
                free.push(s);
            }
        }
        let dev = Dev::new(d, layout, sup, records, free, lost);
        /* The stale copies made the good ones again, a copy at a time */
        for &p in &stale_pages {
            for copy in 0..2 {
                if dev.write_table_page(p, copy).await.is_err() || dev.d.flush().await.is_err() {
                    return Err((dev.d, OpenError::Io));
                }
            }
            repairs.pages += 1;
        }

        let index = {
            let mut devs = self.devs.borrow_mut();
            devs.push(Rc::new(dev));
            devs.len() - 1
        };
        let dev = self.dev(index);
        /* Its chunks: one held twice is kept at its newer incarnation */
        let held: Vec<(u32, Record)> = {
            let st = dev.st.borrow();
            (0..dev.layout.slots).filter_map(|s| st.records.get(s as usize).map(|r| (s, *r)))
                .filter(|(_, r)| matches!(r.state, SlotState::Open | SlotState::Written)).collect()
        };
        for (slot, r) in held {
            let _turn = self.chunk_turns.take(r.chunk, true).await;
            let other = self.s.borrow().at.get(&r.chunk).copied();
            match other {
                Some((dv, s)) if self.record(dv, s).incarnation >= r.incarnation => {
                    let _ = self.release(index, slot).await;
                    repairs.stale += 1;
                }
                Some((dv, s)) => {
                    let _ = self.release(dv, s).await;
                    repairs.stale += 1;
                    self.s.borrow_mut().at.insert(r.chunk, (index, slot));
                }
                None => {
                    self.s.borrow_mut().at.insert(r.chunk, (index, slot));
                }
            }
        }
        /* The chunks it holds open, as it was when it stopped */
        let open: Vec<(u32, u32, u64)> = {
            let s = self.s.borrow();
            s.at.iter().filter(|(_, &(dv, _))| dv == index).map(|(&c, &(_, slot))| (c, dev.record(slot)))
                .filter(|(_, r)| r.state == SlotState::Open).map(|(c, r)| (c, r.incarnation, r.generation)).collect()
        };
        for (c, incarnation, generation) in open {
            self.s.borrow_mut().found.insert(c, (incarnation, generation));
        }
        /* Its fences joined with the cs's: the newer generation of each
           volume, in the cs's place for it. The devices taken in before all
           hold the table as it was; the table written again onto them if
           the join changed it, and onto this one if its own differs */
        let (others, this, table) = {
            let mut s = self.s.borrow_mut();
            let before = s.fences.clone();
            for f in fences.iter().filter(|f| f.volume != 0) {
                match s.fence_of.get(&f.volume).copied() {
                    Some(i) => {
                        let cur = &mut s.fences[i as usize];
                        cur.generation = cur.generation.max(f.generation);
                        cur.seq = cur.seq.max(f.seq);
                    }
                    None => {
                        let Some(i) = s.fence_free.pop() else {
                            drop(s);
                            drop(dev);
                            let dev = self.devs.borrow_mut().remove(index);
                            return Err((unwrap_dev(dev), OpenError::Corrupt("more fences than a cs keeps")));
                        };
                        s.fences[i as usize] = *f;
                        s.fence_of.insert(f.volume, i);
                    }
                }
            }
            (s.fences != before, fences_stale || fences != s.fences, s.fences.clone())
        };
        for dv in 0..self.dev_count() {
            let dev = self.dev(dv);
            if (dv == index && !this) || (dv != index && !others) || dev.failed() {
                continue;
            }
            for copy in 0..2 {
                let mut ok = true;
                for p in 0..FENCE_PAGES {
                    ok = ok && dev.write_fence_page(&table, p, copy).await.is_ok();
                }
                ok = ok && dev.d.flush().await.is_ok();
                if !ok {
                    dev.fail();
                    break;
                }
            }
            repairs.pages += 1;
        }
        Ok((index, repairs))
    }

    /// Whether the device takes new chunks: false to drain it.
    pub fn set_active(&self, dv: usize, active: bool) {
        if let Some(d) = self.devs.borrow().get(dv) {
            d.st.borrow_mut().active = active;
        }
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.devs.borrow().iter().map(|d| {
            let st = d.st.borrow();
            DeviceInfo { device: d.sup.identity.device, slots: d.layout.slots, free: st.free.len() as u32,
                         lost: st.lost, failed: st.failed, active: st.active }
        }).collect()
    }

    /// The device of the cs's that is the metadata's device `device`.
    pub fn device_index(&self, device: u32) -> Option<usize> {
        self.devs.borrow().iter().position(|d| d.sup.identity.device == device)
    }

    /* ---- fences ---- */

    /// The volume's fence: 0 for none.
    pub fn fence_of(&self, volume: u32) -> u64 {
        let s = self.s.borrow();
        s.fence_of.get(&volume).map_or(0, |&i| s.fences[i as usize].generation)
    }

    /// Volume `volume` fenced at `generation`: written to every device and
    /// flushed before it is answered -- with the fence there is now. The
    /// volume's turn alone: every request of it before the fence done.
    pub async fn fence(&self, volume: u32, generation: u64) -> Result<u64, Error> {
        if volume == 0 || generation == 0 {
            return Err(Error::Bad);
        }
        let _turn = self.volume_turns.take(volume, true).await;
        let cur = self.fence_of(volume);
        if cur > generation {
            return Err(Error::Fenced { generation: cur });
        }
        if cur == generation {
            return Ok(cur);
        }
        let index = {
            let mut s = self.s.borrow_mut();
            let index = match s.fence_of.get(&volume) {
                Some(&i) => i,
                None => s.fence_free.pop().ok_or(Error::TooManyFences)?,
            };
            let seq = s.fences[index as usize].seq + 1;
            s.fences[index as usize] = Fence { volume, generation, seq };
            s.fence_of.insert(volume, index);
            index
        };
        self.fence_round(u64::from(index) / FENCES_PER_PAGE as u64).await?;
        Ok(generation)
    }

    /// Volume `volume`'s fence taken off: the volume is gone.
    pub async fn drop_fence(&self, volume: u32) -> Result<(), Error> {
        let _turn = self.volume_turns.take(volume, true).await;
        let index = {
            let mut s = self.s.borrow_mut();
            let Some(index) = s.fence_of.remove(&volume) else { return Ok(()) };
            let seq = s.fences[index as usize].seq + 1;
            s.fences[index as usize] = Fence { volume: 0, generation: 0, seq };
            s.fence_free.push(index);
            index
        };
        self.fence_round(u64::from(index) / FENCES_PER_PAGE as u64).await
    }

    /// The volumes fenced.
    pub fn fenced_volumes(&self) -> Vec<(u32, u64)> {
        let s = self.s.borrow();
        s.fence_of.iter().map(|(&v, &i)| (v, s.fences[i as usize].generation)).collect()
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

    /// The fence table's page `p` onto every device by the next round: a
    /// device whose write fails is taken as failed; none left that took it,
    /// an error.
    async fn fence_round(&self, p: u64) -> Result<(), Error> {
        let round = {
            let mut c = self.commits.borrow_mut();
            c.fences.insert(p);
            c.next
        };
        self.committed(round).await;
        let devs = self.devs.borrow();
        if !devs.is_empty() && devs.iter().all(|d| d.failed()) {
            return Err(Error::Io);
        }
        Ok(())
    }

    /* ---- chunks ---- */

    fn record(&self, dv: usize, slot: u32) -> Record {
        self.devs.borrow().get(dv).map(|d| d.record(slot)).unwrap_or_default()
    }

    /// Slot `slot` of device `dv` holding `r`, its page durable by the
    /// round that takes it -- copy A of it written, flushed, then copy B.
    async fn put(&self, dv: usize, slot: u32, r: Record) -> Result<(), Error> {
        /* Written again in this run: no longer as it was found -- the chunk
           the slot held, and the one it holds now */
        let old = self.record(dv, slot);
        {
            let mut s = self.s.borrow_mut();
            if old.state != SlotState::Free {
                s.found.remove(&old.chunk);
            }
            if r.state != SlotState::Free {
                s.found.remove(&r.chunk);
            }
        }
        let dev = self.dev(dv);
        if let Some(cur) = dev.st.borrow_mut().records.get_mut(slot as usize) {
            *cur = r;
        }
        let round = {
            let mut c = self.commits.borrow_mut();
            c.table.entry(dv).or_default().insert(page_of(slot));
            c.next
        };
        self.committed(round).await;
        if dev.failed() {
            return Err(Error::Io);
        }
        Ok(())
    }

    /// A slot given back: its record free, the slot on the free list.
    async fn release(&self, dv: usize, slot: u32) -> Result<(), Error> {
        let old = self.record(dv, slot);
        if old.state == SlotState::Lost {
            return Ok(());
        }
        self.put(dv, slot, Record { seq: old.seq + 1, ..Record::default() }).await?;
        self.dev(dv).st.borrow_mut().free.push(slot);
        Ok(())
    }

    /// Where the chunk is, its record as a request finds it.
    fn find(&self, c: &ChunkRef) -> Result<Option<(usize, u32, Record)>, Error> {
        let Some((dv, slot)) = self.s.borrow().at.get(&c.chunk).copied() else { return Ok(None) };
        let r = self.record(dv, slot);
        if r.incarnation > c.incarnation {
            return Err(Error::StaleChunk { incarnation: r.incarnation });
        }
        if r.incarnation < c.incarnation {
            /* The chunk its number was: to be freed */
            return Ok(None);
        }
        if self.dev(dv).failed() || r.state == SlotState::Lost {
            return Err(Error::Failed);
        }
        Ok(Some((dv, slot, r)))
    }

    /// `offset` and `len` inside a chunk, whole sectors of the device.
    fn span(&self, dv: Option<usize>, offset: u32, len: usize) -> Result<(), Error> {
        let sector = dv.and_then(|i| self.devs.borrow().get(i).map(|d| d.d.sector())).unwrap_or(SECTOR);
        let end = u64::from(offset) + len as u64;
        if len == 0 || end > SLOT || u64::from(offset) % sector != 0 || len as u64 % sector != 0 {
            return Err(Error::Bad);
        }
        Ok(())
    }

    /// The device a new chunk goes onto: one that takes new chunks, with
    /// the most slots free.
    fn pick(&self) -> Option<usize> {
        let devs = self.devs.borrow();
        devs.iter().enumerate().filter_map(|(i, d)| {
            let st = d.st.borrow();
            (st.active && !st.failed && !st.free.is_empty()).then_some((i, st.free.len()))
        }).max_by_key(|&(i, free)| (free, usize::MAX - i)).map(|(i, _)| i)
    }

    /// Chunk `c`'s data written: `data` at `offset` in it, by `volume`'s
    /// attach at `generation`. `verified`: the chunk, its incarnation and
    /// epoch checked with the host's mds -- needed for a chunk the cs does
    /// not hold yet, and for an epoch other than the one it knows.
    pub async fn write(&self, volume: u32, generation: u64, c: ChunkRef, offset: u32, data: &[u8], verified: bool)
        -> Result<(), Error> {
        let _volume = self.volume_turns.take(volume, false).await;
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        self.check_fence(volume, generation)?;
        /* A joiner takes no write of an epoch before its join: one from a
           view that old -- a writer taken over since among them, which the
           members fenced against it refuse -- would be marked, and the copy
           keep it over the set's (found by fuzz/nbs's cluster target, N9d).
           Its writer reads its map again, and writes at the join's epoch */
        if let Some(since) = self.s.borrow().joining.get(&c.epoch.set).copied() {
            if c.epoch.version < since {
                return Err(Error::Epoch { known: Epoch { set: c.epoch.set, version: since } });
            }
        }
        let found = self.find(&c)?;
        let (dv, slot) = match found {
            Some((dv, slot, r)) => {
                self.span(Some(dv), offset, data.len())?;
                /* Another volume's (N10): a clone's chunk that its image, deleted,
                   named before -- written once the mds vouches that this
                   volume's map names it */
                if r.volume != volume && !verified {
                    return Err(Error::Unverified);
                }
                if r.known != c.epoch && (!verified || older(c.epoch, r.known)) {
                    return Err(Error::Epoch { known: r.known });
                }
                /* The first write of this generation or epoch, or of this
                   volume's: the open mark durable before it -- the writer's
                   volume's, which its reconcile asks of the cs, and its flush
                   is held to (`found_open_in`) */
                if r.state != SlotState::Open || r.generation != generation || r.written != c.epoch
                    || r.known != c.epoch || r.volume != volume
                {
                    let open = Record { state: SlotState::Open, known: c.epoch, written: c.epoch, volume, generation,
                                        seq: r.seq + 1, ..r };
                    self.put(dv, slot, open).await?;
                }
                (dv, slot)
            }
            None => {
                if !verified {
                    return Err(Error::Unverified);
                }
                self.take_slot(volume, generation, &c, offset, data.len()).await?
            }
        };
        /* Marked before it is written, where its set is joined: a copy must
           not write over it, and a mark that cannot be kept refuses it */
        self.mark(&c, offset, data.len())?;
        let dev = self.dev(dv);
        if dev.write_data(dev.data_at(slot, u64::from(offset)), data).await.is_err() {
            dev.fail();
            return Err(Error::Io);
        }
        Ok(())
    }

    /// A slot taken for chunk `c`, which the cs does not hold, open for
    /// `volume`'s `generation` at `c`'s epoch -- a client's first write's,
    /// or a copy's (N9).
    async fn take_slot(&self, volume: u32, generation: u64, c: &ChunkRef, offset: u32, len: usize)
        -> Result<(usize, u32), Error> {
        /* The slot of the chunk its number was -- the mds says this is the
           number's chunk now -- given back first */
        let was = self.s.borrow().at.get(&c.chunk).copied();
        if let Some((odv, oslot)) = was {
            self.release(odv, oslot).await?;
            self.s.borrow_mut().at.remove(&c.chunk);
        }
        let dv = self.pick().ok_or(Error::NoRoom)?;
        self.span(Some(dv), offset, len)?;
        let dev = self.dev(dv);
        let slot = dev.st.borrow_mut().free.pop().ok_or(Error::NoRoom)?;
        /* Zeros, flushed before the record that takes the slot is written: a
           device keeps or loses what it holds unflushed in any order, and a
           record kept with its zeros lost would read what the slot held
           through this chunk -- another volume's data. (Found by fuzz/nbs's
           cs target.) */
        if dev.zero_data(dev.data_at(slot, 0), SLOT).await.is_err() || dev.durable().await.is_err() {
            dev.fail();
            return Err(Error::Io);
        }
        let old = self.record(dv, slot);
        let open = Record { state: SlotState::Open, chunk: c.chunk, incarnation: c.incarnation, known: c.epoch,
                            written: c.epoch, volume, generation, seq: old.seq + 1 };
        self.put(dv, slot, open).await?;
        self.s.borrow_mut().at.insert(c.chunk, (dv, slot));
        Ok((dv, slot))
    }

    /// Chunk `c`'s data at `offset`, into `buf`: zeros where nothing was
    /// written -- a chunk the cs does not hold, once the mds vouches that the
    /// volume's map names it (N10): one freed since is not one never written.
    /// `generation` 0 for a read-only attach, which no fence holds to.
    pub async fn read(&self, volume: u32, generation: u64, c: ChunkRef, offset: u32, buf: &mut [u8], verified: bool)
        -> Result<(), Error> {
        let _volume = if generation != 0 { Some(self.volume_turns.take(volume, false).await) } else { None };
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        if generation != 0 {
            self.check_fence(volume, generation)?;
        }
        let Some((dv, slot, r)) = self.find(&c)? else {
            if !verified {
                return Err(Error::Unverified);
            }
            self.span(None, offset, buf.len())?;
            buf.fill(0);
            return Ok(());
        };
        self.span(Some(dv), offset, buf.len())?;
        /* Another volume's -- a snapshot's or a clone's map shares it (N10)
           -- read once the mds vouches that this volume's map names it:
           nothing of one volume's is read through another's otherwise */
        if r.volume != volume && !verified {
            return Err(Error::Unverified);
        }
        if r.known != c.epoch && (!verified || older(c.epoch, r.known)) {
            return Err(Error::Epoch { known: r.known });
        }
        self.read_at(dv, slot, offset, buf).await
    }

    /// Slot `slot` of device `dv` read at `offset` into `buf`.
    async fn read_at(&self, dv: usize, slot: u32, offset: u32, buf: &mut [u8]) -> Result<(), Error> {
        let dev = self.dev(dv);
        if dev.d.read(dev.data_at(slot, u64::from(offset)), buf).await.is_err() {
            dev.fail();
            return Err(Error::Io);
        }
        Ok(())
    }

    /// Everything written so far durable, for `volume`'s attach at
    /// `generation`: refused to an older one the fence has taken the volume
    /// from -- its flush done would tell its writer that what it wrote is
    /// durable where a newer writer's reconcile may have put other data
    /// (found by fuzz/nbs's cluster target).
    pub async fn flush_for(&self, volume: u32, generation: u64) -> Result<(), Error> {
        let _volume = self.volume_turns.take(volume, false).await;
        self.check_fence(volume, generation)?;
        self.flush().await
    }

    /// Everything written so far, durable: on each device, a flush begun
    /// after its writes done by now.
    pub async fn flush(&self) -> Result<(), Error> {
        let mut failed = false;
        for dv in 0..self.dev_count() {
            let dev = self.dev(dv);
            if dev.failed() {
                continue;
            }
            if dev.durable().await.is_err() {
                dev.fail();
                failed = true;
            }
        }
        if failed { Err(Error::Io) } else { Ok(()) }
    }

    /// Chunk `c` closed by `volume`'s attach at `generation`: its data
    /// flushed, then its record no longer open. Whether it was open.
    pub async fn close(&self, volume: u32, generation: u64, c: ChunkRef) -> Result<bool, Error> {
        let _volume = self.volume_turns.take(volume, false).await;
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        self.check_fence(volume, generation)?;
        let Some((dv, slot, r)) = self.find(&c)? else { return Ok(false) };
        if r.state != SlotState::Open || r.volume != volume || r.generation != generation {
            return Ok(false);
        }
        /* Its data -- every write of it done, its turn this close's --
           durable before the record that closes it */
        let dev = self.dev(dv);
        if dev.durable().await.is_err() {
            dev.fail();
            return Err(Error::Io);
        }
        self.put(dv, slot, Record { state: SlotState::Written, seq: r.seq + 1, ..r }).await?;
        Ok(true)
    }

    /// Chunk `chunk`'s slot given back, the chunk freed in the metadata at
    /// `incarnation`: whether it held one -- at that incarnation or older.
    pub async fn free(&self, chunk: u32, incarnation: u32) -> Result<bool, Error> {
        let _chunk = self.chunk_turns.take(chunk, true).await;
        let Some((dv, slot)) = self.s.borrow().at.get(&chunk).copied() else { return Ok(false) };
        if self.record(dv, slot).incarnation > incarnation || self.dev(dv).failed() {
            return Ok(false);
        }
        self.release(dv, slot).await?;
        self.s.borrow_mut().at.remove(&chunk);
        Ok(true)
    }

    /* ---- joins (N9) ---- */

    /// Clients' writes to set `set`'s chunks marked from now on, as this cs
    /// joins it -- the join begun at `since`: a copy into the set writes only
    /// where no client has. The marks of a join before it dropped: written
    /// while this cs was the joiner of none, the source may have newer
    /// data than they kept.
    pub fn join(&self, set: u32, since: u32) {
        if self.s.borrow().joining.get(&set) != Some(&since) {
            self.leave(set);
            self.s.borrow_mut().joining.insert(set, since);
        }
    }

    /// The join of set `set` over -- done, or given up: its marks dropped.
    pub fn leave(&self, set: u32) {
        let mut s = self.s.borrow_mut();
        s.joining.remove(&set);
        s.marks.retain(|_, m| m.set != set);
    }

    /// The join of set `set` this cs marks for: its `since`.
    pub fn joins(&self, set: u32) -> Option<u32> {
        self.s.borrow().joining.get(&set).copied()
    }

    /// The sets this cs marks for, each with its join's `since`.
    pub fn joined(&self) -> Vec<(u32, u32)> {
        self.s.borrow().joining.iter().map(|(&s, &since)| (s, since)).collect()
    }

    /// Whether a client has written a sector of chunk `c` from `offset` for
    /// `len` bytes since the join of its set began: what a copy leaves as
    /// it is, and its check leaves out.
    pub fn marked(&self, c: &ChunkRef, offset: u32, len: u32) -> bool {
        let s = self.s.borrow();
        let Some(m) = s.marks.get(&c.chunk).filter(|m| m.incarnation == c.incarnation) else { return false };
        let first = offset as usize / SECTOR as usize;
        (first..first + len as usize / SECTOR as usize)
            .any(|s| m.bits.get(s / 64).is_some_and(|w| w & (1 << (s % 64)) != 0))
    }

    /// A client's write of `len` bytes at `offset` in chunk `c` marked,
    /// where its set is joined.
    fn mark(&self, c: &ChunkRef, offset: u32, len: usize) -> Result<(), Error> {
        let mut s = self.s.borrow_mut();
        if !s.joining.contains_key(&c.epoch.set) {
            return Ok(());
        }
        if !s.marks.get(&c.chunk).is_some_and(|m| m.incarnation == c.incarnation) {
            let mut bits = Vec::new();
            bits.try_reserve_exact(MARK_WORDS).map_err(|_| Error::NoMemory)?;
            bits.resize(MARK_WORDS, 0);
            s.marks.insert(c.chunk, Marks { set: c.epoch.set, incarnation: c.incarnation, bits });
        }
        let Some(m) = s.marks.get_mut(&c.chunk) else { return Err(Error::NoMemory) };
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
    async fn advance(&self, c: &ChunkRef) -> Result<(usize, u32), Error> {
        let Some((dv, slot, r)) = self.find(c)? else { return Err(Error::NotHeld) };
        if r.known.set != c.epoch.set || r.known.version > c.epoch.version {
            return Err(Error::Epoch { known: r.known });
        }
        if r.known != c.epoch {
            /* A chunk found open never comes here (`not_found_open`): the
               record written would take its mark off (found by fuzz/nbs's
               cluster target, N9d: a joiner's pull did, and the writer went
               on with a replica short of a write it had done) */
            self.put(dv, slot, Record { known: c.epoch, seq: r.seq + 1, ..r }).await?;
        }
        Ok((dv, slot))
    }

    /// Chunk `c`'s data at `offset`, into `buf`, for its set's joiner: its
    /// epoch moved on first (`advance`). The caller has had the joiner
    /// vouched for by its host's mds.
    pub async fn pull(&self, c: ChunkRef, offset: u32, buf: &mut [u8]) -> Result<(), Error> {
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        self.not_found_open(&c)?;
        let (dv, slot) = self.advance(&c).await?;
        self.span(Some(dv), offset, buf.len())?;
        self.read_at(dv, slot, offset, buf).await
    }

    /// The CRC-32C of each `GRANULE` of chunk `c` from `offset` for `len`
    /// bytes -- the last shorter where `len` ends inside one -- for its set's
    /// joiner, its epoch moved on first as a pull's is.
    pub async fn sums(&self, c: ChunkRef, offset: u32, len: u32) -> Result<Vec<u32>, Error> {
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        self.not_found_open(&c)?;
        let (dv, slot) = self.advance(&c).await?;
        self.sums_at(dv, slot, offset, len).await
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
    pub async fn own_sums(&self, c: ChunkRef, offset: u32, len: u32) -> Result<Vec<u32>, Error> {
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        let Some((dv, slot, _)) = self.find(&c)? else { return Err(Error::NotHeld) };
        self.sums_at(dv, slot, offset, len).await
    }

    /// What the leader's scrub holds chunk `c`'s replicas alike by (N9e):
    /// whether its record is open, the version of the epoch it was last
    /// written at, and the CRC-32C of each `GRANULE` of the whole chunk --
    /// read as a joiner's sums are, but with no epoch moved on: a writer of
    /// now is refused nothing for it. The chunk is the one its number and
    /// incarnation name, as `find` holds every request to.
    pub async fn scrub(&self, c: ChunkRef) -> Result<(bool, u32, Vec<u32>), Error> {
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        let Some((dv, slot, r)) = self.find(&c)? else { return Err(Error::NotHeld) };
        /* Open, it is in flight and compared with nothing: not read */
        if r.state == SlotState::Open {
            return Ok((true, r.written.version, Vec::new()));
        }
        let sums = self.sums_at(dv, slot, 0, SLOT as u32).await?;
        Ok((false, r.written.version, sums))
    }

    /// This cs's own copy of chunk `c` at `offset`, into `buf`: what a
    /// joiner holds a granule that differs to its source's by, a sector at a
    /// time.
    pub async fn own_read(&self, c: ChunkRef, offset: u32, buf: &mut [u8]) -> Result<(), Error> {
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        let Some((dv, slot, _)) = self.find(&c)? else { return Err(Error::NotHeld) };
        self.span(Some(dv), offset, buf.len())?;
        self.read_at(dv, slot, offset, buf).await
    }

    async fn sums_at(&self, dv: usize, slot: u32, offset: u32, len: u32) -> Result<Vec<u32>, Error> {
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
            self.read_at(dv, slot, at, &mut buf).await?;
            sums.push(crc::crc32c::checksum(&buf));
            at += n;
        }
        Ok(sums)
    }

    /// `data` copied into chunk `c` at `offset` for the join of its set,
    /// held as `volume`'s: written where no client's write has marked a
    /// sector -- the client's is newer, and the source has it or will. A
    /// chunk the cs does not hold taken in as a first write takes it, open
    /// at no writer's generation; one it holds opened at the copy's epoch,
    /// held as whichever volume's it was -- the source's data is the set's,
    /// and another volume's chunk is the next write's to vouch for (N10).
    /// Refused where the set is not joined: with no marks kept, a copy could
    /// write over a client's write.
    pub async fn copy_in(&self, volume: u32, c: ChunkRef, offset: u32, data: &[u8]) -> Result<(), Error> {
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        if !self.s.borrow().joining.contains_key(&c.epoch.set) || volume == 0 {
            return Err(Error::Bad);
        }
        let (dv, slot) = match self.find(&c)? {
            Some((dv, slot, r)) => {
                self.span(Some(dv), offset, data.len())?;
                if r.known.set != c.epoch.set || r.known.version > c.epoch.version {
                    return Err(Error::Epoch { known: r.known });
                }
                if r.state != SlotState::Open || r.known != c.epoch {
                    /* A client's open mark kept at its generation, for
                       `close_copy` to leave or close; a closed chunk opened
                       at none, for `close_copy` to close */
                    let generation = if r.state == SlotState::Open { r.generation } else { 0 };
                    self.put(dv, slot, Record { state: SlotState::Open, known: c.epoch, written: c.epoch, generation,
                                                seq: r.seq + 1, ..r }).await?;
                }
                (dv, slot)
            }
            None => self.take_slot(volume, 0, &c, offset, data.len()).await?,
        };
        /* The runs of sectors no client has written */
        let sector = SECTOR as usize;
        let first = offset as usize / sector;
        let n = data.len() / sector;
        let mut runs: Vec<(usize, usize)> = Vec::new();
        {
            let s = self.s.borrow();
            let marks = s.marks.get(&c.chunk).filter(|m| m.incarnation == c.incarnation);
            let marked = |s: usize| marks.is_some_and(|m| m.bits.get(s / 64).is_some_and(|w| w & (1 << (s % 64)) != 0));
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
        }
        let dev = self.dev(dv);
        for (from, to) in runs {
            let at = dev.data_at(slot, u64::from(offset) + (from * sector) as u64);
            if dev.write_data(at, &data[from * sector..to * sector]).await.is_err() {
                dev.fail();
                return Err(Error::Io);
            }
        }
        Ok(())
    }

    /// Chunk `from` copied into chunk `to` of its set, for `volume`'s writer
    /// at `generation`, before it writes a chunk others share (N10): `to`
    /// held as the volume's, closed, written at its epoch -- so that a member
    /// back copies it -- and durable before the answer, its data before its
    /// record. `to` held already, the copy was done: what it holds is kept,
    /// a client's writes since among it. `from` held by none here -- never
    /// written -- the copy is zeros, as `to` reads held by none.
    /// `verified`: the copy-up checked with the host's mds, needed for a
    /// `to` the cs does not hold. Refused where `from` was found open: it
    /// may lack what its writer wrote since its last flush, which the copy
    /// would keep for good.
    pub async fn copy_up(&self, volume: u32, generation: u64, from: ChunkRef, to: ChunkRef, verified: bool)
        -> Result<(), Error> {
        /* The fence looked at under the volume's turn, which no fence takes
           while it is held */
        let _volume = self.volume_turns.take(volume, false).await;
        self.check_fence(volume, generation)?;
        if from.epoch != to.epoch || from.chunk == to.chunk {
            return Err(Error::Bad);
        }
        /* Both chunks' turns, the lower number's first: two copy-ups of a
           pair never wait for each other */
        let (first, second) = (from.chunk.min(to.chunk), from.chunk.max(to.chunk));
        let _first = self.chunk_turns.take(first, true).await;
        let _second = self.chunk_turns.take(second, true).await;
        if self.find(&to)?.is_some() {
            return Ok(());
        }
        if !verified {
            return Err(Error::Unverified);
        }
        self.not_found_open(&from)?;
        let Some((fdv, fslot, r)) = self.find(&from)? else { return Ok(()) };
        if older(from.epoch, r.known) {
            return Err(Error::Epoch { known: r.known });
        }
        /* The slot of the chunk `to`'s number was given back first, as a
           first write's is */
        let was = self.s.borrow().at.get(&to.chunk).copied();
        if let Some((odv, oslot)) = was {
            self.release(odv, oslot).await?;
            self.s.borrow_mut().at.remove(&to.chunk);
        }
        let dv = self.pick().ok_or(Error::NoRoom)?;
        let dev = self.dev(dv);
        /* Taken off the free list for the copy -- no other request takes it
           meanwhile -- and given back where the copy fails before its record
           is written: what it wrote there no chunk's */
        let slot = dev.st.borrow_mut().free.pop().ok_or(Error::NoRoom)?;
        let mut held = Held { dev: &dev, slot: Some(slot) };
        let mut buf = Vec::new();
        buf.try_reserve_exact(COPY_PIECE as usize).map_err(|_| Error::NoMemory)?;
        let mut at = 0;
        while at < SLOT {
            let n = (SLOT - at).min(COPY_PIECE);
            buf.resize(n as usize, 0);
            self.read_at(fdv, fslot, at as u32, &mut buf).await?;
            if dev.write_data(dev.data_at(slot, at), &buf).await.is_err() {
                dev.fail();
                return Err(Error::Io);
            }
            at += n;
        }
        /* The data durable before the record that names it: a device keeps
           or loses what it holds unflushed in any order */
        if dev.durable().await.is_err() {
            dev.fail();
            return Err(Error::Io);
        }
        let old = self.record(dv, slot);
        self.put(dv, slot, Record { state: SlotState::Written, chunk: to.chunk, incarnation: to.incarnation,
                                    known: to.epoch, written: to.epoch, volume, generation: 0, seq: old.seq + 1 })
            .await?;
        held.slot = None;
        self.s.borrow_mut().at.insert(to.chunk, (dv, slot));
        Ok(())
    }

    /// Chunk `c`'s copy done: its data flushed, then its record closed where
    /// no client has written it since the join began -- one a client has,
    /// at its generation, is that client's to close.
    pub async fn close_copy(&self, c: ChunkRef) -> Result<(), Error> {
        let _chunk = self.chunk_turns.take(c.chunk, true).await;
        let Some((dv, slot, r)) = self.find(&c)? else { return Ok(()) };
        /* What the copy took in durable, whatever the record: one a client
           wrote meanwhile stays open at the writer's generation, as any
           chunk it writes, but the join's end makes this cs a member, and a
           power cut after must not take the copy's sectors with it (found
           by fuzz/nbs's cluster target, N9d: the record left open, the
           flush was left out with it) */
        let dev = self.dev(dv);
        if dev.durable().await.is_err() {
            dev.fail();
            return Err(Error::Io);
        }
        /* Open at a writer's generation, the chunk that client's to close
           where a client has written it since the join began. A mark from
           before the join -- this cs a member then, taken out with the chunk
           open, its writer closing it on the members it had left -- is
           nobody's but the copy's, whether the copy wrote the chunk or found
           it the source's already (found by fuzz/nbs's cluster target, N9d:
           kept, the joiner's crash made the chunk found open, and as its
           set's last member it was no source for the joiner after it) */
        let written = self.s.borrow().marks.get(&c.chunk).is_some_and(|m| m.incarnation == c.incarnation);
        if r.state != SlotState::Open || (r.generation != 0 && written) {
            return Ok(());
        }
        self.put(dv, slot, Record { state: SlotState::Written, seq: r.seq + 1, ..r }).await
    }

    /// The chunks of set `set` the cs holds, from chunk number `from` on,
    /// `max` at most: each's number, incarnation, volume and the version it
    /// was last written at -- what its joiner copies.
    pub fn holdings_in(&self, set: u32, from: u32, max: usize) -> Vec<(u32, u32, u32, u32)> {
        let s = self.s.borrow();
        s.at.range(from..).filter_map(|(&chunk, &(dv, slot))| {
            let r = self.record(dv, slot);
            (r.known.set == set && r.state != SlotState::Lost && !self.dev(dv).failed())
                .then_some((chunk, r.incarnation, r.volume, r.written.version))
        }).take(max).collect()
    }

    /// Every chunk the cs holds.
    pub fn holdings(&self) -> Vec<Holding> {
        let s = self.s.borrow();
        s.at.iter().map(|(&chunk, &(dv, slot))| {
            let r = self.record(dv, slot);
            Holding { chunk, incarnation: r.incarnation, state: r.state, volume: r.volume,
                      generation: r.generation, device: self.dev(dv).sup.identity.device, slot, known: r.known,
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
        self.s.borrow().found.get(&chunk).filter(|(i, _)| *i == incarnation).map(|&(_, g)| g)
    }

    /// `found_open`, where the chunk's record is `volume`'s: the generation
    /// of that volume's writer that left it open. Generations are counted
    /// by volume -- a clone's writer may be at the very one another
    /// volume's left a chunk open at, and is not that writer (N10d). The
    /// record is as it was found while the chunk is: a write takes it out.
    pub fn found_open_by(&self, volume: u32, chunk: u32, incarnation: u32) -> Option<u64> {
        let g = self.found_open(chunk, incarnation)?;
        let (dv, slot) = self.s.borrow().at.get(&chunk).copied()?;
        (self.record(dv, slot).volume == volume).then_some(g)
    }

    /// Whether some chunk of `volume`'s is found open at `generation` (as
    /// `found_open`): that writer's flush cannot make durable what may be
    /// gone here.
    pub fn found_open_in(&self, volume: u32, generation: u64) -> bool {
        let s = self.s.borrow();
        s.found.iter().any(|(&chunk, &(incarnation, g))| {
            g == generation && s.at.get(&chunk).is_some_and(|&(dv, slot)| {
                let r = self.record(dv, slot);
                r.volume == volume && r.incarnation == incarnation
            })
        })
    }

    /// The chunk's record, where the cs holds it at this incarnation.
    pub fn holding(&self, chunk: u32) -> Option<Holding> {
        let (dv, slot) = self.s.borrow().at.get(&chunk).copied()?;
        let r = self.record(dv, slot);
        Some(Holding { chunk, incarnation: r.incarnation, state: r.state, volume: r.volume,
                       generation: r.generation, device: self.dev(dv).sup.identity.device, slot, known: r.known,
                       written: r.written })
    }
}
