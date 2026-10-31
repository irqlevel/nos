//! A disk's image: a file of nos's that a guest's disk is, held for as long
//! as the disk is open and read and written off the VFS's lock.
//!
//! The VFS holds a mount's lock across every call on its filesystem, down to
//! the device and back (`vfs`): a guest's disk read and written through it
//! waits behind every other guest's disk on that filesystem and behind nos's
//! own files there, a request at a time. An image takes the file's map
//! from the filesystem once, as it is opened (`Vfs::map_runs`), and holds
//! it (`Map`): every read and write after it is resolved there, no lock
//! taken, and goes to the device straight through a buffer of its own, in
//! batches the driver has in flight together.
//!
//! What makes that safe is the hold (`vfs::OPEN_HOLD`): while an image is
//! open nothing else may write the file, cut it, grow it, rename it or
//! remove it, so the blocks the map names stay the file's -- and nobody else
//! writes them -- until the image is closed, and nothing but the image
//! changes the map. What only the filesystem can do is left to it: a write
//! into a hole needs blocks allocated and pointed at in order, so a batch
//! with a hole in it goes through the VFS as every write did -- the blocks
//! it touched taken into the map again after it, as a punch's are -- and the
//! next flush is then the filesystem's sync and not only the device's. A filesystem with no blocks of its own on a device
//! (`FileSystem::data_block_size` 0 -- ramfs) has its images read and
//! written through the VFS whole.
//!
//! A partial block -- a sector of a guest's in a filesystem block of 4 KiB
//! -- is read, changed and written back whole. That is safe because an
//! image is one writer's: its calls come one at a time (`&mut self`, and the
//! slot's lock across the C ABI), and the hold keeps everyone else off the
//! file.
//!
//! A write into a hole grows the file by the chunk around it first
//! (`Vfs::allocate`, `CHUNK_BYTES`): its blocks made zeros on the device and
//! given to the file in one commit, and the write then goes to the device
//! as an overwrite -- a commit a chunk, where a write through the VFS was a
//! commit a batch. A guest's discard gives whole chunks back to the
//! filesystem (`discard`, `Vfs::punch`): holes from then on, as a write of
//! zeros with its unmap flag does with the chunks it covers whole
//! (`write_zeroes`), the rest of it zeros written. Blocks given
//! back may be the next file's the moment the punch is done -- which is safe
//! because, the image's calls coming one at a time, no read of its own is
//! under way off the VFS's lock on a block it mapped before them.
//!
//! A loadable module reaches one through the C ABI at the bottom -- an image
//! is a slot of this file's table and the generation of what is in it, looked
//! up on every call (`kcore::fs::Image`).

use alloc::boxed::Box;
use alloc::vec::Vec;

use block::{Disk, Piece};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::trace;

use crate::vfs::{Handle, Vfs, OPEN_HOLD, OPEN_READ, OPEN_WRITE};
use ffi::fs::{ImageIo, IMAGE_VEC_MAX};

/// The most blocks a batch moves: a page of the buffer each -- 2 MiB, so
/// that every request a guest's disk has out at once (eight, of up to 256
/// KiB) goes to the device in one batch (`read_vec`, `write_vec`).
const BATCH_PAGES: usize = 512;

/// The most blocks `extent` looks at in one call: 16 MiB of 4 KiB blocks, a
/// map of a batch's at a time under the VFS's lock -- let go of between
/// them, and the caller's to stop between calls.
const EXTENT_BLOCKS: u64 = 4096;

/// The most blocks the map asks the filesystem about in one call: 16 GiB of
/// 4 KiB blocks -- a hole is one run however long, so a sparse image of
/// terabytes is a few calls, each under the mount's lock for no longer than
/// the runs it has take.
const MAP_WINDOW: u64 = 1 << 22;

/// What a write into a hole grows an image by (`Vfs::allocate`): the 16
/// MiB around it, made zeros and given to the file in one commit rather
/// than a commit for every batch the guest writes into it -- and what a
/// guest's discard gives back whole, the rest of its range left as it is.
pub const CHUNK_BYTES: u64 = 16 << 20;

/// What zeros are written from, a piece at a time (`write_zeroes`).
const ZERO_CHUNK: usize = 16 * 1024;
static ZEROS: [u8; ZERO_CHUNK] = [0; ZERO_CHUNK];

/// Blocks of the file's that follow one another on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Its first block, in the file
    first: u64,
    len: u64,
    /// Where its first block begins on the device
    sector: u64,
}

/// Where the file's blocks are on the device, as the image holds it: the
/// runs of blocks the file has, sorted and apart -- a hole is what no run
/// covers. Taken from the filesystem when the image is opened, a window at a
/// time (`Vfs::map_runs`), and changed by nothing but the image's own writes
/// into holes and its punches, the blocks each touched taken again from the
/// filesystem after it: the hold keeps everyone else off the file, and a
/// filesystem moves no file's blocks. So a batch is resolved here, with no
/// lock taken, where it was asked of the VFS under its mount's -- and a
/// guest's disk waits for no other guest's on the same filesystem, nor for
/// nos's own files there. A map that cannot be taken, or taken again, is
/// dropped, and the image asks the VFS for every batch as it did.
struct Map {
    runs: Vec<Run>,
    /// The device's sectors in a block
    per_block: u64,
}

impl Map {
    /// The map of the `blocks` blocks of the file `handle` holds, or None:
    /// an I/O error, or no memory for it.
    fn load(vfs: &Vfs, handle: Handle, blocks: u64, per_block: u64) -> Option<Map> {
        let mut map = Map { runs: Vec::new(), per_block };
        map.retake(vfs, handle, 0, blocks).then_some(map)
    }

