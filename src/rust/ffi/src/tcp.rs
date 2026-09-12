use core::ffi::c_void;

/* The TLS client drives a connection the C++ side owns: these are the two
   calls it makes back into net/tcp.cpp. Both return the byte count, 0 at
   EOF, or a negative Tcp::Recv code. */
extern "C" {
    pub fn kernel_tcp_send(conn: *mut c_void, buf: *const u8, len: usize) -> isize;
    pub fn kernel_tcp_recv(
        conn: *mut c_void,
        buf: *mut u8,
        len: usize,
        timeout_ms: u64,
    ) -> isize;
}

/* A server of Rust's owns its connections: it listens on a port of a
   device -- a kernel_net_find handle -- accepts what arrives, and closes
   both what it accepted and the listener. Accepting returns null once
   timeout_ms (0: forever) passes with nobody, or once the listener is
   closed. */
extern "C" {
    pub fn kernel_tcp_listen(dev: usize, port: u16) -> *mut c_void;
    pub fn kernel_tcp_accept(listener: *mut c_void, timeout_ms: u64) -> *mut c_void;
    pub fn kernel_tcp_close(conn: *mut c_void);
    /// A RST instead of the FIN exchange: the slot comes back at once, not
    /// after a minute of TIME-WAIT.
    pub fn kernel_tcp_abort(conn: *mut c_void);
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
}
