//! The channel's C ABI: what a module reaches it through (ffi/src/ipc.rs
//! declares it, kcore::ipc wraps it), and what the C++ calls after a
//! module's exit and at the halt. Everything a module hands over is checked
//! for its length before a slice is made of it, and copied. A mailbox is
//! looked up by its address on every call, and only a module's own --
//! never one of the image's -- is closed, named, received from or sent from
//! through here.

use crate::{close, demonitor, handle, list, lookup, monitor, open, owner_at, register, send, unwatch, wait, wait_part,
            watch, Addr, Error, Limits, Mbox, Part, Payload, Reason, Scope, Taken, To, MAX_PAYLOAD, NAME_MAX,
            OWNER_NAME_MAX};
use alloc::sync::Arc;
use alloc::vec::Vec;
use ffi::ipc::{IpcHeader, IpcName, IpcPiece, IPC_NAME_MAX, IPC_OK, IPC_PIECES_MAX, IPC_RECV_CLOSED, IPC_RECV_NONE,
               IPC_RECV_STALE, IPC_RECV_TOO_SMALL, IPC_SCOPE_CLUSTER, IPC_SCOPE_HOST};
use kcore::time::Duration;

const _: () = assert!(NAME_MAX == IPC_NAME_MAX, "a name's bytes, the crate's and the ABI's");
const _: () = assert!(ffi::ipc::IPC_REMOTE_NAME_MAX == NAME_MAX + 1 + crate::remote::HOST_NAME_MAX,
                      "a name with its host, the crate's and the ABI's");

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
pub extern "C" fn kernel_ipc_open(owner: usize, messages: u32, bytes: u64, event: u64) -> u64 {
    let Ok(bytes) = usize::try_from(bytes) else { return 0 };
    let owner = owner_at(owner);
    if owner.is_image() {
        /* A module's call names its own code: an address in no module is no
         * module's mailbox to make */
        return 0;
    }
    /* The task's event, looked up: the mailbox holds it from here on, so the
     * module may let go of its handle first */
    let event = match event {
        0 => None,
        handle => match events::lookup(handle) {
            Some(e) => Some(e),
            None => return 0,
        },
    };
    match open(owner, Limits { messages, bytes }, event) {
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
        /* A name, or `name@host` on a peer */
        match unsafe { text(name, name_len, ffi::ipc::IPC_REMOTE_NAME_MAX) } {
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

/// # Safety
/// `header` and `ticket` are writable, and `buf` writable for `cap` bytes
/// when `cap` is past 0.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_recv_part(mbox: u64, timeout_ns: u64, header: *mut IpcHeader, buf: *mut u8,
                                              cap: usize, offset: u64, ticket: *mut u64) -> i64 {
    if header.is_null() || ticket.is_null() || (buf.is_null() && cap != 0) {
        return IPC_RECV_CLOSED;
    }
    let Some(m) = module_box(mbox) else { return IPC_RECV_CLOSED };
    let Ok(offset) = usize::try_from(offset) else { return IPC_RECV_STALE };
    let cap = cap.min(MAX_PAYLOAD);
    let out: &mut [u8] = if cap == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf, cap) } };
    // SAFETY: the caller's writable ticket.
    let mut t = unsafe { ticket.read() };
    let got = wait_part(&m, Duration::from_nanos(timeout_ns), offset, out, &mut t);
    unsafe { ticket.write(t) };
    match got {
        Part::Got { from, tag, reference, len, n } => {
            let h = IpcHeader { from: from.to_u64(), reference, len: len as u64, tag, _pad: 0 };
            unsafe { header.write(h) };
            n as i64
        }
        Part::Nothing => IPC_RECV_NONE,
        Part::Closed => IPC_RECV_CLOSED,
        Part::Stale => IPC_RECV_STALE,
    }
}

/// The `count` pieces at `pieces` as one payload, copied: refused for more
/// than IPC_PIECES_MAX of them, a null one with bytes, or more than the
/// channel's largest payload in all.
///
/// # Safety
/// `pieces` points at `count` readable `IpcPiece`s when `count` is past 0,
/// each naming `len` readable bytes.
unsafe fn payload_of(pieces: *const IpcPiece, count: usize) -> Result<Payload, Error> {
    if count == 0 {
        return Ok(Payload::empty());
    }
    if count > IPC_PIECES_MAX || pieces.is_null() {
        return Err(Error::Invalid);
    }
    // SAFETY: the caller's `count` pieces.
    let given = unsafe { core::slice::from_raw_parts(pieces, count) };
    let mut parts: Vec<&[u8]> = Vec::new();
    parts.try_reserve_exact(count).map_err(|_| Error::NoMemory)?;
    let mut total = 0usize;
    for p in given {
        if p.len > 0 && p.data.is_null() {
            return Err(Error::Invalid);
        }
        total = total.checked_add(p.len).ok_or(Error::TooLarge)?;
        if total > MAX_PAYLOAD {
            return Err(Error::TooLarge);
        }
        // SAFETY: the caller's `len` bytes at the piece's data.
        parts.push(if p.len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(p.data, p.len) } });
    }
    Payload::gather(&parts)
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

