//! ext4: the format through extfmt, every checksum on what is read held to
//! the one it should be -- and metadata written only through the journal,
//! one transaction a call, or a few hundred blocks of a write's data
//! (txn.rs, journal.rs, data.rs), the data before the commit that maps it,
//! as data=ordered has it. A mount that only reads
//! writes nothing: a journal nobody replayed is read through, not replayed.
//! A mount that writes replays it first, says the filesystem needs
//! recovery before anything else is written, releases the orphans the
//! superblock's list holds (orphans.rs) -- a file whose blocks one
//! transaction cannot give back is one -- and clears that only at a clean
//! unmount, once a checkpoint has put everything home.
//!
//! What is refused, read-only or written is extfmt's `features::classify`;
//! what is checked of the geometry past what extfmt checks is here, so
//! that every number worked out from the superblock is one that cannot
//! lead off the device. Directories are read the first time a walk enters
//! them, all or nothing, as ext2's are; an htree directory is read as the
//! linear one it also is, its index blocks checked by their own tails.
//! Names of 64 bytes or more, symlinks and device nodes are left out of
//! the tree, as ext2 leaves them -- and a directory holding any is never
//! removed.

mod bitmaps;
mod cache;
mod data;
mod dirs;
mod extents;
mod inodes;
mod io;
mod journal;
mod meta;
mod ops;
mod orphans;
mod source;
mod txn;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use block::Disk;
use extfmt::dir;
use extfmt::features::{self, incompat, Support};
use extfmt::inode::{Inode, INDEX_FL, S_IFDIR, S_IFREG};
use extfmt::sb::{Super, DESC_SIZE_64, STATE_ERROR, STATE_VALID};
use extfmt::{group, Map};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::time::wall_clock_secs;
use kcore::trace;

use crate::vfs::{FileSystem, Space};
use crate::vnode::{Kind, NodeId, Tree, NAME_MAX};

use cache::Cache;
use io::Io;
use journal::{Journal, JournalMap, Opened, Overlay};
use meta::Meta;
/// The free blocks only a punch may take: what a filesystem that had at
/// least these free when it was mounted can never refuse a punch for.
pub use txn::RESERVE;
use source::{Reader, Source};

const ROOT_INO: u32 = 2;

/// The first inode a file may have is never below this (`EXT4_GOOD_OLD_FIRST_INO`):
/// those under it are the filesystem's own -- the journal's among them.
const FIRST_INO_MIN: u32 = 11;

/// The largest descriptor the kernel takes (`EXT4_MAX_DESC_SIZE`): what a
/// mount holds of the descriptors grows with it, a block of them at a time.
const DESC_SIZE_MAX: u64 = 1024;

/// The superblock's error fields -- the count and the first and last
/// errors -- which a replay must not take back to what the journal's copy
/// had (`EXT4_S_ERR_START`, `EXT4_S_ERR_LEN`).
const ERR_FIELDS: core::ops::Range<usize> = 0x194..0x200;

pub struct Ext4 {
    io: Io,
    m: Option<Meta>,
    cache: Cache,
    jnl: Option<Journal>,
    overlay: Overlay,
    tree: Tree,
    root: Option<NodeId>,
    mounted: bool,
    read_only: bool,
    /// Per directory inode read, the entries of it the tree leaves out.
    hidden: BTreeMap<u32, u32>,
}

/// The superblock off a device: the sectors that hold its 1024 bytes.
fn read_super(dev: &Disk, scratch: &mut DmaBuffer) -> Option<Super> {
    let ss = dev.sector_size() as usize;
    if ss == 0 || ss > PAGE_SIZE || !ss.is_power_of_two() {
        trace!(0, "ext4: a sector of {} bytes", ss);
        return None;
    }
    let off = extfmt::sb::OFFSET as usize;
    let (start, within) = (off / ss, off % ss);
    let count = (within + extfmt::sb::SIZE).div_ceil(ss);
    let buf = scratch.as_mut_slice();
    if count * ss > buf.len() || dev.read(start as u64, &mut buf[..count * ss]).is_err() {
        trace!(0, "ext4: the superblock does not read");
        return None;
    }
    match Super::parse(&buf[within..within + extfmt::sb::SIZE]) {
        Ok(sb) => Some(sb),
        Err(e) => {
            trace!(0, "ext4: {}", e);
            None
        }
    }
}

/// The superblock of `dev`, if it has one of the ext family at all: what a
/// caller choosing between ext2 and ext4 reads.
pub fn probe_super(dev: &Disk) -> Option<Super> {
    let mut scratch = DmaBuffer::new(1)?;
    read_super(dev, &mut scratch)
}

impl Ext4 {
    pub fn new(dev: Disk) -> Option<Ext4> {
        Some(Ext4 {
            io: Io::new(dev)?,
            m: None,
            cache: Cache::new(),
            jnl: None,
            overlay: Overlay::new(),
            tree: Tree::new(),
            root: None,
            mounted: false,
            read_only: true,
            hidden: BTreeMap::new(),
        })
    }

