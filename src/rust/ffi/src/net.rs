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
    pub fn kernel_netdev_enqueue_rx_batch(
        dev_handle: usize,
        frame_handles: *const usize,
        count: usize,
    ) -> usize;
    pub fn kernel_netframe_data(handle: usize) -> *mut u8;
    pub fn kernel_netframe_data_phys(handle: usize) -> u64;
    pub fn kernel_netframe_len(handle: usize) -> usize;
    pub fn kernel_netframe_set_len(handle: usize, len: usize);
    pub fn kernel_netframe_put(handle: usize);
    pub fn kernel_netdev_tx_done(dev_handle: usize, frame_handle: usize);
}

/* The consuming side: a net device already in the kernel's table, found by
   name -- a NetDevice handle, not the RustNetDevice one the driver-side
   functions above take. Devices live as long as the kernel does: there is
   nothing to release. */
extern "C" {
    pub fn kernel_net_find(name: *const u8, name_len: usize) -> usize;
    /// Host byte order; 0 until the device has an address.
    pub fn kernel_net_ip(dev: usize) -> u32;
    pub fn kernel_net_mac(dev: usize, mac: *mut u8);
    /// Hands every UDP datagram to `port` to cb, the frame itself, from the
    /// receive softirq: 0 once listening, 1 when the port is taken, 2 when
    /// the device's listener table is full, 3 for port 0.
    pub fn kernel_net_udp_listen(
        dev: usize,
        port: u16,
        cb: extern "C" fn(ctx: *mut u8, frame: usize),
        ctx: *mut u8,
    ) -> i32;
    /// Takes away the listener kernel_net_udp_listen put on the port with this
    /// ctx, and nobody else's; returns once no call of it is running.
    pub fn kernel_net_udp_unlisten(dev: usize, port: u16, ctx: *mut u8);
    /// Queues frames to transmit, one lock and one doorbell for the lot, from
    /// any context. Takes every frame; returns how many were queued.
    pub fn kernel_net_submit_tx(dev: usize, frames: *const usize, count: usize) -> usize;
    pub fn kernel_netframe_alloc_tx(data_len: usize) -> usize;
    /// Another reference to the frame.
    pub fn kernel_netframe_get(handle: usize);
}
