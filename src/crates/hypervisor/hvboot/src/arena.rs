//! The heap's arithmetic: first fit over the free blocks of one stretch of
//! memory, kept in address order, and a block given back merged with the
//! free ones either side of it -- so that what is given back is taken
//! again, in whatever order it comes back, and a stretch given back whole
//! is one block again. The heap was a bump allocator that took back only
//! its newest block: a reader that dropped a buffer while a newer one lived
//! -- the extent tree's walk a level at a time, the lexer's tokens growing
//! between the words they held -- left it behind for good, and a deep enough
//! tree or a long enough grub.cfg spent the heap. What is in use, and the
//! most ever, is counted: the harness (fuzz/hvboot) runs hvboot's own code
//! over this, its heap no bigger than [`HEAP_BYTES`].
//!
//! A block is a header -- its size, the header's included, and a word that
//! says it is in use -- and the bytes handed out after it. A free block's
//! second word is the offset of the next free block. Offsets, not
//! pointers, are what is kept: every one is checked against the stretch
//! before it is used, and a header that is not what it should be stops the
//! loader, said, rather than being written through.

use core::alloc::Layout;
use core::ptr;

/// hvboot's heap: what `heap.rs` gives this, and what the harness holds the
/// loader's code to.
pub const HEAP_BYTES: usize = 4 << 20;

/// Every block starts, and is as long as, a multiple of this, and the bytes
/// handed out are aligned to it at least.
const GRAIN: usize = 16;
/// A block's header: its size, then the in-use mark -- or, free, the next
/// free block's offset. A free block is a header and nothing else at least.
const HEADER: usize = 16;
const WORD: usize = core::mem::size_of::<usize>();
/// The end of the free list.
const NONE: usize = usize::MAX;
/// The second word of a block in use: odd, so never a free block's next.
const IN_USE: usize = 0x6876_626F_6F74_0001;

/// One stretch of memory, and the free blocks in it.
pub struct Arena {
    base: *mut u8,
    /// The stretch's length, a multiple of `GRAIN`; 0 until `init`.
    len: usize,
    /// The first free block's offset, or `NONE`.
    free: usize,
    /// Bytes in blocks in use, their headers' included, and the most ever.
    used: usize,
    peak: usize,
}

fn round_up(v: usize, to: usize) -> Option<usize> {
    Some(v.checked_add(to - 1)? & !(to - 1))
}

impl Arena {
    /// No memory yet: `init` gives it its stretch.
    pub const EMPTY: Arena = Arena { base: ptr::null_mut(), len: 0, free: NONE, used: 0, peak: 0 };

    /// Whether `init` has run.
    pub fn ready(&self) -> bool {
        self.len != 0
    }

    /// Give the arena `len` bytes at `base`, all of them free.
    ///
    /// # Safety
    /// `base` is aligned to 16 and `len` bytes from it are memory nothing
    /// else reads or writes for as long as the arena and the blocks it
    /// hands out are used; `init` runs once.
    pub unsafe fn init(&mut self, base: *mut u8, len: usize) {
        assert!(base as usize % GRAIN == 0 && len >= HEADER, "hvboot's heap: a stretch of {} bytes at {:p}", len, base);
        self.base = base;
        self.len = len & !(GRAIN - 1);
        self.free = 0;
        self.set_word(0, self.len);
        self.set_word(WORD, NONE);
    }

    /// Bytes in use now, headers included, and the most there ever were.
    pub fn usage(&self) -> (usize, usize) {
        (self.used, self.peak)
    }

    /// Whether `p` points into the arena's stretch.
    pub fn holds(&self, p: *const u8) -> bool {
        let (b, a) = (self.base as usize, p as usize);
        self.len != 0 && a >= b && a - b < self.len
    }

