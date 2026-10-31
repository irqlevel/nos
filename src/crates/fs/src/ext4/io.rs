//! The device under a mounted ext4, by blocks: every transfer through pages
//! the device may DMA, held to the filesystem's bounds -- and no write at
//! all unless the filesystem is mounted for writing, whatever the code
//! above asks. A file's data goes to and from the device in batches, a
//! block to a page of the batch's buffer and the whole batch one call
//! (`Disk::write_pieces`, `Disk::read_pieces`): a device that takes several
//! requests at once -- NVMe, virtio-blk -- has the batch's in flight
//! together.

use alloc::vec::Vec;

use block::{Disk, Piece};
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::trace;

/// The most blocks a batch holds, a block to a page: 256 KiB of 4 KiB
/// blocks. A buffer this size may not be there to be had; the largest that
/// is, down to a page, will do.
const BATCH_PAGES: usize = 64;

pub(super) struct Io {
    pub dev: Disk,
    pub bs: usize,
    pub sector_size: usize,
    pub blocks: u64,
    /// Set by a mount for writing, and only then; cleared for good when its
    /// journal aborts (`Journal::abort`), so that nothing more reaches the
    /// device whoever asks.
    pub writable: bool,
    pub scratch: DmaBuffer,
    /// A file's data on its way to or from the device, a block a page.
    batch: DmaBuffer,
    pieces: Vec<Piece>,
    /// Whether the device's own write of zeros is used: until the first
    /// one that left anything but zeros where it was asked for them.
    device_zeroes: bool,
}

/// `BATCH_PAGES`, or the largest run of pages that can be had short of it,
/// down to one.
fn batch_buffer() -> Option<DmaBuffer> {
    let mut pages = BATCH_PAGES;
    loop {
        if let Some(buf) = DmaBuffer::new(pages) {
            return Some(buf);
        }
        if pages == 1 {
            return None;
        }
        pages /= 2;
    }
}

impl Io {
    pub fn new(dev: Disk) -> Option<Io> {
        let sector_size = dev.sector_size() as usize;
        if sector_size == 0 || sector_size > PAGE_SIZE {
            return None;
        }
        let batch = batch_buffer()?;
        let mut pieces = Vec::new();
        pieces.try_reserve_exact(batch.pages()).ok()?;
        Some(Io { dev, bs: 0, sector_size, blocks: 0, writable: false, scratch: DmaBuffer::new(1)?, batch, pieces,
                  device_zeroes: true })
    }

    fn sector(&self, block: u64) -> u64 {
        block * (self.bs / self.sector_size) as u64
    }

    /// Block `block` into `out`, a block long.
    pub fn read(&mut self, block: u64, out: &mut [u8]) -> bool {
        if block >= self.blocks {
            trace!(0, "ext4: a read of block {}, past the filesystem's {}", block, self.blocks);
            return false;
        }
        let sector = self.sector(block);
        let buf = &mut self.scratch.as_mut_slice()[..self.bs];
        if self.dev.read(sector, buf).is_err() {
            trace!(0, "ext4: block {} does not read", block);
            return false;
        }
        out[..self.bs].copy_from_slice(buf);
        true
    }

    /// `data`, a block, to block `block`: with `fua`, on the medium when
    /// this returns, else in the device's cache.
    pub fn write(&mut self, block: u64, data: &[u8], fua: bool) -> bool {
        if !self.writable {
            trace!(0, "ext4: a write of block {} to a filesystem not mounted for writing, or whose journal is \
                       aborted: refused", block);
            return false;
        }
        if block >= self.blocks || data.len() < self.bs {
            trace!(0, "ext4: a write of block {}, past the filesystem's {}", block, self.blocks);
            return false;
        }
        let sector = self.sector(block);
        let buf = &mut self.scratch.as_mut_slice()[..self.bs];
        buf.copy_from_slice(&data[..self.bs]);
        if self.dev.write(sector, buf, fua).is_err() {
            trace!(0, "ext4: block {} does not write", block);
            return false;
        }
        true
    }

    /// Everything written so far onto the medium.
    pub fn flush(&mut self) -> bool {
        if !self.writable {
            trace!(0, "ext4: a flush of a filesystem not mounted for writing, or whose journal is aborted: \
                       refused");
            return false;
        }
        if self.dev.flush().is_err() {
            trace!(0, "ext4: the device does not flush");
            return false;
        }
        true
    }

    /// How many blocks a batch holds.
    pub fn batch_blocks(&self) -> usize {
        self.batch.pages()
    }

    /// The `k`th block of the batch, a block long: what `read_batch` read
    /// into it.
    pub fn slot(&self, k: usize) -> &[u8] {
        let at = k * PAGE_SIZE;
        &self.batch.as_slice()[at..at + self.bs]
    }

    /// The `k`th block of the batch, to fill for `write_batch`.
    pub fn slot_mut(&mut self, k: usize) -> &mut [u8] {
        let at = k * PAGE_SIZE;
        let bs = self.bs;
        &mut self.batch.as_mut_slice()[at..at + bs]
    }