    /// The first run that ends past block `b`.
    fn index(&self, b: u64) -> usize {
        self.runs.partition_point(|r| r.first + r.len <= b)
    }

    /// Where each of `out.len()` blocks from `first` begins on the device,
    /// 0 for a hole: what `Vfs::map` says of them.
    fn fill(&self, first: u64, out: &mut [u64]) {
        let mut i = self.index(first);
        for (k, slot) in out.iter_mut().enumerate() {
            let b = first + k as u64;
            while self.runs.get(i).is_some_and(|r| r.first + r.len <= b) {
                i += 1;
            }
            *slot = match self.runs.get(i) {
                Some(r) if r.first <= b => r.sector + (b - r.first) * self.per_block,
                _ => 0,
            };
        }
    }

    /// Block `b` the first of a run, where a run has it in its middle:
    /// false when there is no memory for the second half.
    fn split(&mut self, b: u64) -> bool {
        let i = self.index(b);
        let Some(r) = self.runs.get(i).copied() else { return true };
        if r.first >= b {
            return true;
        }
        if self.runs.try_reserve(1).is_err() {
            return false;
        }
        let left = b - r.first;
        self.runs[i].len = left;
        self.runs.insert(i + 1, Run { first: b, len: r.len - left, sector: r.sector + left * self.per_block });
        true
    }

    /// The runs at `i - 1` and `i` made one, where the second carries on
    /// where the first ends, in the file and on the device.
    fn join(&mut self, i: usize) {
        if i == 0 || i >= self.runs.len() {
            return;
        }
        let (a, b) = (self.runs[i - 1], self.runs[i]);
        if a.first + a.len == b.first && a.sector + a.len * self.per_block == b.sector {
            self.runs[i - 1].len += b.len;
            self.runs.remove(i);
        }
    }

    /// Blocks `first..first + count` taken again from the filesystem: what
    /// the map had of them dropped, and what the filesystem says of them now
    /// put in their place, a window at a time. false -- and the map no
    /// longer to be trusted -- on an I/O error, runs that do not add up to
    /// what was asked, or no memory.
    fn retake(&mut self, vfs: &Vfs, handle: Handle, first: u64, count: u64) -> bool {
        let Some(end) = first.checked_add(count) else { return false };
        if !self.split(first) || !self.split(end) {
            return false;
        }
        let (from, to) = (self.index(first), self.index(end));
        self.runs.drain(from..to);
        let mut at_run = from;
        let mut got = Vec::new();
        let mut at = first;
        while at < end {
            let n = (end - at).min(MAP_WINDOW);
            got.clear();
            if !vfs.map_runs(handle, at, n, &mut got) {
                return false;
            }
            let mut b = at;
            for &(sector, len) in &got {
                if len == 0 || len > at + n - b {
                    trace!(0, "image: the filesystem's map of {} blocks from {} does not add up", n, at);
                    return false;
                }
                if sector != 0 {
                    let carries_on = at_run > 0 && {
                        let prev = self.runs[at_run - 1];
                        prev.first + prev.len == b && prev.sector + prev.len * self.per_block == sector
                    };
                    if carries_on {
                        self.runs[at_run - 1].len += len;
                    } else {
                        if self.runs.try_reserve(1).is_err() {
                            return false;
                        }
                        self.runs.insert(at_run, Run { first: b, len, sector });
                        at_run += 1;
                    }
                }
                b += len;
            }
            if b != at + n {
                trace!(0, "image: the filesystem's map of {} blocks from {} does not add up", n, at);
                return false;
            }
            at += n;
        }
        self.join(at_run);
        true
    }
}

/// A request's part in a batch: which request, where in its bytes and how
/// many, the batch's first page it takes -- a page a block -- where in that
/// block it starts, and how many blocks it takes.
#[derive(Clone, Copy)]
struct Stretch {
    req: usize,
    from: usize,
    len: usize,
    page: usize,
    skip: usize,
    blocks: usize,
}

/// What an image reads and writes off the VFS's lock with: the device its
/// file's blocks are on, and a page of the buffer for each block of a batch.
struct Direct {
    disk: Disk,
    /// A block of the file's, in bytes: a page or a whole fraction of one.
    block: usize,
    batch: DmaBuffer,
    /// Where each block of the batch is on the device, 0 for a hole.
    sectors: Vec<u64>,
    pieces: Vec<Piece>,
    /// The requests' parts the batch is made of.
    stretches: Vec<Stretch>,
    /// The file's map as the image holds it; None once it could not be taken.
    map: Option<Map>,
    /// What a write into a hole grows the file by, and a discard gives back
    /// whole, in blocks: `CHUNK_BYTES`, unless told otherwise.
    chunk: u64,
    /// A write went through the VFS since the last sync -- blocks were
    /// allocated, the filesystem's own metadata changed -- so the next sync
    /// is the filesystem's, not only the device's.
    through_vfs: bool,
    /// Batches read and written off the VFS's lock, and written through it:
    /// what the image says as it goes.
    read_off: u64,
    written_off: u64,
    written_through: u64,
    /// Blocks given back by discards, and given to the file ahead of writes.
    punched: u64,
    allocated: u64,
    /// Whether its filesystem gives blocks ahead of writes at all
    /// (`Vfs::allocates`): where it does not, a write into a hole goes
    /// through the VFS without asking it for a chunk first.
    grows: bool,
    /// Chunks its filesystem refused, though it gives blocks ahead of writes
    /// -- each such write going through the VFS, as every one into a hole did
    /// before chunks: said at the first, and counted.
    refused: u64,
}

