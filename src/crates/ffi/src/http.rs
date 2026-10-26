use core::ffi::c_void;

/* HTTP as a loadable module reaches it (src/crates/net/src/abi.rs defines
   it): the image's own client, over TLS where the URL says https and a TCP
   connection with the bulk receive window, which a module's own
   kernel_tcp_connect is not given. What it returns: the body's length, or
   one of these. */
pub const HTTP_FETCH_URL: i64 = -1;
pub const HTTP_FETCH_RESOLVE: i64 = -2;
pub const HTTP_FETCH_CONNECT: i64 = -3;
pub const HTTP_FETCH_TLS: i64 = -4;
pub const HTTP_FETCH_ANSWER: i64 = -5;
pub const HTTP_FETCH_REDIRECTS: i64 = -6;
pub const HTTP_FETCH_NO_MEMORY: i64 = -7;
pub const HTTP_FETCH_NO_DEVICE: i64 = -8;
pub const HTTP_FETCH_EXISTS: i64 = -9;
pub const HTTP_FETCH_CREATE: i64 = -10;
pub const HTTP_FETCH_WRITE: i64 = -11;
pub const HTTP_FETCH_CUT: i64 = -12;
/// An answer that is no body -- not 2xx -- is this less its status: -1404
/// for a 404.
pub const HTTP_FETCH_STATUS: i64 = -1000;

unsafe extern "C" {
    /// The body `url` answers with, into a new file at `path` -- never one
    /// that is there -- its progress printed on `out`, the printer a command
    /// handler was given, a "download:" line every few seconds: the body's
    /// length, or an HTTP_FETCH_ code, and then no file. Sleeps for as long
    /// as the download takes: task context, no lock held.
    pub fn kernel_http_fetch(
        url: *const u8,
        url_len: usize,
        path: *const u8,
        path_len: usize,
        out: *mut c_void,
    ) -> i64;
}
