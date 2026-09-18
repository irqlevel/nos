//! Things made once and then only read: what boot sets up, and what a layer
//! has one of.
//!
//! The kernel's rule is that nothing is built by a static's constructor, so
//! the one of something is a `static` that starts empty and is filled by an
//! explicit call. Written by hand that is an `UnsafeCell<Option<T>>` and an
//! `unsafe` at every read; here the filling is what is checked, once, and
//! reading is a load.

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

const EMPTY: u8 = 0;
const FILLING: u8 = 1;
const READY: u8 = 2;

/// A value set once -- by boot, by a `setup` -- and read from anywhere after.
pub struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

/* Reading shares the `T` between CPUs; setting moves one in from wherever. */
unsafe impl<T: Send + Sync> Sync for Once<T> {}
unsafe impl<T: Send> Send for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(EMPTY), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    /// Fills it. The value comes back when it was filled already, or is
    /// being.
    pub fn set(&self, value: T) -> Result<(), T> {
        if self.state
            .compare_exchange(EMPTY, FILLING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(value);
        }

        /* `FILLING` was taken by this call alone, and nobody reads the cell
         * before `READY`. */
        unsafe { (*self.value.get()).write(value) };
        self.state.store(READY, Ordering::Release);
        Ok(())
    }

    /// What it was filled with, or `None` until it has been.
    #[inline]
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) != READY {
            return None;
        }
        /* `READY` is stored after the value is whole and never leaves. */
        Some(unsafe { (*self.value.get()).assume_init_ref() })
    }
}

impl<T> Drop for Once<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == READY {
            unsafe { self.value.get_mut().assume_init_drop() };
        }
    }
}

/// The one of something, made on the heap the first time it is asked for.
/// Two callers racing to make it both get the same one, and the loser's is
/// dropped -- no state in which a reader has to wait, so it may be asked for
/// from any context the maker itself allows.
pub struct OnceBox<T> {
    ptr: AtomicPtr<T>,
    /* Owns a `T`, which an `AtomicPtr` does not say. */
    _owns: PhantomData<Box<T>>,
}

/* An `AtomicPtr` is `Sync` whatever it points at; what it points at here is
 * shared by `get` and moved in by whoever makes it. */
unsafe impl<T: Send + Sync> Sync for OnceBox<T> {}
unsafe impl<T: Send> Send for OnceBox<T> {}

impl<T> OnceBox<T> {
    pub const fn new() -> Self {
        Self { ptr: AtomicPtr::new(core::ptr::null_mut()), _owns: PhantomData }
    }

    #[inline]
    pub fn get(&self) -> Option<&T> {
        /* Null, or the box `get_or_try_init` leaked into it for as long as
         * `self` lives. */
        unsafe { self.ptr.load(Ordering::Acquire).as_ref() }
    }

    /// The one there is, made by `make` if there is none yet. `None` only
    /// when there is none and `make` could not make one.
    pub fn get_or_try_init(&self, make: impl FnOnce() -> Option<Box<T>>) -> Option<&T> {
        if let Some(existing) = self.get() {
            return Some(existing);
        }

        let made = Box::into_raw(make()?);
        match self.ptr.compare_exchange(
            core::ptr::null_mut(), made, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            /* Somebody else's is in; this one was never shared. */
            Err(_) => drop(unsafe { Box::from_raw(made) }),
        }
        self.get()
    }
}

impl<T> Drop for OnceBox<T> {
    fn drop(&mut self) {
        let ptr = *self.ptr.get_mut();
        if !ptr.is_null() {
            drop(unsafe { Box::from_raw(ptr) });
        }
    }
}