    /// The word at offset `at`: a header's, inside the stretch.
    fn word(&self, at: usize) -> usize {
        if at % WORD != 0 || at.checked_add(WORD).is_none_or(|end| end > self.len) {
            corrupt("a header outside it", at);
        }
        // SAFETY: inside the stretch `init` was given and aligned (the base
        // to 16, `at` to a word); a header is never in the bytes a block
        // hands out, so nothing else holds a reference to it.
        unsafe { self.base.add(at).cast::<usize>().read() }
    }

    fn set_word(&mut self, at: usize, v: usize) {
        if at % WORD != 0 || at.checked_add(WORD).is_none_or(|end| end > self.len) {
            corrupt("a header outside it", at);
        }
        // SAFETY: as `word`.
        unsafe { self.base.add(at).cast::<usize>().write(v) }
    }

    /// A free block's size and next, held to the stretch.
    fn free_block(&self, at: usize) -> (usize, usize) {
        let (size, next) = (self.word(at), self.word(at + WORD));
        if size < HEADER || size % GRAIN != 0 || at.checked_add(size).is_none_or(|end| end > self.len)
            || (next != NONE && next <= at + size)
        {
            corrupt("a free block that is not one", at);
        }
        (size, next)
    }

    /// Make `prev`'s next -- or the list's head -- `to`.
    fn link(&mut self, prev: usize, to: usize) {
        if prev == NONE {
            self.free = to;
        } else {
            self.set_word(prev + WORD, to);
        }
    }

    /// The block size `size` bytes take: a header and whole grains.
    fn block_for(size: usize) -> Option<usize> {
        round_up(size.max(1), GRAIN)?.checked_add(HEADER)
    }

    /// `layout`'s bytes, or null when no free block has room for them.
    pub fn alloc(&mut self, layout: Layout) -> *mut u8 {
        if self.len == 0 {
            return ptr::null_mut();
        }
        let align = layout.align().max(GRAIN);
        let Some(need) = Arena::block_for(layout.size()) else { return ptr::null_mut() };
        let base = self.base as usize;
        let (mut prev, mut at) = (NONE, self.free);
        while at != NONE {
            let (size, next) = self.free_block(at);
            /* Where in it a block can start whose bytes are aligned: the
             * free block's start, or a grain-aligned place further in,
             * the bytes before it left a free block of their own. */
            let start = base
                .checked_add(at + HEADER)
                .and_then(|a| round_up(a, align))
                .map(|a| a - HEADER - base);
            if let Some(b) = start.filter(|&b| b.checked_add(need).is_some_and(|end| end <= at + size)) {
                let tail = at + size - (b + need);
                if tail != 0 {
                    self.set_word(b + need, tail);
                    self.set_word(b + need + WORD, next);
                }
                let after = if tail != 0 { b + need } else { next };
                if b > at {
                    self.set_word(at, b - at);
                    self.set_word(at + WORD, after);
                } else {
                    self.link(prev, after);
                }
                self.set_word(b, need);
                self.set_word(b + WORD, IN_USE);
                self.used += need;
                self.peak = self.peak.max(self.used);
                return self.base.wrapping_add(b + HEADER);
            }
            prev = at;
            at = next;
        }
        ptr::null_mut()
    }

    /// The block `p` is the bytes of, and its size: a block in use, or the
    /// loader stops.
    fn block_of(&self, p: *mut u8) -> (usize, usize) {
        let offset = (p as usize).wrapping_sub(self.base as usize).wrapping_sub(HEADER);
        if !self.holds(p) || offset % GRAIN != 0 || offset >= self.len {
            corrupt("a pointer that is none of its blocks", offset);
        }
        if self.word(offset + WORD) != IN_USE {
            corrupt("a block given back that is not in use", offset);
        }
        let size = self.word(offset);
        if size < HEADER || size % GRAIN != 0 || offset.checked_add(size).is_none_or(|end| end > self.len) {
            corrupt("a block whose size is not one", offset);
        }
        (offset, size)
    }

