//! A peer -- another host of the cluster -- as this host keeps it: the
//! session its two connections are of, its number in the channel while it
//! is up, what waits to go to it by class, and the monitors this host's
//! router holds for it.
//!
//! A peer is up once both of a session's connections -- control and bulk --
//! have said their `Hello`; it is lost the moment either of them ends, says
//! nothing for LOST_MS, or a new session of it arrives, and only once a
//! session: what was queued for it and never went is bounced back to its
//! senders, the channel is told (every monitor of an address there gets
//! HostLost), and the monitors it held here are taken back. Its locks are
//! taken `st` before `out`, never the other way, and nothing is waited for
//! under either.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use ipcwire::Dest;
use kcore::ipc::{Addr, Bounce, Mailbox, RemoteTo};
use kcore::sync::Mutex;

use super::files::PeerConf;

/// The classes of what goes to a peer, each its own connection: control --
/// Hello, heartbeats, monitors, Downs, bounces, and every message whose
/// tag has not TAG_BULK -- and bulk.
pub const CONTROL: usize = 0;
pub const BULK: usize = 1;
pub const CLASSES: usize = 2;

/// What may wait to go to a peer, by class, before the router bounces what
/// comes for it (`Bounce::Unsent`): the bytes of messages not yet begun on
/// their connection.
pub const QUEUE_BYTES: [usize; CLASSES] = [1 << 20, 4 << 20];
/// Frames of the control kind waiting at once: Pongs, Downs, bounces,
/// monitors. Past it the newest is dropped and counted -- a bounce or a Pong
/// is news, and what asked for a monitor's Down is told by HostLost should
/// the peer go.
pub const FRAMES_MAX: usize = 4096;
/// The monitors a peer may have this host's router hold for it.
pub const MONITORS_MAX: usize = 128;

/// A value behind a kernel mutex, which sleeps: never held while waiting
/// for a task, nor taken by anything such a task could be waiting in.
pub struct Locked<T>(Mutex<T>);

impl<T> Locked<T> {
    pub fn new(value: T) -> Option<Self> {
        Some(Self(Mutex::new(value)?))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock())
    }
}

/// A message for a peer, as the router took it from its mailbox: its bytes
/// in the pieces they came in, the first of them starting with the route
/// head, which `skip` passes over.
pub struct OutMsg {
    pub from: u64,
    pub to: Dest,
    pub tag: u32,
    pub reference: u64,
    pub pieces: Vec<Vec<u8>>,
    pub skip: usize,
    pub len: usize,
}

/// What waits to go to a peer by one class.
pub struct Out {
    /// The peer's number while these are for it; 0 while it is down, and
    /// nothing is queued then.
    pub number: u16,
    /// Small frames, encoded, sent before any message's.
    pub frames: VecDeque<Vec<u8>>,
    /// Messages not yet begun on the connection, oldest first.
    pub msgs: VecDeque<OutMsg>,
    pub bytes: usize,
    /// What wakes the connection's task from its wait on the socket, while
    /// it runs: whoever queues something here, once out of the lock.
    pub waker: Option<kcore::tcp::TcpWaker>,
}

impl Out {
    fn new() -> Out {
        Out { number: 0, frames: VecDeque::new(), msgs: VecDeque::new(), bytes: 0, waker: None }
    }
}

/// A peer's session, as this host has it.
pub struct State {
    /// The session its connections are of: 0 for none.
    pub session: u64,
    /// Which of the session's connections have said their Hello.
    pub attached: [bool; CLASSES],
    /// Its number in the channel, while it is up: 0 otherwise.
    pub number: u16,
    pub up_since_ns: u64,
    /// The session's last state change, for `ipcnet`: up, or why lost.
    pub said: &'static str,
    /// Monitors this host's router holds for the peer: the peer's id of
    /// each, and the router's.
    pub monitors: Vec<(u64, u64)>,
}

/// A peer's counts, for `ipcnet`.
#[derive(Default)]
pub struct Counts {
    pub ups: AtomicU64,
    pub losses: AtomicU64,
    pub sent: AtomicU64,
    pub received: AtomicU64,
    pub bytes_out: AtomicU64,
    pub bytes_in: AtomicU64,
    /// Bounces this host sent the peer, and those it sent this host.
    pub bounced_out: AtomicU64,
    pub bounced_in: AtomicU64,
    /// Messages it may not deliver here -- a Host name, a mailbox that never
    /// sent there -- dropped.
    pub refused: AtomicU64,
    /// Messages lost with a connection part way or just sent; frames
    /// dropped past FRAMES_MAX.
    pub dropped: AtomicU64,
}

pub fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

pub struct Peer {
    pub conf: PeerConf,
    /// This host dials it: of the two names, this host's sorts first.
    pub dials: bool,
    /// The TLS clients this host dials it with, by class -- each offering
    /// its class's protocol, both pinned to the peer's keys.
    pub clients: [tls::pinned::Client; CLASSES],
    pub st: Locked<State>,
    pub out: [Locked<Out>; CLASSES],
    /// Its number while up, as the router looks it up -- with no lock; the
    /// queues' own `number` is what a message is held to.
    pub number_now: AtomicU16,
    /// Taken off hosts.toml: its dialer stops, and nothing more is done of
    /// it.
    pub removed: AtomicBool,
    pub counts: Counts,
}

/// Why a connection was not let into its peer's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The session is no longer the peer's.
    Stale,
    /// Its class is attached already.
    Twice,
}

