use core::marker::PhantomData;
use ffi::sync;

pub struct Mutex {
    handle: usize,
}

impl Mutex {
    pub fn new() -> Option<Self> {
        let h = unsafe { sync::kernel_mutex_create() };
        if h == 0 {
            None
        } else {
            Some(Self { handle: h })
        }
    }

    pub fn lock(&self) -> MutexGuard<'_> {
        unsafe {
            sync::kernel_mutex_lock(self.handle);
        }
        MutexGuard { mutex: self, _not_send: PhantomData }
    }
}

impl Drop for Mutex {
    fn drop(&mut self) {
        unsafe {
            sync::kernel_mutex_destroy(self.handle);
        }
    }
}

pub struct MutexGuard<'a> {
    mutex: &'a Mutex,
    _not_send: PhantomData<*const ()>,
}

impl<'a> Drop for MutexGuard<'a> {
    fn drop(&mut self) {
        unsafe {
            sync::kernel_mutex_unlock(self.mutex.handle);
        }
    }
}

pub struct SpinLock {
    handle: usize,
}

impl SpinLock {
    pub fn new() -> Option<Self> {
        let h = unsafe { sync::kernel_spinlock_create() };
        if h == 0 {
            None
        } else {
            Some(Self { handle: h })
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_> {
        let flags = unsafe { sync::kernel_spinlock_lock(self.handle) };
        SpinLockGuard {
            lock: self,
            flags,
            _not_send: PhantomData,
        }
    }
}

impl Drop for SpinLock {
    fn drop(&mut self) {
        unsafe {
            sync::kernel_spinlock_destroy(self.handle);
        }
    }
}

pub struct SpinLockGuard<'a> {
    lock: &'a SpinLock,
    flags: u64,
    _not_send: PhantomData<*const ()>,
}

impl<'a> Drop for SpinLockGuard<'a> {
    fn drop(&mut self) {
        unsafe {
            sync::kernel_spinlock_unlock(self.lock.handle, self.flags);
        }
    }
}

pub struct WaitGroup {
    handle: usize,
}

impl WaitGroup {
    pub fn new() -> Option<Self> {
        let h = unsafe { sync::kernel_waitgroup_create() };
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

    pub fn raw_handle(&self) -> usize {
        self.handle
    }
}

impl Drop for WaitGroup {
    fn drop(&mut self) {
        unsafe { sync::kernel_waitgroup_destroy(self.handle) }
    }
}

/// Read-write spinlock with writer priority.
///
/// Readers acquire with `read()`, which returns a `RwReadGuard` that releases
/// the read lock on drop.  Writers acquire with `write()`, which disables
/// interrupts and returns a `RwWriteGuard` that re-enables them on drop.
///
/// Use read locks in task context for shared lookups; use write locks for
/// mutations.  ISR context may hold a read lock only if it does not sleep.
/// Either side keeps preemption off while it is held, as every kernel
/// spinlock does.
pub struct RwSpinLock {
    handle: usize,
}

impl RwSpinLock {
    pub fn new() -> Option<Self> {
        let h = unsafe { sync::kernel_rw_spinlock_create() };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    pub fn read(&self) -> RwReadGuard<'_> {
        let token = unsafe { sync::kernel_rw_spinlock_read_lock(self.handle) };
        RwReadGuard { lock: self, token, _not_send: PhantomData }
    }

    pub fn write(&self) -> RwWriteGuard<'_> {
        let flags = unsafe { sync::kernel_rw_spinlock_write_lock(self.handle) };
        RwWriteGuard { lock: self, flags, _not_send: PhantomData }
    }
}

impl Drop for RwSpinLock {
    fn drop(&mut self) {
        unsafe { sync::kernel_rw_spinlock_destroy(self.handle) }
    }
}

pub struct RwReadGuard<'a> {
    lock: &'a RwSpinLock,
    /* Whose preemption the read lock disabled, handed back on release: a
       read lock has many holders at once, so it cannot live in the lock. */
    token: u64,
    _not_send: PhantomData<*const ()>,
}

impl<'a> Drop for RwReadGuard<'a> {
    fn drop(&mut self) {
        unsafe { sync::kernel_rw_spinlock_read_unlock(self.lock.handle, self.token) }
    }
}

pub struct RwWriteGuard<'a> {
    lock: &'a RwSpinLock,
    flags: u64,
    _not_send: PhantomData<*const ()>,
}

impl<'a> Drop for RwWriteGuard<'a> {
    fn drop(&mut self) {
        unsafe { sync::kernel_rw_spinlock_write_unlock(self.lock.handle, self.flags) }
    }
}

