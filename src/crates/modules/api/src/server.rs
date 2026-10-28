//! The running server: a listener task on each interface it was told to
//! listen on, and a task for each connection it takes.
//!
//! An interface may not be there yet -- hv0 is made the first time the hv
//! module loads -- so its listener looks for it once a second, and binds
//! once it is there; a net device is never taken back, so the listener stays
//! good through hv's unloads and loads after. What is taken is decided
//! before anything else is done for it: an address its interface's
//! allowlist does not have, or a connection past the limits -- in all, or in
//! its handshake -- is reset, before a byte of TLS.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::net::Nic;
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use kcore::tcp::{TcpListener, TcpStream};
use tls::server::Fingerprint;

use super::config::Config;
use super::files::{Client, Dir, Identity, Role};

/// Connections the kernel keeps for the port before the listener takes
/// them: it takes each within a poll, and resets what is past its limits.
const LISTEN_BACKLOG: usize = 4;
/// How long a listener waits for a connection at a time: how soon it sees a
/// stop.
pub const POLL_MS: u64 = 250;
/// How often a listener looks for an interface that is not there yet.
const NIC_WAIT_MS: u64 = 1000;

/// A value behind a kernel mutex, which sleeps: never held while waiting for
/// a task, nor taken by anything such a task could be waiting in.
pub struct Locked<T>(Mutex<T>);

impl<T> Locked<T> {
    pub fn new(value: T) -> Option<Self> {
        Some(Self(Mutex::new(value)?))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock())
    }
}

/// Counts of what the server has done, for `api status` -- and for a test
/// that has to see a stranger refused before a byte of HTTP was read.
#[derive(Default)]
pub struct Counters {
    pub accepted: AtomicU64,
    /// Reset at the door: an address the allowlist does not have.
    pub not_allowed: AtomicU64,
    /// Reset at the door, or after the handshake: past a limit.
    pub busy: AtomicU64,
    /// Refused in the handshake: an unknown key, none, TLS gone wrong.
    pub refused: AtomicU64,
    pub unknown_key: AtomicU64,
    /// Requests read whole and answered.
    pub requests: AtomicU64,
    pub streams: AtomicU64,
    /// A handshake's time, the last and the slowest, in microseconds.
    pub handshake_us_last: AtomicU64,
    pub handshake_us_max: AtomicU64,
    pub handshakes: AtomicU64,
    pub handshake_us_total: AtomicU64,
}

impl Counters {
    pub fn add(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Where a connection is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Handshake,
    Open,
}

/// A connection, as the server keeps count of it.
pub struct Conn {
    pub id: u64,
    pub nic: String,
    pub ip: u32,
    pub port: u16,
    pub phase: Phase,
    /// Once it is open: whose key, and who that is.
    pub key: Option<Fingerprint>,
    pub name: String,
    pub role: Option<Role>,
    pub since_ns: u64,
}

/// What a listener is doing, for `api status`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Listening {
    /// Its interface is not there yet.
    Waiting,
    Bound,
    /// The port is taken, or no TCP slot is free: tried again in a second.
    Failed,
}

pub struct Shared {
    pub stop: AtomicBool,
    pub config: Config,
    pub dir: Dir,
    pub identity: Identity,
    /// The TLS server, made again whole when the clients change: a
    /// connection keeps the one it was taken with.
    pub tls: Locked<Arc<tls::server::Server>>,
    pub clients: Locked<Vec<Client>>,
    pub conns: Locked<Vec<Conn>>,
    pub listeners: Locked<Vec<(String, Listening)>>,
    pub next_id: AtomicU64,
    pub counters: Counters,
    /// Held through each line the audit log is given
    pub audit: Locked<()>,
    pub started_ns: u64,
}

/// What the API speaks by ALPN, in the order it prefers them.
pub const ALPN: [&[u8]; 2] = [b"nos-api/1", b"http/1.1"];

/// The TLS server for `clients`' keys.
pub fn tls_server(identity: &Identity, clients: &[Client]) -> Result<tls::server::Server, String> {
    let keys: Vec<Fingerprint> = clients.iter().map(|c| c.key).collect();
    tls::server::Server::new(&identity.cert, &identity.key, &keys, &ALPN)
        .map_err(|e| alloc::format!("the TLS server could not be made: {:?}", e))
}

impl Shared {
    /// The client whose key `key` is, as the list stands now -- None for a
    /// key taken off it since the connection's server was made.
    pub fn client(&self, key: &Fingerprint) -> Option<Client> {
        self.clients.with(|v| v.iter().find(|c| c.key == *key).cloned())
    }

    /// A new list of clients, and the TLS server for it.
    pub fn set_clients(&self, clients: Vec<Client>) -> Result<usize, String> {
        let server = Arc::new(tls_server(&self.identity, &clients)?);
        let count = clients.len();
        self.clients.with(|v| *v = clients);
        let old = self.tls.with(|t| core::mem::replace(t, server));
        /* Dropped out of the lock: the last connection with it may be gone */
        drop(old);
        Ok(count)
    }
}

/// A listener's context: the interface it waits for and serves.
pub struct ListenerCtx {
    pub shared: Arc<Shared>,
    pub nic: String,
}

