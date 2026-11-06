use core::ffi::c_void;
use ffi::tcp;

use crate::error::{Error, Result};
use crate::net::Nic;

/// What a receive returns when its wait ran out with nothing received
/// (Tcp::Recv's TcpRecvTimeout).
pub const RECV_TIMEOUT: isize = -2;

/// No bound on the wait for room: what `send_all` sends with.
const WAIT_FOREVER_MS: u64 = 0;

fn send_all(conn: *mut c_void, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let sent = unsafe {
            tcp::kernel_tcp_send_timeout(conn, buf.as_ptr(), buf.len(), WAIT_FOREVER_MS)
        };
        if sent <= 0 {
            return false;
        }
        buf = &buf[sent as usize..];
    }
    true
}

fn recv(conn: *mut c_void, buf: &mut [u8], timeout_ms: u64) -> isize {
    if buf.is_empty() {
        return 0;
    }
    unsafe { tcp::kernel_tcp_recv(conn, buf.as_mut_ptr(), buf.len(), timeout_ms) }
}

/// The most networks an allowlisted listener answers.
pub const ALLOW_MAX: usize = 64;

/// `nets` as the C ABI takes them, on the stack.
fn nets_of(nets: &[(u32, u8)]) -> [tcp::TcpNet; ALLOW_MAX] {
    let mut list = [tcp::TcpNet::default(); ALLOW_MAX];
    for (l, &(ip, prefix)) in list.iter_mut().zip(nets) {
        l.ip = ip;
        l.prefix = prefix;
    }
    list
}

/// A TCP port listened on, on one device. Dropped, it closes the port: a
/// task waiting in `accept` gets None, and a peer that connected without
/// being accepted yet is reset.
pub struct TcpListener {
    conn: *mut c_void,
    port: u16,
}

/* The kernel's listen, accept and close take their own locks */
unsafe impl Send for TcpListener {}
unsafe impl Sync for TcpListener {}

impl TcpListener {
    /// Listens on `port` of `nic`: a connection that comes in by `nic`, for
    /// any of the machine's addresses -- and none that comes in by another
    /// device, so a guest behind the hypervisor's switch does not reach a
    /// server on eth0. Busy when someone listens on the port already, or
    /// every connection slot the kernel has is taken.
    ///
    /// `backlog` is how many connections nobody has accepted yet it holds
    /// at most (taken into 1..=16): past them a SYN is dropped, and its peer
    /// tries again a second later. Each is one of the 64 connection slots
    /// the whole kernel has, so a server that accepts few at a time asks for
    /// few, and a flood of SYNs at its port costs the kernel no more.
    pub fn bind(nic: &Nic, port: u16, backlog: usize) -> Result<Self> {
        Self::listen(nic, port, backlog, false)
    }

    /// `bind`, each connection it accepts given bulk buffers both ways --
    /// up to 512 KiB each, from the heap, as it is accepted, its window
    /// scaled when the peer offers to: what a relay carrying downloads over
    /// a long round trip needs, which 8 KiB a round trip holds to 130 KiB/s
    /// at 62 ms. The slot's own 8 KiB when the heap has none to give.
    pub fn bind_bulk(nic: &Nic, port: u16, backlog: usize) -> Result<Self> {
        Self::listen(nic, port, backlog, true)
    }

    /// `bind` -- `bind_bulk` with `bulk` -- answering the networks `nets`
    /// alone, each an address (host order) and a prefix length: a segment
    /// from any other address for the port is dropped before the backlog
    /// and before a slot, with no SYN-ACK and no reset, so to a stranger the
    /// port is not there at all. At most 64 networks.
    pub fn bind_allowed(nic: &Nic, port: u16, backlog: usize, bulk: bool, nets: &[(u32, u8)]) -> Result<Self> {
        if port == 0 || nets.len() > ALLOW_MAX {
            return Err(Error::InvalidValue);
        }
        let list = nets_of(nets);
        /* `list` holds `nets.len()` TcpNets */
        let conn = unsafe { tcp::kernel_tcp_listen_allowed(nic.handle(), port, backlog, bulk, list.as_ptr(),
                                                           nets.len()) };
        if conn.is_null() {
            Err(Error::Busy)
        } else {
            Ok(Self { conn, port })
        }
    }

    /// The networks it answers from now on -- anyone, for None. What it
    /// holds already stays.
    pub fn set_allowed(&self, nets: Option<&[(u32, u8)]>) -> Result<()> {
        let code = match nets {
            None => unsafe { tcp::kernel_tcp_set_allowed(self.conn, core::ptr::null(), 0, true) },
            Some(nets) => {
                if nets.len() > ALLOW_MAX {
                    return Err(Error::InvalidValue);
                }
                let list = nets_of(nets);
                /* `list` holds `nets.len()` TcpNets */
                unsafe { tcp::kernel_tcp_set_allowed(self.conn, list.as_ptr(), nets.len(), false) }
            }
        };
        if code == 0 { Ok(()) } else { Err(Error::InvalidValue) }
    }

