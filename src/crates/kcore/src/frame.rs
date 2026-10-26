//! Pages of RAM that are mapped nowhere.
//!
//! A [`Frame`] is one page the kernel has handed out by its physical address
//! and not mapped: it is in no page table of the kernel's, so no pointer the
//! host holds reaches it, and the only mapping it ever has is whatever its
//! owner puts it in -- a guest's nested page table, which is what these are
//! for. The host reads and writes one through the kernel's temporary window
//! onto physical memory, a copy at a time, and never holds a reference into
//! it: what is in it may be a guest's, changing while the host looks.
//!
//! What that buys, against pages mapped into the kernel's space the way a
//! [`DmaBuffer`](crate::dma::DmaBuffer) is: there is no mapping to run out
//! of. The kernel's virtual address space for mapped allocations is carved
//! into buckets at boot, and the largest -- 512 KiB runs -- gets an eighth of
//! it; a guest's memory in those runs would run out of addresses long before
//! the machine ran out of memory. A frame costs a page and nothing else.
//!
//! The one exception is a few pages read and written all the time -- a
//! virtqueue's rings, at every frame a guest's NIC handles -- where a copy
//! through the window maps and unmaps a page, a TLB flush, at every access:
//! frames of a [`Run`] mapped together for as long as a [`Mapped`] lives,
//! which holds a share of the run so that none of them is freed under it.
//!
//! A [`HugeFrame`] is the same thing 2 MiB at a time: a block of the huge
//! pool the kernel reserves at boot (`hugepages=`), 512 pages in a row and
//! aligned to their size, which a guest's nested table maps with one entry.
//! Its pages are copied into and out of a page at a time, as a frame's are
//! ([`FramePage`]).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU16, Ordering};

use ffi::frame;

use crate::consts::PAGE_SIZE;
use crate::pod::Pod;

/// One zeroed page of RAM, owned: freed when dropped, and mapped nowhere.
pub struct Frame {
    phys: u64,
}

impl Frame {
    /// A zeroed page off the free list, or None when there is none.
    pub fn new() -> Option<Self> {
        match frame::kernel_frame_alloc() {
            0 => None,
            phys => Some(Self { phys }),
        }
    }

    /// Its physical address: what a table that maps it is told.
    #[inline]
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// The page, to copy into and out of: borrowed from this frame, which
    /// keeps it allocated for as long as the borrow lasts.
    #[inline]
    pub fn page(&self) -> FramePage<'_> {
        FramePage { phys: self.phys, _owner: PhantomData }
    }

    /// Copy `buf.len()` bytes out of the page from `offset`
    /// ([`FramePage::read`]).
    #[inline]
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> bool {
        self.page().read(offset, buf)
    }

    /// Copy `data` into the page at `offset` ([`FramePage::write`]).
    #[inline]
    pub fn write(&self, offset: usize, data: &[u8]) -> bool {
        self.page().write(offset, data)
    }

    /// The 16-bit word at `offset`, loaded as one access with acquire order
    /// ([`FramePage::load_u16`]).
    #[inline]
    pub fn load_u16(&self, offset: usize) -> Option<u16> {
        self.page().load_u16(offset)
    }

    /// `value` stored at `offset` as one access with release order
    /// ([`FramePage::store_u16`]).
    #[inline]
    pub fn store_u16(&self, offset: usize, value: u16) -> bool {
        self.page().store_u16(offset, value)
    }
}

/// One page of RAM someone owns -- a [`Frame`], or a page of a
/// [`HugeFrame`] -- borrowed from its owner, which keeps it allocated for as
/// long as the borrow lasts: what the host copies into and out of, through
/// the kernel's temporary window, and never holds a reference into.
#[derive(Clone, Copy)]
pub struct FramePage<'a> {
    phys: u64,
    _owner: PhantomData<&'a ()>,
}

