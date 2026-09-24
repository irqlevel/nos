/// What `kernel_net_resolve` answers: the Ethernet address a frame to an IP
/// address goes to, if anything said it has one.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Resolved {
    /// 1 when `mac` is an answer, 0 when nothing answered.
    pub found: u8,
    pub mac: [u8; 6],
}

/// What `kernel_net_nat_enable` answers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NatOn {
    /// 0 on; 1 no device has a gateway to go out through; 2 the device
    /// will not do -- no such device, or one with no address; 3 on
    /// already; 4 no memory for the table.
    pub code: i32,
    /// The device it goes out through, a kernel_net_find handle, when on.
    pub outer: usize,
}

/// What `kernel_net_rx_stats` answers: the counters that say whether the
/// receive path is keeping up, summed over every device. They only grow,
/// but for `pool_in_flight`, which is a level.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RxStats {
    /// Frames that had to come from the allocator because the pool had none.
    pub pool_misses: u64,
    /// Frames out of the pool right now.
    pub pool_in_flight: u64,
    /// Receive passes the tick started rather than an interrupt.
    pub rx_polls: u64,
    /// Those of them that found the hardware had frames waiting.
    pub rx_poll_work: u64,
    /// Two such in a row: the shape of an interrupt that is not coming.
    pub rx_stalls: u64,
}

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
    ///
    /// batch_end, when there is one, is called at the end of every receive
    /// batch, whether or not the batch had anything for this port: the
    /// moment for a listener that answers from the receive path to hand the
    /// NIC what it built, one lock and one doorbell for the lot.
    ///
    /// Both are called from the receive pass and from nowhere else, and
    /// there is one receive pass at a time in the whole kernel: what only
    /// they touch needs no lock.
    pub fn kernel_net_udp_listen(
        dev: usize,
        port: u16,
        cb: extern "C" fn(ctx: *mut u8, frame: usize),
        batch_end: Option<extern "C" fn(ctx: *mut u8)>,
        ctx: *mut u8,
    ) -> i32;
    /// Takes away the listener kernel_net_udp_listen put on the port with this
    /// ctx, and nobody else's; returns once no call of it is running.
    pub safe fn kernel_net_udp_unlisten(dev: usize, port: u16, ctx: *mut u8);
    /// Queues frames to transmit, one lock and one doorbell for the lot, from
    /// any context. Takes every frame; returns how many were queued.
    pub fn kernel_net_submit_tx(dev: usize, frames: *const usize, count: usize) -> usize;
    /// How many more frames the device's transmit queue has room for right
    /// now. A sender that asks before it builds a batch loses nothing to a
    /// full queue -- kernel_net_submit_tx releases what finds no room -- as
    /// long as it is the only one sending; with others beside it the answer
    /// is a hint, and what it counts as failed is how far off the hint was.
    pub safe fn kernel_net_tx_room(dev: usize) -> usize;
    /// Where a frame to `ip` (host byte order) goes on the wire: the address
    /// itself when it is on the device's subnet, the gateway when it is not,
    /// asked of ARP. Task context: a cache miss sends a request and sleeps,
    /// up to three seconds, for the answer.
    pub safe fn kernel_net_resolve(dev: usize, ip: u32) -> Resolved;
    pub safe fn kernel_net_rx_stats() -> RxStats;

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

/* NAT (net/src/nat.rs): what is behind one device -- a virtual NIC's guests
   -- reaches the world through the device the default route is on, from
   that device's address. One at a time, and off again only by the one that
   put it on: kcore::net::Nat is that. */
unsafe extern "C" {
    /// NAT on, from the device `inner` names -- a kernel_net_find handle.
    /// Task context: it allocates its table.
    pub safe fn kernel_net_nat_enable(inner: usize) -> NatOn;
    /// NAT off, if it is on for the device `inner` names: 1 when it was.
    pub safe fn kernel_net_nat_disable(inner: usize) -> i32;
    /// The DNS server this machine was given -- its resolver's, or the
    /// DHCP lease's -- host byte order; 0 for none.
    pub safe fn kernel_net_dns_server() -> u32;
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
    /// `rxpoll=on`: the tick looks at the receive path as well as the NIC.
    pub safe fn kernel_param_rxpoll_on() -> i32;
}

/// What `kernel_vnic_attach` calls with each frame the stack sends out of a
/// virtual NIC: the ctx it was given, and the frame, lent for the call --
/// from the transmit path, interrupts off.
pub type VnicSink = unsafe extern "C" fn(ctx: usize, frame: *const u8, len: usize);

/* A virtual NIC a module drives (net/src/vnic.rs): made and registered with
   the stack the first time it is asked for by name, found after; frames the
   stack sends out of it go to the handler attached, frames handed in arrive
   as if received. A handle is 0 for none. */
unsafe extern "C" {
    /// The virtual NIC called `name`, made with `mac` (six bytes) and given
    /// `ip` and `mask` (host byte order) the first time: a handle, or 0.
    pub fn kernel_vnic_open(name: *const u8, name_len: usize, mac: *const u8, ip: u32, mask: u32) -> usize;
    /// Frames the stack sends go to handler(ctx, ...) until the detach: 0,
    /// or -1 for a bad handle or a handler attached already.
    pub fn kernel_vnic_attach(vnic: usize, handler: VnicSink, ctx: usize) -> i32;
    /// No more calls of the handler; back once none is running. Task
    /// context: it waits.
    pub fn kernel_vnic_detach(vnic: usize);
    /// A frame into the stack as if received: 0, or -1 when it will not fit,
    /// the backlog is full or the pool is dry.
    pub fn kernel_vnic_receive(vnic: usize, frame: *const u8, len: usize) -> i32;
    /// The net device it is, a `kernel_net_find` handle; 0 for none.
    pub safe fn kernel_vnic_device(vnic: usize) -> usize;
}
