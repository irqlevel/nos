//! The kernel's message channel: mailboxes, the messages between them, and
//! names for them in a namespace public to the host (plans/ipc.md,
//! docs/ipc.md). Modules cannot call one another -- each is linked on its
//! own -- and the image's tasks and the modules alike reach this, so it is
//! where services meet: the image's Rust through the crate, a module
//! through the C ABI (`abi`, declared in ffi/src/ipc.rs, wrapped in
//! kcore::ipc).
//!
//! A **mailbox** is a bounded queue with one owner, who receives from it.
//! Its address is its slot in the table and the slot's generation, looked
//! up -- never followed -- so the address of a mailbox that has closed is
//! `Gone`, never another's. A **name** is a mailbox's, unique on the host,
//! and goes when the mailbox does. A **message** is who sent it, a tag the
//! protocol defines, a reference that says which request a reply answers,
//! and a payload of up to `MAX_PAYLOAD` bytes in pieces of a page, so that
//! no allocation of the channel's is larger than a page. A **monitor** says
//! once, with a `Down`, when a mailbox it watches is gone; a **watch** says
//! when names under a prefix come and go.
//!
//! What keeps it sound:
//!
//! - **Pull, never push.** Nothing here calls anybody's code: a receiver's
//!   own task takes its messages. No message can land in a module that has
//!   gone, and unloading one is its mailboxes' drops.
//! - **Everything bounded**: a mailbox's messages and bytes, by its owner's
//!   limits; what a module's mailboxes may hold in all; what the host's
//!   queues hold at once; the names, the monitors, the watches. Full is an
//!   answer, not a wait.
//! - **A Down is never lost.** A monitor allocates its Down when it is made
//!   and keeps room for it in its watcher's queue, so a mailbox closing
//!   allocates nothing and finds room for every Down it owes.
//! - **Locks**: the table's, then a mailbox's, never the other way round and
//!   never two mailboxes' at once; both are mutexes, task context only. What
//!   a closing mailbox held is dropped after every lock is let go of.
//! - **The event a receiver sleeps on is held**: the mailbox's own, or the
//!   one of the task it was made on (`Mailbox::new_on`, plans/ipc.md I2), an
//!   `Arc` either way -- nothing signals an event that has been freed,
//!   whoever let go of it first -- and held by one task at a time while it
//!   waits (`SharedEvent::hold`).

#![no_std]

extern crate alloc;

mod abi;
mod echo;
mod remote;
mod selftest;
mod shell;

pub use remote::{peers, router, valid_host, HOST_PREFIX, MAX_PEERS, MAX_REMOTE_MONITORS, MAX_SENT};

pub use echo::{echo_start, echo_status, echo_stop, EchoStatus, TAG_ECHO_PESTER};
pub use selftest::selftest;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use events::SharedEvent;
use kcore::klog;
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::time::{boot_time_ns, Duration};

/// Mailboxes at once on this host: a slot each, numbered from 1.
pub const MAX_MAILBOXES: usize = 1024;
/// Names at once on this host.
pub const MAX_NAMES: usize = 256;
/// A name's bytes, at most.
pub const NAME_MAX: usize = 48;
/// A message's payload, at most: a guest's largest disk request is 256 KiB
/// (`blk::REQUEST_BYTES`), with room to spare.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// A payload's piece: no allocation of a payload is larger than this.
pub const PIECE: usize = 4096;
/// What a mailbox's limits may be, at most.
pub const MAX_QUEUE_MESSAGES: u32 = 4096;
pub const MAX_QUEUE_BYTES: usize = 16 << 20;
/// The limits of a mailbox that asks for none in particular.
pub const DEFAULT_LIMITS: Limits = Limits { messages: 256, bytes: 4 << 20 };
/// What one module's mailboxes may hold, their limits' bytes summed.
pub const OWNER_MAX_BYTES: usize = 64 << 20;
/// What every queue of the host holds at once, at most.
pub const POOL_MAX_BYTES: usize = 128 << 20;
/// Monitors a mailbox holds at once.
pub const MAX_MONITORS: usize = 512;
/// Watches a mailbox holds at once, and the host in all.
pub const MAX_WATCHES_EACH: usize = 16;
pub const MAX_WATCHES: usize = 256;
/// A module's name, as the module table keeps it: kmod's NAME_LEN, less the
/// NUL.
pub const OWNER_NAME_MAX: usize = 31;

/// Tags at and above this one are the channel's own: a sender is refused
/// them.
pub const TAG_SYSTEM: u32 = ffi::ipc::IPC_TAG_SYSTEM;
/// A mailbox monitored is gone. `from` is its address, the reference the
/// monitor's id, the payload the reason (`Reason`, 4 bytes, little-endian).
pub const TAG_DOWN: u32 = ffi::ipc::IPC_TAG_DOWN;
/// A name under a watch's prefix registered: `from` its mailbox, the
/// reference the watch's id, the payload the name.
pub const TAG_NAME_UP: u32 = ffi::ipc::IPC_TAG_NAME_UP;
/// A name under a watch's prefix gone: as `TAG_NAME_UP`.
pub const TAG_NAME_DOWN: u32 = ffi::ipc::IPC_TAG_NAME_DOWN;
/// A watch that could not deliver an event -- its watcher's queue full --
/// has ended: watch again, and `list` what there is. The reference is the
/// watch's id.
pub const TAG_WATCH_LOST: u32 = ffi::ipc::IPC_TAG_WATCH_LOST;
/// A message for another host, in the router's mailbox: its payload begins
/// with ipcwire's route head.
pub const TAG_ROUTE: u32 = ffi::ipc::IPC_TAG_ROUTE;
/// A message for another host that did not get there, told back to its
/// sender: `from` the mailbox it was for as this host knows it (or the
/// peer's alone, slot 0, for a name there), the reference the message's,
/// the payload why (`Bounce`, 4 bytes), its tag (4) and, for a name, the
/// name -- little-endian.
pub const TAG_BOUNCED: u32 = ffi::ipc::IPC_TAG_BOUNCED;
/// A tag with this bit goes between hosts by the bulk connection, any
/// other by the control one: order between two mailboxes holds within a
/// class, not across the two.
pub const TAG_BULK: u32 = ffi::ipc::IPC_TAG_BULK;
/// A bounce's payload before the name: why, and the message's tag.
pub const BOUNCE_BYTES: usize = 8;

/// Why a message for another host did not get there.
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
    pub fn code(self) -> u32 {
        match self {
            Bounce::Full => ffi::ipc::IPC_BOUNCE_FULL,
            Bounce::Gone => ffi::ipc::IPC_BOUNCE_GONE,
            Bounce::Unsent => ffi::ipc::IPC_BOUNCE_UNSENT,
        }
    }

    pub fn from_u32(code: u32) -> Option<Bounce> {
        Some(match code {
            ffi::ipc::IPC_BOUNCE_FULL => Bounce::Full,
            ffi::ipc::IPC_BOUNCE_GONE => Bounce::Gone,
            ffi::ipc::IPC_BOUNCE_UNSENT => Bounce::Unsent,
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

/// The namespaces only the image registers names under.
const RESERVED: [&str; 4] = ["kernel", "ipc", "sys", "host"];
/// A Down's payload: the reason, as a little-endian u32.
const DOWN_BYTES: usize = 4;
/// How often a wait for a message looks for a receiver gone wrong.
const LOOK_NS: u64 = kcore::consts::NS_PER_SEC;

/// Where a mailbox is: its host (0, this one), its slot in the table, and
/// the slot's generation when it was made. A slot of 0 is no mailbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Addr {
    pub host: u16,
    pub slot: u16,
    pub generation: u32,
}

impl Addr {
    pub const NONE: Addr = Addr { host: 0, slot: 0, generation: 0 };

    /// The address as the C ABI carries it: host, slot, generation, from the
    /// top down.
    pub const fn to_u64(self) -> u64 {
        ((self.host as u64) << 48) | ((self.slot as u64) << 32) | self.generation as u64
    }

    pub const fn from_u64(v: u64) -> Addr {
        Addr { host: (v >> 48) as u16, slot: (v >> 32) as u16, generation: v as u32 }
    }

    pub fn is_none(self) -> bool {
        self.slot == 0
    }
}

impl core::fmt::Display for Addr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.host == 0 {
            write!(f, "{}.{}", self.slot, self.generation)
        } else {
            write!(f, "host{}:{}.{}", self.host, self.slot, self.generation)
        }
    }
}

