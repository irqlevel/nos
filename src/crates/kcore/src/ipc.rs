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

use crate::time::{boot_time_ns, Duration};

pub use ipc::{IPC_TAG_DOWN as TAG_DOWN, IPC_TAG_NAME_DOWN as TAG_NAME_DOWN, IPC_TAG_NAME_UP as TAG_NAME_UP,
              IPC_TAG_SYSTEM as TAG_SYSTEM, IPC_TAG_WATCH_LOST as TAG_WATCH_LOST};

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
        match u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]) {
            ipc::IPC_DOWN_CLOSED => Some(Reason::Closed),
            ipc::IPC_DOWN_UNLOADED => Some(Reason::Unloaded),
            ipc::IPC_DOWN_NO_SUCH => Some(Reason::NoSuch),
            ipc::IPC_DOWN_HOST_LOST => Some(Reason::HostLost),
            ipc::IPC_DOWN_STOPPED => Some(Reason::Stopped),
            _ => None,
        }
    }

    /// A name's coming or going: the name.
    pub fn name(&self) -> Option<&str> {
        if self.tag != TAG_NAME_UP && self.tag != TAG_NAME_DOWN {
            return None;
        }
        core::str::from_utf8(&self.data).ok()
    }
}

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
        let addr = ipc::kernel_ipc_open(here(), limits.messages, limits.bytes as u64);
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

    /// `data` to the mailbox `name` names on this host.
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
    /// came, the receiver was woken (`wake`), or the mailbox is closed. One
    /// task receives from a mailbox at a time.
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