    fn reader(&mut self) -> Option<Reader<'_>> {
        Some(Reader { io: &mut self.io, cache: &mut self.cache, overlay: &self.overlay, m: self.m.as_ref()? })
    }

    fn inode(&mut self, ino: u32) -> Option<Vec<u8>> {
        source::inode(&mut self.reader()?, ino)
    }

    fn map(&mut self, ino: u32, raw: &[u8], lblock: u64) -> Option<Map> {
        source::map(&mut self.reader()?, ino, raw, lblock)
    }

    /// Metadata block `b`, as a read sees it.
    fn meta_block(&mut self, b: u64, out: &mut [u8]) -> bool {
        self.reader().is_some_and(|mut r| r.block(b, out))
    }

    /// A file's data block `b`: what a read sees of it, kept nowhere.
    fn data_block(&mut self, b: u64, out: &mut [u8]) -> bool {
        source::read_block(&mut self.io, &mut self.cache, &self.overlay, b, out, false)
    }

    /// Whether a read of block `b` is answered by a copy held -- the
    /// journal's newer one a read-only mount reads through, the cache's --
    /// and not by its home: what `data_block` reads first.
    fn held(&mut self, b: u64) -> bool {
        self.overlay.contains_key(&b) || self.cache.get(b).is_some()
    }

    fn largedir(&self) -> bool {
        self.m.as_ref().is_some_and(|m| m.largedir())
    }

    /// What the driver requires of a superblock past what extfmt does: a
    /// block it can read in a page, a filesystem inside the device, groups
    /// whose bitmaps cover them, the inode count what the groups hold.
    /// Whether `sb` describes a filesystem this can read off this device --
    /// its checksum held to, unless `csum` says otherwise (`mount`'s torn
    /// superblock).
    fn check_super(&self, sb: &Super, csum: bool) -> bool {
        let bs = sb.block_size();
        let bpg = u64::from(sb.blocks_per_group());
        let ipg = u64::from(sb.inodes_per_group());
        let device = self.io.dev.sectors().saturating_mul(self.io.sector_size as u64) / bs.max(1);
        let why = if bs as usize > PAGE_SIZE || bs < self.io.sector_size as u64 {
            "a block size this cannot read"
        } else if sb.blocks_count() > device {
            "more blocks than the device has"
        } else if bpg > 8 * bs || ipg > 8 * bs || u64::from(sb.clusters_per_group()) != bpg || ipg == 0 || bpg == 0 {
            "groups more than their bitmaps cover"
        } else if sb.group_count().checked_mul(ipg) != Some(u64::from(sb.inodes_count())) {
            "an inode count that is not the groups'"
        } else if sb.inode_size() > bs || (ipg * sb.inode_size()) % bs != 0 {
            "inode tables that do not fill their blocks"
        } else if sb.is_64bit() && sb.desc_size() < DESC_SIZE_64 {
            "64bit with descriptors under 64 bytes"
        } else if sb.desc_size() > DESC_SIZE_MAX {
            "descriptors of more than 1024 bytes"
        } else if sb.first_ino() < FIRST_INO_MIN {
            "a first inode files may have below 11"
        } else if u64::from(sb.first_data_block()) != (bs == 1024) as u64 {
            "a first data block that is not the block size's"
        } else if csum && !sb.checksum_ok() {
            "a superblock that fails its checksum"
        } else {
            return true;
        };
        trace!(0, "ext4: refused: {}", why);
        false
    }

    /// The descriptors into `m`, read -- through the overlay -- and held to
    /// their checksums and to the filesystem: every group's bitmaps and
    /// table inside it.
    fn read_descriptors(&mut self, m: &mut Meta) -> bool {
        let bs = m.bs;
        let gdt_blocks = m.gdt_len() as usize;
        let mut gdt: Vec<Vec<u8>> = Vec::new();
        if gdt.try_reserve_exact(gdt_blocks).is_err() {
            trace!(0, "ext4: no memory for {} blocks of descriptors", gdt_blocks);
            return false;
        }
        for k in 0..gdt_blocks {
            let mut blk = Vec::new();
            if blk.try_reserve_exact(bs).is_err() {
                trace!(0, "ext4: no memory for {} blocks of descriptors", gdt_blocks);
                return false;
            }
            blk.resize(bs, 0);
            if !source::read_block(&mut self.io, &mut self.cache, &self.overlay, m.gdt_first() + k as u64, &mut blk,
                false) {
                return false;
            }
            gdt.push(blk);
        }
        m.gdt = gdt;
        let table_blocks = m.table_blocks();
        /* Past the superblock, the descriptors and the blocks kept for them
         * to grow into -- group 0's copy, the one this writes: a bitmap there
         * would be written over the descriptors, and they over it. */
        let base = m.first_data + m.base_meta(0);
        for g in 0..m.groups {
            let Some(d) = m.desc(g) else { return false };
            if let Some(c) = group::desc_csum(&m.sb, g as u32, d.raw()) {
                if c != d.checksum() {
                    trace!(0, "ext4: refused: group {}'s descriptor fails its checksum", g);
                    return false;
                }
            }
            let inside = |b: u64, n: u64| b >= base && b.checked_add(n).is_some_and(|e| e <= m.blocks);
            if !inside(d.block_bitmap(), 1) || !inside(d.inode_bitmap(), 1) || !inside(d.inode_table(), table_blocks) {
                trace!(0, "ext4: refused: group {}'s bitmaps or table outside the filesystem, or on its superblock \
                           and descriptors", g);
                return false;
            }
        }
        true
    }

    /// The superblock and the descriptors, read and held to the device: a
    /// Meta, or None, the reason said.
    fn load(&mut self, sb: &Super) -> Option<Meta> {
        self.load_held(sb, true)
    }

    /// `load`, the superblock's checksum held to or not.
    fn load_held(&mut self, sb: &Super, csum: bool) -> Option<Meta> {
        if !self.check_super(sb, csum) {
            return None;
        }
        self.io.bs = sb.block_size() as usize;
        self.io.blocks = sb.blocks_count();
        let mut m = Meta::new(sb);
        self.read_descriptors(&mut m).then_some(m)
    }

    /// The superblock in the journal's newer copy of it, where a read-only
    /// mount's overlay has one: the filesystem as a replay would leave it.
    /// A `torn` superblock must have one.
    fn reload_through_overlay(&mut self, sb: &Super, torn: bool) -> Option<Meta> {
        let m = self.m.as_ref()?;
        let (first, last) = (m.sb_block(), m.gdt_first() + m.gdt_len());
        if torn && !self.overlay.contains_key(&first) {
            trace!(0, "ext4: refused: the superblock fails its checksum, and the journal holds no copy of it");
            return None;
        }
        if !(first..last).any(|b| self.overlay.contains_key(&b)) {
            return self.m.take();
        }
        let at = m.sb_at();
        let mut blk = vec![0u8; self.io.bs];
        if !source::read_block(&mut self.io, &mut self.cache, &self.overlay, first, &mut blk, false) {
            return None;
        }
        let newer = match Super::parse(&blk[at..at + extfmt::sb::SIZE]) {
            Ok(s) if s.blocks_count() == sb.blocks_count() && s.inodes_count() == sb.inodes_count()
                && s.block_size() == sb.block_size() && s.checksum_ok() => s,
            _ => {
                trace!(0, "ext4: refused: the journal's superblock is not this filesystem's");
                return None;
            }
        };
        /* Read through, it is what the mount reads by: held to all the
         * device's is held to. */
        if !self.check_super(&newer, true) {
            return None;
        }
        if let Support::Refused(why) = features::classify(&newer) {
            trace!(0, "ext4: refused: the journal's superblock: {}", why);
            return None;
        }
        let mut m = Meta::new(&newer);
        self.read_descriptors(&mut m).then_some(m)
    }

    /// The superblock as it is now, onto its block on the device and past
    /// the device's cache.
    fn write_super(&mut self) -> bool {
        let Some(m) = self.m.as_mut() else { return false };
        m.sb.set_checksum();
        let (b, at) = (m.sb_block(), m.sb_at());
        let raw = *m.sb.raw();
        let mut blk = vec![0u8; self.io.bs];
        if !source::read_block(&mut self.io, &mut self.cache, &self.overlay, b, &mut blk, false) {
            return false;
        }
        blk[at..at + extfmt::sb::SIZE].copy_from_slice(&raw);
        self.cache.forget(b);
        self.io.write(b, &blk, true)
    }

    /// Mount: read-only when asked or when the filesystem says so -- the
    /// reason said -- else for writing. Some(whether read-only), or None.
    pub fn mount(&mut self, read_only: bool) -> Option<bool> {
        self.reset();
        let sb = read_super(&self.io.dev, &mut self.io.scratch)?;
        let mut ro_why: Option<String> = read_only.then(|| String::from("asked"));
        match features::classify(&sb) {
            Support::Refused(why) => {
                trace!(0, "ext4: refused: {}", why);
                return None;
            }
            Support::ReadOnly(why) => {
                ro_why.get_or_insert_with(|| String::from(why));
            }
            Support::Writable => {}
        }
        /* A superblock that fails only its checksum, on a filesystem with a
         * journal: one a power cut tore as a checkpoint or a replay wrote it
         * -- its sectors half the old, half the new, which agree on where
         * everything is -- while the journal still holds its copy, as the
         * log's start moves past a copy only once its home is on the
         * medium. Used to find the journal and nothing more: a replay must
         * write a sound one home, or a read through it find one. */
        let mut torn = !sb.checksum_ok() && sb.compat() & features::compat::HAS_JOURNAL != 0;
        if torn {
            trace!(0, "ext4: the superblock fails its checksum: the journal's copy of it is wanted");
        }
        if sb.state() & STATE_ERROR != 0 {
            ro_why.get_or_insert_with(|| String::from("the filesystem says it has errors (e2fsck it)"));
        } else if sb.state() & STATE_VALID == 0 {
            ro_why.get_or_insert_with(|| String::from("the filesystem is not marked valid (e2fsck it)"));
        }
        self.m = Some(self.load_held(&sb, !torn)?);
        let has_journal = sb.compat() & features::compat::HAS_JOURNAL != 0;
        let mut journal_map = JournalMap::default();
        if has_journal {
            journal_map = match journal::blocks(&mut self.reader()?, &sb) {
                Ok(map) => map,
                Err(e) => {
                    trace!(0, "ext4: refused: the journal: {}", e);
                    return None;
                }
            };
        }
        if has_journal && ro_why.is_none() {
            /* The log is written over the journal's blocks, and a replay
             * writes too: none of them may be the filesystem's own
             * metadata. */
            let Some(m) = self.m.as_mut() else { return None };
            if !m.build_system_zone(&JournalMap::default()) {
                trace!(0, "ext4: no memory for the system zone");
                self.reset();
                return None;
            }
            if let Some((dev, n)) = journal_map.runs().find(|&(dev, n)| m.overlaps_system(dev, dev + n)) {
                trace!(0, "ext4: refused: the journal's blocks {}..{} are the filesystem's own metadata", dev,
                       dev + n);
                self.reset();
                return None;
            }
            self.io.writable = true;
            let errors: Vec<u8> = sb.raw()[ERR_FIELDS].to_vec();
            match Journal::open(&mut self.io, journal_map.clone(), &sb) {
                Err(e) => {
                    trace!(0, "ext4: refused: the journal: {}", e);
                    self.io.writable = false;
                    return None;
                }
                Ok(Opened::ReadOnly(why)) => {
                    self.io.writable = false;
                    ro_why = Some(why);
                }
                Ok(Opened::Ready(jnl, replayed)) => {
                    self.jnl = Some(jnl);
                    if replayed {
                        /* What the replay left, the error fields as they
                         * were before it. */
                        self.cache.clear();
                        let mut newer = read_super(&self.io.dev, &mut self.io.scratch)?;
                        if torn && !newer.checksum_ok() {
                            trace!(0, "ext4: refused: the superblock fails its checksum, and the journal held no \
                                       copy of it");
                            self.reset();
                            return None;
                        }
                        newer.raw_mut()[ERR_FIELDS].copy_from_slice(&errors);
                        if newer.blocks_count() != sb.blocks_count() || newer.inodes_count() != sb.inodes_count()
                            || newer.block_size() != sb.block_size()
                            || features::classify(&newer) != Support::Writable {
                            trace!(0, "ext4: refused: the replayed superblock is another filesystem's");
                            self.reset();
                            return None;
                        }
                        newer.set_checksum();
                        let Some(m) = self.load(&newer) else {
                            self.reset();
                            return None;
                        };
                        self.m = Some(m);
                        /* The superblock the replay wrote home is sound: a
                         * read-only mount from here on reads it, with no copy
                         * in a journal now empty. */
                        torn = false;
                        /* What the replay left says how the filesystem is,
                         * as the device's said before it: errors, or not
                         * valid, and it is only read. */
                        let state = newer.state();
                        let why = if state & STATE_ERROR != 0 {
                            Some("the replayed filesystem says it has errors (e2fsck it)")
                        } else if state & STATE_VALID == 0 {
                            Some("the replayed filesystem is not marked valid (e2fsck it)")
                        } else {
                            None
                        };
                        if let Some(why) = why {
                            ro_why = Some(String::from(why));
                            self.jnl = None;
                            self.io.writable = false;
                        }
                    } else if torn {
                        trace!(0, "ext4: refused: the superblock fails its checksum, and the journal holds no copy \
                                   of it");
                        self.reset();
                        return None;
                    }
                }
            }
        }
        if ro_why.is_none() && self.m.as_ref().is_some_and(|m| m.sb.last_orphan() != 0) {
            /* Walked before anything is written: released once the
             * superblock says the filesystem needs recovery. */
            if let Err(why) = self.orphans_sound() {
                ro_why = Some(why);
                self.jnl = None;
                self.io.writable = false;
            }
        }
        if let Some(why) = &ro_why {
            if why != "asked" {
                trace!(0, "ext4: read-only: {}", why);
            }
            if has_journal && self.jnl.is_none() {
                match journal::overlay(&mut self.io, &journal_map, &sb) {
                    Ok(o) => self.overlay = o,
                    Err(e) => {
                        trace!(0, "ext4: refused: the journal: {}", e);
                        self.reset();
                        return None;
                    }
                }
                let m = self.reload_through_overlay(&sb, torn);
                let Some(m) = m else {
                    self.reset();
                    return None;
                };
                self.m = Some(m);
            }
        }
        /* The root, read before anything is written: a filesystem whose
         * root does not read is refused as it was found -- no superblock
         * written, no orphan released. */
        let Some(raw) = self.inode(ROOT_INO) else {
            self.reset();
            return None;
        };
        if Inode::new(&raw).is_none_or(|i| !i.is_dir()) {
            trace!(0, "ext4: refused: the root is not a directory");
            self.reset();
            return None;
        }
        if ro_why.is_none() {
            let now = wall_clock_secs() as u32;
            let Some(m) = self.m.as_mut() else { return None };
            if !m.build_system_zone(&journal_map) {
                trace!(0, "ext4: no memory for the system zone");
                self.reset();
                return None;
            }
            m.count_free_blocks();
            /* Needing recovery, on the medium, before the journal is
             * written at all. */
            let incompat_now = m.sb.incompat() | incompat::RECOVER;
            m.sb.set_incompat(incompat_now);
            m.sb.set_mtime(now);
            let count = m.sb.mnt_count();
            m.sb.set_mnt_count(count.wrapping_add(1));
            if !self.write_super() {
                trace!(0, "ext4: refused: the superblock does not write");
                self.reset();
                return None;
            }
            match self.release_orphans() {
                Ok(0) => {}
                Ok(n) => trace!(0, "ext4: the orphan list released: {} inodes", n),
                Err(why) => {
                    /* Left for whoever mounts it next: nothing more is
                     * written, the list as the last commit left it. */
                    trace!(0, "ext4: read-only: {}", why);
                    if let Some(j) = self.jnl.as_mut() {
                        j.abort(&mut self.io, &why);
                    }
                    ro_why = Some(why);
                }
            }
        }
        self.read_only = ro_why.is_some();
        if self.read_only {
            /* What `space` says of a filesystem only read: the groups'
             * counts as the mount read them -- through the journal, where
             * it holds newer ones. */
            if let Some(m) = self.m.as_mut() {
                m.count_free_blocks();
            }
        }
        self.root = self.new_vnode(None, b"", Kind::Dir, ROOT_INO, 0);
        if self.root.is_none() {
            self.reset();
            return None;
        }
        self.mounted = true;
        Some(self.read_only)
    }

    /// Everything a mount set up, gone -- nothing written.
    fn reset(&mut self) {
        self.tree.clear();
        self.root = None;
        self.overlay.clear();
        self.cache.clear();
        self.hidden.clear();
        self.jnl = None;
        self.m = None;
        self.io.writable = false;
        self.mounted = false;
        self.read_only = true;
    }

    /// A clean unmount, for a mount that wrote: everything committed home,
    /// the log empty, and the filesystem no longer needing recovery -- its
    /// free counts the groups' -- each forced to the medium. After an abort
    /// nothing at all: the journal is left for a replay.
    pub fn unmount(&mut self) {
        if self.mounted && !self.read_only {
            let closed = match self.jnl.as_mut() {
                Some(j) => j.close(&mut self.io, &mut self.cache),
                None => false,
            };
            if closed {
                if let Some(m) = self.m.as_mut() {
                    let (mut blocks, mut inodes) = (0u64, 0u64);
                    for g in 0..m.groups {
                        if let Some(d) = m.desc(g) {
                            blocks += u64::from(d.free_blocks_count());
                            inodes += u64::from(d.free_inodes_count());
                        }
                    }
                    m.sb.set_free_blocks_count(blocks);
                    m.sb.set_free_inodes_count(u32::try_from(inodes).unwrap_or(u32::MAX).min(m.sb.inodes_count()));
                    let incompat_now = m.sb.incompat() & !incompat::RECOVER;
                    m.sb.set_incompat(incompat_now);
                    m.sb.set_wtime(wall_clock_secs() as u32);
                }
                if !self.write_super() {
                    trace!(0, "ext4: the superblock does not write at the unmount: left needing recovery");
                }
            } else {
                trace!(0, "ext4: unmounted with the journal aborted: left for a replay");
            }
        }
        self.reset();
    }

    pub fn sync(&mut self) -> bool {
        if !self.mounted || self.read_only {
            return true;
        }
        if self.jnl.as_ref().is_none_or(|j| j.aborted) {
            trace!(0, "ext4: the journal is aborted: nothing is synced");
            return false;
        }
        /* Every commit is on the medium already: its blocks flushed, its
         * commit block forced. A flush the device fails is one whose order
         * nothing after it can count on: the journal aborts. */
        if self.io.flush() {
            return true;
        }
        if let Some(j) = self.jnl.as_mut() {
            j.abort(&mut self.io, "the device does not flush");
        }
        false
    }

    fn new_vnode(&mut self, parent: Option<NodeId>, name: &[u8], kind: Kind, ino: u32, size: usize) -> Option<NodeId> {
        let Some(node) = self.tree.alloc(name, kind) else {
            trace!(0, "ext4: no memory for a vnode");
            return None;
        };
        {
            let made = &mut self.tree[node];
            made.size = if kind == Kind::File { size } else { 0 };
            made.ino = ino as usize;
        }
        if let Some(parent) = parent {
            self.tree.insert_child(parent, node);
        }
        Some(node)
    }

    /// A directory's entries read into its vnode the first time it is
    /// needed: all of them or none, as ext2's.
    pub fn load_dir(&mut self, dir: NodeId) -> bool {
        match self.tree.get(dir) {
            Some(node) if node.is_dir() => {
                if node.dir_loaded {
                    return true;
                }
            }
            _ => return false,
        }
        if self.read_dir(dir) {
            self.tree[dir].dir_loaded = true;
            return true;
        }
        while let Some(child) = self.tree.first_child(dir) {
            self.tree.free_tree(child);
        }
        false
    }

    fn read_dir(&mut self, dir: NodeId) -> bool {
        let Some(ino) = self.tree.get(dir).map(|n| n.ino as u32) else { return false };
        let Some(raw) = self.inode(ino) else { return false };
        let Some(node) = Inode::new(&raw) else { return false };
        if !node.is_dir() {
            trace!(0, "ext4: inode {} is not a directory", ino);
            return false;
        }
        let Some(seed) = self.m.as_ref().map(|m| m.seed) else { return false };
        let iseed = extfmt::inode::seed(seed, ino, node.generation());
        let indexed = node.flags() & INDEX_FL != 0;
        let bs = self.io.bs as u64;
        let blocks = node.size(self.largedir()).div_ceil(bs);
        self.hidden.insert(ino, 0);
        let mut lblock = 0u64;
        let mut blk = vec![0u8; self.io.bs];
        while lblock < blocks {
            match self.map(ino, &raw, lblock) {
                None => return false,
                Some(Map::Hole { run }) => lblock = lblock.saturating_add(run.max(1)),
                Some(Map::Blocks { block, run }) => {
                    for k in 0..run.min(blocks - lblock) {
                        if !self.meta_block(block + k, &mut blk) || !self.dir_block(dir, ino, iseed, indexed, lblock + k, &blk) {
                            return false;
                        }
                    }
                    lblock = lblock.saturating_add(run.max(1));
                }
            }
        }
        true
    }

    /// One block of directory `ino`: held to its checksum -- an index
    /// block's tail, or a leaf's -- and its entries adopted.
    fn dir_block(&mut self, dir: NodeId, ino: u32, iseed: u32, indexed: bool, lblock: u64, blk: &[u8]) -> bool {
        let spans = extfmt::le32(blk, 0) == 0 && dir::rec_len_from_disk(extfmt::le16(blk, 4), blk.len()) == blk.len();
        if self.m.as_ref().is_some_and(|m| m.csum) {
            let ok = if indexed && (lblock == 0 || spans) {
                dir::dx_csum(iseed, blk).is_some_and(|(s, c)| s == c)
            } else {
                dir::tail(blk).is_some_and(|s| s == dir::leaf_csum(iseed, blk))
            };
            if !ok {
                trace!(0, "ext4: block {} of directory {} fails its checksum", lblock, ino);
                return false;
            }
        }
        let mut entries: Vec<(Vec<u8>, u32, u8)> = Vec::new();
        for e in dir::entries(blk, ino) {
            match e {
                Ok(e) => {
                    if e.ino != 0 && e.name != b"." && e.name != b".." {
                        entries.push((e.name.to_vec(), e.ino, e.file_type));
                    }
                }
                Err(e) => {
                    trace!(0, "ext4: {}", e);
                    return false;
                }
            }
        }
        for (name, child, file_type) in entries {
            if name.len() >= NAME_MAX {
                trace!(0, "ext4: a name of {} bytes in directory {} is too long, skipped", name.len(), ino);
                self.hide(ino);
                continue;
            }
            match self.adopt(dir, &name, child, file_type) {
                Some(true) => {}
                Some(false) => self.hide(ino),
                None => return false,
            }
        }
        true
    }

    /// One more entry of directory `ino` the tree leaves out.
    fn hide(&mut self, ino: u32) {
        *self.hidden.entry(ino).or_insert(0) += 1;
    }

    /// A vnode for one entry, if it names a file or a directory: Some(true)
    /// when one is made, Some(false) when the entry is left out -- one that
    /// names something else, or an inode that does not read, or one no
    /// entry may name: the filesystem's own (the journal's, say, whose
    /// blocks a write to it would go over), one deleted, a file bigger than
    /// a file here can be (each said) -- and None when there is no memory
    /// for it.
    fn adopt(&mut self, dir: NodeId, name: &[u8], ino: u32, file_type: u8) -> Option<bool> {
        if file_type != dir::FT_REG_FILE && file_type != dir::FT_DIR && file_type != dir::FT_UNKNOWN {
            return Some(false);
        }
        let first_ino = self.m.as_ref().map_or(FIRST_INO_MIN, |m| m.first_ino);
        if ino != ROOT_INO && ino < first_ino {
            trace!(0, "ext4: an entry of directory {} names inode {}, one of the filesystem's own: skipped",
                self.tree[dir].ino, ino);
            return Some(false);
        }
        let Some(raw) = self.inode(ino) else {
            trace!(0, "ext4: an entry of directory {} names inode {}, which does not read: skipped",
                self.tree[dir].ino, ino);
            return Some(false);
        };
        let Some(node) = Inode::new(&raw) else { return Some(false) };
        let kind = match node.file_type() {
            S_IFDIR => Kind::Dir,
            S_IFREG => Kind::File,
            _ => return Some(false),
        };
        if node.links_count() == 0 {
            trace!(0, "ext4: an entry of directory {} names inode {}, which is deleted: skipped",
                self.tree[dir].ino, ino);
            return Some(false);
        }
        if kind == Kind::File && node.size(self.largedir()) > self.max_size() {
            trace!(0, "ext4: an entry of directory {} names inode {}, of {} bytes, more than a file here can be: \
                       skipped", self.tree[dir].ino, ino, node.size(self.largedir()));
            return Some(false);
        }
        if kind == Kind::Dir {
            /* An entry leading back up the tree is a cycle in the image. */
            let mut up = Some(dir);
            while let Some(at) = up {
                if self.tree[at].ino == ino as usize {
                    trace!(0, "ext4: an entry of inode {} is an ancestor, skipped", self.tree[dir].ino);
                    return Some(false);
                }
                up = self.tree[at].parent();
            }
        }
        let size = usize::try_from(node.size(self.largedir())).unwrap_or(usize::MAX);
        self.new_vnode(Some(dir), name, kind, ino, size).map(|_| true)
    }

    pub fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        if !self.tree.get(dir)?.is_dir() || !self.load_dir(dir) {
            return None;
        }
        self.tree.find_child(dir, name)
    }

    fn file_ino(&self, file: NodeId) -> Option<u32> {
        match self.tree.get(file) {
            Some(node) if node.is_file() => Some(node.ino as u32),
            _ => None,
        }
    }

    /// `buf` filled from `offset` with as much of the file as there is.
    pub fn read(&mut self, file: NodeId, buf: &mut [u8], offset: usize) -> bool {
        let Some(ino) = self.file_ino(file) else { return false };
        let Some(raw) = self.inode(ino) else { return false };
        let size = Inode::new(&raw).map(|i| i.size(self.largedir())).unwrap_or(0);
        let bs = self.io.bs as u64;
        let end = (offset as u64).saturating_add(buf.len() as u64).min(size);
        let mut pos = offset as u64;
        let mut blk = vec![0u8; self.io.bs];
        let mut batch: Vec<(usize, u64)> = Vec::with_capacity(self.io.batch_blocks());
        while pos < end {
            let lblock = pos / bs;
            match self.map(ino, &raw, lblock) {
                None => return false,
                Some(Map::Hole { run }) => {
                    let (within, at) = (pos % bs, (pos - offset as u64) as usize);
                    let n = run.saturating_mul(bs).saturating_sub(within).min(end - pos) as usize;
                    buf[at..at + n].fill(0);
                    pos += n as u64;
                }
                Some(Map::Blocks { block, run }) => {
                    /* As much of the run as the read reaches and a batch
                     * holds, off the device in one batch -- but a block
                     * whose copy is held, the journal's newer one a
                     * read-only mount reads through, read from that, as a
                     * read of it alone would be. */
                    let blocks = ((end - 1) / bs - lblock + 1).min(run).min(self.io.batch_blocks() as u64).max(1);
                    batch.clear();
                    for k in 0..blocks as usize {
                        let b = block + k as u64;
                        if !self.held(b) {
                            batch.push((k, b));
                        }
                    }
                    if !self.io.read_batch(&batch) {
                        return false;
                    }
                    let mut next = 0;
                    for k in 0..blocks as usize {
                        let (within, at) = ((pos % bs) as usize, (pos - offset as u64) as usize);
                        let n = (self.io.bs - within).min((end - pos) as usize);
                        if batch.get(next).is_some_and(|&(j, _)| j == k) {
                            buf[at..at + n].copy_from_slice(&self.io.slot(k)[within..within + n]);
                            next += 1;
                        } else {
                            if !self.data_block(block + k as u64, &mut blk) {
                                return false;
                            }
                            buf[at..at + n].copy_from_slice(&blk[within..within + n]);
                        }
                        pos += n as u64;
                    }
                }
            }
        }
        true
    }

    /// The device sector each of `out.len()` blocks of `file` from `first`
    /// begins at, 0 for a hole or an unwritten run -- refused while the
    /// journal holds newer copies the device does not: an image is then
    /// read through the VFS (`data_block_size`).
    pub fn map_blocks(&mut self, file: NodeId, first: u64, out: &mut [u64]) -> bool {
        if !self.overlay.is_empty() {
            return false;
        }
        let Some(ino) = self.file_ino(file) else { return false };
        let Some(raw) = self.inode(ino) else { return false };
        let sectors = (self.io.bs / self.io.sector_size) as u64;
        for (k, slot) in out.iter_mut().enumerate() {
            let Some(l) = first.checked_add(k as u64) else { return false };
            *slot = match self.map(ino, &raw, l) {
                Some(Map::Blocks { block, .. }) => block * sectors,
                Some(Map::Hole { .. }) => 0,
                None => return false,
            };
        }
        true
    }
}

