//! ext4, read-only for now: the format through extfmt, every checksum on
//! what is read held to the one it should be, and a journal nobody
//! replayed read through rather than replayed (journal.rs) -- so that a
//! mount writes nothing. Writing, through the journal, is the next step
//! (docs/filesystems.md, "ext4"); until then every filesystem this mounts
//! is mounted read-only, whatever was asked.
//!
//! What is refused, read-only or written is extfmt's `features::classify`;
//! what is checked of the geometry past what extfmt checks is here, so
//! that every number worked out from the superblock is one that cannot
//! lead off the device. Directories are read the first time a walk enters
//! them, all or nothing, as ext2's are; an htree directory is read as the
//! linear one it also is, its index blocks checked by their own tails.
//! Names of 64 bytes or more, symlinks and device nodes are left out of
//! the tree, as ext2 leaves them.

mod journal;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use block::Disk;
use extfmt::features::{self, ro_compat, Support};
use extfmt::group::{self, Desc, INODE_UNINIT};
use extfmt::inode::{self as xinode, Inode, EXTENTS_FL, INDEX_FL, S_IFDIR, S_IFREG};
use extfmt::sb::{Super, DESC_SIZE_64};
use extfmt::{blockmap, dir, extent, Map};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::trace;

use crate::vfs::FileSystem;
use crate::vnode::{Kind, NodeId, Tree, NAME_MAX};

const ROOT_INO: u32 = 2;

/// The device, by blocks: every read through one page the device may DMA
/// into, held to the filesystem's bounds.
struct Io {
    dev: Disk,
    bs: usize,
    sector_size: usize,
    blocks: u64,
    scratch: DmaBuffer,
}

impl Io {
    /// Block `block` into `out`, a block long.
    fn read(&mut self, block: u64, out: &mut [u8]) -> bool {
        if block >= self.blocks {
            trace!(0, "ext4: a read of block {}, past the filesystem's {}", block, self.blocks);
            return false;
        }
        let sector = block * (self.bs / self.sector_size) as u64;
        let buf = &mut self.scratch.as_mut_slice()[..self.bs];
        if self.dev.read(sector, buf).is_err() {
            trace!(0, "ext4: block {} does not read", block);
            return false;
        }
        out[..self.bs].copy_from_slice(buf);
        true
    }
}

