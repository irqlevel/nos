//! A transaction: what one call that changes the filesystem changes of its
//! metadata, kept apart from what is committed until the journal holds all
//! of it -- and dropped, leaving nothing changed, if it never gets there. A
//! block is changed only as one of the kinds below, whose checksum is put
//! on it as the transaction is sealed for the journal, so that none is
//! forgotten; and a transaction holds the device, the cache and the journal
//! for as long as it lives, so that nothing -- a checkpoint above all --
//! can write anything home meanwhile.
//!
//! What it frees stays out of reach until it commits: a block freed is not
//! handed out again by the same transaction, and a block the live log holds
//! a copy of is revoked, so that no replay writes that copy over whatever
//! the block becomes next (`free_run`). What it frees is kept as runs, so
//! that a file of millions of blocks is a handful of them.
//!
//! The last few free blocks are kept for a punch (`RESERVE`): one inside
//! an extent splits it, and on a full disk the leaf the split needs would
//! otherwise be what stops a guest giving its disk's blocks back.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use extfmt::features::ro_compat;
use extfmt::group::{self, Desc, DescMut};
use extfmt::sb::Super;
use extfmt::{dir, extent, inode as xinode, xattr};
use kcore::trace;

use super::cache::Cache;
use super::io::Io;
use super::journal::{Journal, Overlay};
use super::meta::Meta;
use super::source::{read_block, Source};

/// What a changed block is, which says what checksum it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// The superblock's block: a feature set when a file first needs it.
    Super,
    Gdt,
    BlockBitmap(u64),
    InodeBitmap(u64),
    /// Inode table blocks: each inode's checksum is put on it as it is
    /// stored (`put_inode`).
    Inodes,
    /// A directory's block, the directory's checksum seed: a leaf ending in
    /// its tail, or an index block.
    Dir { seed: u32, dx: bool },
    Extent { seed: u32 },
    Xattr,
}

pub(super) struct Dirty {
    pub data: Vec<u8>,
    pub kind: Kind,
}

/// The free blocks only a punch may take: what a split extent's new leaf
/// and the nodes above it could need, the tree as deep as it goes.
pub const RESERVE: u64 = 8;

/// Blocks as runs, each `start..end`, sorted and apart.
#[derive(Default)]
pub(super) struct Runs {
    runs: BTreeMap<u64, u64>,
}

impl Runs {
    pub fn contains(&self, b: u64) -> bool {
        self.runs.range(..=b).next_back().is_some_and(|(_, &end)| b < end)
    }

    /// `start..end` in, run on into the runs it meets.
    pub fn insert(&mut self, mut start: u64, mut end: u64) {
        if let Some((&s, &e)) = self.runs.range(..=start).next_back() {
            if e >= start {
                start = s;
                end = end.max(e);
                self.runs.remove(&s);
            }
        }
        while let Some((&s, &e)) = self.runs.range(start..).next() {
            if s > end {
                break;
            }
            end = end.max(e);
            self.runs.remove(&s);
        }
        self.runs.insert(start, end);
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.runs.iter().map(|(&s, &e)| (s, e))
    }

    /// How many blocks the runs hold.
    pub fn blocks(&self) -> u64 {
        self.runs.iter().map(|(&s, &e)| e - s).sum()
    }
}

pub(super) struct Txn<'a> {
    pub io: &'a mut Io,
    pub m: &'a mut Meta,
    pub cache: &'a mut Cache,
    pub jnl: &'a mut Journal,
    pub dirty: BTreeMap<u64, Dirty>,
    /// Blocks freed: not handed out again before the commit.
    pub freed: Runs,
    /// Blocks freed that the live log holds a copy of.
    pub revoke: BTreeSet<u64>,
    /// Blocks handed out, which the free count loses at the commit.
    pub allocated: u64,
    /// Whether this may take the free blocks kept for a punch.
    pub may_reserve: bool,
    /// The wall clock as the call began: every time it stores.
    pub now: u32,
}

/// The empty overlay a mount that writes reads through: its journal was
/// replayed.
static NO_OVERLAY: Overlay = Overlay::new();

