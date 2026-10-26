//! A partition of a disk -- or the whole of one -- as the filesystem code
//! reads it: bytes at an offset, whole sectors, within its bounds.

use alloc::string::String;

use crate::ext::Source;
use crate::virtio::{Disk, SECTOR};

pub struct Part<'a> {
    pub disk: &'a mut Disk,
    pub first: u64,
    pub sectors: u64,
}

impl Source for Part<'_> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        if offset % SECTOR != 0 || buf.len() as u64 % SECTOR != 0 {
            return Err(alloc::format!("a read of {} bytes at {}, not whole sectors", buf.len(), offset));
        }
        let sector = offset / SECTOR;
        let count = buf.len() as u64 / SECTOR;
        if sector.checked_add(count).is_none_or(|end| end > self.sectors) {
            return Err(alloc::format!("a read past the partition's end: sector {} of {}", sector + count,
                                      self.sectors));
        }
        self.disk.read(self.first + sector, buf)
    }
}