pub struct Image {
    vfs: &'static Vfs,
    handle: Handle,
    /// The file's path, as it was given -- any bytes the VFS takes in a
    /// name -- for what the image says as it goes.
    path: Vec<u8>,
    size: u64,
    writable: bool,
    /// The file's mount and which file it is: what a claim on it is keyed by.
    identity: (u64, u64),
    /// None: its filesystem keeps no blocks of its own on a device, and
    /// everything goes through the VFS.
    direct: Option<Direct>,
}

impl Image {
    /// The regular file at `path`, held as a disk's image, for reading -- and
    /// with `write` for writing too: None when there is none, it cannot be
    /// held so (someone else writes it, or holds it for writing; a read-only
    /// mount), or there is no memory for its buffer.
    pub fn open(vfs: &'static Vfs, path: &[u8], write: bool) -> Option<Image> {
        let flags = OPEN_READ | OPEN_HOLD | if write { OPEN_WRITE } else { 0 };
        let mut name = Vec::new();
        name.try_reserve_exact(path.len()).ok()?;
        name.extend_from_slice(path);
        let handle = vfs.open(path, flags)?;
        /* Closed by its drop from here on, whatever is refused below. */
        let mut image = Image { vfs, handle, path: name, size: 0, writable: write, identity: (0, 0), direct: None };
        image.size = vfs.length(handle)? as u64;
        image.identity = vfs.identity(handle)?;
        if let Some((device, block)) = vfs.geometry(handle) {
            /* Blocks no page holds whole, or no whole number of the device's
             * sectors: through the VFS, as a filesystem with none. */
            if let Some(disk) = Direct::fits(device, block) {
                let mut direct = Direct::new(disk, block)?;
                direct.grows = write && vfs.allocates(handle);
                let per_block = block as u64 / direct.disk.sector_size();
                direct.map = Map::load(vfs, handle, image.size.div_ceil(block as u64), per_block);
                if direct.map.is_none() {
                    trace!(1, "image: {}: no map of its own -- every batch asks the VFS",
                           core::str::from_utf8(path).unwrap_or("(a path that is not UTF-8)"));
                }
                image.direct = Some(direct);
            }
        }
        Some(image)
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The VFS's handle it holds the file by: what tells it apart from the
    /// file's other opens, to whoever looks at them all (the fuzzer).
    pub fn handle(&self) -> Handle {
        self.handle
    }

    /// The file's mount and which file on it this is (`Vfs::identity`).
    pub fn identity(&self) -> (u64, u64) {
        self.identity
    }

    /// Whether a discard gives blocks back -- its filesystem keeps blocks of
    /// its own on a device -- or changes nothing.
    pub fn gives_back(&self) -> bool {
        self.direct.is_some()
    }

    /// Where the image has data from `offset` on, as `(start, end)`: holes
    /// from `offset` to `start`, and from `start` to `end` blocks that are
    /// on the device -- `start` is `offset` unless a hole was there, `end`
    /// where the next hole begins, at most `EXTENT_BLOCKS` blocks on from
    /// `offset`'s and never past the image's end. `start == end` where none
    /// of the blocks looked at holds data: holes up to there, and the next
    /// call goes on from it. So a copy that leaves the holes out -- a VM's
    /// disk cloned -- reads only what is there, and walks a sparse disk of
    /// gigabytes a call at a time, free to stop between them. What a hole
    /// is, is the filesystem's map (`Vfs::map`): a block it reads as zeros
    /// without one. Where its filesystem keeps no blocks of its own, every
    /// byte is data. `(size, size)` from the end on; None on an I/O error.
    pub fn extent(&mut self, offset: u64) -> Option<(u64, u64)> {
        let size = self.size;
        if offset >= size {
            return Some((size, size));
        }
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else { return Some((offset, size)) };
        let block = direct.block as u64;
        let first = offset / block;
        let last = size.div_ceil(block).min(first.saturating_add(EXTENT_BLOCKS));
        /* Byte offsets of blocks inside the image: at most `size`, however
         * near the top of a u64 a damaged length puts it. */
        let at_block = |b: u64| b.checked_mul(block).map_or(size, |at| at.min(size));
        let mut data_from = None;
        let mut at = first;
        while at < last {
            let n = usize::try_from(last - at).map_or(direct.sectors.len(), |n| n.min(direct.sectors.len()));
            if !direct.resolve(vfs, handle, at, 0, n) {
                return None;
            }
            for (k, &sector) in direct.sectors[..n].iter().enumerate() {
                let b = at + k as u64;
                match (data_from, sector != 0) {
                    (None, true) => data_from = Some(b),
                    (Some(from), false) => return Some((at_block(from).max(offset), at_block(b))),
                    _ => {}
                }
            }
            at += n as u64;
        }
        let end = at_block(last);
        Some(match data_from {
            Some(from) => (at_block(from).max(offset), end),
            None => (end, end),
        })
    }

    /// Whether the map the image holds is what the filesystem says of every
    /// one of its blocks now -- taken afresh, as runs, and held to it -- or
    /// it holds none, and asks. For whoever holds the image to its
    /// filesystem (the fuzzer).
    pub fn map_is_the_filesystems(&mut self) -> bool {
        let (vfs, handle, size) = (self.vfs, self.handle, self.size);
        let Some(Direct { map: Some(map), block, .. }) = self.direct.as_ref() else { return true };
        match Map::load(vfs, handle, size.div_ceil(*block as u64), map.per_block) {
            Some(now) if now.runs == map.runs => true,
            Some(_) => {
                trace!(0, "image: its map is not its filesystem's");
                false
            }
            None => false,
        }
    }

    /// Whether `len` bytes from `offset` are all inside the image.
    fn inside(&self, offset: u64, len: usize) -> bool {
        self.inside_len(offset, len as u64)
    }

    fn inside_len(&self, offset: u64, len: u64) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.size)
    }

    /// Fills `buf` from `offset`, all of it inside the image: false on an
    /// I/O error, or a range that is not.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> bool {
        self.read_vec(&mut [(offset, buf)])
    }

    /// Writes `data` at `offset`, all of it inside the image -- which never
    /// grows: false on an I/O error, a range that is not, or an image held
    /// for reading. Not synced: `sync` is.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> bool {
        self.write_vec(&[(offset, data)])
    }

    /// Fills each buffer of `ios` from its offset, all of them inside the
    /// image: the blocks of as many as the batch holds read in one go, so
    /// that the requests a guest's disk has out at once are with the device
    /// at once -- a request's blocks that are holes zeros. False on an I/O
    /// error, or a range that is not inside, by when some may be filled.
    pub fn read_vec(&mut self, ios: &mut [(u64, &mut [u8])]) -> bool {
        if ios.iter().any(|(at, buf)| !self.inside(*at, buf.len())) {
            return false;
        }
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else {
            return ios.iter_mut().all(|(at, buf)| {
                usize::try_from(*at).ok().and_then(|at| vfs.read_at(handle, at, buf)) == Some(buf.len())
            });
        };

        let block = direct.block;
        let pages = direct.sectors.len();
        let (mut req, mut done) = (0usize, 0usize);
        while req < ios.len() {
            /* A batch: the next parts of as many requests as its pages hold */
            direct.stretches.clear();
            let mut used = 0;
            while req < ios.len() && used < pages {
                let (offset, len) = (ios[req].0, ios[req].1.len());
                if done == len {
                    req += 1;
                    done = 0;
                    continue;
                }
                let at = offset + done as u64;
                let skip = (at % block as u64) as usize;
                let blocks = (skip + len - done).div_ceil(block).min(pages - used);
                let take = (blocks * block - skip).min(len - done);
                if !direct.resolve(vfs, handle, at / block as u64, used, blocks) {
                    return false;
                }
                direct.stretches.push(Stretch { req, from: done, len: take, page: used, skip, blocks });
                used += blocks;
                done += take;
            }

            /* The blocks that are there, a page of the buffer each, read in
             * one go -- with no lock held */
            direct.pieces.clear();
            for (k, &sector) in direct.sectors[..used].iter().enumerate() {
                if sector != 0 {
                    direct.pieces.push(Piece { sector, at: k * PAGE_SIZE, len: block });
                }
            }
            if direct.disk.read_pieces(&mut direct.batch, &direct.pieces).is_err() {
                trace!(0, "image: a read of {} blocks failed", direct.pieces.len());
                return false;
            }
            direct.read_off += 1;

            for st in &direct.stretches {
                let buf = &mut ios[st.req].1[st.from..st.from + st.len];
                let (mut off, mut skip, mut k) = (0, st.skip, st.page);
                while off < st.len {
                    let chunk = (block - skip).min(st.len - off);
                    let to = &mut buf[off..off + chunk];
                    if direct.sectors[k] != 0 {
                        let from = k * PAGE_SIZE + skip;
                        to.copy_from_slice(&direct.batch.as_slice()[from..from + chunk]);
                    } else {
                        /* A hole reads as zeros */
                        to.fill(0);
                    }
                    off += chunk;
                    skip = 0;
                    k += 1;
                }
            }
        }
        true
    }

    /// Writes each of `ios` at its offset, all of them inside the image --
    /// which never grows: as many as the batch holds in one go, as
    /// `read_vec` reads them, a hole's chunk given its blocks first (`grow`).
    /// A block two of them write -- two sectors of one block, which the
    /// first and the last block of each are read for and written back whole
    /// -- is never in one batch twice: the second waits for the next batch,
    /// so both land. A part of one into a hole that cannot be given blocks
    /// goes through the VFS, after the batch before it. False on an I/O
    /// error, a range that is not inside, or an image held for reading, by
    /// when some may be written. Not synced: `sync` is.
    pub fn write_vec(&mut self, ios: &[(u64, &[u8])]) -> bool {
        if !self.writable || ios.iter().any(|(at, data)| !self.inside(*at, data.len())) {
            return false;
        }
        let (vfs, handle, size) = (self.vfs, self.handle, self.size);
        let Some(direct) = self.direct.as_mut() else {
            return ios.iter().all(|(at, data)| usize::try_from(*at).is_ok_and(|at| vfs.write_within(handle, at, data)));
        };

        let block = direct.block;
        let pages = direct.sectors.len();
        let file_blocks = size.div_ceil(block as u64);
        let (mut req, mut done) = (0usize, 0usize);
        while req < ios.len() {
            direct.stretches.clear();
            let mut used = 0;
            while req < ios.len() && used < pages {
                let (offset, data) = ios[req];
                if done == data.len() {
                    req += 1;
                    done = 0;
                    continue;
                }
                let at = offset + done as u64;
                let skip = (at % block as u64) as usize;
                let blocks = (skip + data.len() - done).div_ceil(block).min(pages - used);
                /* What of the request this part takes: to the end of its blocks */
                let take = (blocks * block - skip).min(data.len() - done);
                let first = at / block as u64;
                if !direct.resolve(vfs, handle, first, used, blocks) {
                    return false;
                }
                if direct.grows && direct.sectors[used..used + blocks].contains(&0)
                    && direct.grow(vfs, handle, first, blocks as u64, file_blocks)
                    && !direct.resolve(vfs, handle, first, used, blocks) {
                    return false;
                }
                let (batch, part) = direct.sectors.split_at(used);
                let part = &part[..blocks];
                if used > 0 && (part.contains(&0) || part.iter().any(|s| batch.contains(s))) {
                    /* The batch so far first: this part writes a block it
                     * has, or goes through the VFS after it */
                    break;
                }
                if part.contains(&0) {
                    /* A hole still -- a filesystem that gives no blocks ahead
                     * of a write, or none left to give: blocks only it can
                     * allocate and point at, in order, so the part through
                     * it, as every write went before */
                    direct.through_vfs = true;
                    let Ok(to) = usize::try_from(at) else { return false };
                    let written = vfs.write_within(handle, to, &data[done..done + take]);
                    /* Blocks given, as far as the write got: the map's from now */
                    direct.retake(vfs, handle, first, blocks as u64);
                    if !written {
                        return false;
                    }
                    direct.written_through += 1;
                    done += take;
                    continue;
                }
                direct.stretches.push(Stretch { req, from: done, len: take, page: used, skip, blocks });
                used += blocks;
                done += take;
            }
            if used == 0 {
                continue;
            }

            /* The first block and the last of each part, where it takes only
             * part of them: what the rest of each holds, read first */
            direct.pieces.clear();
            for st in &direct.stretches {
                let end = st.skip + st.len;
                if st.skip != 0 {
                    direct.pieces.push(Piece { sector: direct.sectors[st.page], at: st.page * PAGE_SIZE, len: block });
                }
                if end % block != 0 && (st.blocks > 1 || st.skip == 0) {
                    let last = st.page + st.blocks - 1;
                    direct.pieces.push(Piece { sector: direct.sectors[last], at: last * PAGE_SIZE, len: block });
                }
            }
            if direct.disk.read_pieces(&mut direct.batch, &direct.pieces).is_err() {
                trace!(0, "image: a read of the ends of a write failed");
                return false;
            }

            for st in &direct.stretches {
                let data = &ios[st.req].1[st.from..st.from + st.len];
                let (mut off, mut skip) = (0, st.skip);
                for k in st.page..st.page + st.blocks {
                    let chunk = (block - skip).min(st.len - off);
                    let to = k * PAGE_SIZE + skip;
                    direct.batch.as_mut_slice()[to..to + chunk].copy_from_slice(&data[off..off + chunk]);
                    off += chunk;
                    skip = 0;
                }
            }

            direct.pieces.clear();
            for (k, &sector) in direct.sectors[..used].iter().enumerate() {
                direct.pieces.push(Piece { sector, at: k * PAGE_SIZE, len: block });
            }
            if direct.disk.write_pieces(&direct.batch, &direct.pieces).is_err() {
                trace!(0, "image: a write of {} blocks failed", used);
                return false;
            }
            direct.written_off += 1;
        }
        true
    }

    /// How many chunks the image's filesystem refused ahead of a write into
    /// a hole, though it gives blocks so: what its fuzzer holds to its
    /// having had a reason -- no room, a request that failed.
    pub fn chunks_refused(&self) -> u64 {
        self.direct.as_ref().map_or(0, |d| d.refused)
    }

    /// What a discard gives back whole, in bytes -- a chunk -- or 0 where
    /// it gives back nothing (`gives_back`).
    pub fn discard_unit(&self) -> u64 {
        self.direct.as_ref().map_or(0, |d| d.chunk * d.block as u64)
    }

    /// The image's chunk -- what a write into a hole grows it by, and what a
    /// discard gives back whole -- made `blocks` blocks, at least one: for
    /// whoever wants another than `CHUNK_BYTES` (the fuzzer).
    pub fn set_chunk(&mut self, blocks: u64) {
        if let Some(direct) = self.direct.as_mut() {
            direct.chunk = blocks.max(1);
        }
    }

    /// The whole chunks inside `len` bytes from `offset` given back to the
    /// filesystem -- holes from here on, reading as zeros, the image's size
    /// as it was: a guest's discard. The rest of the range is left as it is,
    /// as a disk may leave what it is told it may forget: its blocks stay
    /// the file's, holding the guest's own data, and the next write there
    /// needs nothing allocated -- so a guest that discards as it frees, as
    /// ext4 mounted with `discard` does, costs the filesystem a commit for
    /// every chunk it gives back whole rather than for every range. False
    /// for a range not inside the image, an image held for reading, or an
    /// I/O error. On a filesystem with no blocks of its own there is
    /// nothing to give back, and that is done. What the filesystem changed
    /// of its own metadata is its own to put on the medium -- a journal's
    /// commit may come later -- so the next sync is the filesystem's, as
    /// after a write through it.
    pub fn discard(&mut self, offset: u64, len: u64) -> bool {
        if !self.writable || !self.inside_len(offset, len) {
            return false;
        }
        let unit = self.discard_unit();
        if unit == 0 {
            return true;
        }
        let (from, to) = (offset.div_ceil(unit).saturating_mul(unit), (offset + len) / unit * unit);
        from >= to || self.punch(from, to - from)
    }

    /// The whole blocks inside `len` bytes from `offset` given back to the
    /// filesystem (`Vfs::punch`), and holes in the map from then on.
    fn punch(&mut self, offset: u64, len: u64) -> bool {
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else { return true };
        let given = vfs.punch(handle, offset, len);
        /* The whole blocks inside the range, as far as the punch got: holes
         * in the map from now */
        let block = direct.block as u64;
        let (first, end) = (offset.div_ceil(block), (offset + len) / block);
        if first < end {
            direct.retake(vfs, handle, first, end - first);
        }
        match given {
            Some(given) => {
                direct.punched += given;
                if given != 0 {
                    direct.through_vfs = true;
                }
                true
            }
            None => false,
        }
    }

    /// `len` bytes from `offset` made zeros, all of them inside the image:
    /// with `unmap` the whole chunks among them given back -- holes read as
    /// zeros -- and the rest written over with zeros. False for a range not
    /// inside, an image held for reading, or an I/O error.
    pub fn write_zeroes(&mut self, offset: u64, len: u64, unmap: bool) -> bool {
        if !self.writable || !self.inside_len(offset, len) {
            return false;
        }
        let end = offset + len;
        let unit = self.discard_unit();
        if unmap && unit != 0 {
            /* The whole chunks inside: none, where the first one past
             * `offset` would lie past a u64 -- never a discard from 0. */
            if let Some(from) = offset.div_ceil(unit).checked_mul(unit) {
                let to = end / unit * unit;
                if from < to {
                    return self.punch(from, to - from) && self.zero(offset, from) && self.zero(to, end);
                }
            }
        }
        self.zero(offset, end)
    }

    /// Zeros written over `[from, to)`, a piece at a time: each piece ending
    /// at a multiple of `ZERO_CHUNK` in the image -- which every block size
    /// divides -- so that no block is written by two of them, and a power
    /// cut between two leaves every sector what it held or zeros. Pieces
    /// counted from `from` itself would split a sector between two writes
    /// wherever `from` is not a multiple of one.
    fn zero(&mut self, from: u64, to: u64) -> bool {
        let chunk = ZERO_CHUNK as u64;
        let mut at = from;
        while at < to {
            let piece_end = (at / chunk + 1).checked_mul(chunk).map_or(to, |e| e.min(to));
            let n = (piece_end - at) as usize;
            if !self.write_at(at, &ZEROS[..n]) {
                return false;
            }
            at += n as u64;
        }
        true
    }

    /// Everything written to the image so far on its disk's medium: the
    /// device's cache pushed out -- or, after a write the VFS took, the
    /// filesystem synced, which pushes it out after its own metadata.
    pub fn sync(&mut self) -> bool {
        match self.direct.as_mut() {
            Some(direct) if !direct.through_vfs => direct.disk.flush().is_ok(),
            Some(direct) => {
                if !self.vfs.sync_file(self.handle) {
                    return false;
                }
                direct.through_vfs = false;
                true
            }
            None => self.vfs.sync_file(self.handle),
        }
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let path = core::str::from_utf8(&self.path).unwrap_or("(a path that is not UTF-8)");
        match &self.direct {
            /* Two lines, each short of what a line of the log holds whole. */
            Some(d) => {
                trace!(0, "image: {} let go of -- {} batches read and {} written off the VFS's lock, {} written \
                           through it; {}", path, d.read_off, d.written_off, d.written_through, MapSays(d.map.as_ref()));
                trace!(0, "image: {}: {} blocks given ahead of writes, {} chunks refused, {} blocks given back", path,
                       d.allocated, d.refused, d.punched);
            }
            None => trace!(0, "image: {} let go of -- read and written through the VFS, its filesystem keeping no \
                               blocks of its own on a device", path),
        }
        self.vfs.release(self.handle);
    }
}

