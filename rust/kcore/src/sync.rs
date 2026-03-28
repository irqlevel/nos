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
}

impl Drop for WaitGroup {
    fn drop(&mut self) {
        unsafe { sync::kernel_waitgroup_destroy(self.handle) }
    }
}
