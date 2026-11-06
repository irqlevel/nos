//! The log on disk (`raft::log`) through power cuts, torn writes and lost
//! ones, on a disk of the fuzzer's own ([`Disk`]) that keeps POSIX's
//! promises and no more: a file's writes are on the medium once the file is
//! synced, a name once the directory is, and at a power cut every write
//! since a file's last sync is kept, lost or torn on its own -- its tail
//! zeros, noise, or the file's end -- and a run of the directory's operations
//! since its last sync kept, in order.
//!
//! After each cut the log is opened again and held to what was synced: it
//! must read back as the state at the last sync with some first records of
//! what was written after, at a record's boundary -- nothing synced lost,
//! nothing torn read as an entry, nothing that was cut read again -- or as
//! the state a snapshot that failed half way would have left. And calls of
//! the disk that fail, the log stopping on the first and reading back the
//! same way; a byte of a sealed segment or of the snapshot changed, and the
//! log refusing to open; a directory of noise named as the log names its
//! files, the log refusing it or reading nothing from it. Every allocation of
//! the crate's held to the kernel's heap (`heap`), the disk's own exempt.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use raft::{
    ConfState, DiskLog, Entry, EntryKind, FileError, Files, HardState, LogConfig, LogError, MemStorage, Snapshot,
    SnapshotBuilder, SnapshotData, SnapshotMeta, Storage,
};

use crate::heap::Exempt;
use crate::rng::{hash_bytes, mix, Rng};

/* ---- the disk ---- */

/// A write a file has not synced.
#[derive(Clone, Debug)]
pub enum Op {
    Write { at: u64, bytes: Vec<u8> },
    Truncate(u64),
}

/// A change of the directory's not yet synced.
#[derive(Clone, Debug)]
enum DirOp {
    Link(String, u64),
    Rename(String, String),
    Unlink(String),
}

/// What becomes of a write at a power cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Kept,
    Lost,
    /// Its first `keep` bytes written, and after them noise, zeros, or the
    /// file's end.
    Torn { keep: usize, fill: Fill },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    End,
    Zeros,
    Noise,
}

/// The medium and what is on its way to it: names to inodes and each
/// inode's bytes, as the program sees them now and as they are on the
/// medium, and what is pending between the two.
pub struct Disk {
    dir: BTreeMap<String, u64>,
    durable_dir: BTreeMap<String, u64>,
    dir_ops: Vec<DirOp>,
    data: BTreeMap<u64, Vec<u8>>,
    durable: BTreeMap<u64, Vec<u8>>,
    pending: BTreeMap<u64, Vec<Op>>,
    next_ino: u64,
    /// One call in this many fails, 0 for none; an append that fails
    /// writes some of its bytes first.
    pub fail_one_in: u64,
    /// Every call of this kind fails while it is set: "remove", "sync_dir".
    pub fail_kind: Option<&'static str>,
    pub failures: u64,
    rng: Rng,
}

impl Disk {
    pub fn new(seed: u64) -> Disk {
        Disk {
            dir: BTreeMap::new(),
            durable_dir: BTreeMap::new(),
            dir_ops: Vec::new(),
            data: BTreeMap::new(),
            durable: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_ino: 1,
            fail_one_in: 0,
            fail_kind: None,
            failures: 0,
            rng: Rng::new(seed ^ 0xD15C),
        }
    }

    fn fault(&mut self) -> bool {
        if self.fail_one_in != 0 && self.rng.below(self.fail_one_in) == 0 {
            self.failures += 1;
            return true;
        }
        false
    }

    fn ino(&self, name: &str) -> Result<u64, FileError> {
        self.dir.get(name).copied().ok_or(FileError::NotFound)
    }

    /// A power cut, each write's fate drawn.
    pub fn crash(&mut self, r: &mut Rng) {
        let keep_dir = r.below(self.dir_ops.len() as u64 + 1) as usize;
        let mut fates = Vec::new();
        for ops in self.pending.values() {
            for op in ops {
                let len = match op {
                    Op::Write { bytes, .. } => bytes.len(),
                    Op::Truncate(_) => 0,
                };
                fates.push(match r.below(8) {
                    0..=1 => Fate::Lost,
                    2..=3 if len > 0 => Fate::Torn {
                        keep: r.below(len as u64) as usize,
                        fill: [Fill::End, Fill::Zeros, Fill::Noise][r.below(3) as usize],
                    },
                    _ => Fate::Kept,
                });
            }
        }
        let mut it = fates.into_iter();
        let mut noise = Rng::new(r.next());
        self.crash_with(keep_dir, &mut |_, _| it.next().unwrap_or(Fate::Kept), &mut noise);
    }