impl FramePage<'_> {
    /// Its physical address.
    #[inline]
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Copy `buf.len()` bytes out of the page from `offset`. False, and
    /// nothing read, when that is not inside the page.
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> bool {
        if !inside(offset, buf.len()) {
            return false;
        }
        /* The page is the owner's, borrowed for `self`'s life, and `buf` is
         * writable for its length. */
        unsafe { frame::kernel_frame_read(self.phys, offset, buf.as_mut_ptr(), buf.len()) == 0 }
    }

    /// Copy `data` into the page at `offset`. False, and nothing written,
    /// when that is not inside the page.
    ///
    /// Through `&self`, as `read` is: the copy goes through the calling
    /// CPU's own slot of the window with its interrupts off, so two CPUs
    /// copying into one page at once share nothing of the kernel's -- and
    /// no reference into the page exists for them to alias. What the page
    /// holds is not the compiler's to reason about: the guest whose memory
    /// it is writes it from its own CPUs whenever it runs, several at once
    /// for a guest of several CPUs, and a host copy racing them gets what
    /// the race gives, byte by byte.
    pub fn write(&self, offset: usize, data: &[u8]) -> bool {
        if !inside(offset, data.len()) {
            return false;
        }
        /* The page is the owner's, borrowed for `self`'s life, and `data` is
         * readable for its length. */
        unsafe { frame::kernel_frame_write(self.phys, offset, data.as_ptr(), data.len()) == 0 }
    }

    /// The 16-bit word at `offset`, loaded as one access -- whole, where a
    /// copy of its two bytes could take one from before another CPU's store
    /// of it and one from after -- with acquire order: what was written
    /// before the word is there to read after it. What a virtqueue's indices
    /// are read with. None when `offset` is not an aligned word's inside the
    /// page.
    pub fn load_u16(&self, offset: usize) -> Option<u16> {
        if offset % 2 != 0 || !inside(offset, 2) {
            return None;
        }
        let mut value = 0u16;
        /* The page is the owner's, borrowed for `self`'s life, and `value` is
         * a word to write. */
        (unsafe { frame::kernel_frame_load16(self.phys, offset, &mut value) } == 0).then_some(value)
    }

    /// `value` stored at `offset` as one access, with release order: what
    /// was written before it is there before it. False, and nothing stored,
    /// when `offset` is not an aligned word's inside the page.
    pub fn store_u16(&self, offset: usize, value: u16) -> bool {
        if offset % 2 != 0 || !inside(offset, 2) {
            return false;
        }
        /* The page is the owner's, borrowed for `self`'s life. */
        unsafe { frame::kernel_frame_store16(self.phys, offset, value) == 0 }
    }
}

fn inside(offset: usize, len: usize) -> bool {
    offset <= PAGE_SIZE && len <= PAGE_SIZE - offset
}

impl Drop for Frame {
    fn drop(&mut self) {
        /* The address `new` was given, freed once: a `Frame` is neither
         * `Clone` nor `Copy`, and this is the only place it is freed. That no
         * table still maps it by then -- no guest can reach it through a
         * nested page table once it is back on the free list -- is the
         * owner's to keep, as it is for any address handed to hardware
         * (`phys`); `hv::GuestMemory` keeps it by owning the table too. */
        unsafe { frame::kernel_frame_free(self.phys) }
    }
}

/// The pages of a [`HugeFrame`]: 2 MiB of them.
pub const HUGE_PAGES: usize = 512;
/// A huge frame's bytes, and the alignment of its physical address.
pub const HUGE_SIZE: u64 = (HUGE_PAGES * PAGE_SIZE) as u64;

/// A block of the huge pool, owned: 2 MiB of RAM -- [`HUGE_PAGES`] pages in a
/// row, aligned to their size -- handed out zeroed and mapped nowhere, back
/// into the pool when dropped. What a guest's nested table maps with one
/// entry where a [`Frame`] takes one each.
pub struct HugeFrame {
    phys: u64,
}

impl HugeFrame {
    /// A zeroed block of the pool, or None when it has none free -- or there
    /// is no pool, `hugepages=` not given.
    pub fn new() -> Option<Self> {
        match frame::kernel_frame_alloc_huge() {
            0 => None,
            phys => Some(Self { phys }),
        }
    }

    /// Its physical address: what a table that maps it is told.
    #[inline]
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Its page `i`, to copy into and out of: None past its last.
    #[inline]
    pub fn page(&self, i: usize) -> Option<FramePage<'_>> {
        (i < HUGE_PAGES).then(|| FramePage { phys: self.phys + (i * PAGE_SIZE) as u64, _owner: PhantomData })
    }
}

impl Drop for HugeFrame {
    fn drop(&mut self) {
        /* The address `new` was given, back once: a `HugeFrame` is neither
         * `Clone` nor `Copy`, and this is the only place it goes back. That
         * no table still maps it is the owner's to keep, as for a `Frame`. */
        unsafe { frame::kernel_frame_free_huge(self.phys) }
    }
}

