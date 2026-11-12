
/* TCP as a loadable module reaches it (src/crates/layers/net/src/abi.rs defines
   these): a module is linked on its own, so a C ABI is the only seam it and
   the network layer can share. Code inside the kernel image calls the layer
   itself. The byte count, 0 at EOF, or a negative code: -2 is a timeout. */
/* A connection -- a listener's too -- crosses as a word: its slot in the
 * kernel's pool and one, and above that the generation of the connection the
 * slot held when the word was made; 0 for none. A word kept past its
 * connection's close names nothing once the slot is someone else's (it was
 * the slot's address, which then named the next connection). */
unsafe extern "C" {
    pub fn kernel_tcp_recv(
        conn: usize,
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
    pub safe fn kernel_tcp_listen(dev: usize, port: u16, backlog: usize, bulk: bool) -> usize;
    /// An active open to ip:port (host byte order) through the device, from
    /// an ephemeral port: null when nothing answered in time. Sleeps.
    pub safe fn kernel_tcp_connect(dev: usize, ip: u32, port: u16) -> usize;
    /// `kernel_tcp_connect`, with bulk buffers both ways -- up to 512 KiB
    /// each, from the heap, the window scaled when the peer offers to: a
    /// connection that sends as much as it takes in. Each the slot's own
    /// 8 KiB when the heap has none to give.
    pub safe fn kernel_tcp_connect_bulk(dev: usize, ip: u32, port: u16) -> usize;
    pub safe fn kernel_tcp_accept(listener: usize, timeout_ms: u64) -> usize;
    pub safe fn kernel_tcp_close(conn: usize);
    /// A RST instead of the FIN exchange: the slot comes back at once, not
    /// after a minute of TIME-WAIT.
    pub safe fn kernel_tcp_abort(conn: usize);
    /// The bytes queued: 0 when timeout_ms found room for none, -1 once the
    /// connection is gone.
    pub fn kernel_tcp_send_timeout(
        conn: usize,
        buf: *const u8,
        len: usize,
        timeout_ms: u64,
    ) -> isize;
    /// The address at the other end, host byte order, and its port.
    pub fn kernel_tcp_peer(conn: usize, ip: *mut u32, port: *mut u16);

    /// `kernel_tcp_listen`, answering the `count` networks at `nets` alone:
    /// a segment from any other address for the port is dropped before the
    /// backlog and before a slot, with no SYN-ACK and no reset. Null for more
    /// than 64 networks or a prefix past 32, and for what kernel_tcp_listen
    /// refuses.
    pub fn kernel_tcp_listen_allowed(dev: usize, port: u16, backlog: usize, bulk: bool, nets: *const TcpNet,
                                     count: usize) -> usize;
    /// A listener's allowlist changed: the `count` networks at `nets` from
    /// now on -- anyone, with `any`. What it holds already stays. 0 done,
    /// -1 refused: no listener, more than 64 networks, a prefix past 32.
    pub fn kernel_tcp_set_allowed(listener: usize, nets: *const TcpNet, count: usize, any: bool) -> i32;
    /// A connection's own floor for its retransmission timeout, `ms`, from
    /// 20 up to the 200 every connection starts with: for a link both of
    /// whose ends are nos, which acknowledges every segment at once, where
    /// no peer delays its acknowledgements under that floor. In effect from
    /// the next round trip measured. 0 done, -1 refused.
    pub safe fn kernel_tcp_set_rto_floor(conn: usize, ms: u32) -> i32;

    /// A task waiting in `kernel_tcp_recv` on `conn` -- or the next to --
    /// back at once, as from a timeout: for one that waits on its connection
    /// and on something of its own, woken by whoever has that something.
    /// Nothing, unless `conn` is still the connection `generation` says.
    pub safe fn kernel_tcp_wake(conn: usize, generation: u64);
    /// What a wake of `conn` is held to: a number of its slot's, new for
    /// every connection the slot is handed out for. 0 for no connection.
    pub safe fn kernel_tcp_generation(conn: usize) -> u64;
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

/* The numbers that cross with these functions, one copy of each: the
 * layer that answers and kcore that reads them both use these, and this
 * file is in the module interface's digest. They were written twice --
 * a copy in the layer, literals in kcore -- outside it: a release that
 * renumbered one built, hashed the same, and its modules misread the
 * answers. */

/// What `kernel_tcp_recv` answers for a wait that ran out with nothing.
pub const TCP_RECV_TIMEOUT: isize = -2;
/// The most networks a listener's allowlist holds.
pub const TCP_ALLOW_MAX: usize = 64;