/// What the image says of its map as it is let go of.
struct MapSays<'a>(Option<&'a Map>);

impl core::fmt::Display for MapSays<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(map) => write!(f, "its map {} runs", map.runs.len()),
            None => f.write_str("no map of its own"),
        }
    }
}

impl Direct {
    /// The device `device`, when blocks of `block` bytes on it are what a
    /// batch moves: a page holds a whole number of them, and each is whole
    /// sectors of the device's.
    fn fits(device: usize, block: usize) -> Option<Disk> {
        let disk = Disk::from_handle(device)?;
        let sector = disk.sector_size() as usize;
        if block == 0 || block > PAGE_SIZE || PAGE_SIZE % block != 0 || sector == 0 || block % sector != 0 {
            trace!(0, "image: blocks of {} bytes on sectors of {}: read and written through the VFS", block, sector);
            return None;
        }
        Some(disk)
    }

    /// For blocks of `block` bytes on `disk` (`fits`): None when there is
    /// no memory for the buffer.
    fn new(disk: Disk, block: usize) -> Option<Direct> {
        let batch = batch_buffer()?;
        let mut sectors = Vec::new();
        let mut pieces = Vec::new();
        let mut stretches = Vec::new();
        sectors.try_reserve_exact(batch.pages()).ok()?;
        pieces.try_reserve_exact(batch.pages()).ok()?;
        stretches.try_reserve_exact(batch.pages()).ok()?;
        sectors.resize(batch.pages(), 0);
        let chunk = (CHUNK_BYTES / block as u64).max(1);
        Some(Direct { disk, block, batch, sectors, pieces, stretches, map: None, chunk, through_vfs: false,
                      read_off: 0, written_off: 0, written_through: 0, punched: 0, allocated: 0, grows: false,
                      refused: 0 })
    }

