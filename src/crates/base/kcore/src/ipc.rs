//! The message channel as a loadable module reaches it (docs/ipc.md): a
//! mailbox of the module's own -- closed when dropped -- messages sent from
//! it by address or by name and received into it, monitors that say when a
//! peer is gone, and watches that say when a name comes and goes. Inside
//! the kernel image the channel is the `ipc` crate itself.
//!
//! The channel calls nothing of the module's: the module's own task takes
//! its messages. A mailbox is named by its address, which the kernel looks
//! up on every call, so one that has closed is gone, never another's. The
//! kernel knows which module a mailbox is by an address in this module's
//! own code, handed over as it is made: its names are under that module's
//! namespace -- its name up to its first `-` -- and whatever it leaves open
//! at its unload is closed after it, and said.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ffi::ipc;

use crate::events::SharedEvent;
use crate::time::{boot_time_ns, Duration};

pub use ipc::{IPC_TAG_BOUNCED as TAG_BOUNCED, IPC_TAG_BULK as TAG_BULK, IPC_TAG_DOWN as TAG_DOWN,
              IPC_TAG_NAME_DOWN as TAG_NAME_DOWN, IPC_TAG_NAME_UP as TAG_NAME_UP, IPC_TAG_SYSTEM as TAG_SYSTEM,
              IPC_TAG_WATCH_LOST as TAG_WATCH_LOST};

/// Where a mailbox is: its host (0, this one), its slot and the slot's
/// generation, packed as the channel carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Addr(pub u64);

impl Addr {
    pub const NONE: Addr = Addr(0);

    pub fn host(self) -> u16 {
        (self.0 >> 48) as u16
    }

    pub fn slot(self) -> u16 {
        (self.0 >> 32) as u16
    }

    pub fn generation(self) -> u32 {
        self.0 as u32
    }

    pub fn is_none(self) -> bool {
        self.slot() == 0
    }
}

impl core::fmt::Display for Addr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.host() == 0 {
            write!(f, "{}.{}", self.slot(), self.generation())
        } else {
            write!(f, "host{}:{}.{}", self.host(), self.slot(), self.generation())
        }
    }
}

/// What the router's messages for other hosts are tagged: their data begins
/// with ipcwire's route head.
pub const TAG_ROUTE: u32 = ipc::IPC_TAG_ROUTE;

/// Why something was refused (ipc's `Error`, by its code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Full,
    Gone,
    NoRoute,
    TooLarge,
    NoMemory,
    Invalid,
    Taken,
    NotYours,
    Busy,
    Stopped,
    /// Refused, the kernel saying no more than that: a mailbox, a monitor or
    /// a watch it would not make.
    Refused,
}

impl Error {
    fn from_code(code: i32) -> Result<(), Error> {
        Err(match code {
            ipc::IPC_OK => return Ok(()),
            ipc::IPC_FULL => Error::Full,
            ipc::IPC_GONE => Error::Gone,
            ipc::IPC_NO_ROUTE => Error::NoRoute,
            ipc::IPC_TOO_LARGE => Error::TooLarge,
            ipc::IPC_NO_MEMORY => Error::NoMemory,
            ipc::IPC_INVALID => Error::Invalid,
            ipc::IPC_TAKEN => Error::Taken,
            ipc::IPC_NOT_YOURS => Error::NotYours,
            ipc::IPC_BUSY => Error::Busy,
            ipc::IPC_STOPPED => Error::Stopped,
            _ => Error::Refused,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Error::Full => "full",
            Error::Gone => "gone",
            Error::NoRoute => "no route",
            Error::TooLarge => "too large",
            Error::NoMemory => "no memory",
            Error::Invalid => "invalid",
            Error::Taken => "taken",
            Error::NotYours => "not yours",
            Error::Busy => "busy",
            Error::Stopped => "stopped",
            Error::Refused => "refused",
        }
    }
}

/// Why a Down came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Closed,
    Unloaded,
    NoSuch,
    HostLost,
    Stopped,
}

