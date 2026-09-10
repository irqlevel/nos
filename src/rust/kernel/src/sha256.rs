//! SHA-256 for the C++ side -- the `sha256` shell command, which checks a
//! downloaded kernel against its release's SHA256SUMS. RustCrypto's sha2 is
//! in the tree already for TLS; this hands it out through a C API, the
//! wrapper being `Kernel::Sha256Hash` (kernel/sha256.h).

use alloc::boxed::Box;
use sha2::{Digest, Sha256};

pub struct Sha256Ctx(Sha256);

/// A fresh hash, released by `sha256_finish` or `sha256_free`.
#[no_mangle]
pub extern "C" fn sha256_new() -> *mut Sha256Ctx {
    Box::into_raw(Box::new(Sha256Ctx(Sha256::new())))
}

/// Feeds `len` bytes at `data` into the hash.
///
/// # Safety
/// `ctx` must come from `sha256_new` and not have been released; `data`
/// must be readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn sha256_update(ctx: *mut Sha256Ctx, data: *const u8, len: usize) {
    if ctx.is_null() || data.is_null() || len == 0 {
        return;
    }
    (*ctx).0.update(core::slice::from_raw_parts(data, len));
}

/// Writes the 32-byte digest to `out` and releases `ctx`.
///
/// # Safety
/// `ctx` must come from `sha256_new` and is invalid afterwards; `out` must
/// be writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn sha256_finish(ctx: *mut Sha256Ctx, out: *mut u8) {
    if ctx.is_null() {
        return;
    }
    let ctx = Box::from_raw(ctx);
    if out.is_null() {
        return;
    }
    let digest = ctx.0.finalize();
    core::ptr::copy_nonoverlapping(digest.as_ptr(), out, digest.len());
}

/// Releases a hash without taking its digest.
///
/// # Safety
/// `ctx` is null or comes from `sha256_new`, and is invalid afterwards.
#[no_mangle]
pub unsafe extern "C" fn sha256_free(ctx: *mut Sha256Ctx) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx));
    }
}
