//! `ipcnet`: the ipcnet module -- its source, as the kernel builds it into
//! ipcnet.ko -- as host `b` of a cluster whose other host, `a`, is the
//! world's: on the LAN, pinned by the key the world shows, dialing `b`
//! (its name sorts first) on both of a session's connections, control and
//! bulk, and speaking ipcwire inside them -- its Hello, Pongs for `b`'s
//! Pings, and the messages, monitors and bounces the input says.
//!
//! On the machine, the channel (the `ipc` crate, the image's) with two
//! mailboxes of the image's: `nbs.sink`, a Cluster name, which keeps what it
//! is sent and answers the world when the input says, and `kernel.hidden`,
//! a Host name, which nothing from another host may ever reach.
//!
//! What the world does to `b`: messages to `nbs.sink` by name and by its
//! address -- the latter `b` takes only once the sink has sent to `a` -- to
//! `kernel.hidden`, to addresses nobody has, by both classes, of every
//! size; frames the wire refuses, a frame out of its place, a Hello not
//! this cluster's or for another host, a key that is not the one pinned,
//! silence, a connection closed. What `b` does: the sink's answers to
//! `nbs.w@a`, `ipcnet stop` and `start`, the unload.
//!
//! Its control mailbox, `ipcnet.ctl`, is asked as the cluster's member asks
//! it: who this host is, and lists of peers -- among them `a` as a host
//! invited, which may reach `nbs.sink` by that name alone until a list
//! makes it a host again, with no connection lost either way.
//!
//! What must hold: nothing from `a` ever reaches `kernel.hidden`, nor the
//! sink by its address before the sink has sent to `a` or while `a` may
//! reach it by its name alone; what reaches the
//! sink is what the world sent it, the bytes, the tag and the reference,
//! each once and in order within a class; what reaches the world is what
//! the sink sent, each once and in order; a key not the pinned one is never
//! up; silence past 3 s, or a connection ended, loses the peer -- within
//! the time and a margin -- and the world dials again; and once the module
//! is unloaded, the machine holds nothing.

#[path = "../../../../src/crates/modules/cluster/ipcnet/src/lib.rs"]
#[allow(unused_attributes, dead_code, clippy::all)]
mod ipcnet;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use ipc::{Limits, Mailbox, Payload, Scope};
use ipcwire::{control, Assembler, Dest, Frame, To};
use kcore::time::Duration;

use crate::machine::{cmd, sched};
use crate::world::lan::Lan;
use crate::world::tcpm::{Data, Tcp, S};
use crate::world::tlsc::{keys, Break, Client, Plan, Talk, Who};
use crate::common::runner::reached;
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

const PORT: u16 = 39240;
/// Host `a`, the world's, on the machine's LAN.
const A_IP: u32 = 0x0A00_0264;
const A_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x64];
const CLUSTER: [u8; 16] = *b"nos-ipc-fuzz-0x1";
const OTHER_CLUSTER: [u8; 16] = *b"nos-ipc-fuzz-0x2";
/// The world's own mailbox's address, as its host has it (host 0), and the
/// one the input sends from now and then.
const A_BOX: u64 = (3 << 32) | 1;
const A_BOX2: u64 = (4 << 32) | 1;
/// What the classes' connections say they are.
const ALPN: [&[u8]; 2] = [b"nos-ipc/1", b"nos-ipc-bulk/1"];
const CONTROL: usize = 0;
const BULK: usize = 1;
/// The bulk bit of a tag (the channel's TAG_BULK).
const TAG_BULK: u32 = 0x4000_0000;
/// The machine's sink: its limits, small enough for a pause to fill.
const SINK_LIMITS: Limits = Limits { messages: 4, bytes: 96 << 10 };
/// How late a loss may be seen past the module's 3 s: a Ping's period and
/// the machine's ticks.
const LOST_LATE: u64 = 3 * SEC + 700 * MS;
/// Messages the world sends, and the most bytes one has: what the world
/// keeps stays well under a block of the machine's heap.
const SENDS: usize = 48;
const BYTES_MAX: usize = 40_000;

/* ---- what the sink was sent, and what it got ---- */

/// A message, as the model keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Msg {
    from: u64,
    tag: u32,
    reference: u64,
    data: Vec<u8>,
}

/// A key `a` is pinned by beside its own, as a host moving to a new key is:
/// one the world never shows.
const A_NEXT_KEY: [u8; 32] = [0x5A; 32];

/// While set, `a` is a host invited, which may reach `nbs.sink` by that name
/// alone: set once ipcnet has taken the list that says so, cleared before
/// it is given one that does not -- never set while `a` may reach more.
static A_ONLY: AtomicBool = AtomicBool::new(false);
/// How many lists making `a` a host invited ipcnet has taken: a monitor
/// asked before one of them may meet it.
static A_ONLY_SETS: AtomicU64 = AtomicU64::new(0);
/// How many lists letting `a` reach more it has been given: a monitor asked
/// while `a` was invited, with none since, met it for certain.
static A_ONLY_LIFTS: AtomicU64 = AtomicU64::new(0);

/// What a monitor of the world's may have met of `a` as a host invited:
/// whether `a` was one as it was asked, and the lists of each kind by then.
#[derive(Clone, Copy)]
struct Invited {
    only: bool,
    sets: u64,
    lifts: u64,
}

impl Invited {
    fn now() -> Invited {
        Invited { only: A_ONLY.load(Ordering::Acquire), sets: A_ONLY_SETS.load(Ordering::Acquire),
                  lifts: A_ONLY_LIFTS.load(Ordering::Acquire) }
    }

    /// It may have been refused, and told HostLost rather than Closed.
    fn may_have_met(self) -> bool {
        self.only || A_ONLY_SETS.load(Ordering::Acquire) != self.sets
    }

    /// It was refused: no Closed may come for it.
    fn met(self) -> bool {
        self.only && A_ONLY_LIFTS.load(Ordering::Acquire) == self.lifts
    }
}

/// `a`'s ups and the messages got from it, as `ipcnet` says them -- zero
/// while it lists no `a`.
fn a_counts(status: &str) -> (u64, u64) {
    let Some(line) = status.lines().find(|l| l.trim_start().starts_with("a ")) else { return (0, 0) };
    let after = |word: &str| -> u64 {
        line.split(word).nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok()).unwrap_or(0)
    };
    (after("ups "), after(" got "))
}

