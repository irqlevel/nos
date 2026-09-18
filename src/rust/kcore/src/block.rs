use ffi::block;

use crate::error::{Error, Result};

pub use ffi::block::BlockIo;

/// What a BlockIo asks for
pub const IO_READ: u8 = 0;
pub const IO_WRITE: u8 = 1;
pub const IO_FLUSH: u8 = 2;

/// What a driver's submit answers
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

/// What a name for the device table fits in, its terminator included.
const NAME_MAX: usize = 32;

/// A block device, as the driver behind it: what the kernel's device table
/// calls when somebody reads, writes or flushes the disk.
///
/// The driver is something that lives for good -- a device is registered for
/// the life of the kernel, and the table has no way to give one back -- and
/// every call can arrive from any task on any CPU, several at once: hence
/// `Sync`, and `&'static self`. What a call needs exclusively the driver
/// keeps behind a lock of its own.
pub trait BlockDriver: Sync + 'static {
    /// Whether the device has the asynchronous path -- `submit` and `kick`.
    /// Without it the table answers Unsupported for the driver.
    const ASYNC: bool = false;

    /// Its size, in sectors.
    fn capacity(&self) -> u64;

    /// Bytes to a sector.
    fn sector_size(&self) -> u64;

    /// Fill `buf` -- whole sectors, never empty: a request for none is
    /// answered before it gets here -- from `sector` on, and return once the
    /// data is in it. The device may be pointed straight at
    /// the buffer: the caller has given one it can DMA into.
    fn read(&'static self, sector: u64, buf: &mut [u8]) -> bool;

    /// Write `data` -- whole sectors, never empty -- at `sector`, and return
    /// once the device has it; with `fua`, once it is on the medium.
    fn write(&'static self, sector: u64, data: &[u8], fua: bool) -> bool;

    /// Push the device's write cache out. A device without one has nothing
    /// to do, which is the default.
    fn flush(&'static self) -> bool {
        true
    }

    /// One asynchronous I/O straight to or from physical memory: never
    /// blocks, never waits. `io.done` is called exactly once when the device
    /// is done, from interrupt context. With `kick` false the doorbell may
    /// be left for `kick`. That `io.phys` is memory the device may use is
    /// what whoever called `Disk::submit` promised; the driver passes it on.
    fn submit(&'static self, _io: &BlockIo, _kick: bool) -> core::result::Result<(), SubmitError> {
        Err(SubmitError::Unsupported)
    }

    /// Ring the doorbell for what `submit` queued without one.
    fn kick(&'static self) {}
}

/// Register `driver` as the block device `name`. None when the table is
/// full, or the name is empty or too long for it.
pub fn register_driver<D: BlockDriver>(
    name: &str, parent: usize, driver: &'static D,
) -> Option<BlockDeviceRegistration> {
    if name.is_empty() || name.len() >= NAME_MAX || name.as_bytes().contains(&0) {
        return None;
    }

    /* The table copies the name: it has to be a C string for the length of
     * the call and no longer. */
    let mut c_name = [0u8; NAME_MAX];
    c_name[..name.len()].copy_from_slice(name.as_bytes());

    let ops = block::BlockDeviceOps {
        name: c_name.as_ptr(),
        capacity: driver.capacity(),
        sector_size: driver.sector_size(),
        read_sectors: Some(read_sectors::<D>),
        write_sectors: Some(write_sectors::<D>),
        flush: Some(flush::<D>),
        submit: if D::ASYNC { Some(submit::<D>) } else { None },
        kick: if D::ASYNC { Some(kick::<D>) } else { None },
        ctx: crate::callback::ctx_of(driver),
        parent,
    };

    let h = unsafe { block::kernel_blockdev_register(&ops) };
    if h == 0 { None } else { Some(BlockDeviceRegistration { handle: h }) }
}

/* What the device table is given to call. `ctx` is the driver, as
 * `register_driver` passed it; a buffer is the caller's, `count` sectors of
 * it, for the length of the call -- which is the block ABI's contract with
 * whoever called in, and what makes it a slice here. */

fn byte_len<D: BlockDriver>(driver: &D, count: u32) -> Option<usize> {
    usize::try_from((count as u64).checked_mul(driver.sector_size())?).ok()
}

extern "C" fn read_sectors<D: BlockDriver>(
    ctx: *mut u8, sector: u64, buf: *mut u8, count: u32,
) -> i32 {
    let driver = unsafe { crate::callback::target_of::<D>(ctx) };
    if count == 0 {
        return 0;
    }
    let len = match byte_len(driver, count) {
        Some(len) if !buf.is_null() => len,
        _ => return -1,
    };

    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    if driver.read(sector, buf) { 0 } else { -1 }
}

extern "C" fn write_sectors<D: BlockDriver>(
    ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32,
) -> i32 {
    let driver = unsafe { crate::callback::target_of::<D>(ctx) };
    if count == 0 {
        return 0;
    }
    let len = match byte_len(driver, count) {
        Some(len) if !buf.is_null() => len,
        _ => return -1,
    };

    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    if driver.write(sector, data, fua != 0) { 0 } else { -1 }
}

extern "C" fn flush<D: BlockDriver>(ctx: *mut u8) -> i32 {
    let driver = unsafe { crate::callback::target_of::<D>(ctx) };
    if driver.flush() { 0 } else { -1 }
}

extern "C" fn submit<D: BlockDriver>(ctx: *mut u8, io: *const BlockIo, kick: i32) -> i32 {
    let driver = unsafe { crate::callback::target_of::<D>(ctx) };
    /* The table hands over the `&BlockIo` it was given, or nothing. */
    let io = match unsafe { io.as_ref() } {
        Some(io) => io,
        None => return SUBMIT_INVALID,
    };

    match driver.submit(io, kick != 0) {
        Ok(()) => SUBMIT_OK,
        Err(SubmitError::Busy) => SUBMIT_BUSY,
        Err(SubmitError::Invalid) => SUBMIT_INVALID,
        Err(SubmitError::Unsupported) => SUBMIT_UNSUPPORTED,
    }
}

extern "C" fn kick<D: BlockDriver>(ctx: *mut u8) {
    unsafe { crate::callback::target_of::<D>(ctx) }.kick();
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

/// Claim a device by handle, naming the holder a refusal reports: what the
/// VFS does for as long as a filesystem is mounted on it. 0 if something
/// overlapping holds it.
///
/// `holder` must be NUL-terminated and outlive the claim.
pub fn claim_as(device: usize, holder: *const u8) -> core::result::Result<usize, &'static str> {
    let mut held: *const u8 = core::ptr::null();
    let claim = unsafe { block::kernel_blockdev_claim_as(device, holder, &mut held) };
    if claim != 0 {
        return Ok(claim);
    }
    Err(holder_name(held))
}

/// The holder a refused claim named. The kernel keeps the string for as long
/// as the claim, and a static holder's for good.
fn holder_name(held: *const u8) -> &'static str {
    if held.is_null() {
        return "something";
    }
    let name = unsafe { core::ffi::CStr::from_ptr(held as *const core::ffi::c_char) };
    name.to_str().unwrap_or("something")
}

/// Give back a claim from `claim_as`. A claim of 0 is nothing to give back.
pub fn release(claim: usize) {
    if claim != 0 {
        unsafe { block::kernel_blockdev_release(claim) }
    }
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
        Err(holder_name(held))
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