impl FileSystem for Ext4 {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn info(&self, out: &mut dyn core::fmt::Write) {
        let _ = out.write_str(self.io.dev.name());
        if let Some(m) = &self.m {
            let label = m.sb.volume_name();
            let end = label.iter().position(|b| *b == 0).unwrap_or(label.len());
            if let Ok(label) = core::str::from_utf8(&label[..end]) {
                if !label.is_empty() {
                    let _ = write!(out, " label={}", label);
                }
            }
        }
        if !self.overlay.is_empty() {
            let _ = write!(out, " journal-not-replayed={}", self.overlay.len());
        }
        if let Some(j) = &self.jnl {
            if j.aborted {
                let _ = out.write_str(" journal-aborted");
            } else {
                let (used, ring, seq) = j.info();
                let _ = write!(out, " journal={}/{} seq={}", used, ring, seq);
            }
        }
    }

    fn device(&self) -> usize {
        self.io.dev.handle()
    }

    fn mount(&mut self, read_only: bool) -> Option<bool> {
        Ext4::mount(self, read_only)
    }

    fn unmount(&mut self) {
        Ext4::unmount(self);
    }

    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    fn root(&self) -> Option<NodeId> {
        self.root
    }

    fn load_dir(&mut self, dir: NodeId) -> bool {
        Ext4::load_dir(self, dir)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        Ext4::lookup(self, dir, name)
    }

