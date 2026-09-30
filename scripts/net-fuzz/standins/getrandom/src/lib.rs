//! What of getrandom 0.2 the crates in the fuzzer use -- `getrandom`,
//! `Error` and `register_custom_getrandom!` -- over the custom source alone.
//! The macro is the real one's, and so is the symbol it defines: the `tls`
//! crate's registration is what the fuzzer's randomness comes from, as the
//! kernel's does.

#![no_std]

use core::num::NonZeroU32;

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Error(NonZeroU32);

impl Error {
    /// The real crate's code for a platform with no source.
    pub const UNSUPPORTED: Error = Error(match NonZeroU32::new(Self::INTERNAL_START + 2) {
        Some(code) => code,
        None => panic!("a nonzero code"),
    });
    pub const INTERNAL_START: u32 = 1 << 31;
    pub const CUSTOM_START: u32 = (1 << 31) + (1 << 30);

    pub fn code(self) -> NonZeroU32 {
        self.0
    }
}

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "getrandom::Error({})", self.0)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "getrandom error {}", self.0)
    }
}

impl From<NonZeroU32> for Error {
    fn from(code: NonZeroU32) -> Self {
        Error(code)
    }
}

extern "Rust" {
    /// Defined by `register_custom_getrandom!`: 0, or an error's code.
    fn __getrandom_custom(dest: *mut u8, len: usize) -> u32;
}

pub fn getrandom(dest: &mut [u8]) -> Result<(), Error> {
    if dest.is_empty() {
        return Ok(());
    }
    // SAFETY: `dest` is `len` writable bytes, which is what the registered
    // function is handed and all it touches.
    let code = unsafe { __getrandom_custom(dest.as_mut_ptr(), dest.len()) };
    match NonZeroU32::new(code) {
        None => Ok(()),
        Some(code) => Err(Error(code)),
    }
}

/// The real crate's macro, to the letter of what it defines.
#[macro_export]
macro_rules! register_custom_getrandom {
    ($path:path) => {
        const _: () = {
            #[no_mangle]
            unsafe extern "Rust" fn __getrandom_custom(dest: *mut u8, len: usize) -> u32 {
                let f: fn(&mut [u8]) -> ::core::result::Result<(), $crate::Error> = $path;
                // SAFETY: the stand-in's `getrandom` passes a slice's pointer
                // and length, and nothing else calls this.
                let slice = unsafe { ::core::slice::from_raw_parts_mut(dest, len) };
                match f(slice) {
                    Ok(()) => 0,
                    Err(e) => e.code().get(),
                }
            }
        };
    };
}
