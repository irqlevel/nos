use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};
use ffi::sync;

/* Every lock here owns what it guards. `lock()` hands back a guard that
 * derefs to the data and releases on drop, so neither the access nor the
 * unlock is `unsafe` at a call site -- the one place that needs it is the
 * guard's own `Deref`, below, once per lock type.
 *
 * Guards are values: two may be held at once and dropped in either order
 * (`drop(a)` before `b` goes out of scope), and a function that takes a lock
 * on its caller's behalf returns the guard. A lock that guards nothing --
 * one that only serialises a stretch of code -- is a lock over `()`. */

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

/// The kernel's mutex: a taker that finds it held sleeps. Task context only.
pub struct Mutex<T> {
    handle: usize,
    data: UnsafeCell<T>,
}

/* The mutex is what makes handing `&mut T` to one thread at a time sound. */
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// None when the kernel has no memory for one.
    pub fn new(data: T) -> Option<Self> {
        let h = sync::kernel_mutex_create();
        if h == 0 {
            None
        } else {
            Some(Self { handle: h, data: UnsafeCell::new(data) })
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        unsafe { sync::kernel_mutex_lock(self.handle) };
        MutexGuard { mutex: self, _not_send: PhantomData }
    }
}

impl<T> Drop for Mutex<T> {
    fn drop(&mut self) {
        unsafe { sync::kernel_mutex_destroy(self.handle) };
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { sync::kernel_mutex_unlock(self.mutex.handle) };
    }
}

/// The kernel's spin lock: interrupts and preemption off while it is held.
/// The one to use unless it cannot be had -- see [`IrqSpinLock`].
pub struct SpinLock<T> {
    handle: usize,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// None when the kernel has no memory for one.
    pub fn new(data: T) -> Option<Self> {
        let h = sync::kernel_spinlock_create();
        if h == 0 {
            None
        } else {
            Some(Self { handle: h, data: UnsafeCell::new(data) })
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let flags = unsafe { sync::kernel_spinlock_lock(self.handle) };
        SpinLockGuard { lock: self, flags, _not_send: PhantomData }
    }
}

impl<T> Drop for SpinLock<T> {
    fn drop(&mut self) {
        unsafe { sync::kernel_spinlock_destroy(self.handle) };
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    flags: u64,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { sync::kernel_spinlock_unlock(self.lock.handle, self.flags) };
    }
}

pub struct WaitGroup {
    handle: usize,
}

impl WaitGroup {
    pub fn new() -> Option<Self> {
        let h = sync::kernel_waitgroup_create();
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    pub fn add(&self, delta: isize) {
        unsafe { sync::kernel_waitgroup_add(self.handle, delta) }
    }

    pub fn done(&self) {
        unsafe { sync::kernel_waitgroup_done(self.handle) }
    }

    pub fn wait(&self) {
        unsafe { sync::kernel_waitgroup_wait(self.handle) }
    }
}

impl Drop for WaitGroup {
    fn drop(&mut self) {
        unsafe { sync::kernel_waitgroup_destroy(self.handle) }
    }
}

/// One task waiting for others to have something for it (kernel/event.h):
/// `signal` from anywhere -- a hard IRQ handler included -- and the one task
/// that calls `wait` comes back once for every run of signals since it last
/// did. The waiter is out of the scheduler's walk until then rather than
/// spinning, and a signal that finds it blocked sends its CPU an IPI. Pin a
/// waiter whose wakeups have to be prompt: one moved to another CPU as it
/// blocks is woken at that CPU's next scheduling point instead.
pub struct Event {
    handle: usize,
}

/* The kernel's event is built to be signalled from every CPU */
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub fn new() -> Option<Self> {
        let h = sync::kernel_event_create();
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    /// Task context, one waiter at a time.
    pub fn wait(&self) {
        unsafe { sync::kernel_event_wait(self.handle) }
    }

    /// `wait`, for at most `timeout`: true when signalled, false when the
    /// time ran out first. Blocked either way -- the scheduler wakes the
    /// task at its deadline -- so a waiter costs its CPU nothing.
    pub fn wait_for(&self, timeout: crate::time::Duration) -> bool {
        unsafe { sync::kernel_event_wait_for(self.handle, timeout.as_nanos()) != 0 }
    }

    #[inline]
    pub fn signal(&self) {
        unsafe { sync::kernel_event_signal(self.handle) }
    }

    /// The raw handle, for an owner that keeps the event for the life of the
    /// kernel and signals it from a `static` -- which is what `wait` and
    /// `signal` need, and what `Drop` must therefore never get.
    pub fn handle(&self) -> usize {
        self.handle
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { sync::kernel_event_destroy(self.handle) }
    }
}

/// A spin lock made of an atomic flag, with interrupts and preemption off
/// while it is held.
///
/// [`SpinLock`] is the one to use -- it is the kernel's own, and it can be
/// held across the things kernel locks may be held across. This one exists
/// for the code that cannot have it: netconsole arms its capture ring from
/// the kernel command line, long before the page allocator can make anything,
/// and a lock that allocates is a lock it cannot have. Being `const`, it can
/// also live in a `static` rather than behind a pointer.
///
/// Nothing that sleeps may run while it is held, and nothing that takes
/// longer than a few hundred instructions: interrupts are off on this CPU.
pub struct IrqSpinLock<T> {
    held: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for IrqSpinLock<T> {}
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self { held: AtomicBool::new(false), data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> IrqSpinGuard<'_, T> {
        /* Interrupts off first: a CPU that takes an interrupt while holding
         * this, and whose handler takes it again, deadlocks against itself. */
        let flags = ffi::cpu::kernel_irq_save();
        while self.held.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        IrqSpinGuard { lock: self, flags }
    }

