//! A loadable module's runtime, as the fuzzer loads one: `module!` makes
//! `insmod` of the module's init, and the value it returns is the loaded
//! module, whose drop is `rmmod`.

#![no_std]

extern crate alloc;

pub use kcore;

/// A module while it is loaded, as the real crate has it.
pub trait Module: Send {}

/// The ffi digest a module was built against: on the fuzzer's bench there
/// is no loader to compare it with.
pub const ABI: &str = "fuzz";

/// What `insmod` was given after a module's path, as the real crate has
/// it: words split at blanks, each `key=value` or a word alone.
pub struct Params<'a> {
    text: &'a str,
}

impl<'a> Params<'a> {
    pub fn new(text: &'a str) -> Params<'a> {
        Params { text }
    }

    pub fn text(&self) -> &'a str {
        self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn words(&self) -> impl Iterator<Item = (&'a str, Option<&'a str>)> + 'a {
        self.text.split([' ', '\t']).filter(|w| !w.is_empty()).map(|w| match w.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (w, None),
        })
    }
}

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
    (name: $name:literal, init_with_params: $init:path $(,)?) => {
        /// The module's name, as `lsmod` shows it.
        pub const MODULE_NAME: &str = $name;

        /// `insmod` with what its line had after the path: the module's
        /// init, run, saying what it says on `out` -- a command's output,
        /// which is where the fuzzer has one.
        pub fn insmod_with(params: &str, out: &mut $crate::kcore::cmd::Output)
                           -> $crate::kcore::error::Result<::alloc::boxed::Box<dyn $crate::Module>> {
            $init(&$crate::Params::new(params), out)
        }
    };
}
