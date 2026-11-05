//! The running server: the router's mailbox and its task, a listener on
//! ipcnet's port that answers the peers' addresses alone (TCP's allowlist:
//! to anyone else the port is not there), a dialer for each peer this host
//! dials -- the one whose name sorts after its own -- and a task for each
//! connection, either way.
//!
//! What it costs a stranger, and a flood: nothing past its packets until
//! its address is a peer's; then at most two TLS handshakes at once, one a
//! source, each given HANDSHAKE_MS; then a key the peer is pinned by, the
//! name the key is pinned under and this cluster's id, or a reset.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::ipc::{Limits, Mailbox};
use kcore::net::Nic;
use kcore::task::TaskHandle;
use kcore::tcp::TcpListener;
use tls::pinned::{Client, Fingerprint, Suite};

use super::files::{self, Config, Dir, Identity, PeerConf};
use super::link::{link_main, LinkCtx, Side, ALPN, HANDSHAKE_MS};
use super::peer::{add, Locked, Peer, CONTROL, BULK};

/// What waits to go out to every host, at most: the router mailbox's own
/// limits, the channel's largest -- a send past them is `Full`.
const ROUTER_LIMITS: Limits = Limits { messages: 4096, bytes: 16 << 20 };
/// The cipher suites a link asks for, the first first: ChaCha20-Poly1305
/// carries a 1 GbE link with room to spare on these CPUs' software
/// backends, AES-GCM at under half of it (plans/ipc.md, measured).
pub const SUITES: [Suite; 3] = [Suite::ChaCha20Poly1305, Suite::Aes128Gcm, Suite::Aes256Gcm];
/// TLS handshakes under way at once, at most -- one a source.
const HANDSHAKES_MAX: usize = 2;
/// Connections the kernel holds for the port before the listener takes
/// them.
const BACKLOG: usize = 2;
/// Connections' tasks the listener keeps at once: two a peer, and those in
/// their handshake or their Hello.
const ACCEPTED_MAX: usize = 2 * files::PEERS_MAX + 8;
/// How long a listener waits for a connection at a time, and a dialer
/// between looks at its session: how soon a stop is seen.
const POLL_MS: u64 = 100;
/// A dialer's wait between sessions: from this, doubling while none comes
/// up, to the most.
const BACKOFF_MIN_MS: u64 = 250;
const BACKOFF_MAX_MS: u64 = 8000;
/// How long the bulk connection waits for the control one to have said its
/// Hello: its handshake, its Hello and a margin.
const CONTROL_FIRST_MS: u64 = 2 * HANDSHAKE_MS + 1000;
/// How often the listener's bind is tried again when it fails.
const BIND_RETRY_MS: u64 = 1000;
const NS_PER_MS: u64 = 1_000_000;

/// What the server counts, for `ipcnet`.
#[derive(Default)]
pub struct Counters {
    /// Connections the listener took, and those it reset at the door:
    /// handshakes enough under way, or from that address already.
    pub accepted: AtomicU64,
    pub busy: AtomicU64,
    /// Handshakes refused: of a connection taken, and of one dialed.
    pub refused_in: AtomicU64,
    pub refused_out: AtomicU64,
    /// Hellos not this cluster's, or not the key's host.
    pub hello_refused: AtomicU64,
    /// Frames the wire refused, or in no place a frame of theirs may be.
    pub wire_refused: AtomicU64,
    /// What the router could not read, or had no peer for.
    pub bad_route: AtomicU64,
    pub unrouted: AtomicU64,
    pub stray: AtomicU64,
}

pub struct Shared {
    pub stop: AtomicBool,
    pub cfg: Config,
    pub nic: Nic,
    pub identity: Identity,
    /// The router: what the channel hands everything for another host.
    pub router: Mailbox,
    pub peers: Locked<Vec<Arc<Peer>>>,
    /// The TLS server, pinned to every peer's keys: made again whole when
    /// they change, a connection keeping the one it was taken with.
    pub server: Locked<Arc<tls::pinned::Server>>,
    /// The addresses with a handshake under way.
    handshakes: Locked<Vec<u32>>,
    /// The listener's allowlist is to be the peers' again.
    nets_changed: AtomicBool,
    pub counters: Counters,
    pub started_ns: u64,
}

