use core::ffi::c_void;

/* TCP as a loadable module reaches it (src/crates/net/src/abi.rs defines
   these): a module is linked on its own, so a C ABI is the only seam it and
   the network layer can share. Code inside the kernel image calls the layer
   itself. The byte count, 0 at EOF, or a negative code: -2 is a timeout. */
unsafe extern "C" {
    pub fn kernel_tcp_recv(
        conn: *mut c_void,
        buf: *mut u8,
        len: usize,
        timeout_ms: u64,
    ) -> isize;
}

/* A server of Rust's owns its connections: it listens on a port of a
   device -- a kernel_net_find handle -- accepts what arrives, and closes
   both what it accepted and the listener. The listener holds `backlog`
   connections nobody has accepted at most (taken into 1..=16), and with
   `bulk` gives each it accepts bulk buffers both ways -- a relay's. Accepting
   returns null once timeout_ms (0: forever) passes with nobody, or once the
   listener is closed. */
unsafe extern "C" {
    pub safe fn kernel_tcp_listen(dev: usize, port: u16, backlog: usize, bulk: bool) -> *mut c_void;
    /// An active open to ip:port (host byte order) through the device, from
    /// an ephemeral port: null when nothing answered in time. Sleeps.
    pub safe fn kernel_tcp_connect(dev: usize, ip: u32, port: u16) -> *mut c_void;
    /// `kernel_tcp_connect`, with bulk buffers both ways -- up to 512 KiB
    /// each, from the heap, the window scaled when the peer offers to: a
    /// connection that sends as much as it takes in. Each the slot's own
    /// 8 KiB when the heap has none to give.
    pub safe fn kernel_tcp_connect_bulk(dev: usize, ip: u32, port: u16) -> *mut c_void;
    pub safe fn kernel_tcp_accept(listener: *mut c_void, timeout_ms: u64) -> *mut c_void;
    pub safe fn kernel_tcp_close(conn: *mut c_void);
    /// A RST instead of the FIN exchange: the slot comes back at once, not
    /// after a minute of TIME-WAIT.
    pub safe fn kernel_tcp_abort(conn: *mut c_void);
    /// The bytes queued: 0 when timeout_ms found room for none, -1 once the
    /// connection is gone.
    pub fn kernel_tcp_send_timeout(
        conn: *mut c_void,
        buf: *const u8,
        len: usize,
        timeout_ms: u64,
    ) -> isize;
    /// The address at the other end, host byte order, and its port.
    pub fn kernel_tcp_peer(conn: *mut c_void, ip: *mut u32, port: *mut u16);

    /// `kernel_tcp_listen`, answering the `count` networks at `nets` alone:
    /// a segment from any other address for the port is dropped before the
    /// backlog and before a slot, with no SYN-ACK and no reset. Null for more
    /// than 64 networks or a prefix past 32, and for what kernel_tcp_listen
    /// refuses.
    pub fn kernel_tcp_listen_allowed(dev: usize, port: u16, backlog: usize, bulk: bool, nets: *const TcpNet,
                                     count: usize) -> *mut c_void;
    /// A listener's allowlist changed: the `count` networks at `nets` from
    /// now on -- anyone, with `any`. What it holds already stays. 0 done,
    /// -1 refused: no listener, more than 64 networks, a prefix past 32.
    pub fn kernel_tcp_set_allowed(listener: *mut c_void, nets: *const TcpNet, count: usize, any: bool) -> i32;
    /// A connection's own floor for its retransmission timeout, `ms`, from
    /// 20 up to the 200 every connection starts with: for a link both of
    /// whose ends are nos, which acknowledges every segment at once, where
    /// no peer delays its acknowledgements under that floor. In effect from
    /// the next round trip measured. 0 done, -1 refused.
    pub safe fn kernel_tcp_set_rto_floor(conn: *mut c_void, ms: u32) -> i32;
}

/// A network an allowlisted listener answers: an address, host byte order,
/// and how many of its leading bits count -- 32 for a host.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TcpNet {
    pub ip: u32,
    pub prefix: u8,
    pub _pad: [u8; 3],
}