/// Read-write mutex with writer priority.
///
/// Like `RwSpinLock` but yields the CPU (`Schedule()`) when contending instead
/// of busy-spinning.  Use in task context only — must not be called from IRQ
/// handlers or with interrupts disabled.
pub struct RwMutex {
    handle: usize,
}

impl RwMutex {
    pub fn new() -> Option<Self> {
        let h = unsafe { sync::kernel_rw_mutex_create() };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    pub fn read(&self) -> RwMutexReadGuard<'_> {
        unsafe { sync::kernel_rw_mutex_read_lock(self.handle) }
        RwMutexReadGuard { lock: self, _not_send: PhantomData }
    }

    pub fn write(&self) -> RwMutexWriteGuard<'_> {
        unsafe { sync::kernel_rw_mutex_write_lock(self.handle) }
        RwMutexWriteGuard { lock: self, _not_send: PhantomData }
    }
}

impl Drop for RwMutex {
    fn drop(&mut self) {
        unsafe { sync::kernel_rw_mutex_destroy(self.handle) }
    }
}

pub struct RwMutexReadGuard<'a> {
    lock: &'a RwMutex,
    _not_send: PhantomData<*const ()>,
}

impl<'a> Drop for RwMutexReadGuard<'a> {
    fn drop(&mut self) {
        unsafe { sync::kernel_rw_mutex_read_unlock(self.lock.handle) }
    }
}

pub struct RwMutexWriteGuard<'a> {
    lock: &'a RwMutex,
    _not_send: PhantomData<*const ()>,
}

impl<'a> Drop for RwMutexWriteGuard<'a> {
    fn drop(&mut self) {
        unsafe { sync::kernel_rw_mutex_write_unlock(self.lock.handle) }
    }
}

/// Signal a WaitGroup by raw handle from ISR context, where no `&WaitGroup`
/// borrow is possible (e.g. the handle was stashed in an inflight slot).
///
/// `handle` must come from `WaitGroup::raw_handle()` (or
/// `Completion::raw_handle()`) on a WaitGroup that is still alive.
pub fn waitgroup_done_raw(handle: usize) {
    unsafe { sync::kernel_waitgroup_done(handle) }
}

/// One-shot completion event.
///
/// Wraps a `WaitGroup` pre-armed with `add(1)`.  Call `complete()` exactly
/// once (typically from an ISR or another task) to unblock anyone calling
/// `wait()`.
pub struct Completion {
    wg: WaitGroup,
}

impl Completion {
    pub fn new() -> Option<Self> {
        let wg = WaitGroup::new()?;
        wg.add(1);
        Some(Self { wg })
    }

    /// Signal the completion (call once, typically from ISR context).
    pub fn complete(&self) {
        self.wg.done();
    }

    /// Block until `complete()` has been called.
    pub fn wait(&self) {
        self.wg.wait();
    }

    /// Raw WaitGroup handle for signaling via direct FFI from ISR context
    /// where borrowing `self` is not possible (e.g. stored in an inflight array).
    pub fn raw_handle(&self) -> usize {
        self.wg.raw_handle()
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
        let h = unsafe { sync::kernel_event_create() };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    /// Task context, one waiter at a time.
    pub fn wait(&self) {
        unsafe { sync::kernel_event_wait(self.handle) }
    }

    #[inline]
    pub fn signal(&self) {
        unsafe { sync::kernel_event_signal(self.handle) }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { sync::kernel_event_destroy(self.handle) }
    }
}

/// A spin lock that owns nothing: an atomic flag, with interrupts and
/// preemption off while it is held.
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
pub struct IrqSpinLock {
    held: core::sync::atomic::AtomicBool,
}

unsafe impl Send for IrqSpinLock {}
unsafe impl Sync for IrqSpinLock {}

impl IrqSpinLock {
    pub const fn new() -> Self {
        Self { held: core::sync::atomic::AtomicBool::new(false) }
    }

    pub fn lock(&self) -> IrqSpinGuard<'_> {
        use core::sync::atomic::Ordering;

        /* Interrupts off first: a CPU that takes an interrupt while holding
         * this, and whose handler takes it again, deadlocks against itself. */
        let flags = unsafe { ffi::cpu::kernel_irq_save() };
        while self.held.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        IrqSpinGuard { lock: self, flags }
    }
}

pub struct IrqSpinGuard<'a> {
    lock: &'a IrqSpinLock,
    flags: usize,
}

impl Drop for IrqSpinGuard<'_> {
    fn drop(&mut self) {
        use core::sync::atomic::Ordering;

        self.lock.held.store(false, Ordering::Release);
        unsafe { ffi::cpu::kernel_irq_restore(self.flags) };
    }
}