impl<'a> Txn<'a> {
    pub fn new(io: &'a mut Io, m: &'a mut Meta, cache: &'a mut Cache, jnl: &'a mut Journal, now: u32) -> Txn<'a> {
        Txn {
            io, m, cache, jnl, dirty: BTreeMap::new(), freed: Runs::default(), revoke: BTreeSet::new(), allocated: 0,
            may_reserve: false, now,
        }
    }

    /// Room for one more block, or why not.
    fn admit(&mut self, b: u64) -> bool {
        if self.freed.contains(b) {
            trace!(0, "ext4: block {} changed after it was freed in the same transaction: refused", b);
            return false;
        }
        if self.dirty.len() >= self.jnl.max_txn {
            trace!(0, "ext4: a transaction of more than {} blocks, more than the journal takes at once: refused",
                self.jnl.max_txn);
            return false;
        }
        true
    }

    /// Block `b` as this transaction has it, a copy.
    pub fn read(&mut self, b: u64) -> Option<Vec<u8>> {
        if let Some(d) = self.dirty.get(&b) {
            return Some(d.data.clone());
        }
        let mut out = vec![0u8; self.m.bs];
        read_block(self.io, self.cache, &NO_OVERLAY, b, &mut out, true).then_some(out)
    }

    /// Block `b` to change, as `kind`: read first, unless this transaction
    /// has it already.
    pub fn modify(&mut self, b: u64, kind: Kind) -> Option<&mut [u8]> {
        if !self.dirty.contains_key(&b) {
            if !self.admit(b) {
                return None;
            }
            let data = self.read(b)?;
            self.dirty.insert(b, Dirty { data, kind });
        }
        let d = self.dirty.get_mut(&b)?;
        if d.kind != kind {
            trace!(0, "ext4: block {} changed as {:?} and as {:?} in one transaction: refused", b, d.kind, kind);
            return None;
        }
        Some(&mut d.data)
    }

    /// Block `b`, just allocated, to fill as `kind`: zeros, whatever its
    /// home holds.
    pub fn fresh(&mut self, b: u64, kind: Kind) -> Option<&mut [u8]> {
        if self.dirty.contains_key(&b) {
            trace!(0, "ext4: block {} allocated twice in one transaction", b);
            return None;
        }
        if !self.admit(b) {
            return None;
        }
        self.cache.forget(b);
        self.dirty.insert(b, Dirty { data: vec![0u8; self.m.bs], kind });
        self.dirty.get_mut(&b).map(|d| d.data.as_mut_slice())
    }

    /// Group `g`'s descriptor to change.
    pub fn group_mut(&mut self, g: u64) -> Option<DescMut<'_>> {
        if g >= self.m.groups {
            return None;
        }
        let (b, at) = self.m.desc_at(g);
        let size = self.m.desc_size;
        if !self.dirty.contains_key(&b) {
            if !self.admit(b) {
                return None;
            }
            let data = self.m.gdt.get((b - self.m.gdt_first()) as usize)?.clone();
            self.dirty.insert(b, Dirty { data, kind: Kind::Gdt });
        }
        let d = self.dirty.get_mut(&b)?;
        DescMut::new(&mut d.data[at..at + size])
    }

    /// Store inode `ino` as `raw`, its checksum put on it.
    pub fn put_inode(&mut self, ino: u32, raw: &mut [u8]) -> bool {
        let (ipg, isz, bs, csum, seed) = (self.m.ipg, self.m.inode_size, self.m.bs, self.m.csum, self.m.seed);
        if ino == 0 || ino > self.m.sb.inodes_count() || raw.len() != isz {
            return false;
        }
        let (g, index) = (u64::from((ino - 1) / ipg), (ino - 1) % ipg);
        let Some(table) = self.group(g).map(|d| d.inode_table()) else { return false };
        if csum {
            xinode::set_csum(seed, ino, raw);
        }
        let at = u64::from(index) * isz as u64;
        let (block, within) = (table + at / bs as u64, (at % bs as u64) as usize);
        let Some(blk) = self.modify(block, Kind::Inodes) else { return false };
        blk[within..within + isz].copy_from_slice(raw);
        true
    }

    /// Inode `ino` as this transaction has it.
    pub fn inode(&mut self, ino: u32) -> Option<Vec<u8>> {
        super::source::inode(self, ino)
    }

    /// The superblock as this transaction has it.
    pub fn super_now(&self) -> Option<Super> {
        let (b, at) = (self.m.sb_block(), self.m.sb_at());
        match self.dirty.get(&b) {
            Some(d) => Super::parse(&d.data[at..at + extfmt::sb::SIZE]).ok(),
            None => Some(self.m.sb.clone()),
        }
    }

    /// The superblock changed by `change`, in this transaction.
    pub fn change_super(&mut self, change: impl FnOnce(&mut Super)) -> bool {
        let (b, at) = (self.m.sb_block(), self.m.sb_at());
        let Some(blk) = self.modify(b, Kind::Super) else { return false };
        let Ok(mut s) = Super::parse(&blk[at..at + extfmt::sb::SIZE]) else { return false };
        change(&mut s);
        blk[at..at + extfmt::sb::SIZE].copy_from_slice(s.raw());
        true
    }

    /// The superblock saying a file may be 2 GiB or more (large_file), as
    /// e2fsck wants of a filesystem that has one -- in this transaction,
    /// with the file that first needs it.
    pub fn large_file(&mut self) -> bool {
        if self.super_now().is_some_and(|s| s.ro_compat() & ro_compat::LARGE_FILE != 0) {
            return true;
        }
        let set = self.change_super(|s| {
            let features = s.ro_compat() | ro_compat::LARGE_FILE;
            s.set_ro_compat(features);
        });
        if set {
            trace!(1, "ext4: a file of 2 GiB or more: large_file set");
        }
        set
    }

    /// Put on every block the checksum its kind says it carries: the
    /// bitmaps' into their descriptors first, then the descriptors' own,
    /// then every other block's. False, and the
    /// transaction not to be committed, for a block that cannot carry one.
    pub fn seal(&mut self) -> bool {
        if !self.m.csum {
            return true;
        }
        let bitmaps: Vec<(u64, Kind)> = self.dirty.iter()
            .filter(|(_, d)| matches!(d.kind, Kind::BlockBitmap(_) | Kind::InodeBitmap(_)))
            .map(|(b, d)| (*b, d.kind)).collect();
        for (b, kind) in bitmaps {
            let Some(data) = self.dirty.get(&b).map(|d| d.data.clone()) else { return false };
            let ok = match kind {
                Kind::BlockBitmap(g) => match group::block_bitmap_csum(&self.m.sb, &data) {
                    Some(c) => self.group_mut(g).map(|mut d| d.set_block_bitmap_csum(c)).is_some(),
                    None => false,
                },
                Kind::InodeBitmap(g) => match group::inode_bitmap_csum(&self.m.sb, &data) {
                    Some(c) => self.group_mut(g).map(|mut d| d.set_inode_bitmap_csum(c)).is_some(),
                    None => false,
                },
                _ => false,
            };
            if !ok {
                trace!(0, "ext4: the bitmap in block {} cannot be sealed", b);
                return false;
            }
        }
        let (bs, ds, groups, gdt_first, fs_seed) = (self.m.bs, self.m.desc_size, self.m.groups, self.m.gdt_first(),
            self.m.seed);
        let (sb_block, sb_at) = (self.m.sb_block(), self.m.sb_at());
        let sb = self.m.sb.clone();
        for (&b, d) in self.dirty.iter_mut() {
            let ok = match d.kind {
                Kind::Super => b == sb_block && match Super::parse(&d.data[sb_at..sb_at + extfmt::sb::SIZE]) {
                    Ok(mut s) => {
                        s.set_checksum();
                        d.data[sb_at..sb_at + extfmt::sb::SIZE].copy_from_slice(s.raw());
                        true
                    }
                    Err(_) => false,
                },
                Kind::Gdt => {
                    let first = (b - gdt_first) * (bs / ds) as u64;
                    for k in 0..bs / ds {
                        let g = first + k as u64;
                        if g >= groups {
                            break;
                        }
                        if let Some(mut desc) = DescMut::new(&mut d.data[k * ds..(k + 1) * ds]) {
                            desc.set_checksum(&sb, g as u32);
                        }
                    }
                    true
                }
                Kind::Dir { seed, dx: true } => dir::set_dx_csum(seed, &mut d.data),
                Kind::Dir { seed, dx: false } => dir::set_leaf_csum(seed, &mut d.data),
                Kind::Extent { seed } => extent::set_node_csum(seed, &mut d.data),
                Kind::Xattr => xattr::set_block_csum(fs_seed, b, &mut d.data),
                Kind::BlockBitmap(_) | Kind::InodeBitmap(_) | Kind::Inodes => true,
            };
            if !ok {
                trace!(0, "ext4: block {}, a {:?}, cannot carry its checksum", b, d.kind);
                return false;
            }
        }
        true
    }
}

impl Source for Txn<'_> {
    fn meta(&self) -> &Meta {
        self.m
    }

    fn group(&self, g: u64) -> Option<Desc<'_>> {
        let (b, at) = self.m.desc_at(g);
        match self.dirty.get(&b) {
            Some(d) if g < self.m.groups => Desc::new(d.data.get(at..at + self.m.desc_size)?),
            _ => self.m.desc(g),
        }
    }

    fn block(&mut self, b: u64, out: &mut [u8]) -> bool {
        if let Some(d) = self.dirty.get(&b) {
            out[..d.data.len()].copy_from_slice(&d.data);
            return true;
        }
        read_block(self.io, self.cache, &NO_OVERLAY, b, out, true)
    }
}
