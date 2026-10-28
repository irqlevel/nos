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

/* ---- a module calling the bus: the host API module's side ---- */

/// The longest provider's or method's name a module hands over: past it,
/// no name the bus takes.
const NAME_MAX: usize = 64;

/// `len` bytes at `ptr` as UTF-8, if they are.
///
/// # Safety
/// `ptr` is valid for reads of `len` bytes, or `len` is 0.
unsafe fn text<'a>(ptr: *const u8, len: usize, max: usize) -> Option<&'a str> {
    if len > max || (ptr.is_null() && len != 0) {
        return None;
    }
    if len == 0 {
        return Some("");
    }
    core::str::from_utf8(unsafe { core::slice::from_raw_parts(ptr, len) }).ok()
}

/// What a module hands its answers to, through `sink`: false when it takes
/// no more. Made only by the functions below, for the length of their call.
struct ModuleSink {
    sink: ffi::mgmt::MgmtSink,
    ctx: *mut c_void,
}

impl ModuleSink {
    fn take(&mut self, b: &[u8]) -> bool {
        /* The module's sink, with the ctx it passed for this call, which
         * it keeps valid until the call returns; `b` is valid for its
         * length while the sink runs */
        unsafe { (self.sink)(self.ctx, b.as_ptr(), b.len()) }
    }
}

/// `provider`'s `method` called with `params`, its answer -- at most
/// `max_reply` bytes -- handed to `sink(ctx, ...)`: what it ended with,
/// as a status code.
///
/// # Safety
/// The names and `params` are valid for reads of their lengths for the
/// call; `sink` may be called with `ctx` until it returns.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_call(provider: *const u8, provider_len: usize, method: *const u8,
                                          method_len: usize, params: *const u8, params_len: usize, max_reply: usize,
                                          sink: ffi::mgmt::MgmtSink, ctx: *mut c_void) -> i32 {
    let (Some(p), Some(m)) = (unsafe { text(provider, provider_len, NAME_MAX) }, unsafe { text(method, method_len, NAME_MAX) })
    else {
        return crate::Status::Invalid.code();
    };
    if params.is_null() && params_len != 0 {
        return crate::Status::Invalid.code();
    }
    let params = if params_len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(params, params_len) } };
    let mut out = ModuleSink { sink, ctx };
    crate::call(p, m, params, max_reply, &mut |b| out.take(b)).code()
}

/// The kind `provider`'s `method` is registered as -- `MGMT_READ`,
/// `MGMT_ACTION` or `MGMT_JOB` -- or a status code below 0.
///
/// # Safety
/// The names are valid for reads of their lengths.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_kind(provider: *const u8, provider_len: usize, method: *const u8,
                                          method_len: usize) -> i32 {
    let (Some(p), Some(m)) = (unsafe { text(provider, provider_len, NAME_MAX) }, unsafe { text(method, method_len, NAME_MAX) })
    else {
        return crate::Status::Invalid.code();
    };
    match crate::method_kind(p, m) {
        Ok(Kind::Read) => MGMT_READ as i32,
        Ok(Kind::Action) => MGMT_ACTION as i32,
        Ok(Kind::Job) => MGMT_JOB as i32,
        Err(s) => s.code(),
    }
}

/// Every provider with a method of the kinds `kinds` (`MGMT_KINDS_*`), as
/// the JSON `view::write_providers` writes, to `sink(ctx, ...)`: Ok, or
/// TooLarge when the sink took not all of it.
///
/// # Safety
/// `sink` may be called with `ctx` until this returns.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_providers(kinds: u32, sink: ffi::mgmt::MgmtSink, ctx: *mut c_void) -> i32 {
    let mut out = ModuleSink { sink, ctx };
    let mut take = |b: &[u8]| out.take(b);
    let mut w = crate::Writer::new(&mut take);
    crate::view::write_providers(&mut w, kinds);
    if w.ok() { crate::Status::Ok.code() } else { crate::Status::TooLarge.code() }
}

/// `provider`'s descriptor to `sink(ctx, ...)`: Ok, NoProvider, or
/// TooLarge when the sink took not all of it.
///
/// # Safety
/// `provider` is valid for reads of its length; `sink` may be called with
/// `ctx` until this returns.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_describe(provider: *const u8, provider_len: usize, sink: ffi::mgmt::MgmtSink,
                                              ctx: *mut c_void) -> i32 {
    let Some(p) = (unsafe { text(provider, provider_len, NAME_MAX) }) else { return crate::Status::Invalid.code() };
    let Some(d) = crate::descriptor(p) else { return crate::Status::NoProvider.code() };
    let mut out = ModuleSink { sink, ctx };
    if out.take(&d) { crate::Status::Ok.code() } else { crate::Status::TooLarge.code() }
}

/// A job of `provider`'s `method` started with `params`: its number, above
/// 0, or a status code below it.
///
/// # Safety
/// The names and `params` are valid for reads of their lengths.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_job_start(provider: *const u8, provider_len: usize, method: *const u8,
                                               method_len: usize, params: *const u8, params_len: usize) -> i64 {
    let (Some(p), Some(m)) = (unsafe { text(provider, provider_len, NAME_MAX) }, unsafe { text(method, method_len, NAME_MAX) })
    else {
        return i64::from(crate::Status::Invalid.code());
    };
    if params.is_null() && params_len != 0 {
        return i64::from(crate::Status::Invalid.code());
    }
    let params = if params_len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(params, params_len) } };
    match crate::job_start(p, m, params) {
        /* Numbers are counted from 1, and never reach i64's top */
        Ok(id) => i64::try_from(id).unwrap_or(i64::MAX),
        Err(s) => i64::from(s.code()),
    }
}

/// The job `id`'s record, its log from line `notes_since` and its result, as
/// the JSON `view::write_job` writes, to `sink(ctx, ...)`: Ok, NotFound, or
/// TooLarge when the sink took not all of it.
///
/// # Safety
/// `sink` may be called with `ctx` until this returns.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_job(id: u64, notes_since: u64, sink: ffi::mgmt::MgmtSink, ctx: *mut c_void) -> i32 {
    let mut out = ModuleSink { sink, ctx };
    let mut take = |b: &[u8]| out.take(b);
    let mut w = crate::Writer::new(&mut take);
    let status = crate::view::write_job(&mut w, id, notes_since);
    if status != crate::Status::Ok {
        return status.code();
    }
    if w.ok() { crate::Status::Ok.code() } else { crate::Status::TooLarge.code() }
}

/// Every job's record, as the JSON `view::write_jobs` writes, to
/// `sink(ctx, ...)`: Ok, or TooLarge when the sink took not all of it.
///
/// # Safety
/// `sink` may be called with `ctx` until this returns.
#[no_mangle]
pub unsafe extern "C" fn kernel_mgmt_jobs(sink: ffi::mgmt::MgmtSink, ctx: *mut c_void) -> i32 {
    let mut out = ModuleSink { sink, ctx };
    let mut take = |b: &[u8]| out.take(b);
    let mut w = crate::Writer::new(&mut take);
    crate::view::write_jobs(&mut w);
    if w.ok() { crate::Status::Ok.code() } else { crate::Status::TooLarge.code() }
}

/// The job `id` told to stop: Ok, or NotFound for one there is no record
/// of, or that is done.
#[no_mangle]
pub extern "C" fn kernel_mgmt_job_cancel(id: u64) -> i32 {
    if crate::job_cancel(id) { crate::Status::Ok.code() } else { crate::Status::NotFound.code() }
}