/// The name of the peer whose number `host` is now -- up, of that epoch --
/// into the `cap` bytes at `out`: its length, or 0 for a number that is no
/// peer's up now, or a `cap` its name does not fit.
///
/// # Safety
/// `out` takes `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_peer_name(host: u32, out: *mut u8, cap: usize) -> usize {
    let Ok(host) = u16::try_from(host) else { return 0 };
    if out.is_null() || host == 0 {
        return 0;
    }
    let Some(name) = crate::remote::peer_name(host) else { return 0 };
    if name.len() > cap {
        return 0;
    }
    /* `out` takes `cap` bytes, and the name is no longer */
    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), out, name.len()) };
    name.len()
}

/* ---- the router's: ipcnet's ---- */

#[no_mangle]
pub extern "C" fn kernel_ipc_route(mbox: u64) -> i32 {
    let Some(m) = module_box(mbox) else { return Error::Gone.code() };
    match crate::remote::route(&m) {
        Ok(()) => IPC_OK,
        Err(e) => e.code(),
    }
}

/// # Safety
/// `name` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_peer_up(router: u64, name: *const u8, len: usize) -> u32 {
    let Some(m) = module_box(router) else { return 0 };
    if name.is_null() || len == 0 || len > crate::remote::HOST_NAME_MAX {
        return 0;
    }
    // SAFETY: the caller's `len` bytes at `name`.
    let bytes = unsafe { core::slice::from_raw_parts(name, len) };
    let Ok(name) = core::str::from_utf8(bytes) else { return 0 };
    crate::remote::peer_up(&m, name).map_or(0, u32::from)
}

#[no_mangle]
pub extern "C" fn kernel_ipc_peer_lost(router: u64, peer: u32) -> i32 {
    let Some(m) = module_box(router) else { return Error::Gone.code() };
    let Ok(host) = u16::try_from(peer) else { return Error::Invalid.code() };
    match crate::remote::peer_lost(&m, host) {
        Ok(()) => IPC_OK,
        Err(e) => e.code(),
    }
}

/// # Safety
/// `name` points at `name_len` readable bytes when `name_len` is past 0,
/// and `pieces` at `count` readable `IpcPiece`s, each naming its readable
/// bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_deliver_remote(router: u64, peer: u32, from: u64, to: u64, name: *const u8,
                                                   name_len: usize, tag: u32, reference: u64,
                                                   pieces: *const IpcPiece, count: usize) -> i32 {
    let Some(m) = module_box(router) else { return Error::Gone.code() };
    let Ok(host) = u16::try_from(peer) else { return Error::Invalid.code() };
    if name_len > crate::NAME_MAX || (name_len > 0 && name.is_null()) {
        return Error::Invalid.code();
    }
    let to = if name_len > 0 {
        // SAFETY: the caller's `name_len` bytes at `name`.
        let bytes = unsafe { core::slice::from_raw_parts(name, name_len) };
        match core::str::from_utf8(bytes) {
            Ok(n) if crate::valid_name(n) => crate::remote::RemoteTo::Name(n),
            _ => return Error::Invalid.code(),
        }
    } else {
        crate::remote::RemoteTo::Addr(Addr::from_u64(to))
    };
    // SAFETY: the caller's `count` pieces, passed on.
    let payload = match unsafe { payload_of(pieces, count) } {
        Ok(p) => p,
        Err(e) => return e.code(),
    };
    match crate::remote::deliver_remote(&m, host, from, to, tag, reference, payload) {
        Ok(()) => IPC_OK,
        Err(e) => e.code(),
    }
}

/// # Safety
/// `name` points at `name_len` readable bytes when `name_len` is past 0.
#[no_mangle]
pub unsafe extern "C" fn kernel_ipc_bounced(router: u64, peer: u32, sender: u64, target: u64, name: *const u8,
                                            name_len: usize, tag: u32, reference: u64, why: u32) -> i32 {
    let Some(m) = module_box(router) else { return Error::Gone.code() };
    let Ok(host) = u16::try_from(peer) else { return Error::Invalid.code() };
    let Some(why) = crate::Bounce::from_u32(why) else { return Error::Invalid.code() };
    if name_len > crate::NAME_MAX || (name_len > 0 && name.is_null()) {
        return Error::Invalid.code();
    }
    let target = if name_len > 0 {
        // SAFETY: the caller's `name_len` bytes at `name`.
        let bytes = unsafe { core::slice::from_raw_parts(name, name_len) };
        match core::str::from_utf8(bytes) {
            Ok(n) if crate::valid_name(n) => crate::remote::RemoteTo::Name(n),
            _ => return Error::Invalid.code(),
        }
    } else {
        crate::remote::RemoteTo::Addr(Addr::from_u64(target))
    };
    match crate::remote::bounced(&m, host, Addr::from_u64(sender), target, tag, reference, why) {
        Ok(()) => IPC_OK,
        Err(e) => e.code(),
    }
}

#[no_mangle]
pub extern "C" fn kernel_ipc_remote_down(router: u64, peer: u32, id: u64, reason: u32) -> i32 {
    let Some(m) = module_box(router) else { return Error::Gone.code() };
    let Ok(host) = u16::try_from(peer) else { return Error::Invalid.code() };
    let Some(reason) = Reason::from_u32(reason) else { return Error::Invalid.code() };
    match crate::remote::remote_down(&m, host, id, reason) {
        Ok(()) => IPC_OK,
        Err(e) => e.code(),
    }
}