impl Reason {
    /// The channel's code for it (IPC_DOWN_*): what a Down's payload says,
    /// and ipcnet's Down frame.
    pub fn code(self) -> u32 {
        match self {
            Reason::Closed => ipc::IPC_DOWN_CLOSED,
            Reason::Unloaded => ipc::IPC_DOWN_UNLOADED,
            Reason::NoSuch => ipc::IPC_DOWN_NO_SUCH,
            Reason::HostLost => ipc::IPC_DOWN_HOST_LOST,
            Reason::Stopped => ipc::IPC_DOWN_STOPPED,
        }
    }

    pub fn from_code(code: u32) -> Option<Reason> {
        Some(match code {
            ipc::IPC_DOWN_CLOSED => Reason::Closed,
            ipc::IPC_DOWN_UNLOADED => Reason::Unloaded,
            ipc::IPC_DOWN_NO_SUCH => Reason::NoSuch,
            ipc::IPC_DOWN_HOST_LOST => Reason::HostLost,
            ipc::IPC_DOWN_STOPPED => Reason::Stopped,
            _ => return None,
        })
    }
}

/// Why a message for another host did not get there (a TAG_BOUNCED's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bounce {
    /// Its mailbox there was full.
    Full,
    /// Its mailbox there was gone, or no mailbox had its name.
    Gone,
    /// It never went: the way to that host was full, or the host was lost
    /// before it did.
    Unsent,
}

impl Bounce {
    /// The channel's code for it (IPC_BOUNCE_*): what a TAG_BOUNCED says,
    /// and ipcnet's Bounced frame.
    pub fn code(self) -> u32 {
        match self {
            Bounce::Full => ipc::IPC_BOUNCE_FULL,
            Bounce::Gone => ipc::IPC_BOUNCE_GONE,
            Bounce::Unsent => ipc::IPC_BOUNCE_UNSENT,
        }
    }

    pub fn from_code(code: u32) -> Option<Bounce> {
        Some(match code {
            ipc::IPC_BOUNCE_FULL => Bounce::Full,
            ipc::IPC_BOUNCE_GONE => Bounce::Gone,
            ipc::IPC_BOUNCE_UNSENT => Bounce::Unsent,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Bounce::Full => "full",
            Bounce::Gone => "gone",
            Bounce::Unsent => "unsent",
        }
    }
}

/// What a TAG_BOUNCED says: why, the message's tag, and the name it was
/// sent to when it went by name (`from` is then the peer's alone).
pub struct Bounced<'a> {
    pub why: Bounce,
    pub tag: u32,
    pub name: Option<&'a str>,
}

/// Where a message for a peer went there: the peer's own address of a
/// mailbox (host 0), or a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteTo<'a> {
    Addr(Addr),
    Name(&'a str),
}

/// Who may reach a name: this host's tasks and modules alone, or the
/// cluster's hosts too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Host,
    Cluster,
}

/// What a mailbox holds at most: messages, and their payloads' bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub messages: u32,
    pub bytes: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits { messages: 256, bytes: 4 << 20 };
}

/// A message received: who sent it, its tag and reference, its bytes.
pub struct Message {
    pub from: Addr,
    pub tag: u32,
    pub reference: u64,
    pub data: Vec<u8>,
}

impl Message {
    /// One of the channel's own: a Down, a name's coming or going, a watch
    /// lost.
    pub fn is_system(&self) -> bool {
        self.tag >= TAG_SYSTEM
    }

    /// A Down's reason; None for any other message.
    pub fn down(&self) -> Option<Reason> {
        if self.tag != TAG_DOWN || self.data.len() != 4 {
            return None;
        }
        Reason::from_code(u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]))
    }

    /// A bounce's reason, the bounced message's tag and the name it went
    /// to; None for any other message.
    pub fn bounced(&self) -> Option<Bounced<'_>> {
        if self.tag != TAG_BOUNCED || self.data.len() < 8 {
            return None;
        }
        let why = Bounce::from_code(u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]))?;
        let tag = u32::from_le_bytes([self.data[4], self.data[5], self.data[6], self.data[7]]);
        let name = match &self.data[8..] {
            [] => None,
            n => Some(core::str::from_utf8(n).ok()?),
        };
        Some(Bounced { why, tag, name })
    }

    /// A name's coming or going: the name.
    pub fn name(&self) -> Option<&str> {
        if self.tag != TAG_NAME_UP && self.tag != TAG_NAME_DOWN {
            return None;
        }
        core::str::from_utf8(&self.data).ok()
    }
}