/// Who may reach a name: this host's tasks and modules alone, or the
/// cluster's hosts too (once ipcnet carries messages between them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Host,
    Cluster,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Host => "host",
            Scope::Cluster => "cluster",
        }
    }
}

/// What a mailbox holds at most: messages, and their payloads' bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub messages: u32,
    pub bytes: usize,
}

/// Why something was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The mailbox is at its limit: try again after it has taken some.
    Full,
    /// No such mailbox, or no longer.
    Gone,
    /// Another host's, and nothing carries messages there.
    NoRoute,
    /// Past `MAX_PAYLOAD`, or past what the mailbox holds at all.
    TooLarge,
    /// No memory, or the host's queues at `POOL_MAX_BYTES`.
    NoMemory,
    /// Not a name, a prefix, a tag a sender may use, or limits.
    Invalid,
    /// The name is another mailbox's, or the mailbox has a name already.
    Taken,
    /// Under a namespace that is not the owner's.
    NotYours,
    /// Every slot, name, monitor or watch there is, taken.
    Busy,
    /// The halt has begun.
    Stopped,
}

impl Error {
    /// The code the C ABI says it by (ffi::ipc's IPC_*): done is IPC_OK.
    pub fn code(self) -> i32 {
        use ffi::ipc::*;
        match self {
            Error::Full => IPC_FULL,
            Error::Gone => IPC_GONE,
            Error::NoRoute => IPC_NO_ROUTE,
            Error::TooLarge => IPC_TOO_LARGE,
            Error::NoMemory => IPC_NO_MEMORY,
            Error::Invalid => IPC_INVALID,
            Error::Taken => IPC_TAKEN,
            Error::NotYours => IPC_NOT_YOURS,
            Error::Busy => IPC_BUSY,
            Error::Stopped => IPC_STOPPED,
        }
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
        }
    }
}

/// Why a Down came: its payload, as ffi::ipc's IPC_DOWN_* say it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Reason {
    /// Its owner closed it.
    Closed = ffi::ipc::IPC_DOWN_CLOSED,
    /// Its module was unloaded with it still open.
    Unloaded = ffi::ipc::IPC_DOWN_UNLOADED,
    /// It was gone already when the monitor was made.
    NoSuch = ffi::ipc::IPC_DOWN_NO_SUCH,
    /// Its host stopped answering.
    HostLost = ffi::ipc::IPC_DOWN_HOST_LOST,
    /// The halt.
    Stopped = ffi::ipc::IPC_DOWN_STOPPED,
}

impl Reason {
    pub fn from_u32(v: u32) -> Option<Reason> {
        use ffi::ipc::*;
        match v {
            IPC_DOWN_CLOSED => Some(Reason::Closed),
            IPC_DOWN_UNLOADED => Some(Reason::Unloaded),
            IPC_DOWN_NO_SUCH => Some(Reason::NoSuch),
            IPC_DOWN_HOST_LOST => Some(Reason::HostLost),
            IPC_DOWN_STOPPED => Some(Reason::Stopped),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Reason::Closed => "closed",
            Reason::Unloaded => "unloaded",
            Reason::NoSuch => "no such mailbox",
            Reason::HostLost => "host lost",
            Reason::Stopped => "stopped",
        }
    }
}

/// Whose a mailbox is: the image's, or a module's, by the module's name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owner {
    len: u8,
    name: [u8; OWNER_NAME_MAX],
}

impl Owner {
    pub(crate) const IMAGE: Owner = Owner { len: 0, name: [0; OWNER_NAME_MAX] };

    /// A module's, by its name: None for one that is no module's name.
    pub(crate) fn module(name: &[u8]) -> Option<Owner> {
        if name.is_empty() || name.len() > OWNER_NAME_MAX || core::str::from_utf8(name).is_err() {
            return None;
        }
        let mut o = Owner { len: name.len() as u8, name: [0; OWNER_NAME_MAX] };
        o.name[..name.len()].copy_from_slice(name);
        Some(o)
    }

    pub(crate) fn is_image(&self) -> bool {
        self.len == 0
    }

    /// The module's name, or "kernel" for the image.
    pub(crate) fn as_str(&self) -> &str {
        if self.is_image() {
            return "kernel";
        }
        /* Only a UTF-8 name is ever put in */
        core::str::from_utf8(&self.name[..usize::from(self.len)]).unwrap_or("?")
    }

    /// The namespace a module's names are under: its name up to its first
    /// `-` -- nbs-mds's and nbs-cs's both `nbs`.
    fn namespace(&self) -> &str {
        let name = self.as_str();
        name.split('-').next().unwrap_or(name)
    }
}

/// A message's bytes, in pieces of `PIECE` -- the last one shorter.
pub struct Payload {
    len: usize,
    pieces: Vec<Box<[u8]>>,
}

impl Payload {
    pub const fn empty() -> Payload {
        Payload { len: 0, pieces: Vec::new() }
    }

    /// `data`, copied.
    pub fn from_slice(data: &[u8]) -> Result<Payload, Error> {
        Self::gather(&[data])
    }

    /// The parts, one after the other, copied: what a message is built of
    /// without putting it together first.
    pub fn gather(parts: &[&[u8]]) -> Result<Payload, Error> {
        Self::gather_within(parts, MAX_PAYLOAD)
    }

    /// `gather`, held to `limit` bytes: a message routed to another host is
    /// its route head and a payload of up to `MAX_PAYLOAD`.
    pub(crate) fn gather_within(parts: &[&[u8]], limit: usize) -> Result<Payload, Error> {
        let mut len = 0usize;
        for p in parts {
            len = len.checked_add(p.len()).ok_or(Error::TooLarge)?;
        }
        if len > limit {
            return Err(Error::TooLarge);
        }
        let mut pieces: Vec<Box<[u8]>> = Vec::new();
        pieces.try_reserve_exact(len.div_ceil(PIECE)).map_err(|_| Error::NoMemory)?;
        let mut piece: Vec<u8> = Vec::new();
        let mut want = 0usize;
        let mut done = 0usize;
        for part in parts {
            let mut rest: &[u8] = part;
            while !rest.is_empty() {
                if want == 0 {
                    want = PIECE.min(len - done);
                    piece.try_reserve_exact(want).map_err(|_| Error::NoMemory)?;
                }
                let take = (want - piece.len()).min(rest.len());
                piece.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                done += take;
                if piece.len() == want {
                    pieces.push(core::mem::take(&mut piece).into_boxed_slice());
                    want = 0;
                }
            }
        }
        Ok(Payload { len, pieces })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The pieces, in order.
    pub fn pieces(&self) -> impl Iterator<Item = &[u8]> {
        self.pieces.iter().map(|p| &p[..])
    }

    /// What is there from `offset` on, into `out`: how many bytes.
    pub fn read(&self, offset: usize, out: &mut [u8]) -> usize {
        let mut skip = offset;
        let mut put = 0;
        for p in self.pieces() {
            if put == out.len() {
                break;
            }
            if skip >= p.len() {
                skip -= p.len();
                continue;
            }
            let from = &p[skip..];
            skip = 0;
            let take = from.len().min(out.len() - put);
            out[put..put + take].copy_from_slice(&from[..take]);
            put += take;
        }
        put
    }

    /// Whether it holds exactly `data`.
    pub fn equals(&self, data: &[u8]) -> bool {
        if data.len() != self.len {
            return false;
        }
        let mut at = 0;
        for p in self.pieces() {
            if data[at..at + p.len()] != *p {
                return false;
            }
            at += p.len();
        }
        true
    }

    /// The bytes, put together: for a payload its taker wants whole.
    pub fn to_vec(&self) -> Result<Vec<u8>, Error> {
        let mut v = Vec::new();
        v.try_reserve_exact(self.len).map_err(|_| Error::NoMemory)?;
        for p in self.pieces() {
            v.extend_from_slice(p);
        }
        Ok(v)
    }

    /// The first piece written over from its start: a Down's reason.
    fn overwrite(&mut self, data: &[u8]) {
        if let Some(p) = self.pieces.first_mut() {
            let n = data.len().min(p.len());
            p[..n].copy_from_slice(&data[..n]);
        }
    }
}

/// A message, as its receiver takes it.
pub struct Message {
    from: Addr,
    tag: u32,
    reference: u64,
    payload: Payload,
}

impl Message {
    pub fn from(&self) -> Addr {
        self.from
    }

