//! A disk's image: a file of nos's that a guest's disk is, held for as long
//! as the disk is open and read and written off the VFS's lock.
//!
//! The VFS is one lock, held across every call down to the device and back
//! (`vfs`): a guest's disk read and written through it waits behind every
//! other guest's and behind nos's own files, a request at a time, machine-
//! wide. An image takes only what it must under that lock -- where the
//! blocks of the range it is asked for are on the device (`Vfs::map`), which
//! the filesystem's pointers say and its caches mostly hold -- and does the
//! reads and writes themselves to the device straight, with the lock let
//! go, through a buffer of its own, in batches the driver has in flight
//! together.
//!
//! What makes that safe is the hold (`vfs::OPEN_HOLD`): while an image is
//! open nothing else may write the file, cut it, grow it, rename it or
//! remove it, so the blocks a map named stay the file's -- and nobody else
//! writes them -- until the image is closed. What only the filesystem can do
//! is left to it: a write into a hole needs blocks allocated and pointed at
//! in order, so a batch with a hole in it goes through the VFS as every
//! write did, and the next flush is then the filesystem's sync and not only
//! the device's. A filesystem with no blocks of its own on a device
//! (`FileSystem::data_block_size` 0 -- ramfs) has its images read and
//! written through the VFS whole.
//!
//! A partial block -- a sector of a guest's in a filesystem block of 4 KiB
//! -- is read, changed and written back whole. That is safe because an
//! image is one writer's: its calls come one at a time (`&mut self`, and the
//! slot's lock across the C ABI), and the hold keeps everyone else off the
//! file.
//!
//! A guest's discard gives whole blocks back to the filesystem (`discard`,
//! `Vfs::punch`): holes from then on, as a write of zeros with its unmap
//! flag does with the blocks it covers whole (`write_zeroes`). Blocks given
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

/// The most blocks a batch moves: a page of the buffer each.
const BATCH_PAGES: usize = 64;

/// The most blocks `extent` looks at in one call: 16 MiB of 4 KiB blocks, a
/// map of a batch's at a time under the VFS's lock -- let go of between
/// them, and the caller's to stop between calls.
const EXTENT_BLOCKS: u64 = 4096;

