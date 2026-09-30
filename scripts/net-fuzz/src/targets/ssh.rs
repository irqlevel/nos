//! `ssh`: the sshd module -- its source, as the kernel builds it into
//! sshd.ko -- over the `ssh` crate, serving the world's SSH clients from a
//! ramfs root: its host key made on the first start and kept in /etc/ssh,
//! the key it lets in written there beforehand.
//!
//! The clients (`sshc`) connect as the input says, a few at a time: log in
//! with the key the server knows, or a stranger's, or a signature over the
//! wrong session; run a command whose output is known, or a shell, with a
//! window of their choosing, rekeying on the way; answer the server's
//! keepalives or not; or break the protocol at one point -- a version line
//! of HTTP, no cipher in common, a point of small order, a length past any,
//! an IGNORE where the strict exchange allows none, a packet tampered with,
//! a channel before a login, more data than the window, data for no
//! channel -- or say nothing at all.
//!
//! Besides what the client checks of every packet: a login is had exactly
//! when the known key signs the right session -- within six tries, as the
//! server allows -- and what the command prints comes back whole, with its
//! exit status; a client that breaks the protocol is told so and let go;
//! one that never logs in is let go when the login grace runs out; and once
//! every client has gone and the server stops, the machine holds nothing.

#[path = "../../../../src/rust/modules/sshd/src/lib.rs"]
#[allow(unused_attributes, dead_code, clippy::all)]
mod sshd;

use std::cell::RefCell;
use std::rc::Rc;

use crate::machine::{cmd, sched};
use crate::world::lan::Lan;
use crate::world::sshc::{Break, Client, Ending, Phase, Plan, Run, Try, Who};
use crate::world::tcpm::{Data, Tcp, S};
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

const PORT: u16 = 22;
const CLIENT_IP: u32 = 0x0A00_0264;
const CLIENT_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x64];
/// The server's limits, as the module has them.
const MAX_AUTH_TRIES: usize = 6;
const LOGIN_GRACE: u64 = 30 * SEC;
const MAX_LOGGING_IN: usize = 4;
const MAX_SESSIONS: usize = 8;

/* DISCONNECT reasons (RFC 4250 4.2.2) */
const PROTOCOL_ERROR: u32 = 2;
const KEY_EXCHANGE_FAILED: u32 = 3;
const MAC_ERROR: u32 = 5;
const BY_APPLICATION: u32 = 11;
const NO_MORE_AUTH_METHODS: u32 = 14;

/// A client, and what the world knew when it connected.
struct Conn {
    client: Client,
    started: bool,
    at: u64,
    /// When it was over, its connection closed by the client.
    over_at: Option<u64>,
    /// Others logging in, and connected at all, when it connected: the
    /// server may turn it away past its limits.
    crowded: bool,
}

/// How long the server may count a session after its client is done: until
/// its listener, which looks four times a second, reaps it.
const REAP: u64 = 2 * SEC;
/// Clients an input makes at most.
const CLIENTS: usize = 24;

struct Wire {
    lan: Lan,
    tcp: Tcp,
    conns: Rc<RefCell<Vec<Conn>>>,
}

