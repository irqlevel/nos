//! Tracepoints from Rust (docs/tracing.md): the kernel's events recorded by
//! Rust's sites -- the image's and a module's alike -- into the recording
//! CPU's ring, by the same `emit()` the C++ sites call.
//!
//! An event's description is the kernel's: its name, group, help, fields and
//! how each prints, in kernel/tracepoint.cpp's table, by which `trace` prints
//! every record. What this side has of one is its number and its payload, a
//! struct of the description's layout declared with the event it records
//! (`events.rs`): so that no payload can be emitted as another event's, and
//! so that the boot's self-test (`check`) can hold every field's name,
//! offset and size to the kernel's description. The two are written apart,
//! and a field one side has and the other has not would be a record printed
//! wrong rather than a build that fails -- unless something compares them.
//!
//! A site:
//!
//! ```ignore
//! if trace::on::<HvExit>() {
//!     trace::emit(&HvExit { vcpu, ..Default::default() });
//! }
//! ```
//!
//! Off, that is two loads -- the flags' address, kept here once asked, and
//! the event's byte -- and a branch not taken: the payload is built only
//! inside. On, a call to the kernel's `emit()`, which turns interrupts off
//! for the write and takes no lock.

mod events;
pub use events::*;

use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

use crate::pod::bytes_of;

/// The kernel's flags, once asked: null until the first `on`. Each copy of
/// kcore -- the image's, a module's -- asks once for its own.
static FLAGS: AtomicPtr<AtomicU8> = AtomicPtr::new(core::ptr::null_mut());

/// Every event off: where the kernel describes fewer events than this side
/// has -- never, the two being built together, but then a read of the
/// kernel's flags would be one past their end.
static NONE: [AtomicU8; ID_LIMIT] = [const { AtomicU8::new(0) }; ID_LIMIT];

#[cold]
#[inline(never)]
fn flags_asked() -> *mut AtomicU8 {
    let flags = if ffi::trace::kernel_tp_count() >= ID_LIMIT {
        ffi::trace::kernel_tp_flags() as *mut AtomicU8
    } else {
        NONE.as_ptr() as *mut AtomicU8
    };
    /* Two CPUs asking at once store the same address */
    FLAGS.store(flags, Ordering::Relaxed);
    flags
}

#[inline(always)]
fn flags() -> &'static [AtomicU8; ID_LIMIT] {
    let mut flags = FLAGS.load(Ordering::Relaxed);
    if flags.is_null() {
        flags = flags_asked();
    }
    /* SAFETY: `flags` is the kernel's tp::enabled -- a static array of
       kernel_tp_count() bytes, ID_LIMIT at least (flags_asked), there for
       the kernel's life and only ever read and written by C++ with atomics
       -- or NONE. An AtomicU8 has a u8's size and alignment. */
    unsafe { &*(flags as *const [AtomicU8; ID_LIMIT]) }
}

/// Whether event `E` is on: what a site asks before it builds the payload.
/// A stale answer costs a record kept or lost as the flag moves.
#[inline(always)]
pub fn on<E: Event>() -> bool {
    flags()[E::ID as usize].load(Ordering::Relaxed) != 0
}

/// `event`'s record into this CPU's ring -- none when it has no ring, or
/// recording is frozen. Any context: interrupts are off for the write, and
/// nothing is locked or allocated.
#[inline]
pub fn emit<E: Event>(event: &E) {
    let bytes = bytes_of(event);
    /* SAFETY: `bytes` is every byte of the payload, initialised (Pod), and
       its length the payload's size: the event's description's, which
       `check` holds it to and the kernel's emit() BUG_ONs on. */
    unsafe { ffi::trace::kernel_tp_emit(E::ID as u16, bytes.as_ptr(), bytes.len()) }
}

/// The pid a record says the running task by: its own -- an interrupt's on
/// a task's stack the task's -- or the kernel's none, `u32::MAX`, off one.
/// What a site that records on its task's behalf puts in its payload.
#[inline]
pub fn pid() -> u32 {
    ffi::trace::kernel_tp_pid()
}

/// What the kernel's description of an event says that this side's payload
/// does not.
#[derive(Debug)]
pub enum Mismatch {
    /// The kernel has no event of the number
    NoEvent,
    /// Another name at the number
    Name,
    /// Another payload size
    Size { ours: usize, kernels: usize },
    /// A field of the kernel's this side has not -- by its name, or not at
    /// its offset, or not of its size
    Field { index: usize },
    /// That field's values named otherwise: an enumeration on one side and
    /// not the other, of another count of values, or one of them called
    /// another name
    Names { index: usize },
    /// A field of this side's the kernel's description has not
    Missing,
}

/// A name the kernel wrote into `buf`, up to its terminator.
fn written(buf: &[u8]) -> &[u8] {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..end]
}

/// Every payload declared here held to the kernel's description of its
/// event: its name, its size, each field's name, offset and size, both
/// ways, and an enumeration's names value by value. The first that differs,
/// by the payload's name.
pub fn check() -> Result<(), (&'static str, Mismatch)> {
    /* More fields than any payload of 40 bytes can have: the bound on a walk
       of the kernel's that never says it is done */
    const FIELDS_MAX: usize = 64;
    const NAME_MAX: usize = 32;

    for e in EVENTS {
        let id = e.id as u16;
        let mut name = [0u8; NAME_MAX];
        let mut size = 0usize;
        /* SAFETY: the name's buffer is NAME_MAX bytes, as its length says,
           and `size` a usize this side owns */
        if unsafe { ffi::trace::kernel_tp_event(id, name.as_mut_ptr(), name.len(), &mut size) } == 0 {
            return Err((e.name, Mismatch::NoEvent));
        }
        if written(&name) != e.name.as_bytes() {
            return Err((e.name, Mismatch::Name));
        }
        if size != e.size {
            return Err((e.name, Mismatch::Size { ours: e.size, kernels: size }));
        }

        let mut theirs = 0;
        for index in 0..FIELDS_MAX {
            let mut field = [0u8; NAME_MAX];
            let mut offset = 0usize;
            let mut fsize = 0usize;
            let mut names = 0usize;
            /* SAFETY: as above, the four of them this side's */
            let there = unsafe {
                ffi::trace::kernel_tp_field(id, index, field.as_mut_ptr(), field.len(), &mut offset, &mut fsize,
                    &mut names)
            };
            if there == 0 {
                break;
            }
            theirs += 1;
            let named = written(&field);
            let ours = match e.fields.iter().find(|f| f.name.as_bytes() == named) {
                Some(f) if f.offset == offset && f.size == fsize => f,
                _ => return Err((e.name, Mismatch::Field { index })),
            };
            if ours.names.len() != names {
                return Err((e.name, Mismatch::Names { index }));
            }
            for (value, name) in ours.names.iter().enumerate() {
                let mut kernels = [0u8; NAME_MAX];
                /* SAFETY: the buffer is NAME_MAX bytes, as its length says */
                let known = unsafe {
                    ffi::trace::kernel_tp_field_name(id, index, value, kernels.as_mut_ptr(), kernels.len())
                };
                if known == 0 || written(&kernels) != name.as_bytes() {
                    return Err((e.name, Mismatch::Names { index }));
                }
            }
        }
        let ours = e.fields.iter().filter(|f| !f.name.starts_with('_')).count();
        if ours != theirs {
            return Err((e.name, Mismatch::Missing));
        }
    }
    Ok(())
}
