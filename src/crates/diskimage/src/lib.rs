//! Disk images of other hypervisors' formats, made raw: what a cloud image
//! is shipped as -- QEMU's qcow2, Ubuntu's and Debian's and Fedora's -- read
//! and written out as the disk it describes, the one file a guest of hv's
//! is given (`hv convert`, docs/hypervisor.md).
//!
//! - [`detect`] says what the first bytes of a file are, so that a file of
//!   any other kind is refused by name -- a VMDK, a gzip stream, a disk
//!   that is raw already -- rather than as a damaged qcow2;
//! - [`qcow2`] reads QEMU's format and converts it, cluster by cluster, its
//!   compressed clusters through [`inflate`];
//! - what is written goes through a writer that leaves every block of zeros
//!   unwritten, so that the disk keeps the image's holes: a 3.5 GiB Ubuntu
//!   disk costs the 1.7 GiB it holds.
//!
//! The image is a download, and untrusted: everything it says is checked
//! before it is used, and nothing it says makes this read, write or
//! allocate more than a piece ([`CHUNK`]) at once, whatever its cluster
//! size. It reads through a [`Source`] and writes through a [`Sink`] of the
//! caller's -- a file of nos's in the hv module, a host file in the fuzzer.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod detect;
pub mod inflate;
pub mod iso;
pub mod qcow2;
mod sparse;

pub use detect::{detect, Format};

/// The most this reads, or writes, or holds of anything at once: a piece of
/// a table, of a cluster, of output on its way.
pub const CHUNK: usize = 64 * 1024;

/// What an image is read from: a file of the caller's, by offset.
pub trait Source {
    type Error;
    /// The file's length in bytes.
    fn len(&self) -> u64;
    /// Exactly `buf.len()` bytes from `offset`, which the caller keeps
    /// inside the file: `offset + buf.len() <= len()`.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
}

/// Where the raw disk goes: a file as long as the disk, reading as zeros
/// wherever nothing has been written to it -- a new file, grown to its size
/// with a hole. Written in the order of its offsets, each byte at most once.
pub trait Sink {
    type Error;
    /// `data` at `offset`, all of it inside the disk.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), Self::Error>;
}

/// A buffer of `len` zeros, or None out of memory: what this holds is
/// taken so, never by an allocation that would panic.
pub(crate) fn buffer(len: usize) -> Option<alloc::vec::Vec<u8>> {
    let mut v = alloc::vec::Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, 0);
    Some(v)
}
