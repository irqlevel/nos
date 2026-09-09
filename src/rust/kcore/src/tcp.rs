use core::ffi::c_void;
use ffi::tcp;

/* A TCP connection opened and owned by the C++ side. The handle is passed
   in, used, and never closed here: whoever opened it closes it. */
pub struct TcpSocket {
    conn: *mut c_void,
}

impl TcpSocket {
    /// # Safety
    /// `conn` must be a live `Kernel::TcpConn*` that outlives this socket.
    pub unsafe fn from_raw(conn: *mut c_void) -> Self {
        Self { conn }
    }

    /// Sends the whole buffer; false if the connection failed part way.
    pub fn send_all(&mut self, mut buf: &[u8]) -> bool {
        while !buf.is_empty() {
            let sent = unsafe { tcp::kernel_tcp_send(self.conn, buf.as_ptr(), buf.len()) };
            if sent <= 0 {
                return false;
            }
            buf = &buf[sent as usize..];
        }
        true
    }

    /// Bytes read, 0 at EOF, negative on error or timeout.
    pub fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> isize {
        if buf.is_empty() {
            return 0;
        }
        unsafe { tcp::kernel_tcp_recv(self.conn, buf.as_mut_ptr(), buf.len(), timeout_ms) }
    }
}
