//! What the tunnel (`modules/hypervisor/hv/src/tunnel.rs`) reaches of the
//! kernel besides the switch, as the `tunnel` target has it: each host's
//! uplink -- its address and MAC, the room it says it has, what was sent
//! out of it, its listener on the tunnel's port, the next hops ARP finds --
//! and `kcore::ipc`, the hosts' mailboxes, whose messages the target carries
//! between the hosts itself. `kcore::net::Nic` (network.rs) reaches an
//! uplink by its handle; everything is reset for each input.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::net::UdpHandler;

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the uplinks' handles begin: past any device of the stack's.
const BASE: usize = 0x7E00_0000;

/// A host's uplink.
pub struct Uplink {
    pub ip: u32,
    pub mac: [u8; 6],
    /// What `tx_room` says; a transmit with none is refused.
    pub room: AtomicUsize,
    /// What was sent out of it, for the target to put on the wire.
    pub sent: Mutex<Vec<Vec<u8>>>,
    /// Its listener: the port, the listener's own number -- so that one
    /// dropped late takes away no listener after it -- and the handler a
    /// datagram is lent to.
    pub listener: Mutex<Option<(u16, u64, Arc<dyn UdpHandler>)>>,
}

static UPLINKS: Mutex<Vec<Arc<Uplink>>> = Mutex::new(Vec::new());
/// The listeners' numbers.
static LISTENERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_listener() -> u64 {
    LISTENERS.fetch_add(1, Ordering::Relaxed)
}
/// The next hops ARP answers for: an address, and its MAC.
static HOPS: Mutex<Vec<(u32, [u8; 6])>> = Mutex::new(Vec::new());
/// The frame pool, dry.
static DRY: AtomicBool = AtomicBool::new(false);

/// Every uplink gone, every hop forgotten, the pool full: an input's start.
pub fn reset() {
    let gone = core::mem::take(&mut *locked(&UPLINKS));
    drop(gone);
    locked(&HOPS).clear();
    DRY.store(false, Ordering::Relaxed);
    ipc::reset();
}

/// A new uplink at `ip` and `mac`: its handle, for `kcore::net::Nic`.
pub fn add(ip: u32, mac: [u8; 6]) -> usize {
    let mut all = locked(&UPLINKS);
    all.push(Arc::new(Uplink {
        ip, mac, room: AtomicUsize::new(256), sent: Mutex::new(Vec::new()), listener: Mutex::new(None),
    }));
    BASE + all.len() - 1
}

/// The uplink a handle names, if it names one.
pub fn get(handle: usize) -> Option<Arc<Uplink>> {
    let i = handle.checked_sub(BASE)?;
    locked(&UPLINKS).get(i).cloned()
}

/// What ARP answers for `ip`: its MAC, or nothing -- where `known` is false,
/// it is forgotten.
pub fn set_hop(ip: u32, mac: [u8; 6], known: bool) {
    let mut hops = locked(&HOPS);
    hops.retain(|(a, _)| *a != ip);
    if known {
        hops.push((ip, mac));
    }
}

pub fn hop(ip: u32) -> Option<[u8; 6]> {
    locked(&HOPS).iter().find(|(a, _)| *a == ip).map(|(_, m)| *m)
}

pub fn set_dry(dry: bool) {
    DRY.store(dry, Ordering::Relaxed);
}

pub fn dry() -> bool {
    DRY.load(Ordering::Relaxed)
}

/// What was sent out of the uplink `handle` names since last asked.
pub fn take_sent(handle: usize) -> Vec<Vec<u8>> {
    get(handle).map(|u| core::mem::take(&mut *locked(&u.sent))).unwrap_or_default()
}

/// The handler listening on `port` of the uplink `handle` names.
pub fn listener(handle: usize, port: u16) -> Option<Arc<dyn UdpHandler>> {
    let u = get(handle)?;
    let l = locked(&u.listener);
    l.as_ref().filter(|(p, _, _)| *p == port).map(|(_, _, h)| h.clone())
}

