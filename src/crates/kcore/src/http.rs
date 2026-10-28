//! A download, for a module: what a URL answers with, by HTTP or HTTPS, in a
//! new file -- through the image's own client, with TLS and the bulk receive
//! window a module's own TCP connection is not given (`net::wget::fetch`).

use core::ffi::c_void;
use core::fmt;

use ffi::http;

/// Why a download left no file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a URL the client fetches.
    Url,
    Resolve,
    Connect,
    Tls,
    /// The request did not go, or no whole answer came back.
    Answer,
    Redirects,
    NoMemory,
    NoDevice,
    /// A file is at the path already.
    Exists,
    /// The file could not be made.
    Create,
    /// A write failed.
    Write,
    /// The body was cut short.
    Cut,
    /// The caller's progress said to stop.
    Cancelled,
    /// An answer that is no body: the status, not 2xx.
    Status(u16),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Url => f.write_str("not a URL this fetches -- http:// or https://, under 2048 characters"),
            Error::Resolve => f.write_str("its host has no address the resolver found -- is DNS on (dns=on)?"),
            Error::Connect => f.write_str("no connection to its host"),
            Error::Tls => f.write_str("the TLS handshake was refused -- a certificate that did not verify (is the \
                                       clock right?), or no protocol in common"),
            Error::Answer => f.write_str("no answer came back"),
            Error::Redirects => f.write_str("one redirect too many"),
            Error::NoMemory => f.write_str("out of memory"),
            Error::NoDevice => f.write_str("there is no network device (eth0) to fetch it through"),
            Error::Exists => f.write_str("a file is there already -- not written over"),
            Error::Create => f.write_str("the file could not be made"),
            Error::Write => f.write_str("writing the file failed -- is the filesystem full?"),
            Error::Cut => f.write_str("the body was cut short -- the server went quiet, or closed early"),
            Error::Cancelled => f.write_str("stopped, as asked"),
            Error::Status(404) => f.write_str("the server answered 404 -- there is nothing at that URL"),
            Error::Status(status) => write!(f, "the server answered {}, not a body", status),
        }
    }
}

type Progress<'a> = &'a mut dyn FnMut(u64, u64) -> bool;

/// # Safety
/// `ctx` is the `&mut Progress` `fetch` passed, alive until it returns --
/// and the download calls this only until then.
unsafe extern "C" fn progress_trampoline(ctx: *mut c_void, done: u64, total: u64) -> bool {
    let progress = unsafe { &mut *(ctx as *mut Progress) };
    progress(done, total)
}

/// What `url` answers with, by HTTP or HTTPS, in a new file at `path` --
/// never one that is there -- `progress` handed the bytes in the file so far
/// and the body's length (0 when the server did not say) as they come, and
/// asked whether to go on: the body's length, or why not, and then no file.
/// Only a 2xx answer is a body. Sleeps as long as the download takes; a stop
/// is seen as the next of the body comes, so a server gone quiet holds it
/// for the client's idle time.
pub fn fetch(url: &str, path: &str, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<u64, Error> {
    let mut progress: Progress = progress;
    let ctx = &mut progress as *mut Progress as *mut c_void;
    /* The strings are readable for their lengths, and `ctx` is the progress
     * the trampoline takes it for, alive until the call returns. */
    let got = unsafe {
        http::kernel_http_fetch(url.as_ptr(), url.len(), path.as_ptr(), path.len(), progress_trampoline, ctx)
    };
    if let Ok(len) = u64::try_from(got) {
        return Ok(len);
    }
    Err(match got {
        http::HTTP_FETCH_URL => Error::Url,
        http::HTTP_FETCH_RESOLVE => Error::Resolve,
        http::HTTP_FETCH_CONNECT => Error::Connect,
        http::HTTP_FETCH_TLS => Error::Tls,
        http::HTTP_FETCH_ANSWER => Error::Answer,
        http::HTTP_FETCH_REDIRECTS => Error::Redirects,
        http::HTTP_FETCH_NO_MEMORY => Error::NoMemory,
        http::HTTP_FETCH_NO_DEVICE => Error::NoDevice,
        http::HTTP_FETCH_EXISTS => Error::Exists,
        http::HTTP_FETCH_CREATE => Error::Create,
        http::HTTP_FETCH_WRITE => Error::Write,
        http::HTTP_FETCH_CUT => Error::Cut,
        http::HTTP_FETCH_CANCELLED => Error::Cancelled,
        code => match u16::try_from(http::HTTP_FETCH_STATUS - code) {
            Ok(status) if code <= http::HTTP_FETCH_STATUS => Error::Status(status),
            _ => Error::Answer,
        },
    })
}
