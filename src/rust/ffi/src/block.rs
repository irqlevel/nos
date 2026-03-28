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