impl Shared {
    /// A handshake from `ip` let begin: refused past HANDSHAKES_MAX, or
    /// while `ip` has one under way.
    fn handshake_begin(&self, ip: u32) -> bool {
        self.handshakes.with(|v| {
            if v.len() >= HANDSHAKES_MAX || v.contains(&ip) || v.try_reserve(1).is_err() {
                return false;
            }
            v.push(ip);
            true
        })
    }

    pub fn handshake_done(&self, ip: u32) {
        self.handshakes.with(|v| {
            if let Some(i) = v.iter().position(|&a| a == ip) {
                v.swap_remove(i);
            }
        });
    }

    fn nets(&self) -> Vec<(u32, u8)> {
        self.peers.with(|v| v.iter().map(|p| (p.conf.ip, 32)).collect())
    }
}

/// The TLS server for `peers`' keys.
fn tls_server(identity: &Identity, peers: &[PeerConf]) -> Result<tls::pinned::Server, String> {
    let keys: Vec<Fingerprint> = peers.iter().flat_map(|p| p.keys.iter().copied()).collect();
    tls::pinned::Server::with_suites(&identity.cert, &identity.key, &keys, &ALPN, &SUITES)
        .map_err(|e| alloc::format!("the TLS server could not be made: {:?}", e))
}

fn make_peer(identity: &Identity, me: &str, conf: PeerConf) -> Result<Peer, String> {
    let client = |class: usize| {
        Client::with_suites(&identity.cert, &identity.key, &conf.keys, &[ALPN[class]], &SUITES)
            .map_err(|e| alloc::format!("{}'s TLS client could not be made: {:?}", conf.name, e))
    };
    let clients = [client(CONTROL)?, client(BULK)?];
    /* Of the two names, the one that sorts first dials */
    let dials = me < conf.name.as_str();
    Peer::new(conf, dials, clients).ok_or_else(|| String::from("out of memory"))
}

/// The server as the module holds it: dropped, it stops -- every peer lost,
/// every task waited out.
pub struct Server {
    pub shared: Arc<Shared>,
    tasks: Vec<TaskHandle>,
    dialers: Locked<Vec<(String, TaskHandle)>>,
}

