//! The channel's C ABI: what a module reaches it through (ffi/src/ipc.rs
//! declares it, kcore::ipc wraps it), and what the C++ calls after a
//! module's exit and at the halt. Everything a module hands over is checked
//! for its length before a slice is made of it, and copied. A mailbox is
//! looked up by its address on every call, and only a module's own --
//! never one of the image's -- is closed, named, received from or sent from
//! through here.

use crate::{close, demonitor, handle, list, lookup, monitor, open, owner_at, register, send, unwatch, wait, watch,
            Addr, Error, Limits, Mbox, Payload, Reason, Scope, Taken, To, MAX_PAYLOAD, NAME_MAX, OWNER_NAME_MAX};
use alloc::sync::Arc;
use ffi::ipc::{IpcHeader, IpcName, IPC_NAME_MAX, IPC_RECV_CLOSED, IPC_RECV_NONE, IPC_RECV_TOO_SMALL,
               IPC_SCOPE_CLUSTER, IPC_SCOPE_HOST};
use kcore::time::Duration;

const _: () = assert!(NAME_MAX == IPC_NAME_MAX, "a name's bytes, the crate's and the ABI's");

/// The halt's part, after the modules are unloaded: no mailbox is made and
/// no message sent from here on.
#[no_mangle]
pub extern "C" fn rust_ipc_stop() {
    crate::stop();
}

/// After a module's exit: the mailboxes it left open closed, each said.
///
/// # Safety
/// `name` is readable for `len` bytes: the module table's name of the
/// module whose exit has just returned.
#[no_mangle]
pub unsafe extern "C" fn rust_ipc_module_gone(name: *const u8, len: usize) {
    if name.is_null() || len == 0 || len > OWNER_NAME_MAX {
        return;
    }
    crate::module_gone(unsafe { core::slice::from_raw_parts(name, len) });
}

/// The `len` bytes at `p`, at most `max` of them: None past that, or for a
/// null pointer with a length.
///
/// # Safety
/// `p` is readable for `len` bytes when it is not null.
unsafe fn bytes<'a>(p: *const u8, len: usize, max: usize) -> Option<&'a [u8]> {
    if len > max {
        return None;
    }
    if len == 0 {
        return Some(&[]);
    }
    if p.is_null() {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(p, len) })
}

/// The `len` bytes at `p` as text, at most `max` of them.
///
/// # Safety
/// As `bytes`.
unsafe fn text<'a>(p: *const u8, len: usize, max: usize) -> Option<&'a str> {
    core::str::from_utf8(unsafe { bytes(p, len, max) }?).ok()
}

/// A module's mailbox at `addr`: never one of the image's.
fn module_box(addr: u64) -> Option<Arc<Mbox>> {
    handle(Addr::from_u64(addr)).filter(|m| !m.is_image())
}

#[no_mangle]
pub extern "C" fn kernel_ipc_open(owner: usize, messages: u32, bytes: u64) -> u64 {
    let Ok(bytes) = usize::try_from(bytes) else { return 0 };
    let owner = owner_at(owner);
    if owner.is_image() {
        /* A module's call names its own code: an address in no module is no
         * module's mailbox to make */
        return 0;
    }
    match open(owner, Limits { messages, bytes }) {
        /* The table keeps the mailbox; the module holds its address */
        Ok(m) => m.addr().to_u64(),
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "C" fn kernel_ipc_close(mbox: u64) {
    if let Some(m) = module_box(mbox) {
        close(&m, Reason::Closed);
    }
}

/// # Safety
/// `name` is readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_register(mbox: u64, name: *const u8, len: usize, scope: u32) -> i32 {
    let Some(m) = module_box(mbox) else { return Error::Gone.code() };
    let Some(name) = (unsafe { text(name, len, IPC_NAME_MAX) }) else { return Error::Invalid.code() };
    let scope = match scope {
        IPC_SCOPE_HOST => Scope::Host,
        IPC_SCOPE_CLUSTER => Scope::Cluster,
        _ => return Error::Invalid.code(),
    };
    match register(&m, name, scope) {
        Ok(()) => 0,
        Err(e) => e.code(),
    }
}

/// # Safety
/// `name` is readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_lookup(name: *const u8, len: usize) -> u64 {
    match unsafe { text(name, len, IPC_NAME_MAX) } {
        Some(name) => lookup(name).map_or(0, Addr::to_u64),
        None => 0,
    }
}

