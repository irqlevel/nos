//! A table kept in pieces of at most [`PIECE`] records, each shared until it
//! is written: a copy of the state -- the shell's view, copied at every turn
//! of the member, or a snapshot being written -- costs a handle a piece, and
//! a change copies the one piece it touches, and only while a copy holds it
//! (plans/nbs.md, "The mds's state on disk": taken without stopping). No
//! piece is ever bigger than a few KiB, where the kernel's heap gives no
//! block past 512 KiB.

use alloc::sync::Arc;
use alloc::vec::Vec;

/// Records a piece, at most.
pub const PIECE: usize = 1024;

#[derive(Clone, Debug)]
pub struct Pieces<T: Clone> {
    pieces: Vec<Arc<Vec<T>>>,
    len: usize,
}

impl<T: Clone> Default for Pieces<T> {
    fn default() -> Pieces<T> {
        Pieces { pieces: Vec::new(), len: 0 }
    }
}

impl<T: Clone> Pieces<T> {
    pub fn new() -> Pieces<T> {
        Pieces::default()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<&T> {
        if i >= self.len {
            return None;
        }
        self.pieces.get(i / PIECE)?.get(i % PIECE)
    }

    /// Record `i`, to change: its piece copied first where a copy of the
    /// table still holds it.
    #[inline]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        if i >= self.len {
            return None;
        }
        Arc::make_mut(self.pieces.get_mut(i / PIECE)?).get_mut(i % PIECE)
    }

    pub fn push(&mut self, v: T) {
        if self.len % PIECE == 0 {
            let mut piece = Vec::new();
            piece.push(v);
            self.pieces.push(Arc::new(piece));
        } else if let Some(last) = self.pieces.last_mut() {
            Arc::make_mut(last).push(v);
        }
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<T> {
        let last = self.pieces.last_mut()?;
        let v = Arc::make_mut(last).pop();
        if last.is_empty() {
            self.pieces.pop();
        }
        if v.is_some() {
            self.len -= 1;
        }
        v
    }

    /// Record `i` taken out, the last moved into its place: the order is
    /// the table's own, the same on every member.
    pub fn swap_remove(&mut self, i: usize) -> Option<T> {
        if i >= self.len {
            return None;
        }
        let last = self.pop()?;
        if i == self.len {
            return Some(last);
        }
        let slot = self.get_mut(i)?;
        Some(core::mem::replace(slot, last))
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.pieces.iter().flat_map(|p| p.iter())
    }
}

impl<T: Clone + PartialEq> PartialEq for Pieces<T> {
    fn eq(&self, other: &Pieces<T>) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Clone + Eq> Eq for Pieces<T> {}