    fn create_file(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.create(dir, name, false)
    }

    fn create_dir(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.create(dir, name, true)
    }

    fn read(&mut self, file: NodeId, buf: &mut [u8], offset: usize) -> bool {
        Ext4::read(self, file, buf, offset)
    }

    fn write(&mut self, file: NodeId, data: &[u8], offset: usize) -> bool {
        Ext4::write(self, file, data, offset)
    }

    fn truncate(&mut self, file: NodeId, size: usize) -> bool {
        Ext4::truncate(self, file, size)
    }

    fn rename(&mut self, node: NodeId, dir: NodeId, name: &[u8]) -> bool {
        Ext4::rename(self, node, dir, name)
    }

    fn remove(&mut self, node: NodeId) -> bool {
        Ext4::remove(self, node)
    }

    fn sync(&mut self) -> bool {
        Ext4::sync(self)
    }

    /// What extents number: 2^32 - 1 blocks.
    fn sparse_max(&self) -> usize {
        if self.mounted { usize::try_from(self.max_size()).unwrap_or(usize::MAX) } else { 0 }
    }

    /// The filesystem's blocks, read off the VFS's lock by an image -- but
    /// not while a journal holds newer copies the device does not.
    /// The blocks past the first data block, and of them those the groups
    /// count free -- as the last commit left them -- but the few kept for a
    /// punch (`RESERVE`), which no file's data is given.
    fn space(&self) -> Option<Space> {
        let m = self.m.as_ref().filter(|_| self.mounted)?;
        let bs = m.bs as u64;
        let data = m.blocks.saturating_sub(m.first_data);
        let free = m.free_blocks.saturating_sub(RESERVE).min(data);
        Some(Space { total_bytes: data * bs, free_bytes: free * bs })
    }