/// What the machine's programs saw: the sink's messages, kept by its task,
/// and whether anything reached `kernel.hidden`.
#[derive(Default)]
struct Seen {
    /// What the sink got, and for each whether a send of its own to `a` had
    /// been taken before it looked -- what alone lets `a` reach it by its
    /// address -- and whether `a` might reach it by its name alone then.
    sink: Vec<(Msg, bool, bool)>,
    hidden: usize,
    /// The sink's own address, once it has one.
    sink_addr: u64,
    /// The victim -- a mailbox the world monitors -- and whether it has
    /// been closed.
    victim_addr: u64,
    victim_closed: bool,
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

fn seen() -> MutexGuard<'static, Option<Seen>> {
    SEEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// The sink's task: what comes, kept, until told to stop.
fn sink_main(mb: Mailbox, stop: Arc<AtomicBool>, sends: Arc<Mutex<VecDeque<Msg>>>, paused: Arc<AtomicBool>) {
    let mut sent_ok = false;
    while !stop.load(Ordering::Acquire) {
        /* Paused, it takes nothing: what comes fills it, and bounces */
        if paused.load(Ordering::Acquire) {
            kcore::task::sleep_ms(20);
            continue;
        }
        /* What the input has the sink say to `a`, first */
        let out: Vec<Msg> = sends.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        for m in out {
            let p = Payload::from_slice(&m.data).expect("bytes");
            sent_ok |= mb.send(ipc::To::Name("nbs.w@a"), m.tag, m.reference, p).is_ok();
        }
        let Some(m) = mb.recv(Duration::from_millis(20)) else { continue };
        if m.is_system() {
            continue;
        }
        let data = m.payload().to_vec().expect("bytes");
        let msg = Msg { from: m.from().to_u64(), tag: m.tag(), reference: m.reference(), data };
        let only = A_ONLY.load(Ordering::Acquire);
        sched::harness(|| {
            if let Some(s) = seen().as_mut() {
                s.sink.push((msg, sent_ok, only));
            }
        });
    }
}

fn hidden_main(mb: Mailbox, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        if mb.recv(Duration::from_millis(20)).is_some() {
            sched::harness(|| {
                if let Some(s) = seen().as_mut() {
                    s.hidden += 1;
                }
            });
        }
    }
}

/* ---- the world's host ---- */

/// What the world's Hello says, on one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hello {
    Right,
    /// Another cluster's id.
    Cluster,
    /// Calls the machine by another host's name.
    To,
    /// Says it is a host the machine does not know.
    From,
    /// Says nothing at all.
    Silent,
}

/// One connection's speaker: the Hello, Pongs for the machine's Pings, and
/// what the input queues -- and what the machine says, taken apart.
struct Speaker {
    class: usize,
    session: u64,
    hello: Hello,
    said_hello: bool,
    /// The machine's Hello, once it came: version, cluster, session, from, to.
    heard: Option<(u16, [u8; 16], u64, Vec<u8>, Vec<u8>)>,
    /// What the machine said is taken apart up to here.
    consumed: usize,
    /// Frames to say, encoded, the next first.
    queue: VecDeque<Vec<u8>>,
    asm: Assembler,
    /// What the machine sent here, whole.
    got: Vec<ipcwire::Message>,
    /// The Downs it said, by monitor id and reason; the bounces, by the
    /// bounced message's tag and reference.
    downs: Vec<(u64, u8)>,
    bounces: Vec<(u32, u64)>,
    /// The machine broke the wire: a frame it should not have sent.
    broken: Option<String>,
    /// Answers no Ping: what goes silent does.
    silent: bool,
    pings: usize,
    /// The Pings it said, and when, and the Pongs it heard back.
    pinged: Vec<(u64, u64)>,
    ponged: Vec<u64>,
}

impl Speaker {
    fn new(class: usize, session: u64, hello: Hello) -> Speaker {
        Speaker { class, session, hello, said_hello: false, heard: None, consumed: 0, queue: VecDeque::new(),
                  asm: Assembler::new(8, 4 << 20, 4096), got: Vec::new(), downs: Vec::new(), bounces: Vec::new(),
                  broken: None, silent: false, pings: 0, pinged: Vec::new(), ponged: Vec::new() }
    }

    fn up(&self) -> bool {
        self.heard.is_some()
    }

    fn say(&mut self, f: &Frame<'_>) {
        let mut b = Vec::new();
        ipcwire::encode(f, &mut b).expect("a frame the world makes");
        self.queue.push_back(b);
    }

    /// A message of the world's, cut into its frames.
    fn message(&mut self, msg: u32, to: To<'_>, from: u64, tag: u32, reference: u64, data: &[u8]) {
        for f in ipcwire::frames(msg, to, from, tag, reference, data).expect("a message within the limit") {
            self.say(&f);
        }
    }

    fn hear(&mut self, got: &[u8]) {
        while self.broken.is_none() {
            match ipcwire::decode(&got[self.consumed..]) {
                Ok(Some((f, used))) => {
                    self.consumed += used;
                    self.frame(&f);
                }
                Ok(None) => break,
                Err(e) => self.broken = Some(format!("the machine said bytes the wire refuses: {:?}", e)),
            }
        }
    }

    fn frame(&mut self, f: &Frame<'_>) {
        match *f {
            Frame::Hello { version, cluster, session, from, to } => {
                if self.heard.is_some() {
                    self.broken = Some(String::from("the machine said Hello twice"));
                    return;
                }
                self.heard = Some((version, cluster, session, from.to_vec(), to.to_vec()));
            }
            _ if self.heard.is_none() => {
                self.broken = Some(format!("the machine said {:?} before its Hello", f));
            }
            Frame::Ping { seq } => {
                self.pings += 1;
                if !self.silent {
                    /* Ahead of whatever is queued: a Pong is due now -- but
                     * never into the middle of a frame already queued, each
                     * entry being a whole frame */
                    let mut b = Vec::new();
                    ipcwire::encode(&Frame::Pong { seq }, &mut b).expect("a Pong");
                    self.queue.push_front(b);
                }
            }
            Frame::Head { .. } | Frame::Body { .. } => match self.asm.take(f) {
                Ok(Some(m)) => self.got.push(m),
                Ok(None) => {}
                Err(e) => self.broken = Some(format!("the machine's message past an assembler's limits: {:?}", e)),
            },
            Frame::Pong { seq } => self.ponged.push(seq),
            Frame::Down { id, reason } => {
                if self.class != CONTROL {
                    self.broken = Some(String::from("the machine said a Down on the bulk connection"));
                }
                self.downs.push((id, reason));
            }
            Frame::Bounced { tag, reference, .. } => {
                if self.class != CONTROL {
                    self.broken = Some(String::from("the machine said a bounce on the bulk connection"));
                }
                self.bounces.push((tag, reference));
            }
            _ => {}
        }
    }
}

