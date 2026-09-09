/* The TLS client drives a connection the C++ side owns: these are the two
   calls it makes back into net/tcp.cpp. Both return the byte count, 0 at
   EOF, or a negative Tcp::Recv code. */
extern "C" {
    pub fn kernel_tcp_send(conn: *mut core::ffi::c_void, buf: *const u8, len: usize) -> isize;
    pub fn kernel_tcp_recv(
        conn: *mut core::ffi::c_void,
        buf: *mut u8,
        len: usize,
        timeout_ms: u64,
    ) -> isize;
}