impl Wire {
    fn tend(&mut self, net: &mut Net) {
        for c in self.conns.borrow_mut().iter_mut() {
            let st = self.tcp.eps[c.client.ep].st;
            if !c.started && matches!(st, S::Established | S::CloseWait) {
                c.started = true;
                c.client.start(&mut self.tcp, net);
            }
            if c.started {
                c.client.pump(&mut self.tcp, net);
            }
            /* The connection gone under a client that still had business
             * on it. */
            if !self.tcp.eps[c.client.ep].open() || self.tcp.eps[c.client.ep].reset.is_some() {
                if c.client.ending.is_none() {
                    c.client.ending = Some(Ending::Gone);
                }
                c.client.phase = Phase::Over;
            }
            /* Done: the client hangs up, as ssh does. */
            if c.client.over() && c.over_at.is_none() {
                c.over_at = Some(sched::now());
                if self.tcp.eps[c.client.ep].open() {
                    self.tcp.close(net, c.client.ep);
                }
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

/// A command the fuzzer knows the output of, or the kernel's own.
fn command(r: &mut Input) -> (String, Option<Vec<u8>>) {
    match r.u8() % 8 {
        0..=2 => {
            let w = format!("w{}", r.u16());
            (format!("echo {}", w), Some(format!("{}\n", w).into_bytes()))
        }
        3 => {
            let n = r.pick(&[0usize, 1, 1000, 32768, 65536, 100_000, 300_000]);
            (format!("spew {}", n), Some(spew(n)))
        }
        4 => (format!("nap {}", r.below(3000)), Some(b"awake\n".to_vec())),
        5 => ("frob".to_string(), Some(b"unknown command: frob\n".to_vec())),
        _ => (r.pick(&["ls /", "cat /etc/ssh/authorized_keys", "sshd", "sshd keys", "net", "sshd stop", "mounts"])
                .to_string(), None),
    }
}

/// What the fuzzer's `spew n` prints.
fn spew(n: usize) -> Vec<u8> {
    let n = n.min(1 << 20);
    let mut out = Vec::with_capacity(n);
    let mut k = 0usize;
    while out.len() < n {
        let mut line = [0u8; 97];
        for (i, b) in line.iter_mut().enumerate().take(96) {
            *b = b'a' + ((k + i) % 26) as u8;
        }
        line[96] = b'\n';
        let take = (n - out.len()).min(97);
        out.extend_from_slice(&line[..take]);
        k += 1;
    }
    out
}

/// A client's plan, as the input has it.
fn plan(r: &mut Input) -> Plan {
    let brk = match r.u8() % 24 {
        0 => Break::Version,
        1 => Break::NoCipher,
        2 => Break::SmallPoint,
        3 => Break::Length,
        4 => Break::IgnoreFirst,
        5 => Break::Tamper,
        6 => Break::ChannelEarly,
        7 => Break::Overrun,
        8 => Break::NoChannel,
        9 => Break::Silent,
        _ => Break::None,
    };
    let mut tries = Vec::new();
    for _ in 0..r.below(8) {
        tries.push(match r.u8() % 6 {
            0 => Try::None,
            1 => Try::Query(Who::Known),
            2 => Try::Query(Who::Stranger),
            3 => Try::Sign(Who::Stranger),
            4 => Try::BadSignature,
            _ => Try::Sign(Who::Known),
        });
        if tries.last() == Some(&Try::Sign(Who::Known)) {
            break;
        }
    }
    if r.u8() < 200 && !tries.contains(&Try::Sign(Who::Known)) {
        tries.push(Try::Sign(Who::Known));
    }
    let run = match r.u8() % 6 {
        0 => {
            let lines: Vec<(String, Option<Vec<u8>>)> = (0..1 + r.below(4)).map(|_| command(r))
                .filter(|c| !c.0.starts_with("sshd stop")).collect();
            Run::Shell(lines)
        }
        1 => Run::Pty((0..r.below(3)).map(|_| command(r).0).filter(|c| !c.starts_with("sshd stop")).collect()),
        _ => {
            let (c, out) = command(r);
            Run::Exec(c, out)
        }
    };
    /* A window of a byte or a hundred is for what prints little: a hundred
     * thousand round trips is a long time to spend on one client. */
    let big = matches!(&run, Run::Exec(c, _) if c.starts_with("spew ") && c.len() > 9);
    /* Past the window is past it only where nothing takes what came in:
     * a shell takes it as typing, and gives the window back. */
    let brk = if brk == Break::Overrun && !matches!(run, Run::Exec(..)) { Break::None } else { brk };
    let window = if big { r.pick(&[0x20_0000u32, 64 * 1024, 4096]) }
                 else { r.pick(&[0x20_0000u32, 64 * 1024, 32 * 1024, 4096, 100, 1]) };
    let max_packet = r.pick(&[32 * 1024u32, 16 * 1024, 1024, 100, 1]);
    let adjust_after = match r.u8() % 3 {
        0 => 1,
        1 => window / 2 + 1,
        _ => window,
    };
    Plan { tries, brk, run, window, max_packet, adjust_after: adjust_after.max(1),
           rekey_after: (r.u8() < 48).then(|| 1 + r.below(8) as usize), answers_keepalive: r.u8() < 224,
           unknown: r.u8() < 32, global: r.u8() < 32 }
}

/// What a client's connection must have come to, now that it is over.
fn judge(c: &Conn, lossless: bool) {
    let cl = &c.client;
    let p = &cl.plan;
    let ending = cl.ending.clone();
    /* Turned away at the door: a reset before a word. And over a link that
     * loses, the last word may be lost to the reset behind it. */
    if (c.crowded || !lossless) && ending == Some(Ending::Gone) && !cl.logged_in {
        return;
    }
    if !lossless && ending == Some(Ending::Gone) {
        return;
    }
    let expect_disconnect = |code: u32| {
        invariant!(ending == Some(Ending::Disconnect(code)) && !cl.logged_in,
                   "a client that {:?} ended {:?}{}, not told {}", p.brk, ending,
                   if cl.logged_in { ", logged in" } else { "" }, code);
    };
    match p.brk {
        Break::Version | Break::Silent => expect_disconnect(BY_APPLICATION),
        Break::NoCipher => expect_disconnect(KEY_EXCHANGE_FAILED),
        Break::SmallPoint | Break::Length | Break::IgnoreFirst | Break::ChannelEarly => expect_disconnect(PROTOCOL_ERROR),
        Break::Tamper => expect_disconnect(MAC_ERROR),
        /* A command may finish, and the session close, before the server
         * reads what was wrong -- or a client that answers no keepalive be
         * let go for that first. */
        Break::Overrun | Break::NoChannel => {
            invariant!(matches!(ending, Some(Ending::Disconnect(PROTOCOL_ERROR)) | Some(Ending::Closed)
                                        | Some(Ending::Disconnect(BY_APPLICATION)) | None)
                       || !cl.logged_in, "a client that sent {:?} ended {:?}", p.brk, ending);
        }
        Break::None => {
            /* The tries that come before the known key's signature, and
             * which of them the server counts. */
            let before = p.tries.iter().take_while(|t| **t != Try::Sign(Who::Known));
            let counted = before.filter(|t| !matches!(t, Try::None | Try::Query(Who::Known))).count();
            let can = p.tries.contains(&Try::Sign(Who::Known)) && counted < MAX_AUTH_TRIES;
            /* Over a link that loses, a login may take longer than the
             * server gives one. */
            if can && !cl.logged_in && !lossless && ending == Some(Ending::Disconnect(BY_APPLICATION)) {
                return;
            }
            if !can {
                invariant!(!cl.logged_in, "logged in with the tries {:?}", p.tries);
                if counted >= MAX_AUTH_TRIES {
                    invariant!(ending == Some(Ending::Disconnect(NO_MORE_AUTH_METHODS)),
                               "{} failed tries ended {:?}", counted, ending);
                }
                return;
            }
            invariant!(cl.logged_in, "the known key never logged in: tries {:?}, ended {:?}", p.tries, ending);
            if ending != Some(Ending::Closed) || cl.cut {
                return;
            }
            match &p.run {
                Run::Exec(cmd, Some(want)) => {
                    invariant!(cl.output == *want && cl.exit_status == Some(0),
                               "'{}' came back {} bytes {:?}..., status {:?}, it printing {} {:?}...", cmd,
                               cl.output.len(), String::from_utf8_lossy(&cl.output[..cl.output.len().min(40)]),
                               cl.exit_status, want.len(), String::from_utf8_lossy(&want[..want.len().min(40)]));
                }
                Run::Shell(lines) if lines.iter().all(|l| l.1.is_some()) => {
                    let want: Vec<u8> = lines.iter().flat_map(|l| l.1.clone().unwrap_or_default()).collect();
                    invariant!(cl.output == want, "a shell's lines came back {} bytes {:?}..., them printing {} {:?}...",
                               cl.output.len(), String::from_utf8_lossy(&cl.output[..cl.output.len().min(40)]),
                               want.len(), String::from_utf8_lossy(&want[..want.len().min(40)]));
                }
                _ => {}
            }
        }
    }
}

pub fn ssh(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);

    let known = Client::key(Who::Known).verifying_key().to_bytes();
    let line = format!("{} fuzz@world\n", ssh::PublicKey { key: known }.to_line());
    /* The root, the key it lets in, the module, the server: as /etc/rc
     * brings them up. */
    let setup = App::spawn("setup", 1, move || {
        let mounted = fs::ramfs::mount_at("/", false);
        fs::init();
        let written = kcore::fs::create_dir("/etc").and_then(|_| kcore::fs::create_dir("/etc/ssh"))
            .and_then(|_| kcore::fs::create("/etc/ssh/authorized_keys", line.as_bytes()));
        let module = sshd::insmod();
        let out = cmd::run("sshd start");
        (mounted, written.is_ok(), module.ok(), out)
    });
    let link = if r.u8() < 216 { Link::perfect() } else { Link::from_input(r) };
    let lossless = link.lossless();
    let mut lan = Lan::new();
    lan.add(CLIENT_IP, CLIENT_MAC);
    let conns = Rc::new(RefCell::new(Vec::new()));
    let mut w = World::new(link, Wire { lan, tcp: Tcp::new(CLIENT_IP, CLIENT_MAC), conns: conns.clone() });
    invariant!(w.wait_app(&setup, 60 * SEC), "the setup never finished");
    let (mounted, written, module, out) = setup.take().expect("done");
    invariant!(mounted && written && module.is_some() && out.contains("listening on port 22"),
               "sshd would not start: mounted {} key written {} loaded {} -- {:?}", mounted, written,
               module.is_some(), out);
    let mut module = module;

    let mut next_id = 1usize;
    while let Some(op) = r.op(8) {
        match op {
            0..=2 if conns.borrow().len() < CLIENTS => {
                /* A client connects, with its plan. */
                let plan = plan(r);
                let now = sched::now();
                let live: Vec<bool> = conns.borrow().iter()
                    .filter(|c: &&Conn| c.over_at.is_none_or(|t| now < t + REAP))
                    .map(|c| c.client.logged_in).collect();
                let logging_in = live.iter().filter(|l| !**l).count();
                let crowded = logging_in >= MAX_LOGGING_IN || live.len() >= MAX_SESSIONS;
                let id = next_id;
                next_id += 1;
                let port = 40000 + id as u16;
                let iss = r.u32();
                let window = r.pick(&[65535u32, 8192, 1024]);
                let ep = w.peers.tcp.connect(&mut w.net, 50_000 + id, port, ETH0_IP, PORT, iss,
                                             crate::world::frames::mss(1460), window);
                w.peers.tcp.eps[ep].data = Some(Data::default());
                let seed = r.u64();
                conns.borrow_mut().push(Conn { client: Client::new(ep, plan, seed), started: false,
                                               at: sched::now(), over_at: None, crowded });
                w.pump();
            }
            3 => {
                /* The console asks the server how it is. */
                let app = App::spawn("sshd-status", 1, || cmd::run("sshd"));
                invariant!(w.wait_app(&app, 30 * SEC), "`sshd` never returned");
            }
            _ => {
                let step = match r.u8() % 8 {
                    0..=5 => r.below(2000) * MS,
                    6 => r.below(40) * SEC,
                    _ => r.pick(&[61u64, 180, 250]) * SEC,
                };
                w.run_for(step);
            }
        }
        for c in conns.borrow().iter().filter(|c| c.client.over()) {
            judge(c, lossless);
        }
    }

    /* Every client that is still at it finishes, over a good link: a session
     * closes its channel, a login gives up. */
    w.net.link = Link::perfect();
    let end = sched::now() + 600 * SEC;
    let all_over = |conns: &Rc<RefCell<Vec<Conn>>>| conns.borrow().iter().all(|c| c.client.over());
    w.run_until(sched::now() + 60 * SEC, || all_over(&conns));
    {
        let mut v = conns.borrow_mut();
        let wire = &mut w.peers;
        for c in v.iter_mut() {
            if !c.client.over() && c.started {
                c.client.finish(&mut wire.tcp, &mut w.net);
            }
        }
    }
    let over = w.run_until(end, || all_over(&conns));
    for c in conns.borrow().iter() {
        invariant!(c.client.over() || !lossless, "a client from {} s never done: phase {:?}, logged in {}, plan \
                   {:?}", (c.at - sched::START) / SEC, c.client.phase, c.client.logged_in, c.client.plan.brk);
        if c.client.over() {
            judge(c, lossless);
        }
    }
    let _ = over;

    /* The server stops, the module goes, and nothing is left behind. */
    let app = App::spawn("sshd-stop", 1, || cmd::run("sshd stop"));
    invariant!(w.wait_app(&app, 120 * SEC), "`sshd stop` never returned");
    let m = module.take();
    let app = App::spawn("rmmod", 1, move || drop(m));
    invariant!(w.wait_app(&app, 120 * SEC), "rmmod sshd never returned");
    for e in 0..w.peers.tcp.eps.len() {
        if w.peers.tcp.eps[e].open() {
            w.peers.tcp.close(&mut w.net, e);
        }
    }
    w.run_for(200 * SEC);
    super::audit();
}
