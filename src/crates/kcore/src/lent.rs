//! The buffers a queue of the kernel's lends a module between the module's
//! requests -- an image's (`fs::ImageQueue`), a device's
//! (`block::DiskQueue`): one a request, filled with a write's data before it
//! is started and a read's taken out once it is back.
//!
//! Each buffer is nobody's, lent, or the kernel's, and goes from one to
//! another by a compare-and-swap of its state: so its bytes are reached
//! through one guard at a time (`Lent`), never while its request is the
//! kernel's -- whose device may be writing them -- and a request is started
//! only on a buffer nobody has.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::error::{Error, Result};

/// Nobody's: to be lent, or started.
const FREE: u8 = 0;
/// Lent (`Buffers::lend`): its bytes one guard's.
const LENT: u8 = 1;
/// The kernel's: its request started and not given back yet.
const STARTED: u8 = 2;

/// What a queue's `start` did with a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Started {
    /// With the device: `reap` gives it back.
    InFlight,
    /// Done already.
    Done,
}

/// A buffer: where it is, and whose it is now.
struct Buffer {
    at: *mut u8,
    state: AtomicU8,
}

pub(crate) struct Buffers {
    list: Vec<Buffer>,
    len: usize,
}

/* The buffers are the kernel's, kept for as long as the queue holding these
 * is open -- which outlives them -- and each one's bytes are reached only by
 * the one guard its state lets have them. */
unsafe impl Send for Buffers {}
unsafe impl Sync for Buffers {}

impl Buffers {
    /// The buffers at `at`, `len` bytes each. NoMemory when there is none
    /// for the list.
    ///
    /// # Safety
    /// Each of `at` is `len` bytes of the kernel's that stay its -- and are
    /// reached through these alone -- for as long as the `Buffers` lives.
    pub(crate) unsafe fn new(at: Vec<*mut u8>, len: usize) -> Result<Buffers> {
        let mut list = Vec::new();
        list.try_reserve_exact(at.len()).map_err(|_| Error::NoMemory)?;
        list.extend(at.into_iter().map(|at| Buffer { at, state: AtomicU8::new(FREE) }));
        Ok(Buffers { list, len })
    }

    pub(crate) fn count(&self) -> usize {
        self.list.len()
    }

    /// A buffer's bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Buffer `k`'s bytes, while nobody else has them: None while they are
    /// lent already, or the kernel's.
    pub(crate) fn lend(&self, k: usize) -> Option<Lent<'_>> {
        let b = self.list.get(k)?;
        b.state.compare_exchange(FREE, LENT, Ordering::Acquire, Ordering::Relaxed).ok()?;
        Some(Lent { buffers: self, k })
    }

    /// Buffer `k` taken for a request it is to be started on: InvalidValue
    /// past the list, Busy while it is lent or started already.
    pub(crate) fn take(&self, k: usize) -> Result<()> {
        let b = self.list.get(k).ok_or(Error::InvalidValue)?;
        b.state.compare_exchange(FREE, STARTED, Ordering::AcqRel, Ordering::Relaxed).map(|_| ()).map_err(|_| Error::Busy)
    }

    /// Buffer `k`'s request done as it was started -- or not started after
    /// all: nobody's again.
    pub(crate) fn untake(&self, k: usize) {
        if let Some(b) = self.list.get(k) {
            b.state.store(FREE, Ordering::Release);
        }
    }

    /// Buffer `k`'s request given back by the kernel: nobody's again --
    /// false, and nothing changed, for one that was not started.
    pub(crate) fn give_back(&self, k: usize) -> bool {
        self.list.get(k).is_some_and(|b| b.state.compare_exchange(STARTED, FREE, Ordering::AcqRel, Ordering::Relaxed).is_ok())
    }
}

/// A buffer of a queue's, lent (`Buffers::lend`): its bytes, until this goes.
pub struct Lent<'a> {
    buffers: &'a Buffers,
    k: usize,
}

impl core::ops::Deref for Lent<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        /* Lent: the state says these bytes are this guard's alone, and the
         * kernel keeps them for as long as the queue is open. */
        unsafe { core::slice::from_raw_parts(self.buffers.list[self.k].at, self.buffers.len) }
    }
}

impl core::ops::DerefMut for Lent<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        /* As `deref`, and `&mut self` is this guard's one way in. */
        unsafe { core::slice::from_raw_parts_mut(self.buffers.list[self.k].at, self.buffers.len) }
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        self.buffers.list[self.k].state.store(FREE, Ordering::Release);
    }
}