    /// A power cut: the directory as synced and its first `keep_dir`
    /// changes since; each file named then as synced, and each of its writes
    /// since -- inode by inode, in order -- as `fate` says.
    pub fn crash_with(&mut self, keep_dir: usize, fate: &mut dyn FnMut(u64, &Op) -> Fate, noise: &mut Rng) {
        let _own = Exempt::new();
        let mut dir = self.durable_dir.clone();
        for op in self.dir_ops.iter().take(keep_dir) {
            match op {
                DirOp::Link(n, i) => {
                    dir.insert(n.clone(), *i);
                }
                DirOp::Rename(a, b) => {
                    if let Some(i) = dir.remove(a) {
                        dir.insert(b.clone(), i);
                    }
                }
                DirOp::Unlink(n) => {
                    dir.remove(n);
                }
            }
        }
        let mut data = BTreeMap::new();
        for (&ino, ops) in self.pending.iter() {
            let mut bytes = self.durable.get(&ino).cloned().unwrap_or_default();
            for op in ops {
                match (op, fate(ino, op)) {
                    (_, Fate::Lost) => {}
                    (Op::Truncate(len), _) => bytes.truncate(*len as usize),
                    (Op::Write { at, bytes: w }, Fate::Kept) => put(&mut bytes, *at as usize, w),
                    (Op::Write { at, bytes: w }, Fate::Torn { keep, fill }) => {
                        let keep = keep.min(w.len());
                        put(&mut bytes, *at as usize, &w[..keep]);
                        let rest = w.len() - keep;
                        match fill {
                            Fill::End => {}
                            Fill::Zeros => put(&mut bytes, *at as usize + keep, &vec![0; rest]),
                            Fill::Noise => {
                                let n: Vec<u8> = (0..rest).map(|_| noise.next() as u8).collect();
                                put(&mut bytes, *at as usize + keep, &n);
                            }
                        }
                    }
                }
            }
            data.insert(ino, bytes);
        }
        for (&ino, bytes) in self.durable.iter() {
            data.entry(ino).or_insert_with(|| bytes.clone());
        }
        data.retain(|ino, _| dir.values().any(|i| i == ino));
        self.dir = dir.clone();
        self.durable_dir = dir;
        self.dir_ops.clear();
        self.durable = data.clone();
        self.data = data;
        self.pending.clear();
    }

    /// A bit of `name` changed, on the medium and as read.
    pub fn flip(&mut self, name: &str, at: usize, bit: u8) {
        let Ok(ino) = self.ino(name) else { return };
        for d in [&mut self.data, &mut self.durable] {
            if let Some(b) = d.get_mut(&ino).and_then(|v| v.get_mut(at)) {
                *b ^= 1 << (bit % 8);
            }
        }
    }

    /// Bytes put as a file, on the medium at once.
    pub fn put_file(&mut self, name: &str, bytes: &[u8]) {
        let _own = Exempt::new();
        let ino = self.next_ino;
        self.next_ino += 1;
        self.dir.insert(name.to_string(), ino);
        self.durable_dir.insert(name.to_string(), ino);
        self.data.insert(ino, bytes.to_vec());
        self.durable.insert(ino, bytes.to_vec());
    }

    /// What is on the disk, said: each name, its length now and on the
    /// medium, and the writes it has pending.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (name, ino) in self.dir.iter() {
            let now = self.data.get(ino).map_or(0, |d| d.len());
            let durable = self.durable.get(ino).map_or(0, |d| d.len());
            let pending = self.pending.get(ino).map_or(0, |p| p.len());
            let named = if self.durable_dir.get(name) == Some(ino) { "" } else { " (name not synced)" };
            out.push_str(&format!("  {} ino {} len {} durable {} pending {}{}\n", name, ino, now, durable, pending,
                                  named));
        }
        out.push_str(&format!("  {} directory changes pending\n", self.dir_ops.len()));
        out
    }

    /// The names now, sorted.
    pub fn names(&self) -> Vec<String> {
        self.dir.keys().cloned().collect()
    }

    pub fn len_of(&self, name: &str) -> usize {
        self.ino(name).ok().and_then(|i| self.data.get(&i)).map_or(0, |d| d.len())
    }
}

fn put(file: &mut Vec<u8>, at: usize, bytes: &[u8]) {
    if file.len() < at + bytes.len() {
        file.resize(at + bytes.len(), 0);
    }
    file[at..at + bytes.len()].copy_from_slice(bytes);
}

/// The disk as the log's files: a handle the run keeps too.
#[derive(Clone)]
pub struct SimFiles(pub Rc<RefCell<Disk>>);

impl Files for SimFiles {
    fn list(&mut self, out: &mut Vec<String>) -> Result<(), FileError> {
        let mut d = self.0.borrow_mut();
        if d.fault() {
            return Err(FileError::Io);
        }
        for n in d.dir.keys() {
            out.push(n.clone());
        }
        Ok(())
    }

    fn len(&mut self, name: &str) -> Result<u64, FileError> {
        let mut d = self.0.borrow_mut();
        if d.fault() {
            return Err(FileError::Io);
        }
        let ino = d.ino(name)?;
        Ok(d.data.get(&ino).map_or(0, |v| v.len()) as u64)
    }

    fn read_at(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FileError> {
        let mut d = self.0.borrow_mut();
        if d.fault() {
            return Err(FileError::Io);
        }
        let ino = d.ino(name)?;
        let data = d.data.get(&ino).map(|v| v.as_slice()).unwrap_or(&[]);
        let Ok(at) = usize::try_from(offset) else { return Ok(0) };
        if at >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - at);
        buf[..n].copy_from_slice(&data[at..at + n]);
        Ok(n)
    }

    fn append(&mut self, name: &str, bytes: &[u8]) -> Result<(), FileError> {
        let _own = Exempt::new();
        let mut d = self.0.borrow_mut();
        let fail = d.fault();
        let ino = match d.dir.get(name) {
            Some(&i) => i,
            None => {
                let i = d.next_ino;
                d.next_ino += 1;
                d.dir.insert(name.to_string(), i);
                d.dir_ops.push(DirOp::Link(name.to_string(), i));
                d.data.insert(i, Vec::new());
                i
            }
        };
        let n = if fail { d.rng.below(bytes.len() as u64 + 1) as usize } else { bytes.len() };
        let file = d.data.entry(ino).or_default();
        let at = file.len() as u64;
        file.extend_from_slice(&bytes[..n]);
        if n > 0 {
            d.pending.entry(ino).or_default().push(Op::Write { at, bytes: bytes[..n].to_vec() });
        }
        if fail { Err(FileError::Io) } else { Ok(()) }
    }