pub struct Ext4 {
    io: Io,
    sb: Option<Super>,
    seed: u32,
    csum: bool,
    /// Whether descriptors' uninit flags mean anything: with metadata_csum
    /// or gdt_csum.
    uninit: bool,
    gdt: Vec<u8>,
    groups: u64,
    desc_size: usize,
    inode_size: usize,
    ipg: u32,
    tree: Tree,
    root: Option<NodeId>,
    mounted: bool,
    overlay: journal::Overlay,
    /// The inode table block last read, and its bytes.
    itab: Option<(u64, Vec<u8>)>,
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
        let sector_size = dev.sector_size() as usize;
        if sector_size == 0 || sector_size > PAGE_SIZE {
            return None;
        }
        Some(Ext4 {
            io: Io { dev, bs: 0, sector_size, blocks: 0, scratch: DmaBuffer::new(1)? },
            sb: None,
            seed: 0,
            csum: false,
            uninit: false,
            gdt: Vec::new(),
            groups: 0,
            desc_size: 0,
            inode_size: 0,
            ipg: 0,
            tree: Tree::new(),
            root: None,
            mounted: false,
            overlay: journal::Overlay::new(),
            itab: None,
        })
    }

    /// Block `b`: its newest copy, in a journal nobody replayed if it is
    /// there, else its home.
    fn block(&mut self, b: u64, out: &mut [u8]) -> bool {
        if let Some(&(copy, escaped)) = self.overlay.get(&b) {
            if !self.io.read(copy, out) {
                return false;
            }
            if escaped {
                out[..4].copy_from_slice(&extfmt::jbd2::MAGIC.to_be_bytes());
            }
            return true;
        }
        self.io.read(b, out)
    }

    /// What the driver requires of a superblock past what extfmt does: a
    /// block it can read in a page, a filesystem inside the device, groups
    /// whose bitmaps cover them, the inode count what the groups hold.
    fn check_super(&self, sb: &Super) -> bool {
        let bs = sb.block_size();
        let bpg = u64::from(sb.blocks_per_group());
        let ipg = u64::from(sb.inodes_per_group());
        let device = self.io.dev.sectors().saturating_mul(self.io.sector_size as u64) / bs.max(1);
        let why = if bs as usize > PAGE_SIZE || bs < self.io.sector_size as u64 {
            "a block size this cannot read"
        } else if sb.blocks_count() > device {
            "more blocks than the device has"
        } else if bpg > 8 * bs || ipg > 8 * bs || u64::from(sb.clusters_per_group()) != bpg {
            "groups more than their bitmaps cover"
        } else if sb.group_count().checked_mul(ipg) != Some(u64::from(sb.inodes_count())) {
            "an inode count that is not the groups'"
        } else if sb.inode_size() > bs || (ipg * sb.inode_size()) % bs != 0 {
            "inode tables that do not fill their blocks"
        } else if sb.is_64bit() && sb.desc_size() < DESC_SIZE_64 {
            "64bit with descriptors under 64 bytes"
        } else if u64::from(sb.first_data_block()) != (bs == 1024) as u64 {
            "a first data block that is not the block size's"
        } else if !sb.checksum_ok() {
            "a superblock that fails its checksum"
        } else {
            return true;
        };
        trace!(0, "ext4: refused: {}", why);
        false
    }

    fn desc(&self, group: u64) -> Option<Desc<'_>> {
        let at = (group as usize).checked_mul(self.desc_size)?;
        Desc::new(self.gdt.get(at..at + self.desc_size)?)
    }

    /// The descriptors, read and held to their checksums and to the
    /// filesystem: every group's bitmaps and table inside it.
    fn read_descriptors(&mut self, sb: &Super) -> bool {
        let bs = self.io.bs;
        let bytes = (self.groups as usize).saturating_mul(self.desc_size);
        let gdt_blocks = bytes.div_ceil(bs);
        let mut gdt = Vec::new();
        if gdt.try_reserve_exact(gdt_blocks * bs).is_err() {
            trace!(0, "ext4: no memory for {} bytes of descriptors", gdt_blocks * bs);
            return false;
        }
        gdt.resize(gdt_blocks * bs, 0);
        let first = u64::from(sb.first_data_block()) + 1;
        for k in 0..gdt_blocks {
            let mut blk = vec![0u8; bs];
            if !self.block(first + k as u64, &mut blk) {
                return false;
            }
            gdt[k * bs..(k + 1) * bs].copy_from_slice(&blk);
        }
        self.gdt = gdt;
        let table_blocks = u64::from(sb.inodes_per_group()) * sb.inode_size() / bs as u64;
        for g in 0..self.groups {
            let Some(d) = self.desc(g) else { return false };
            if let Some(c) = group::desc_csum(sb, g as u32, d.raw()) {
                if c != d.checksum() {
                    trace!(0, "ext4: refused: group {}'s descriptor fails its checksum", g);
                    return false;
                }
            }
            let inside = |b: u64, n: u64| b > u64::from(sb.first_data_block()) && b.checked_add(n).is_some_and(|e| e <= self.io.blocks);
            if !inside(d.block_bitmap(), 1) || !inside(d.inode_bitmap(), 1) || !inside(d.inode_table(), table_blocks) {
                trace!(0, "ext4: refused: group {}'s bitmaps or table outside the filesystem", g);
                return false;
            }
        }
        true
    }

    pub fn mount(&mut self, read_only: bool) -> bool {
        let _ = read_only;
        let Some(sb) = read_super(&self.io.dev, &mut self.io.scratch) else { return false };
        match features::classify(&sb) {
            Support::Refused(why) => {
                trace!(0, "ext4: refused: {}", why);
                return false;
            }
            Support::ReadOnly(why) => trace!(0, "ext4: read-only: {}", why),
            Support::Writable => {}
        }
        if !self.check_super(&sb) {
            return false;
        }
        self.io.bs = sb.block_size() as usize;
        self.io.blocks = sb.blocks_count();
        self.seed = sb.csum_seed();
        self.csum = sb.metadata_csum();
        self.uninit = self.csum || sb.ro_compat() & ro_compat::GDT_CSUM != 0;
        self.groups = sb.group_count();
        self.desc_size = sb.desc_size() as usize;
        self.inode_size = sb.inode_size() as usize;
        self.ipg = sb.inodes_per_group();
        self.overlay.clear();
        self.itab = None;
        if !self.read_descriptors(&sb) {
            return false;
        }
        self.sb = Some(sb.clone());
        if sb.compat() & features::compat::HAS_JOURNAL != 0 {
            match journal::overlay(self, &sb) {
                Ok(o) => self.overlay = o,
                Err(e) => {
                    trace!(0, "ext4: refused: the journal: {}", e);
                    return false;
                }
            }
            /* The superblock and descriptors as the journal has them, if it
             * has newer copies: as a replay would have left them. */
            let first = u64::from(sb.first_data_block());
            let gdt_blocks = (self.groups as usize * self.desc_size).div_ceil(self.io.bs) as u64;
            if (first..=first + gdt_blocks).any(|b| self.overlay.contains_key(&b)) {
                let mut blk = vec![0u8; self.io.bs];
                let at = extfmt::sb::OFFSET as usize % self.io.bs;
                if !self.block(first, &mut blk) {
                    return false;
                }
                let newer = match Super::parse(&blk[at..at + extfmt::sb::SIZE]) {
                    Ok(s) if s.blocks_count() == sb.blocks_count() && s.inodes_count() == sb.inodes_count()
                        && s.block_size() == sb.block_size() && s.checksum_ok() => s,
                    _ => {
                        trace!(0, "ext4: refused: the journal's superblock is not this filesystem's");
                        return false;
                    }
                };
                self.itab = None;
                if !self.read_descriptors(&newer) {
                    return false;
                }
                self.sb = Some(newer);
            }
        }
        let Some(raw) = self.inode(ROOT_INO) else { return false };
        if Inode::new(&raw).is_none_or(|i| !i.is_dir()) {
            trace!(0, "ext4: refused: the root is not a directory");
            return false;
        }
        self.tree.clear();
        self.root = self.new_vnode(None, b"", Kind::Dir, ROOT_INO, 0);
        if self.root.is_none() {
            return false;
        }
        self.mounted = true;
        true
    }

    pub fn unmount(&mut self) {
        self.tree.clear();
        self.root = None;
        self.overlay.clear();
        self.gdt = Vec::new();
        self.itab = None;
        self.mounted = false;
    }

    /// Inode `ino`'s bytes: None for one outside the filesystem, in a part
    /// of an inode table never written, or failing its checksum -- each
    /// said.
    fn inode(&mut self, ino: u32) -> Option<Vec<u8>> {
        let count = self.sb.as_ref()?.inodes_count();
        if ino == 0 || ino > count {
            trace!(0, "ext4: inode {} of {}", ino, count);
            return None;
        }
        let (group, index) = (u64::from((ino - 1) / self.ipg), (ino - 1) % self.ipg);
        let (flags, unused, table) = self.desc(group).map(|d| (d.flags(), d.itable_unused(), d.inode_table()))?;
        if self.uninit && (flags & INODE_UNINIT != 0 || index >= self.ipg.saturating_sub(unused)) {
            trace!(0, "ext4: inode {} is in a part of its group's table never written", ino);
            return None;
        }
        let at = u64::from(index) * self.inode_size as u64;
        let block = table + at / self.io.bs as u64;
        let within = (at % self.io.bs as u64) as usize;
        if self.itab.as_ref().is_none_or(|(b, _)| *b != block) {
            let mut blk = vec![0u8; self.io.bs];
            if !self.block(block, &mut blk) {
                return None;
            }
            self.itab = Some((block, blk));
        }
        let raw = self.itab.as_ref()?.1.get(within..within + self.inode_size)?.to_vec();
        let zero = raw[..xinode::GOOD_OLD_SIZE].iter().all(|&b| b == 0);
        if self.csum && !zero && !xinode::csum_matches(self.seed, ino, &raw) {
            trace!(0, "ext4: inode {} fails its checksum", ino);
            return None;
        }
        Some(raw)
    }

    fn largedir(&self) -> bool {
        self.sb.as_ref().is_some_and(|s| s.largedir())
    }

    /// Where block `lblock` of the file inode `ino` (`raw`) is: through its
    /// extent tree, every node of it held to its checksum, or its block map.
    fn map(&mut self, ino: u32, raw: &[u8], lblock: u64) -> Option<Map> {
        let node = Inode::new(raw)?;
        let iseed = xinode::seed(self.seed, ino, node.generation());
        let (fs_blocks, bs, csum) = (self.io.blocks, self.io.bs, self.csum);
        let extents = node.flags() & EXTENTS_FL != 0;
        let mut read = |b: u64, buf: &mut [u8]| {
            if !self.block(b, buf) {
                return Err(alloc::format!("block {} does not read", b));
            }
            if csum && extents && extent::stored_csum(buf).zip(extent::node_csum(iseed, buf)).is_none_or(|(s, c)| s != c) {
                return Err(alloc::format!("inode {}'s extent block {} fails its checksum", ino, b));
            }
            Ok(())
        };
        let mapped = if extents {
            extent::map(node.i_block(), lblock, fs_blocks, bs, ino, &mut read)
        } else {
            blockmap::map(node.i_block(), lblock, bs, ino, &mut read)
        };
        match mapped {
            Ok(Map::Blocks { block, run }) if block.checked_add(run).is_none_or(|e| e > fs_blocks) => {
                trace!(0, "ext4: inode {}: blocks {}..+{} past the filesystem", ino, block, run);
                None
            }
            Ok(m) => Some(m),
            Err(e) => {
                trace!(0, "ext4: {}", e);
                None
            }
        }
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
        let iseed = xinode::seed(self.seed, ino, node.generation());
        let indexed = node.flags() & INDEX_FL != 0;
        let bs = self.io.bs as u64;
        let blocks = node.size(self.largedir()).div_ceil(bs);
        let mut lblock = 0u64;
        let mut blk = vec![0u8; self.io.bs];
        while lblock < blocks {
            match self.map(ino, &raw, lblock) {
                None => return false,
                Some(Map::Hole { run }) => lblock = lblock.saturating_add(run.max(1)),
                Some(Map::Blocks { block, run }) => {
                    for k in 0..run.min(blocks - lblock) {
                        if !self.block(block + k, &mut blk) || !self.dir_block(dir, ino, iseed, indexed, lblock + k, &blk) {
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
        if self.csum {
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
                continue;
            }
            if !self.adopt(dir, &name, child, file_type) {
                return false;
            }
        }
        true
    }

    /// A vnode for one entry, if it names a file or a directory: what else
    /// it names, and an inode that does not read, are left out (the latter
    /// said). False only when there is no memory for it.
    fn adopt(&mut self, dir: NodeId, name: &[u8], ino: u32, file_type: u8) -> bool {
        if file_type != dir::FT_REG_FILE && file_type != dir::FT_DIR && file_type != dir::FT_UNKNOWN {
            return true;
        }
        let Some(raw) = self.inode(ino) else {
            trace!(0, "ext4: an entry of directory {} names inode {}, which does not read: skipped",
                self.tree[dir].ino, ino);
            return true;
        };
        let Some(node) = Inode::new(&raw) else { return true };
        let kind = match node.file_type() {
            S_IFDIR => Kind::Dir,
            S_IFREG => Kind::File,
            _ => return true,
        };
        if kind == Kind::Dir {
            /* An entry leading back up the tree is a cycle in the image. */
            let mut up = Some(dir);
            while let Some(at) = up {
                if self.tree[at].ino == ino as usize {
                    trace!(0, "ext4: an entry of inode {} is an ancestor, skipped", self.tree[dir].ino);
                    return true;
                }
                up = self.tree[at].parent();
            }
        }
        let size = usize::try_from(node.size(self.largedir())).unwrap_or(usize::MAX);
        self.new_vnode(Some(dir), name, kind, ino, size).is_some()
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
        while pos < end {
            let (lblock, within) = (pos / bs, (pos % bs) as usize);
            let at = (pos - offset as u64) as usize;
            match self.map(ino, &raw, lblock) {
                None => return false,
                Some(Map::Hole { run }) => {
                    let n = run.saturating_mul(bs).saturating_sub(within as u64).min(end - pos) as usize;
                    buf[at..at + n].fill(0);
                    pos += n as u64;
                }
                Some(Map::Blocks { block, .. }) => {
                    if !self.block(block, &mut blk) {
                        return false;
                    }
                    let n = (self.io.bs - within).min((end - pos) as usize);
                    buf[at..at + n].copy_from_slice(&blk[within..within + n]);
                    pos += n as u64;
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
        if let Some(sb) = &self.sb {
            let label = sb.volume_name();
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
    }

    fn device(&self) -> usize {
        self.io.dev.handle()
    }

    /// Read-only, whatever was asked: this driver does not write yet.
    fn mount(&mut self, read_only: bool) -> Option<bool> {
        if Ext4::mount(self, read_only) { Some(true) } else { None }
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

    fn create_file(&mut self, _dir: NodeId, _name: &[u8]) -> Option<NodeId> {
        None
    }

    fn create_dir(&mut self, _dir: NodeId, _name: &[u8]) -> Option<NodeId> {
        None
    }

    fn read(&mut self, file: NodeId, buf: &mut [u8], offset: usize) -> bool {
        Ext4::read(self, file, buf, offset)
    }

    fn write(&mut self, _file: NodeId, _data: &[u8], _offset: usize) -> bool {
        false
    }

    fn truncate(&mut self, _file: NodeId, _size: usize) -> bool {
        false
    }

    fn rename(&mut self, _node: NodeId, _dir: NodeId, _name: &[u8]) -> bool {
        false
    }

    fn remove(&mut self, _node: NodeId) -> bool {
        false
    }

    /// What extents number: 2^32 blocks.
    fn sparse_max(&self) -> usize {
        if self.mounted { usize::try_from((1u64 << 32) * self.io.bs as u64).unwrap_or(usize::MAX) } else { 0 }
    }

    /// The filesystem's blocks, read off the VFS's lock by an image -- but
    /// not while a journal holds newer copies the device does not.
    fn data_block_size(&self) -> usize {
        if self.mounted && self.overlay.is_empty() { self.io.bs } else { 0 }
    }

    fn map_blocks(&mut self, file: NodeId, first: u64, out: &mut [u64]) -> bool {
        Ext4::map_blocks(self, file, first, out)
    }
}

/// Mount the device's ext4 at `path`: 1 when mounted (read-only, all this
/// driver does yet), -1 when not.
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
