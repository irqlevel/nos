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
    /// The addresses a lease gives the device; host byte order.
    pub fn kernel_net_set_ip(dev: usize, ip: u32);
    pub fn kernel_net_set_mask(dev: usize, mask: u32);
    pub fn kernel_net_set_gw(dev: usize, gw: u32);
    /// What to ARP for to reach `dst`: the gateway off-subnet, `dst` on it.
    /// Host byte order both ways.
    pub fn kernel_net_route_ip(dev: usize, dst: u32) -> u32;
    /// A frame built whole by the caller, out of the device: 0 queued, -1 not.
    pub fn kernel_net_send_raw(dev: usize, data: *const u8, len: usize) -> i32;
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
    /// The same, with a call at the end of each receive batch: where a
    /// listener that answers from the receive path hands the batch's replies
    /// to the NIC together rather than one at a time.
    pub fn kernel_net_udp_listen_batch(
        dev: usize,
        port: u16,
        cb: extern "C" fn(ctx: *mut u8, frame: usize),
        ctx: *mut u8,
        batch_end: extern "C" fn(ctx: *mut u8),
    ) -> i32;
    pub fn kernel_net_udp_unlisten(dev: usize, port: u16, ctx: *mut u8);
    /// Allocations the frame pool could not serve, and frames in flight.
    pub fn kernel_netframe_pool_stats(misses: *mut usize, in_flight: *mut usize);
    /// Receive polls, polls that found work, and polls that found work with
    /// no interrupt-driven pass since the last one.
    pub fn kernel_net_rx_poll_stats(
        polls: *mut usize, work: *mut usize, stalls: *mut usize,
    );
    /// Queues frames to transmit, one lock and one doorbell for the lot, from
    /// any context. Takes every frame; returns how many were queued.
    pub fn kernel_net_submit_tx(dev: usize, frames: *const usize, count: usize) -> usize;
    pub fn kernel_netframe_alloc_tx(data_len: usize) -> usize;
    /// Another reference to the frame.
    pub fn kernel_netframe_get(handle: usize);
}

/* A quoted TCP segment came back unreachable. Goes when TCP moves over. */
extern "C" {
    pub fn kernel_tcp_icmp_unreachable(
        src_ip: u32, src_port: u16, dst_ip: u32, dst_port: u16, seq: u32,
    );
}

/* What netconsole needs of the kernel: what it was asked for, the log that
   happened before it was set up, and whether a panic has started. */
extern "C" {
    /// The collector's address, port and the nctail= cap in KiB; 1 when
    /// netconsole= was given at all.
    pub fn kernel_netconsole_params(
        ip: *mut u32, port: *mut u16, tail_kb: *mut usize,
    ) -> i32;
    /// Every message the kernel log already holds, oldest first.
    pub fn kernel_dmesg_replay(
        line: extern "C" fn(ctx: *mut u8, s: *const u8, len: usize), ctx: *mut u8,
    );
    /// Whether a panic has started -- so code writes without taking a lock
    /// another CPU may hold on its way to a halt.
    pub fn kernel_panic_active() -> i32;
}