    pub fn tag(&self) -> u32 {
        self.tag
    }

    pub fn reference(&self) -> u64 {
        self.reference
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    pub fn into_payload(self) -> Payload {
        self.payload
    }

    /// One of the channel's own: a Down, a name's coming or going, a watch
    /// lost.
    pub fn is_system(&self) -> bool {
        self.tag >= TAG_SYSTEM
    }

    /// A Down's reason; None for any other message.
    pub fn down(&self) -> Option<Reason> {
        if self.tag != TAG_DOWN {
            return None;
        }
        let mut b = [0u8; DOWN_BYTES];
        (self.payload.read(0, &mut b) == DOWN_BYTES).then(|| Reason::from_u32(u32::from_le_bytes(b))).flatten()
    }

    /// Whether it takes a place kept for it in its queue rather than one of
    /// the queue's own: a Down, and the end of a watch.
    fn kept_room(&self) -> bool {
        self.tag == TAG_DOWN || self.tag == TAG_WATCH_LOST
    }
}

/// The counters `ipc` prints, the host's.
struct Stats {
    sent: AtomicU64,
    received: AtomicU64,
    full: AtomicU64,
    gone: AtomicU64,
    no_memory: AtomicU64,
    downs: AtomicU64,
    watch_lost: AtomicU64,
    stray: AtomicU64,
    /// Messages handed to the router for other hosts
    routed: AtomicU64,
    /// From other hosts, delivered here
    delivered_remote: AtomicU64,
    /// From other hosts, refused: not a Cluster name, nor a mailbox that
    /// had sent there
    refused_remote: AtomicU64,
    peers_up: AtomicU64,
    peers_lost: AtomicU64,
    /// Messages for other hosts told back to their senders, and those
    /// bounces dropped for want of room
    bounced: AtomicU64,
    bounces_dropped: AtomicU64,
}

static STATS: Stats = Stats {
    sent: AtomicU64::new(0),
    received: AtomicU64::new(0),
    full: AtomicU64::new(0),
    gone: AtomicU64::new(0),
    no_memory: AtomicU64::new(0),
    downs: AtomicU64::new(0),
    watch_lost: AtomicU64::new(0),
    stray: AtomicU64::new(0),
    routed: AtomicU64::new(0),
    delivered_remote: AtomicU64::new(0),
    refused_remote: AtomicU64::new(0),
    peers_up: AtomicU64::new(0),
    peers_lost: AtomicU64::new(0),
    bounced: AtomicU64::new(0),
    bounces_dropped: AtomicU64::new(0),
};

fn count(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// The ids of monitors and watches, and the references calls are made with.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// The payload bytes in every queue of the host.
static QUEUED: AtomicUsize = AtomicUsize::new(0);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A monitor, kept at the mailbox it watches: whose it is, and its Down,
/// made when the monitor was.
struct MonitorRec {
    id: u64,
    watcher: Arc<Mbox>,
    down: Message,
}

struct Inner {
    queue: VecDeque<Message>,
    /// Messages queued in the queue's own places, and their payloads' bytes.
    normal: u32,
    bytes: usize,
    /// Places kept for messages promised -- a Down for each monitor this
    /// mailbox holds, the end of each watch -- and those of them queued now.
    reserved: usize,
    kept_queued: usize,
    closed: bool,
    /// Who watches this mailbox: their Downs, to be delivered when it goes.
    monitored_by: Vec<MonitorRec>,
    /// Whom this mailbox watches, and by which monitor.
    monitors: Vec<(Addr, u64)>,
    /// Its watches' ids.
    watches: Vec<u64>,
    received: u64,
    refused: u64,
    peak: u32,
}

impl Inner {
    /// The places the queue must have: its own, and those kept.
    fn places(&self, limits: &Limits) -> usize {
        limits.messages as usize + self.reserved + self.kept_queued
    }

    /// One more place kept, the queue grown to hold it.
    fn keep_one(&mut self, limits: &Limits) -> Result<(), Error> {
        let need = self.places(limits) + 1;
        if self.queue.capacity() < need {
            self.queue.try_reserve(need - self.queue.len()).map_err(|_| Error::NoMemory)?;
        }
        self.reserved += 1;
        Ok(())
    }
}

/// A mailbox, as the table and its users share it.
pub(crate) struct Mbox {
    addr: Addr,
    owner: Owner,
    limits: Limits,
    inner: Mutex<Inner>,
    /// What its receiver sleeps on, and whatever comes to it signals: its
    /// own, or the event of the task it was made on (`Mailbox::new_on`) --
    /// held either way, so that nothing signals an event that has been
    /// freed, whoever let go of it first. A receiver holds it while it
    /// waits (`SharedEvent::hold`): a second task waiting is a bug, said and
    /// refused.
    event: Arc<SharedEvent>,
    /// Made on a task's event: a receive's wait ends at the event's first
    /// signal, whatever it was for -- the task has other things to look at.
    on_task_event: bool,
}

struct Entry {
    generation: u32,
    mbox: Option<Arc<Mbox>>,
}

struct NameRec {
    name: String,
    addr: Addr,
    scope: Scope,
}

struct WatchRec {
    id: u64,
    prefix: String,
    watcher: Arc<Mbox>,
    /// The end of the watch, made with it; taken when delivered.
    lost: Option<Message>,
}

struct Table {
    /// Slot 0 is no mailbox: `Addr::NONE`'s.
    slots: Vec<Entry>,
    names: Vec<NameRec>,
    watches: Vec<WatchRec>,
    /// Each module's mailboxes' bytes, by their limits.
    owners: Vec<(Owner, usize)>,
    /// What carries messages to other hosts -- ipcnet's mailbox -- or none.
    router: Option<Arc<Mbox>>,
    /// The peers the router has told of, by their numbers' index.
    peers: Vec<Option<remote::Peer>>,
    /// Monitors of mailboxes on peers, kept here with their Downs.
    remote_monitors: Vec<remote::RemoteMonitor>,
    stopped: bool,
}

static TABLE: OnceBox<Mutex<Table>> = OnceBox::new();

fn table() -> Option<&'static Mutex<Table>> {
    TABLE.get_or_try_init(|| {
        let mut slots = Vec::new();
        slots.try_reserve_exact(MAX_MAILBOXES + 1).ok()?;
        slots.resize_with(MAX_MAILBOXES + 1, || Entry { generation: 1, mbox: None });
        let mut names = Vec::new();
        names.try_reserve_exact(MAX_NAMES).ok()?;
        let mut watches = Vec::new();
        watches.try_reserve_exact(MAX_WATCHES).ok()?;
        let mut peers = Vec::new();
        peers.try_reserve_exact(remote::MAX_PEERS).ok()?;
        peers.resize_with(remote::MAX_PEERS, || None);
        let table = Mutex::new(Table { slots, names, watches, owners: Vec::new(), router: None, peers,
                                       remote_monitors: Vec::new(), stopped: false })?;
        Some(Box::new(table))
    })
}

/// The channel made, and its command put in front of whoever runs one:
/// from rust_init. The table itself is made by whatever needs it first.
pub fn init() {
    if table().is_none() {
        klog!(0, "ipc: no memory for the table of mailboxes");
        return;
    }
    shell::init();
}

fn copy(s: &str) -> Result<String, Error> {
    let mut c = String::new();
    c.try_reserve_exact(s.len()).map_err(|_| Error::NoMemory)?;
    c.push_str(s);
    Ok(c)
}

/// Whether `part` is one part of a name: a lowercase letter, then lowercase
/// letters, digits, `_` and `-`.
fn valid_part(part: &str) -> bool {
    let mut bytes = part.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// Whether `name` is a name: parts joined by dots, `NAME_MAX` bytes in all.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX && name.split('.').all(valid_part)
}

/// Whether `prefix` begins names: empty, or a name's beginning -- its last
/// part may be cut short, and it may end with the dot after a part.
pub fn valid_prefix(prefix: &str) -> bool {
    if prefix.len() > NAME_MAX {
        return false;
    }
    if prefix.is_empty() {
        return true;
    }
    let body = prefix.strip_suffix('.').unwrap_or(prefix);
    !body.is_empty() && body.split('.').all(valid_part)
}

/// A name's namespace: its first part.
fn namespace(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// Whether `owner` may register `name`: the image under any namespace, a
/// module only under its own, and never under one the image keeps.
fn may_register(owner: &Owner, name: &str) -> bool {
    let ns = namespace(name);
    /* `host.` is the channel's own, for the peers' coming and going: nobody
     * names a mailbox under it, so a watch of it says the hosts alone */
    if ns == "host" {
        return false;
    }
    if owner.is_image() {
        return true;
    }
    !RESERVED.contains(&ns) && ns == owner.namespace()
}

/// The mailbox at `addr`, if it is still there.
fn find(addr: Addr) -> Option<Arc<Mbox>> {
    if addr.host != 0 || addr.slot == 0 || usize::from(addr.slot) > MAX_MAILBOXES {
        return None;
    }
    let t = table()?.lock();
    t.slots[usize::from(addr.slot)].mbox.as_ref().filter(|m| m.addr == addr).cloned()
}

/// Where a message goes: a mailbox here, or the router, for a peer.
enum Target<'a> {
    Local(Arc<Mbox>),
    Remote { router: Arc<Mbox>, host: u16, to: remote::RemoteTo<'a> },
}

/// Where a message to `to` goes, looked up under one hold of the table's
/// lock: the halt, a host nothing reaches and a mailbox gone each said. An
/// address of a peer's, or `name@host`, goes to the router.
fn resolve<'a>(to: To<'a>) -> Result<Target<'a>, Error> {
    let t = table().ok_or(Error::NoMemory)?.lock();
    if t.stopped {
        return Err(Error::Stopped);
    }
    let addr = match to {
        To::Addr(a) => a,
        To::Name(n) => match n.split_once('@') {
            Some((name, host)) => {
                if !valid_name(name) || !remote::valid_host(host) {
                    return Err(Error::Invalid);
                }
                let router = t.router.clone().ok_or(Error::NoRoute)?;
                let host = remote::live_by_name(&t, host).ok_or(Error::NoRoute)?;
                return Ok(Target::Remote { router, host, to: remote::RemoteTo::Name(name) });
            }
            None => t.names.iter().find(|r| r.name == n).ok_or(Error::Gone)?.addr,
        },
    };
    if addr.host != 0 {
        let router = t.router.clone().ok_or(Error::NoRoute)?;
        remote::live(&t, addr.host).ok_or(Error::NoRoute)?;
        return Ok(Target::Remote { router, host: addr.host, to: remote::RemoteTo::Addr(addr) });
    }
    if addr.slot == 0 || usize::from(addr.slot) > MAX_MAILBOXES {
        return Err(Error::Gone);
    }
    t.slots[usize::from(addr.slot)].mbox.as_ref().filter(|m| m.addr == addr).cloned().map(Target::Local)
        .ok_or(Error::Gone)
}

/// A mailbox made for `owner`, held to `limits` -- on `event`, a task's,
/// when one is given, and on an event of its own otherwise.
pub(crate) fn open(owner: Owner, limits: Limits, event: Option<Arc<SharedEvent>>) -> Result<Arc<Mbox>, Error> {
    if limits.messages == 0 || limits.messages > MAX_QUEUE_MESSAGES || limits.bytes == 0
        || limits.bytes > MAX_QUEUE_BYTES {
        return Err(Error::Invalid);
    }
    let mut queue = VecDeque::new();
    queue.try_reserve_exact(limits.messages as usize).map_err(|_| Error::NoMemory)?;
    let inner = Mutex::new(Inner {
        queue,
        normal: 0,
        bytes: 0,
        reserved: 0,
        kept_queued: 0,
        closed: false,
        monitored_by: Vec::new(),
        monitors: Vec::new(),
        watches: Vec::new(),
        received: 0,
        refused: 0,
        peak: 0,
    })
    .ok_or(Error::NoMemory)?;
    let on_task_event = event.is_some();
    let event = match event {
        Some(e) => e,
        None => SharedEvent::new().ok_or(Error::NoMemory)?,
    };
    let table = table().ok_or(Error::NoMemory)?;
    let mut t = table.lock();
    if t.stopped {
        return Err(Error::Stopped);
    }
    if !owner.is_image() {
        let held = t.owners.iter().find(|(o, _)| *o == owner).map_or(0, |(_, b)| *b);
        if held + limits.bytes > OWNER_MAX_BYTES {
            return Err(Error::Busy);
        }
        t.owners.try_reserve(1).map_err(|_| Error::NoMemory)?;
    }
    let slot = (1..=MAX_MAILBOXES).find(|&i| t.slots[i].mbox.is_none()).ok_or(Error::Busy)?;
    let addr = Addr { host: 0, slot: slot as u16, generation: t.slots[slot].generation };
    let mbox = Arc::new(Mbox { addr, owner, limits, inner, event, on_task_event });
    t.slots[slot].mbox = Some(mbox.clone());
    if !owner.is_image() {
        match t.owners.iter_mut().find(|(o, _)| *o == owner) {
            Some((_, b)) => *b += limits.bytes,
            None => t.owners.push((owner, limits.bytes)),
        }
    }
    Ok(mbox)
}

/// A message into the queue's own places: Full, Gone, TooLarge or NoMemory
/// hand it back.
fn push(mbox: &Arc<Mbox>, msg: Message) -> Result<(), (Error, Message)> {
    let len = msg.payload.len();
    if len > mbox.limits.bytes {
        return Err((Error::TooLarge, msg));
    }
    if QUEUED.fetch_add(len, Ordering::Relaxed) + len > POOL_MAX_BYTES {
        QUEUED.fetch_sub(len, Ordering::Relaxed);
        count(&STATS.no_memory);
        return Err((Error::NoMemory, msg));
    }
    let mut i = mbox.inner.lock();
    let refused = if i.closed {
        Some(Error::Gone)
    } else if i.normal >= mbox.limits.messages || i.bytes + len > mbox.limits.bytes {
        i.refused += 1;
        Some(Error::Full)
    } else {
        None
    };
    if let Some(e) = refused {
        drop(i);
        QUEUED.fetch_sub(len, Ordering::Relaxed);
        count(if e == Error::Gone { &STATS.gone } else { &STATS.full });
        return Err((e, msg));
    }
    /* Into the places taken at open and since: the queue never grows here */
    i.queue.push_back(msg);
    i.normal += 1;
    i.bytes += len;
    i.peak = i.peak.max(i.normal);
    drop(i);
    mbox.event.signal();
    Ok(())
}

/// A message promised into a place kept for it: dropped if its mailbox has
/// closed meanwhile.
fn push_kept(mbox: &Arc<Mbox>, msg: Message) {
    let mut i = mbox.inner.lock();
    if i.closed {
        drop(i);
        drop(msg);
        return;
    }
    i.reserved = i.reserved.saturating_sub(1);
    i.kept_queued += 1;
    i.queue.push_back(msg);
    drop(i);
    mbox.event.signal();
}

/// The next message, if there is one that fits in `cap` bytes; one that
/// does not stays first, and says how long it is.
pub(crate) enum Taken {
    Message(Message),
    TooSmall { len: usize },
    Nothing,
    Closed,
}

/// The first message off the queue, its place and its bytes given back.
fn pop_front(i: &mut Inner) -> Option<Message> {
    let msg = i.queue.pop_front()?;
    if msg.kept_room() {
        i.kept_queued = i.kept_queued.saturating_sub(1);
    } else {
        i.normal = i.normal.saturating_sub(1);
        i.bytes = i.bytes.saturating_sub(msg.payload.len());
        QUEUED.fetch_sub(msg.payload.len(), Ordering::Relaxed);
    }
    if msg.tag == TAG_DOWN {
        /* A monitor delivered is a monitor no longer held */
        let target = msg.from;
        let id = msg.reference;
        i.monitors.retain(|&(a, m)| !(a == target && m == id));
    }
    i.received += 1;
    Some(msg)
}

fn take(mbox: &Mbox, cap: usize) -> Taken {
    let mut i = mbox.inner.lock();
    let Some(front) = i.queue.front() else {
        return if i.closed { Taken::Closed } else { Taken::Nothing };
    };
    if front.payload.len() > cap {
        return Taken::TooSmall { len: front.payload.len() };
    }
    let Some(msg) = pop_front(&mut i) else { return Taken::Nothing };
    drop(i);
    count(&STATS.received);
    Taken::Message(msg)
}

/// What a receive of part of the first message came to.
pub(crate) enum Part {
    /// `n` of its bytes copied, from the offset asked: the message whose
    /// sender, tag, reference and whole length these are -- taken, off the
    /// queue, when they were its last.
    Got { from: Addr, tag: u32, reference: u64, len: usize, n: usize },
    Nothing,
    Closed,
    /// The first message is not the one the ticket names, or the offset is
    /// past its end.
    Stale,
}

/// Part of the first message into `out`, from `offset` -- `ticket` set at
/// the first part (offset 0) and held to at every part after, so that all
/// of them are of one message: the queue's count of what it has given out,
/// which only a message taken off its front changes. The message is taken
/// once its last byte is copied.
fn take_part(mbox: &Mbox, offset: usize, out: &mut [u8], ticket: &mut u64) -> Part {
    let mut i = mbox.inner.lock();
    let Some(front) = i.queue.front() else {
        return if i.closed { Part::Closed } else { Part::Nothing };
    };
    if offset == 0 {
        *ticket = i.received;
    } else if *ticket != i.received {
        return Part::Stale;
    }
    let len = front.payload.len();
    if offset > len {
        return Part::Stale;
    }
    let n = front.payload.read(offset, out);
    let got = Part::Got { from: front.from, tag: front.tag, reference: front.reference, len, n };
    if offset + n < len {
        return got;
    }
    let msg = pop_front(&mut i);
    drop(i);
    count(&STATS.received);
    /* Its pieces given back here, out of the lock */
    drop(msg);
    got
}

/// What `look` finds, looked for until `deadline` with the mailbox's event
/// slept on between looks: None when the time ran out first, another task
/// was found waiting on the event, or -- the mailbox on a task's event -- a
/// signal brought nothing it could find, which may have been for another of
/// the task's things. The event is held only while this would sleep, so
/// that a look that finds its message, or does not wait at all, never meets
/// a task asleep on the event.
fn look_and_wait<R>(mbox: &Mbox, deadline: u64, mut look: impl FnMut() -> Option<R>) -> Option<R> {
    let mut held = None;
    loop {
        if let Some(got) = look() {
            return Some(got);
        }
        let now = boot_time_ns();
        if now >= deadline {
            return None;
        }
        if held.is_none() {
            held = mbox.event.hold();
            if held.is_none() {
                klog!(0, "ipc: two tasks waiting on mailbox {}'s event at once -- the second is refused", mbox.addr);
                return None;
            }
        }
        /* A signal between the look and the wait is kept by the event */
        let signalled = held.as_ref().is_some_and(|h| h.wait_for(Duration::from_nanos((deadline - now).min(LOOK_NS))));
        if signalled && mbox.on_task_event {
            /* One more look, and back to the task: what woke it may be its
             * other things' */
            return look();
        }
    }
}

/// `take_part`, the first part waited for up to `timeout`; a part after the
/// first is there or is not, and is not waited for.
pub(crate) fn wait_part(mbox: &Mbox, timeout: Duration, offset: usize, out: &mut [u8], ticket: &mut u64) -> Part {
    let now = boot_time_ns();
    let deadline = if offset == 0 { now.saturating_add(timeout.as_nanos()) } else { now };
    let look = || match take_part(mbox, offset, out, ticket) {
        Part::Nothing => None,
        got => Some(got),
    };
    look_and_wait(mbox, deadline, look).unwrap_or(Part::Nothing)
}

/// `take`, waiting up to `timeout` for one.
pub(crate) fn wait(mbox: &Mbox, timeout: Duration, cap: usize) -> Taken {
    let deadline = boot_time_ns().saturating_add(timeout.as_nanos());
    let look = || match take(mbox, cap) {
        Taken::Nothing => None,
        got => Some(got),
    };
    look_and_wait(mbox, deadline, look).unwrap_or(Taken::Nothing)
}

/// Messages to `to` from `from`.
pub(crate) fn send(from: Addr, to: To, tag: u32, reference: u64, payload: Payload)
    -> Result<(), (Error, Payload)> {
    if tag >= TAG_SYSTEM {
        return Err((Error::Invalid, payload));
    }
    let target = match resolve(to) {
        Ok(Target::Local(t)) => t,
        Ok(Target::Remote { router, host, to }) => return remote::send(from, router, host, to, tag, reference, payload),
        Err(e) => {
            if e == Error::Gone {
                count(&STATS.gone);
            }
            return Err((e, payload));
        }
    };
    match push(&target, Message { from, tag, reference, payload }) {
        Ok(()) => {
            count(&STATS.sent);
            Ok(())
        }
        Err((e, msg)) => Err((e, msg.payload)),
    }
}

/// Where a message goes: an address -- a peer's too -- or a name on this
/// host, or `name@host` on a peer.
#[derive(Clone, Copy)]
pub enum To<'a> {
    Addr(Addr),
    Name(&'a str),
}

/// A Down to deliver: its reason written in, into its watcher's kept place.
fn deliver_down(rec: MonitorRec, reason: Reason) {
    let MonitorRec { watcher, mut down, .. } = rec;
    down.payload.overwrite(&(reason as u32).to_le_bytes());
    count(&STATS.downs);
    push_kept(&watcher, down);
}

/// `name`'s coming or going told to each of `watchers`: a watch whose
/// watcher has no room for it ends, with the message that says so.
fn tell_watchers(name: &str, addr: Addr, tag: u32, watchers: Vec<(Arc<Mbox>, u64)>) {
    for (w, id) in watchers {
        let told = match Payload::from_slice(name.as_bytes()) {
            Ok(p) => push(&w, Message { from: addr, tag, reference: id, payload: p }).is_ok(),
            Err(_) => false,
        };
        if !told {
            lose_watch(&w, id);
        }
    }
}

/// Watch `id` of `w` ended for an event it could not deliver.
fn lose_watch(w: &Arc<Mbox>, id: u64) {
    let rec = table().and_then(|t| {
        let mut t = t.lock();
        let i = t.watches.iter().position(|r| r.id == id && Arc::ptr_eq(&r.watcher, w))?;
        Some(t.watches.swap_remove(i))
    });
    let Some(mut rec) = rec else { return };
    w.inner.lock().watches.retain(|&x| x != id);
    if let Some(lost) = rec.lost.take() {
        count(&STATS.watch_lost);
        push_kept(w, lost);
    }
}

/// The watchers of names under prefixes `name` begins with.
fn watchers_of(t: &Table, name: &str) -> Vec<(Arc<Mbox>, u64)> {
    let mut out = Vec::new();
    for r in t.watches.iter().filter(|r| name.starts_with(r.prefix.as_str())) {
        if out.try_reserve(1).is_err() {
            break;
        }
        out.push((r.watcher.clone(), r.id));
    }
    out
}

/// `mbox` closed: no message reaches it from here on, what it held is
/// dropped, every monitor of it gets its Down, its own monitors and watches
/// end, and its name goes -- each watch of it told.
pub(crate) fn close(mbox: &Arc<Mbox>, reason: Reason) {
    let Some(table) = table() else { return };
    /* The table first: the slot freed for another mailbox, of another
     * generation, the name and the watches taken away */
    let (name, watchers, watches, remote_gone, router, lost) = {
        let mut t = table.lock();
        let was_router = t.router.as_ref().is_some_and(|r| Arc::ptr_eq(r, mbox));
        if was_router {
            t.router = None;
        }
        let remote_gone = remote::forget(&mut t, mbox);
        let router = t.router.clone();
        let slot = usize::from(mbox.addr.slot);
        /* Two closes of one mailbox -- its module's own and the sweep after
         * its exit, say -- and only the one that takes its slot gives back
         * what it held of its module's bytes */
        let ours = t.slots[slot].mbox.as_ref().is_some_and(|m| Arc::ptr_eq(m, mbox));
        if ours {
            t.slots[slot].mbox = None;
            t.slots[slot].generation = t.slots[slot].generation.wrapping_add(1).max(1);
        }
        let name = t.names.iter().position(|n| n.addr == mbox.addr).map(|i| t.names.swap_remove(i).name);
        let watchers = name.as_deref().map(|n| watchers_of(&t, n)).unwrap_or_default();
        let mut watches = Vec::new();
        let mut i = 0;
        while i < t.watches.len() {
            if Arc::ptr_eq(&t.watches[i].watcher, mbox) {
                if watches.try_reserve(1).is_err() {
                    break;
                }
                watches.push(t.watches.swap_remove(i));
            } else {
                i += 1;
            }
        }
        if ours && !mbox.owner.is_image() {
            if let Some((_, b)) = t.owners.iter_mut().find(|(o, _)| *o == mbox.owner) {
                *b = b.saturating_sub(mbox.limits.bytes);
            }
            t.owners.retain(|(_, b)| *b != 0);
        }
        /* A router's close: nothing carries messages to other hosts, so
         * every peer is lost -- after its own watches and remote monitors
         * went above, so that what the loss tells goes to everyone but the
         * mailbox closing, which would otherwise hear it as it closes and
         * hand it to the task waiting on it in place of the close */
        let lost = if was_router { remote::lose_all(&mut t) } else { Vec::new() };
        (name, watchers, watches, remote_gone, router, lost)
    };
    remote::tell_lost(lost);
    /* Its monitors of mailboxes on peers: their records gone with it, their
     * peers asked to stop watching */
    for rec in &remote_gone {
        let mut i = mbox.inner.lock();
        i.reserved = i.reserved.saturating_sub(1);
        i.monitors.retain(|&(_, m)| m != rec.id);
    }
    remote::unwatch_all(router, mbox.addr, remote_gone);
    /* Then the mailbox itself */
    let (queue, monitored_by, monitors) = {
        let mut i = mbox.inner.lock();
        if i.closed {
            return;
        }
        i.closed = true;
        QUEUED.fetch_sub(i.bytes, Ordering::Relaxed);
        i.normal = 0;
        i.bytes = 0;
        (core::mem::take(&mut i.queue), core::mem::take(&mut i.monitored_by), core::mem::take(&mut i.monitors))
    };
    /* A receiver waiting wakes, and finds it closed */
    mbox.event.signal();
    /* Its own monitors' records, at the mailboxes it watched */
    for (target, id) in monitors {
        let Some(t) = find(target) else { continue };
        let rec = {
            let mut ti = t.inner.lock();
            ti.monitored_by.iter().position(|r| r.id == id).map(|i| ti.monitored_by.swap_remove(i))
        };
        drop(rec);
    }
    /* The Downs it owes */
    for rec in monitored_by {
        deliver_down(rec, reason);
    }
    if let Some(name) = name {
        tell_watchers(&name, mbox.addr, TAG_NAME_DOWN, watchers);
    }
    /* What it held, out of every lock */
    drop(queue);
    drop(watches);
}

/// `mbox` named `name`, in `scope`.
pub(crate) fn register(mbox: &Arc<Mbox>, name: &str, scope: Scope) -> Result<(), Error> {
    if !valid_name(name) {
        return Err(Error::Invalid);
    }
    if !may_register(&mbox.owner, name) {
        return Err(Error::NotYours);
    }
    let copied = copy(name)?;
    let table = table().ok_or(Error::NoMemory)?;
    let watchers = {
        let mut t = table.lock();
        if t.stopped {
            return Err(Error::Stopped);
        }
        let slot = usize::from(mbox.addr.slot);
        if !t.slots[slot].mbox.as_ref().is_some_and(|m| Arc::ptr_eq(m, mbox)) {
            return Err(Error::Gone);
        }
        if t.names.iter().any(|n| n.addr == mbox.addr || n.name == name) {
            return Err(Error::Taken);
        }
        if t.names.len() >= MAX_NAMES {
            return Err(Error::Busy);
        }
        t.names.push(NameRec { name: copied, addr: mbox.addr, scope });
        watchers_of(&t, name)
    };
    tell_watchers(name, mbox.addr, TAG_NAME_UP, watchers);
    Ok(())
}

/// `watcher` told once, with a Down, when `target` goes: the monitor's id.
pub(crate) fn monitor(watcher: &Arc<Mbox>, target: Addr) -> Result<u64, Error> {
    if target.host != 0 {
        return remote::monitor(watcher, target);
    }
    let id = next_id();
    let down = Message { from: target, tag: TAG_DOWN, reference: id, payload: Payload::from_slice(&[0; DOWN_BYTES])? };
    {
        let mut w = watcher.inner.lock();
        if w.closed {
            return Err(Error::Gone);
        }
        if w.monitors.len() >= MAX_MONITORS {
            return Err(Error::Busy);
        }
        w.monitors.try_reserve(1).map_err(|_| Error::NoMemory)?;
        w.keep_one(&watcher.limits)?;
        w.monitors.push((target, id));
    }
    let mut rec = MonitorRec { id, watcher: watcher.clone(), down };
    if let Some(t) = find(target) {
        let mut ti = t.inner.lock();
        if !ti.closed {
            if ti.monitored_by.try_reserve(1).is_err() {
                drop(ti);
                let mut w = watcher.inner.lock();
                w.reserved = w.reserved.saturating_sub(1);
                w.monitors.retain(|&(_, m)| m != id);
                return Err(Error::NoMemory);
            }
            ti.monitored_by.push(rec);
            drop(ti);
            /* A watcher that closed meanwhile -- after its close looked for
             * its monitors' records, before this one was there -- takes it
             * back, one mailbox's lock at a time */
            if watcher.inner.lock().closed {
                let rec = {
                    let mut ti = t.inner.lock();
                    ti.monitored_by.iter().position(|r| r.id == id).map(|i| ti.monitored_by.swap_remove(i))
                };
                drop(rec);
                return Err(Error::Gone);
            }
            return Ok(id);
        }
        drop(ti);
    }
    /* Gone already, or going: its Down now */
    rec.down.from = target;
    deliver_down(rec, Reason::NoSuch);
    Ok(id)
}

/// Monitor `id` of `watcher` dropped. If its mailbox went meanwhile, its
/// Down is on its way or here already, in the place kept for it.
pub(crate) fn demonitor(watcher: &Arc<Mbox>, id: u64) {
    let target = {
        let mut w = watcher.inner.lock();
        let Some(i) = w.monitors.iter().position(|&(_, m)| m == id) else { return };
        w.monitors.swap_remove(i).0
    };
    if target.host != 0 {
        remote::demonitor(watcher, id, target);
        return;
    }
    let rec = find(target).and_then(|t| {
        let mut ti = t.inner.lock();
        ti.monitored_by.iter().position(|r| r.id == id).map(|i| ti.monitored_by.swap_remove(i))
    });
    if let Some(rec) = rec {
        let mut w = watcher.inner.lock();
        w.reserved = w.reserved.saturating_sub(1);
        drop(w);
        drop(rec);
    }
}

/// `watcher` told of every name under `prefix` that comes and goes, from
/// now on: the watch's id. What is there already, `list` says.
pub(crate) fn watch(watcher: &Arc<Mbox>, prefix: &str) -> Result<u64, Error> {
    if !valid_prefix(prefix) {
        return Err(Error::Invalid);
    }
    let id = next_id();
    let lost = Message { from: Addr::NONE, tag: TAG_WATCH_LOST, reference: id, payload: Payload::empty() };
    let prefix = copy(prefix)?;
    {
        let mut w = watcher.inner.lock();
        if w.closed {
            return Err(Error::Gone);
        }
        if w.watches.len() >= MAX_WATCHES_EACH {
            return Err(Error::Busy);
        }
        w.watches.try_reserve(1).map_err(|_| Error::NoMemory)?;
        w.keep_one(&watcher.limits)?;
        w.watches.push(id);
    }
    let table = table().ok_or(Error::NoMemory)?;
    let added = {
        let mut t = table.lock();
        /* A close takes the mailbox off its slot and its watches out of the
         * table under this lock: one that has begun refuses this watch,
         * which would otherwise outlive the mailbox in the table */
        let slot = usize::from(watcher.addr.slot);
        if !t.slots[slot].mbox.as_ref().is_some_and(|m| Arc::ptr_eq(m, watcher)) {
            Err(Error::Gone)
        } else if t.watches.len() >= MAX_WATCHES {
            Err(Error::Busy)
        } else {
            t.watches.push(WatchRec { id, prefix, watcher: watcher.clone(), lost: Some(lost) });
            Ok(())
        }
    };
    if let Err(e) = added {
        let mut w = watcher.inner.lock();
        w.reserved = w.reserved.saturating_sub(1);
        w.watches.retain(|&x| x != id);
        return Err(e);
    }
    Ok(id)
}

/// Watch `id` of `watcher` ended.
pub(crate) fn unwatch(watcher: &Arc<Mbox>, id: u64) {
    let rec = table().and_then(|t| {
        let mut t = t.lock();
        let i = t.watches.iter().position(|r| r.id == id && Arc::ptr_eq(&r.watcher, watcher))?;
        Some(t.watches.swap_remove(i))
    });
    let mut w = watcher.inner.lock();
    w.watches.retain(|&x| x != id);
    if rec.as_ref().is_some_and(|r| r.lost.is_some()) {
        w.reserved = w.reserved.saturating_sub(1);
    }
    drop(w);
    drop(rec);
}

/// A mailbox of the image's: closed when dropped.
pub struct Mailbox {
    mbox: Arc<Mbox>,
}

impl Mailbox {
    /// A mailbox held to `limits`.
    pub fn new(limits: Limits) -> Result<Mailbox, Error> {
        open(Owner::IMAGE, limits, None).map(|mbox| Mailbox { mbox })
    }

