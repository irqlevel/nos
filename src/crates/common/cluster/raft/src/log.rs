//! The log on disk: what a member keeps durable -- its entries, its hard
//! state and its newest snapshot -- in the files of a directory its user
//! hands it ([`Files`]), read back after a crash as the member left it. What
//! was synced is all there; a torn tail is cut, and never read as an entry;
//! anything else the files say that a log never would is refused.
//!
//! The log is etcd's write-ahead log in shape. Segments (`wal.<seq>`) are
//! written only at their end: a header, then records, each its length, a
//! CRC32C over the length and the payload, and the payload -- an entry, the
//! hard state, or a snapshot's marker. Entries that replace others are
//! written after them, never over them, and reading the log back replays
//! the records in order as [`MemStorage::append`] takes them: so nothing is
//! ever cut and written again in place, where a crash between the two would
//! bring back what was cut. The hard state is a record too, riding the one
//! sync that makes a `Ready`'s entries durable; every segment begins with
//! the hard state as it stood, so the newest is never in a segment that is
//! removed. A snapshot is a file of its own (`snap.<index>`), written as
//! `.tmp`, synced, renamed and the directory synced -- and only then named
//! by a marker in the log: taken (the state machine's, the entries up to it
//! compacted) or installed (a leader's, the whole log dropped).
//!
//! Opening the log reads it twice. The first pass checks every record, cuts
//! the last segment's tail at the first record that does not check -- and
//! syncs the cut before anything is written after it -- and finds the
//! newest marker and hard state. Any other segment was synced before the
//! next was made, so a bad record there is corruption, and the log refuses
//! to open. The second pass loads the snapshot the newest marker names and
//! replays the entries past it into a [`MemStorage`], which is what the
//! member reads: as etcd does, the log since the snapshot is held in
//! memory, in pieces, and the files are for its durability.
//!
//! Nothing is allocated larger than a record (an entry of
//! `wire::MAX_ENTRY_BYTES` and its header) or a snapshot's piece, however
//! long the log: the kernel's heap gives 512 KiB at most.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::conf::MAX_MEMBERS;
use crate::node::MAX_TERM_OR_INDEX;
use crc::crc32c;
use crate::storage::{bootstrap_entries, MemStorage, RaftState, Storage, StorageError, BOOTSTRAP_TERM};
use crate::types::{
    ConfState, Entry, HardState, NodeId, Snapshot, SnapshotBuilder, SnapshotData, SnapshotMeta, MAX_SNAPSHOT_BYTES,
    NONE, PIECE_MAX,
};
use crate::wire;

/// Why a call of the user's files failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileError {
    /// No file of that name.
    NotFound,
    /// Anything else: the medium, the file system.
    Io,
}

/// A directory of the user's, the log's alone, flat, its files named by the
/// log. What each call promises is POSIX's: an `append` or a `truncate` is
/// on the medium once `sync` of that file returns, and a name made, renamed
/// or removed once `sync_dir` does. A file system that makes everything
/// durable at either sync -- nos's, whose `File::sync` flushes the whole of
/// it -- keeps both promises at once.
pub trait Files {
    /// The names in the directory, in any order, after what `out` holds.
    fn list(&mut self, out: &mut Vec<String>) -> Result<(), FileError>;
    fn len(&mut self, name: &str) -> Result<u64, FileError>;
    /// Up to `buf.len()` bytes from `offset`: how many -- fewer only at the
    /// file's end.
    fn read_at(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FileError>;
    /// `data` after the file's end; the file made, empty, where there is none.
    fn append(&mut self, name: &str, data: &[u8]) -> Result<(), FileError>;
    /// The file cut to `len` bytes.
    fn truncate(&mut self, name: &str, len: u64) -> Result<(), FileError>;
    /// Everything written to the file, its length too, on the medium.
    fn sync(&mut self, name: &str) -> Result<(), FileError>;
    /// The directory's names -- made, renamed, removed -- on the medium.
    fn sync_dir(&mut self) -> Result<(), FileError>;
    /// `from` named `to`, at once. Where `to` is there already, refused or
    /// replaced -- nos's file system refuses it -- and the log never asks
    /// for that: a snapshot's file named and never marked is removed by
    /// the next open, before any snapshot of its index is written again.
    fn rename(&mut self, from: &str, to: &str) -> Result<(), FileError>;
    fn remove(&mut self, name: &str) -> Result<(), FileError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogError {
    /// A call of the files failed. The log takes no more: what it wrote may
    /// be half there, and only opening it again says what is.
    Io(FileError),
    /// The files say what a log of this kind never would -- a record in the
    /// middle that does not check, a segment missing, a snapshot the log
    /// names and does not have: refused, and nothing changed. What it is.
    Corrupt(&'static str),
    /// A log is there already: `bootstrap` is for a directory with none.
    Exists,
    /// Not what the log takes: entries that do not follow on, a snapshot no
    /// newer than the one there, a commit past the log's end.
    Refused(StorageError),
    /// An entry larger than the wire carries.
    TooLarge,
    /// As many segments as the log keeps ([`MAX_SEGMENTS`]): entries are
    /// refused -- before any of them is written, the log going on, its hard
    /// state and a snapshot taken -- until a snapshot compacts it, opened
    /// again or not. An append that fills the last segment there stops the
    /// log as a failure does, its records perhaps half written.
    Full,
    /// An earlier call failed: the log is to be opened again.
    Stopped,
}

/// How the log cuts its segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogConfig {
    /// A segment is closed once it is this long, and the next begun -- no
    /// shorter than `MIN_SEGMENT_BYTES`.
    pub segment_bytes: u64,
}

pub const DEFAULT_SEGMENT_BYTES: u64 = 8 << 20;
pub const MIN_SEGMENT_BYTES: u64 = 4096;

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig { segment_bytes: DEFAULT_SEGMENT_BYTES }
    }
}