impl Peer {
    pub fn new(conf: PeerConf, dials: bool, clients: [tls::pinned::Client; CLASSES]) -> Option<Peer> {
        Some(Peer {
            conf,
            dials,
            clients,
            st: Locked::new(State { session: 0, attached: [false; CLASSES], number: 0, up_since_ns: 0, said: "new",
                                    monitors: Vec::new() })?,
            out: [Locked::new(Out::new())?, Locked::new(Out::new())?],
            number_now: AtomicU16::new(0),
            removed: AtomicBool::new(false),
            counts: Counts::default(),
        })
    }

    /// A session begun by this host, dialing: only while it has none.
    pub fn begin(&self, session: u64) -> bool {
        self.st.with(|st| {
            if st.session != 0 {
                return false;
            }
            st.session = session;
            st.attached = [false; CLASSES];
            st.said = "dialing";
            true
        })
    }

    /// Whether `session` is still the peer's.
    pub fn current(&self, session: u64) -> bool {
        self.st.with(|st| st.session == session && session != 0)
    }

    /// The peer's number, while `session` is its and up.
    pub fn number(&self, session: u64) -> Option<u16> {
        self.st.with(|st| (st.session == session && st.number != 0).then_some(st.number))
    }

    /// The connection of class `class` of `session` has said its Hello: let
    /// into the session -- which, accepted, may be a new one that ends the
    /// peer's old -- and, the second of two, the peer is up.
    pub fn attach(&self, router: &Mailbox, class: usize, session: u64, accepted: bool) -> Result<(), Refusal> {
        let mut lost = None;
        let up = self.st.with(|st| {
            if st.session != session {
                if !accepted || session == 0 {
                    return Err(Refusal::Stale);
                }
                /* A new session from the host that dials: its old one, up or
                 * half made, is over */
                if st.session != 0 {
                    lost = Some(self.end(st, "replaced by a new session"));
                }
                st.session = session;
                st.attached = [false; CLASSES];
                st.said = "connecting";
            }
            if st.attached[class] {
                return Err(Refusal::Twice);
            }
            st.attached[class] = true;
            if !st.attached.iter().all(|&a| a) {
                return Ok(false);
            }
            match router.peer_up(&self.conf.name) {
                Ok(number) => {
                    st.number = number;
                    st.up_since_ns = kcore::time::boot_time_ns();
                    st.said = "up";
                    for o in &self.out {
                        o.with(|o| o.number = number);
                    }
                    self.number_now.store(number, Ordering::Release);
                    Ok(true)
                }
                Err(_) => {
                    /* The channel would not have it -- stopping, its table
                     * full: the session is over before it began */
                    lost = Some(self.end(st, "refused by the channel"));
                    Ok(false)
                }
            }
        });
        if let Some(l) = lost {
            l.finish(router, self);
        }
        if up == Ok(true) {
            add(&self.counts.ups, 1);
            kcore::klog!(0, "ipcnet: {} ({}) up", self.conf.name, super::files::Ip(self.conf.ip));
        }
        up.map(|_| ())
    }

    /// `session` lost, for `why` -- once: a session already ended, or
    /// another's, is left alone.
    pub fn lose(&self, router: &Mailbox, session: u64, why: &'static str) {
        let lost = self.st.with(|st| (st.session == session && session != 0).then(|| self.end(st, why)));
        if let Some(l) = lost {
            l.finish(router, self);
        }
    }

    /// The session ended, under the state's lock: what is left to do of it
    /// out of the lock.
    fn end(&self, st: &mut State, why: &'static str) -> Lost {
        let was_up = st.number;
        st.session = 0;
        st.attached = [false; CLASSES];
        st.number = 0;
        self.number_now.store(0, Ordering::Release);
        st.said = why;
        let monitors = core::mem::take(&mut st.monitors);
        let mut unsent = Vec::new();
        for o in &self.out {
            o.with(|o| {
                o.number = 0;
                o.frames.clear();
                o.bytes = 0;
                unsent.extend(o.msgs.drain(..));
            });
        }
        if was_up != 0 {
            add(&self.counts.losses, 1);
        }
        Lost { number: was_up, why, unsent, monitors }
    }
}

/// What ending a session leaves to do once its lock is let go.
struct Lost {
    number: u16,
    why: &'static str,
    unsent: Vec<OutMsg>,
    monitors: Vec<(u64, u64)>,
}

impl Lost {
    /// Every message that never went bounced to its sender -- while the
    /// peer is still up in the channel, which a bounce needs -- then the
    /// channel told, and the monitors held for the peer taken back.
    fn finish(self, router: &Mailbox, peer: &Peer) {
        if self.number != 0 {
            for m in &self.unsent {
                let to = match &m.to {
                    Dest::Addr(a) => RemoteTo::Addr(Addr(*a)),
                    Dest::Name(n) => match core::str::from_utf8(n) {
                        Ok(n) => RemoteTo::Name(n),
                        Err(_) => continue,
                    },
                };
                if router.bounced(self.number, Addr(m.from), to, m.tag, m.reference, Bounce::Unsent).is_ok() {
                    add(&peer.counts.bounced_out, 1);
                }
            }
            let _ = router.peer_lost(self.number);
            kcore::klog!(0, "ipcnet: {} lost: {}", peer.conf.name, self.why);
        }
        for (_, local) in self.monitors {
            router.demonitor(local);
        }
    }
}

/// A peer's name, as `ipcnet` lists it with its state.
pub fn describe(p: &Peer) -> (String, &'static str, u16, u64) {
    p.st.with(|st| (p.conf.name.clone(), st.said, st.number, st.up_since_ns))
}