    fn truncate(&mut self, name: &str, len: u64) -> Result<(), FileError> {
        let mut d = self.0.borrow_mut();
        if d.fault() {
            return Err(FileError::Io);
        }
        let ino = d.ino(name)?;
        if let Some(f) = d.data.get_mut(&ino) {
            f.truncate(len as usize);
        }
        d.pending.entry(ino).or_default().push(Op::Truncate(len));
        Ok(())
    }

    fn sync(&mut self, name: &str) -> Result<(), FileError> {
        let _own = Exempt::new();
        let mut d = self.0.borrow_mut();
        if d.fault() {
            return Err(FileError::Io);
        }
        let ino = d.ino(name)?;
        let now = d.data.get(&ino).cloned().unwrap_or_default();
        d.durable.insert(ino, now);
        d.pending.remove(&ino);
        Ok(())
    }

    fn sync_dir(&mut self) -> Result<(), FileError> {
        let _own = Exempt::new();
        let mut d = self.0.borrow_mut();
        if d.fault() || d.fail_kind == Some("sync_dir") {
            return Err(FileError::Io);
        }
        d.durable_dir = d.dir.clone();
        d.dir_ops.clear();
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), FileError> {
        let mut d = self.0.borrow_mut();
        if d.fault() {
            return Err(FileError::Io);
        }
        let ino = d.dir.remove(from).ok_or(FileError::NotFound)?;
        d.dir.insert(to.to_string(), ino);
        d.dir_ops.push(DirOp::Rename(from.to_string(), to.to_string()));
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<(), FileError> {
        let mut d = self.0.borrow_mut();
        if d.fault() || d.fail_kind == Some("remove") {
            return Err(FileError::Io);
        }
        d.dir.remove(name).ok_or(FileError::NotFound)?;
        d.dir_ops.push(DirOp::Unlink(name.to_string()));
        Ok(())
    }
}

/* ---- states in one number ---- */

fn entry_hash(e: &Entry) -> u64 {
    mix(mix(mix(e.index, e.term), e.kind as u64), hash_bytes(&e.data))
}

fn snap_hash(s: &Snapshot) -> u64 {
    let mut h = mix(s.meta.index, s.meta.term);
    for v in &s.meta.conf.voters {
        h = mix(h, *v);
    }
    h = mix(h, u64::MAX);
    for l in &s.meta.conf.learners {
        h = mix(h, *l);
    }
    for p in s.data.pieces() {
        h = mix(h, hash_bytes(p));
    }
    mix(h, s.data.len())
}

fn state_hash(hs: HardState, snap: u64, chain: u64, last: u64) -> u64 {
    mix(mix(mix(mix(mix(hs.term, hs.vote), hs.commit), snap), chain), last)
}

/// A storage's state in one number: its hard state, its snapshot, and
/// every entry after the snapshot.
fn digest<S: Storage>(s: &S) -> u64 {
    let hs = s.initial_state().map(|st| st.hard_state).unwrap_or_default();
    let snap = s.snapshot().map(|sn| snap_hash(&sn)).unwrap_or(0);
    let mut chain = 0;
    let mut out = Vec::new();
    for i in s.first_index()..=s.last_index() {
        out.clear();
        if s.entries(i, i + 1, u64::MAX, &mut out).is_err() || out.len() != 1 {
            return 0;
        }
        chain = mix(chain, entry_hash(&out[0]));
    }
    state_hash(hs, snap, chain, s.last_index())
}

/// What a state is, said: for a finding.
fn describe<S: Storage>(s: &S) -> String {
    let hs = s.initial_state().map(|st| st.hard_state).unwrap_or_default();
    let snap = s.snapshot().map(|sn| (sn.meta.index, sn.meta.term)).unwrap_or((0, 0));
    format!("hard {:?}, snapshot {:?}, entries {}..={}", hs, snap, s.first_index(), s.last_index())
}

/* ---- the run ---- */

/// A record of the log's, as reading it back replays it.
#[derive(Clone, Debug)]
enum Rec {
    Entry(Entry),
    Hard(HardState),
}

/// Which state the log read back as.
enum Matched {
    /// The state at the last sync and the first `k` records after it.
    Prefix(usize),
    /// What the snapshot that failed would have left.
    Post,
}

struct Run {
    r: Rng,
    disk: SimFiles,
    config: LogConfig,
    log: Option<DiskLog<SimFiles>>,
    /// What the log has taken, as a storage in memory has it.
    model: MemStorage,
    /// The model as of the last sync, and the records written since.
    synced: MemStorage,
    pending: Vec<Rec>,
    /// The hard state the log last wrote.
    written: HardState,
    /// What a durable call that failed would have left, had it finished.
    post: Option<MemStorage>,
    /// Faults while the run goes, and off for opening when they will not
    /// let it.
    faults: u64,
}

const OPEN_TRIES: u32 = 40;

impl Run {
    fn log(&mut self) -> &mut DiskLog<SimFiles> {
        self.log.as_mut().expect("an open log")
    }

    fn set_faults(&self, on: bool) {
        self.disk.0.borrow_mut().fail_one_in = if on { self.faults } else { 0 };
    }

    /// The log opened -- again and again while the disk fails it, and with
    /// the faults off at the last -- and refused only where `may_refuse`.
    fn open(&mut self) -> Result<Result<DiskLog<SimFiles>, LogError>, String> {
        for k in 0..=OPEN_TRIES {
            self.set_faults(k < OPEN_TRIES);
            match DiskLog::open(self.disk.clone(), self.config) {
                Err(LogError::Io(_)) if k < OPEN_TRIES => continue,
                other => {
                    self.set_faults(true);
                    return Ok(other);
                }
            }
        }
        unreachable!()
    }