    /// A mailbox held to `limits`, made on `event` -- the event of a task
    /// that waits for this and more: whatever comes to it, a message, a
    /// Down, a wake, its close, signals that event, and a `recv` waits on it
    /// until its first signal. The mailbox holds the event for as long as
    /// it is open, so the task may let go of it first.
    pub fn new_on(limits: Limits, event: &Arc<SharedEvent>) -> Result<Mailbox, Error> {
        open(Owner::IMAGE, limits, Some(event.clone())).map(|mbox| Mailbox { mbox })
    }

    pub fn addr(&self) -> Addr {
        self.mbox.addr
    }

    /// The mailbox named `name` on this host, for `scope`: refused for a
    /// name taken, or a mailbox named already.
    pub fn register(&self, name: &str, scope: Scope) -> Result<(), Error> {
        register(&self.mbox, name, scope)
    }

    /// A message to `to`, from this mailbox: Ok once it is in the mailbox it
    /// goes to. What is refused hands the payload back.
    pub fn send(&self, to: To, tag: u32, reference: u64, payload: Payload) -> Result<(), (Error, Payload)> {
        send(self.mbox.addr, to, tag, reference, payload)
    }

    /// The answer to `msg`: to its sender, with its reference.
    pub fn reply(&self, msg: &Message, tag: u32, payload: Payload) -> Result<(), (Error, Payload)> {
        send(self.mbox.addr, To::Addr(msg.from), tag, msg.reference, payload)
    }