/// A connection's task, what it is started with.
pub struct ConnCtx {
    pub shared: Arc<Shared>,
    pub stream: TcpStream,
    pub id: u64,
}

/// A listener: wait for the interface, bind, take connections until the
/// stop -- each connection's task joined before it returns.
pub fn listener_main(ctx: ListenerCtx) {
    let ListenerCtx { shared, nic } = ctx;
    let set = |state: Listening| {
        shared.listeners.with(|v| {
            if let Some(e) = v.iter_mut().find(|(n, _)| *n == nic) {
                e.1 = state;
            }
        })
    };
    let mut tasks: Vec<(u64, TaskHandle)> = Vec::new();
    let mut listener: Option<TcpListener> = None;
    while !shared.stop.load(Ordering::Acquire) {
        reap(&shared, &mut tasks);
        let Some(l) = listener.as_ref() else {
            match Nic::find(&nic) {
                None => {
                    set(Listening::Waiting);
                }
                Some(device) => match TcpListener::bind(&device, shared.config.port, LISTEN_BACKLOG) {
                    Ok(l) => {
                        kcore::trace!(0, "api: listening on {} port {}", nic, shared.config.port);
                        listener = Some(l);
                        set(Listening::Bound);
                        continue;
                    }
                    Err(_) => set(Listening::Failed),
                },
            }
            sleep_unless_stopped(&shared, NIC_WAIT_MS);
            continue;
        };
        let Some(stream) = l.accept(POLL_MS) else { continue };
        if let Some(task) = admit(&shared, stream, &nic) {
            tasks.push(task);
        }
    }
    /* No connection is taken after the stop: the listener goes first, then
     * every connection's task is waited for -- each sees the stop within a
     * read's wait, and resets its connection */
    drop(listener);
    drop(tasks);
}

fn sleep_unless_stopped(shared: &Shared, ms: u64) {
    let mut left = ms;
    while left > 0 && !shared.stop.load(Ordering::Acquire) {
        let step = left.min(POLL_MS);
        kcore::task::sleep_ms(step);
        left -= step;
    }
}

/// Connections' tasks that are done, joined: their entries are gone.
fn reap(shared: &Shared, tasks: &mut Vec<(u64, TaskHandle)>) {
    let live: Vec<u64> = shared.conns.with(|v| v.iter().map(|c| c.id).collect());
    let (done, keep): (Vec<_>, Vec<_>) = core::mem::take(tasks).into_iter().partition(|(id, _)| !live.contains(id));
    *tasks = keep;
    /* Their tasks have taken themselves off the list, and are ending: the
     * join is short, and out of any lock */
    drop(done);
}

/// A connection taken -- or reset, if its address or the limits say so --
/// and its task started.
fn admit(shared: &Arc<Shared>, stream: TcpStream, nic: &str) -> Option<(u64, TaskHandle)> {
    let (ip, port) = stream.peer();
    if !shared.config.allows(nic, ip) {
        Counters::add(&shared.counters.not_allowed);
        stream.abort();
        return None;
    }
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let max = shared.config.max_connections;
    let max_hs = shared.config.max_handshakes;
    let taken = shared.conns.with(|v| {
        let handshaking = v.iter().filter(|c| c.phase == Phase::Handshake).count();
        if v.len() >= max || handshaking >= max_hs || v.try_reserve(1).is_err() {
            return false;
        }
        v.push(Conn { id, nic: String::from(nic), ip, port, phase: Phase::Handshake, key: None, name: String::new(),
                      role: None, since_ns: kcore::time::boot_time_ns() });
        true
    });
    if !taken {
        Counters::add(&shared.counters.busy);
        stream.abort();
        return None;
    }
    Counters::add(&shared.counters.accepted);
    let ctx = ConnCtx { shared: shared.clone(), stream, id };
    match kcore::task::spawn_with("api/conn", ctx, super::http::conn_main) {
        Some(task) => Some((id, task)),
        None => {
            /* The context, and the connection in it, went with the failed
             * spawn: only its entry is left */
            shared.conns.with(|v| v.retain(|c| c.id != id));
            None
        }
    }
}

/// The server's listeners started: one task an interface.
pub fn start_listeners(shared: &Arc<Shared>) -> Result<Vec<TaskHandle>, String> {
    let mut tasks = Vec::new();
    for nic in &shared.config.listen {
        shared.listeners.with(|v| v.push((nic.clone(), Listening::Waiting)));
        let ctx = ListenerCtx { shared: shared.clone(), nic: nic.clone() };
        match kcore::task::spawn_with("api/listen", ctx, listener_main) {
            Some(t) => tasks.push(t),
            None => {
                shared.stop.store(true, Ordering::Release);
                drop(tasks);
                return Err(String::from("could not start a listener's task"));
            }
        }
    }
    Ok(tasks)
}

/// The server as the module holds it: dropped, it stops -- the listeners
/// and every connection waited out.
pub struct Server {
    pub shared: Arc<Shared>,
    pub listeners: Vec<TaskHandle>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let tasks = core::mem::take(&mut self.listeners);
        drop(tasks);
    }
}