/// The uplink's listener taken away, as a host that died takes it.
pub fn kill_listener(handle: usize) {
    if let Some(u) = get(handle) {
        let gone = locked(&u.listener).take();
        drop(gone);
    }
}

/// `kcore::events` as the tunnel's task reaches it: an event signalled from
/// anywhere -- `hv0`'s sink among them -- which the target looks at.
pub mod events {
    use std::sync::atomic::{AtomicBool, Ordering};

    pub struct SharedEvent {
        signalled: AtomicBool,
    }

    impl SharedEvent {
        pub fn new() -> Option<SharedEvent> {
            Some(SharedEvent { signalled: AtomicBool::new(false) })
        }
        pub fn signal(&self) {
            self.signalled.store(true, Ordering::Relaxed);
        }
        pub fn wait_for(&self, _timeout: crate::time::Duration) -> bool {
            self.signalled.swap(false, Ordering::Relaxed)
        }
    }
}

/// `kcore::ipc` as the tunnel's task reaches it: a mailbox is the host's
/// whose turn it is (`set_current`) as it is made, and what it sends is kept
/// for the target to carry -- or not -- to the host it names.
pub mod ipc {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use crate::time::Duration;

    fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Addr(pub u64);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Scope {
        Host,
        Cluster,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Limits {
        pub messages: u32,
        pub bytes: usize,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Error {
        Full,
        NoRoute,
    }

    impl Error {
        pub fn name(self) -> &'static str {
            match self {
                Error::Full => "full",
                Error::NoRoute => "no route",
            }
        }
    }

    pub const TAG_SYSTEM: u32 = 0xFFFF_0000;

    pub struct Message {
        pub from: Addr,
        pub tag: u32,
        pub reference: u64,
        pub data: Vec<u8>,
    }

    impl Message {
        pub fn is_system(&self) -> bool {
            self.tag >= TAG_SYSTEM
        }
    }

    /// The host whose code runs now: what a mailbox made is that host's.
    static CURRENT: AtomicUsize = AtomicUsize::new(0);
    /// A send refused, as a full mailbox or a link down refuses one.
    static REFUSE: AtomicBool = AtomicBool::new(false);
    /// What was sent: the sending host, the name, the tag, the bytes.
    static SENT: Mutex<Vec<(usize, String, u32, Vec<u8>)>> = Mutex::new(Vec::new());

    pub(super) fn reset() {
        CURRENT.store(0, Ordering::Relaxed);
        REFUSE.store(false, Ordering::Relaxed);
        locked(&SENT).clear();
    }

    pub fn set_current(host: usize) {
        CURRENT.store(host, Ordering::Relaxed);
    }

    pub fn current() -> usize {
        CURRENT.load(Ordering::Relaxed)
    }

    pub fn set_refuse(refuse: bool) {
        REFUSE.store(refuse, Ordering::Relaxed);
    }

    pub fn take_sent() -> Vec<(usize, String, u32, Vec<u8>)> {
        core::mem::take(&mut *locked(&SENT))
    }

    pub struct Mailbox {
        host: usize,
    }

    impl Mailbox {
        pub fn new(_limits: Limits) -> Result<Mailbox, Error> {
            Ok(Mailbox { host: CURRENT.load(Ordering::Relaxed) })
        }

        pub fn new_on(limits: Limits, _event: &crate::events::SharedEvent) -> Result<Mailbox, Error> {
            Self::new(limits)
        }

        pub fn register(&self, _name: &str, _scope: Scope) -> Result<(), Error> {
            Ok(())
        }

        pub fn send_to(&self, name: &str, tag: u32, _reference: u64, data: &[u8]) -> Result<(), Error> {
            if REFUSE.load(Ordering::Relaxed) {
                return Err(Error::Full);
            }
            locked(&SENT).push((self.host, String::from(name), tag, data.to_vec()));
            Ok(())
        }

        /// Nothing comes by the task's own wait: the target hands each
        /// message to the tunnel itself.
        pub fn recv(&self, _timeout: Duration) -> Option<Message> {
            None
        }
    }

    pub fn peer_name(_addr: Addr) -> Option<String> {
        None
    }
}