    /// The log opened after a cut, a failure or a stop, and held to the
    /// states it may read back as; the model then made what it read.
    fn reopen(&mut self, exact: bool) -> Result<(), String> {
        self.log = None;
        let log = match self.open()? {
            Ok(log) => log,
            Err(e) => return Err(format!("a log the crash model allows was refused: {:?}", e)),
        };
        let got = digest(&log);
        if exact {
            if got != digest(&self.model) {
                return Err(format!("read back as {}, written as {}", describe(&log), describe(&self.model)));
            }
        } else {
            match self.matched(got) {
                Some(Matched::Prefix(k)) => {
                    let mut m = self.synced.clone();
                    for rec in self.pending.iter().take(k) {
                        match rec {
                            Rec::Entry(e) => m.append(std::slice::from_ref(e)).map_err(|e| format!("{:?}", e))?,
                            Rec::Hard(hs) => m.set_hard_state(*hs),
                        }
                    }
                    self.model = m;
                }
                Some(Matched::Post) => self.model = self.post.take().expect("the state after"),
                None => {
                    return Err(format!(
                        "read back as {}, which is neither the state at the last sync ({}) with some of the {} \
                         records written after it nor the state a failed snapshot would have left",
                        describe(&log),
                        describe(&self.synced),
                        self.pending.len()
                    ))
                }
            }
        }
        self.written = log.hard_state();
        self.synced = self.model.clone();
        self.pending.clear();
        self.post = None;
        self.log = Some(log);
        Ok(())
    }

    /// The state among those the log may read back as whose number is `got`.
    fn matched(&self, got: u64) -> Option<Matched> {
        let _own = Exempt::new();
        let s = &self.synced;
        let snap = s.snapshot().map(|sn| snap_hash(&sn)).unwrap_or(0);
        let first = s.first_index();
        let mut chains: Vec<u64> = Vec::new();
        let mut c = 0;
        let mut out = Vec::new();
        for i in first..=s.last_index() {
            out.clear();
            s.entries(i, i + 1, u64::MAX, &mut out).ok()?;
            c = mix(c, entry_hash(&out[0]));
            chains.push(c);
        }
        let mut hs = s.hard_state();
        let state = |hs: HardState, chains: &[u64]| {
            state_hash(hs, snap, chains.last().copied().unwrap_or(0), first - 1 + chains.len() as u64)
        };
        if state(hs, &chains) == got {
            return Some(Matched::Prefix(0));
        }
        for (k, rec) in self.pending.iter().enumerate() {
            match rec {
                Rec::Entry(e) => {
                    if e.index >= first {
                        chains.truncate((e.index - first) as usize);
                        let prev = chains.last().copied().unwrap_or(0);
                        chains.push(mix(prev, entry_hash(e)));
                    }
                }
                Rec::Hard(h) => hs = *h,
            }
            if state(hs, &chains) == got {
                return Some(Matched::Prefix(k + 1));
            }
        }
        if self.post.as_ref().is_some_and(|p| digest(p) == got) {
            return Some(Matched::Post);
        }
        None
    }

    /// A call that failed: the log stops, and is opened again -- after a
    /// power cut, or as a program started over.
    fn failed(&mut self) -> Result<(), String> {
        match self.log().sync() {
            Err(LogError::Stopped) => {}
            other => return Err(format!("a log that failed answered {:?} to the next call", other)),
        }
        if self.r.chance(50) { self.cut() } else { self.reopen(false) }
    }

    fn cut(&mut self) -> Result<(), String> {
        if std::env::var_os("RAFT_LOG_TRACE").is_some() {
            eprintln!("cut, the disk:\n{}", self.disk.0.borrow().dump());
        }
        self.log = None;
        let mut r = Rng::new(self.r.next());
        self.disk.0.borrow_mut().crash(&mut r);
        self.reopen(false)
    }

    fn data(&mut self) -> Vec<u8> {
        let n = match self.r.below(1000) {
            0 => self.r.range(64 * 1024, raft::wire::MAX_ENTRY_BYTES as u64),
            1..=40 => self.r.range(2048, 32 * 1024),
            41..=300 => self.r.range(32, 2048),
            _ => self.r.below(32),
        } as usize;
        let seed = self.r.next();
        (0..n).map(|k| (mix(seed, k as u64 / 8) >> ((k % 8) * 8)) as u8).collect()
    }

    fn append(&mut self) -> Result<(), String> {
        let first = self.model.first_index();
        let last = self.model.last_index();
        let commit = self.model.hard_state().commit;
        /* Never over what is committed, as Raft never writes there */
        let lo = (commit + 1).max(first);
        let start = if self.r.chance(80) || lo > last { last + 1 } else { self.r.range(lo, last + 1) };
        let n = if self.r.chance(5) { self.r.range(30, 300) } else { self.r.range(1, 12) };
        let mut term = self.model.term(start - 1).unwrap_or(0).max(1);
        let mut ents = Vec::with_capacity(n as usize);
        for k in 0..n {
            if self.r.chance(3) {
                term += 1;
            }
            let kinds = [EntryKind::Normal, EntryKind::Normal, EntryKind::Noop, EntryKind::Change];
            let kind = kinds[self.r.below(4) as usize];
            ents.push(Entry { term, index: start + k, kind, data: self.data() });
        }
        self.pending.extend(ents.iter().filter(|e| e.index >= first).cloned().map(Rec::Entry));
        match self.log().append(&ents) {
            Ok(()) => self.model.append(&ents).map_err(|e| format!("the model refused an append: {:?}", e)),
            Err(LogError::Io(_)) | Err(LogError::Full) => self.failed(),
            Err(e) => Err(format!("an append from {} of {} refused: {:?}", start, n, e)),
        }
    }

