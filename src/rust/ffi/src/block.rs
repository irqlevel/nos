/// One asynchronous I/O straight to or from physical memory: what a caller
/// that must not block hands a device. The zero-copy block server (the netblk
/// module) has the disk DMA a read into the frame it is about to transmit,
/// and a write out of the frame it received.
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

/* A block device already in the kernel's table -- a disk, or a partition of
   one -- read and written by name: the block layer as a loadable module
   reaches it (src/rust/block/src/table.rs defines these). A module is linked
   on its own, so a C ABI is the only seam it and the layer can share; code
   inside the kernel image calls the layer itself, and a driver registers
   with it as a `block::BlockDriver`. */
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
