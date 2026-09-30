//! A loadable module's runtime, as the fuzzer loads one: `module!` makes
//! `insmod` of the module's init, and the value it returns is the loaded
//! module, whose drop is `rmmod`.

#![no_std]

extern crate alloc;

pub use kcore;

/// A module while it is loaded, as the real crate has it.
pub trait Module: Send {}

/// What `insmod` runs: the module's init, whose answer is the module --
/// or why it would not load.
pub type Init = fn() -> kcore::error::Result<alloc::boxed::Box<dyn Module>>;

/// Declares the module: `insmod` in the module's own namespace, which is
/// how the fuzzer reaches the init the macro names.
#[macro_export]
macro_rules! module {
    (name: $name:literal, init: $init:path $(,)?) => {
        /// The module's name, as `lsmod` shows it.
        pub const MODULE_NAME: &str = $name;

        /// `insmod`: the module's init, run.
        pub fn insmod() -> $crate::kcore::error::Result<::alloc::boxed::Box<dyn $crate::Module>> {
            $init()
        }
    };
}