    fn hard_state(&mut self) -> Result<(), String> {
        let cur = self.model.hard_state();
        let last = self.model.last_index();
        let term = cur.term + u64::from(self.r.chance(20));
        let vote = if self.r.chance(30) { self.r.below(5) } else { cur.vote };
        let commit = if last > cur.commit && self.r.chance(70) { self.r.range(cur.commit, last) } else { cur.commit };
        let hs = HardState { term, vote, commit };
        if hs != self.written {
            self.pending.push(Rec::Hard(hs));
        }
        match self.log().set_hard_state(hs) {
            Ok(()) => {
                self.model.set_hard_state(hs);
                self.written = hs;
                Ok(())
            }
            Err(LogError::Io(_)) => self.failed(),
            Err(e) => Err(format!("a hard state {:?} refused: {:?}", hs, e)),
        }
    }

    fn sync(&mut self) -> Result<(), String> {
        match self.log().sync() {
            Ok(()) => {
                self.synced = self.model.clone();
                self.pending.clear();
                self.post = None;
                Ok(())
            }
            Err(LogError::Io(_)) => self.failed(),
            Err(e) => Err(format!("a sync refused: {:?}", e)),
        }
    }

    fn conf(&mut self) -> ConfState {
        let mut voters: Vec<u64> = (0..self.r.range(0, 4)).map(|_| self.r.range(1, 9)).collect();
        voters.sort_unstable();
        voters.dedup();
        let mut learners: Vec<u64> =
            (0..self.r.range(0, 2)).map(|_| self.r.range(1, 9)).filter(|l| voters.binary_search(l).is_err()).collect();
        learners.sort_unstable();
        learners.dedup();
        ConfState { voters, learners }
    }

    fn snapshot_data(&mut self) -> SnapshotData {
        let n = match self.r.below(10) {
            0 => self.r.range(200 * 1024, 700 * 1024),
            1..=4 => self.r.range(1024, 200 * 1024),
            _ => self.r.below(1024),
        };
        let seed = self.r.next();
        let mut b = SnapshotBuilder::new();
        let mut k = 0u64;
        while k < n {
            let len = (n - k).min(4096) as usize;
            let chunk: Vec<u8> = (0..len as u64).map(|j| mix(seed, k + j) as u8).collect();
            assert!(b.push(&chunk));
            k += len as u64;
        }
        b.finish()
    }

    /// A snapshot of the state machine's, as of an entry committed.
    fn take(&mut self) -> Result<(), String> {
        let at = self.model.snapshot_meta().index;
        let commit = self.model.hard_state().commit;
        if commit <= at {
            return Ok(());
        }
        let index = self.r.range(at + 1, commit);
        let (conf, data) = (self.conf(), self.snapshot_data());
        let mut post = self.model.clone();
        post.create_snapshot(index, conf.clone(), data.clone()).map_err(|e| format!("model: {:?}", e))?;
        self.post = Some(post);
        match self.log().create_snapshot(index, conf, data) {
            Ok(()) => self.durable_done(),
            Err(LogError::Io(_)) => self.failed(),
            Err(e) => Err(format!("a snapshot at {} refused: {:?}", index, e)),
        }
    }

    /// A snapshot a leader sent: within the log, or past its end -- never
    /// below what is committed, which a node never restores.
    fn install(&mut self) -> Result<(), String> {
        let lo = self.model.snapshot_meta().index.max(self.model.hard_state().commit) + 1;
        let last = self.model.last_index();
        let index = lo + self.r.range(0, last.saturating_sub(lo) + 40);
        let term = match self.model.term(index) {
            Ok(t) if self.r.chance(50) => t,
            _ => self.model.hard_state().term.max(self.model.term(last).unwrap_or(0)) + self.r.below(2),
        };
        let snap = Snapshot { meta: SnapshotMeta { index, term, conf: self.conf() }, data: self.snapshot_data() };
        let mut post = self.model.clone();
        post.apply_snapshot(snap.clone()).map_err(|e| format!("model: {:?}", e))?;
        self.post = Some(post);
        match self.log().apply_snapshot(snap) {
            Ok(()) => self.durable_done(),
            Err(LogError::Io(_)) => self.failed(),
            Err(e) => Err(format!("a snapshot at {} sent refused: {:?}", index, e)),
        }
    }

    /// A durable call done: the model is what it left, on the medium.
    fn durable_done(&mut self) -> Result<(), String> {
        self.model = self.post.take().expect("the state after");
        self.synced = self.model.clone();
        self.pending.clear();
        Ok(())
    }

    /// Started over with nothing lost: synced first, every byte of it back.
    fn restart(&mut self) -> Result<(), String> {
        if self.r.chance(50) {
            match self.log().sync() {
                Ok(()) => {
                    self.synced = self.model.clone();
                    self.pending.clear();
                    self.post = None;
                    self.log = None;
                    return self.reopen(true);
                }
                Err(LogError::Io(_)) => return self.failed(),
                Err(e) => return Err(format!("a sync refused: {:?}", e)),
            }
        }
        self.reopen(false)
    }