    /// Where each of `n` blocks from `first` is on the device, into
    /// `sectors` from `slot` on: from the map the image holds, or asked of
    /// the VFS where it holds none.
    fn resolve(&mut self, vfs: &Vfs, handle: Handle, first: u64, slot: usize, n: usize) -> bool {
        let out = &mut self.sectors[slot..slot + n];
        match &self.map {
            Some(map) => {
                map.fill(first, out);
                true
            }
            None => vfs.map(handle, first, out),
        }
    }

    /// Blocks `first..first + count`, which the filesystem may have changed
    /// -- given blocks for holes, taken them back -- taken again into the map;
    /// the map dropped where they cannot be.
    fn retake(&mut self, vfs: &Vfs, handle: Handle, first: u64, count: u64) {
        if let Some(map) = self.map.as_mut() {
            if !map.retake(vfs, handle, first, count) {
                trace!(0, "image: its map of blocks {}..+{} cannot be taken again: every batch asks the VFS from \
                           here on", first, count);
                self.map = None;
            }
        }
    }

    /// The chunks blocks `first..first + n` are in, of a file of `blocks`
    /// blocks, given every block they lack -- zeros, and the file's -- and
    /// taken into the map: whether the filesystem gave them.
    fn grow(&mut self, vfs: &Vfs, handle: Handle, first: u64, n: u64, blocks: u64) -> bool {
        let from = first / self.chunk * self.chunk;
        let to = (first + n).div_ceil(self.chunk).saturating_mul(self.chunk).min(blocks);
        if from >= to {
            return false;
        }
        let given = vfs.allocate(handle, from, to - from);
        self.retake(vfs, handle, from, to - from);
        match given {
            Some(given) => {
                self.allocated += given;
                true
            }
            None => {
                /* Not wrong -- the write goes through the VFS, as every one
                 * into a hole did before chunks -- but a commit for each
                 * such write, which is what chunks are for: said at the
                 * first, and counted. */
                if self.refused == 0 {
                    trace!(0, "image: its filesystem refused blocks {}..+{} ahead of a write into a hole: such \
                               writes go through the VFS", from, to - from);
                }
                self.refused += 1;
                false
            }
        }
    }
}