    /// The next message, if there is one.
    pub fn try_recv(&self) -> Option<Message> {
        match take(&self.mbox, usize::MAX) {
            Taken::Message(m) => Some(m),
            _ => None,
        }
    }

    /// The next message, waiting up to `timeout` for one -- None sooner if
    /// closed, and, on a task's event (`new_on`), at the event's first
    /// signal that brought none.
    pub fn recv(&self, timeout: Duration) -> Option<Message> {
        match wait(&self.mbox, timeout, usize::MAX) {
            Taken::Message(m) => Some(m),
            _ => None,
        }
    }

    pub fn monitor(&self, target: Addr) -> Result<u64, Error> {
        monitor(&self.mbox, target)
    }

    pub fn demonitor(&self, id: u64) {
        demonitor(&self.mbox, id)
    }

    pub fn watch(&self, prefix: &str) -> Result<u64, Error> {
        watch(&self.mbox, prefix)
    }

    pub fn unwatch(&self, id: u64) {
        unwatch(&self.mbox, id)
    }

    /// A reference for a request, no other's.
    pub fn next_reference(&self) -> u64 {
        next_id()
    }

    /* ---- the router's: what carries messages to other hosts ---- */

    /// This mailbox made the router: one at a time, until it closes. What
    /// is sent to a peer's address, or to `name@host`, comes here as a
    /// message of `TAG_ROUTE`, its payload ipcwire's route head and then
    /// the message's own bytes.
    pub fn route(&self) -> Result<(), Error> {
        remote::route(&self.mbox)
    }

