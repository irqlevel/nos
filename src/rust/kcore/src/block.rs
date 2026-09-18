use ffi::block;

use crate::error::{Error, Result};

pub use ffi::block::BlockIo;

/// What a BlockIo asks for (AsyncBlockIo::Read, Write, Flush in block_device.h)
pub const IO_READ: u8 = 0;
pub const IO_WRITE: u8 = 1;
pub const IO_FLUSH: u8 = 2;

/// What a driver's submit answers (BlockDevice::Submit* in block_device.h)
pub const SUBMIT_OK: i32 = 0;
pub const SUBMIT_BUSY: i32 = 1;
pub const SUBMIT_INVALID: i32 = 2;
pub const SUBMIT_UNSUPPORTED: i32 = 3;

/// Why an asynchronous I/O was not taken
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// No room right now; there will be after a completion.
    Busy,
    /// Out of range, misaligned, or more than the device takes at once.
    Invalid,
    /// The device has only the synchronous path.
    Unsupported,
}

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
    /// Optional: the asynchronous path (BlockIo) -- never blocking, answering
    /// with a SUBMIT_* -- and the doorbell for submissions made without one.
    pub submit: Option<extern "C" fn(ctx: *mut u8, io: *const BlockIo, kick: i32) -> i32>,
    pub kick: Option<extern "C" fn(ctx: *mut u8)>,
    pub ctx: *mut u8,
    /// The disk this device is a partition of, or 0 for a whole disk. A
    /// claim on either end refuses one on the other, and `Disk::partitions`
    /// counts through it.
    pub parent: usize,
}

/// Register a block device with the kernel block device table.
/// Returns `None` if the slot pool is full or the name is null.
pub fn register(ops: &BlockDeviceOps) -> Option<BlockDeviceRegistration> {
    let ffi_ops = block::BlockDeviceOps {
        name: ops.name,
        capacity: ops.capacity,
        sector_size: ops.sector_size,
        read_sectors: Some(ops.read_sectors),
        write_sectors: Some(ops.write_sectors),
        flush: ops.flush,
        submit: ops.submit,
        kick: ops.kick,
        ctx: ops.ctx,
        parent: ops.parent,
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

/// Whether a driver may block waiting for a completion: false early in
/// boot, before interrupts and the scheduler are running, when a
/// synchronous I/O has to poll its device instead.
pub fn interrupts_started() -> bool {
    unsafe { block::kernel_blockdev_interrupts_started() != 0 }
}

/// How many block devices the kernel's table holds. The table only grows,
/// so an index once valid stays valid and names the same device.
pub fn count() -> u32 {
    unsafe { block::kernel_blockdev_count() }
}

/// The index'th device of the table, or None past its end.
pub fn at(index: u32) -> Option<Disk> {
    Disk::from_handle(unsafe { block::kernel_blockdev_at(index) })
}

impl Disk {
    pub fn open(name: &str) -> Option<Self> {
        let handle = unsafe { block::kernel_blockdev_find(name.as_ptr(), name.len()) };
        if handle == 0 { None } else { Some(Self { handle }) }
    }

    /// A device by the handle its registration or the table gave back.
    pub fn from_handle(handle: usize) -> Option<Self> {
        if handle == 0 { None } else { Some(Self { handle }) }
    }

    pub fn handle(&self) -> usize {
        self.handle
    }

    /// Its name into buf, as `disks` shows it; None if it does not fit.
    pub fn name<'a>(&self, buf: &'a mut [u8]) -> Option<&'a str> {
        let n = unsafe { block::kernel_blockdev_name(self.handle, buf.as_mut_ptr(), buf.len()) };
        if n == 0 {
            return None;
        }
        core::str::from_utf8(&buf[..n]).ok()
    }

    /// The disk this is a partition of, or None for a whole disk.
    pub fn parent(&self) -> Option<Disk> {
        Disk::from_handle(unsafe { block::kernel_blockdev_parent(self.handle) })
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

    /// Whether it takes asynchronous I/O -- `submit`: NVMe does, and so does
    /// a partition of an NVMe disk.
    pub fn can_submit(&self) -> bool {
        unsafe { block::kernel_blockdev_can_submit(self.handle) != 0 }
    }

    /// Hands the device an I/O straight to or from physical memory and
    /// returns at once; io.done is called when the device is done, exactly
    /// once, from interrupt context. Task or softirq context. With kick false
    /// the device may leave its doorbell for `kick` -- one doorbell for a
    /// batch. The io is read before this returns, and may be submitted again
    /// as it is after Busy.
    ///
    /// # Safety
    /// io.phys names io.count sectors of physically contiguous memory, dword
    /// aligned, that stay valid -- and, for a read, untouched -- until io.done
    /// has run; io.ctx stays valid as long. io.done runs in interrupt context:
    /// no sleeping, allocating or freeing in it.
    #[inline]
    pub unsafe fn submit(&self, io: &BlockIo, kick: bool) -> core::result::Result<(), SubmitError> {
        match unsafe { block::kernel_blockdev_submit(self.handle, io, kick as i32) } {
            SUBMIT_OK => Ok(()),
            SUBMIT_BUSY => Err(SubmitError::Busy),
            SUBMIT_UNSUPPORTED => Err(SubmitError::Unsupported),
            _ => Err(SubmitError::Invalid),
        }
    }

    /// Rings the doorbell for what `submit(.., false)` queued.
    #[inline]
    pub fn kick(&self) {
        unsafe { block::kernel_blockdev_kick(self.handle) }
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