/// The data path's buffer: `BATCH_PAGES`, or the largest run of pages that
/// can be had short of it, down to one.
fn batch_buffer() -> Option<DmaBuffer> {
    let mut pages = BATCH_PAGES;
    loop {
        if let Some(buf) = DmaBuffer::new(pages) {
            return Some(buf);
        }
        if pages == 1 {
            trace!(0, "image: no memory for a buffer");
            return None;
        }
        pages /= 2;
    }
}

/* ---- the images a module holds (`kcore::fs::Image`) ----
 *
 * An image crosses as a word: a slot of the table and the generation of
 * what is in it, looked up on every call -- a word that is no image's reads
 * as none, which is why the calls that take nothing else are safe for any
 * word. A call holds its slot's lock for as long as it reads or writes:
 * one image's calls come one at a time, and nobody else's wait on them. */

/// Images held at once: every disk of every guest the hv module runs, and
/// as many again.
const MAX_IMAGES: usize = 128;

struct Slot {
    generation: u32,
    image: Option<Image>,
}

static IMAGES: OnceBox<Vec<Mutex<Slot>>> = OnceBox::new();

fn images() -> Option<&'static [Mutex<Slot>]> {
    let table = IMAGES.get_or_try_init(|| {
        let mut slots = Vec::new();
        slots.try_reserve_exact(MAX_IMAGES).ok()?;
        for _ in 0..MAX_IMAGES {
            slots.push(Mutex::new(Slot { generation: 0, image: None })?);
        }
        Some(Box::new(slots))
    });
    if table.is_none() {
        trace!(0, "image: no memory for the table of images");
    }
    table.map(|slots| slots.as_slice())
}

