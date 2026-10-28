//! `tls_server`: the kernel's TLS server (`tls::server`) -- what the host API
//! module serves its clients over -- run as the module will run it: a
//! listener and a task a connection, over the kernel's TCP, letting in the
//! clients whose keys it was given and answering each line said to it.
//!
//! The world's clients (`tlsc`) connect as the input says, a few at a time:
//! with the key the server knows, a stranger's, none, or the known key's
//! certificate and a signature by another; speaking TLS 1.3, or
//! 1.2 alone; offering the API's protocol, HTTP/1.1, both, none, or only one
//! the server does not speak; or breaking the protocol -- an HTTP request
//! where the ClientHello would be, nothing at all, a ClientHello and then
//! goodbye, a record of data tampered with.
//!
//! What must hold of every connection: the server lets in exactly the
//! clients with the known key that speak TLS 1.3 and a protocol it has --
//! and never another, whatever the link does: no key but the known one is
//! ever let in, and no client without it is ever answered a byte. A client
//! let in over a good link gets every line back, in order, and is told
//! goodbye; one refused is refused for the reason its plan gives, and reset;
//! one that says nothing is let go when the handshake's time is up. The
//! server never closes first, so it holds no slot in TIME-WAIT; and once
//! every client has gone and the server stops, the machine holds nothing.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kcore::tcp::{TcpListener, TcpStream, RECV_TIMEOUT};
use tls::server::{Fingerprint, Read, Recv, Refused, Server, Socket, HANDSHAKE_MS};

use crate::machine::sched;
use crate::world::lan::Lan;
use crate::world::tcpm::{Data, Tcp, S};
use crate::world::tlsc::{keys, Break, Client, Ending, Plan, Who};
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

const PORT: u16 = 39234;
const CLIENT_IP: u32 = 0x0A00_0264;
const CLIENT_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x64];
/// What the server speaks by ALPN, in its order: the API's.
const ALPN: [&[u8]; 2] = [b"nos-api/1", b"http/1.1"];
/// Connections the server serves at once; past them it resets.
const MAX_CONNS: usize = 4;
/// A connection let in that says nothing for this long is let go.
const IDLE_MS: u64 = 60_000;
/// What waits to be sent, at most, before the server flushes.
const MAX_PENDING: usize = 64 * 1024;
/// Clients an input makes at most.
const CLIENTS: usize = 16;