/// The huge pool as it stands: its blocks, 0 with no `hugepages=`, and how
/// many of them are free.
#[derive(Clone, Copy, Debug)]
pub struct HugePool {
    pub blocks: u64,
    pub free: u64,
}

/// The huge pool, this instant.
pub fn huge_pool() -> HugePool {
    HugePool { blocks: frame::kernel_frame_huge_blocks(), free: frame::kernel_frame_huge_free() }
}

/// A run's pages [`HUGE_PAGES`] at a time: one block of the pool, or that
/// many frames -- fewer in a run's last piece -- each a page of its own, in a
/// table of a page's worth so that no one allocation of it outgrows a page
/// however many frames there are.
enum Piece {
    Huge(HugeFrame),
    Pages(Vec<Frame>),
}

const _: () = assert!(HUGE_PAGES * core::mem::size_of::<Frame>() == PAGE_SIZE);

/// Pages a table maps with one entry: the first one's address and how many
/// are in a row from it -- 1 for a frame, [`HUGE_PAGES`] for a block of the
/// pool, its address aligned to [`HUGE_SIZE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub phys: u64,
    pub pages: usize,
}

/// The most frames one [`Mapped`] maps: a queue of 256 entries' rings are
/// three pages.
pub const MAPPED_MAX: usize = 4;

/// Pages owned together, and shared: a region of a guest's memory, which
/// a mapping of some of them holds a share of ([`Mapped`]), so that they go
/// back to the free list -- or to the pool -- only once every mapping of
/// them is undone, whichever of the run's holders goes last.
#[derive(Clone)]
pub struct Run {
    pieces: Arc<Vec<Piece>>,
}

impl Run {
    /// `count` zeroed frames, or None when they cannot all be had -- those
    /// taken by then going back as the attempt is dropped.
    pub fn new(count: usize) -> Option<Run> {
        let mut pieces: Vec<Piece> = Vec::new();
        pieces.try_reserve_exact(count.div_ceil(HUGE_PAGES)).ok()?;
        let mut left = count;
        while left > 0 {
            let n = left.min(HUGE_PAGES);
            let mut piece = Vec::new();
            piece.try_reserve_exact(n).ok()?;
            for _ in 0..n {
                piece.push(Frame::new()?);
            }
            pieces.push(Piece::Pages(piece));
            left -= n;
        }
        Some(Run { pieces: Arc::new(pieces) })
    }

    /// `count` zeroed pages, every one of them the huge pool's: whole
    /// blocks, so a multiple of [`HUGE_PAGES`], or None -- as it is when the
    /// pool cannot give them all, the blocks taken by then going back.
    pub fn new_huge(count: usize) -> Option<Run> {
        if count % HUGE_PAGES != 0 {
            return None;
        }
        let blocks = count / HUGE_PAGES;
        let mut pieces: Vec<Piece> = Vec::new();
        pieces.try_reserve_exact(blocks).ok()?;
        for _ in 0..blocks {
            pieces.push(Piece::Huge(HugeFrame::new()?));
        }
        Some(Run { pieces: Arc::new(pieces) })
    }

    /// Its page `i`, to copy into and out of: None past its end.
    #[inline]
    pub fn page(&self, i: usize) -> Option<FramePage<'_>> {
        match self.pieces.get(i / HUGE_PAGES)? {
            Piece::Huge(block) => block.page(i % HUGE_PAGES),
            Piece::Pages(frames) => frames.get(i % HUGE_PAGES).map(Frame::page),
        }
    }

    /// Its pages in order, as a table maps them: a block of the pool one
    /// extent of [`HUGE_PAGES`], a frame one of its own.
    pub fn extents(&self) -> impl Iterator<Item = Extent> + '_ {
        self.pieces.iter().flat_map(|piece| {
            let (block, frames): (Option<Extent>, &[Frame]) = match piece {
                Piece::Huge(b) => (Some(Extent { phys: b.phys(), pages: HUGE_PAGES }), &[]),
                Piece::Pages(f) => (None, f.as_slice()),
            };
            block.into_iter().chain(frames.iter().map(|f| Extent { phys: f.phys(), pages: 1 }))
        })
    }

    /// Its pages `first..first + count`, `MAPPED_MAX` at most, mapped into
    /// the kernel's space together in that order: None when they are not
    /// all its own, or there is no address left for them.
    pub fn map(&self, first: usize, count: usize) -> Option<Mapped> {
        if count == 0 || count > MAPPED_MAX {
            return None;
        }
        let mut phys = [0u64; MAPPED_MAX];
        for (k, p) in phys.iter_mut().take(count).enumerate() {
            *p = self.page(first.checked_add(k)?)?.phys();
        }
        /* Pages of this run, `count` of them in `phys`: the mapping made
         * holds a share of the run, which keeps them allocated until after
         * its `drop` has undone it. */
        let va = unsafe { frame::kernel_frames_map(phys.as_ptr(), count) };
        if va == 0 {
            return None;
        }
        Some(Mapped { va, pages: count, _run: self.clone() })
    }
}

