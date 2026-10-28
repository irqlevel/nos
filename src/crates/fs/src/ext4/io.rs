//! The device under a mounted ext4, by blocks: every transfer through one
//! page the device may DMA, held to the filesystem's bounds -- and no write
//! at all unless the filesystem is mounted for writing, whatever the code
//! above asks.

use block::Disk;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::trace;

pub(super) struct Io {
    pub dev: Disk,
    pub bs: usize,
    pub sector_size: usize,
    pub blocks: u64,
    /// Set by a mount for writing, and only then.
    pub writable: bool,
    pub scratch: DmaBuffer,
}

impl Io {
    pub fn new(dev: Disk) -> Option<Io> {
        let sector_size = dev.sector_size() as usize;
        if sector_size == 0 || sector_size > PAGE_SIZE {
            return None;
        }
        Some(Io { dev, bs: 0, sector_size, blocks: 0, writable: false, scratch: DmaBuffer::new(1)? })
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
            trace!(0, "ext4: a write of block {} to a filesystem not mounted for writing: refused", block);
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
            trace!(0, "ext4: a flush of a filesystem not mounted for writing: refused");
            return false;
        }
        if self.dev.flush().is_err() {
            trace!(0, "ext4: the device does not flush");
            return false;
        }
        true
    }
}
