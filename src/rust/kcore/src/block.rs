use ffi::block;

use crate::error::{Error, Result};

/// Registration handle for a Rust-implemented block device.
/// Registration is permanent (boot-lifetime); there is no unregister.
pub struct BlockDeviceRegistration {
    handle: usize,
}

impl BlockDeviceRegistration {
    pub fn handle(&self) -> usize {
        self.handle
    }
}

/// Ops table passed to `register`. All function pointers must remain valid
/// for the lifetime of the kernel (static or leaked allocations).
pub struct BlockDeviceOps {
    /// Null-terminated ASCII device name (e.g. b"nvme0\0").
    pub name: *const u8,
    pub capacity: u64,
    pub sector_size: u64,
    pub read_sectors: extern "C" fn(
        ctx: *mut u8, sector: u64, buf: *mut u8, count: u32,
    ) -> i32,
    pub write_sectors: extern "C" fn(
        ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32,
    ) -> i32,
    /// Optional. Pass `None` if the device has no write cache to flush.
    pub flush: Option<extern "C" fn(ctx: *mut u8) -> i32>,
    pub ctx: *mut u8,
}

/// Register a block device with the kernel block device table.
/// Returns `None` if the slot pool is full or the name is null.
pub fn register(ops: &BlockDeviceOps) -> Option<BlockDeviceRegistration> {
    let ffi_ops = block::BlockDeviceOps {
        name: ops.name,
        capacity: ops.capacity,
        sector_size: ops.sector_size,
        read_sectors: ops.read_sectors,
        write_sectors: ops.write_sectors,
        flush: ops.flush,
        ctx: ops.ctx,
    };
    let h = unsafe { block::kernel_blockdev_register(&ffi_ops) };
    if h == 0 { None } else { Some(BlockDeviceRegistration { handle: h }) }
}

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
#[derive(Clone, Copy)]
pub struct Disk {
    handle: usize,
}

impl Disk {
    pub fn open(name: &str) -> Option<Self> {
        let handle = unsafe { block::kernel_blockdev_find(name.as_ptr(), name.len()) };
        if handle == 0 { None } else { Some(Self { handle }) }
    }

    /// Its size, in sectors
    pub fn sectors(&self) -> u64 {
        unsafe { block::kernel_blockdev_capacity(self.handle) }
    }

    pub fn sector_size(&self) -> u64 {
        unsafe { block::kernel_blockdev_sector_size(self.handle) }
    }

    /// Fills buf -- a whole number of sectors -- from `sector` on.
    pub fn read(&self, sector: u64, buf: &mut [u8]) -> Result<()> {
        let count = self.sector_count(buf.len())?;
        let rc = unsafe { block::kernel_blockdev_read(self.handle, sector, buf.as_mut_ptr(), count) };
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// Writes buf -- a whole number of sectors -- from `sector` on; with
    /// `fua`, past the device's write cache.
    pub fn write(&self, sector: u64, buf: &[u8], fua: bool) -> Result<()> {
        let count = self.sector_count(buf.len())?;
        let rc = unsafe {
            block::kernel_blockdev_write(self.handle, sector, buf.as_ptr(), count, fua as i32)
        };
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    pub fn flush(&self) -> Result<()> {
        let rc = unsafe { block::kernel_blockdev_flush(self.handle) };
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// The device, for writing to it around whatever else would: refused --
    /// with who holds it -- while a mounted filesystem, the disk log or
    /// another writer is on it, on the disk it is a partition of, or on a
    /// partition of it. While the DiskClaim lives none of those can have it.
    pub fn claim(&self) -> core::result::Result<DiskClaim, &'static str> {
        let mut held: *const u8 = core::ptr::null();
        let claim = unsafe { block::kernel_blockdev_claim(self.handle, &mut held) };
        if claim != 0 {
            return Ok(DiskClaim { claim });
        }
        if held.is_null() {
            return Err("something");
        }
        /* The kernel keeps the name for good */
        let name = unsafe { core::ffi::CStr::from_ptr(held as *const core::ffi::c_char) };
        Err(name.to_str().unwrap_or("something"))
    }

    /// How many partitions of it the kernel found
    pub fn partitions(&self) -> u32 {
        unsafe { block::kernel_blockdev_partitions(self.handle) }
    }

    fn sector_count(&self, len: usize) -> Result<u32> {
        let size = self.sector_size() as usize;
        if size == 0 || len == 0 || len % size != 0 {
            return Err(Error::InvalidValue);
        }
        u32::try_from(len / size).map_err(|_| Error::InvalidValue)
    }
}

/// A claim on a block device, from Disk::claim; given back on drop.
pub struct DiskClaim {
    claim: usize,
}

impl Drop for DiskClaim {
    fn drop(&mut self) {
        unsafe { block::kernel_blockdev_release(self.claim) };
    }
}
