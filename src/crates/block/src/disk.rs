//! A block device, to the rest of the kernel image: a filesystem, the disk
//! log, the shell's disk commands.
//!
//! The same shape as `kcore::block::Disk`, which is what a loadable module
//! holds -- but that one reaches the table through the C ABI, as something
//! linked on its own has to, and this one calls it.

use core::ffi::CStr;

use ffi::block::BlockIo;
use kcore::block::SubmitError;
use kcore::dma::DmaBuffer;
use kcore::error::{Error, Result};

use crate::table::{self, Piece};

/// A block device to read and write -- a disk, or a partition of one -- by
/// the name `disks` shows it under. Block devices stay as long as the
/// kernel does, so a Disk holds nothing and copies freely. Reads and writes
/// are synchronous, and may come from several tasks at once. The drivers
/// DMA straight to and from the buffer, so give them one they can: page-
/// aligned, physically contiguous, and no bigger than they take in one I/O
/// (a page for virtio-blk, two for NVMe) -- a DmaBuffer.
///
/// Writing needs a `claim` first: it keeps mounts, the disk log and other
/// writers off the device -- and off the disk it is a partition of, and its
/// partitions -- while the writes go around them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Disk {
    handle: usize,
}

/// How many block devices the table holds. It only grows, so an index once
/// valid stays valid and names the same device.
pub fn count() -> u32 {
    table::count()
}

/// The index'th device of the table, or None past its end.
pub fn at(index: u32) -> Option<Disk> {
    Disk::from_handle(table::at(index))
}

/// Register `driver` as the whole disk `name`: the device, or None when the
/// table is full or the name will not do. For good -- nothing unregisters.
/// (A partition is not registered by anybody: it is an entry the table makes
/// for itself when it reads the disk's partition table.)
pub fn register_driver(name: &str, driver: &'static dyn table::BlockDriver) -> Option<Disk> {
    Disk::from_handle(table::register_driver(name, 0, driver))
}

/// Claim a device by handle, naming the holder a refusal reports: what the
/// VFS does for as long as a filesystem is mounted on it. The error is who
/// holds something overlapping.
pub fn claim_as(device: usize, holder: &'static CStr) -> core::result::Result<usize, &'static str> {
    table::claim(device, holder).map_err(holder_name)
}

/// Give back a claim from `claim_as`. A claim of 0 is nothing to give back.
pub fn release(claim: usize) {
    if claim != 0 {
        table::release(claim);
    }
}

fn holder_name(held: &'static CStr) -> &'static str {
    held.to_str().unwrap_or("something")
}

impl Disk {
    pub fn open(name: &str) -> Option<Self> {
        Self::from_handle(table::find(name.as_bytes()))
    }

    /// A device by the handle its registration or the table gave back: None
    /// for a word that names no device.
    pub fn from_handle(handle: usize) -> Option<Self> {
        if table::exists(handle) { Some(Self { handle }) } else { None }
    }

    pub fn handle(&self) -> usize {
        self.handle
    }

    /// Its name, as `disks` shows it. The table keeps it as long as the
    /// device, which is for good.
    pub fn name(&self) -> &'static str {
        table::name(self.handle).unwrap_or("?")
    }

    /// The disk this is a partition of, or None for a whole disk.
    pub fn parent(&self) -> Option<Disk> {
        Disk::from_handle(table::parent(self.handle))
    }

    /// Its size, in sectors
    pub fn sectors(&self) -> u64 {
        table::capacity(self.handle)
    }

    pub fn sector_size(&self) -> u64 {
        table::sector_size(self.handle)
    }

    /// Fills buf -- a whole number of sectors -- from `sector` on.
    pub fn read(&self, sector: u64, buf: &mut [u8]) -> Result<()> {
        self.whole_sectors(buf.len())?;
        if table::read(self.handle, sector, buf) { Ok(()) } else { Err(Error::IoError) }
    }

    /// Writes buf -- a whole number of sectors -- from `sector` on; with
    /// `fua`, past the device's write cache.
    pub fn write(&self, sector: u64, buf: &[u8], fua: bool) -> Result<()> {
        self.whole_sectors(buf.len())?;
        if table::write(self.handle, sector, buf, fua) { Ok(()) } else { Err(Error::IoError) }
    }

    pub fn flush(&self) -> Result<()> {
        if table::flush(self.handle) { Ok(()) } else { Err(Error::IoError) }
    }

    /// Writes each of `pieces` of `buf` at its sector, and returns once
    /// every one is on the device -- in flight together where the device
    /// takes several at once, as NVMe and virtio-blk do, one after another
    /// where it does not. A piece is a page of `buf` or the start of one,
    /// whole sectors (`Piece`). An error once they are all done or given
    /// up on, with some perhaps written: none of them past the device's
    /// cache (a flush is what does that).
    pub fn write_pieces(&self, buf: &DmaBuffer, pieces: &[Piece]) -> Result<()> {
        if table::write_pieces(self.handle, buf, pieces) { Ok(()) } else { Err(Error::IoError) }
    }

    /// Fills each of `pieces` of `buf` from its sector, as `write_pieces`
    /// writes them.
    pub fn read_pieces(&self, buf: &mut DmaBuffer, pieces: &[Piece]) -> Result<()> {
        if table::read_pieces(self.handle, buf, pieces) { Ok(()) } else { Err(Error::IoError) }
    }

    /// How many partitions of it the kernel found
    pub fn partitions(&self) -> u32 {
        table::partitions(self.handle)
    }

    /// Whether it takes asynchronous I/O -- `submit`: NVMe does, and so does
    /// a partition of an NVMe disk.
    pub fn can_submit(&self) -> bool {
        table::can_submit(self.handle)
    }

    /// Hands the device an I/O straight to or from physical memory and
    /// returns at once; io.done is called when the device is done, exactly
    /// once, from interrupt context. With kick false the device may leave
    /// its doorbell for `kick`.
    ///
    /// # Safety
    /// io.phys names io.count sectors of physically contiguous memory, dword
    /// aligned, that stay valid -- and, for a read, untouched -- until io.done
    /// has run; io.ctx stays valid as long.
    pub unsafe fn submit(&self, io: &BlockIo, kick: bool) -> core::result::Result<(), SubmitError> {
        table::submit(self.handle, io, kick)
    }

    /// Rings the doorbell for what `submit(.., false)` queued.
    pub fn kick(&self) {
        table::kick(self.handle)
    }

    fn whole_sectors(&self, len: usize) -> Result<()> {
        let size = self.sector_size() as usize;
        if size == 0 || len == 0 || len % size != 0 {
            return Err(Error::InvalidValue);
        }
        Ok(())
    }
}
