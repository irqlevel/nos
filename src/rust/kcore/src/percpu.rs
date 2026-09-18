//! State a CPU keeps to itself -- how a datapath gets by without a lock, or
//! an atomic, or a cache line two cores write.
//!
//! A shared atomic counter on a receive path is a line every core fights
//! over; a lock there is an instruction the profile is then about. What is
//! here costs what a plain field costs, and says in its type why that is
//! sound.

use core::cell::{Cell, UnsafeCell};
use core::sync::atomic::{AtomicUsize, Ordering};

pub use crate::const_init::ConstInit;
use crate::consts::MAX_CPUS;

/// A counter one CPU adds to and any CPU may read.
///
/// The add is a plain load, add and store -- no bus lock, the same code a
/// `usize` field gets -- and that is sound rather than merely harmless: each
/// half is an atomic access, so a reader on another CPU sees a value, never a
/// torn one. What the type does *not* promise is that two writers keep each
/// other's counts, so it belongs in a `PerCpu`: a count lost to a migration
/// between picking the slot and adding to it costs a statistic nothing worth
/// a locked instruction.
#[repr(transparent)]
pub struct LocalCounter(AtomicUsize);

impl LocalCounter {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    #[inline]
    pub fn add(&self, n: usize) {
        self.0.store(self.0.load(Ordering::Relaxed).wrapping_add(n), Ordering::Relaxed);
    }

    #[inline]
    pub fn sub(&self, n: usize) {
        self.0.store(self.0.load(Ordering::Relaxed).wrapping_sub(n), Ordering::Relaxed);
    }

    #[inline]
    pub fn set(&self, n: usize) {
        self.0.store(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl ConstInit for LocalCounter {
    const INIT: Self = LocalCounter::new();
}

/// One `T` per CPU, which any CPU may look at: counters, in practice. Give
/// `T` a `#[repr(align(64))]` and each CPU's is a cache line of its own,
/// which is the point of having one each.
pub struct PerCpu<T>([T; MAX_CPUS]);

impl<T: ConstInit> PerCpu<T> {
    pub const fn new() -> Self {
        Self([const { T::INIT }; MAX_CPUS])
    }
}

impl<T> PerCpu<T> {
    /// The running CPU's. A task that is moved between reading the id and
    /// using the slot touches another CPU's -- which `T: Sync` makes safe,
    /// and which is why what goes in one of these is counters.
    #[inline]
    pub fn here(&self) -> &T {
        &self.0[(crate::cpu::id() as usize).min(MAX_CPUS - 1)]
    }

    pub fn get(&self, cpu: usize) -> Option<&T> {
        self.0.get(cpu)
    }

    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.0.iter()
    }
}

/// One `T` per CPU that only its own CPU ever touches, and only with
/// interrupts off: no lock, because there is nobody to keep out.
///
/// What makes that a safe interface rather than a promise: `with` turns
/// interrupts off *before* it asks which CPU this is -- the other order
/// leaves a window in which the task is moved, and then two CPUs are inside
/// one slot -- and with them off nothing else runs here until it returns.
/// The one way left to reach a slot twice is from inside `work` itself, and
/// the slot's flag turns that into a `None`.
///
/// Not for an NMI handler, which interrupts-off does not keep out.
pub struct CpuLocal<T> {
    slots: [Slot<T>; MAX_CPUS],
}

/// Own cache lines: two CPUs must never share one, and the alignment has to
/// be on the type -- padding the size alone leaves the array free to start
/// mid-line.
#[repr(align(64))]
struct Slot<T> {
    busy: Cell<bool>,
    value: UnsafeCell<T>,
}

/* A slot is its CPU's alone -- `with` is the only way in -- so sharing the
 * array shares nothing; a `T` does get used on whichever CPU runs `with`. */
unsafe impl<T: Send> Sync for CpuLocal<T> {}

impl<T: ConstInit> CpuLocal<T> {
    pub const fn new() -> Self {
        Self {
            slots: [const {
                Slot { busy: Cell::new(false), value: UnsafeCell::new(T::INIT) }
            }; MAX_CPUS],
        }
    }
}

impl<T> CpuLocal<T> {
    /// This CPU's, and which CPU that is, for the length of `work`, with
    /// interrupts off. `None` on a CPU past the table, or from inside a
    /// `with` on this same one.
    #[inline]
    pub fn with<R>(&self, work: impl FnOnce(&mut T, usize) -> R) -> Option<R> {
        let flags = crate::cpu::irq_save();

        let cpu = crate::cpu::id() as usize;
        let result = match self.slots.get(cpu) {
            Some(slot) if !slot.busy.replace(true) => {
                /* Interrupts are off and this is the CPU the slot belongs
                 * to: nothing else can be in it, and the flag just taken
                 * says this call is not inside another. */
                let result = work(unsafe { &mut *slot.value.get() }, cpu);
                slot.busy.set(false);
                Some(result)
            }
            _ => None,
        };

        /* The flags `irq_save` returned a few lines up, on this CPU. */
        unsafe { crate::cpu::irq_restore(flags) };
        result
    }
}