    /// The peer `name` up: its number, the host part of its mailboxes'
    /// addresses here -- the same while it stays up, another once lost.
    pub fn peer_up(&self, name: &str) -> Result<u16, Error> {
        remote::peer_up(&self.mbox, name)
    }

    /// The peer `host` lost: every monitor of an address there gets its Down
    /// (HostLost), and what it may deliver to is forgotten.
    pub fn peer_lost(&self, host: u16) -> Result<(), Error> {
        remote::peer_lost(&self.mbox, host)
    }

    /// A message come from the peer `host`, from its mailbox `from` (its
    /// own address of it), to this host's `to` -- an address here (host 0)
    /// or a name: delivered to a `Cluster` name, or to a mailbox that has
    /// sent to that peer, `NotYours` otherwise.
    pub fn deliver_remote(&self, host: u16, from: u64, to: To, tag: u32, reference: u64, payload: Payload)
        -> Result<(), Error>
    {
        let to = match to {
            To::Addr(a) => remote::RemoteTo::Addr(a),
            To::Name(n) => {
                if !valid_name(n) {
                    return Err(Error::Invalid);
                }
                remote::RemoteTo::Name(n)
            }
        };
        remote::deliver_remote(&self.mbox, host, from, to, tag, reference, payload)
    }

    /// The peer `host` says the mailbox the monitor `id` watched has gone.
    pub fn remote_down(&self, host: u16, id: u64, reason: Reason) -> Result<(), Error> {
        remote::remote_down(&self.mbox, host, id, reason)
    }

