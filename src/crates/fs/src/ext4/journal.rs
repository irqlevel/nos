//! The journal, JBD2's, in its inode. A mount that only reads leaves it as
//! it is: when it holds transactions nobody replayed it is read through
//! (`overlay`), so that a read sees what a replay would leave and nothing is
//! written. A mount that writes replays it first, and then writes through
//! it: every transaction's blocks into the log before anything is written
//! home, the commit block last and forced to the medium, and the blocks
//! written home only by a checkpoint, once each is committed. Its rules,
//! each against a way of losing the filesystem to a crash:
//!
//! - The filesystem says it needs recovery, forced to the medium, before
//!   the log is written at all (the mount sets it); it says so no longer
//!   only once a checkpoint has put everything home and the log is marked
//!   empty (`close`).
//! - The first commit to an empty log writes the journal's superblock with
//!   the log's start first, which the commit's own flush orders.
//! - The features a write uses -- 64bit tags where the filesystem is
//!   64bit, v3 checksums, revokes -- are set, forced, before the first
//!   commit; a copy whose first word is the journal's magic is escaped; a
//!   commit's time never goes back.
//! - Nothing more is written once a write of the journal, of a block's
//!   home or of a superblock fails, or a flush does (`abort`): the journal,
//!   and the filesystem's state, are left for the next mount's replay -- or
//!   e2fsck's -- to settle. A file's data that does not write fails its
//!   call alone (`data.rs`).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use extfmt::features::incompat;
use extfmt::inode::{Inode, S_IFREG};
use extfmt::jbd2::{self, set_be32, set_be64, JournalSuper};
use extfmt::recovery::{self, Plan};
use extfmt::sb::Super;
use extfmt::Map;
use kcore::time::wall_clock_secs;
use kcore::trace;

use super::cache::Cache;
use super::io::Io;
use super::source::{self, Source};
use super::txn::Txn;

/// Where the newest copy of each home block is, in a journal nobody
/// replayed: its device block, and whether its first word was escaped.
pub type Overlay = BTreeMap<u64, (u64, bool)>;

/// The most blocks one transaction changes: what the journal takes at once
/// is a quarter of it, as Linux has it, and never more than this.
const MAX_TXN: usize = 512;

/// The committed blocks a checkpoint is due at: what memory holds of them.
const PIN_MAX: usize = 1024;

