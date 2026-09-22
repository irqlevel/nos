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

use ffi::frame;

use crate::consts::PAGE_SIZE;

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

    /// Copy `buf.len()` bytes out of the page from `offset`. False, and
    /// nothing read, when that is not inside the page.
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> bool {
        if !inside(offset, buf.len()) {
            return false;
        }
        /* The page is this value's, and `buf` is writable for its length. */
        unsafe { frame::kernel_frame_read(self.phys, offset, buf.as_mut_ptr(), buf.len()) == 0 }
    }

    /// Copy `data` into the page at `offset`. False, and nothing written,
    /// when that is not inside the page.
    pub fn write(&mut self, offset: usize, data: &[u8]) -> bool {
        if !inside(offset, data.len()) {
            return false;
        }
        /* The page is this value's, and `data` is readable for its length. */
        unsafe { frame::kernel_frame_write(self.phys, offset, data.as_ptr(), data.len()) == 0 }
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
