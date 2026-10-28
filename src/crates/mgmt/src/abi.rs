//! The bus's C ABI: what a module registers and answers through
//! (ffi/src/mgmt.rs declares it, kcore::mgmt wraps it), and what the C++
//! calls to stop it at the halt. Everything a module
//! hands over is checked for its length before a slice is made of it, and
//! copied; a call's id is looked up, and an answer taken only from the
//! task its call runs on.

use crate::{active, add, unregister, Handler, Kind, Opaque, MAX_DESCRIPTOR, MAX_METHODS};
use core::ffi::c_void;
use ffi::mgmt::{MgmtHandler, MgmtMethod, MGMT_ACTION, MGMT_JOB, MGMT_READ};

/// The halt's part, after the modules are unloaded and before the
/// filesystems go: no call, job or provider starts from here on, every job
/// is told to stop and waited for.
#[no_mangle]
pub extern "C" fn rust_mgmt_stop() {
    crate::stop();
}

fn kind(k: u32) -> Option<Kind> {
    match k {
        MGMT_READ => Some(Kind::Read),
        MGMT_ACTION => Some(Kind::Action),
        MGMT_JOB => Some(Kind::Job),
        _ => None,
    }
}

/// A module's provider added; its handle, or 0 for anything refused.
///
/// # Safety
/// `name` and `descriptor` are valid for reads of their lengths, `methods`
/// of `count` entries, and each entry's name of its length, for the call:
/// what is kept is copied. `handler` is the module's, and stays callable
/// with `ctx` until `kernel_mgmt_unregister` of the handle returns.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_register(name: *const u8, name_len: usize, methods: *const MgmtMethod,
                                              count: usize, descriptor: *const u8, descriptor_len: usize,
                                              handler: MgmtHandler, ctx: *mut c_void) -> u64 {
    if name.is_null() || methods.is_null() || count == 0 || count > MAX_METHODS || name_len > 64
        || descriptor_len > MAX_DESCRIPTOR || (descriptor.is_null() && descriptor_len != 0) {
        return 0;
    }
    let name = unsafe { core::slice::from_raw_parts(name, name_len) };
    let Ok(name) = core::str::from_utf8(name) else { return 0 };
    let table = unsafe { core::slice::from_raw_parts(methods, count) };
    let mut named: [(&str, Kind); MAX_METHODS] = [("", Kind::Read); MAX_METHODS];
    for (slot, m) in named.iter_mut().zip(table) {
        if m.name.is_null() || m.name_len > 64 {
            return 0;
        }
        let bytes = unsafe { core::slice::from_raw_parts(m.name, m.name_len) };
        let (Ok(n), Some(k)) = (core::str::from_utf8(bytes), kind(m.kind)) else { return 0 };
        *slot = (n, k);
    }
    let descriptor = if descriptor_len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(descriptor, descriptor_len) } };
    add(name, named[..count].iter().copied(), count, descriptor, Handler::Module { f: handler, ctx: Opaque(ctx) })
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn kernel_mgmt_unregister(handle: u64) {
    unregister(handle);
}

/// # Safety
/// `buf` is valid for reads of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_reply(call: u64, buf: *const u8, len: usize) -> bool {
    let Some(state) = active(call) else { return false };
    if len == 0 {
        return true;
    }
    if buf.is_null() {
        return false;
    }
    state.append(unsafe { core::slice::from_raw_parts(buf, len) })
}

#[no_mangle]
pub extern "C" fn kernel_mgmt_progress(call: u64, done: u64, total: u64) {
    if let Some(j) = active(call).as_ref().and_then(|s| s.job()) {
        j.progress(done, total);
    }
}

/// # Safety
/// `line` is valid for reads of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_note(call: u64, line: *const u8, len: usize) {
    if line.is_null() {
        return;
    }
    let Some(state) = active(call) else { return };
    let Some(j) = state.job() else { return };
    /* No more than a line keeps, cut where its UTF-8 is whole */
    let bytes = unsafe { core::slice::from_raw_parts(line, len.min(crate::job::NOTE_MAX * 4)) };
    let text = match core::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
    };
    j.note(text);
}

#[no_mangle]
pub extern "C" fn kernel_mgmt_cancelled(call: u64) -> bool {
    active(call).as_ref().and_then(|s| s.job()).is_some_and(|j| j.cancelled())
}
