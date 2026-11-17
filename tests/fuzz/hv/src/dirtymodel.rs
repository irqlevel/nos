//! A migration's dirty log (`hv::dirty`, plans/vm-migration.md's M2), as
//! the old host runs it under a guest that goes on writing: the real `Log`
//! -- its marks, its faults, its rounds -- over a nested table and the
//! guest's CPUs' TLBs of this model's own, the host's writes beside them,
//! and each round's copy read a page at a time while the writes go on.
//!
//! The model of the hardware, as the log relies on it:
//!
//! - a CPU writes a page through what its TLB holds of it, which it took
//!   from the table while the page was writable -- or, holding nothing, walks
//!   the table: writable, it caches the page and writes; read-only, it faults
//!   (`Log::fault`, under the table's lock), and writes once the fault has
//!   made the page writable again;
//! - the table narrowed -- the log begun, a round taken -- has a new
//!   identity, and a CPU drops all it caches of the table when it takes the
//!   new one, at the top of its loop (`run.rs`'s `sync_tlbs`); a round's
//!   pages are read only once every CPU has taken it;
//! - the host writes its bytes, then marks (`Log::wrote`);
//! - paused, nothing writes.
//!
//! - **Nothing lost**: once paused, the last round taken and copied, the
//!   copy is the memory, page for page -- every write after a page's copy
//!   in a later round's pages.
//! - **The log over** (`release`): every page writable again, no write
//!   faults.

use std::collections::{BTreeSet, VecDeque};

use crate::dirty::{Log, Table};
use crate::targets::Input;

const PAGE: u64 = 4096;

/// The nested table, as the log sees it: which pages are read-only, and
/// which are RAM -- two runs, as a guest's below and above its hole.
struct Nested {
    ram: Vec<(u64, u64)>,
    ro: BTreeSet<u64>,
    protects: u64,
}

impl Nested {
    fn is_ram(&self, gpa: u64) -> bool {
        self.ram.iter().any(|&(b, n)| gpa >= b && gpa < b + n * PAGE)
    }
}

impl Table for Nested {
    fn protect(&mut self, gpa: u64) -> bool {
        if !self.is_ram(gpa) || gpa % PAGE != 0 {
            return false;
        }
        self.protects += 1;
        self.ro.insert(gpa);
        true
    }
    fn unprotect(&mut self, gpa: u64) -> bool {
        if !self.is_ram(gpa) || gpa % PAGE != 0 {
            return false;
        }
        self.ro.remove(&gpa);
        true
    }
}

struct Cpu {
    /// The pages its TLB holds writable.
    tlb: BTreeSet<u64>,
    /// The table's identity it last took.
    took: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The log not begun.
    Idle,
    /// The table narrowed; a round's pages are read once every CPU has
    /// taken identity `need`.
    Sync { need: u64 },
    /// Pages being read.
    Copying,
    /// Paused: the last round's pages being read.
    Last,
    Done,
}

struct World {
    pages: Vec<u64>,
    mem: Vec<u64>,
    copy: Vec<u64>,
    table: Nested,
    log: Log,
    cpus: Vec<Cpu>,
    id: u64,
    phase: Phase,
    queue: VecDeque<u64>,
    round: u32,
    rounds: u32,
}

impl World {
    fn new(r: &mut Input) -> World {
        let low = 1 + r.below(64);
        let high = r.below(64);
        let high_base = 0x1_0000_0000;
        let mut ram = vec![(0, low)];
        let mut log = Log::new();
        assert!(log.add(0, low * PAGE));
        if high != 0 {
            ram.push((high_base, high));
            assert!(log.add(high_base, high * PAGE));
        }
        let pages: Vec<u64> = ram.iter().flat_map(|&(b, n)| (0..n).map(move |k| b + k * PAGE)).collect();
        let n = pages.len();
        let cpus = (0..1 + r.below(4)).map(|_| Cpu { tlb: BTreeSet::new(), took: 1 }).collect();
        World {
            pages,
            mem: vec![0; n],
            copy: vec![u64::MAX; n],
            table: Nested { ram, ro: BTreeSet::new(), protects: 0 },
            log,
            cpus,
            id: 1,
            phase: Phase::Idle,
            queue: VecDeque::new(),
            round: 0,
            rounds: r.below(6) as u32,
        }
    }

    fn index(&self, gpa: u64) -> usize {
        self.pages.binary_search(&gpa).expect("a page of its RAM")
    }

    fn writes(&self) -> bool {
        !matches!(self.phase, Phase::Last | Phase::Done)
    }

    /// CPU `c` writes `gpa`.
    fn cpu_write(&mut self, c: usize, gpa: u64) {
        if !self.writes() {
            return;
        }
        if !self.cpus[c].tlb.contains(&gpa) {
            if self.table.ro.contains(&gpa) {
                /* A nested fault: marked and writable again, under the
                   table's lock -- one step here */
                invariant!(self.log.fault(&mut self.table, gpa), "a write fault on RAM not the log's");
                invariant!(!self.table.ro.contains(&gpa), "a page still read-only after its fault");
            }
            self.cpus[c].tlb.insert(gpa);
        }
        let i = self.index(gpa);
        self.mem[i] += 1;
    }

    /// The host writes `n` pages from `gpa`: its bytes, then its marks.
    fn host_write(&mut self, gpa: u64, n: u64) {
        if !self.writes() {
            return;
        }
        let mut done = 0;
        let mut at = gpa;
        while done < n && self.table.is_ram(at) {
            let i = self.index(at);
            self.mem[i] += 1;
            done += 1;
            at += PAGE;
        }
        self.log.wrote(gpa, done * PAGE);
    }