const WAL_MAGIC: [u8; 8] = *b"RAFTWAL\0";
const SNAP_MAGIC: [u8; 8] = *b"RAFTSNAP";
/// A segment's format: its records' CRCs over their length and payload
/// alone (`WAL_PLAIN`), or over the segment's seq and the record's offset
/// in it too (`WAL_PLACED`) -- so that a record whole in a tail that was
/// never this one's, a removed segment's blocks a file system handed back
/// unwritten, does not check there. Segments are begun in the second; one
/// of the first is read, and finished, as it is.
const WAL_PLAIN: u32 = 1;
const WAL_PLACED: u32 = 2;
/// A snapshot file's format.
const SNAP_FORMAT: u32 = 1;
/// A segment's header: its magic, the format, its seq, a CRC32C of those.
const WAL_HEADER: usize = 8 + 4 + 8 + 4;
/// A record's: the payload's length and its CRC32C.
const RECORD_HEADER: usize = 4 + 4;
/// A hard state record's payload: its kind and three words.
const HARD_STATE_PAYLOAD: usize = 1 + 24;
/// What `roll` writes and syncs before anything else goes into a segment:
/// its header and the hard state as it stood. A last segment longer than
/// this was synced this far: a head of it that does not check went bad on
/// the medium -- which no torn write does -- and the log refuses it.
const HEAD: u64 = (WAL_HEADER + RECORD_HEADER + HARD_STATE_PAYLOAD) as u64;
const KIND_ENTRY: u8 = 1;
const KIND_HARD_STATE: u8 = 2;
const KIND_MARKER: u8 = 3;
const MARK_TAKEN: u8 = 0;
const MARK_INSTALLED: u8 = 1;
/// A marker's body: the snapshot's index, its term, how it came.
const MARKER_BODY: usize = 8 + 8 + 1;
/// The most a payload is: its kind and the largest entry the wire carries,
/// with its index, term, kind and length.
const MAX_PAYLOAD: usize = 1 + 8 + 8 + 1 + 4 + wire::MAX_ENTRY_BYTES;
/// A snapshot file's header: magic, format, index, term, the data's length,
/// the membership's, and a CRC32C over all of it and the membership after.
const SNAP_HEADER: usize = 8 + 4 + 8 + 8 + 8 + 4 + 4;
/// The most a membership is as `wire::encode_conf_state` writes it.
const MAX_CONF_BYTES: usize = 2 * (2 + 8 * MAX_MEMBERS);
/// What is gathered before it goes to the file: one append for many small
/// records. A record larger goes on its own.
const BUF_MAX: usize = 128 * 1024;
/// The most segments the log takes entries into before they wait on a
/// snapshot: with the two an open and a marker may begin past it, a
/// snapshot's file and a half-made one, fewer names than a directory of
/// nos's lists at once (`kcore::fs::list`, 4096) -- past that its listing
/// fails, and the log could not be opened again.
pub const MAX_SEGMENTS: usize = 4000;

fn wal_name(seq: u64) -> String {
    format!("wal.{:016x}", seq)
}

fn snap_name(index: u64) -> String {
    format!("snap.{:016x}", index)
}

fn tmp_name(index: u64) -> String {
    format!("snap.{:016x}.tmp", index)
}