impl Talk for Speaker {
    fn next(&mut self, got: &[u8]) -> Option<Vec<u8>> {
        self.hear(got);
        /* Silent: nothing at all, as a host gone quiet */
        if self.silent {
            return None;
        }
        if !self.said_hello {
            self.said_hello = true;
            let (cluster, from, to): (&[u8; 16], &[u8], &[u8]) = match self.hello {
                Hello::Right => (&CLUSTER, b"a", b"b"),
                Hello::Cluster => (&OTHER_CLUSTER, b"a", b"b"),
                Hello::To => (&CLUSTER, b"a", b"c"),
                Hello::From => (&CLUSTER, b"z", b"b"),
                Hello::Silent => return None,
            };
            let mut b = Vec::new();
            ipcwire::encode(&Frame::Hello { version: ipcwire::VERSION, cluster: *cluster, session: self.session,
                                            from, to }, &mut b).expect("a Hello");
            return Some(b);
        }
        /* Nothing more until the machine has said its own */
        if !self.up() {
            return None;
        }
        self.queue.pop_front()
    }

    fn done(&self, _got: &[u8]) -> bool {
        false
    }
}

/// Shares a speaker between the client that runs it and the target.
struct Shared(Rc<RefCell<Speaker>>);

impl Talk for Shared {
    fn next(&mut self, got: &[u8]) -> Option<Vec<u8>> {
        self.0.borrow_mut().next(got)
    }

    fn done(&self, got: &[u8]) -> bool {
        self.0.borrow().done(got)
    }
}

/// A connection of the world's to the machine.
struct Link1 {
    client: Client,
    speaker: Rc<RefCell<Speaker>>,
}

struct Wire {
    lan: Lan,
    tcp: Tcp,
    links: Vec<Link1>,
}

impl Wire {
    fn tend(&mut self, net: &mut Net) {
        for l in self.links.iter_mut() {
            let ep = l.client.ep;
            if matches!(self.tcp.eps[ep].st, S::Established | S::CloseWait) {
                l.client.start(&mut self.tcp, net);
            }
            l.client.pump(&mut self.tcp, net);
            if l.client.ending.is_none() && (!self.tcp.eps[ep].open() || self.tcp.eps[ep].reset.is_some()) {
                l.client.ending = Some(crate::world::tlsc::Ending::Gone);
            }
        }
    }
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if !self.lan.on_frame(net, frame) {
            self.tcp.on_frame(net, frame);
        }
        self.tend(net);
    }

    fn next_timer(&self) -> u64 {
        self.tcp.next_timer()
    }

    fn on_timer(&mut self, net: &mut Net) {
        self.tcp.on_timer(net);
        self.tend(net);
    }
}

/// What the world has queued, said now: its clients pumped, as they are
/// only when the machine sends or a timer runs -- then what that brings.
fn flush(w: &mut World<Wire>) {
    let World { net, peers } = w;
    peers.tend(net);
    w.pump();
}

/* ---- the machine ---- */

/// The module table's answer to which module an address of code is in:
/// what the channel names the owner of a module's mailbox by. In this
/// machine the only module that opens mailboxes is ipcnet's source.
///
/// # Safety
/// `buf` takes `size` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_module_name_at(_addr: usize, buf: *mut u8, size: usize) -> usize {
    let name = b"ipcnet";
    let n = name.len().min(size);
    // SAFETY: `buf` takes `size` bytes, and `n` is no more.
    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), buf, n) };
    n
}

static MODULE: Mutex<Option<Box<dyn kmod::Module>>> = Mutex::new(None);

fn module() -> MutexGuard<'static, Option<Box<dyn kmod::Module>>> {
    MODULE.lock().unwrap_or_else(|e| e.into_inner())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// hosts.toml and the host's identity in a ramfs root, the module loaded:
/// what its output said.
fn setup() -> String {
    let keys = keys();
    fs::ramfs::mount_at("/", false);
    fs::init();
    let a_key = tls::pinned::fingerprint(&keys.known.cert).expect("the world's certificate");
    let toml = format!("name = \"b\"\ncluster = \"{}\"\npeers = [\"a {} {}\"]\n", hex(&CLUSTER),
                       netwire::Ipv4(A_IP), hex(&a_key));
    let key = web::x509::pem("PRIVATE KEY", &keys.server.key).expect("a PEM");
    let cert = web::x509::pem("CERTIFICATE", &keys.server.cert).expect("a PEM");
    let written = kcore::fs::create_dir("/etc").and_then(|_| kcore::fs::create_dir("/etc/ipc"))
        .and_then(|_| kcore::fs::create_dir("/etc/ipc/tls"))
        .and_then(|_| kcore::fs::create("/etc/ipc/hosts.toml", toml.as_bytes()))
        .and_then(|_| kcore::fs::create("/etc/ipc/tls/key.pem", key.as_bytes()))
        .and_then(|_| kcore::fs::create("/etc/ipc/tls/cert.pem", cert.as_bytes()));
    invariant!(written.is_ok(), "the module's folder not written: {:?}", written);
    let loader = kcore::cmd::Command::register("insmod-ipcnet", "insmod-ipcnet - the ipcnet module's source", |args, out| {
        if let Ok(m) = ipcnet::insmod_with(args, out) {
            sched::harness(|| *module() = Some(m));
        }
    });
    let loaded = match loader {
        Ok(_) => cmd::run("insmod-ipcnet"),
        Err(_) => String::from("no command"),
    };
    drop(loader);
    loaded
}

/// Whether the channel has `a` up, and as which number.
fn a_up() -> Option<u16> {
    ipc::peers().into_iter().find(|p| p.1 == "a" && p.2).map(|p| p.0)
}

/* ---- the target ---- */

/// A message the world sent the sink: where to, its class, and the
/// message as the sink should get it, `from` but for its host.
struct Sent {
    by_name: bool,
    bulk: bool,
    from: u64,
    tag: u32,
    reference: u64,
    data: Vec<u8>,
}