    /// A message from `sender` -- a mailbox here that has sent to the peer
    /// `host` -- to `target` there (the peer's own address of a mailbox, or
    /// a name) did not get there, for `why`: told to the sender, TAG_BOUNCED.
    /// Dropped and counted when the sender has no room.
    pub fn bounced(&self, host: u16, sender: Addr, target: To, tag: u32, reference: u64, why: Bounce)
        -> Result<(), Error>
    {
        let target = match target {
            To::Addr(a) => remote::RemoteTo::Addr(a),
            To::Name(n) => {
                if !valid_name(n) {
                    return Err(Error::Invalid);
                }
                remote::RemoteTo::Name(n)
            }
        };
        remote::bounced(&self.mbox, host, sender, target, tag, reference, why)
    }
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        close(&self.mbox, Reason::Closed);
    }
}

/// The receiver of the mailbox at `addr` woken, if it waits: from anywhere
/// in task context.
pub fn wake(addr: Addr) {
    if let Some(m) = find(addr) {
        m.event.signal();
    }
}

/// How a call ended, when no answer came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The request was refused.
    Send(Error),
    /// The mailbox called went, before it answered.
    Down(Reason),
    /// Nothing came in time.
    Timeout,
}

/// A request to `to` and its answer: from `mine`, which takes nothing else
/// meanwhile -- what else comes to it is dropped, and counted.
pub fn call(mine: &Mailbox, to: To, tag: u32, payload: Payload, timeout: Duration) -> Result<Message, CallError> {
    let target = match to {
        To::Addr(a) => a,
        To::Name(n) => lookup(n).ok_or(CallError::Send(Error::Gone))?,
    };
    let reference = mine.next_reference();
    let monitor = mine.monitor(target).map_err(CallError::Send)?;
    if let Err((e, _)) = mine.send(To::Addr(target), tag, reference, payload) {
        mine.demonitor(monitor);
        return Err(CallError::Send(e));
    }
    let deadline = boot_time_ns().saturating_add(timeout.as_nanos());
    loop {
        let now = boot_time_ns();
        if now >= deadline {
            mine.demonitor(monitor);
            return Err(CallError::Timeout);
        }
        let Some(msg) = mine.recv(Duration::from_nanos(deadline - now)) else { continue };
        if !msg.is_system() && msg.reference == reference && msg.from == target {
            mine.demonitor(monitor);
            return Ok(msg);
        }
        if msg.tag == TAG_DOWN && msg.reference == monitor {
            return Err(CallError::Down(msg.down().unwrap_or(Reason::Closed)));
        }
        count(&STATS.stray);
    }
}

