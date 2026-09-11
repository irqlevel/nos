#[repr(C)]
pub struct BlockDeviceOps {
    pub name: *const u8,
    pub capacity: u64,
    pub sector_size: u64,
    pub read_sectors: extern "C" fn(
        ctx: *mut u8, sector: u64, buf: *mut u8, count: u32,
    ) -> i32,
    pub write_sectors: extern "C" fn(
        ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32,
    ) -> i32,
    pub flush: Option<extern "C" fn(ctx: *mut u8) -> i32>,
    pub ctx: *mut u8,
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

    pub fn kernel_blockdev_release(claim: usize);

    /// How many partitions of the device the kernel found.
    pub fn kernel_blockdev_partitions(handle: usize) -> u32;
}