/// A message received in pieces (`Mailbox::recv_pieces`): who sent it, its
/// tag and reference, its bytes -- `len` of them -- in pieces of at most
/// the size asked.
pub struct Pieces {
    pub from: Addr,
    pub tag: u32,
    pub reference: u64,
    pub len: usize,
    pub pieces: Vec<Vec<u8>>,
}

/// The largest piece `recv_pieces` is asked for: well under what one block
/// of the kernel's heap gives (512 KiB).
pub const PIECE_MAX: usize = 64 * 1024;

/// How a call ended, when no answer came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The request was refused.
    Send(Error),
    /// The mailbox called went before it answered.
    Down(Reason),
    /// Nothing came in time.
    Timeout,
}

/// An address in this module's own code: what the kernel knows the module
/// by when a mailbox is made.
fn here() -> usize {
    here as fn() -> usize as usize
}

/// A mailbox of this module's: closed when dropped.
pub struct Mailbox {
    addr: u64,
    references: AtomicU64,
}

impl Mailbox {
    /// A mailbox held to `limits`.
    pub fn new(limits: Limits) -> Result<Mailbox, Error> {
        Self::open(limits, 0)
    }

    /// A mailbox held to `limits`, made on `event` -- the event of a task
    /// that waits for this and more: whatever comes to it, a message, a
    /// Down, a wake, its close, signals that event, and a `recv` sleeps on
    /// it only until its first signal, which may have been for one of the
    /// task's other things. The kernel holds the event for as long as the
    /// mailbox is open, so `event` may be dropped first.
    pub fn new_on(limits: Limits, event: &SharedEvent) -> Result<Mailbox, Error> {
        Self::open(limits, event.handle())
    }

    fn open(limits: Limits, event: u64) -> Result<Mailbox, Error> {
        let addr = ipc::kernel_ipc_open(here(), limits.messages, limits.bytes as u64, event);
        if addr == 0 {
            return Err(Error::Refused);
        }
        Ok(Mailbox { addr, references: AtomicU64::new(1) })
    }

    pub fn addr(&self) -> Addr {
        Addr(self.addr)
    }

    /// The mailbox named `name` on this host, for `scope`: under this
    /// module's namespace, and refused for a name taken or a mailbox named
    /// already.
    pub fn register(&self, name: &str, scope: Scope) -> Result<(), Error> {
        let scope = match scope {
            Scope::Host => ipc::IPC_SCOPE_HOST,
            Scope::Cluster => ipc::IPC_SCOPE_CLUSTER,
        };
        /* `name` is its length's bytes */
        Error::from_code(unsafe { ipc::kernel_ipc_register(self.addr, name.as_ptr(), name.len(), scope) })
    }

    /// `data` to the mailbox at `to`: Ok once it is there.
    pub fn send(&self, to: Addr, tag: u32, reference: u64, data: &[u8]) -> Result<(), Error> {
        /* `data` is its length's bytes; no name is given */
        Error::from_code(unsafe {
            ipc::kernel_ipc_send(self.addr, to.0, core::ptr::null(), 0, tag, reference, data.as_ptr(), data.len())
        })
    }

    /// `data` to the mailbox `name` names on this host -- or, as
    /// `name@host`, on a peer.
    pub fn send_to(&self, name: &str, tag: u32, reference: u64, data: &[u8]) -> Result<(), Error> {
        if name.is_empty() {
            return Err(Error::Invalid);
        }
        /* `name` and `data` are their lengths' bytes */
        Error::from_code(unsafe {
            ipc::kernel_ipc_send(self.addr, 0, name.as_ptr(), name.len(), tag, reference, data.as_ptr(), data.len())
        })
    }

    /// The answer to `msg`: to its sender, with its reference.
    pub fn reply(&self, msg: &Message, tag: u32, data: &[u8]) -> Result<(), Error> {
        self.send(msg.from, tag, msg.reference, data)
    }