    /// A byte of a sealed segment or of the snapshot changed on the medium:
    /// the log refuses to open; the byte put back, it reads as it was.
    fn flip(&mut self) -> Result<(), String> {
        match self.log().sync() {
            Ok(()) => {}
            Err(LogError::Io(_)) => return self.failed(),
            Err(e) => return Err(format!("a sync refused: {:?}", e)),
        }
        self.synced = self.model.clone();
        self.pending.clear();
        self.post = None;
        self.log = None;
        let names = self.disk.0.borrow().names();
        let mut wal: Vec<&String> = names.iter().filter(|n| n.starts_with("wal.")).collect();
        wal.pop();
        let mut sealed: Vec<&String> = names.iter().filter(|n| n.starts_with("snap.")).collect();
        sealed.extend(wal);
        let Some(&name) = sealed.get(self.r.below(sealed.len() as u64) as usize) else {
            return self.reopen(true);
        };
        let len = self.disk.0.borrow().len_of(name);
        if len == 0 {
            return self.reopen(true);
        }
        let (at, bit) = (self.r.below(len as u64) as usize, self.r.below(8) as u8);
        self.disk.0.borrow_mut().flip(name, at, bit);
        match self.open()? {
            Err(LogError::Corrupt(_)) => {}
            Ok(log) => {
                return Err(format!("{} with byte {} of {} changed opened, as {}", name, at, len, describe(&log)))
            }
            Err(e) => return Err(format!("{} with a byte changed: {:?}", name, e)),
        }
        self.disk.0.borrow_mut().flip(name, at, bit);
        self.reopen(true)
    }

    fn step(&mut self) -> Result<(), String> {
        let pick = self.r.below(100);
        let r = match pick {
            0..=51 => self.append(),
            52..=66 => self.hard_state(),
            67..=81 => self.sync(),
            82..=85 => self.take(),
            86..=87 => self.install(),
            88..=94 => self.cut(),
            95..=97 => self.restart(),
            _ => self.flip(),
        };
        if std::env::var_os("RAFT_LOG_TRACE").is_some() {
            let what = match pick {
                0..=51 => "append",
                52..=66 => "hard state",
                67..=81 => "sync",
                82..=85 => "take",
                86..=87 => "install",
                88..=94 => "cut",
                95..=97 => "restart",
                _ => "flip",
            };
            eprintln!("{:10} -> model {}, {} pending, {} failures", what, describe(&self.model), self.pending.len(),
                      self.disk.0.borrow().failures);
        }
        r
    }
}

/// Noise named as the log names its files -- some with a segment's header
/// that checks, so that their records are read -- refused, or read as an
/// empty log: never an entry, never a panic.
fn noise(seed: u64) -> Result<(), String> {
    let mut r = Rng::new(seed ^ 0x0153);
    let disk = SimFiles(Rc::new(RefCell::new(Disk::new(seed))));
    let first = r.range(1, 1 << 40);
    for k in 0..r.range(1, 4) {
        let seq = first + k;
        let mut bytes = Vec::new();
        if r.chance(60) {
            bytes.extend_from_slice(b"RAFTWAL\0");
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&seq.to_le_bytes());
            let crc = raft::crc32c::crc32c(&bytes);
            bytes.extend_from_slice(&crc.to_le_bytes());
        }
        for _ in 0..r.below(2000) {
            bytes.push(r.next() as u8);
        }
        disk.0.borrow_mut().put_file(&format!("wal.{:016x}", seq), &bytes);
    }
    if r.chance(50) {
        let bytes: Vec<u8> = (0..r.below(3000)).map(|_| r.next() as u8).collect();
        disk.0.borrow_mut().put_file(&format!("snap.{:016x}", r.range(1, 1000)), &bytes);
    }
    match DiskLog::open(disk, LogConfig::default()) {
        Ok(log) => {
            if log.last_index() != 0 || log.hard_state() != HardState::default() || log.snapshot_meta().index != 0 {
                return Err(format!("noise read as a log: {}", describe(&log)));
            }
            Ok(())
        }
        Err(LogError::Corrupt(_)) => Ok(()),
        Err(e) => Err(format!("noise: {:?}", e)),
    }
}

/// `iterations` steps from `seed`.
pub fn run(seed: u64, iterations: u64) -> Result<(), String> {
    noise(seed)?;
    let mut r = Rng::new(seed ^ 0x106);
    let disk = SimFiles(Rc::new(RefCell::new(Disk::new(seed))));
    let config = LogConfig { segment_bytes: [4096, 8192, 65536, 1 << 20][r.below(4) as usize] };
    let faults = if r.chance(30) { r.range(40, 400) } else { 0 };
    let (log, model) = if r.chance(50) {
        let voters = [1, 2, 3];
        let log = DiskLog::bootstrap(disk.clone(), config, &voters, &[])
            .map_err(|e| format!("bootstrap: {:?}", e))?;
        (log, MemStorage::bootstrap(&voters, &[]))
    } else {
        let log = DiskLog::open(disk.clone(), config).map_err(|e| format!("open: {:?}", e))?;
        (log, MemStorage::new())
    };
    let written = log.hard_state();
    let mut run = Run {
        r,
        disk,
        config,
        log: Some(log),
        synced: model.clone(),
        model,
        pending: Vec::new(),
        written,
        post: None,
        faults,
    };
    run.set_faults(true);
    for _ in 0..iterations {
        run.step()?;
    }
    run.cut()
}

/* ---- the scenarios each rule of the log's is held to, one at a time ---- */

pub const SCENARIOS: &[(&str, fn() -> Result<(), String>)] = &[
    ("log_torn_tail", torn_tail),
    ("log_sealed_segments", sealed_segments),
    ("log_cut_synced", cut_synced),
    ("log_snapshot_durable", snapshot_durable),
    ("log_compaction", compaction),
    ("log_open_syncs", open_syncs),
    ("log_install_drops_log", install_drops_log),
];

fn entry(index: u64, term: u64, len: usize) -> Entry {
    Entry { term, index, kind: EntryKind::Normal, data: (0..len).map(|k| (index as usize * 31 + k) as u8).collect() }
}

