//! A partition of a disk -- or the whole of one -- as the filesystem code
//! reads it: bytes at an offset, whole sectors, within its bounds.

use alloc::string::String;

use crate::ext::{Sink, Source};
use crate::virtio::{Disk, SECTOR};

pub struct Part<'a> {
    pub disk: &'a mut Disk,
    pub first: u64,
    pub sectors: u64,
}

impl Part<'_> {
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

impl Source for Part<'_> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let sector = self.sector("read", offset, buf.len())?;
        self.disk.read(sector, buf)
    }
}

impl Sink for Part<'_> {
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        let sector = self.sector("write", offset, buf.len())?;
        self.disk.write(sector, buf)
    }
}
