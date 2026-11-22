//! The dirty log (plans/vm-migration.md, M2): which pages of a guest's
//! memory were written since a migration last looked, as the guest goes on
//! running and its memory is copied from under it, round after round.
//!
//! - **The host's writes** into the guest's memory -- a disk's read, a frame
//!   received, a ring's index -- are marked as they are made, after the
//!   bytes (`Log::wrote`), always: a mark is one atomic `or` beside a copy,
//!   and a log that marked only once it was on would have to order every
//!   such copy against its start.
//! - **The guest's own writes** are found by the nested table: each page of
//!   its RAM is read-only there while the log looks after it, so that the
//!   guest's first write to it is a nested fault -- where the page is marked
//!   and made writable again, and the write runs again (`Log::fault`).
//! - **A round** takes the marks and makes their pages read-only again, the
//!   two as one step against the faults, under the table's lock (`take`):
//!   a fault's mark lands either in the marks taken, its page protected
//!   after it, or in the next round's. The caller then has every CPU drop
//!   what its TLB keeps of the table -- a new table id, which each CPU of
//!   the guest takes before it enters again (`run.rs`) -- and only then reads
//!   the pages: a translation still writable in a TLB would let a write by
//!   unmarked, after its page was read.
//!
//! The table is behind a trait, and nothing here is the kernel's: hv-fuzz
//! runs it as it is, over a table and TLBs of its own, against host writers
//! and the guest's CPUs (`dirtylog`).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const PAGE: u64 = 4096;
const BITS: u64 = 64;
/// The words a piece of a run's marks holds: a page of them. In pieces, as
/// the nested table keeps its tables: one `Vec` of a run's every word was
/// past the heap's largest block, 512 KiB, for a run of RAM past 16 GiB --
/// and a guest that big could not be built.
const PIECE_WORDS: usize = 512;
/// The pages a piece's marks are of: where a run's marks go on in the next
/// piece (hv-fuzz marks either side of it).
pub const PIECE_PAGES: u64 = PIECE_WORDS as u64 * BITS;

/// The nested table's side: a page of the guest's RAM made read-only to the
/// guest -- a 2 MiB leaf it is in split first -- or writable again. False
/// where it could not be: no page the table maps, or no memory for the
/// split.
pub trait Table {
    fn protect(&mut self, gpa: u64) -> bool;
    fn unprotect(&mut self, gpa: u64) -> bool;
}

/// One run of the guest's RAM, a bit a page, in pieces of `PIECE_WORDS`
/// words.
struct Bits {
    base: u64,
    pages: u64,
    pieces: Vec<Vec<AtomicU64>>,
}

impl Bits {
    /// The word and bit of the page `gpa` is in, where it is this run's.
    #[inline]
    fn at(&self, gpa: u64) -> Option<(&AtomicU64, u64)> {
        let page = gpa.checked_sub(self.base)? / PAGE;
        if page >= self.pages {
            return None;
        }
        let piece = usize::try_from(page / PIECE_PAGES).ok()?;
        let within = page % PIECE_PAGES;
        let w = self.pieces.get(piece)?.get((within / BITS) as usize)?;
        Some((w, 1u64 << (within % BITS)))
    }

    /// Every word, in order, and the address of the page its bit 0 is.
    fn words(&self) -> impl Iterator<Item = (u64, &AtomicU64)> + '_ {
        let base = self.base;
        self.pieces.iter().flatten().enumerate().map(move |(i, w)| (base + i as u64 * BITS * PAGE, w))
    }
}

/// The marks of a guest's RAM: made with the guest, before it runs.
#[derive(Default)]
pub struct Log {
    regions: Vec<Bits>,
    /// Between `start` and `release`: a movable page is mapped read-only.
    active: AtomicBool,
}

impl Log {
    pub fn new() -> Log {
        Log { regions: Vec::new(), active: AtomicBool::new(false) }
    }