/* The slot, plus one, below; the generation above -- as a VFS handle. */
const SLOT_BITS: u32 = 32;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1;
const _: () = assert!(usize::BITS == 2 * SLOT_BITS, "an image is a slot and a generation");

fn word(slot: usize, generation: u32) -> usize {
    ((generation as usize) << SLOT_BITS) | (slot + 1)
}

/// The image `image` names, locked, for `work` -- or None for a word that
/// names none.
fn with<R>(image: usize, work: impl FnOnce(&mut Image) -> R) -> Option<R> {
    let at = (image & SLOT_MASK).checked_sub(1)?;
    let generation = (image >> SLOT_BITS) as u32;
    let mut slot = images()?.get(at)?.lock();
    if slot.generation != generation {
        return None;
    }
    slot.image.as_mut().map(work)
}

/// Holds the regular file at `path` -- or at `<path>.new`, where `locate`
/// finds it -- as a disk's image, for reading, and with `write` not 0 for
/// writing too: the image, or 0 -- no such file, one held already or
/// written by someone else, a read-only mount, no memory, every slot taken.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_open(path: *const u8, path_len: usize, write: i32) -> usize {
    let (Some(vfs), Some(path), Some(table)) =
        (crate::vfs_instance(), unsafe { crate::path(path, path_len) }, images())
    else {
        return 0;
    };
    let Some(at) = core::str::from_utf8(path).ok().and_then(crate::files::locate) else { return 0 };
    let Some(image) = Image::open(vfs, at.as_bytes(), write != 0) else { return 0 };

    let mut image = Some(image);
    for (index, slot) in table.iter().enumerate() {
        let mut slot = slot.lock();
        if slot.image.is_none() {
            slot.image = image.take();
            return word(index, slot.generation);
        }
    }
    trace!(0, "image: all {} images are held", MAX_IMAGES);
    /* Closed as it goes, with no slot's lock held. */
    drop(image);
    0
}

/// Lets go of the image; a word that is no image's is nothing to close.
#[no_mangle]
pub extern "C" fn kernel_image_close(image: usize) {
    let Some(at) = (image & SLOT_MASK).checked_sub(1) else { return };
    let Some(slot) = images().and_then(|table| table.get(at)) else { return };
    let taken = {
        let mut slot = slot.lock();
        if slot.generation != (image >> SLOT_BITS) as u32 {
            return;
        }
        let taken = slot.image.take();
        /* The word names nothing from here on, whatever takes the slot. */
        slot.generation = slot.generation.wrapping_add(1);
        taken
    };
    /* Freed, and its file closed, off the slot's lock. */
    drop(taken);
}

/// The image's size in bytes, or -1 for a word that is no image's.
#[no_mangle]
pub extern "C" fn kernel_image_length(image: usize) -> i64 {
    with(image, |image| i64::try_from(image.size()).unwrap_or(-1)).unwrap_or(-1)
}

