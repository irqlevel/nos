/// One asynchronous I/O straight to or from physical memory (AsyncBlockIo in
/// block/block_device.h, whose layout this is).
#[repr(C)]
pub struct BlockIo {
    /// 0 read, 1 write, 2 flush
    pub op: u8,
    pub fua: u8,
    pub reserved: u16,
    /// Sectors; 0 for a flush.
    pub count: u32,
    pub sector: u64,
    /// Physically contiguous, dword aligned.
    pub phys: u64,
    /// Called exactly once, from interrupt context: 0, or the device's error.
    pub done: extern "C" fn(ctx: *mut u8, status: i32),
    pub ctx: *mut u8,
}

#[repr(C)]
pub struct BlockDeviceOps {
    pub name: *const u8,
    pub capacity: u64,
    pub sector_size: u64,
    pub read_sectors: Option<extern "C" fn(
        ctx: *mut u8, sector: u64, buf: *mut u8, count: u32,
    ) -> i32>,
    pub write_sectors: Option<extern "C" fn(
        ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32,
    ) -> i32>,
    pub flush: Option<extern "C" fn(ctx: *mut u8) -> i32>,
    /// The asynchronous path; None for a device without one.
    pub submit: Option<extern "C" fn(ctx: *mut u8, io: *const BlockIo, kick: i32) -> i32>,
    pub kick: Option<extern "C" fn(ctx: *mut u8)>,
    pub ctx: *mut u8,
    /// The disk this is a partition of, as its handle, or 0 for a whole
    /// disk. What claims are refused through: one on a disk keeps its
    /// partitions out, and one on a partition keeps the disk out.
    pub parent: usize,
}

extern "C" {
    pub fn kernel_blockdev_register(ops: *const BlockDeviceOps) -> usize;
}

/* The consuming side: a block device already in the kernel's table -- a disk,
   or a partition of one -- read and written by name. */
extern "C" {
    /// The device `disks` lists under this name, or 0. Devices live as long
    /// as the kernel does: there is nothing to release.
    pub fn kernel_blockdev_find(name: *const u8, name_len: usize) -> usize;

    /// How many devices the table holds. It only grows, so an index once
    /// valid stays valid and names the same device.
    pub fn kernel_blockdev_count() -> u32;

    /// The index'th device of the table, or 0.
    pub fn kernel_blockdev_at(index: u32) -> usize;

    /// The device's name into buf, NUL-terminated: the length written, or 0
    /// if it does not fit.
    pub fn kernel_blockdev_name(handle: usize, buf: *mut u8, len: usize) -> usize;

    /// The name as the table holds it: NUL-terminated, kept for as long as
    /// the device is registered, which is for good. Null for a handle that
    /// names nothing.
    pub fn kernel_blockdev_name_ptr(handle: usize) -> *const u8;

    /// The disk a partition is on, or 0 for a whole disk.
    pub fn kernel_blockdev_parent(handle: usize) -> usize;

    /// Its size, in sectors
    pub fn kernel_blockdev_capacity(handle: usize) -> u64;

    pub fn kernel_blockdev_sector_size(handle: usize) -> u64;

    /// Synchronous, count in sectors: 0 once the data is in buf.
    pub fn kernel_blockdev_read(handle: usize, sector: u64, buf: *mut u8, count: u32) -> i32;

    /// Synchronous, count in sectors: 0 once the device has the data.
    pub fn kernel_blockdev_write(
        handle: usize, sector: u64, buf: *const u8, count: u32, fua: i32,
    ) -> i32;

    pub fn kernel_blockdev_flush(handle: usize) -> i32;

    /// Claims the device against mounts, the disk log and other writers: a
    /// claim for kernel_blockdev_release, or 0 -- with *held_by set to who
    /// holds it, a NUL-terminated name the kernel keeps -- while one of
    /// those is on it, on the disk it is a partition of, or on a partition
    /// of it.
    pub fn kernel_blockdev_claim(handle: usize, held_by: *mut *const u8) -> usize;

    /// The same, naming the holder a refusal reports -- what the kernel's own
    /// claimants (a mount, the disk log, the shell) use.
    pub fn kernel_blockdev_claim_as(
        handle: usize, holder: *const u8, held_by: *mut *const u8,
    ) -> usize;

    pub fn kernel_blockdev_release(claim: usize);

    /// How many partitions of the device the kernel found.
    pub fn kernel_blockdev_partitions(handle: usize) -> u32;

    /// 1 if the device has the asynchronous path: NVMe and its partitions.
    pub fn kernel_blockdev_can_submit(handle: usize) -> i32;

    /// Never blocks. 0 once submitted -- io.done is then called exactly
    /// once, from interrupt context -- 1 when the device has no room right
    /// now, 2 for an io it cannot take, 3 when it has no asynchronous path.
    /// io is read before this returns. kick 0 leaves the doorbell for
    /// kernel_blockdev_kick.
    pub fn kernel_blockdev_submit(handle: usize, io: *const BlockIo, kick: i32) -> i32;

    pub fn kernel_blockdev_kick(handle: usize);
}
