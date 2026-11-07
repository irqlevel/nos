//! HTTP/1.1 and WebSocket as nos's host API serves them, with nothing of the
//! kernel in it:
//!
//! - [`http`]: a request read where it lies in the connection's buffer --
//!   its line, its headers and its body each held to a limit, nothing
//!   allocated -- and a response's head written with no header a value
//!   could inject;
//! - [`ws`]: the WebSocket handshake and its frames (RFC 6455): a client's
//!   frames masked, every length checked, control frames short and whole,
//!   text valid UTF-8, a message no larger than its limit;
//! - [`sha1`]: SHA-1, for the handshake's accept key and nothing that
//!   trusts it;
//! - [`cidr`]: IPv4 networks, for an allowlist;
//! - [`x509`]: a self-signed certificate made for a P-256 key, its key in
//!   PKCS#8, PEM both ways, and fingerprints.
//!
//! It runs in the kernel, so nothing here allocates without being able to
//! fail: what would need memory that is not there is refused rather than
//! panic the machine. And it answers whoever connects, so nothing a request
//! holds is used as a length, an index or a count before it is checked.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cidr;
pub mod http;
pub mod sha1;
pub mod ws;
pub mod x509;
