/* The message channel as a loadable module reaches it (src/crates/ipc
   defines these, kcore::ipc wraps them): a module is linked on its own, so a
   C ABI is the only seam it and the channel can share. A mailbox is named by
   its address -- host, slot, generation, from the top down -- looked up on
   every call, so one that has closed is gone, never another's. Everything a
   module hands over is copied; nothing here calls the module's code. Code
   inside the kernel image uses the crate itself. Task context, all of it. */

/// A message's header, as `kernel_ipc_recv` fills it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IpcHeader {
    pub from: u64,
    pub reference: u64,
    /// The payload's bytes: copied into the buffer, or -- for one too small
    /// -- how many it needs.
    pub len: u64,
    pub tag: u32,
    pub _pad: u32,
}

/// A name with its host -- `name@host`, a send's to a peer -- at most: a
/// name's bytes, the `@`, a host's name (a DNS label).
pub const IPC_REMOTE_NAME_MAX: usize = IPC_NAME_MAX + 1 + 63;

/// A name's bytes, at most.
pub const IPC_NAME_MAX: usize = 48;

/// A name, as `kernel_ipc_list` copies it out.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IpcName {
    pub addr: u64,
    pub scope: u32,
    pub len: u32,
    pub name: [u8; IPC_NAME_MAX],
}

/// Tags at and above this one are the channel's own: a sender is refused
/// them. What each says is ipc's (src/crates/ipc/src/lib.rs).
pub const IPC_TAG_SYSTEM: u32 = 0xFFFF_0000;
/// A mailbox monitored is gone: `from` it, the reference the monitor's id,
/// the payload the reason (IPC_DOWN_*, 4 bytes, little-endian).
pub const IPC_TAG_DOWN: u32 = IPC_TAG_SYSTEM | 1;
/// A name under a watch's prefix registered, or gone: `from` its mailbox,
/// the reference the watch's id, the payload the name.
pub const IPC_TAG_NAME_UP: u32 = IPC_TAG_SYSTEM | 2;
pub const IPC_TAG_NAME_DOWN: u32 = IPC_TAG_SYSTEM | 3;
/// A watch that could not deliver an event has ended; the reference is its
/// id.
pub const IPC_TAG_WATCH_LOST: u32 = IPC_TAG_SYSTEM | 4;
/// A message for another host, in the router's mailbox: its payload begins
/// with ipcwire's route head (`ipcwire::route`), its own bytes after it.
pub const IPC_TAG_ROUTE: u32 = IPC_TAG_SYSTEM | 5;
/// A message for another host that did not reach its mailbox there, told
/// back to its sender: `from` the mailbox it was for as this host knows it
/// -- the peer's number in its address, or the peer's alone (slot 0) for a
/// name there -- the reference the message's, and the payload why
/// (IPC_BOUNCE_*), the message's tag and, for a name, the name: 4 bytes, 4
/// bytes, little-endian, and the name's.
pub const IPC_TAG_BOUNCED: u32 = IPC_TAG_SYSTEM | 6;

/// Why a message bounced: its mailbox there was full; it was gone, or no
/// mailbox had the name; or it never went -- the way to that host was full,
/// or the host was lost before it did.
pub const IPC_BOUNCE_FULL: u32 = 1;
pub const IPC_BOUNCE_GONE: u32 = 2;
pub const IPC_BOUNCE_UNSENT: u32 = 3;

/// A tag with this bit set goes between hosts by the bulk connection, any
/// other by the control one: order between two mailboxes holds within a
/// class, not across the two (plans/ipc.md, ipcnet).
pub const IPC_TAG_BULK: u32 = 0x4000_0000;

/// Why a Down came.
pub const IPC_DOWN_CLOSED: u32 = 1;
pub const IPC_DOWN_UNLOADED: u32 = 2;
pub const IPC_DOWN_NO_SUCH: u32 = 3;
pub const IPC_DOWN_HOST_LOST: u32 = 4;
pub const IPC_DOWN_STOPPED: u32 = 5;

/// What a call answers: done, or why not.
pub const IPC_OK: i32 = 0;
pub const IPC_FULL: i32 = 1;
pub const IPC_GONE: i32 = 2;
pub const IPC_NO_ROUTE: i32 = 3;
pub const IPC_TOO_LARGE: i32 = 4;
pub const IPC_NO_MEMORY: i32 = 5;
pub const IPC_INVALID: i32 = 6;
pub const IPC_TAKEN: i32 = 7;
pub const IPC_NOT_YOURS: i32 = 8;
pub const IPC_BUSY: i32 = 9;
pub const IPC_STOPPED: i32 = 10;

/// This host's tasks and modules alone.
pub const IPC_SCOPE_HOST: u32 = 0;
/// The cluster's hosts too.
pub const IPC_SCOPE_CLUSTER: u32 = 1;

/// What `kernel_ipc_recv` answers when it has no length to: nothing came
/// in time (or it was woken); the next message is longer than the buffer,
/// and stays first -- `len` says how long; the mailbox is closed, or none.
pub const IPC_RECV_NONE: i64 = -1;
pub const IPC_RECV_TOO_SMALL: i64 = -2;
pub const IPC_RECV_CLOSED: i64 = -3;

