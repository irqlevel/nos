//! The raw disk, written a piece at a time in the order of its offsets:
//! every block that is all zeros left unwritten -- the disk reads as zeros
//! there already, and a filesystem gives a block never written no room --
//! and the rest gathered into writes of up to CHUNK bytes.

use alloc::vec::Vec;

use crate::{Sink, CHUNK};

/// What is looked at for zeros at once, on its boundaries: a block of
/// nos's ext2 as `mkfs` makes it, so that a block of zeros left unwritten
/// is one the file never takes.
const BLOCK: u64 = 4096;

pub(crate) struct Sparse<'a, D: Sink> {
    disk: &'a mut D,
    /// What is gathered to go at `at`.
    pending: Vec<u8>,
    at: u64,
    /// Where the last piece ended: the next starts there or later.
    end: u64,
    size: u64,
    written: u64,
}

impl<'a, D: Sink> Sparse<'a, D> {
    /// A writer of a disk of `size` bytes, or None out of memory.
    pub(crate) fn new(disk: &'a mut D, size: u64) -> Option<Self> {
        let mut pending = Vec::new();
        pending.try_reserve_exact(CHUNK).ok()?;
        Some(Sparse { disk, pending, at: 0, end: 0, size, written: 0 })
    }

    /// `data`, the disk's at `offset`: after what came before, and inside
    /// the disk -- which the converter's own arithmetic keeps it, whatever
    /// the image says, so that anything else is a bug of its own.
    pub(crate) fn put(&mut self, offset: u64, data: &[u8]) -> Result<(), D::Error> {
        assert!(offset >= self.end && offset.checked_add(data.len() as u64).is_some_and(|end| end <= self.size),
                "diskimage: a piece of the disk out of order, or outside it");
        let mut pos = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let edge = (pos / BLOCK + 1) * BLOCK;
            let take = ((edge - pos) as usize).min(rest.len());
            let (piece, tail) = rest.split_at(take);
            if zeros(piece) {
                self.flush()?;
            } else {
                if !self.pending.is_empty()
                    && (self.at + self.pending.len() as u64 != pos || self.pending.len() + piece.len() > CHUNK)
                {
                    self.flush()?;
                }
                if self.pending.is_empty() {
                    self.at = pos;
                }
                /* Into the room taken at `new`: a piece is at most a block,
                 * and what is gathered goes before it would pass CHUNK. */
                self.pending.extend_from_slice(piece);
            }
            pos += take as u64;
            rest = tail;
        }
        self.end = pos;
        Ok(())
    }

    /// What is gathered, written.
    pub(crate) fn flush(&mut self) -> Result<(), D::Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.disk.write_at(self.at, &self.pending)?;
        self.written += self.pending.len() as u64;
        self.pending.clear();
        Ok(())
    }

    /// The bytes written so far.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }
}

/// Whether every byte is 0: a word at a time.
fn zeros(bytes: &[u8]) -> bool {
    let mut words = bytes.chunks_exact(8);
    words.by_ref().all(|w| u64::from_ne_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]) == 0)
        && words.remainder().iter().all(|&b| b == 0)
}