/// The number in a name of the log's own: `prefix`, sixteen hex digits,
/// `suffix`, and nothing else.
fn parse_name(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let hex = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// A snapshot's piece's CRC: over its length's bytes, then the piece.
fn piece_crc(piece: &[u8]) -> u32 {
    crc32c::update(crc32c::checksum(&(piece.len() as u32).to_le_bytes()), piece)
}

/// A record's CRC: over its length's bytes, then its payload -- after, in
/// a segment of `WAL_PLACED`, its segment's seq and its offset in it.
fn record_crc(format: u32, seq: u64, at: u64, payload: &[u8]) -> u32 {
    let len = (payload.len() as u32).to_le_bytes();
    if format == WAL_PLACED {
        let mut place = [0u8; 16];
        place[..8].copy_from_slice(&seq.to_le_bytes());
        place[8..].copy_from_slice(&at.to_le_bytes());
        crc32c::update(crc32c::update(crc32c::checksum(&place), &len), payload)
    } else {
        crc32c::update(crc32c::checksum(&len), payload)
    }
}

fn wal_header(seq: u64, format: u32) -> [u8; WAL_HEADER] {
    let mut h = [0u8; WAL_HEADER];
    h[..8].copy_from_slice(&WAL_MAGIC);
    h[8..12].copy_from_slice(&format.to_le_bytes());
    h[12..20].copy_from_slice(&seq.to_le_bytes());
    let crc = crc32c::checksum(&h[..20]);
    h[20..].copy_from_slice(&crc.to_le_bytes());
    h
}

/// The format of the segment whose header `h` is, where it is segment
/// `seq`'s and checks.
fn wal_format(h: &[u8; WAL_HEADER], seq: u64) -> Option<u32> {
    let format = le32(&h[8..12]);
    (matches!(format, WAL_PLAIN | WAL_PLACED) && *h == wal_header(seq, format)).then_some(format)
}

/// A segment as the log keeps it.
#[derive(Clone, Copy, Debug)]
struct Segment {
    seq: u64,
    /// Its length: where its next record goes, for the last.
    len: u64,
    /// The highest index any entry of it has, 0 for none -- whether it may
    /// go once a snapshot is taken.
    max_index: u64,
    /// Its records' CRCs' (`WAL_PLAIN`, `WAL_PLACED`).
    format: u32,
}

/// A snapshot's marker, as the first pass finds it.
#[derive(Clone, Copy, Debug)]
struct Marker {
    seq: u64,
    at: u64,
    index: u64,
    term: u64,
}

/// What the first pass finds.
#[derive(Default)]
struct Found {
    hard: Option<HardState>,
    marker: Option<Marker>,
    /// The newest installed snapshot's marker: nothing before it is the
    /// log's, whatever snapshot was taken since.
    reset: Option<Marker>,
    /// The last entry's index so far, for the next to follow on from.
    last: Option<u64>,
}

/// How a segment reads.
enum Scan {
    Whole { max_index: u64, format: u32 },
    /// Its tail does not check from `at` on.
    Cut { at: u64, max_index: u64, format: u32 },
    /// Its head never made it whole: the crash came as it was begun.
    Headless,
}

/// A record read: its payload, or nothing at all that checks.
type Read = Option<Vec<u8>>;

pub struct DiskLog<F: Files> {
    files: F,
    config: LogConfig,
    /// What the member reads: the snapshot and the log after it.
    mem: MemStorage,
    /// The segments, oldest first, their seqs one after another; the last
    /// is the one written to.
    segments: Vec<Segment>,
    /// The segment of the newest snapshot's marker, 0 for no snapshot.
    marker_seq: u64,
    /// The segment of the newest installed snapshot's marker, 0 for none:
    /// every segment before it is dead, whatever the newest marker is.
    reset_seq: u64,
    /// The hard state as last written.
    hard: HardState,
    /// Records gathered for the last segment, not yet appended.
    buf: Vec<u8>,
    /// Records gathered or appended since the last sync: a sync with none
    /// is no call of the files' -- on nos a flush of the device.
    unsynced: bool,
    /// No segment there before this opened it: what `bootstrap` takes.
    fresh: bool,
    stopped: bool,
}

impl<F: Files> DiskLog<F> {
    /// The log in `files` as it was left -- a crash's torn tail cut -- or an
    /// empty one where there is none: a member that is to be sent its group.
    /// Refused (`Corrupt`) with nothing changed where the files say what no
    /// crash leaves.
    pub fn open(files: F, config: LogConfig) -> Result<DiskLog<F>, LogError> {
        let config = LogConfig { segment_bytes: config.segment_bytes.max(MIN_SEGMENT_BYTES) };
        let mut log = DiskLog {
            files,
            config,
            mem: MemStorage::new(),
            segments: Vec::new(),
            marker_seq: 0,
            reset_seq: 0,
            hard: HardState::default(),
            buf: Vec::new(),
            unsynced: false,
            fresh: false,
            stopped: false,
        };
        log.load()?;
        Ok(log)
    }

    /// A new group's first member, in a directory with no log: its log
    /// begins with the changes that made the group
    /// ([`crate::storage::bootstrap_entries`]), committed, and is synced
    /// before this returns.
    pub fn bootstrap(files: F, config: LogConfig, voters: &[NodeId], learners: &[NodeId])
        -> Result<DiskLog<F>, LogError>
    {
        let mut log = DiskLog::open(files, config)?;
        if !log.fresh {
            return Err(LogError::Exists);
        }
        let entries = bootstrap_entries(voters, learners);
        let last = entries.last().map_or(0, |e| e.index);
        log.append(&entries)?;
        log.set_hard_state(HardState { term: BOOTSTRAP_TERM, vote: NONE, commit: last })?;
        log.sync()?;
        Ok(log)
    }

    pub fn hard_state(&self) -> HardState {
        self.mem.hard_state()
    }

    pub fn snapshot_meta(&self) -> &SnapshotMeta {
        self.mem.snapshot_meta()
    }

    pub fn files(&self) -> &F {
        &self.files
    }

    /// The files back, with nothing more written: what was not synced may
    /// or may not be on the medium.
    pub fn into_files(self) -> F {
        self.files
    }

    /// How many segments there are now.
    pub fn segments(&self) -> usize {
        self.segments.len()
    }

    /* ---- opening ---- */

    fn load(&mut self) -> Result<(), LogError> {
        let mut names = Vec::new();
        self.files.list(&mut names).map_err(LogError::Io)?;
        let mut seqs = Vec::new();
        let mut snaps = Vec::new();
        let mut temps = Vec::new();
        for n in names.iter() {
            if let Some(seq) = parse_name(n, "wal.", "") {
                seqs.push(seq);
            } else if let Some(i) = parse_name(n, "snap.", ".tmp") {
                temps.push(i);
            } else if let Some(i) = parse_name(n, "snap.", "") {
                snaps.push(i);
            }
        }
        drop(names);
        seqs.sort_unstable();
        if seqs.windows(2).any(|w| w[1] != w[0].wrapping_add(1)) {
            return Err(LogError::Corrupt("a segment of the log is missing"));
        }
        if seqs.len() > MAX_SEGMENTS + 2 {
            return Err(LogError::Corrupt("more segments than a log keeps"));
        }

        /* The first pass: every record checked, the newest marker and hard
         * state found, the last segment's torn tail -- or a head it never
         * made whole -- noted. Nothing is changed before the log is known
         * to be whole: one refused is left as it was, for whoever looks at
         * it, its snapshot among it */
        let mut found = Found::default();
        let mut cut: Option<(u64, u64)> = None;
        let mut headless: Option<u64> = None;
        let count = seqs.len();
        for (k, &seq) in seqs.iter().enumerate() {
            let last = k + 1 == count;
            let name = wal_name(seq);
            let len = self.io(|f| f.len(&name))?;
            match self.scan(seq, len, last, &mut found)? {
                Scan::Whole { max_index, format } => self.segments.push(Segment { seq, len, max_index, format }),
                Scan::Cut { at, max_index, format } => {
                    cut = Some((seq, at));
                    self.segments.push(Segment { seq, len: at, max_index, format });
                }
                Scan::Headless => headless = Some(seq),
            }
        }
        self.fresh = self.segments.is_empty();
        if self.fresh && !snaps.is_empty() {
            /* A segment is begun before anything else, and the marker's is
             * never removed: a snapshot with no log is a log lost */
            return Err(LogError::Corrupt("a snapshot and no log"));
        }

        /* The snapshot the newest marker names, and no other */
        let base = found.marker;
        if let Some(m) = base {
            let snap = self.read_snapshot(m.index, m.term)?;
            self.mem.apply_snapshot(snap).map_err(|_| LogError::Corrupt("a snapshot at index 0"))?;
            self.marker_seq = m.seq;
        }
        self.reset_seq = found.reset.map_or(0, |m| m.seq);

        /* The second pass: the entries past the snapshot replayed */
        let segments = self.segments.clone();
        for s in segments.iter() {
            self.replay(s, found.reset)?;
        }

        let mut hs = found.hard.unwrap_or_default();
        let floor = base.map_or(0, |m| m.index);
        if hs.commit < floor {
            hs.commit = floor;
        }
        if hs.commit > self.mem.last_index() {
            return Err(LogError::Corrupt("the hard state commits past the log's end"));
        }

        /* The log is whole: what opening changes is made now -- what a
         * snapshot left half made removed, the torn tail cut, a segment
         * begun and never made whole removed, every snapshot the newest
         * marker does not name removed -- and, with everything read, made
         * the medium's before anyone is told of it. A program started over
         * after a failure, not a power cut, sees what was written and never
         * synced, a segment named and never synced into its directory among
         * it, and a member that took that for durable would answer for
         * entries a cut then loses; and a cut that did not last, under
         * records written after it, would leave what was cut to be read
         * again past them */
        for &i in temps.iter() {
            self.io(|f| f.remove(&tmp_name(i)))?;
        }
        if let Some((seq, at)) = cut {
            let name = wal_name(seq);
            self.io(|f| f.truncate(&name, at))?;
        }
        if let Some(seq) = headless {
            let name = wal_name(seq);
            self.io(|f| f.remove(&name))?;
        }
        for &i in snaps.iter() {
            if base.map(|m| m.index) != Some(i) {
                self.io(|f| f.remove(&snap_name(i)))?;
            }
        }
        for s in self.segments.clone().iter() {
            self.io(|f| f.sync(&wal_name(s.seq)))?;
        }
        if let Some(m) = base {
            self.io(|f| f.sync(&snap_name(m.index)))?;
        }
        self.io(|f| f.sync_dir())?;
        self.mem.set_hard_state(hs);
        self.hard = hs;

        /* The last segment goes on, or a new one begins */
        match self.segments.last() {
            Some(s) if s.len < self.config.segment_bytes => {}
            _ => self.roll()?,
        }
        Ok(())
    }

    /// A segment checked from end to end. The last one's tail that does
    /// not check is what a crash tore -- its writes since its last sync --
    /// and is cut; and the last one no longer than its head, its head not
    /// whole, was begun as the crash came. Anything else that does not
    /// check -- in a sealed segment, or in the head of a last one longer
    /// than that head, synced before anything was written after it -- went
    /// bad on the medium, which no crash does: refused.
    fn scan(&mut self, seq: u64, len: u64, last: bool, found: &mut Found) -> Result<Scan, LogError> {
        let name = wal_name(seq);
        if last && len < HEAD {
            /* Shorter than the head every segment is begun with, and every
             * record goes after: begun as a crash came, holding nothing.
             * Removed, and the next begun -- kept, records would go after a
             * head that was never written whole, nor synced (log seed 4336:
             * a header alone kept, written after, and its tail torn read as
             * a head gone bad) */
            return Ok(Scan::Headless);
        }
        let unfinished = last && len <= HEAD;
        let mut h = [0u8; WAL_HEADER];
        let format = if self.read_exact(&name, 0, &mut h, len)? { wal_format(&h, seq) } else { None };
        let Some(format) = format else {
            return if unfinished {
                Ok(Scan::Headless)
            } else {
                Err(LogError::Corrupt("a segment's header does not check"))
            };
        };
        let mut at = WAL_HEADER as u64;
        let mut max_index = 0;
        loop {
            if at == len {
                return Ok(Scan::Whole { max_index, format });
            }
            let Some(p) = self.read_record(seq, format, &name, at, len)? else {
                return if unfinished {
                    Ok(Scan::Headless)
                } else if last && at >= HEAD {
                    Ok(Scan::Cut { at, max_index, format })
                } else if last {
                    Err(LogError::Corrupt("the head of the last segment, synced, does not check"))
                } else {
                    Err(LogError::Corrupt("a record of a sealed segment does not check"))
                };
            };
            match p[0] {
                KIND_ENTRY => {
                    let e = wire::decode_entry(&p[1..])
                        .map_err(|_| LogError::Corrupt("an entry record that is no entry"))?;
                    if let Some(prev) = found.last {
                        if e.index > prev.saturating_add(1) {
                            return Err(LogError::Corrupt("an entry past the log's end"));
                        }
                    }
                    if e.index == 0 {
                        return Err(LogError::Corrupt("an entry at index 0"));
                    }
                    /* Past any a group reaches, as a message's is refused:
                     * nothing done with it then overflows */
                    if e.index >= MAX_TERM_OR_INDEX || e.term >= MAX_TERM_OR_INDEX {
                        return Err(LogError::Corrupt("an entry past any index or term a group reaches"));
                    }
                    found.last = Some(e.index);
                    max_index = max_index.max(e.index);
                }
                KIND_HARD_STATE => {
                    let hs = wire::decode_hard_state(&p[1..])
                        .map_err(|_| LogError::Corrupt("a hard state record that is no hard state"))?;
                    if hs.term >= MAX_TERM_OR_INDEX || hs.commit >= MAX_TERM_OR_INDEX {
                        return Err(LogError::Corrupt("a hard state past any term or index a group reaches"));
                    }
                    found.hard = Some(hs);
                }
                KIND_MARKER => {
                    let b = &p[1..];
                    if b.len() != MARKER_BODY || !matches!(b[16], MARK_TAKEN | MARK_INSTALLED) {
                        return Err(LogError::Corrupt("a marker record that is no marker"));
                    }
                    let (index, term) = (le64(&b[0..8]), le64(&b[8..16]));
                    if index == 0 {
                        return Err(LogError::Corrupt("a marker at index 0"));
                    }
                    if index >= MAX_TERM_OR_INDEX || term >= MAX_TERM_OR_INDEX {
                        return Err(LogError::Corrupt("a marker past any index or term a group reaches"));
                    }
                    let m = Marker { seq, at, index, term };
                    if b[16] == MARK_INSTALLED {
                        /* The log after it starts after the snapshot */
                        found.last = Some(index);
                        found.reset = Some(m);
                    }
                    found.marker = Some(m);
                }
                _ => return Err(LogError::Corrupt("a record of no kind")),
            }
            at += (RECORD_HEADER + p.len()) as u64;
        }
    }

    /// A segment's entries past the snapshot, into what the member reads --
    /// nothing before the newest installed snapshot's marker, which dropped
    /// them, even where a snapshot was taken since.
    fn replay(&mut self, s: &Segment, reset: Option<Marker>) -> Result<(), LogError> {
        let name = wal_name(s.seq);
        let mut at = WAL_HEADER as u64;
        while at < s.len {
            let Some(p) = self.read_record(s.seq, s.format, &name, at, s.len)? else {
                return Err(LogError::Corrupt("a record that checked does not any more"));
            };
            let here = at;
            at += (RECORD_HEADER + p.len()) as u64;
            if p[0] != KIND_ENTRY {
                continue;
            }
            if let Some(m) = reset {
                if (s.seq, here) < (m.seq, m.at) {
                    continue;
                }
            }
            let e = wire::decode_entry(&p[1..]).map_err(|_| LogError::Corrupt("an entry record that is no entry"))?;
            if e.index < self.mem.first_index() {
                /* In the snapshot -- but written after whatever of the
                 * log past the snapshot comes before it here, it replaced
                 * that when it was written: an entry of a conflict the
                 * snapshot was taken over. So that goes, as it went */
                self.mem.drop_entries();
                continue;
            }
            self.mem.append(&[e]).map_err(|_| LogError::Corrupt("an entry that does not follow on"))?;
        }
        Ok(())
    }

    /// `buf` filled from `offset` of a file `len` long: false, with nothing
    /// to read, where it does not hold that much.
    fn read_exact(&mut self, name: &str, offset: u64, buf: &mut [u8], len: u64) -> Result<bool, LogError> {
        if len < offset || len - offset < buf.len() as u64 {
            return Ok(false);
        }
        let mut done = 0;
        while done < buf.len() {
            let n = self.io(|f| f.read_at(name, offset + done as u64, &mut buf[done..]))?;
            if n == 0 {
                return Ok(false);
            }
            done += n;
        }
        Ok(true)
    }

    /// The record at `at` of segment `seq`, of `format`, a file `len` long:
    /// its payload, or `None` where its length, its CRC or its room in the
    /// file do not check.
    fn read_record(&mut self, seq: u64, format: u32, name: &str, at: u64, len: u64) -> Result<Read, LogError> {
        let mut h = [0u8; RECORD_HEADER];
        if !self.read_exact(name, at, &mut h, len)? {
            return Ok(None);
        }
        let plen = le32(&h[0..4]) as usize;
        let crc = le32(&h[4..8]);
        if plen == 0 || plen > MAX_PAYLOAD || (plen as u64) > len - at - RECORD_HEADER as u64 {
            return Ok(None);
        }
        let mut p = vec![0u8; plen];
        if !self.read_exact(name, at + RECORD_HEADER as u64, &mut p, len)? || record_crc(format, seq, at, &p) != crc {
            return Ok(None);
        }
        Ok(Some(p))
    }

    /// The snapshot file at `index`, as its marker names it.
    fn read_snapshot(&mut self, index: u64, term: u64) -> Result<Snapshot, LogError> {
        let name = snap_name(index);
        let len = match self.files.len(&name) {
            Ok(len) => len,
            Err(FileError::NotFound) => return Err(LogError::Corrupt("the snapshot the log names is not there")),
            Err(e) => return Err(self.fail(e)),
        };
        let mut h = [0u8; SNAP_HEADER];
        if !self.read_exact(&name, 0, &mut h, len)? || h[..8] != SNAP_MAGIC || le32(&h[8..12]) != SNAP_FORMAT {
            return Err(LogError::Corrupt("a snapshot's header does not check"));
        }
        let (hindex, hterm, data_len) = (le64(&h[12..20]), le64(&h[20..28]), le64(&h[28..36]));
        let conf_len = le32(&h[36..40]) as usize;
        if hindex != index || hterm != term || data_len > MAX_SNAPSHOT_BYTES || conf_len > MAX_CONF_BYTES {
            return Err(LogError::Corrupt("a snapshot that is not the one its marker names"));
        }
        let mut conf_bytes = vec![0u8; conf_len];
        if !self.read_exact(&name, SNAP_HEADER as u64, &mut conf_bytes, len)?
            || crc32c::update(crc32c::checksum(&h[..40]), &conf_bytes) != le32(&h[40..44])
        {
            return Err(LogError::Corrupt("a snapshot's header does not check"));
        }
        let conf = wire::decode_conf_state(&conf_bytes).map_err(|_| LogError::Corrupt("a snapshot's membership"))?;
        let mut at = (SNAP_HEADER + conf_len) as u64;
        let mut left = data_len;
        let mut b = SnapshotBuilder::new();
        while left > 0 {
            let mut ph = [0u8; RECORD_HEADER];
            if !self.read_exact(&name, at, &mut ph, len)? {
                return Err(LogError::Corrupt("a snapshot cut short"));
            }
            let plen = le32(&ph[0..4]) as usize;
            if plen == 0 || plen > PIECE_MAX || plen as u64 > left {
                return Err(LogError::Corrupt("a snapshot's piece does not check"));
            }
            let mut piece = vec![0u8; plen];
            if !self.read_exact(&name, at + RECORD_HEADER as u64, &mut piece, len)?
                || piece_crc(&piece) != le32(&ph[4..8])
            {
                return Err(LogError::Corrupt("a snapshot's piece does not check"));
            }
            if !b.push(&piece) {
                return Err(LogError::Corrupt("a snapshot past the largest there is"));
            }
            left -= plen as u64;
            at += (RECORD_HEADER + plen) as u64;
        }
        if at != len {
            return Err(LogError::Corrupt("bytes after a snapshot's end"));
        }
        Ok(Snapshot { meta: SnapshotMeta { index, term, conf }, data: b.finish() })
    }

    /* ---- writing ---- */

    /// A call of the files: a failure stops the log.
    fn io<T>(&mut self, call: impl FnOnce(&mut F) -> Result<T, FileError>) -> Result<T, LogError> {
        call(&mut self.files).map_err(|e| self.fail(e))
    }

    fn fail(&mut self, e: FileError) -> LogError {
        self.stopped = true;
        LogError::Io(e)
    }

    fn check(&self) -> Result<(), LogError> {
        if self.stopped { Err(LogError::Stopped) } else { Ok(()) }
    }

    fn current_name(&self) -> Option<String> {
        self.segments.last().map(|s| wal_name(s.seq))
    }

    /// What is gathered, appended to the last segment.
    fn flush(&mut self) -> Result<(), LogError> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let Some(name) = self.current_name() else { return Err(LogError::Stopped) };
        let buf = core::mem::take(&mut self.buf);
        let r = self.io(|f| f.append(&name, &buf));
        self.buf = buf;
        self.buf.clear();
        r
    }

    /// The last segment closed, synced, and the next begun: its header and
    /// the hard state as it stands, synced, and its name too.
    fn roll(&mut self) -> Result<(), LogError> {
        self.flush()?;
        if let Some(name) = self.current_name() {
            self.io(|f| f.sync(&name))?;
        }
        let seq = self.segments.last().map_or(1, |s| s.seq + 1);
        let name = wal_name(seq);
        self.segments.push(Segment { seq, len: 0, max_index: 0, format: WAL_PLACED });
        let mut head = Vec::with_capacity(WAL_HEADER + RECORD_HEADER + HARD_STATE_PAYLOAD);
        head.extend_from_slice(&wal_header(seq, WAL_PLACED));
        self.io(|f| f.append(&name, &head))?;
        if let Some(s) = self.segments.last_mut() {
            s.len = WAL_HEADER as u64;
        }
        self.hard_state_record(self.hard)?;
        self.flush()?;
        self.io(|f| f.sync(&name))?;
        self.io(|f| f.sync_dir())?;
        self.unsynced = false;
        Ok(())
    }

    /// A record after the last segment's, gathered -- the next segment begun
    /// first when this one is full.
    fn record(&mut self, payload: &[u8], may_roll: bool) -> Result<(), LogError> {
        let full = self.segments.last().is_none_or(|s| s.len >= self.config.segment_bytes);
        if full && may_roll {
            if self.segments.len() >= MAX_SEGMENTS {
                self.stopped = true;
                return Err(LogError::Full);
            }
            self.roll()?;
        }
        let size = RECORD_HEADER + payload.len();
        if self.buf.len() + size > BUF_MAX {
            self.flush()?;
        }
        /* Its CRC over its place too, where its segment's format has it */
        let Some(&Segment { seq, len: at, format, .. }) = self.segments.last() else { return Err(LogError::Stopped) };
        let mut h = [0u8; RECORD_HEADER];
        h[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        h[4..].copy_from_slice(&record_crc(format, seq, at, payload).to_le_bytes());
        self.unsynced = true;
        if size > BUF_MAX {
            /* Larger than what is gathered: on its own, in two appends */
            let Some(name) = self.current_name() else { return Err(LogError::Stopped) };
            self.io(|f| f.append(&name, &h))?;
            self.io(|f| f.append(&name, payload))?;
        } else {
            if self.buf.capacity() < BUF_MAX {
                self.buf.reserve_exact(BUF_MAX - self.buf.len());
            }
            self.buf.extend_from_slice(&h);
            self.buf.extend_from_slice(payload);
        }
        if let Some(s) = self.segments.last_mut() {
            s.len += size as u64;
        }
        Ok(())
    }

    fn hard_state_record(&mut self, hs: HardState) -> Result<(), LogError> {
        let mut p = Vec::with_capacity(1 + 24);
        p.push(KIND_HARD_STATE);
        wire::encode_hard_state(&hs, &mut p);
        self.record(&p, false)
    }

    /// Entries as a `Ready` hands them: in order, the first at most the
    /// last there is and one; what they replace is dropped, and what comes
    /// before the snapshot skipped. Written, not synced: `sync` makes them
    /// durable.
    pub fn append(&mut self, entries: &[Entry]) -> Result<(), LogError> {
        self.check()?;
        let Some(first) = entries.first() else { return Ok(()) };
        if entries.windows(2).any(|w| w[1].index != w[0].index + 1) || first.index > self.mem.last_index() + 1 {
            return Err(LogError::Refused(StorageError::Corrupt));
        }
        if entries.iter().any(|e| e.data.len() > wire::MAX_ENTRY_BYTES) {
            return Err(LogError::TooLarge);
        }
        /* As many segments as the log keeps: no more entries until a
         * snapshot compacts it -- refused before any is written, so the log
         * goes on, and holds the same line opened again (a check made only
         * as a segment fills let each open's new segment take entries, and
         * the fourth open found more segments than a log keeps) */
        if self.segments.len() >= MAX_SEGMENTS {
            return Err(LogError::Full);
        }
        let from = self.mem.first_index();
        for e in entries.iter().filter(|e| e.index >= from) {
            let mut p = Vec::with_capacity(1 + 8 + 8 + 1 + 4 + e.data.len());
            p.push(KIND_ENTRY);
            wire::encode_entry(e, &mut p);
            self.record(&p, true)?;
            if let Some(s) = self.segments.last_mut() {
                s.max_index = s.max_index.max(e.index);
            }
        }
        self.mem.append(entries).map_err(LogError::Refused)
    }

    /// The hard state, written where it changed. Not synced: `sync` makes
    /// it durable, with the entries before it.
    pub fn set_hard_state(&mut self, hs: HardState) -> Result<(), LogError> {
        self.check()?;
        if hs.commit > self.mem.last_index() {
            return Err(LogError::Refused(StorageError::Unavailable));
        }
        if hs != self.hard {
            let mut p = Vec::with_capacity(1 + 24);
            p.push(KIND_HARD_STATE);
            wire::encode_hard_state(&hs, &mut p);
            self.record(&p, true)?;
            self.hard = hs;
        }
        self.mem.set_hard_state(hs);
        Ok(())
    }

    /// Everything written so far on the medium -- with nothing written since
    /// the last sync, no call of the files at all: on nos a file's sync is a
    /// flush of the whole device.
    pub fn sync(&mut self) -> Result<(), LogError> {
        self.check()?;
        if !self.unsynced {
            return Ok(());
        }
        self.flush()?;
        if let Some(name) = self.current_name() {
            self.io(|f| f.sync(&name))?;
        }
        self.unsynced = false;
        Ok(())
    }

    /// A snapshot a leader sent, made the whole of the log -- what was before
    /// it is gone, and so is everything after. Durable when this returns.
    /// Refused below what is committed, as a node never restores one there:
    /// the log would commit past its own end, and refuse to open.
    pub fn apply_snapshot(&mut self, snap: Snapshot) -> Result<(), LogError> {
        self.check()?;
        if snap.meta.index <= self.mem.snapshot_meta().index || snap.meta.index < self.mem.hard_state().commit {
            return Err(LogError::Refused(StorageError::SnapshotOutOfDate));
        }
        let (index, term) = (snap.meta.index, snap.meta.term);
        self.write_snapshot(&snap)?;
        self.mark(MARK_INSTALLED, index, term)?;
        self.reset_seq = self.marker_seq;
        self.mem.apply_snapshot(snap).map_err(LogError::Refused)?;
        self.compact(index)
    }

    /// The state as of entry `index` -- `conf` its membership then, `data`
    /// the state machine's bytes -- made the newest snapshot, and the
    /// entries up to it dropped. Durable when this returns.
    pub fn create_snapshot(&mut self, index: u64, conf: ConfState, data: SnapshotData) -> Result<(), LogError> {
        self.check()?;
        if index <= self.mem.snapshot_meta().index {
            return Err(LogError::Refused(StorageError::SnapshotOutOfDate));
        }
        if index > self.mem.last_index() {
            return Err(LogError::Refused(StorageError::Unavailable));
        }
        let term = self.mem.term(index).map_err(LogError::Refused)?;
        let snap = Snapshot { meta: SnapshotMeta { index, term, conf: conf.clone() }, data: data.clone() };
        self.write_snapshot(&snap)?;
        self.mark(MARK_TAKEN, index, term)?;
        self.mem.create_snapshot(index, conf, data).map_err(LogError::Refused)?;
        self.compact(index)
    }

    /// The snapshot's file, all of it on the medium under its own name
    /// before anything names it.
    fn write_snapshot(&mut self, snap: &Snapshot) -> Result<(), LogError> {
        let (tmp, name) = (tmp_name(snap.meta.index), snap_name(snap.meta.index));
        let mut conf = Vec::with_capacity(MAX_CONF_BYTES);
        wire::encode_conf_state(&snap.meta.conf, &mut conf);
        let mut head = Vec::with_capacity(SNAP_HEADER + conf.len());
        head.extend_from_slice(&SNAP_MAGIC);
        head.extend_from_slice(&SNAP_FORMAT.to_le_bytes());
        head.extend_from_slice(&snap.meta.index.to_le_bytes());
        head.extend_from_slice(&snap.meta.term.to_le_bytes());
        head.extend_from_slice(&snap.data.len().to_le_bytes());
        head.extend_from_slice(&(conf.len() as u32).to_le_bytes());
        let crc = crc32c::update(crc32c::checksum(&head), &conf);
        head.extend_from_slice(&crc.to_le_bytes());
        head.extend_from_slice(&conf);
        self.io(|f| f.append(&tmp, &head))?;
        for piece in snap.data.pieces() {
            let mut h = [0u8; RECORD_HEADER];
            h[..4].copy_from_slice(&(piece.len() as u32).to_le_bytes());
            h[4..].copy_from_slice(&piece_crc(piece).to_le_bytes());
            self.io(|f| f.append(&tmp, &h))?;
            self.io(|f| f.append(&tmp, piece))?;
        }
        self.io(|f| f.sync(&tmp))?;
        self.io(|f| f.rename(&tmp, &name))?;
        self.io(|f| f.sync_dir())
    }

    /// The snapshot named in the log, in a segment of its own begun for it
    /// -- so that every segment before may go -- and synced.
    fn mark(&mut self, how: u8, index: u64, term: u64) -> Result<(), LogError> {
        self.roll()?;
        let mut p = Vec::with_capacity(1 + MARKER_BODY);
        p.push(KIND_MARKER);
        p.extend_from_slice(&index.to_le_bytes());
        p.extend_from_slice(&term.to_le_bytes());
        p.push(how);
        self.record(&p, false)?;
        self.sync()?;
        self.marker_seq = self.segments.last().map_or(0, |s| s.seq);
        Ok(())
    }

    /// What the snapshot at `index` made needless, removed: the snapshot
    /// before it, and the segments before its marker's, oldest first, while
    /// none would take a live entry with it -- every one before the newest
    /// installed snapshot's marker, which dropped the whole log. One at a
    /// time, its removal synced before the next: what is left is always a
    /// run of segments one after another.
    fn compact(&mut self, index: u64) -> Result<(), LogError> {
        let mut names = Vec::new();
        self.io(|f| f.list(&mut names))?;
        let mut removed = false;
        for n in names.iter() {
            if let Some(i) = parse_name(n, "snap.", "") {
                if i < index {
                    self.io(|f| f.remove(n))?;
                    removed = true;
                }
            }
        }
        drop(names);
        if removed {
            self.io(|f| f.sync_dir())?;
        }
        while let Some(&s) = self.segments.first() {
            if s.seq >= self.marker_seq || (s.seq >= self.reset_seq && s.max_index > index) {
                break;
            }
            self.io(|f| f.remove(&wal_name(s.seq)))?;
            self.io(|f| f.sync_dir())?;
            self.segments.remove(0);
        }
        Ok(())
    }
}

impl<F: Files> Storage for DiskLog<F> {
    fn initial_state(&self) -> Result<RaftState, StorageError> {
        self.mem.initial_state()
    }

    fn entries(&self, lo: u64, hi: u64, max_bytes: u64, out: &mut Vec<Entry>) -> Result<(), StorageError> {
        self.mem.entries(lo, hi, max_bytes, out)
    }

    fn term(&self, index: u64) -> Result<u64, StorageError> {
        self.mem.term(index)
    }

    fn first_index(&self) -> u64 {
        self.mem.first_index()
    }

    fn last_index(&self) -> u64 {
        self.mem.last_index()
    }

    fn snapshot(&self) -> Result<Snapshot, StorageError> {
        self.mem.snapshot()
    }
}