fn fresh(segment_bytes: u64) -> (SimFiles, DiskLog<SimFiles>) {
    let disk = SimFiles(Rc::new(RefCell::new(Disk::new(1))));
    let log = DiskLog::open(disk.clone(), LogConfig { segment_bytes }).expect("an empty log");
    (disk, log)
}

fn entries_of<S: Storage>(s: &S) -> Vec<Entry> {
    let mut out = Vec::new();
    if s.last_index() >= s.first_index() {
        s.entries(s.first_index(), s.last_index() + 1, u64::MAX, &mut out).expect("the entries");
    }
    out
}

/// A record torn in the middle of its bytes, its tail noise: cut, never
/// read -- its own length and its header's are whole, so only its CRC says.
fn torn_tail() -> Result<(), String> {
    let (disk, mut log) = fresh(1 << 20);
    log.append(&[entry(1, 1, 10), entry(2, 1, 10)]).map_err(|e| format!("{:?}", e))?;
    log.sync().map_err(|e| format!("{:?}", e))?;
    /* Larger than what the log gathers: on the disk without a sync */
    log.append(&[entry(3, 1, 200 * 1024)]).map_err(|e| format!("{:?}", e))?;
    drop(log);
    let mut noise = Rng::new(7);
    disk.0.borrow_mut().crash_with(
        usize::MAX,
        &mut |_, op| match op {
            Op::Write { bytes, .. } if bytes.len() > 1000 => Fate::Torn { keep: 100, fill: Fill::Noise },
            _ => Fate::Kept,
        },
        &mut noise,
    );
    let log = DiskLog::open(disk, LogConfig { segment_bytes: 1 << 20 }).map_err(|e| format!("open: {:?}", e))?;
    let got = entries_of(&log);
    if got != [entry(1, 1, 10), entry(2, 1, 10)] {
        let last = got.last().map(|e| e.index);
        return Err(format!("a torn record read back: {} entries, the last {:?}", got.len(), last));
    }
    Ok(())
}

/// A segment synced before the next is begun: a cut that loses everything
/// not synced still leaves every segment but the last whole.
fn sealed_segments() -> Result<(), String> {
    let (disk, mut log) = fresh(4096);
    let mut all = Vec::new();
    for i in 1..=60 {
        let e = entry(i, 1, 300);
        log.append(std::slice::from_ref(&e)).map_err(|e| format!("{:?}", e))?;
        all.push(e);
    }
    log.sync().map_err(|e| format!("{:?}", e))?;
    if log.segments() < 3 {
        return Err(format!("{} segments: the scenario wants several", log.segments()));
    }
    drop(log);
    disk.0.borrow_mut().crash_with(0, &mut |_, _| Fate::Lost, &mut Rng::new(1));
    let log = DiskLog::open(disk, LogConfig { segment_bytes: 4096 }).map_err(|e| format!("open: {:?}", e))?;
    if entries_of(&log) != all {
        return Err(format!("synced entries lost: {} of {} back", log.last_index(), all.len()));
    }
    Ok(())
}

/// A torn tail's cut synced before anything is written after it: a record
/// of the tail that was whole, past a lost one, never comes back behind
/// what is written in the lost one's place.
fn cut_synced() -> Result<(), String> {
    let (disk, mut log) = fresh(1 << 20);
    log.append(&[entry(1, 1, 10)]).map_err(|e| format!("{:?}", e))?;
    log.sync().map_err(|e| format!("{:?}", e))?;
    /* Two records each larger than what is gathered: on the disk, unsynced */
    let big = 150 * 1024;
    log.append(&[entry(2, 1, big), entry(3, 1, big)]).map_err(|e| format!("{:?}", e))?;
    drop(log);
    /* The cut: the second's writes lost, the third's kept */
    let mut nth = 0;
    disk.0.borrow_mut().crash_with(
        usize::MAX,
        &mut |_, op| match op {
            Op::Write { bytes, .. } if bytes.len() == 8 || bytes.len() > 1000 => {
                nth += 1;
                if nth <= 2 { Fate::Lost } else { Fate::Kept }
            }
            _ => Fate::Kept,
        },
        &mut Rng::new(2),
    );
    /* Opened: cut where the second was. A record the size of the second
     * written there, and a cut that loses the cut itself but keeps that */
    let mut log = DiskLog::open(disk.clone(), LogConfig { segment_bytes: 1 << 20 })
        .map_err(|e| format!("open: {:?}", e))?;
    if log.last_index() != 1 {
        return Err(format!("the tail past a lost record read: last {}", log.last_index()));
    }
    let other = Entry { term: 2, ..entry(2, 2, big) };
    log.append(std::slice::from_ref(&other)).map_err(|e| format!("{:?}", e))?;
    drop(log);
    disk.0.borrow_mut().crash_with(
        usize::MAX,
        &mut |_, op| match op {
            Op::Truncate(_) => Fate::Lost,
            Op::Write { .. } => Fate::Kept,
        },
        &mut Rng::new(3),
    );
    let log = DiskLog::open(disk, LogConfig { segment_bytes: 1 << 20 }).map_err(|e| format!("open: {:?}", e))?;
    let got = entries_of(&log);
    if got.len() > 2 || got.iter().any(|e| e.index == 3) {
        return Err(format!("an entry cut once read again: {} entries back", got.len()));
    }
    Ok(())
}