    /// The next message, waiting up to `timeout` for one: None when nothing
    /// came, the receiver was woken (`wake`), or the mailbox is closed -- and,
    /// made on a task's event (`new_on`), at the event's first signal that
    /// brought none. One task waits on a mailbox at a time.
    pub fn recv(&self, timeout: Duration) -> Option<Message> {
        let mut h = ipc::IpcHeader::default();
        /* Asked with no room first: a message with no payload comes whole,
         * any other says how long it is and stays first */
        let got = unsafe { ipc::kernel_ipc_recv(self.addr, timeout.as_nanos(), &mut h, core::ptr::null_mut(), 0) };
        if got == 0 {
            return Some(Message { from: Addr(h.from), tag: h.tag, reference: h.reference, data: Vec::new() });
        }
        if got != ipc::IPC_RECV_TOO_SMALL {
            return None;
        }
        let len = usize::try_from(h.len).ok()?;
        let mut data: Vec<u8> = Vec::new();
        data.try_reserve_exact(len).ok()?;
        /* The message is still first, and this task its one receiver: taken
         * now into room for all of it, which `data` has for `len` bytes */
        let got = unsafe { ipc::kernel_ipc_recv(self.addr, 0, &mut h, data.as_mut_ptr(), len) };
        let Ok(n) = usize::try_from(got) else { return None };
        if n > len {
            return None;
        }
        /* The kernel wrote `n` bytes, no more than `len`, which `data` holds */
        unsafe { data.set_len(n) };
        Some(Message { from: Addr(h.from), tag: h.tag, reference: h.reference, data })
    }

    /// The next message, waiting up to `timeout` for one, in pieces of at
    /// most `piece` bytes (1 to PIECE_MAX): what a message is received as
    /// that may be larger than one block of the heap gives -- `recv`'s one
    /// Vec cannot be, and leaves such a message first. None when nothing
    /// came, or the mailbox is closed; and, the message left first to be
    /// received again whole, when the heap had no room for a piece of it.
    pub fn recv_pieces(&self, timeout: Duration, piece: usize) -> Option<Pieces> {
        let piece = piece.clamp(1, PIECE_MAX);
        let mut h = ipc::IpcHeader::default();
        let mut ticket = 0u64;
        /* Asked with no room first: its length, and a message of none taken
         * whole */
        let got = unsafe {
            ipc::kernel_ipc_recv_part(self.addr, timeout.as_nanos(), &mut h, core::ptr::null_mut(), 0, 0,
                                      &mut ticket)
        };
        if got < 0 {
            return None;
        }
        let len = usize::try_from(h.len).ok()?;
        let mut pieces: Vec<Vec<u8>> = Vec::new();
        pieces.try_reserve_exact(len.div_ceil(piece)).ok()?;
        let mut offset = 0usize;
        while offset < len {
            let want = piece.min(len - offset);
            let mut p: Vec<u8> = Vec::new();
            p.try_reserve_exact(want).ok()?;
            /* `p` has room for `want` bytes; the ticket is the first part's */
            let n = unsafe {
                ipc::kernel_ipc_recv_part(self.addr, 0, &mut h, p.as_mut_ptr(), want, offset as u64, &mut ticket)
            };
            let n = usize::try_from(n).ok().filter(|&n| n > 0 && n <= want)?;
            /* The kernel wrote `n` bytes, no more than `want` */
            unsafe { p.set_len(n) };
            pieces.push(p);
            offset += n;
        }
        Some(Pieces { from: Addr(h.from), tag: h.tag, reference: h.reference, len, pieces })
    }

    /// The next message, if there is one.
    pub fn try_recv(&self) -> Option<Message> {
        self.recv(Duration::zero())
    }

    /// The task waiting in `recv` woken, with nothing.
    pub fn wake(&self) {
        ipc::kernel_ipc_wake(self.addr);
    }

    /// This mailbox told once, with a Down, when `target` goes: the
    /// monitor's id.
    pub fn monitor(&self, target: Addr) -> Result<u64, Error> {
        match ipc::kernel_ipc_monitor(self.addr, target.0) {
            0 => Err(Error::Refused),
            id => Ok(id),
        }
    }

    pub fn demonitor(&self, id: u64) {
        ipc::kernel_ipc_demonitor(self.addr, id);
    }