/// What the server made of a connection, by the client's port.
#[derive(Clone, Debug)]
enum Seen {
    Accepted { port: u16, key: Fingerprint, alpn: Option<Vec<u8>>, lines: usize, ended: &'static str },
    Refused { port: u16, why: Refused },
    /// Turned away at the door: more than it serves at once.
    Busy { port: u16 },
}

impl Seen {
    fn port(&self) -> u16 {
        match self {
            Seen::Accepted { port, .. } | Seen::Refused { port, .. } | Seen::Busy { port } => *port,
        }
    }
}

/// The server's side: what it was asked to stop by, and what it saw.
struct Side {
    stop: AtomicBool,
    seen: Mutex<Vec<Seen>>,
    server: Server,
}

/// The kernel's TCP, as a TLS session runs over it.
struct Conn(TcpStream);

impl Socket for Conn {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Recv {
        match self.0.recv(buf, wait_ms.max(1)) {
            n if n > 0 => Recv::Data(n as usize),
            0 => Recv::End,
            RECV_TIMEOUT => Recv::Idle,
            _ => Recv::Failed,
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        let n = self.0.send(buf, wait_ms.max(1));
        if n >= 0 { Some(n as usize) } else { None }
    }
}

/// A connection, served: its handshake, and each line said to it answered.
fn serve((side, conn, done): (Arc<Side>, TcpStream, Arc<AtomicBool>)) {
    let (_, port) = conn.peer();
    match side.server.accept(Conn(conn), HANDSHAKE_MS) {
        Err((sock, why)) => {
            side.seen.lock().unwrap().push(Seen::Refused { port, why });
            sock.0.abort();
        }
        Ok(mut s) => {
            let key = *s.client();
            let alpn = s.alpn().map(|a| a.to_vec());
            let mut line = Vec::new();
            let mut lines = 0;
            let mut idle = 0;
            let mut buf = [0u8; 700];
            let ended = loop {
                match s.read(&mut buf, 1000) {
                    Read::Data(n) => {
                        idle = 0;
                        for &b in &buf[..n] {
                            line.push(b);
                            if b == b'\n' {
                                let reply = [b"ok ".as_slice(), &line].concat();
                                while !s.write(&reply, MAX_PENDING) {
                                    if s.flush(1000) == tls::server::Flush::Failed {
                                        break;
                                    }
                                }
                                line.clear();
                                lines += 1;
                            }
                        }
                        if s.flush(5000) == tls::server::Flush::Failed {
                            break "failed";
                        }
                    }
                    Read::Idle => {
                        idle += 1000;
                        if idle >= IDLE_MS || side.stop.load(Ordering::Relaxed) {
                            break "idle";
                        }
                    }
                    Read::End => break "end",
                    Read::Failed => break "failed",
                }
            };
            side.seen.lock().unwrap().push(Seen::Accepted { port, key, alpn, lines, ended });
            /* The client said goodbye first: the server's goodbye, and its
             * FIN after the client's. Otherwise a reset, with no TIME-WAIT
             * left behind on the server's side */
            if ended == "end" {
                match s.close(2000) {
                    (sock, true) => drop(sock),
                    (sock, false) => sock.0.abort(),
                }
            } else {
                s.into_socket().0.abort();
            }
        }
    }
    done.store(true, Ordering::Release);
}

/// The listener: a task a connection, at most `MAX_CONNS` at once.
fn listen(side: Arc<Side>) -> bool {
    let Some(nic) = kcore::net::Nic::find("eth0") else { return false };
    let Ok(listener) = TcpListener::bind(&nic, PORT, 4) else { return false };
    let mut conns: Vec<(kcore::task::TaskHandle, Arc<AtomicBool>)> = Vec::new();
    while !side.stop.load(Ordering::Relaxed) {
        conns.retain(|(_, done)| !done.load(Ordering::Acquire));
        let Some(conn) = listener.accept(250) else { continue };
        if conns.len() >= MAX_CONNS {
            let (_, port) = conn.peer();
            side.seen.lock().unwrap().push(Seen::Busy { port });
            conn.abort();
            continue;
        }
        let done = Arc::new(AtomicBool::new(false));
        match kcore::task::spawn_with("tls/conn", (side.clone(), conn, done.clone()), serve) {
            Some(task) => conns.push((task, done)),
            None => return false,
        }
    }
    /* Every connection's task done before the listener goes */
    drop(conns);
    true
}

struct Wire {
    lan: Lan,
    tcp: Tcp,
    clients: Rc<RefCell<Vec<Client>>>,
}

impl Wire {
    fn tend(&mut self, net: &mut Net) {
        for c in self.clients.borrow_mut().iter_mut() {
            let st = self.tcp.eps[c.ep].st;
            if matches!(st, S::Established | S::CloseWait) {
                c.start(&mut self.tcp, net);
            }
            c.pump(&mut self.tcp, net);
            if c.ending.is_none() && (!self.tcp.eps[c.ep].open() || self.tcp.eps[c.ep].reset.is_some()) {
                c.ending = Some(Ending::Gone);
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

fn plan(r: &mut Input) -> Plan {
    let who = match r.u8() % 8 {
        0 => Who::Stranger,
        1 => Who::Nobody,
        2 => Who::Impostor,
        _ => Who::Known,
    };
    let brk = match r.u8() % 24 {
        0 => Break::Garbage,
        1 => Break::Silent,
        2 => Break::Cut,
        3 => Break::Tamper,
        4 => Break::Tls12,
        5 => Break::Alpn,
        _ => Break::None,
    };
    let alpn: Vec<&'static [u8]> = match (brk, r.u8() % 5) {
        (Break::Alpn, _) => vec![b"h2"],
        (_, 0) => vec![],
        (_, 1) => vec![b"http/1.1"],
        (_, 2) => vec![b"h2", b"http/1.1", b"nos-api/1"],
        _ => vec![b"nos-api/1"],
    };
    let lines = (0..r.below(6)).map(|i| {
        let len = r.pick(&[1usize, 10, 100, 600]);
        let mut l: Vec<u8> = (0..len).map(|k| b'a' + ((i as usize * 7 + k) % 26) as u8).collect();
        l.push(b'\n');
        l
    }).collect();
    Plan { who, brk, alpn, lines }
}

/// What the server made of a client, by its port.
fn seen_of(seen: &[Seen], port: u16) -> Option<Seen> {
    seen.iter().find(|s| s.port() == port).cloned()
}

/// What must hold of every connection, over any link: no key but the known
/// one let in, and nothing said to a client without it.
fn hold_always(c: &Client, s: &Option<Seen>, known: &Fingerprint) {
    if let Some(Seen::Accepted { key, .. }) = s {
        invariant!(key == known, "a key the server was not given was let in: {:?}", c.plan);
        invariant!(c.plan.who == Who::Known, "a client showing {:?} was let in", c.plan.who);
    }
    if c.plan.who != Who::Known {
        invariant!(c.got.is_empty(), "a client showing {:?} was answered {} bytes", c.plan.who, c.got.len());
    }
}

/// What a client's connection must have come to, over a link that loses
/// nothing, now that it is over.
fn judge(c: &Client, s: &Option<Seen>, known: &Fingerprint) {
    hold_always(c, s, known);
    let p = &c.plan;
    if matches!(s, Some(Seen::Busy { .. })) {
        return;
    }
    let refused = |want: &[Refused]| {
        invariant!(matches!(s, Some(Seen::Refused { why, .. }) if want.contains(why)),
                   "a client {:?} {:?} offering {:?} was seen as {:?}, not refused as {:?}", p.who, p.brk,
                   p.alpn.iter().map(|a| String::from_utf8_lossy(a)).collect::<Vec<_>>(), s, want);
    };
    match p.brk {
        Break::Garbage => refused(&[Refused::Protocol]),
        Break::Silent => refused(&[Refused::Timeout]),
        Break::Cut => refused(&[Refused::Closed]),
        Break::Tls12 => refused(&[Refused::Incompatible]),
        Break::Alpn => refused(&[Refused::Incompatible]),
        Break::Tamper | Break::None => match p.who {
            Who::Stranger => refused(&[Refused::UnknownKey]),
            Who::Nobody => refused(&[Refused::NoCertificate]),
            /* Its certificate is the known key's, but it does not hold it:
             * the handshake's signature gives it away */
            Who::Impostor => refused(&[Refused::BadCertificate]),
            /* A record tampered with fails the session where it is read:
             * after the lines before it were answered -- or in the
             * handshake, when it is the first and came in the flight of the
             * client's Finished */
            Who::Known if p.brk == Break::Tamper && p.lines.len() >= 2 => {
                let n = p.lines.len();
                invariant!(matches!(s, Some(Seen::Accepted { ended: "failed", lines, .. }) if *lines == n - 1),
                           "a tampered last line of {} was taken, or more than the lines before it: seen {:?}", n, s);
                invariant!(c.got == c.answers_of(n - 1), "the lines before a tampered one came back {} bytes, {} \
                           expected", c.got.len(), c.answers_of(n - 1).len());
            }
            Who::Known if p.brk == Break::Tamper && !p.lines.is_empty() => {
                invariant!(matches!(s, Some(Seen::Refused { why: Refused::Protocol, .. })
                                       | Some(Seen::Accepted { ended: "failed", .. })),
                           "a tampered record was taken: seen {:?}", s);
            }
            Who::Known => {
                let Some(Seen::Accepted { alpn, lines, ended, .. }) = s else {
                    invariant!(false, "the known key was not let in: {:?} {:?}, seen {:?}", p.brk, p.alpn, s);
                    return;
                };
                let agreed = ALPN.iter().find(|a| p.alpn.contains(a)).map(|a| a.to_vec());
                invariant!(*alpn == agreed, "agreed on {:?} where the client offered {:?}", alpn, p.alpn);
                invariant!(c.got == c.answers(), "the answers came back {} bytes, {} expected ({} lines, {} said)",
                           c.got.len(), c.answers().len(), p.lines.len(), lines);
                invariant!(*lines == p.lines.len() && *ended == "end",
                           "the server heard {} lines of {} and ended {}", lines, p.lines.len(), ended);
                invariant!(c.ending == Some(Ending::Closed), "a client let in ended {:?}", c.ending);
            }
        },
    }
}

pub fn tls_server(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);

    let keys = keys();
    let known = tls::server::fingerprint(&keys.known.cert).expect("the known key's certificate");
    let server = Server::new(&keys.server.cert, &keys.server.key, &[known], &ALPN).expect("the server's setup");
    let side = Arc::new(Side { stop: AtomicBool::new(false), seen: Mutex::new(Vec::new()), server });
    let listener = {
        let side = side.clone();
        App::spawn("tls/listen", 1, move || listen(side))
    };

    let link = if r.u8() < 216 { Link::perfect() } else { Link::from_input(r) };
    let lossless = link.lossless();
    let mut lan = Lan::new();
    lan.add(CLIENT_IP, CLIENT_MAC);
    let clients = Rc::new(RefCell::new(Vec::new()));
    let mut w = World::new(link, Wire { lan, tcp: Tcp::new(CLIENT_IP, CLIENT_MAC), clients: clients.clone() });
    w.run_for(100 * MS);

    let mut next_id = 1usize;
    let mut judged = Vec::new();
    let mut port_of = Vec::new();
    while let Some(op) = r.op(6) {
        match op {
            0..=2 if clients.borrow().len() < CLIENTS => {
                let plan = plan(r);
                let id = next_id;
                next_id += 1;
                let port = 40000 + id as u16;
                let iss = r.u32();
                let window = r.pick(&[65535u32, 8192, 1024]);
                let ep = w.peers.tcp.connect(&mut w.net, 50_000 + id, port, ETH0_IP, PORT, iss,
                                             crate::world::frames::mss(1460), window);
                w.peers.tcp.eps[ep].data = Some(Data::default());
                clients.borrow_mut().push(Client::new(ep, plan));
                port_of.push(port);
                judged.push(false);
                w.pump();
            }
            _ => {
                let step = match r.u8() % 4 {
                    0..=2 => r.below(2000) * MS,
                    _ => r.below(15) * SEC,
                };
                w.run_for(step);
            }
        }
    }

    /* Every client finishes, over a good link: the server's handshake time
     * runs out on the silent, and every other has its answers or its
     * refusal */
    w.net.link = Link::perfect();
    let all_over = |clients: &Rc<RefCell<Vec<Client>>>| clients.borrow().iter().all(|c| c.over());
    w.run_until(sched::now() + 2 * HANDSHAKE_MS * MS + 30 * SEC, || all_over(&clients));
    /* What the server saw of each, once its task is done with it */
    w.run_for(3 * SEC);
    let seen = side.seen.lock().unwrap().clone();
    for (i, c) in clients.borrow().iter().enumerate() {
        let s = seen_of(&seen, port_of[i]);
        if lossless {
            invariant!(c.over(), "a client from {} s never done: {:?}, up {}, sent {} lines, got {} bytes",
                       (c.at - sched::START) / SEC, c.plan, c.up, c.lines_sent, c.got.len());
            judge(c, &s, &known);
        } else {
            hold_always(c, &s, &known);
        }
        judged[i] = true;
    }

    /* The server never closes first: no slot of its port in TIME-WAIT */
    for i in 0..net::tcp::MAX_CONNECTIONS {
        if let Some(c) = net::tcp::TCP.snapshot(i) {
            if c.local_port == PORT && c.state == net::tcp::State::TimeWait {
                let at = port_of.iter().position(|p| *p == c.remote_port);
                let clients = clients.borrow();
                let client = at.map(|i| (&clients[i].plan, &clients[i].ending, clients[i].up));
                invariant!(false, "the server's side of a connection from port {} is in TIME-WAIT: the client {:?}, \
                           seen {:?}", c.remote_port, client, seen_of(&seen, c.remote_port));
            }
        }
    }

    /* The server stops, and nothing is left behind */
    side.stop.store(true, Ordering::Relaxed);
    invariant!(w.wait_app(&listener, 120 * SEC), "the listener never stopped");
    invariant!(listener.take() == Some(true), "the listener could not start or spawn");
    for e in 0..w.peers.tcp.eps.len() {
        if w.peers.tcp.eps[e].open() {
            w.peers.tcp.close(&mut w.net, e);
        }
    }
    w.run_for(200 * SEC);
    super::audit();
}
