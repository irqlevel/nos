//! A disk as the boot reads it -- the guest's virtio disk, or on the host an
//! image's file -- and a partition of one, or the whole: bytes at an
//! offset, whole sectors, within its bounds.

use alloc::string::String;

use crate::ext::{Sink, Source};

pub const SECTOR: u64 = 512;

/// A disk, by whole sectors: what `plan` reads the boot off, whatever is
/// under it.
pub trait Device {
    /// How many sectors it has.
    fn sectors(&self) -> u64;
    /// `buf.len()` bytes -- whole sectors -- from `sector` on.
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), String>;
    /// `buf` -- whole sectors -- at `sector`; refused for a disk that is
    /// read-only.
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), String>;
}

pub struct Part<'a, D: Device> {
    pub disk: &'a mut D,
    pub first: u64,
    pub sectors: u64,
}

impl<D: Device> Part<'_, D> {
    /// The disk's sector `len` bytes at `offset` start at: whole sectors,
    /// inside the partition.
    fn sector(&self, what: &str, offset: u64, len: usize) -> Result<u64, String> {
        if offset % SECTOR != 0 || len as u64 % SECTOR != 0 {
            return Err(alloc::format!("a {} of {} bytes at {}, not whole sectors", what, len, offset));
        }
        let sector = offset / SECTOR;
        let count = len as u64 / SECTOR;
        if sector.checked_add(count).is_none_or(|end| end > self.sectors) {
            return Err(alloc::format!("a {} past the partition's end: sector {} of {}", what,
                                      sector.saturating_add(count), self.sectors));
        }
        Ok(self.first + sector)
    }
}

impl<D: Device> Source for Part<'_, D> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let sector = self.sector("read", offset, buf.len())?;
        self.disk.read(sector, buf)
    }
}

impl<D: Device> Sink for Part<'_, D> {
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        let sector = self.sector("write", offset, buf.len())?;
        self.disk.write(sector, buf)
    }
}