/// # Safety
/// `name` is readable for `name_len` bytes, and `data` for `len`.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_send(mbox: u64, to: u64, name: *const u8, name_len: usize, tag: u32,
                                         reference: u64, data: *const u8, len: usize) -> i32 {
    let Some(m) = module_box(mbox) else { return Error::Gone.code() };
    if len > MAX_PAYLOAD {
        return Error::TooLarge.code();
    }
    let Some(data) = (unsafe { bytes(data, len, MAX_PAYLOAD) }) else { return Error::Invalid.code() };
    let target = if name_len == 0 {
        To::Addr(Addr::from_u64(to))
    } else {
        match unsafe { text(name, name_len, IPC_NAME_MAX) } {
            Some(n) => To::Name(n),
            None => return Error::Invalid.code(),
        }
    };
    let payload = match Payload::from_slice(data) {
        Ok(p) => p,
        Err(e) => return e.code(),
    };
    match send(m.addr(), target, tag, reference, payload) {
        Ok(()) => 0,
        Err((e, _)) => e.code(),
    }
}

/// # Safety
/// `header` is writable, and `buf` writable for `cap` bytes when `cap` is
/// past 0.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_recv(mbox: u64, timeout_ns: u64, header: *mut IpcHeader, buf: *mut u8,
                                         cap: usize) -> i64 {
    if header.is_null() || (buf.is_null() && cap != 0) {
        return IPC_RECV_CLOSED;
    }
    let Some(m) = module_box(mbox) else { return IPC_RECV_CLOSED };
    /* A buffer longer than any payload is never needed whole */
    let cap = cap.min(MAX_PAYLOAD);
    match wait(&m, Duration::from_nanos(timeout_ns), cap) {
        Taken::Message(msg) => {
            let out: &mut [u8] = if cap == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf, cap) } };
            let n = msg.payload().read(0, out);
            let h = IpcHeader { from: msg.from().to_u64(), reference: msg.reference(), len: n as u64, tag: msg.tag(),
                                _pad: 0 };
            unsafe { header.write(h) };
            /* The message's pieces go back here, after the copy */
            drop(msg);
            n as i64
        }
        Taken::TooSmall { len } => {
            unsafe { header.write(IpcHeader { len: len as u64, ..IpcHeader::default() }) };
            IPC_RECV_TOO_SMALL
        }
        Taken::Nothing => IPC_RECV_NONE,
        Taken::Closed => IPC_RECV_CLOSED,
    }
}

#[no_mangle]
pub extern "C" fn kernel_ipc_wake(mbox: u64) {
    if let Some(m) = module_box(mbox) {
        m.wake();
    }
}

#[no_mangle]
pub extern "C" fn kernel_ipc_monitor(mbox: u64, target: u64) -> u64 {
    let Some(m) = module_box(mbox) else { return 0 };
    monitor(&m, Addr::from_u64(target)).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn kernel_ipc_demonitor(mbox: u64, id: u64) {
    if let Some(m) = module_box(mbox) {
        demonitor(&m, id);
    }
}

/// # Safety
/// `prefix` is readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_watch(mbox: u64, prefix: *const u8, len: usize) -> u64 {
    let Some(m) = module_box(mbox) else { return 0 };
    match unsafe { text(prefix, len, IPC_NAME_MAX) } {
        Some(p) => watch(&m, p).unwrap_or(0),
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn kernel_ipc_unwatch(mbox: u64, id: u64) {
    if let Some(m) = module_box(mbox) {
        unwatch(&m, id);
    }
}

/// # Safety
/// `prefix` is readable for `len` bytes, and `out` writable for `max`
/// entries.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_list(prefix: *const u8, len: usize, out: *mut IpcName, max: usize) -> usize {
    if out.is_null() || max == 0 {
        return 0;
    }
    let Some(prefix) = (unsafe { text(prefix, len, IPC_NAME_MAX) }) else { return 0 };
    let mut n = 0;
    for info in list(prefix).iter().take(max) {
        let mut rec = IpcName {
            addr: info.addr.to_u64(),
            scope: if info.scope == Scope::Cluster { IPC_SCOPE_CLUSTER } else { IPC_SCOPE_HOST },
            len: info.name.len() as u32,
            name: [0; IPC_NAME_MAX],
        };
        /* A name is NAME_MAX bytes at most, which is IPC_NAME_MAX */
        let k = info.name.len().min(IPC_NAME_MAX);
        rec.name[..k].copy_from_slice(&info.name.as_bytes()[..k]);
        /* `n` is under `max`, which `out` is writable for */
        unsafe { out.add(n).write(rec) };
        n += 1;
    }
    n
}