    fn listen(nic: &Nic, port: u16, backlog: usize, bulk: bool) -> Result<Self> {
        if port == 0 {
            return Err(Error::InvalidValue);
        }
        let conn = tcp::kernel_tcp_listen(nic.handle(), port, backlog, bulk);
        if conn.is_null() {
            Err(Error::Busy)
        } else {
            Ok(Self { conn, port })
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The next connection, or None once `timeout_ms` (0: forever) passes
    /// with nobody -- or the listener has been closed.
    pub fn accept(&self, timeout_ms: u64) -> Option<TcpStream> {
        let conn = tcp::kernel_tcp_accept(self.conn, timeout_ms);
        if conn.is_null() {
            None
        } else {
            Some(TcpStream { conn })
        }
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        tcp::kernel_tcp_close(self.conn)
    }
}

/// What wakes a task waiting in `recv` on a stream -- from another task,
/// one with something else for it to do: that receive comes back at once,
/// as from its timeout, or the next one does. Copied freely; a wake after the
/// stream is gone reaches nobody, the kernel holding it to the connection's
/// generation.
#[derive(Clone, Copy)]
pub struct TcpWaker {
    conn: usize,
    generation: u64,
}

impl TcpWaker {
    pub fn wake(&self) {
        tcp::kernel_tcp_wake(self.conn as *mut c_void, self.generation);
    }
}

/// A connection a `TcpListener` accepted: closed -- the FIN exchange left
/// to the kernel -- when dropped.
pub struct TcpStream {
    conn: *mut c_void,
}

/* One task uses it at a time; the kernel's calls take their own locks */
unsafe impl Send for TcpStream {}

impl TcpStream {
    /// What wakes a task waiting in this stream's `recv`, from another task.
    pub fn waker(&self) -> TcpWaker {
        TcpWaker { conn: self.conn as usize, generation: tcp::kernel_tcp_generation(self.conn) }
    }

    /// Its own floor for its retransmission timeout, `ms` (20 to 200): for
    /// a link both of whose ends are nos, which acknowledges every segment
    /// at once, below the 200 ms a connection to anyone else keeps, whose
    /// peer may delay its acknowledgements that long. In effect from the
    /// next round trip measured.
    pub fn set_rto_floor(&self, ms: u32) -> Result<()> {
        if tcp::kernel_tcp_set_rto_floor(self.conn, ms) == 0 { Ok(()) } else { Err(Error::InvalidValue) }
    }

    /// An active open to `ip`:`port` (host byte order) through `nic`, from an
    /// ephemeral port: None when nothing answered in time. Sleeps.
    pub fn connect(nic: &Nic, ip: u32, port: u16) -> Option<TcpStream> {
        let conn = tcp::kernel_tcp_connect(nic.handle(), ip, port);
        if conn.is_null() { None } else { Some(TcpStream { conn }) }
    }

    /// `connect`, with bulk buffers both ways -- up to 512 KiB each, from
    /// the heap, the window scaled when the peer offers to: a connection that
    /// sends as much as it takes in, which 8 KiB in flight would hold to
    /// some 27 MB/s at a data centre's 0.3 ms round trip. The slot's own 8
    /// KiB where the heap has none to give.
    pub fn connect_bulk(nic: &Nic, ip: u32, port: u16) -> Option<TcpStream> {
        let conn = tcp::kernel_tcp_connect_bulk(nic.handle(), ip, port);
        if conn.is_null() { None } else { Some(TcpStream { conn }) }
    }

    /// Sends the whole buffer, waiting for room as long as it takes; false
    /// if the connection failed part way.
    pub fn send_all(&mut self, buf: &[u8]) -> bool {
        send_all(self.conn, buf)
    }

    /// Queues what there is room for within `timeout_ms`: the count queued,
    /// 0 when there was room for none, negative once the connection is
    /// gone. For a sender that has to notice something else meanwhile.
    pub fn send(&mut self, buf: &[u8], timeout_ms: u64) -> isize {
        if buf.is_empty() {
            return 0;
        }
        unsafe { tcp::kernel_tcp_send_timeout(self.conn, buf.as_ptr(), buf.len(), timeout_ms) }
    }

    /// Bytes read, 0 at the end of the stream, `RECV_TIMEOUT` when
    /// `timeout_ms` (0: forever) passed with nothing, other negatives on
    /// error.
    pub fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> isize {
        recv(self.conn, buf, timeout_ms)
    }

    /// The address at the other end, host byte order, and its port.
    pub fn peer(&self) -> (u32, u16) {
        let mut ip = 0u32;
        let mut port = 0u16;
        unsafe { tcp::kernel_tcp_peer(self.conn, &mut ip, &mut port) };
        (ip, port)
    }

    /// Ends the connection with a RST rather than the FIN exchange, for one
    /// a server refuses or drops: closed the usual way, the side that closes
    /// first keeps the slot for a minute of TIME-WAIT, and a server that
    /// closes first on everyone it turns away runs the kernel out of them.
    pub fn abort(self) {
        let conn = self.conn;
        core::mem::forget(self);
        tcp::kernel_tcp_abort(conn)
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        tcp::kernel_tcp_close(self.conn)
    }
}