/// The address `name` names on this host.
pub fn lookup(name: &str) -> Option<Addr> {
    let t = table()?.lock();
    t.names.iter().find(|n| n.name == name).map(|n| n.addr)
}

/// A name, as `list` copies it out.
pub struct NameInfo {
    pub name: String,
    pub addr: Addr,
    pub scope: Scope,
    pub owner: String,
}

/// The names under `prefix`, in no particular order.
pub fn list(prefix: &str) -> Vec<NameInfo> {
    let mut out = Vec::new();
    let Some(table) = table() else { return out };
    let t = table.lock();
    for n in t.names.iter().filter(|n| n.name.starts_with(prefix)) {
        let owner = t.slots[usize::from(n.addr.slot)].mbox.as_ref().map(|m| m.owner).unwrap_or(Owner::IMAGE);
        let (Ok(name), Ok(owner)) = (copy(&n.name), copy(owner.as_str())) else { break };
        if out.try_reserve(1).is_err() {
            break;
        }
        out.push(NameInfo { name, addr: n.addr, scope: n.scope, owner });
    }
    out
}

/// A mailbox, as `boxes` copies it out.
pub struct BoxInfo {
    pub addr: Addr,
    pub owner: String,
    pub name: Option<String>,
    pub queued: u32,
    pub limit: u32,
    pub bytes: usize,
    pub byte_limit: usize,
    pub kept: usize,
    pub monitors: usize,
    pub monitored_by: usize,
    pub watches: usize,
    pub received: u64,
    pub refused: u64,
    pub peak: u32,
}

/// Every mailbox of the host.
pub fn boxes() -> Vec<BoxInfo> {
    let mut out = Vec::new();
    let Some(table) = table() else { return out };
    /* The mailboxes and their names copied under the table's lock, each
     * mailbox looked at after it, one at a time */
    let mut found: Vec<(Arc<Mbox>, Option<String>)> = Vec::new();
    {
        let t = table.lock();
        for m in t.slots.iter().filter_map(|e| e.mbox.as_ref()) {
            let name = t.names.iter().find(|n| n.addr == m.addr).and_then(|n| copy(&n.name).ok());
            if found.try_reserve(1).is_err() {
                break;
            }
            found.push((m.clone(), name));
        }
    }
    for (m, name) in found {
        let Ok(owner) = copy(m.owner.as_str()) else { break };
        let i = m.inner.lock();
        let info = BoxInfo {
            addr: m.addr,
            owner,
            name,
            queued: i.normal,
            limit: m.limits.messages,
            bytes: i.bytes,
            byte_limit: m.limits.bytes,
            kept: i.reserved + i.kept_queued,
            monitors: i.monitors.len(),
            monitored_by: i.monitored_by.len(),
            watches: i.watches.len(),
            received: i.received,
            refused: i.refused,
            peak: i.peak,
        };
        drop(i);
        if out.try_reserve(1).is_err() {
            break;
        }
        out.push(info);
    }
    out
}

/// The host's counters.
pub struct Counters {
    pub sent: u64,
    pub received: u64,
    pub full: u64,
    pub gone: u64,
    pub no_memory: u64,
    pub downs: u64,
    pub watch_lost: u64,
    pub stray: u64,
    pub routed: u64,
    pub delivered_remote: u64,
    pub refused_remote: u64,
    pub peers_up: u64,
    pub peers_lost: u64,
    pub bounced: u64,
    pub bounces_dropped: u64,
    pub queued_bytes: usize,
    pub mailboxes: usize,
    pub names: usize,
    pub watches: usize,
}

pub fn counters() -> Counters {
    let (mailboxes, names, watches) = table().map_or((0, 0, 0), |t| {
        let t = t.lock();
        (t.slots.iter().filter(|e| e.mbox.is_some()).count(), t.names.len(), t.watches.len())
    });
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    Counters {
        sent: get(&STATS.sent),
        received: get(&STATS.received),
        full: get(&STATS.full),
        gone: get(&STATS.gone),
        no_memory: get(&STATS.no_memory),
        downs: get(&STATS.downs),
        watch_lost: get(&STATS.watch_lost),
        stray: get(&STATS.stray),
        routed: get(&STATS.routed),
        delivered_remote: get(&STATS.delivered_remote),
        refused_remote: get(&STATS.refused_remote),
        peers_up: get(&STATS.peers_up),
        peers_lost: get(&STATS.peers_lost),
        bounced: get(&STATS.bounced),
        bounces_dropped: get(&STATS.bounces_dropped),
        queued_bytes: QUEUED.load(Ordering::Relaxed),
        mailboxes,
        names,
        watches,
    }
}

/// The mailboxes module `name` left open, closed after its exit -- each
/// said, a bug of the module's -- with a Down to every monitor of each.
pub fn module_gone(name: &[u8]) {
    let (Some(owner), Some(table)) = (Owner::module(name), table()) else { return };
    let mut left: Vec<Arc<Mbox>> = Vec::new();
    {
        let t = table.lock();
        for m in t.slots.iter().filter_map(|e| e.mbox.as_ref()).filter(|m| m.owner == owner) {
            if left.try_reserve(1).is_err() {
                break;
            }
            left.push(m.clone());
        }
    }
    for m in left {
        klog!(0, "ipc: module {} left mailbox {} open at its unload -- closed now", owner.as_str(), m.addr);
        close(&m, Reason::Unloaded);
    }
}

/// The halt: no mailbox is made and no message sent from here on, and a
/// module's mailbox still open -- its module not unloaded, which the halt
/// does first -- is said.
pub fn stop() {
    let Some(table) = table() else { return };
    let mut t = table.lock();
    t.stopped = true;
    for m in t.slots.iter().filter_map(|e| e.mbox.as_ref()).filter(|m| !m.owner.is_image()) {
        klog!(0, "ipc: mailbox {} of module {} still open at the halt", m.addr, m.owner.as_str());
    }
}

/// The owner a C ABI caller is, from an address in its code: the module
/// whose image holds it, or the image.
pub(crate) fn owner_at(addr: usize) -> Owner {
    let mut name = [0u8; OWNER_NAME_MAX + 1];
    /* `name` takes its length's bytes */
    let n = unsafe { ffi::system::kernel_module_name_at(addr, name.as_mut_ptr(), name.len()) };
    if n == 0 || n > OWNER_NAME_MAX {
        return Owner::IMAGE;
    }
    Owner::module(&name[..n]).unwrap_or(Owner::IMAGE)
}

/// For the C ABI: a mailbox by its address, still open.
pub(crate) fn handle(addr: Addr) -> Option<Arc<Mbox>> {
    find(addr)
}

impl Mbox {
    pub(crate) fn addr(&self) -> Addr {
        self.addr
    }

    pub(crate) fn wake(&self) {
        self.event.signal();
    }

    pub(crate) fn is_image(&self) -> bool {
        self.owner.is_image()
    }
}