    fn data_block_size(&self) -> usize {
        if self.mounted && self.overlay.is_empty() { self.io.bs } else { 0 }
    }

    fn map_blocks(&mut self, file: NodeId, first: u64, out: &mut [u64]) -> bool {
        Ext4::map_blocks(self, file, first, out)
    }

    fn punch(&mut self, file: NodeId, first: u64, count: u64) -> Option<u64> {
        Ext4::punch(self, file, first, count)
    }

    fn links(&mut self, file: NodeId) -> Option<u32> {
        let ino = self.file_ino(file)?;
        let raw = self.inode(ino)?;
        Inode::new(&raw).map(|i| u32::from(i.links_count()))
    }
}

/// Mount the device's ext4 at `path`: 1 when mounted read-only, 0 when
/// for writing, -1 when not mounted.
pub fn mount_at(path: &str, device: usize, read_only: bool) -> i32 {
    let Some(dev) = Disk::from_handle(device) else { return -1 };
    let Some(vfs) = crate::vfs_instance() else { return -1 };
    let Some(fs) = Ext4::new(dev) else {
        trace!(0, "ext4: no memory for the filesystem");
        return -1;
    };
    match vfs.mount(path.as_bytes(), Box::new(fs), read_only) {
        Some(true) => 1,
        Some(false) => 0,
        None => -1,
    }
}