    /// CPU `c` at the top of its loop: the table's identity taken, its TLB
    /// dropped where it is new.
    fn top(&mut self, c: usize) {
        let cpu = &mut self.cpus[c];
        if cpu.took != self.id {
            cpu.tlb.clear();
            cpu.took = self.id;
        }
    }

    fn narrow(&mut self) -> u64 {
        self.id += 1;
        self.id
    }

    /// One step of the migration's.
    fn step(&mut self, r: &mut Input) {
        match self.phase {
            Phase::Idle => {
                invariant!(self.log.start(&mut self.table), "the log not begun over its own RAM");
                invariant!(self.pages.iter().all(|p| self.table.ro.contains(p)), "a page writable once the log began");
                let need = self.narrow();
                self.queue = self.pages.iter().copied().collect();
                self.phase = Phase::Sync { need };
            }
            Phase::Sync { need } => {
                if self.cpus.iter().all(|c| c.took >= need) {
                    self.phase = Phase::Copying;
                }
            }
            Phase::Copying | Phase::Last => {
                /* A page or a few read, as the rounds read them */
                for _ in 0..1 + r.below(4) {
                    let Some(gpa) = self.queue.pop_front() else { break };
                    let i = self.index(gpa);
                    self.copy[i] = self.mem[i];
                }
                if !self.queue.is_empty() {
                    return;
                }
                if self.phase == Phase::Last {
                    self.phase = Phase::Done;
                    return;
                }
                let mut taken = Vec::new();
                if self.round < self.rounds {
                    self.round += 1;
                    let n = self.log.take(Some(&mut self.table), &mut |gpa| taken.push(gpa));
                    invariant!(n == Some(taken.len() as u64), "a round's count is not its pages");
                    invariant!(taken.iter().all(|p| self.table.ro.contains(p)), "a round's page writable after it");
                    let need = self.narrow();
                    self.queue = taken.into_iter().collect();
                    self.phase = Phase::Sync { need };
                } else {
                    /* Paused: nothing writes from here; the last round */
                    self.phase = Phase::Last;
                    self.log.take(None, &mut |gpa| taken.push(gpa));
                    self.queue = taken.into_iter().collect();
                }
            }
            Phase::Done => {}
        }
    }

    fn page(&self, r: &mut Input) -> u64 {
        self.pages[r.below(self.pages.len() as u64) as usize]
    }
}

/// A run of RAM over pieces of marks: pages either side of each piece's
/// edge and anywhere marked by the host, then taken -- each exactly once,
/// in order, and counted as `marked` said.
fn pieces(r: &mut Input) {
    let pieces = 2 + r.below(3);
    let pages = pieces * crate::dirty::PIECE_PAGES + r.below(200);
    let base = 0x1_0000_0000u64;
    let mut log = Log::new();
    assert!(log.add(base, pages * PAGE));
    let mut want = BTreeSet::new();
    while let Some(op) = r.op(3) {
        let page = match op {
            0 => {
                let k = 1 + r.below(pieces - 1);
                k * crate::dirty::PIECE_PAGES + r.below(3) - 1
            }
            1 => pages - 1 - r.below(3),
            _ => r.below(pages),
        };
        let n = 1 + r.below(3).min(pages - 1 - page);
        log.wrote(base + page * PAGE, n * PAGE);
        for k in page..page + n {
            want.insert(base + k * PAGE);
        }
    }
    invariant!(log.marked() == want.len() as u64, "{} marked, {} written", log.marked(), want.len());
    let mut got = Vec::new();
    let n = log.take(None, &mut |gpa| got.push(gpa));
    invariant!(n == Some(got.len() as u64), "a take's count is not its pages");
    invariant!(got.iter().copied().eq(want.iter().copied()), "the pages taken are not those written: {:?}",
               got.iter().zip(want.iter()).find(|(a, b)| a != b));
    invariant!(log.marked() == 0, "{} marked after the take", log.marked());
}

pub fn dirtylog_target(r: &mut Input) {
    if r.below(8) == 0 {
        return pieces(r);
    }
    let mut w = World::new(r);
    let cpus = w.cpus.len() as u64;
    while let Some(op) = r.op(6) {
        match op {
            0 | 1 => {
                let c = r.below(cpus) as usize;
                let gpa = w.page(r);
                w.cpu_write(c, gpa);
            }
            2 => {
                let gpa = w.page(r);
                let n = 1 + r.below(3);
                w.host_write(gpa, n);
            }
            3 => {
                let c = r.below(cpus) as usize;
                w.top(c);
            }
            /* A TLB lets go of a page of its own accord */
            4 => {
                let c = r.below(cpus) as usize;
                let gpa = w.page(r);
                w.cpus[c].tlb.remove(&gpa);
            }
            _ => w.step(r),
        }
        if w.phase == Phase::Done {
            break;
        }
    }
    /* Everything let go of from here: every CPU through its loop as often
       as the steps want, the steps to their end */
    for _ in 0..10_000 {
        if w.phase == Phase::Done {
            break;
        }
        for c in 0..w.cpus.len() {
            w.top(c);
        }
        w.step(r);
    }
    invariant!(w.phase == Phase::Done, "the migration never ended: {:?}", w.phase);
    for (i, gpa) in w.pages.iter().enumerate() {
        invariant!(w.copy[i] == w.mem[i], "page {:#x} copied as of write {} and written {} times: a write lost", gpa,
                   w.copy[i], w.mem[i]);
    }
    /* The log over: every page writable again, and none faults after */
    w.log.release(&mut w.table);
    invariant!(w.table.ro.is_empty(), "{} pages read-only after the log's release", w.table.ro.len());
}