/// The journal's blocks on the device: every block of its inode's size,
/// none a hole or unwritten, each inside the filesystem.
pub(super) fn blocks(src: &mut dyn Source, sb: &Super) -> Result<Vec<u64>, String> {
    let ino = sb.journal_inum();
    let raw = source::inode(src, ino).ok_or_else(|| format!("the journal's inode {} does not read", ino))?;
    let node = Inode::new(&raw).ok_or("a short journal inode")?;
    if node.file_type() != S_IFREG {
        return Err(format!("the journal's inode {} is not a regular file", ino));
    }
    let bs = src.meta().bs as u64;
    let n = node.size(true).div_ceil(bs);
    if n > u64::from(u32::MAX) || n < 2 {
        return Err(format!("a journal of {} blocks", n));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(n as usize).map_err(|_| String::from("no memory for the journal's map"))?;
    let mut l = 0u64;
    while l < n {
        match source::map(src, ino, &raw, l) {
            Some(Map::Blocks { block, run }) => {
                for k in 0..run.min(n - l) {
                    out.push(block + k);
                }
                l += run;
            }
            Some(Map::Hole { .. }) => return Err(format!("the journal has a hole, or an unwritten run, at {}", l)),
            None => return Err(String::from("the journal's map does not read")),
        }
    }
    Ok(out)
}

/// The journal's superblock, and its block whole, off the device.
fn read_jsb(io: &mut Io, map: &[u64]) -> Result<(JournalSuper, Vec<u8>), String> {
    let mut buf = vec![0u8; io.bs];
    if !io.read(map[0], &mut buf) {
        return Err(String::from("the journal's superblock does not read"));
    }
    let jsb = JournalSuper::parse(&buf)?;
    if jsb.maxlen() as usize > map.len() {
        return Err(format!("a journal of {} blocks in an inode of {}", jsb.maxlen(), map.len()));
    }
    Ok((jsb, buf))
}

/// The replay's plan of a journal that has a log, its copies held inside
/// the filesystem and off the journal itself.
fn plan_of(io: &mut Io, map: &[u64], jsb: &JournalSuper) -> Result<Plan, String> {
    let bs = io.bs;
    let plan = {
        let mut read = |jb: u32, out: &mut [u8]| match map.get(jb as usize) {
            Some(&dev) if io.read(dev, out) => Ok(()),
            _ => Err(format!("journal block {} does not read", jb)),
        };
        recovery::plan(jsb, bs, &mut read)?
    };
    let mut sorted: Vec<u64> = map.to_vec();
    sorted.sort_unstable();
    for w in &plan.writes {
        if w.home >= io.blocks {
            return Err(format!("a journal copy for block {}, past the filesystem", w.home));
        }
        if sorted.binary_search(&w.home).is_ok() {
            return Err(format!("a journal copy for block {}, the journal's own", w.home));
        }
        if w.log as usize >= map.len() {
            return Err(format!("a journal copy at journal block {}, past the journal", w.log));
        }
    }
    Ok(plan)
}

/// The overlay of the journal whose blocks are `map`, of the filesystem
/// `sb` describes, for a mount that only reads: empty for a journal with
/// nothing to replay; why not for one that cannot be read through -- a
/// mount then refused, as a replay around it would show the filesystem
/// neither as it was nor as it was to be.
pub(super) fn overlay(io: &mut Io, map: &[u64], sb: &Super) -> Result<Overlay, String> {
    let (jsb, _) = read_jsb(io, map)?;
    let needs = sb.incompat() & incompat::RECOVER != 0;
    if jsb.start() != 0 && !needs {
        /* Linux throws such a log away (`jbd2_journal_wipe`): what is on
         * the disk is the filesystem. */
        trace!(0, "ext4: the journal holds a log the filesystem does not say needs replaying: read as it is");
        return Ok(Overlay::new());
    }
    if jsb.errno() != 0 {
        trace!(0, "ext4: the journal recorded error {}", jsb.errno() as i32);
    }
    if jsb.start() == 0 {
        recovery::check_features(&jsb)?;
        return Ok(Overlay::new());
    }
    let plan = plan_of(io, map, &jsb)?;
    if let Some(t) = plan.failed_commit {
        trace!(0, "ext4: journal transaction {} was corrupt: read up to it", t);
    }
    let mut out = Overlay::new();
    for w in &plan.writes {
        out.insert(w.home, (map[w.log as usize], w.escaped));
    }
    trace!(0, "ext4: {} transactions in the journal not replayed: read through them, {} blocks", plan.transactions,
        out.len());
    Ok(out)
}

/// What a mount for writing makes of its journal.
pub(super) enum Opened {
    /// Ready to write through; whether a replay wrote anything home.
    Ready(Journal, bool),
    /// To be mounted read-only, for the reason given.
    ReadOnly(String),
}

pub(super) struct Journal {
    /// Journal block to device block.
    pub map: Vec<u64>,
    jsb: JournalSuper,
    /// The superblock's block as the device has it: what lies past the
    /// superblock in it is kept.
    jsb_block: Vec<u8>,
    first: u32,
    last: u32,
    /// Where the next transaction's first block goes.
    head: u32,
    /// The blocks of the live log, from the superblock's start to the head.
    used: u32,
    next_seq: u32,
    /// Whether the superblock on the device gives the log a start.
    started: bool,
    /// Per home block, the transaction of the live log that holds its
    /// newest copy: what a block freed is revoked for.
    pub in_log: BTreeMap<u64, u32>,
    pub aborted: bool,
    seed: u32,
    bit64: bool,
    uuid: [u8; 16],
    last_sec: u64,
    pub max_txn: usize,
}

/// The tags a descriptor block holds: the first followed by the journal's
/// UUID, the rest marked as the same, the block's tail after them.
fn tags_per_block(bs: usize) -> usize {
    let room = bs - jbd2::HEADER - jbd2::BLOCK_TAIL;
    1 + (room - 16 - jbd2::TAG_UUID) / 16
}

fn records_per_block(bs: usize, bit64: bool) -> usize {
    (bs - jbd2::REVOKE_HEADER - jbd2::BLOCK_TAIL) / if bit64 { 8 } else { 4 }
}

impl Journal {
    /// The journal of a filesystem mounted for writing, `sb` its superblock
    /// as read: checked, replayed if it holds a log -- its homes written
    /// and the log marked empty -- and its features set for writing,
    /// forced to the medium, before anything is written through it. An
    /// error refuses the mount.
    pub fn open(io: &mut Io, map: Vec<u64>, sb: &Super) -> Result<Opened, String> {
        let (mut jsb, jsb_block) = read_jsb(io, &map)?;
        let needs = sb.incompat() & incompat::RECOVER != 0;
        if jsb.version() != jbd2::SUPERBLOCK_V2 {
            return Ok(Opened::ReadOnly(String::from("a journal of the first version, which has no features")));
        }
        recovery::check_features(&jsb)?;
        if jsb.errno() != 0 {
            return Ok(Opened::ReadOnly(format!("the journal recorded error {}", jsb.errno() as i32)));
        }
        if jsb.start() != 0 && !needs {
            return Ok(Opened::ReadOnly(String::from(
                "the journal holds a log the filesystem does not say needs replaying (Linux would wipe it)")));
        }
        if jsb.incompat() & jbd2::INCOMPAT_FAST_COMMIT != 0 && jsb.start() != 0 {
            return Err(String::from("a journal of fast commits, not empty"));
        }
        let mut next_seq = jsb.sequence();
        let mut replayed = false;
        if jsb.start() != 0 {
            let plan = plan_of(io, &map, &jsb)?;
            if let Some(t) = plan.failed_commit {
                return Ok(Opened::ReadOnly(format!("journal transaction {} is corrupt (e2fsck should look)", t)));
            }
            replay(io, &map, &plan)?;
            trace!(0, "ext4: the journal replayed: {} transactions, {} blocks", plan.transactions,
                plan.writes.len());
            next_seq = plan.next_sequence;
            replayed = true;
        }
        let mut features = jsb.incompat() | jbd2::INCOMPAT_REVOKE | jbd2::INCOMPAT_CSUM_V3;
        if sb.is_64bit() {
            features |= jbd2::INCOMPAT_64BIT;
        }
        let changed = features != jsb.incompat() || jsb.checksum_type() != jbd2::CHECKSUM_CRC32C;
        jsb.set_incompat(features);
        jsb.set_checksum_type(jbd2::CHECKSUM_CRC32C);
        jsb.set_start(0);
        jsb.set_sequence(next_seq);
        jsb.set_checksum();
        let (first, last) = (jsb.first(), jsb.last());
        let ring = (last - first) as usize;
        let mut j = Journal {
            map,
            seed: jsb.csum_seed(),
            /* What a reader parses a revoke record by: the journal's
             * feature, which may be set where the filesystem is not 64bit. */
            bit64: features & jbd2::INCOMPAT_64BIT != 0,
            uuid: jsb.uuid(),
            jsb,
            jsb_block,
            first,
            last,
            head: first,
            used: 0,
            next_seq,
            started: false,
            in_log: BTreeMap::new(),
            aborted: false,
            last_sec: 0,
            max_txn: (ring / 4).clamp(1, MAX_TXN),
        };
        if (changed || replayed) && !j.write_jsb(io) {
            return Err(String::from("the journal's superblock does not write"));
        }
        Ok(Opened::Ready(j, replayed))
    }

    fn ring(&self) -> u32 {
        self.last - self.first
    }

    fn next(&self, p: u32) -> u32 {
        if p + 1 >= self.last { self.first } else { p + 1 }
    }

    /// The superblock as it is now, onto the device and past its cache.
    fn write_jsb(&mut self, io: &mut Io) -> bool {
        self.jsb.set_checksum();
        let mut blk = self.jsb_block.clone();
        blk[..jbd2::SB_SIZE].copy_from_slice(self.jsb.raw());
        if !io.write(self.map[0], &blk, true) {
            return false;
        }
        self.jsb_block = blk;
        true
    }

    /// Nothing more is to be written: said once.
    pub fn abort(&mut self, why: &str) {
        if !self.aborted {
            trace!(0, "ext4: the journal is aborted: {}; nothing more is written, and the filesystem is left for a \
                       replay", why);
        }
        self.aborted = true;
    }

    /// The blocks a transaction of `copies` blocks and `revokes` revoked
    /// takes in the log, its commit block among them.
    fn blocks_for(&self, bs: usize, copies: usize, revokes: usize) -> usize {
        copies.div_ceil(tags_per_block(bs)) + copies + revokes.div_ceil(records_per_block(bs, self.bit64)) + 1
    }

    /// The log empty -- its start 0, the next transaction's sequence kept --
    /// once a checkpoint has put everything home: what a clean unmount
    /// leaves.
    pub fn close(&mut self, io: &mut Io, cache: &mut Cache) -> bool {
        /* The superblock written once, empty: a crash before it replays
         * only what is home already. */
        if self.aborted || !write_home(io, cache, self, false) {
            return false;
        }
        self.jsb.set_start(0);
        self.jsb.set_sequence(self.next_seq);
        if !self.write_jsb(io) {
            self.abort("the journal's superblock does not write");
            return false;
        }
        self.started = false;
        true
    }

    pub fn info(&self) -> (u32, u32, u32) {
        (self.used, self.ring(), self.next_seq)
    }
}

/// The plan's copies written home, the newest of each, and flushed.
fn replay(io: &mut Io, map: &[u64], plan: &Plan) -> Result<(), String> {
    let mut newest: BTreeMap<u64, (u32, bool)> = BTreeMap::new();
    for w in &plan.writes {
        newest.insert(w.home, (w.log, w.escaped));
    }
    let mut buf = vec![0u8; io.bs];
    for (&home, &(log, escaped)) in &newest {
        if !io.read(map[log as usize], &mut buf) {
            return Err(format!("journal block {} does not read", log));
        }
        if escaped {
            buf[..4].copy_from_slice(&jbd2::MAGIC.to_be_bytes());
        }
        if !io.write(home, &buf, false) {
            return Err(format!("block {} does not write in the replay", home));
        }
    }
    if !io.flush() {
        return Err(String::from("the device does not flush after the replay"));
    }
    Ok(())
}

/// Everything committed written home, and the log's start moved past it:
/// the live log empty. False, and the journal aborted, when a write fails
/// -- what is pinned stays pinned, the only copy the reads have.
pub(super) fn checkpoint(io: &mut Io, cache: &mut Cache, jnl: &mut Journal) -> bool {
    write_home(io, cache, jnl, true)
}

/// `checkpoint`, the log's start moved in the journal's superblock only
/// with `restart`: without, its caller writes the superblock (`close`).
fn write_home(io: &mut Io, cache: &mut Cache, jnl: &mut Journal, restart: bool) -> bool {
    if jnl.aborted {
        return false;
    }
    if jnl.used == 0 && cache.pinned_len() == 0 {
        return true;
    }
    for (&b, data) in cache.pinned() {
        if !io.write(b, data, false) {
            jnl.abort("a checkpoint's write home failed");
            return false;
        }
    }
    if !io.flush() {
        jnl.abort("the device does not flush a checkpoint");
        return false;
    }
    if restart && jnl.started {
        jnl.jsb.set_start(jnl.head);
        jnl.jsb.set_sequence(jnl.next_seq);
        if !jnl.write_jsb(io) {
            jnl.abort("the journal's superblock does not write");
            return false;
        }
    }
    cache.settle();
    jnl.used = 0;
    jnl.in_log.clear();
    true
}

impl Txn<'_> {
    /// Into the journal, all of it, and only then into what the reads see:
    /// sealed, its blocks -- revokes, descriptors, copies -- written into
    /// the log, flushed, and the commit block forced to the medium. False
    /// with nothing changed in memory: refused (too big, sealed wrong), or a
    /// write failed and the journal is aborted.
    pub fn commit(mut self) -> bool {
        if self.jnl.aborted {
            trace!(0, "ext4: the journal is aborted: nothing is committed");
            return false;
        }
        if self.dirty.is_empty() && self.revoke.is_empty() && self.freed.is_empty() {
            return true;
        }
        if !self.seal() {
            return false;
        }
        let bs = self.m.bs;
        let copies: Vec<(u64, Vec<u8>, bool)> = self.dirty.iter().map(|(&b, d)| {
            let mut c = d.data.clone();
            let escaped = jbd2::be32(&c, 0) == jbd2::MAGIC;
            if escaped {
                c[..4].fill(0);
            }
            (b, c, escaped)
        }).collect();
        let revokes: Vec<u64> = self.revoke.iter().copied().collect();
        let n = self.jnl.blocks_for(bs, copies.len(), revokes.len());
        if n >= self.jnl.ring() as usize {
            trace!(0, "ext4: a transaction of {} blocks, more than the journal's {} hold: refused", n, self.jnl.ring());
            return false;
        }
        if n >= (self.jnl.ring() - self.jnl.used) as usize && !checkpoint(self.io, self.cache, self.jnl) {
            return false;
        }
        let seq = self.jnl.next_seq;
        let jnl = &mut *self.jnl;
        let io = &mut *self.io;
        if !jnl.started {
            /* The log begins here: the superblock says so before the
             * commit block can be on the medium. */
            jnl.jsb.set_start(jnl.head);
            jnl.jsb.set_sequence(seq);
            if !jnl.write_jsb(io) {
                jnl.abort("the journal's superblock does not write");
                return false;
            }
            jnl.started = true;
        }
        let mut pos = jnl.head;
        let mut blk = vec![0u8; bs];
        let header = |blk: &mut [u8], kind: u32| {
            blk.fill(0);
            set_be32(blk, 0, jbd2::MAGIC);
            set_be32(blk, 4, kind);
            set_be32(blk, 8, seq);
        };
        let width = if jnl.bit64 { 8 } else { 4 };
        for chunk in revokes.chunks(records_per_block(bs, jnl.bit64)) {
            header(&mut blk, jbd2::REVOKE_BLOCK);
            set_be32(&mut blk, jbd2::R_COUNT, (jbd2::REVOKE_HEADER + chunk.len() * width) as u32);
            for (k, &b) in chunk.iter().enumerate() {
                let at = jbd2::REVOKE_HEADER + k * width;
                if jnl.bit64 { set_be64(&mut blk, at, b) } else { set_be32(&mut blk, at, b as u32) }
            }
            let c = jbd2::block_tail_csum(jnl.seed, &blk).unwrap_or(0);
            set_be32(&mut blk, bs - jbd2::BLOCK_TAIL, c);
            if !io.write(jnl.map[pos as usize], &blk, false) {
                jnl.abort("a revoke block does not write");
                return false;
            }
            pos = jnl.next(pos);
        }
        for chunk in copies.chunks(tags_per_block(bs)) {
            header(&mut blk, jbd2::DESCRIPTOR_BLOCK);
            let mut at = jbd2::HEADER;
            for (k, (home, copy, escaped)) in chunk.iter().enumerate() {
                let mut flags = if *escaped { jbd2::FLAG_ESCAPE } else { 0 };
                if k > 0 {
                    flags |= jbd2::FLAG_SAME_UUID;
                }
                if k + 1 == chunk.len() {
                    flags |= jbd2::FLAG_LAST_TAG;
                }
                set_be32(&mut blk, at, *home as u32);
                set_be32(&mut blk, at + 4, flags);
                set_be32(&mut blk, at + 8, (*home >> 32) as u32);
                set_be32(&mut blk, at + 12, jbd2::copy_csum(jnl.seed, seq, copy));
                at += 16;
                if k == 0 {
                    blk[at..at + jbd2::TAG_UUID].copy_from_slice(&jnl.uuid);
                    at += jbd2::TAG_UUID;
                }
            }
            let c = jbd2::block_tail_csum(jnl.seed, &blk).unwrap_or(0);
            set_be32(&mut blk, bs - jbd2::BLOCK_TAIL, c);
            if !io.write(jnl.map[pos as usize], &blk, false) {
                jnl.abort("a descriptor block does not write");
                return false;
            }
            pos = jnl.next(pos);
            for (_, copy, _) in chunk {
                if !io.write(jnl.map[pos as usize], copy, false) {
                    jnl.abort("a block's copy does not write to the journal");
                    return false;
                }
                pos = jnl.next(pos);
            }
        }
        if !io.flush() {
            jnl.abort("the device does not flush a transaction");
            return false;
        }
        header(&mut blk, jbd2::COMMIT_BLOCK);
        let sec = wall_clock_secs().max(jnl.last_sec);
        set_be64(&mut blk, jbd2::H_COMMIT_SEC, sec);
        let c = jbd2::commit_csum(jnl.seed, &blk).unwrap_or(0);
        set_be32(&mut blk, jbd2::H_CHKSUM, c);
        if !io.write(jnl.map[pos as usize], &blk, true) {
            jnl.abort("a commit block does not write");
            return false;
        }
        pos = jnl.next(pos);
        jnl.last_sec = sec;
        jnl.head = pos;
        jnl.used += n as u32;
        jnl.next_seq = seq.wrapping_add(1);

        /* Committed: what the reads see now, and what a checkpoint is to
         * write home. */
        let (sb_block, sb_at) = (self.m.sb_block(), self.m.sb_at());
        let (gdt_first, gdt_len) = (self.m.gdt_first(), self.m.gdt_len());
        let dirty = core::mem::take(&mut self.dirty);
        for (b, d) in dirty {
            if b == sb_block {
                if let Ok(s) = Super::parse(&d.data[sb_at..sb_at + extfmt::sb::SIZE]) {
                    self.m.sb = s;
                }
            }
            if b >= gdt_first && b < gdt_first + gdt_len {
                let k = ((b - gdt_first) as usize) * bs;
                self.m.gdt[k..k + bs].copy_from_slice(&d.data);
            }
            self.jnl.in_log.insert(b, seq);
            self.cache.pin(b, d.data);
        }
        for (start, end) in self.freed.iter() {
            self.cache.unpin_range(start, end);
            let logged: Vec<u64> = self.jnl.in_log.range(start..end).map(|(&b, _)| b).collect();
            for b in logged {
                self.jnl.in_log.remove(&b);
            }
        }
        self.m.free_blocks = self.m.free_blocks.saturating_sub(self.allocated) + self.freed.blocks();
        if self.cache.pinned_len() > PIN_MAX || self.jnl.used > self.jnl.ring() / 2 {
            /* Committed whatever comes of it: a checkpoint that fails
             * aborts the journal for the calls after this one. */
            checkpoint(self.io, self.cache, self.jnl);
        }
        true
    }
}