/// What a discard of the image gives back whole, in bytes -- its chunk --
/// or 0 where a discard gives back nothing, and for a word that is no
/// image's (`Image::discard_unit`).
#[no_mangle]
pub extern "C" fn kernel_image_discard_unit(image: usize) -> u64 {
    with(image, |image| image.discard_unit()).unwrap_or(0)
}

/// The mount the image's file is on, never 0 -- or 0 for a word that is no
/// image's. With `kernel_image_key`, which file it is.
#[no_mangle]
pub extern "C" fn kernel_image_mount(image: usize) -> u64 {
    with(image, |image| image.identity().0).unwrap_or(0)
}

/// Which file on its mount the image is (`kernel_file_key`): never 0 -- or
/// 0 for a word that is no image's.
#[no_mangle]
pub extern "C" fn kernel_image_key(image: usize) -> u64 {
    with(image, |image| image.identity().1).unwrap_or(0)
}

/// Fills `buf` -- `len` bytes -- from `offset` of the image, all of it
/// inside: 0, or -1.
///
/// # Safety
/// `buf` takes `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_read(image: usize, offset: u64, buf: *mut u8, len: usize) -> i32 {
    if buf.is_null() || isize::try_from(len).is_err() {
        return -1;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    match with(image, |image| image.read_at(offset, buf)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Writes `len` bytes at `offset` of the image, all of them inside -- the
/// image never grows: 0, or -1. Not synced: `kernel_image_sync` is.
///
/// # Safety
/// `data` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_write(image: usize, offset: u64, data: *const u8, len: usize) -> i32 {
    if data.is_null() || isize::try_from(len).is_err() {
        return -1;
    }
    let data = unsafe { core::slice::from_raw_parts(data, len) };
    match with(image, |image| image.write_at(offset, data)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Fills each of the `n` parts of `ios` from its offset of the image
/// (`Image::read_vec`): 0, or -1.
///
/// # Safety
/// `ios` points at `n` parts, each `buf` at `len` writable bytes, and no two
/// of the buffers overlap.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_read_vec(image: usize, ios: *const ImageIo, n: usize) -> i32 {
    if ios.is_null() || !ios.is_aligned() || n == 0 || n > IMAGE_VEC_MAX {
        return -1;
    }
    let ios = unsafe { core::slice::from_raw_parts(ios, n) };
    if ios.iter().any(|io| io.buf.is_null() || isize::try_from(io.len).is_err()) {
        return -1;
    }
    let mut parts: [(u64, &mut [u8]); IMAGE_VEC_MAX] = core::array::from_fn(|_| (0, Default::default()));
    for (part, io) in parts.iter_mut().zip(ios) {
        *part = (io.offset, unsafe { core::slice::from_raw_parts_mut(io.buf, io.len) });
    }
    match with(image, |image| image.read_vec(&mut parts[..n])) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Writes each of the `n` parts of `ios` at its offset of the image
/// (`Image::write_vec`): 0, or -1. Not synced: `kernel_image_sync` is.
///
/// # Safety
/// `ios` points at `n` parts, each `buf` at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_write_vec(image: usize, ios: *const ImageIo, n: usize) -> i32 {
    if ios.is_null() || !ios.is_aligned() || n == 0 || n > IMAGE_VEC_MAX {
        return -1;
    }
    let ios = unsafe { core::slice::from_raw_parts(ios, n) };
    if ios.iter().any(|io| io.buf.is_null() || isize::try_from(io.len).is_err()) {
        return -1;
    }
    let mut parts: [(u64, &[u8]); IMAGE_VEC_MAX] = [(0, &[]); IMAGE_VEC_MAX];
    for (part, io) in parts.iter_mut().zip(ios) {
        *part = (io.offset, unsafe { core::slice::from_raw_parts(io.buf, io.len) });
    }
    match with(image, |image| image.write_vec(&parts[..n])) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Where the image has data from `offset` on (`Image::extent`): the first
/// byte of it into `out[0]`, holes before it, and the end of its run into
/// `out[1]` -- the two the same where none of the blocks looked at holds
/// any: 0, or -1.
///
/// # Safety
/// `out` takes two `u64`s.
#[no_mangle]
pub unsafe extern "C" fn kernel_image_extent(image: usize, offset: u64, out: *mut u64) -> i32 {
    if out.is_null() || !out.is_aligned() {
        return -1;
    }
    match with(image, |image| image.extent(offset)) {
        Some(Some((start, end))) => {
            let out = unsafe { core::slice::from_raw_parts_mut(out, 2) };
            out[0] = start;
            out[1] = end;
            0
        }
        _ => -1,
    }
}

/// The whole blocks inside `len` bytes from `offset` given back -- holes,
/// reading as zeros -- a guest's discard: 0, or -1.
#[no_mangle]
pub extern "C" fn kernel_image_discard(image: usize, offset: u64, len: u64) -> i32 {
    match with(image, |image| image.discard(offset, len)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// `len` bytes from `offset` made zeros, the whole blocks among them given
/// back with `unmap` not 0: 0, or -1.
#[no_mangle]
pub extern "C" fn kernel_image_write_zeroes(image: usize, offset: u64, len: u64, unmap: i32) -> i32 {
    match with(image, |image| image.write_zeroes(offset, len, unmap != 0)) {
        Some(true) => 0,
        _ => -1,
    }
}

/// Everything written to the image so far on its disk's medium -- a
/// guest's flush: 0, or -1.
#[no_mangle]
pub extern "C" fn kernel_image_sync(image: usize) -> i32 {
    match with(image, Image::sync) {
        Some(true) => 0,
        _ => -1,
    }
}