unsafe extern "C" {
    /// A mailbox, held to `messages` and `bytes` queued: its address, or 0
    /// when refused -- limits past what a mailbox takes, the module's
    /// mailboxes at their bytes, every slot taken, the halt. `owner` is an
    /// address in the caller's own code: the module whose image holds it
    /// owns the mailbox, and its names are under that module's namespace.
    pub safe fn kernel_ipc_open(owner: usize, messages: u32, bytes: u64) -> u64;

    /// Closes it: what it holds is dropped, every monitor of it gets its
    /// Down, its monitors and watches end, its name goes. An address that is
    /// no mailbox is nothing.
    pub safe fn kernel_ipc_close(mbox: u64);

    /// Names the mailbox `name` (`len` bytes) on this host, `scope` one of
    /// the IPC_SCOPE_* -- ipc's `Error::code`, 0 for done.
    pub fn kernel_ipc_register(mbox: u64, name: *const u8, len: usize, scope: u32) -> i32;

    /// The address `name` names on this host, or 0.
    pub fn kernel_ipc_lookup(name: *const u8, len: usize) -> u64;

    /// A message from `mbox` to `to` -- or, with `name_len` past 0, to the
    /// mailbox `name` names -- its `len` bytes copied: ipc's `Error::code`,
    /// 0 once it is in the mailbox it goes to.
    pub fn kernel_ipc_send(mbox: u64, to: u64, name: *const u8, name_len: usize, tag: u32, reference: u64,
                           data: *const u8, len: usize) -> i32;

    /// The next message of `mbox`, waiting up to `timeout_ns` for one: its
    /// header into `header` and its payload into `buf` (`cap` bytes) --
    /// its length -- or one of the IPC_RECV_*. One task receives from a
    /// mailbox at a time.
    pub fn kernel_ipc_recv(mbox: u64, timeout_ns: u64, header: *mut IpcHeader, buf: *mut u8, cap: usize) -> i64;

    /// The task waiting in `kernel_ipc_recv` on `mbox` woken, with nothing:
    /// a module's way to tell its own server task to look at something else
    /// -- that it is to stop, say.
    pub safe fn kernel_ipc_wake(mbox: u64);

    /// `mbox` told once, with a Down, when `target` goes: the monitor's id,
    /// or 0 when refused.
    pub safe fn kernel_ipc_monitor(mbox: u64, target: u64) -> u64;

    /// A monitor of `mbox`'s dropped. A Down already on its way still comes.
    pub safe fn kernel_ipc_demonitor(mbox: u64, id: u64);

    /// `mbox` told of every name under `prefix` that comes and goes: the
    /// watch's id, or 0 when refused.
    pub fn kernel_ipc_watch(mbox: u64, prefix: *const u8, len: usize) -> u64;

    /// A watch of `mbox`'s ended.
    pub safe fn kernel_ipc_unwatch(mbox: u64, id: u64);

    /// The names under `prefix`, at most `max` of them, into `out`: how many
    /// were copied.
    pub fn kernel_ipc_list(prefix: *const u8, len: usize, out: *mut IpcName, max: usize) -> usize;

    /// `mbox` -- a module's own -- made the router: what carries messages
    /// to other hosts (ipcnet's). One at a time, until it closes: ipc's
    /// `Error::code`, 0 for done.
    pub safe fn kernel_ipc_route(mbox: u64) -> i32;

    /// The peer `name` (`len` bytes, a host's name) up, the router's call:
    /// its number -- the host part of its mailboxes' addresses here, the
    /// same while it stays up and another after it was lost -- or 0 when
    /// refused.
    pub fn kernel_ipc_peer_up(router: u64, name: *const u8, len: usize) -> u32;

    /// The peer `peer` lost, the router's call: every monitor of an address
    /// there gets its Down (HostLost), and what was let reply from there no
    /// longer is. ipc's `Error::code`.
    pub safe fn kernel_ipc_peer_lost(router: u64, peer: u32) -> i32;

    /// A message come from the peer `peer`, the router's call: from its
    /// mailbox `from` (the peer's own address of it), to this host's mailbox
    /// `to` -- or, with `name_len` past 0, to the `Cluster` name `name` -- its
    /// `len` bytes copied. Delivered to a `Cluster` name, or to a mailbox that
    /// has sent to that peer; refused otherwise. ipc's `Error::code`.
    pub fn kernel_ipc_deliver_remote(router: u64, peer: u32, from: u64, to: u64, name: *const u8, name_len: usize,
                                     tag: u32, reference: u64, data: *const u8, len: usize) -> i32;

    /// The peer `peer` says the mailbox a monitor of this host's watched has
    /// gone (`reason`, an IPC_DOWN_*): the monitor `id`'s Down delivered.
    /// ipc's `Error::code`.
    pub safe fn kernel_ipc_remote_down(router: u64, peer: u32, id: u64, reason: u32) -> i32;

    /// The router's: a message from this host's mailbox `sender` to the
    /// peer `peer`'s mailbox `target` (the peer's own address of it) -- or,
    /// with `name_len` past 0, to the name `name` there -- with `tag` and
    /// `reference`, did not get there (`why`, an IPC_BOUNCE_*): told to the
    /// sender as IPC_TAG_BOUNCED, to a mailbox that has sent to that peer
    /// alone. Dropped, and counted, when the sender has no room. ipc's
    /// `Error::code`.
    pub fn kernel_ipc_bounced(router: u64, peer: u32, sender: u64, target: u64, name: *const u8, name_len: usize,
                              tag: u32, reference: u64, why: u32) -> i32;
}
