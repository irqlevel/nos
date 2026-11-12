//! A vector kept in chunks of a page or less: however long it grows, no one
//! allocation of it outgrows what the kernel's heap gives in one block --
//! 128 pages, 512 KiB -- nor needs a run of pages that size to be free. Its
//! table of chunks grows too, by a word and a half a chunk, which takes a
//! vector of millions of elements to reach a block.
//!
//! A guest's memory met the heap's limit twice before this: its nested
//! table's arena of tables, a `Vec` doubled to past 512 KiB at 16 GiB of
//! guest, and the table of its memory's 2 MiB pieces (`frame::Run`), one
//! allocation of 24 bytes a piece, at 42.7 GiB -- each a refusal that named
//! the wrong thing, on a machine with the memory free.

use alloc::vec::Vec;

use crate::consts::PAGE_SIZE;

pub struct Chunked<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T> Chunked<T> {
    /// Elements a chunk: a page's worth, one at least.
    const PER_CHUNK: usize = {
        let size = core::mem::size_of::<T>();
        if size == 0 || size >= PAGE_SIZE { 1 } else { PAGE_SIZE / size }
    };

    pub const fn new() -> Self {
        Self { chunks: Vec::new(), len: 0 }
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
        self.chunks.get(i / Self::PER_CHUNK)?.get(i % Self::PER_CHUNK)
    }

    #[inline]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        self.chunks.get_mut(i / Self::PER_CHUNK)?.get_mut(i % Self::PER_CHUNK)
    }

    /// `value` at the end, and its index -- or `value` back, nothing
    /// changed, when the heap has no room for a chunk or for the table of
    /// them.
    pub fn try_push(&mut self, value: T) -> Result<usize, T> {
        let at = self.len;
        if at % Self::PER_CHUNK == 0 {
            let mut chunk = Vec::new();
            if chunk.try_reserve_exact(Self::PER_CHUNK).is_err() || self.chunks.try_reserve(1).is_err() {
                return Err(value);
            }
            self.chunks.push(chunk);
        }
        /* The last chunk has room: a new one was reserved whole above, and
         * one not full has its capacity from then. */
        let Some(chunk) = self.chunks.last_mut() else { return Err(value) };
        chunk.push(value);
        self.len += 1;
        Ok(at)
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.chunks.iter().flat_map(|c| c.iter())
    }
}

impl<T> Default for Chunked<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> core::ops::Index<usize> for Chunked<T> {
    type Output = T;

    #[inline]
    fn index(&self, i: usize) -> &T {
        match self.get(i) {
            Some(t) => t,
            None => panic!("chunked: index {} past {}", i, self.len),
        }
    }
}

impl<T> core::ops::IndexMut<usize> for Chunked<T> {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut T {
        let len = self.len;
        match self.get_mut(i) {
            Some(t) => t,
            None => panic!("chunked: index {} past {}", i, len),
        }
    }
}