impl Server {
    /// Started as hosts.toml in `dir` says, the host's identity made first
    /// if it has none.
    pub fn start(dir: &Dir, cfg: Config) -> Result<Box<Server>, String> {
        let nic = Nic::find(&cfg.nic).ok_or_else(|| alloc::format!("there is no interface {}", cfg.nic))?;
        let (identity, made) = files::identity(dir, &cfg.name)?;
        if made {
            kcore::klog!(0, "ipcnet: a key made for {}: {}", cfg.name, files::hex(&identity.fingerprint));
        }
        let router = Mailbox::new(ROUTER_LIMITS).map_err(|_| String::from("no mailbox for the router"))?;
        router.route().map_err(|e| alloc::format!("not the router: {} -- is another ipcnet running?", e.name()))?;
        let server = tls_server(&identity, &cfg.peers)?;
        let mut peers = Vec::new();
        for p in &cfg.peers {
            peers.push(Arc::new(make_peer(&identity, &cfg.name, p.clone())?));
        }
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            nic,
            identity,
            router,
            peers: Locked::new(peers).ok_or_else(|| String::from("out of memory"))?,
            server: Locked::new(Arc::new(server)).ok_or_else(|| String::from("out of memory"))?,
            handshakes: Locked::new(Vec::new()).ok_or_else(|| String::from("out of memory"))?,
            nets_changed: AtomicBool::new(false),
            counters: Counters::default(),
            started_ns: kcore::time::boot_time_ns(),
            cfg,
        });
        let mut s = Box::new(Server {
            shared: shared.clone(),
            tasks: Vec::new(),
            dialers: Locked::new(Vec::new()).ok_or_else(|| String::from("out of memory"))?,
        });
        /* From here on a failure drops `s`, which stops what was started */
        let router_task = kcore::task::spawn_with("ipcnet/router", shared.clone(), super::router::router_main)
            .ok_or_else(|| String::from("could not start the router's task"))?;
        s.tasks.push(router_task);
        let listener = kcore::task::spawn_with("ipcnet/listen", shared.clone(), listener_main)
            .ok_or_else(|| String::from("could not start the listener's task"))?;
        s.tasks.push(listener);
        for p in shared.peers.with(|v| v.clone()) {
            s.dial(&p)?;
        }
        kcore::klog!(0, "ipcnet: {} in cluster {}, on {} port {}, {} peers", shared.cfg.name,
                     files::hex(&shared.cfg.cluster), shared.cfg.nic, shared.cfg.port,
                     shared.peers.with(|v| v.len()));
        Ok(s)
    }

    /// A dialer for `p`, if this host dials it.
    fn dial(&self, p: &Arc<Peer>) -> Result<(), String> {
        if !p.dials {
            return Ok(());
        }
        let t = kcore::task::spawn_with("ipcnet/dial", (self.shared.clone(), p.clone()), dialer_main)
            .ok_or_else(|| alloc::format!("could not start {}'s dialer", p.conf.name))?;
        self.dialers.with(|v| v.push((p.conf.name.clone(), t)));
        Ok(())
    }

    /// The peers made `peers`: one taken off lost and its dialer waited
    /// out, one added made and dialed, one whose address or keys changed
    /// both -- and the listener's allowlist and the TLS server's keys made
    /// theirs.
    pub fn set_peers(&self, peers: &[PeerConf]) -> Result<(), String> {
        let sh = &self.shared;
        let server = Arc::new(tls_server(&sh.identity, peers)?);
        let old: Vec<Arc<Peer>> = sh.peers.with(|v| v.clone());
        let gone: Vec<Arc<Peer>> = old.iter().filter(|p| !peers.contains(&p.conf)).cloned().collect();
        let mut new = Vec::new();
        for c in peers.iter().filter(|c| !old.iter().any(|p| p.conf == **c)) {
            new.push(Arc::new(make_peer(&sh.identity, &sh.cfg.name, c.clone())?));
        }
        for p in &gone {
            p.removed.store(true, Ordering::Release);
            let session = p.st.with(|st| st.session);
            p.lose(&sh.router, session, "taken off hosts.toml");
        }
        sh.peers.with(|v| {
            v.retain(|p| !p.removed.load(Ordering::Acquire));
            v.extend(new.iter().cloned());
        });
        let old_server = sh.server.with(|s| core::mem::replace(s, server));
        drop(old_server);
        sh.nets_changed.store(true, Ordering::Release);
        /* The dialers of those gone, waited out of the lock */
        let names: Vec<&str> = gone.iter().map(|p| p.conf.name.as_str()).collect();
        let stopped: Vec<(String, TaskHandle)> = self.dialers.with(|v| {
            let (stop, keep): (Vec<_>, Vec<_>) = core::mem::take(v).into_iter().partition(|(n, _)| names.contains(&n.as_str()));
            *v = keep;
            stop
        });
        drop(stopped);
        for p in &new {
            self.dial(p)?;
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.router.wake();
        /* The dialers -- each waiting out its connections -- then the
         * listener with those it took, then the router: out of every lock */
        let dialers = self.dialers.with(core::mem::take);
        drop(dialers);
        let tasks = core::mem::take(&mut self.tasks);
        drop(tasks);
    }
}

fn sleep_unless_stopped(shared: &Shared, peer: Option<&Peer>, ms: u64) {
    let until = kcore::time::boot_time_ns() + ms * NS_PER_MS;
    while kcore::time::boot_time_ns() < until && !shared.stop.load(Ordering::Acquire)
        && !peer.is_some_and(|p| p.removed.load(Ordering::Acquire)) {
        kcore::task::sleep_ms(POLL_MS.min(ms));
    }
}

/// A connection's task started; None when it could not be, the context --
/// and the connection in it -- gone with the failed spawn.
fn spawn_link(ctx: LinkCtx) -> Option<TaskHandle> {
    kcore::task::spawn_with("ipcnet/link", ctx, link_main)
}