/// A snapshot's file on the medium before its name is: a cut that loses
/// every write not synced leaves the snapshot the log names readable.
fn snapshot_durable() -> Result<(), String> {
    let (disk, mut log) = fresh(1 << 20);
    let ents: Vec<Entry> = (1..=20).map(|i| entry(i, 1, 50)).collect();
    log.append(&ents).map_err(|e| format!("{:?}", e))?;
    log.set_hard_state(HardState { term: 1, vote: 0, commit: 20 }).map_err(|e| format!("{:?}", e))?;
    log.sync().map_err(|e| format!("{:?}", e))?;
    let mut b = SnapshotBuilder::new();
    assert!(b.push(&[7u8; 100_000]));
    let conf = ConfState { voters: vec![1, 2, 3], learners: vec![] };
    log.create_snapshot(15, conf, b.finish()).map_err(|e| format!("{:?}", e))?;
    drop(log);
    disk.0.borrow_mut().crash_with(0, &mut |_, _| Fate::Lost, &mut Rng::new(4));
    let log = DiskLog::open(disk, LogConfig { segment_bytes: 1 << 20 }).map_err(|e| format!("open: {:?}", e))?;
    if log.snapshot_meta().index != 15 || log.last_index() != 20 {
        return Err(format!("after the snapshot: {}", describe(&log)));
    }
    Ok(())
}

/// Compaction removes a segment only when every entry of it is in the
/// snapshot: the log past the snapshot all there after it.
fn compaction() -> Result<(), String> {
    let (disk, mut log) = fresh(4096);
    let ents: Vec<Entry> = (1..=120).map(|i| entry(i, 1, 200)).collect();
    log.append(&ents).map_err(|e| format!("{:?}", e))?;
    log.set_hard_state(HardState { term: 1, vote: 0, commit: 120 }).map_err(|e| format!("{:?}", e))?;
    log.sync().map_err(|e| format!("{:?}", e))?;
    let before = log.segments();
    log.create_snapshot(50, ConfState { voters: vec![1], learners: vec![] }, SnapshotData::default())
        .map_err(|e| format!("{:?}", e))?;
    /* The snapshot's marker begins a segment of its own: one at least
     * removed leaves no more than there were */
    if log.segments() > before {
        return Err(format!("{} segments before the snapshot, {} after: none removed", before, log.segments()));
    }
    drop(log);
    let log = DiskLog::open(disk, LogConfig { segment_bytes: 4096 }).map_err(|e| format!("open: {:?}", e))?;
    let got = entries_of(&log);
    if got.first().map(|e| e.index) != Some(51) || got.len() != 70 || got != ents[50..] {
        return Err(format!("after compaction: {}", describe(&log)));
    }
    Ok(())
}

/// What a log opened reads is the medium's before it is used: a segment
/// begun whose name never made it into the directory -- the directory's
/// sync failed, and the program started over, not the machine -- is made
/// durable by the open, so that entries written to it after survive a cut.
fn open_syncs() -> Result<(), String> {
    let (disk, mut log) = fresh(4096);
    let mut all = Vec::new();
    let mut i = 1;
    /* Entries until a segment is to be begun, the directory failing */
    disk.0.borrow_mut().fail_kind = Some("sync_dir");
    loop {
        let e = entry(i, 1, 300);
        match log.append(std::slice::from_ref(&e)) {
            Ok(()) => all.push(e),
            Err(LogError::Io(_)) => break,
            Err(e) => return Err(format!("{:?}", e)),
        }
        i += 1;
        if i > 100 {
            return Err("no segment begun in 100 entries".into());
        }
    }
    drop(log);
    disk.0.borrow_mut().fail_kind = None;
    /* Started over, the machine never down: what was synced before the
     * failure is there, and more is written and synced */
    let mut log = DiskLog::open(disk.clone(), LogConfig { segment_bytes: 4096 }).map_err(|e| format!("open: {:?}", e))?;
    let back = entries_of(&log);
    let more = entry(back.len() as u64 + 1, 1, 300);
    log.append(std::slice::from_ref(&more)).map_err(|e| format!("{:?}", e))?;
    log.sync().map_err(|e| format!("{:?}", e))?;
    let want = log.last_index();
    drop(log);
    /* The cut keeps the directory's changes no sync made durable: none */
    disk.0.borrow_mut().crash_with(0, &mut |_, _| Fate::Lost, &mut Rng::new(5));
    let log = DiskLog::open(disk, LogConfig { segment_bytes: 4096 }).map_err(|e| format!("open: {:?}", e))?;
    if log.last_index() != want {
        return Err(format!("synced entries lost: {} back of {}", log.last_index(), want));
    }
    Ok(())
}

/// A leader's snapshot drops the whole log before it, even where the
/// segments that held it are still there -- the removal of them failed --
/// and no entry has followed the snapshot yet.
fn install_drops_log() -> Result<(), String> {
    let (disk, mut log) = fresh(4096);
    let ents: Vec<Entry> = (1..=80).map(|i| entry(i, 1, 200)).collect();
    log.append(&ents).map_err(|e| format!("{:?}", e))?;
    log.set_hard_state(HardState { term: 1, vote: 0, commit: 10 }).map_err(|e| format!("{:?}", e))?;
    log.sync().map_err(|e| format!("{:?}", e))?;
    let snap = Snapshot {
        meta: SnapshotMeta { index: 40, term: 2, conf: ConfState { voters: vec![1], learners: vec![] } },
        data: SnapshotData::default(),
    };
    disk.0.borrow_mut().fail_kind = Some("remove");
    match log.apply_snapshot(snap) {
        Err(LogError::Io(_)) => {}
        other => return Err(format!("the snapshot with removals failing: {:?}", other)),
    }
    drop(log);
    disk.0.borrow_mut().fail_kind = None;
    let log = DiskLog::open(disk, LogConfig { segment_bytes: 4096 }).map_err(|e| format!("open: {:?}", e))?;
    if log.snapshot_meta().index != 40 || log.last_index() != 40 {
        return Err(format!("the log before a leader's snapshot read back: {}", describe(&log)));
    }
    Ok(())
}