    /// This mailbox told of every name under `prefix` that comes and goes:
    /// the watch's id. What is there already, `list` says.
    pub fn watch(&self, prefix: &str) -> Result<u64, Error> {
        /* `prefix` is its length's bytes */
        match unsafe { ipc::kernel_ipc_watch(self.addr, prefix.as_ptr(), prefix.len()) } {
            0 => Err(Error::Refused),
            id => Ok(id),
        }
    }

    pub fn unwatch(&self, id: u64) {
        ipc::kernel_ipc_unwatch(self.addr, id);
    }

    /// A reference for a request of this mailbox's, no other of its own.
    pub fn next_reference(&self) -> u64 {
        self.references.fetch_add(1, Ordering::Relaxed)
    }

    /* ---- the router's: what carries messages to other hosts (ipcnet) ---- */

    /// This mailbox made the router, one at a time, until it closes: what
    /// is sent to a peer's address, or to `name@host`, comes here tagged
    /// `TAG_ROUTE`, its data ipcwire's route head and then the message's own.
    pub fn route(&self) -> Result<(), Error> {
        Error::from_code(ipc::kernel_ipc_route(self.addr))
    }

    /// The peer `name` up: its number, the host part of its mailboxes'
    /// addresses here -- the same while it stays up, another once lost.
    pub fn peer_up(&self, name: &str) -> Result<u16, Error> {
        if name.is_empty() {
            return Err(Error::Invalid);
        }
        /* `name` is its length's bytes */
        let n = unsafe { ipc::kernel_ipc_peer_up(self.addr, name.as_ptr(), name.len()) };
        u16::try_from(n).ok().filter(|&n| n != 0).ok_or(Error::Refused)
    }

    /// The peer `peer` lost: every monitor of an address there gets its
    /// Down, HostLost.
    pub fn peer_lost(&self, peer: u16) -> Result<(), Error> {
        Error::from_code(ipc::kernel_ipc_peer_lost(self.addr, u32::from(peer)))
    }

    /// What the peer `peer` said -- from its mailbox `from`, the peer's own
    /// address of it -- delivered here: to `to` (an address here, host 0),
    /// or to the `Cluster` name `name` when one is given, its payload the
    /// pieces `data` in order. `NotYours` for anything a peer may not reach.
    pub fn deliver_remote(&self, peer: u16, from: u64, to: Addr, name: Option<&str>, tag: u32, reference: u64,
                          data: &[&[u8]]) -> Result<(), Error> {
        let (np, nl) = match name {
            Some("") => return Err(Error::Invalid),
            Some(n) => (n.as_ptr(), n.len()),
            None => (core::ptr::null(), 0),
        };
        if data.len() > ipc::IPC_PIECES_MAX {
            return Err(Error::TooLarge);
        }
        let mut pieces: Vec<ipc::IpcPiece> = Vec::new();
        pieces.try_reserve_exact(data.len()).map_err(|_| Error::NoMemory)?;
        pieces.extend(data.iter().map(|d| ipc::IpcPiece { data: d.as_ptr(), len: d.len() }));
        /* The name is its length's bytes, and each piece names its own */
        Error::from_code(unsafe {
            ipc::kernel_ipc_deliver_remote(self.addr, u32::from(peer), from, to.0, np, nl, tag, reference,
                                           pieces.as_ptr(), pieces.len())
        })
    }

    /// The peer `peer` says the mailbox the monitor `id` watched has gone.
    pub fn remote_down(&self, peer: u16, id: u64, reason: Reason) -> Result<(), Error> {
        Error::from_code(ipc::kernel_ipc_remote_down(self.addr, u32::from(peer), id, reason.code()))
    }

