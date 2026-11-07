//! The device a cs's engine runs over: the caller's. nbs-cs.ko's is a
//! device's queue (`kcore::block::DiskQueue`), its requests waited out one
//! at a time; fuzz/nbs's is a disk with a write cache that a power cut
//! loses or tears.
//!
//! A write may sit in the device's cache until a flush: only a flush makes
//! what was written before it durable. A write is torn, if at all, at its
//! sectors' edges: a sector is the device's unit of writing.

/// A request the device could not do: the engine takes the device to have
/// failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoError;

pub trait Device {
    /// Its size, in bytes.
    fn bytes(&self) -> u64;
    /// The sector, in bytes: 512 or 4096.
    fn sector(&self) -> u64;
    /// `buf.len()` bytes from `offset`: whole sectors.
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError>;
    /// `data` at `offset`: whole sectors.
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), IoError>;
    /// What was written before it, durable.
    fn flush(&mut self) -> Result<(), IoError>;
    /// Zeros over `len` bytes from `offset`, as a write is -- the device's
    /// own command where it has one.
    fn zero(&mut self, offset: u64, len: u64) -> Result<(), IoError> {
        /* Off the heap: a kernel task's stack is no place for it */
        const BLOCK: usize = 64 * 1024;
        let zeros = alloc::vec![0u8; BLOCK];
        let mut at = 0u64;
        while at < len {
            let n = (len - at).min(BLOCK as u64) as usize;
            self.write(offset + at, &zeros[..n])?;
            at += n as u64;
        }
        Ok(())
    }
}
