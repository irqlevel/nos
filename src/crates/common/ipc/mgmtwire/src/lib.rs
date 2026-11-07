//! What the management bus carries -- between the providers that answer
//! (the kernel's own, a module's) and whoever asks (the API module, the
//! `mgmt` command) -- with nothing of the kernel in it:
//!
//! - [`json`]: JSON as the bus takes it. Whole numbers only -- the kernel
//!   keeps the FPU off, and a size or a time is a count anyway -- no key
//!   twice, valid UTF-8, and a limit on everything a text could make large:
//!   its length, how deep it nests, how many values, how long a string;
//! - [`Writer`]: JSON written as it goes, to a sink that may refuse more,
//!   keeping to the same rules -- and to JavaScript's: a count past 2^53,
//!   which a browser's number would round, is written as its digits;
//! - [`Params`]: the parameters a method is handed, taken by name and
//!   checked, and refused whole if one is left over that it does not know;
//! - [`Status`]: what a call ends with, as a code the C ABI carries and a
//!   name a client reads;
//! - [`descriptor`]: a provider's descriptor checked against what it
//!   registers.
//!
//! It runs in the kernel, so nothing here allocates without being able to
//! fail: what a text would make is refused when there is no memory for it,
//! rather than panic the machine.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod descriptor;
pub mod json;
mod params;
mod status;
mod writer;

pub use json::{Limits, ParseError, Value};
pub use params::{Invalid, Params};
pub use status::Status;
pub use writer::Writer;

/// The largest whole number a JavaScript number holds exactly; past it a
/// count is written, and may be read, as its digits in a string.
pub const SAFE_INT: u64 = (1 << 53) - 1;

/// Whether `name` may name a provider: a lowercase letter, then up to 23
/// lowercase letters, digits and `_`.
pub fn valid_provider(name: &str) -> bool {
    valid_name(name, 24, b"_")
}

/// Whether `name` may name a method: a lowercase letter, then up to 31
/// lowercase letters, digits, `_` and `.`.
pub fn valid_method(name: &str) -> bool {
    valid_name(name, 32, b"_.")
}

fn valid_name(name: &str, max: usize, punct: &[u8]) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || punct.contains(c))
}