    /// The router's: a message from `sender` -- a mailbox here that has
    /// sent to the peer `peer` -- to `target` there did not get there, for
    /// `why`: told to the sender, TAG_BOUNCED. Dropped and counted when the
    /// sender has no room.
    pub fn bounced(&self, peer: u16, sender: Addr, target: RemoteTo<'_>, tag: u32, reference: u64, why: Bounce)
        -> Result<(), Error>
    {
        let (to, np, nl) = match target {
            RemoteTo::Addr(a) => (a.0, core::ptr::null(), 0),
            RemoteTo::Name("") => return Err(Error::Invalid),
            RemoteTo::Name(n) => (0, n.as_ptr(), n.len()),
        };
        /* The name is its length's bytes */
        Error::from_code(unsafe {
            ipc::kernel_ipc_bounced(self.addr, u32::from(peer), sender.0, to, np, nl, tag, reference, why.code())
        })
    }

    /// A request to `to` and its answer, from this mailbox, which takes
    /// nothing else meanwhile -- what else comes is dropped.
    pub fn call(&self, to: Addr, tag: u32, data: &[u8], timeout: Duration) -> Result<Message, CallError> {
        let reference = self.next_reference();
        let monitor = self.monitor(to).map_err(CallError::Send)?;
        if let Err(e) = self.send(to, tag, reference, data) {
            self.demonitor(monitor);
            return Err(CallError::Send(e));
        }
        let deadline = boot_time_ns().saturating_add(timeout.as_nanos());
        loop {
            let now = boot_time_ns();
            if now >= deadline {
                self.demonitor(monitor);
                return Err(CallError::Timeout);
            }
            let Some(msg) = self.recv(Duration::from_nanos(deadline - now)) else { continue };
            if !msg.is_system() && msg.reference == reference && msg.from == to {
                self.demonitor(monitor);
                return Ok(msg);
            }
            if msg.tag == TAG_DOWN && msg.reference == monitor {
                return Err(CallError::Down(msg.down().unwrap_or(Reason::Closed)));
            }
        }
    }
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        ipc::kernel_ipc_close(self.addr);
    }
}

/// A host's name at most: a DNS label's.
const HOST_NAME_MAX: usize = 63;

/// The host a message from another host came from: the name of the peer
/// whose number `addr`'s host part is, while it is up -- None for this
/// host's own addresses, and for a number no peer has now.
pub fn peer_name(addr: Addr) -> Option<String> {
    let host = addr.host();
    if host == 0 {
        return None;
    }
    let mut buf = [0u8; HOST_NAME_MAX];
    /* `buf` takes its length's bytes */
    let n = unsafe { ipc::kernel_ipc_peer_name(u32::from(host), buf.as_mut_ptr(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    core::str::from_utf8(&buf[..n]).ok().map(String::from)
}

/// The address `name` names on this host.
pub fn lookup(name: &str) -> Option<Addr> {
    /* `name` is its length's bytes */
    match unsafe { ipc::kernel_ipc_lookup(name.as_ptr(), name.len()) } {
        0 => None,
        a => Some(Addr(a)),
    }
}

/// A name, as `list` copies it out.
pub struct Name {
    pub name: String,
    pub addr: Addr,
    pub scope: Scope,
}

/// The most names `list` asks for at once.
const LIST_MAX: usize = 256;

/// The names under `prefix` on this host.
pub fn list(prefix: &str) -> Vec<Name> {
    let mut out = Vec::new();
    let mut raw: Vec<ipc::IpcName> = Vec::new();
    if raw.try_reserve_exact(LIST_MAX).is_err() {
        return out;
    }
    /* `prefix` is its length's bytes; `raw` has room for LIST_MAX entries */
    let n = unsafe { ipc::kernel_ipc_list(prefix.as_ptr(), prefix.len(), raw.as_mut_ptr(), LIST_MAX) };
    /* The kernel wrote `n` entries, no more than LIST_MAX */
    unsafe { raw.set_len(n.min(LIST_MAX)) };
    if out.try_reserve_exact(raw.len()).is_err() {
        return out;
    }
    for r in &raw {
        let len = (r.len as usize).min(ipc::IPC_NAME_MAX);
        let Ok(text) = core::str::from_utf8(&r.name[..len]) else { continue };
        let mut name = String::new();
        if name.try_reserve_exact(len).is_err() {
            break;
        }
        name.push_str(text);
        let scope = if r.scope == ipc::IPC_SCOPE_CLUSTER { Scope::Cluster } else { Scope::Host };
        out.push(Name { name, addr: Addr(r.addr), scope });
    }
    out
}