    /// One attempt, no spin: None when someone else has it. For the panic
    /// path, whose lock may be held by a CPU that is never going to release
    /// it.
    pub fn try_lock(&self) -> Option<IrqSpinGuard<'_, T>> {
        let flags = ffi::cpu::kernel_irq_save();
        if self.held.swap(true, Ordering::Acquire) {
            unsafe { ffi::cpu::kernel_irq_restore(flags) };
            None
        } else {
            Some(IrqSpinGuard { lock: self, flags })
        }
    }

    /// The data, around the lock. For the panic path once `try_lock` has
    /// failed: every other CPU has been sent the halting IPI, and a lock one
    /// of them died holding must not keep the report from going out.
    ///
    /// # Safety
    /// Nothing else is running that could touch the data: the rest of the
    /// machine is stopped.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn steal(&self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

pub struct IrqSpinGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    flags: usize,
}

impl<T> Deref for IrqSpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqSpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqSpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        unsafe { ffi::cpu::kernel_irq_restore(self.flags) };
    }
}

/// A spin lock made of an atomic flag that holds preemption off while it is
/// taken -- the kernel's own `RawSpinLock`, in the shape Rust can put in a
/// `static`.
///
/// Interrupts stay **on**. That is what makes it the wrong lock for anything
/// a hard interrupt handler touches (use [`IrqSpinLock`] there) and the right
/// one for a pool with a lock per entry, where a holder may go on to do work
/// that must not run with interrupts off. The holder cannot be switched away,
/// so no other taker spins out a whole time slice.
///
/// Nothing that sleeps may run while it is held.
pub struct PreemptSpinLock<T> {
    held: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for PreemptSpinLock<T> {}
unsafe impl<T: Send> Sync for PreemptSpinLock<T> {}

impl<T> PreemptSpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self { held: AtomicBool::new(false), data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> PreemptSpinGuard<'_, T> {
        ffi::cpu::kernel_preempt_disable();
        while self.held.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        PreemptSpinGuard { lock: self }
    }
}

pub struct PreemptSpinGuard<'a, T> {
    lock: &'a PreemptSpinLock<T>,
}

impl<T> Deref for PreemptSpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for PreemptSpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for PreemptSpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        unsafe { ffi::cpu::kernel_preempt_enable() };
    }
}

/// A lock nobody ever waits for: `try_lock` takes it or says it is taken,
/// and that is all. For work that one caller at a time must do and that
/// anyone may start -- a log's writer, say, where losing the race costs
/// nothing because the winner does the loser's work too. The holder may
/// sleep: nothing spins on this, and neither interrupts nor preemption are
/// touched.
pub struct TryLock<T> {
    held: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for TryLock<T> {}
unsafe impl<T: Send> Sync for TryLock<T> {}

impl<T> TryLock<T> {
    pub const fn new(data: T) -> Self {
        Self { held: AtomicBool::new(false), data: UnsafeCell::new(data) }
    }

    pub fn try_lock(&self) -> Option<TryLockGuard<'_, T>> {
        if self.held.swap(true, Ordering::Acquire) {
            None
        } else {
            Some(TryLockGuard { lock: self })
        }
    }

    /// The data, around the lock: the panic path's, as `IrqSpinLock::steal`.
    ///
    /// # Safety
    /// Nothing else is running that could touch the data: the rest of the
    /// machine is stopped.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn steal(&self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

pub struct TryLockGuard<'a, T> {
    lock: &'a TryLock<T>,
}

impl<T> Deref for TryLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for TryLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for TryLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}