    /// Block `block` into the batch's `k`th block, as `read` reads it.
    pub fn read_slot(&mut self, block: u64, k: usize) -> bool {
        if block >= self.blocks {
            trace!(0, "ext4: a read of block {}, past the filesystem's {}", block, self.blocks);
            return false;
        }
        let sector = self.sector(block);
        let at = k * PAGE_SIZE;
        let bs = self.bs;
        if self.dev.read(sector, &mut self.batch.as_mut_slice()[at..at + bs]).is_err() {
            trace!(0, "ext4: block {} does not read", block);
            return false;
        }
        true
    }

    /// `pieces`, made the batch `blocks` is: each `(k, block)` a block of
    /// this filesystem's to or from the batch's `k`th page, no more of them
    /// than the batch holds -- or false, said.
    fn pieces_of(&mut self, blocks: &[(usize, u64)]) -> bool {
        self.pieces.clear();
        if blocks.len() > self.batch.pages() {
            trace!(0, "ext4: a batch of {} blocks, past the {} its buffer holds", blocks.len(), self.batch.pages());
            return false;
        }
        for &(k, block) in blocks {
            if block >= self.blocks || k >= self.batch.pages() {
                trace!(0, "ext4: block {} in a batch, past the filesystem's {}", block, self.blocks);
                return false;
            }
            self.pieces.push(Piece { sector: self.sector(block), at: k * PAGE_SIZE, len: self.bs });
        }
        true
    }

    /// Each `(k, block)` of `blocks`: the batch's `k`th block written to
    /// `block`, all of them in one batch, in the device's cache when this
    /// returns. False on a failure, by when some may be written.
    pub fn write_batch(&mut self, blocks: &[(usize, u64)]) -> bool {
        if !self.writable {
            trace!(0, "ext4: a write of {} blocks to a filesystem not mounted for writing, or whose journal is \
                       aborted: refused", blocks.len());
            return false;
        }
        if !self.pieces_of(blocks) {
            return false;
        }
        if self.dev.write_pieces(&self.batch, &self.pieces).is_err() {
            trace!(0, "ext4: a batch of {} blocks does not write", blocks.len());
            return false;
        }
        true
    }

    /// Blocks `start..start + n` made zeros on the device -- in its cache
    /// when this returns, as `write_batch` leaves what it writes: by the
    /// device's own command for it where it has one, its first and last
    /// block read back to be sure -- a device whose command leaves anything
    /// else there is written zeros from then on -- and otherwise the batch's
    /// pages, zeros, written over them a batch at a time. False on a
    /// failure, by when some may be zeros and some not.
    pub fn zero_blocks(&mut self, start: u64, n: u64) -> bool {
        if !self.writable {
            trace!(0, "ext4: zeros over {} blocks of a filesystem not mounted for writing, or whose journal is \
                       aborted: refused", n);
            return false;
        }
        let Some(end) = start.checked_add(n).filter(|&end| end <= self.blocks) else {
            trace!(0, "ext4: zeros over blocks {}..+{}, past the filesystem's {}", start, n, self.blocks);
            return false;
        };
        if n == 0 {
            return true;
        }
        let per = (self.bs / self.sector_size) as u64;
        if self.device_zeroes && self.dev.write_zeroes(self.sector(start), n * per) {
            if self.reads_zeros(start) && self.reads_zeros(end - 1) {
                return true;
            }
            trace!(0, "ext4: the device's write of zeros over blocks {}..{} left other bytes there: zeros are \
                       written from here on", start, end);
            self.device_zeroes = false;
        }
        let pages = self.batch.pages();
        for k in 0..pages {
            self.slot_mut(k).fill(0);
        }
        let mut blocks: Vec<(usize, u64)> = Vec::new();
        if blocks.try_reserve_exact(pages).is_err() {
            return false;
        }
        let mut at = start;
        while at < end {
            let m = (end - at).min(pages as u64) as usize;
            blocks.clear();
            blocks.extend((0..m).map(|k| (k, at + k as u64)));
            if !self.write_batch(&blocks) {
                return false;
            }
            at += m as u64;
        }
        true
    }

    /// Whether block `b` reads back as zeros.
    fn reads_zeros(&mut self, b: u64) -> bool {
        let sector = self.sector(b);
        let buf = &mut self.scratch.as_mut_slice()[..self.bs];
        self.dev.read(sector, buf).is_ok() && buf.iter().all(|&x| x == 0)
    }

    /// Each `(k, block)` of `blocks`: `block` read into the batch's `k`th
    /// block, all of them in one batch.
    pub fn read_batch(&mut self, blocks: &[(usize, u64)]) -> bool {
        if !self.pieces_of(blocks) {
            return false;
        }
        if self.dev.read_pieces(&mut self.batch, &self.pieces).is_err() {
            trace!(0, "ext4: a batch of {} blocks does not read", blocks.len());
            return false;
        }
        true
    }
}