pub fn ipcnet(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);
    *seen() = Some(Seen::default());
    A_ONLY.store(false, Ordering::Release);
    A_ONLY_SETS.store(0, Ordering::Release);
    A_ONLY_LIFTS.store(0, Ordering::Release);
    ipc::init();

    /* The machine's mailboxes: the sink, a Cluster name, and a Host one */
    let sink = Mailbox::new(SINK_LIMITS).expect("the sink");
    sink.register("nbs.sink", Scope::Cluster).expect("the sink's name");
    let hidden = Mailbox::new(SINK_LIMITS).expect("a mailbox");
    hidden.register("kernel.hidden", Scope::Host).expect("its name");
    let sink_addr = sink.addr().to_u64();
    seen().as_mut().expect("seen").sink_addr = sink_addr;
    let stop = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let sink_sends: Arc<Mutex<VecDeque<Msg>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (s1, s2, s3, s4) = (stop.clone(), stop.clone(), sink_sends.clone(), paused.clone());
    let sink_task = App::spawn("sink", 1, move || sink_main(sink, s1, s3, s4));
    /* The victim: a mailbox of the image's the world monitors, closed when
     * the input says */
    let mut victim = Some(Mailbox::new(SINK_LIMITS).expect("the victim"));
    let victim_addr = victim.as_ref().expect("the victim").addr().to_u64();
    seen().as_mut().expect("seen").victim_addr = victim_addr;
    let hidden_task = App::spawn("hidden", 2, move || hidden_main(hidden, s2));

    let perfect = r.u8() < 200;
    let link = if perfect { Link::perfect() } else { Link::from_input(r) };
    let mut lan = Lan::new();
    lan.add(A_IP, A_MAC);
    let mut w = World::new(link, Wire { lan, tcp: Tcp::new(A_IP, A_MAC), links: Vec::new() });
    w.net.expect.ips = vec![ETH0_IP];

    let loaded = setup();
    invariant!(module().is_some(), "the module's source would not load: {:?}", loaded);
    let a_key = tls::pinned::fingerprint(&keys().known.cert).expect("the world's certificate");
    let b_key = tls::pinned::fingerprint(&keys().server.cert).expect("the machine's certificate");
    let out = cmd::run("ipcnet start");
    invariant!(out.contains("serving"), "`ipcnet start` said {:?}", out);
    /* The listener binds on its first run */
    w.run_for(200 * MS);

    let mut sent: Vec<Sent> = Vec::new();
    let mut said: Vec<Msg> = Vec::new();
    let mut next_msg = [1u32; 2];
    let mut session = 0u64;
    let mut port = 41000u16;
    let mut silent_since: Option<u64> = None;
    let mut stopped = false;
    /* The monitors the world asked of the machine: id, the connection it
     * went on, and whether it was the victim's */
    /* Each with whether it may meet `a` as a host invited, which watches
     * nothing on `b`: answered at once with HostLost rather than Closed */
    let mut monitors: Vec<(u64, usize, bool, Invited)> = Vec::new();
    /* The key staged for `b`, once one is: the same one each time asked */
    let mut staged: Option<[u8; 32]> = None;
    /* Far from the ids the channel gives its own monitors: a Down said by
     * the router's own id is one nobody asked for */
    let mut next_monitor = 0x5000_0000_0000u64 + u64::from(r.u16());
    /* The world's references, each its own: what a bounce is told apart by */
    let mut next_ref = 1u64;

    /* Most inputs begin with a session */
    let first = r.u8() < 192;
    let mut first_op = first;
    loop {
        let op = if first_op {
            first_op = false;
            0
        } else {
            match r.op(15) {
                Some(op) => op,
                None => break,
            }
        };
        if std::env::var_os("IPCNET_OPS").is_some() {
            eprintln!("ipcnet: op {} at {} ms", op, sched::now() / MS);
        }
        match op {
            0 if !stopped => {
                /* A session: both connections, with the Hellos and the key
                 * the input says */
                session = session.wrapping_add(1 + r.below(1000));
                let who = r.pick(&[Who::Known, Who::Known, Who::Known, Who::Stranger, Who::Impostor]);
                /* As a host of nos dials: the bulk connection once the control
                 * one has said its Hello, the listener taking one handshake
                 * from a source at a time -- and now and then both at once,
                 * the second of which it resets */
                let together = r.u8() < 24;
                for class in [CONTROL, BULK] {
                    if class == BULK && !together {
                        let control = w.peers.links.last().map(|l| l.speaker.clone());
                        if let Some(c) = control {
                            let end = sched::now() + 2 * SEC;
                            w.run_until(end, || c.borrow().up());
                        }
                    }
                    let hello = if r.u8() < 220 { Hello::Right } else {
                        r.pick(&[Hello::Cluster, Hello::To, Hello::From, Hello::Silent])
                    };
                    let speaker = Rc::new(RefCell::new(Speaker::new(class, session, hello)));
                    port = port.wrapping_add(1).max(41000);
                    let ep = w.peers.tcp.connect(&mut w.net, 60_000 + port as usize, port, ETH0_IP, PORT, r.u32(),
                                                 crate::world::frames::mss(1460), 65535);
                    w.peers.tcp.eps[ep].data = Some(Data::default());
                    let plan = Plan { who, brk: Break::None, alpn: vec![ALPN[class]], lines: Vec::new(), pin: None };
                    let client = Client::talking(ep, plan, Box::new(Shared(speaker.clone())));
                    w.peers.links.push(Link1 { client, speaker });
                    w.pump();
                }
                silent_since = None;
                w.run_for(300 * MS);
                let fresh_up = w.peers.links.iter().rev().take(2).all(|l| l.speaker.borrow().up());
                if who != Who::Known {
                    invariant!(w.peers.links.iter().rev().take(2).all(|l| !l.speaker.borrow().up()),
                               "a key not the pinned one let up");
                    reached("ipcnet: a key not the pinned one refused");
                } else if fresh_up && a_up().is_some() {
                    reached("ipcnet: a session up");
                }
            }
            1 | 2 if sent.len() < SENDS => {
                /* A message to the sink, by name -- or by its address, which
                 * reaches it only once the sink has sent to `a` */
                let Some(i) = w.peers.links.iter().rposition(|l| l.speaker.borrow().up()) else { continue };
                let bulk = r.bool();
                let class = if bulk { BULK } else { CONTROL };
                /* The live session's connection of that class */
                let Some(j) = w.peers.links.iter().rposition(|l| {
                    let s = l.speaker.borrow();
                    s.up() && s.class == class && s.session == w.peers.links[i].speaker.borrow().session
                        && l.client.ending.is_none()
                }) else { continue };
                let len = match r.u8() % 8 {
                    0 => 0,
                    1 => BYTES_MAX.min(r.below(BYTES_MAX as u64 + 1) as usize),
                    _ => r.below(300) as usize,
                };
                let data: Vec<u8> = (0..len).map(|k| (k as u8).wrapping_mul(13) ^ r.u8()).collect();
                let tag = if bulk { 7 | TAG_BULK } else { 7 };
                let reference = next_ref << 16 | u64::from(r.u16());
                next_ref += 1;
                let from = if r.u8() < 230 { A_BOX } else { A_BOX2 };
                let by_name = op == 1 || r.bool();
                let to = if by_name { To::Name(b"nbs.sink") } else { To::Addr(sink_addr & !(0xFFFF << 48)) };
                let num = next_msg[class];
                next_msg[class] = next_msg[class].wrapping_add(1).max(1);
                w.peers.links[j].speaker.borrow_mut().message(num, to, from, tag, reference, &data);
                sent.push(Sent { by_name, bulk, from, tag, reference, data });
                flush(&mut w);
            }
            3 => {
                /* To the Host name, which nothing from outside may reach --
                 * and to an address nobody has */
                let Some(j) = w.peers.links.iter().rposition(|l| l.speaker.borrow().up() && l.client.ending.is_none())
                    else { continue };
                let to = if r.bool() { To::Name(b"kernel.hidden") } else { To::Addr((r.u64() & 0xFFFF_FFFF_FFFF) | 1) };
                let class = w.peers.links[j].speaker.borrow().class;
                let num = next_msg[class];
                next_msg[class] = next_msg[class].wrapping_add(1).max(1);
                let tag = if class == BULK { 9 | TAG_BULK } else { 9 };
                w.peers.links[j].speaker.borrow_mut().message(num, to, A_BOX, tag, 0, b"not for you");
                flush(&mut w);
            }
            4 if !stopped => {
                /* The sink says something to `a`: from then on `a` may
                 * answer it at its address */
                /* Of a frame's bytes or a few: two of them begun at once could
                 * end, and be delivered, out of their order */
                let len = if r.u8() < 64 { r.below(BYTES_MAX as u64) as usize } else { r.below(200) as usize };
                let m = Msg { from: sink_addr, tag: if r.bool() { 5 | TAG_BULK } else { 5 }, reference: r.u64(),
                              data: (0..len).map(|k| (k as u8) ^ 0x5A ^ (len as u8)).collect() };
                said.push(m.clone());
                sink_sends.lock().unwrap_or_else(|e| e.into_inner()).push_back(m);
            }
            5 => {
                /* The world goes silent, or speaks again */
                let silent = r.bool();
                for l in w.peers.links.iter() {
                    l.speaker.borrow_mut().silent = silent;
                }
                silent_since = if silent { Some(sched::now()) } else { None };
            }
            6 => {
                /* A frame the wire refuses, or one out of its place: the
                 * connection ends, and the peer with it */
                let Some(j) = w.peers.links.iter().rposition(|l| l.speaker.borrow().up() && l.client.ending.is_none())
                    else { continue };
                let mut s = w.peers.links[j].speaker.borrow_mut();
                match r.u8() % 3 {
                    0 => s.queue.push_back(vec![0xEE, 0, 0, 0, 0, 0, 0, 1, 0]),
                    1 => s.say(&Frame::Hello { version: 1, cluster: CLUSTER, session: 1, from: b"a", to: b"b" }),
                    _ => {
                        if s.class == BULK {
                            s.say(&Frame::Monitor { id: 1, target: 2 });
                        } else {
                            s.say(&Frame::Body { msg: 999, data: b"x" });
                        }
                    }
                }
                drop(s);
                flush(&mut w);
            }
            7 => {
                /* A connection closed under the session */
                if let Some(l) = w.peers.links.iter().rposition(|l| l.client.ending.is_none()) {
                    let ep = w.peers.links[l].client.ep;
                    if w.peers.tcp.eps[ep].open() {
                        w.peers.tcp.close(&mut w.net, ep);
                    }
                    w.pump();
                }
            }
            8 => {
                let out = cmd::run("ipcnet");
                invariant!(out.contains("ipcnet:"), "`ipcnet` said {:?}", out);
            }
            10 => {
                /* The world monitors the victim -- whose Down must come once,
                 * and only once it is closed -- or the sink pauses or goes on */
                if r.bool() {
                    paused.store(!paused.load(Ordering::Acquire), Ordering::Release);
                    reached("ipcnet: the sink paused or going on");
                    continue;
                }
                let Some(j) = w.peers.links.iter().rposition(|l| {
                    let sp = l.speaker.borrow();
                    sp.up() && sp.class == CONTROL && l.client.ending.is_none()
                }) else { continue };
                let id = next_monitor;
                next_monitor += 1;
                let of_victim = r.u8() < 230;
                monitors.push((id, j, of_victim && victim.is_some(), Invited::now()));
                let target = if of_victim { victim_addr & 0xFFFF_FFFF_FFFF } else { (r.u64() & 0xFFFF_FFFF_FFFF) | 1 };
                w.peers.links[j].speaker.borrow_mut().say(&Frame::Monitor { id, target });
                flush(&mut w);
                reached("ipcnet: a monitor asked across hosts");
                /* Taken in by the machine before anything else happens: a
                 * close that came first would make it a monitor of one gone */
                w.run_for(50 * MS);
            }
            12 if sent.len() + 6 <= SENDS => {
                /* A burst into the sink, paused: past its four places, each
                 * one it has no room for bounced back, Full -- from a world
                 * that speaks */
                let Some(j) = w.peers.links.iter().rposition(|l| {
                    let sp = l.speaker.borrow();
                    sp.up() && sp.class == CONTROL && l.client.ending.is_none() && !sp.silent
                }) else { continue };
                /* Paused before the burst: a sink in its receive takes one
                 * more until it looks at the flag again */
                paused.store(true, Ordering::Release);
                w.run_for(50 * MS);
                let before = sent.len();
                for _ in 0..6 {
                    let num = next_msg[CONTROL];
                    next_msg[CONTROL] = next_msg[CONTROL].wrapping_add(1).max(1);
                    let reference = next_ref << 16 | u64::from(r.u16());
                    next_ref += 1;
                    let data: Vec<u8> = (0..r.below(64) as usize).map(|k| k as u8).collect();
                    w.peers.links[j].speaker.borrow_mut().message(num, To::Name(b"nbs.sink"), A_BOX, 11, reference,
                                                                   &data);
                    sent.push(Sent { by_name: true, bulk: false, from: A_BOX, tag: 11, reference, data });
                }
                flush(&mut w);
                w.run_for(200 * MS);
                let alive = w.peers.links[j].client.ending.is_none() && a_up().is_some()
                    && w.peers.links.iter().skip(j + 1).all(|m| m.speaker.borrow().session
                                                                == w.peers.links[j].speaker.borrow().session);
                if alive && perfect {
                    let sp = w.peers.links[j].speaker.borrow();
                    let bounced = sent[before..].iter().filter(|x| sp.bounces.contains(&(x.tag, x.reference))).count();
                    invariant!(bounced >= 2, "six messages into a paused sink of four places: {} bounced", bounced);
                    reached("ipcnet: a burst into a full mailbox bounced back");
                }
                paused.store(false, Ordering::Release);
            }
            11 => {
                /* The victim closed: a Down for every monitor of it there is */
                if let Some(v) = victim.take() {
                    seen().as_mut().expect("seen").victim_closed = true;
                    drop(v);
                    w.run_for(300 * MS);
                    /* Every monitor of it on a connection still up has had
                     * its Down by now */
                    for &(id, j, of_victim, inv) in &monitors {
                        let invited = inv.may_have_met();
                        let l = &w.peers.links[j];
                        /* Its Monitor gone out: a silent world's waits in its
                         * queue */
                        let out = !l.speaker.borrow().silent && l.speaker.borrow().queue.is_empty();
                        let alive = out && l.client.ending.is_none() && a_up().is_some()
                            && w.peers.links.iter().skip(j + 1).all(|m| m.speaker.borrow().session
                                                                        == l.speaker.borrow().session);
                        /* On a perfect link: a lossy one may have held the
                         * Monitor back past the close, and NoSuch is then its
                         * answer */
                        if of_victim && alive && perfect {
                            let downs = &l.speaker.borrow().downs;
                            invariant!(downs.iter().any(|d| d.0 == id && (d.1 == 1 || invited)),
                                       "the victim closed with its monitor {} up across hosts, and no Down came", id);
                            if invited && downs.iter().any(|d| d.0 == id && d.1 != 1) {
                                reached("ipcnet: a host invited refused a monitor, told at once");
                            } else {
                                reached("ipcnet: every monitor of a mailbox closed told, on a session that stayed up");
                            }
                        }
                    }
                }
            }
            9 if r.u8() < 40 => {
                /* Stopped and started: every peer lost, and the world dials
                 * again */
                let out = cmd::run(if stopped { "ipcnet start" } else { "ipcnet stop" });
                stopped = !stopped;
                invariant!(out.contains("serving") || out.contains("stopped"), "ipcnet said {:?}", out);
                if !stopped {
                    w.run_for(200 * MS);
                }
            }
            14 => {
                /* ipcnet's control mailbox, asked as the cluster's registry
                 * asks it (control.rs) */
                let kind = r.u8() % 9;
                let a = control::Peer { name: String::from("a"), ip: A_IP, port: 0, keys: vec![a_key], only: None };
                /* A list that lets `a` reach more: the flag down before it is
                 * taken */
                if kind == 1 || kind == 2 || kind == 8 {
                    A_ONLY.store(false, Ordering::Release);
                    A_ONLY_LIFTS.fetch_add(1, Ordering::AcqRel);
                }
                let (tag, data) = match kind {
                    0 => (control::TAG_ID, Vec::new()),
                    /* The peers as they are */
                    1 => (control::TAG_PEERS, control::Peers { cluster: CLUSTER, peers: vec![a.clone()] }.encode()),
                    /* One more: a host the world never is */
                    2 => {
                        /* At this host's port, or at one of its own (#99) */
                        let c = control::Peer { name: String::from("c"), ip: 0x0A00_0296 + u32::from(r.u8() % 8),
                                                port: [0, 39241, 39242][usize::from(r.u8() % 3)],
                                                keys: vec![[r.u8() | 1; 32]], only: None };
                        (control::TAG_PEERS, control::Peers { cluster: CLUSTER, peers: vec![a.clone(), c] }.encode())
                    }
                    /* Refused: another cluster's, this host among its own, noise */
                    3 => (control::TAG_PEERS, control::Peers { cluster: OTHER_CLUSTER, peers: vec![a.clone()] }.encode()),
                    4 => {
                        let me = control::Peer { name: String::from("b"), ip: 0x0A00_0299, port: 0,
                                                 keys: vec![[9; 32]], only: None };
                        (control::TAG_PEERS, control::Peers { cluster: CLUSTER, peers: vec![a.clone(), me] }.encode())
                    }
                    /* `a` a host invited: `nbs.sink` by that name, and nothing else */
                    6 => {
                        let invited = control::Peer { only: Some(String::from("nbs.sink")), ..a.clone() };
                        (control::TAG_PEERS, control::Peers { cluster: CLUSTER, peers: vec![invited] }.encode())
                    }
                    /* A key staged for this host, as `cluster rekey` stages one */
                    7 => (control::TAG_KEY_STAGE, Vec::new()),
                    /* `a` pinned by a second key beside its own, as a host moving
                     * to a new one is: kept, its connections with it */
                    8 => {
                        let moving = control::Peer { keys: vec![a_key, A_NEXT_KEY], ..a.clone() };
                        (control::TAG_PEERS, control::Peers { cluster: CLUSTER, peers: vec![moving] }.encode())
                    }
                    _ => (control::TAG_PEERS, (0..r.below(64)).map(|_| r.u8()).collect()),
                };
                /* `a` as ipcnet has it before: what a list that keeps it
                 * never takes back -- its ups and messages are counted on
                 * the peer, and a peer made anew counts from nothing */
                let counts_before = a_counts(&cmd::run("ipcnet"));
                let call = App::spawn("ctl", 3, move || {
                    let before = kcore::fs::read("/etc/ipc/hosts.toml", 16384).unwrap_or_default();
                    let mb = Mailbox::new(Limits { messages: 4, bytes: 64 << 10 }).expect("a mailbox");
                    let p = Payload::from_slice(&data).expect("bytes");
                    /* A list without a peer waits out that peer's dial, if one is under way: its
                     * connect's five seconds, its address's resolution before them */
                    let answer = ipc::call(&mb, ipc::To::Name(control::NAME), tag, p, Duration::from_millis(15_000))
                        .ok().map(|m| (m.tag(), m.payload().to_vec().expect("bytes")));
                    let after = kcore::fs::read("/etc/ipc/hosts.toml", 16384).unwrap_or_default();
                    (answer, before, after)
                });
                let end = sched::now() + 20 * SEC;
                w.run_until(end, || call.done());
                let Some((answer, before, after)) = call.take() else {
                    invariant!(false, "ipcnet's control mailbox never answered");
                    continue;
                };
                let Some((got, payload)) = answer else {
                    invariant!(false, "ipcnet's control mailbox answered nothing to request {}", kind);
                    continue;
                };
                let text = String::from_utf8_lossy(&after).into_owned();
                match kind {
                    0 => {
                        let id = control::Identity::decode(&payload);
                        let want = control::Identity { name: String::from("b"), cluster: CLUSTER, key: b_key,
                                                       nic: String::from("eth0"), ip: ETH0_IP, port: PORT,
                                                       serving: !stopped, next: staged };
                        invariant!(got == control::TAG_ID && id.as_ref() == Some(&want),
                                   "ipcnet said who it is as {:?}, not {:?}", id, want);
                        reached("ipcnet: its control mailbox said who this host is");
                    }
                    7 => {
                        /* A key, not this host's, and the same one asked again;
                         * hosts.toml untouched */
                        let key: Option<[u8; 32]> = payload.as_slice().try_into().ok();
                        invariant!(got == control::TAG_KEY_STAGE && key.is_some_and(|k| k != b_key)
                                   && (staged.is_none() || key == staged) && after == before,
                                   "a key staged answered {} with {:?}, staged before {:?}", got, key, staged);
                        staged = key;
                        reached("ipcnet: a key staged for this host");
                    }
                    1 | 2 | 6 | 8 => {
                        invariant!(got == control::TAG_OK, "ipcnet refused a list of its peers: {:?}",
                                   String::from_utf8_lossy(&payload));
                        let has_c = text.contains("\"c ");
                        let line = match kind {
                            6 => format!("\"a {} {} only=nbs.sink\"", netwire::Ipv4(A_IP), hex(&a_key)),
                            8 => format!("\"a {} {} {}\"", netwire::Ipv4(A_IP), hex(&a_key), hex(&A_NEXT_KEY)),
                            _ => format!("\"a {} {}\"", netwire::Ipv4(A_IP), hex(&a_key)),
                        };
                        invariant!(text.contains(&line) && has_c == (kind == 2),
                                   "hosts.toml is not the peers it was told: {:?}", text);
                        if kind == 6 {
                            A_ONLY.store(true, Ordering::Release);
                            A_ONLY_SETS.fetch_add(1, Ordering::AcqRel);
                        }
                        /* The peer kept is kept: lost now and then for the world's
                         * doing, never taken off by a list that has it, nor made
                         * anew -- what it may reach changed in place */
                        /* Stopped, ipcnet has the list in hosts.toml alone */
                        let st = cmd::run("ipcnet");
                        if !stopped {
                            let line = st.lines().find(|l| l.trim_start().starts_with("a ")).unwrap_or("");
                            invariant!(!line.is_empty() && !line.contains("taken off hosts.toml"),
                                       "a peer kept on the list taken off by it: {:?}", st);
                            invariant!(line.contains("(invited: nbs.sink alone)") == (kind == 6),
                                       "a peer's line says {:?}, after a list that made it {}", line,
                                       if kind == 6 { "a host invited" } else { "a host" });
                            let counts = a_counts(&st);
                            invariant!(counts.0 >= counts_before.0 && counts.1 >= counts_before.1,
                                       "a peer kept on the list made anew: ups and messages {:?}, {:?} before",
                                       counts, counts_before);
                        }
                        reached(match kind {
                            1 => "ipcnet: told its peers as they are",
                            2 => "ipcnet: told one more peer",
                            8 => "ipcnet: told a peer has a second key, its connections kept",
                            _ => "ipcnet: told a peer is a host invited, which reaches one name",
                        });
                    }
                    _ => {
                        invariant!(got == control::TAG_REFUSED && after == before,
                                   "a list of peers that is none taken: answered {}, hosts.toml {:?}", got, text);
                        reached("ipcnet: a list of peers that is none refused, hosts.toml as it was");
                    }
                }
            }
            13 => {
                /* The world pings, on every connection that is up and speaks:
                 * each answered */
                for l in w.peers.links.iter() {
                    let mut sp = l.speaker.borrow_mut();
                    if sp.up() && !sp.silent && l.client.ending.is_none() && sp.pinged.len() < 64 {
                        let seq = 0x7000_0000 + sp.pinged.len() as u64;
                        let at = sched::now();
                        sp.say(&Frame::Ping { seq });
                        sp.pinged.push((seq, at));
                    }
                }
                flush(&mut w);
            }
            _ => {
                let step = match r.u8() % 8 {
                    0 => 5 * SEC,
                    1..=3 => r.below(1000) * MS,
                    _ => r.below(100) * MS,
                };
                w.run_for(step);
            }
        }
        check(&w, &sent, &said, silent_since, &monitors, perfect);
    }

    /* The end: every connection closed, the world quiet, the module
     * unloaded -- and nothing of it left */
    for l in w.peers.links.iter() {
        let ep = l.client.ep;
        if w.peers.tcp.eps[ep].open() {
            w.peers.tcp.close(&mut w.net, ep);
        }
    }
    w.run_for(SEC);
    check(&w, &sent, &said, silent_since, &monitors, perfect);
    let m = module().take();
    kmod::unload(m);
    drop(victim);
    stop.store(true, Ordering::Release);
    invariant!(w.wait_app(&sink_task, 10 * SEC) && w.wait_app(&hidden_task, 10 * SEC), "the mailboxes' tasks would not end");
    let _ = sink_task.take();
    let _ = hidden_task.take();
    w.run_for(70 * SEC);
    invariant!(ipc::router().is_none(), "a router left after the module's unload");
    super::audit();
}

