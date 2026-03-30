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
pub struct RwSpinLock {
    handle: usize,
}

impl RwSpinLock {
    pub fn new() -> Option<Self> {
        let h = unsafe { sync::kernel_rw_spinlock_create() };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    pub fn read(&self) -> RwReadGuard<'_> {
        unsafe { sync::kernel_rw_spinlock_read_lock(self.handle) }
        RwReadGuard { lock: self, _not_send: PhantomData }
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
    _not_send: PhantomData<*const ()>,
}

impl<'a> Drop for RwReadGuard<'a> {
    fn drop(&mut self) {
        unsafe { sync::kernel_rw_spinlock_read_unlock(self.lock.handle) }
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
