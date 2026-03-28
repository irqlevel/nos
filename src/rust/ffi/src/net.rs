#[repr(C)]
pub struct NetDeviceOps {
    pub name: *const u8,
    pub mac: [u8; 6],
    pub flush_tx: extern "C" fn(ctx: *mut u8),
    pub process_rx: extern "C" fn(ctx: *mut u8),
    pub ctx: *mut u8,
}

extern "C" {
    pub fn kernel_netdev_register(ops: *const NetDeviceOps) -> usize;
    pub fn kernel_netdev_set_ip(handle: usize, ip: u32);
    pub fn kernel_netdev_set_mask(handle: usize, mask: u32);
    pub fn kernel_netdev_set_gw(handle: usize, gw: u32);
    pub fn kernel_netdev_tx_dequeue(handle: usize) -> usize;
    pub fn kernel_netdev_tx_notify(handle: usize);
    pub fn kernel_netframe_alloc_rx(data_len: usize) -> usize;
    pub fn kernel_netdev_enqueue_rx(dev_handle: usize, frame_handle: usize);
    pub fn kernel_netframe_data(handle: usize) -> *mut u8;
    pub fn kernel_netframe_data_phys(handle: usize) -> u64;
    pub fn kernel_netframe_len(handle: usize) -> usize;
    pub fn kernel_netframe_set_len(handle: usize, len: usize);
    pub fn kernel_netframe_put(handle: usize);
}