/// The listener: bound with the peers' addresses as its allowlist, taking
/// connections until the stop -- each one's task waited out before it
/// returns.
fn listener_main(shared: Arc<Shared>) {
    let mut tasks: Vec<(Arc<AtomicBool>, TaskHandle)> = Vec::new();
    let listener = loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        match TcpListener::bind_allowed(&shared.nic, shared.cfg.port, BACKLOG, true, &shared.nets()) {
            Ok(l) => break l,
            Err(_) => {
                kcore::klog!(0, "ipcnet: port {} on {} not to be had -- tried again in a second", shared.cfg.port,
                             shared.cfg.nic);
                sleep_unless_stopped(&shared, None, BIND_RETRY_MS);
            }
        }
    };
    while !shared.stop.load(Ordering::Acquire) {
        if shared.nets_changed.swap(false, Ordering::AcqRel) && listener.set_allowed(Some(&shared.nets())).is_err() {
            kcore::klog!(0, "ipcnet: the listener's allowlist could not be changed");
        }
        /* Tasks done, joined: their join is short, and out of any lock */
        let (done, keep): (Vec<_>, Vec<_>) = core::mem::take(&mut tasks).into_iter()
            .partition(|(d, _)| d.load(Ordering::Acquire));
        tasks = keep;
        drop(done);
        let Some(stream) = listener.accept(POLL_MS) else { continue };
        let (ip, _) = stream.peer();
        if tasks.len() >= ACCEPTED_MAX || !shared.handshake_begin(ip) {
            add(&shared.counters.busy, 1);
            stream.abort();
            continue;
        }
        add(&shared.counters.accepted, 1);
        let done = Arc::new(AtomicBool::new(false));
        let ctx = LinkCtx { shared: shared.clone(), side: Side::Accepted { stream, ip }, done: Some(done.clone()) };
        match spawn_link(ctx) {
            Some(t) => tasks.push((done, t)),
            None => shared.handshake_done(ip),
        }
    }
    /* No connection is taken after the stop: the listener goes first, then
     * every connection's task is waited for -- each sees the stop within a
     * poll */
    drop(listener);
    drop(tasks);
}

/// A dialer: a session at a time -- the control connection, and once it
/// has said its Hello the bulk one -- each waited out as it ends, and the
/// next after a wait that doubles while none comes up.
fn dialer_main((shared, peer): (Arc<Shared>, Arc<Peer>)) {
    let mut backoff = BACKOFF_MIN_MS;
    while !shared.stop.load(Ordering::Acquire) && !peer.removed.load(Ordering::Acquire) {
        let Some(session) = kcore::random::random_u64().filter(|&s| s != 0) else {
            sleep_unless_stopped(&shared, Some(&peer), backoff);
            continue;
        };
        if !peer.begin(session) {
            sleep_unless_stopped(&shared, Some(&peer), POLL_MS);
            continue;
        }
        let ups = peer.counts.ups.load(Ordering::Relaxed);
        let link = |class: usize| {
            spawn_link(LinkCtx { shared: shared.clone(), side: Side::Dial { peer: peer.clone(), class, session },
                                 done: None })
        };
        let control = link(CONTROL);
        if control.is_none() {
            peer.lose(&shared.router, session, "no task for its connection");
        }
        /* The bulk connection once the control one is in: so that a peer
         * takes one handshake from this host at a time */
        let until = kcore::time::boot_time_ns() + CONTROL_FIRST_MS * NS_PER_MS;
        while peer.current(session) && !peer.st.with(|st| st.attached[CONTROL])
            && kcore::time::boot_time_ns() < until && !shared.stop.load(Ordering::Acquire) {
            kcore::task::sleep_ms(5);
        }
        let bulk = if peer.current(session) { link(BULK) } else { None };
        if bulk.is_none() {
            peer.lose(&shared.router, session, "its bulk connection never began");
        }
        /* Both end when the session does */
        drop(bulk);
        drop(control);
        backoff = if peer.counts.ups.load(Ordering::Relaxed) != ups {
            BACKOFF_MIN_MS
        } else {
            (backoff * 2).min(BACKOFF_MAX_MS)
        };
        sleep_unless_stopped(&shared, Some(&peer), backoff);
    }
}
