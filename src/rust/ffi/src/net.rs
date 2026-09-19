/* The network layer as a loadable module reaches it (src/rust/net defines
   these, in device.rs and frame.rs): a device by name, a UDP port to listen
   on, frames to build and transmit. A module is linked on its own, so a C
   ABI is the only seam it and the layer can share. Nothing inside the kernel
   image comes through here -- a NIC's driver registers with the `net` crate
   as a `net::NetDriver`, and the layer's own services call it directly.

   A device is a handle from kernel_net_find; devices live as long as the
   kernel does, so there is nothing to release. A frame is a handle too, and
   a reference: whoever holds one hands it on or gives it up with
   kernel_netframe_put. */
unsafe extern "C" {
    pub fn kernel_net_find(name: *const u8, name_len: usize) -> usize;
    /// Host byte order; 0 until the device has an address.
    pub safe fn kernel_net_ip(dev: usize) -> u32;
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
    pub safe fn kernel_net_udp_unlisten(dev: usize, port: u16, ctx: *mut u8);
    /// Queues frames to transmit, one lock and one doorbell for the lot, from
    /// any context. Takes every frame; returns how many were queued.
    pub fn kernel_net_submit_tx(dev: usize, frames: *const usize, count: usize) -> usize;

    pub safe fn kernel_netframe_alloc_tx(data_len: usize) -> usize;
    /// Another reference to the frame.
    pub fn kernel_netframe_get(handle: usize);
    pub fn kernel_netframe_put(handle: usize);
    pub fn kernel_netframe_data(handle: usize) -> *mut u8;
    pub fn kernel_netframe_data_phys(handle: usize) -> u64;
    pub fn kernel_netframe_len(handle: usize) -> usize;
    /// How many bytes the frame's buffer has room for.
    pub fn kernel_netframe_capacity(handle: usize) -> usize;
    pub fn kernel_netframe_set_len(handle: usize, len: usize);
}

/* What netconsole needs of the kernel: what it was asked for, the log that
   happened before it was set up, and whether a panic has started. */
unsafe extern "C" {
    /// The collector's address, port and the nctail= cap in KiB; 1 when
    /// netconsole= was given at all.
    pub fn kernel_netconsole_params(
        ip: *mut u32, port: *mut u16, tail_kb: *mut usize,
    ) -> i32;
    /// Every message the kernel log already holds, oldest first.
    pub fn kernel_dmesg_replay(
        line: extern "C" fn(ctx: *mut u8, s: *const u8, len: usize), ctx: *mut u8,
    );
}

/* What the kernel command line said about the network. */
unsafe extern "C" {
    /// `dhcp=off`: do not run a DHCP client.
    pub safe fn kernel_param_dhcp_off() -> i32;
    /// `dns=on`: start a resolver on a lease's DNS server.
    pub safe fn kernel_param_dns_on() -> i32;
}
