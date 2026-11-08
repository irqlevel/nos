//! What `ipc ping` sends, and how a message is held to it: by the ping, the
//! answer that comes back; by `kernel.echo`, the message as it came. Every
//! 8 bytes of a ping -- a word, little-endian -- hold the message's
//! reference and the word's own index in it, mixed, so that bytes that are
//! not where their sender put them say whose they are and where they were:
//! another message's (by the reference), the message's own from elsewhere
//! (by the index), or neither -- a page of a heap block another owner wrote,
//! a frame handed out twice.

use crate::Payload;
use alloc::vec::Vec;
use core::fmt;

/// What `ipc ping` sends: "PIN", bulk's bit clear -- `bulk` sets it.
pub(crate) const TAG: u32 = 0x0050_494E;
/// A ping's bytes are made, and a message's compared, a piece at a time: a
/// megabyte is more than one block of the heap holds. A multiple of a word,
/// so that no word of a ping is ever cut between two pieces.
pub(crate) const PIECE: usize = 64 * 1024;
const _: () = assert!(PIECE % 8 == 0);
/// What every word is mixed with: so that zeroes, or the bytes of anything
/// but a ping, never read as one.
const MIX: u64 = 0x5049_4E47_A5C3_96E1;

/// The word a ping has at index `w`.
fn word(reference: u64, w: usize) -> u64 {
    ((reference << 32) | (w as u64 & 0xFFFF_FFFF)) ^ MIX
}

/// `pieces` -- a message's bytes, every piece but the last a multiple of a
/// word -- written as the ping `reference`'s.
pub(crate) fn fill(pieces: &mut [Vec<u8>], reference: u64) {
    let mut base = 0usize;
    for p in pieces.iter_mut() {
        for (k, chunk) in p.chunks_mut(8).enumerate() {
            let w = word(reference, base / 8 + k).to_le_bytes();
            chunk.copy_from_slice(&w[..chunk.len()]);
        }
        base += p.len();
    }
}

/// How a message is not the bytes its ping was made of: its length, the
/// words not as made, in how many runs, and the first -- where, and what it
/// held -- and the last.
pub(crate) struct Wrong {
    pub len: usize,
    pub words: usize,
    pub runs: usize,
    pub first: Option<(usize, u64)>,
    pub last: usize,
}

/// Whether `p` holds the `len` bytes the ping `reference` was made of, read
/// a piece at a time into `scratch` (a multiple of a word, or the whole
/// ping): how it is not, if it is not.
pub(crate) fn check(p: &Payload, len: usize, reference: u64, scratch: &mut [u8]) -> Option<Wrong> {
    let mut w = Wrong { len: p.len(), words: 0, runs: 0, first: None, last: 0 };
    let common = len.min(p.len());
    let mut in_run = false;
    let mut at = 0;
    while at < common {
        let want = scratch.len().min(common - at);
        let n = p.read(at, &mut scratch[..want]);
        if n == 0 {
            break;
        }
        for (k, chunk) in scratch[..n].chunks(8).enumerate() {
            let off = at + k * 8;
            let made = word(reference, off / 8).to_le_bytes();
            if chunk == &made[..chunk.len()] {
                in_run = false;
                continue;
            }
            let mut held = [0u8; 8];
            held[..chunk.len()].copy_from_slice(chunk);
            w.words += 1;
            if !in_run {
                w.runs += 1;
                in_run = true;
            }
            if w.first.is_none() {
                w.first = Some((off, u64::from_le_bytes(held)));
            }
            w.last = off;
        }
        at += n;
    }
    (w.words > 0 || w.len != len).then_some(w)
}

impl Wrong {
    /// What it is, said: `made` the length the ping was made with.
    pub(crate) fn said(&self, made: usize) -> Said<'_> {
        Said { w: self, made }
    }
}

/// `Wrong::said`'s.
pub(crate) struct Said<'a> {
    w: &'a Wrong,
    made: usize,
}

impl fmt::Display for Said<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let w = self.w;
        write!(f, "{} bytes (made {}), {} words not as made in {} runs", w.len, self.made, w.words, w.runs)?;
        if let Some((off, held)) = w.first {
            let h = held ^ MIX;
            write!(f, ", the first at byte {} holding {:#018x} -- as a ping's, reference {} word {} (this word is \
                       {}), the last at byte {}", off, held, h >> 32, h & 0xFFFF_FFFF, off / 8, w.last)?;
        }
        Ok(())
    }
}