/// What zeros are written from, a piece at a time (`write_zeroes`).
const ZERO_CHUNK: usize = 16 * 1024;
static ZEROS: [u8; ZERO_CHUNK] = [0; ZERO_CHUNK];

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
    /// A write went through the VFS since the last sync -- blocks were
    /// allocated, the filesystem's own metadata changed -- so the next sync
    /// is the filesystem's, not only the device's.
    through_vfs: bool,
    /// Batches read and written off the VFS's lock, and written through it:
    /// what the image says as it goes.
    read_off: u64,
    written_off: u64,
    written_through: u64,
    /// Blocks given back by discards.
    punched: u64,
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
                image.direct = Some(Direct::new(disk, block)?);
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
        /* Byte offsets of blocks inside the image: below `size`, which a
         * file's length keeps far from the top of a u64. */
        let at_block = |b: u64| (b * block).min(size);
        let mut data_from = None;
        let mut at = first;
        while at < last {
            let n = usize::try_from(last - at).map_or(direct.sectors.len(), |n| n.min(direct.sectors.len()));
            if !vfs.map(handle, at, &mut direct.sectors[..n]) {
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
        if !self.inside(offset, buf.len()) {
            return false;
        }
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else {
            return usize::try_from(offset).ok().and_then(|at| vfs.read_at(handle, at, buf)) == Some(buf.len());
        };

        let block = direct.block as u64;
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let mut skip = (at % block) as usize;
            let blocks = (skip + buf.len() - done).div_ceil(direct.block).min(direct.sectors.len());
            if !vfs.map(handle, at / block, &mut direct.sectors[..blocks]) {
                return false;
            }

            /* The blocks that are there, a page of the buffer each, read in
             * one go -- with the VFS's lock let go of */
            direct.pieces.clear();
            for (k, &sector) in direct.sectors[..blocks].iter().enumerate() {
                if sector != 0 {
                    direct.pieces.push(Piece { sector, at: k * PAGE_SIZE, len: direct.block });
                }
            }
            if direct.disk.read_pieces(&mut direct.batch, &direct.pieces).is_err() {
                trace!(0, "image: a read of {} blocks failed", direct.pieces.len());
                return false;
            }
            direct.read_off += 1;

            for (k, &sector) in direct.sectors[..blocks].iter().enumerate() {
                let chunk = (direct.block - skip).min(buf.len() - done);
                let to = &mut buf[done..done + chunk];
                if sector != 0 {
                    let from = k * PAGE_SIZE + skip;
                    to.copy_from_slice(&direct.batch.as_slice()[from..from + chunk]);
                } else {
                    /* A hole reads as zeros */
                    to.fill(0);
                }
                done += chunk;
                skip = 0;
            }
        }
        true
    }

    /// Writes `data` at `offset`, all of it inside the image -- which never
    /// grows: false on an I/O error, a range that is not, or an image held
    /// for reading. Not synced: `sync` is.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> bool {
        if !self.writable || !self.inside(offset, data.len()) {
            return false;
        }
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else {
            return usize::try_from(offset).is_ok_and(|at| vfs.write_within(handle, at, data));
        };

        let block = direct.block;
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let skip = (at % block as u64) as usize;
            let blocks = (skip + data.len() - done).div_ceil(block).min(direct.sectors.len());
            /* What of `data` this batch takes: to the end of its blocks */
            let take = (blocks * block - skip).min(data.len() - done);
            if !vfs.map(handle, at / block as u64, &mut direct.sectors[..blocks]) {
                return false;
            }

            if direct.sectors[..blocks].contains(&0) {
                /* A hole: blocks only the filesystem can allocate and point
                 * at, in order -- so the batch through it, as every write
                 * went before */
                direct.through_vfs = true;
                let Ok(to) = usize::try_from(at) else { return false };
                if !vfs.write_within(handle, to, &data[done..done + take]) {
                    return false;
                }
                direct.written_through += 1;
                done += take;
                continue;
            }

            /* The first block and the last, where the batch takes only part
             * of them: what the rest of each holds, read first */
            let end = skip + take;
            direct.pieces.clear();
            if skip != 0 {
                direct.pieces.push(Piece { sector: direct.sectors[0], at: 0, len: block });
            }
            if end % block != 0 && (blocks > 1 || skip == 0) {
                let last = blocks - 1;
                direct.pieces.push(Piece { sector: direct.sectors[last], at: last * PAGE_SIZE, len: block });
            }
            if direct.disk.read_pieces(&mut direct.batch, &direct.pieces).is_err() {
                trace!(0, "image: a read of the ends of a write failed");
                return false;
            }

            let mut from = done;
            let mut off = skip;
            for k in 0..blocks {
                let chunk = (block - off).min(done + take - from);
                let to = k * PAGE_SIZE + off;
                direct.batch.as_mut_slice()[to..to + chunk].copy_from_slice(&data[from..from + chunk]);
                from += chunk;
                off = 0;
            }

            direct.pieces.clear();
            for (k, &sector) in direct.sectors[..blocks].iter().enumerate() {
                direct.pieces.push(Piece { sector, at: k * PAGE_SIZE, len: block });
            }
            if direct.disk.write_pieces(&direct.batch, &direct.pieces).is_err() {
                trace!(0, "image: a write of {} blocks failed", blocks);
                return false;
            }
            direct.written_off += 1;
            done += take;
        }
        true
    }

    /// The whole blocks inside `len` bytes from `offset` given back to the
    /// filesystem -- holes from here on, reading as zeros, the image's size
    /// as it was: a guest's discard. What the range covers of a block only
    /// in part is left as it is, as a disk may leave it. False for a range
    /// not inside the image, an image held for reading, or an I/O error. On
    /// a filesystem with no blocks of its own there is nothing to give back,
    /// and that is done. The filesystem has its metadata written before it
    /// is done, so a sync after it is the device's alone.
    pub fn discard(&mut self, offset: u64, len: u64) -> bool {
        if !self.writable || !self.inside_len(offset, len) {
            return false;
        }
        let (vfs, handle) = (self.vfs, self.handle);
        let Some(direct) = self.direct.as_mut() else { return true };
        match vfs.punch(handle, offset, len) {
            Some(given) => {
                direct.punched += given;
                true
            }
            None => false,
        }
    }

    /// `len` bytes from `offset` made zeros, all of them inside the image:
    /// with `unmap` the whole blocks among them given back -- holes read as
    /// zeros -- and the rest written over with zeros. False for a range not
    /// inside, an image held for reading, or an I/O error.
    pub fn write_zeroes(&mut self, offset: u64, len: u64, unmap: bool) -> bool {
        if !self.writable || !self.inside_len(offset, len) {
            return false;
        }
        let end = offset + len;
        if let Some(block) = self.direct.as_ref().map(|direct| direct.block as u64).filter(|_| unmap) {
            let (from, to) = (offset.div_ceil(block) * block, end / block * block);
            if from < to {
                return self.discard(from, to - from) && self.zero(offset, from) && self.zero(to, end);
            }
        }
        self.zero(offset, end)
    }

    /// Zeros written over `[from, to)`, a piece at a time.
    fn zero(&mut self, from: u64, to: u64) -> bool {
        let mut at = from;
        while at < to {
            let n = ((to - at) as usize).min(ZERO_CHUNK);
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
            Some(d) => trace!(0, "image: {} let go of -- {} batches read and {} written off the VFS's lock, {} \
                                  written through it, {} blocks given back", path, d.read_off, d.written_off,
                              d.written_through, d.punched),
            None => trace!(0, "image: {} let go of -- read and written through the VFS, its filesystem keeping no \
                               blocks of its own on a device", path),
        }
        self.vfs.release(self.handle);
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
        sectors.try_reserve_exact(batch.pages()).ok()?;
        pieces.try_reserve_exact(batch.pages()).ok()?;
        sectors.resize(batch.pages(), 0);
        Some(Direct { disk, block, batch, sectors, pieces, through_vfs: false, read_off: 0, written_off: 0,
                      written_through: 0, punched: 0 })
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
