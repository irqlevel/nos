use ffi::block;

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
