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
use core::fmt::Write;
use kcore::cmd::Output;
use kcore::error::Result;

#[global_allocator]
static ALLOCATOR: ffi::alloc::KernelAllocator = ffi::alloc::KernelAllocator;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    ffi::panic::panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    ffi::panic::alloc_error(layout)
}

/// A module while it is loaded. Its init hands one back and `rmmod` drops
/// it, and that drop is where the module lets go of what it holds -- the
/// commands it registered, the tasks it started. `Send`, because the task
/// that drops it is not the one that made it; nothing shares it, so it need
/// not be `Sync`.
pub trait Module: Send {}

/// `"NOSM"`, the first field of [`ModuleInfo`].
pub const MAGIC: u32 = 0x4D53_4F4E;
/// The layout of [`ModuleInfo`]; bumped with any change to it. 2: the init
/// is handed the module's parameters and `insmod`'s output.
pub const INFO_VERSION: u32 = 2;
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
    /// `params`: what `insmod`'s line had after the path, `len` bytes of
    /// it; `out`: the `Stdlib::Printer` its output goes to. Both valid while
    /// the init runs (`ModuleInfo` in kernel/module.cpp).
    pub init: unsafe extern "C" fn(params: *const u8, len: usize, out: *mut c_void) -> *mut c_void,
    pub exit: unsafe extern "C" fn(state: *mut c_void),
}

/// What `insmod` was given after a module's path, for an init that takes
/// parameters (`module!(name: ..., init_with_params: ...)`): words split
/// at blanks, each `key=value` or a word alone -- a value has no blank in
/// it. Lent for the init's call: what is kept of it is copied.
pub struct Params<'a> {
    text: &'a str,
}

impl<'a> Params<'a> {
    /// The parameters as they were typed.
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Whether there were none.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Each word as `(key, value)`: `vms=/data/vms` is `("vms",
    /// Some("/data/vms"))`, `debug` is `("debug", None)`.
    pub fn words(&self) -> impl Iterator<Item = (&'a str, Option<&'a str>)> + 'a {
        self.text.split([' ', '\t']).filter(|w| !w.is_empty()).map(|w| match w.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (w, None),
        })
    }
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

/// What an init made, as the loader keeps it.
fn handed_back(name: &str, made: Result<Box<dyn Module>>) -> *mut c_void {
    match made {
        /* A trait object is a fat pointer, and the loader keeps a thin one. */
        Ok(module) => Box::into_raw(Box::new(module)) as *mut c_void,
        Err(e) => {
            kcore::trace!(0, "module {}: init failed: {}", name, e);
            core::ptr::null_mut()
        }
    }
}

/// The init of a module that takes no parameters (`module!(name: ...,
/// init: ...)`): given some, it refuses the load, rather than drop words
/// meant for it without a sound.
#[doc(hidden)]
pub fn __no_params(name: &str, params: &Params, out: &mut Output,
                   init: fn() -> Result<Box<dyn Module>>) -> Result<Box<dyn Module>> {
    if !params.is_empty() {
        let _ = writeln!(out, "{}: takes no parameters, and was given \"{}\"", name, params.text());
        return Err(kcore::error::Error::InvalidValue);
    }
    init()
}

/// What the loader runs as a module's init: the parameters and `insmod`'s
/// output, lent to `init` for its call, and what it made handed back as
/// the loader keeps it.
///
/// # Safety
/// `params` is readable for `len` bytes, and `out` is a `Stdlib::Printer*`,
/// for as long as the call runs: what the loader hands an init.
#[doc(hidden)]
pub unsafe fn __init(name: &str, params: *const u8, len: usize, out: *mut c_void,
                     init: fn(&Params, &mut Output) -> Result<Box<dyn Module>>) -> *mut c_void {
    let mut out = unsafe { Output::from_raw(out) };
    let bytes: &[u8] = if len == 0 || params.is_null() {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(params, len) }
    };
    let Ok(text) = core::str::from_utf8(bytes) else {
        let _ = writeln!(out, "{}: its parameters are not UTF-8", name);
        return core::ptr::null_mut();
    };
    handed_back(name, init(&Params { text }, &mut out))
}

#[doc(hidden)]
pub unsafe fn __exit(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut Box<dyn Module>) });
    }
}

/// Declares the module: its name, as `lsmod` shows it and `rmmod` takes it,
/// and the function `insmod` runs -- with `init_with_params`, handed what
/// `insmod` was given after the path, and its output to say what it makes
/// of them; with `init`, a module that takes none, whose load any is
/// refused.
///
/// ```ignore
/// fn init() -> kcore::error::Result<Box<dyn kmod::Module>> { ... }
/// kmod::module!(name: "hello", init: init);
///
/// fn init(params: &kmod::Params, out: &mut kcore::cmd::Output)
///     -> kcore::error::Result<Box<dyn kmod::Module>> { ... }
/// kmod::module!(name: "hv", init_with_params: init);
/// ```
#[macro_export]
macro_rules! module {
    (@header $name:literal, $init:expr) => {
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

        unsafe extern "C" fn __nos_module_init(
            params: *const u8,
            len: usize,
            out: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void {
            /* What the loader hands an init: ModuleInfo's comment. */
            unsafe { $crate::__init($name, params, len, out, $init) }
        }

        unsafe extern "C" fn __nos_module_exit(state: *mut core::ffi::c_void) {
            unsafe { $crate::__exit(state) }
        }
    };
    (name: $name:literal, init: $init:path $(,)?) => {
        $crate::module!(@header $name, |params, out| $crate::__no_params($name, params, out, $init));
    };
    (name: $name:literal, init_with_params: $init:path $(,)?) => {
        $crate::module!(@header $name, $init);
    };
}