/// Frames of a [`Run`] mapped into the kernel's space together, from
/// [`Run::map`] until this is dropped -- the guest pages a device reads and
/// writes at every request, without a page mapped and unmapped at every
/// access as [`Frame::read`] has.
///
/// It hands out no reference into them, as a `Frame` does not: what is
/// there is a guest's, its CPUs writing it whenever they run, so every
/// access is one volatile load or store of a plain value at a checked,
/// aligned offset -- what is read is whatever was there that instant, as
/// with memory a device writes ([`DmaBuffer::load`](crate::dma::DmaBuffer::load)).
/// An index another CPU publishes is read and written as one atomic access
/// with acquire or release order (`load_u16`, `store_u16`). Through `&self`:
/// the host side's own accesses are ordered by whoever holds the device.
pub struct Mapped {
    va: usize,
    pages: usize,
    /// The run the frames are: a share of it, dropped after `drop` has
    /// undone the mapping, so no frame is freed while it is mapped here.
    _run: Run,
}

impl Mapped {
    /// Whether a `T` lies at `offset`: inside, and aligned for one.
    #[inline]
    fn holds<T>(&self, offset: usize) -> bool {
        let bytes = self.pages * PAGE_SIZE;
        let size = core::mem::size_of::<T>();
        offset % core::mem::align_of::<T>() == 0 && offset <= bytes && size <= bytes - offset
    }

    /// The `T` at `offset`, as it is this instant: a volatile load. None
    /// when no `T` lies there.
    #[inline]
    pub fn load<T: Pod>(&self, offset: usize) -> Option<T> {
        if !self.holds::<T>(offset) {
            return None;
        }
        /* Inside the mapping, which is there for as long as `self`, and
         * aligned: checked above. Any bytes are a `T`. */
        Some(unsafe { ((self.va + offset) as *const T).read_volatile() })
    }

    /// `value` put at `offset` with a volatile store. False, and nothing
    /// stored, when no `T` lies there.
    #[inline]
    pub fn store<T: Pod>(&self, offset: usize, value: T) -> bool {
        if !self.holds::<T>(offset) {
            return false;
        }
        /* Inside the mapping and aligned, as `load` has it. */
        unsafe { ((self.va + offset) as *mut T).write_volatile(value) };
        true
    }

    /// The 16-bit word at `offset`, loaded as one access with acquire order:
    /// what was written before it is there to read after it. What a
    /// virtqueue's indices are read with. None when no aligned word lies
    /// there.
    #[inline]
    pub fn load_u16(&self, offset: usize) -> Option<u16> {
        if !self.holds::<u16>(offset) {
            return None;
        }
        /* An aligned word inside the mapping, as `load` has it, taken as an
         * atomic for the one access and its order. */
        Some(unsafe { AtomicU16::from_ptr((self.va + offset) as *mut u16) }.load(Ordering::Acquire))
    }

    /// `value` stored at `offset` as one access with release order: what
    /// was written before it is there before it. False, and nothing stored,
    /// when no aligned word lies there.
    #[inline]
    pub fn store_u16(&self, offset: usize, value: u16) -> bool {
        if !self.holds::<u16>(offset) {
            return false;
        }
        /* An aligned word inside the mapping, as `load_u16` has it. */
        unsafe { AtomicU16::from_ptr((self.va + offset) as *mut u16) }.store(value, Ordering::Release);
        true
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        /* The address the map gave and the count it was given, undone once:
         * a `Mapped` is neither `Clone` nor `Copy`, and this is the only
         * place it is unmapped. Its share of the run goes after this. */
        unsafe { frame::kernel_frames_unmap(self.va, self.pages) }
    }
}
