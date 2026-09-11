#![no_std]
#![feature(alloc_error_handler)]

//! The runtime of a loadable kernel module.
//!
//! A module is a `staticlib` crate that depends on this one and invokes
//! [`module!`] once. The build (the Makefile's module rules) links it into a
//! position-independent ELF shared object, and `insmod` loads that into the
//! running kernel. Everything a module asks of the kernel goes through `ffi`
//! -- the C API the drivers built into the kernel use -- and `kcore` wraps
//! it: the loader resolves a module's calls against that API and nothing
//! else, so a module can do what the in-kernel Rust can, and no more.
//!
//! What this crate adds is what every such object needs exactly once: the
//! allocator, the panic and allocation-error handlers, and the descriptor
//! the loader looks for.

extern crate alloc;

pub use kcore;

use alloc::boxed::Box;
use core::ffi::c_void;

#[global_allocator]
static ALLOCATOR: ffi::alloc::KernelAllocator = ffi::alloc::KernelAllocator;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    ffi::panic::panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(_layout: core::alloc::Layout) -> ! {
    ffi::panic::alloc_error()
}

/// A module while it is loaded. Its init hands one back and `rmmod` drops
/// it, and that drop is where the module lets go of what it holds -- the
/// commands it registered, the tasks it started. `Send`, because the task
/// that drops it is not the one that made it; nothing shares it, so it need
/// not be `Sync`.
pub trait Module: Send {}

/// `"NOSM"`, the first field of [`ModuleInfo`].
pub const MAGIC: u32 = 0x4D53_4F4E;
/// The layout of [`ModuleInfo`]; bumped with any change to it.
pub const INFO_VERSION: u32 = 1;
pub const NAME_LEN: usize = 32;
pub const ABI_LEN: usize = 64;

/// A hash of the `ffi` crate's sources, set by the build. A module built
/// against another `ffi` would call the kernel with signatures it no longer
/// has, so the loader refuses one whose hash is not the kernel's -- and one
/// built outside the Makefile, which carries "unset".
pub const ABI: &str = match option_env!("NOS_MODULE_ABI") {
    Some(abi) => abi,
    None => "unset",
};

/// The descriptor the loader looks up, by the name `nos_module_info`.
#[repr(C)]
pub struct ModuleInfo {
    pub magic: u32,
    pub version: u32,
    pub abi: [u8; ABI_LEN],
    pub name: [u8; NAME_LEN],
    pub init: unsafe extern "C" fn() -> *mut c_void,
    pub exit: unsafe extern "C" fn(state: *mut c_void),
}

/// `s` as a NUL-terminated, NUL-padded array, for the name: one that does
/// not fit, NUL included, fails the build.
pub const fn fixed<const N: usize>(s: &str) -> [u8; N] {
    assert!(s.len() < N, "string too long for its field");
    padded(s)
}

/// `s` NUL-padded to N bytes, all N of them if it is that long -- the ABI
/// digest is 64 hex digits in a 64-byte field, which the loader compares
/// whole. One longer fails the build.
pub const fn padded<const N: usize>(s: &str) -> [u8; N] {
    let bytes = s.as_bytes();
    assert!(bytes.len() <= N, "string too long for its field");
    let mut out = [0u8; N];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

#[doc(hidden)]
pub fn __init(init: fn() -> kcore::error::Result<Box<dyn Module>>) -> *mut c_void {
    match init() {
        /* A trait object is a fat pointer, and the loader keeps a thin one. */
        Ok(module) => Box::into_raw(Box::new(module)) as *mut c_void,
        Err(e) => {
            kcore::trace!(0, "module init failed: {}", e);
            core::ptr::null_mut()
        }
    }
}

#[doc(hidden)]
pub unsafe fn __exit(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut Box<dyn Module>) });
    }
}

/// Declares the module: its name, as `lsmod` shows it and `rmmod` takes it,
/// and the function `insmod` runs.
///
/// ```ignore
/// fn init() -> kcore::error::Result<Box<dyn kmod::Module>> { ... }
/// kmod::module!(name: "hello", init: init);
/// ```
#[macro_export]
macro_rules! module {
    (name: $name:literal, init: $init:path $(,)?) => {
        #[no_mangle]
        #[used]
        #[allow(non_upper_case_globals)]
        pub static nos_module_info: $crate::ModuleInfo = $crate::ModuleInfo {
            magic: $crate::MAGIC,
            version: $crate::INFO_VERSION,
            abi: $crate::padded($crate::ABI),
            name: $crate::fixed($name),
            init: __nos_module_init,
            exit: __nos_module_exit,
        };

        unsafe extern "C" fn __nos_module_init() -> *mut core::ffi::c_void {
            $crate::__init($init)
        }

        unsafe extern "C" fn __nos_module_exit(state: *mut core::ffi::c_void) {
            unsafe { $crate::__exit(state) }
        }
    };
}