    /// Whether guest writes are being found by read-only nested mappings.
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// A run of RAM, `size` bytes from `base`, both whole pages: its marks,
    /// none set. False out of memory.
    pub fn add(&mut self, base: u64, size: u64) -> bool {
        let pages = size / PAGE;
        let Ok(mut n) = usize::try_from(pages.div_ceil(BITS)) else { return false };
        let mut pieces = Vec::new();
        if pieces.try_reserve_exact(n.div_ceil(PIECE_WORDS)).is_err() || self.regions.try_reserve(1).is_err() {
            return false;
        }
        while n != 0 {
            let k = n.min(PIECE_WORDS);
            let mut words = Vec::new();
            if words.try_reserve_exact(k).is_err() {
                return false;
            }
            words.resize_with(k, || AtomicU64::new(0));
            pieces.push(words);
            n -= k;
        }
        self.regions.push(Bits { base, pages, pieces });
        true
    }

    fn bits(&self, gpa: u64) -> Option<(&AtomicU64, u64)> {
        self.regions.iter().find_map(|r| r.at(gpa))
    }

    /// The host wrote `len` bytes at `gpa`: their pages marked -- after the
    /// bytes, so that a round that finds a mark finds them too.
    #[inline]
    pub fn wrote(&self, gpa: u64, len: u64) {
        if len == 0 {
            return;
        }
        let Some(last) = gpa.checked_add(len - 1) else { return };
        let mut page = gpa & !(PAGE - 1);
        while page <= last {
            if let Some((word, bit)) = self.bits(page) {
                word.fetch_or(bit, Ordering::Release);
            }
            match page.checked_add(PAGE) {
                Some(next) => page = next,
                None => break,
            }
        }
    }

    /// The guest's write to `gpa`, a fault of the nested table's -- `table`
    /// locked by the caller for this and every other step of the log's: the
    /// page marked and made writable again. True where it is a page of its
    /// RAM, whose write is to run again; false for anything else, which is
    /// no fault of the log's.
    pub fn fault(&self, table: &mut dyn Table, gpa: u64) -> bool {
        let page = gpa & !(PAGE - 1);
        let Some((word, bit)) = self.bits(page) else { return false };
        word.fetch_or(bit, Ordering::Release);
        table.unprotect(page)
    }

    /// The log begun: every page of the guest's RAM read-only to it, and
    /// the marks so far let go of -- the round after this one reads every
    /// page. False where a page could not be protected; those before it are,
    /// and `release` undoes them.
    pub fn start(&self, table: &mut dyn Table) -> bool {
        self.active.store(true, Ordering::Release);
        for r in &self.regions {
            for (_, w) in r.words() {
                w.store(0, Ordering::Relaxed);
            }
            for page in 0..r.pages {
                if !table.protect(r.base + page * PAGE) {
                    return false;
                }
            }
        }
        true
    }

    /// The pages marked since the last take, each handed to `out` -- and,
    /// with `table`, read-only to the guest again before it is handed: a
    /// round's, the TLBs the caller's to flush before it reads them. Without
    /// one, the guest paused: its last. How many pages it handed, or None
    /// where a page could not be protected: the round is no good.
    pub fn take(&self, mut table: Option<&mut dyn Table>, out: &mut dyn FnMut(u64)) -> Option<u64> {
        let mut n = 0;
        for r in &self.regions {
            for (first, w) in r.words() {
                let mut bits = w.swap(0, Ordering::AcqRel);
                while bits != 0 {
                    let b = u64::from(bits.trailing_zeros());
                    bits &= bits - 1;
                    let gpa = first + b * PAGE;
                    if let Some(t) = table.as_deref_mut() {
                        if !t.protect(gpa) {
                            /* Its mark back, and the word's others not
                             * handed yet, for whoever looks next */
                            w.fetch_or(bits | (1 << b), Ordering::Release);
                            return None;
                        }
                    }
                    out(gpa);
                    n += 1;
                }
            }
        }
        Some(n)
    }

    /// The log over: every page writable to the guest again, so that it
    /// does not fault on each one it writes for the rest of its run. The
    /// marks stay, and go on being made.
    pub fn release(&self, table: &mut dyn Table) {
        self.active.store(false, Ordering::Release);
        for r in &self.regions {
            for page in 0..r.pages {
                table.unprotect(r.base + page * PAGE);
            }
        }
    }

    /// How many pages are marked now: what a round would take.
    pub fn marked(&self) -> u64 {
        self.regions.iter().flat_map(|r| r.words()).map(|(_, w)| u64::from(w.load(Ordering::Relaxed).count_ones())).sum()
    }
}
