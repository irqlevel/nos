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
    /// Listens on `port` of `nic`: on its address, or on any if it has none
    /// yet. Busy when someone listens on the port already, or every
    /// connection slot the kernel has is taken.
    pub fn bind(nic: &Nic, port: u16) -> Result<Self> {
        if port == 0 {
            return Err(Error::InvalidValue);
        }
        let conn = tcp::kernel_tcp_listen(nic.handle(), port);
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

/// A connection a `TcpListener` accepted: closed -- the FIN exchange left
/// to the kernel -- when dropped.
pub struct TcpStream {
    conn: *mut c_void,
}

/* One task uses it at a time; the kernel's calls take their own locks */
unsafe impl Send for TcpStream {}

impl TcpStream {
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