    /// `[at, at + size)` free again, merged with a free block right before
    /// or right after it.
    fn give_back(&mut self, at: usize, size: usize) {
        let (mut prev, mut next) = (NONE, self.free);
        while next != NONE && next < at {
            prev = next;
            next = self.free_block(next).1;
        }
        if next != NONE && next < at + size {
            corrupt("a block given back over a free one", at);
        }
        let mut end = at + size;
        let mut after = next;
        if next != NONE && next == end {
            let (s, n) = self.free_block(next);
            end += s;
            after = n;
        }
        /* The in-use mark goes, whether or not the header stays a header:
         * a block given back twice is found, not merged twice. */
        self.set_word(at + WORD, after);
        if prev != NONE {
            let (s, _) = self.free_block(prev);
            if prev + s > at {
                corrupt("a block given back inside a free one", at);
            }
            if prev + s == at {
                self.set_word(prev, end - prev);
                self.set_word(prev + WORD, after);
                return;
            }
        }
        self.set_word(at, end - at);
        self.link(prev, at);
    }

    /// Give back the block `p` points into.
    ///
    /// # Safety
    /// `p` is what this arena's `alloc` or `realloc` returned, not given
    /// back since; nothing reads or writes its bytes after.
    pub unsafe fn dealloc(&mut self, p: *mut u8) {
        let (at, size) = self.block_of(p);
        self.used -= size;
        self.give_back(at, size);
    }

    /// The block `p` points into made `new_size` bytes: in place when it
    /// shrinks or the block after it is free and big enough, else moved,
    /// its bytes copied. Null -- the old block as it was -- when there is
    /// no room.
    ///
    /// # Safety
    /// As `dealloc`, and `layout` is the one `p` was handed out for.
    pub unsafe fn realloc(&mut self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let (at, size) = self.block_of(p);
        let Some(need) = Arena::block_for(new_size) else { return ptr::null_mut() };
        if need <= size {
            if size - need >= HEADER {
                self.set_word(at, need);
                self.used -= size - need;
                self.give_back(at + need, size - need);
            }
            return p;
        }
        /* The free block right after it, if any, and what before it in the
         * list links to it. */
        let (mut prev, mut next) = (NONE, self.free);
        while next != NONE && next < at + size {
            prev = next;
            next = self.free_block(next).1;
        }
        if next == at + size {
            let (s, n) = self.free_block(next);
            if size + s >= need {
                let rest = size + s - need;
                let grown = if rest >= HEADER {
                    self.set_word(at + need, rest);
                    self.set_word(at + need + WORD, n);
                    self.link(prev, at + need);
                    need
                } else {
                    self.link(prev, n);
                    size + s
                };
                self.set_word(at, grown);
                self.used += grown - size;
                self.peak = self.peak.max(self.used);
                return p;
            }
        }
        let Ok(moved) = Layout::from_size_align(new_size, layout.align()) else { return ptr::null_mut() };
        let new = self.alloc(moved);
        if !new.is_null() {
            // SAFETY: two blocks of this arena, both in use, so apart; the
            // old one has `layout.size()` bytes, the new one `new_size`.
            unsafe {
                ptr::copy_nonoverlapping(p, new, layout.size().min(new_size));
                self.dealloc(p);
            }
        }
        new
    }

    /// Every free block, in order -- (offset, size) -- into `out` as far as
    /// it goes, and how many there are: the harness's check that the arena
    /// is one block again once all is given back.
    #[allow(dead_code)]
    pub fn free_blocks(&self, out: &mut [(usize, usize)]) -> usize {
        let (mut n, mut at) = (0, self.free);
        while at != NONE {
            let (size, next) = self.free_block(at);
            if n < out.len() {
                out[n] = (at, size);
            }
            n += 1;
            at = next;
        }
        n
    }
}

#[cold]
fn corrupt(what: &str, at: usize) -> ! {
    panic!("hvboot's heap: {} at offset {:#x}", what, at)
}