/// What must hold at every step.
fn check(w: &World<Wire>, sent: &[Sent], said: &[Msg], silent_since: Option<u64>,
         asked: &[(u64, usize, bool, Invited)],
         perfect: bool) {
    /* The kernel asked first, and the model's lock taken after: a call into
     * the kernel may hand the machine's turn to a task -- the sink's -- that
     * waits on that lock to say what it got */
    let up = a_up();
    let s = seen();
    let s = s.as_ref().expect("seen");
    invariant!(s.hidden == 0, "a Host name reached from another host: {} messages", s.hidden);
    if !s.sink.is_empty() {
        reached("ipcnet: a message of the world's delivered to the sink");
    }
    if s.sink.iter().any(|(g, ok, _)| *ok && sent.iter().any(|x| !x.by_name && x.reference == g.reference)) {
        reached("ipcnet: the sink reached at its address, having sent there");
    }
    /* What reached the sink: the world's, each once, in order within a
     * class and a sender -- and by address only once the sink had sent */
    for class in [false, true] {
        for from in [A_BOX, A_BOX2] {
            let mine: Vec<&Sent> = sent.iter().filter(|x| x.bulk == class && x.from == from).collect();
            let mut at = 0;
            for (g, sent_ok, only) in s.sink.iter().filter(|(g, _, _)| (g.tag & TAG_BULK != 0) == class
                                                                 && (g.from & 0xFFFF_FFFF_FFFF) == from) {
                let Some(k) = mine[at..].iter().position(|x| x.tag == g.tag && x.reference == g.reference && x.data == g.data)
                    else {
                        panic!("invariant: the sink got a message ({} bytes, tag {:x}, reference {:x}) the world never \
                                sent it, or out of its order", g.data.len(), g.tag, g.reference);
                    };
                let x = mine[at + k];
                invariant!(x.by_name || *sent_ok, "a message to the sink's address reached it before the sink had \
                           sent to the world's host");
                invariant!(x.by_name || !*only, "a message to the sink's address reached it while the world's host \
                           was a host invited, which may reach it by its name alone");
                if *only {
                    reached("ipcnet: a host invited reached the one name it may, by that name");
                }
                invariant!(g.from >> 48 != 0, "a message from another host delivered with no host in its sender");
                at += k + 1;
            }
        }
    }
    /* What reached the world: the sink's, each once, in order within a
     * class -- the connections in the order they were made, a session's
     * after the one before */
    for class in [false, true] {
        let mine: Vec<&Msg> = said.iter().filter(|m| (m.tag & TAG_BULK != 0) == class).collect();
        let mut at = 0;
        let speakers: Vec<std::cell::Ref<'_, Speaker>> = w.peers.links.iter().map(|l| l.speaker.borrow()).collect();
        for g in speakers.iter().flat_map(|sp| sp.got.iter()).filter(|g| (g.tag & TAG_BULK != 0) == class) {
            let data: Vec<u8> = g.pieces.concat();
            let Some(k) = mine[at..].iter().position(|m| m.tag == g.tag && m.reference == g.reference && m.data == data)
                else {
                    panic!("invariant: the world got a message (tag {:x}, {} bytes) the sink never sent, or out of \
                            its order", g.tag, data.len());
                };
            invariant!(g.to == Dest::Name(b"nbs.w".to_vec()), "the sink's message reached the world for {:?}", g.to);
            invariant!(g.from == s.sink_addr & 0xFFFF_FFFF_FFFF, "the sink's message came from {:x}, not its own \
                       address on its host", g.from);
            reached("ipcnet: the sink's message to the world's host delivered there");
            at += k + 1;
        }
    }
    for l in w.peers.links.iter() {
        let sp = l.speaker.borrow();
        if let Some(b) = &sp.broken {
            panic!("invariant: {}", b);
        }
        /* A Down names a monitor the world asked for, once -- the victim's
         * only once it is closed -- and a bounce a message the world sent
         * the sink that the sink never got */
        /* A Ping the world said half a second ago and more, on a connection
         * still up, over a perfect link: answered */
        if perfect && l.client.ending.is_none() && up.is_some() && !sp.silent {
            for (seq, at) in sp.pinged.iter() {
                if sched::now() > at + 500 * MS {
                    invariant!(sp.ponged.contains(seq), "the world's Ping {:x} on a connection up, unanswered since {} ms",
                               seq, at / MS);
                    reached("ipcnet: the world's Ping answered");
                }
            }
        }
        /* A Hello not this cluster's, or not for this host, or from a host
         * it does not know, is never answered with the machine's */
        invariant!(sp.hello == Hello::Right || sp.heard.is_none(), "a Hello the machine should refuse ({:?}) \
                   answered with its own", sp.hello);
        for (k, (id, reason)) in sp.downs.iter().enumerate() {
            invariant!(asked.iter().any(|m| m.0 == *id) && sp.downs[..k].iter().all(|d| d.0 != *id),
                       "a Down for monitor {} said twice, or for one never asked", id);
            invariant!(*reason != 1 || !asked.iter().any(|m| m.0 == *id && m.3.met()),
                       "monitor {}, asked while the world's host was a host invited, which watches nothing, \
                       told its mailbox closed", id);
            if *reason == 1 {
                invariant!(s.victim_closed, "a Down (Closed) for monitor {} before the victim closed", id);
                reached("ipcnet: a monitor across hosts told of its mailbox's close");
            } else {
                reached("ipcnet: a monitor across hosts told its mailbox was never there, or is out of reach");
            }
        }
        for (tag, reference) in sp.bounces.iter() {
            let ours = sent.iter().any(|x| x.tag == *tag && x.reference == *reference);
            let got = s.sink.iter().any(|(g, _, _)| g.tag == *tag && g.reference == *reference);
            invariant!(ours && !got, "a bounce of tag {:x} reference {:x}: the world's {} and the sink got it {}",
                       tag, reference, ours, got);
            reached("ipcnet: a message the sink had no room for bounced back");
        }
    }
    /* Silence past the module's 3 s: the peer lost by now */
    if let Some(t) = silent_since {
        if sched::now() > t + LOST_LATE {
            invariant!(up.is_none(), "the world silent since {} ms, and still up at {} ms", t / MS,
                       sched::now() / MS);
            reached("ipcnet: the world silent, its host lost in time");
        }
    }
}
