//! A connection to a peer, and the task that runs it: the TCP connection
//! -- dialed, or taken by the listener -- with a link's 20 ms floor for its
//! retransmissions; TLS 1.3 with both keys pinned; the `Hello` each way;
//! the session joined; and then, until it ends, frames both ways.
//!
//! Out, small frames go first and messages after, a frame at a time and the
//! messages begun in turn, so that a megabyte never stands in front of a
//! Pong or another pair's message; at most OPEN_MAX of them begun at once
//! and OPEN_BYTES_MAX of their bytes promised, which is what the other end's
//! assembler holds this end to. Two messages of one pair -- one sender, one
//! destination -- are never begun at once: the other end hands a message
//! over as its last frame comes, and the channel promises a pair its order. In, frames are decoded as the bytes come,
//! and a message put back together is handed to the channel at once: a
//! mailbox full is a `Bounced` back, never a connection that stops reading.
//! Nothing heard for LOST_MS -- a Ping goes every PING_MS each way -- or a
//! frame the wire refuses, and the peer is lost.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ipcwire::{Assembler, Dest, Frame, To};
use kcore::ipc::{Addr, Bounce, Reason, RemoteTo};
use kcore::tcp::{TcpStream, RECV_TIMEOUT};
use tls::pinned::{Flush, Read, Recv, Socket};

use super::peer::{add, OutMsg, Peer, BULK, CLASSES, CONTROL, FRAMES_MAX, MONITORS_MAX};
use super::server::Shared;

/// What a connection says it is, by ALPN: the control connection or the
/// bulk one of a pair.
pub const ALPN: [&[u8]; CLASSES] = [b"nos-ipc/1", b"nos-ipc-bulk/1"];
/// A link's floor for its retransmissions: both ends are nos, which
/// acknowledges every segment at once (plans/tcp.md, T2).
const LINK_RTO_MS: u32 = 20;
/// A handshake's time, and the Hello's after it.
pub const HANDSHAKE_MS: u64 = 3000;
const HELLO_MS: u64 = 3000;
/// How long a connection waits for the other of its session.
const ATTACH_MS: u64 = 3000;
const ATTACH_POLL_MS: u64 = 5;
/// A Ping each way this often; nothing heard for LOST_MS and the peer is
/// lost.
pub const PING_MS: u64 = 500;
pub const LOST_MS: u64 = 3000;
/// The longest a socket's wait is: how soon a stop is seen.
const POLL_MS: u64 = 100;
/// TLS bytes waiting to go on a connection before it takes no more frames.
const MAX_PENDING: usize = 64 * 1024;
/// A frame always fits once what waits is under MAX_PENDING.
const WRITE_ROOM: usize = MAX_PENDING + ipcwire::FRAME_MAX + 1024;
/// Messages begun on a connection and not yet sent whole, and the bytes
/// their heads promise: the other end's assembler is held to the same.
pub const OPEN_MAX: usize = 8;
pub const OPEN_BYTES_MAX: usize = 4 << 20;
/// The pieces the other end's open messages are kept in here, at most: a
/// body each. This end's bodies are a frame long but where a piece of the
/// router's ends -- some 80 to a megabyte, well under this for OPEN_MAX of
/// them -- and a peer sending a byte a frame is refused long before its
/// pieces cost more than their bytes.
const PIECES_MAX: usize = 2048;
/// A read's buffer, and the most of a frame kept waiting for its rest.
const READ_BYTES: usize = 16 * 1024;
/// Reads a turn of the loop takes at most while the session holds more of
/// what came: a bulk connection's receive buffer's worth of records.
const DRAIN_READS: usize = 32;
/// The goodbye's time: the close_notify, and at the server's end the FIN.
const CLOSE_MS: u64 = 500;
const NS_PER_MS: u64 = 1_000_000;

/// The kernel's TCP, as a TLS session runs over it, and the module's stop:
/// a wait is never longer than POLL_MS, and once the module stops every one
/// fails, so a handshake under way ends at once.
pub struct Sock {
    pub tcp: TcpStream,
    shared: Arc<Shared>,
}

impl Socket for Sock {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Recv {
        if self.shared.stop.load(Ordering::Acquire) {
            return Recv::Failed;
        }
        match self.tcp.recv(buf, wait_ms.clamp(1, POLL_MS)) {
            n if n > 0 => Recv::Data(n as usize),
            0 => Recv::End,
            RECV_TIMEOUT => Recv::Idle,
            _ => Recv::Failed,
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        if self.shared.stop.load(Ordering::Acquire) {
            return None;
        }
        let n = self.tcp.send(buf, wait_ms.clamp(1, POLL_MS));
        if n >= 0 { Some(n as usize) } else { None }
    }
}

/// A session either way, and what both do with it.
enum Session {
    Server(tls::pinned::ServerStream<Sock>),
    Client(tls::pinned::ClientStream<Sock>),
}

impl Session {
    fn read(&mut self, buf: &mut [u8], wait_ms: u64) -> Read {
        match self {
            Session::Server(s) => s.read(buf, wait_ms),
            Session::Client(s) => s.read(buf, wait_ms),
        }
    }

    fn write(&mut self, data: &[u8], max_pending: usize) -> bool {
        match self {
            Session::Server(s) => s.write(data, max_pending),
            Session::Client(s) => s.write(data, max_pending),
        }
    }

    fn flush(&mut self, wait_ms: u64) -> Flush {
        match self {
            Session::Server(s) => s.flush(wait_ms),
            Session::Client(s) => s.flush(wait_ms),
        }
    }

    fn pending(&self) -> usize {
        match self {
            Session::Server(s) => s.pending(),
            Session::Client(s) => s.pending(),
        }
    }

    fn buffered(&self) -> bool {
        match self {
            Session::Server(s) => s.buffered(),
            Session::Client(s) => s.buffered(),
        }
    }

    fn waker(&self) -> kcore::tcp::TcpWaker {
        match self {
            Session::Server(s) => s.socket().tcp.waker(),
            Session::Client(s) => s.socket().tcp.waker(),
        }
    }

    fn failure(&self) -> Option<tls::pinned::Failure> {
        match self {
            Session::Server(s) => s.failure(),
            Session::Client(s) => s.failure(),
        }
    }

    /// Its goodbye, and the connection closed -- or reset, when the goodbye
    /// did not go.
    fn close(self) {
        let (sock, ok) = match self {
            Session::Server(s) => s.close(CLOSE_MS),
            Session::Client(s) => s.close(CLOSE_MS),
        };
        if !ok {
            sock.tcp.abort();
        }
    }

    /// Reset, nothing more said.
    fn reset(self) {
        let sock = match self {
            Session::Server(s) => s.into_socket(),
            Session::Client(s) => s.into_socket(),
        };
        sock.tcp.abort();
    }
}

/// What a peer is lost by when its session fails: the session's own word
/// for why -- a connection reset and a record whose bytes changed on the
/// way are not the same loss.
fn failed(sess: &Session) -> &'static str {
    sess.failure().map_or("the session failed", tls::pinned::Failure::said)
}

/// Where a connection's task starts.
pub enum Side {
    /// Dialing the peer, for `session`, the connection of `class`.
    Dial { peer: Arc<Peer>, class: usize, session: u64 },
    /// A connection the listener took, its handshake counted from `ip`.
    Accepted { stream: TcpStream, ip: u32 },
}

pub struct LinkCtx {
    pub shared: Arc<Shared>,
    pub side: Side,
    /// Set as the task ends: what the listener reaps its tasks by.
    pub done: Option<Arc<AtomicBool>>,
}

/// A connection's task.
pub fn link_main(ctx: LinkCtx) {
    let LinkCtx { shared, side, done } = ctx;
    serve(&shared, side);
    if let Some(d) = done {
        d.store(true, Ordering::Release);
    }
}

fn serve(shared: &Arc<Shared>, side: Side) {
    let up = match side {
        Side::Dial { peer, class, session } => dial(shared, peer, class, session),
        Side::Accepted { stream, ip } => accept(shared, stream, ip),
    };
    let Some((mut sess, peer, class, session)) = up else { return };
    let why = run(shared, &mut sess, &peer, class, session);
    peer.lose(&shared.router, session, why);
    if why == "stopping" || why == "the session ended" {
        sess.close();
    } else {
        sess.reset();
    }
}

/// What a dial or an accept hands the loop: the session, the peer, the
/// class and the session's number.
type Up = Option<(Session, Arc<Peer>, usize, u64)>;

/// The connection of `class` for `session` dialed, its handshake made, its
/// Hello said and heard, and the peer's session joined.
fn dial(shared: &Arc<Shared>, peer: Arc<Peer>, class: usize, session: u64) -> Up {
    let ip = peer.conf.ip;
    let stream = if class == BULK {
        TcpStream::connect_bulk(&shared.nic, ip, shared.cfg.port)
    } else {
        TcpStream::connect(&shared.nic, ip, shared.cfg.port)
    };
    let Some(stream) = stream else {
        peer.lose(&shared.router, session, "no answer");
        return None;
    };
    let _ = stream.set_rto_floor(LINK_RTO_MS);
    let sock = Sock { tcp: stream, shared: shared.clone() };
    let clients = peer.clients();
    let mut sess = match clients[class].connect(sock, ip, HANDSHAKE_MS) {
        Ok(s) => Session::Client(s),
        Err((sock, why)) => {
            sock.tcp.abort();
            add(&shared.counters.refused_out, 1);
            kcore::klog!(0, "ipcnet: {} refused this host's handshake: {}", peer.conf.name, why.name());
            peer.lose(&shared.router, session, "handshake refused");
            return None;
        }
    };
    let hello = Frame::Hello { version: ipcwire::VERSION, cluster: shared.cfg.cluster, session,
                               from: shared.cfg.name.as_bytes(), to: peer.conf.name.as_bytes() };
    if !say(&mut sess, &hello) {
        sess.reset();
        peer.lose(&shared.router, session, "hello not sent");
        return None;
    }
    let mut inbuf = Vec::new();
    let heard = match hear_hello(&mut sess, &mut inbuf) {
        Some(Heard { version, cluster, session: s, from, to }) => {
            version == ipcwire::VERSION && cluster == shared.cfg.cluster && s == session
                && from == peer.conf.name.as_bytes() && to == shared.cfg.name.as_bytes()
        }
        None => false,
    };
    if !heard {
        add(&shared.counters.hello_refused, 1);
        sess.reset();
        peer.lose(&shared.router, session, "its hello not this cluster's");
        return None;
    }
    if !inbuf.is_empty() || peer.attach(&shared.router, class, session, false).is_err() {
        sess.reset();
        peer.lose(&shared.router, session, "not let into its session");
        return None;
    }
    if !wait_up(shared, &peer, session) {
        sess.reset();
        peer.lose(&shared.router, session, "its other connection never came");
        return None;
    }
    Some((sess, peer, class, session))
}

/// A connection the listener took: the handshake, the peer it is by its key
/// and its Hello, this host's Hello said, and the peer's session joined --
/// a new one, which ends the peer's old.
fn accept(shared: &Arc<Shared>, stream: TcpStream, ip: u32) -> Up {
    /* The listener's count of handshakes under way, `ip`'s among them, given
     * back once the TLS handshake is over -- however it ends */
    let slot = Handshake { shared, ip };
    /* The link's floor on this end too: the accepting host sends as much as
     * the dialing one -- a reply, an echo -- and its losses waited out 200 ms
     * each where the dialer's waited out 20 */
    let _ = stream.set_rto_floor(LINK_RTO_MS);
    let server = shared.server.with(|s| s.clone());
    let sock = Sock { tcp: stream, shared: shared.clone() };
    let got = server.accept(sock, HANDSHAKE_MS);
    drop(server);
    drop(slot);
    let s = match got {
        Ok(s) => s,
        Err((sock, why)) => {
            sock.tcp.abort();
            add(&shared.counters.refused_in, 1);
            kcore::klog!(0, "ipcnet: a handshake from {} refused: {}", super::files::Ip(ip), why.name());
            return None;
        }
    };
    let key = *s.client();
    let class = match s.alpn() {
        Some(p) if p == ALPN[CONTROL] => CONTROL,
        Some(p) if p == ALPN[BULK] => BULK,
        _ => {
            s.into_socket().tcp.abort();
            add(&shared.counters.hello_refused, 1);
            return None;
        }
    };
    let mut sess = Session::Server(s);
    let mut inbuf = Vec::new();
    let Some(h) = hear_hello(&mut sess, &mut inbuf) else {
        add(&shared.counters.hello_refused, 1);
        sess.reset();
        return None;
    };
    /* The peer it says it is, which its key and its address must be too --
     * and one that dials this host, never one it dials */
    let peer = shared.peers.with(|v| {
        v.iter().find(|p| p.conf.name.as_bytes() == h.from && !p.removed.load(Ordering::Acquire)).cloned()
    });
    let ok = match &peer {
        Some(p) => h.version == ipcwire::VERSION && h.cluster == shared.cfg.cluster && h.session != 0
            && h.to == shared.cfg.name.as_bytes() && p.pinned(&key) && p.conf.ip == ip && !p.dials,
        None => false,
    };
    let (Some(peer), true) = (peer, ok) else {
        add(&shared.counters.hello_refused, 1);
        kcore::klog!(0, "ipcnet: {} said a hello this host does not take: not its key, its address or its \
                         cluster", super::files::Ip(ip));
        sess.reset();
        return None;
    };
    let session = h.session;
    if !inbuf.is_empty() || peer.attach(&shared.router, class, session, true).is_err() {
        sess.reset();
        return None;
    }
    let hello = Frame::Hello { version: ipcwire::VERSION, cluster: shared.cfg.cluster, session,
                               from: shared.cfg.name.as_bytes(), to: peer.conf.name.as_bytes() };
    if !say(&mut sess, &hello) {
        sess.reset();
        peer.lose(&shared.router, session, "hello not sent");
        return None;
    }
    if !wait_up(shared, &peer, session) {
        sess.reset();
        peer.lose(&shared.router, session, "its other connection never came");
        return None;
    }
    Some((sess, peer, class, session))
}

/// A handshake the listener let begin, given back when dropped.
struct Handshake<'a> {
    shared: &'a Shared,
    ip: u32,
}

impl Drop for Handshake<'_> {
    fn drop(&mut self) {
        self.shared.handshake_done(self.ip);
    }
}

/// A frame said, and flushed.
fn say(sess: &mut Session, frame: &Frame<'_>) -> bool {
    let mut b = Vec::new();
    if ipcwire::encode(frame, &mut b).is_err() || !sess.write(&b, WRITE_ROOM) {
        return false;
    }
    let until = kcore::time::boot_time_ns() + HELLO_MS * NS_PER_MS;
    loop {
        match sess.flush(POLL_MS) {
            Flush::Done => return true,
            Flush::More if kcore::time::boot_time_ns() < until => {}
            _ => return false,
        }
    }
}

/// A Hello, owned.
struct Heard {
    version: u16,
    cluster: [u8; 16],
    session: u64,
    from: Vec<u8>,
    to: Vec<u8>,
}

/// The first frame, within HELLO_MS: a Hello, or None. What came after it
/// is left in `inbuf`.
fn hear_hello(sess: &mut Session, inbuf: &mut Vec<u8>) -> Option<Heard> {
    let until = kcore::time::boot_time_ns() + HELLO_MS * NS_PER_MS;
    let mut buf = [0u8; 512];
    loop {
        match ipcwire::decode(inbuf) {
            Ok(Some((Frame::Hello { version, cluster, session, from, to }, used))) => {
                let h = Heard { version, cluster, session, from: from.to_vec(), to: to.to_vec() };
                inbuf.drain(..used);
                return Some(h);
            }
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => {}
        }
        if inbuf.len() > ipcwire::FRAME_MAX {
            return None;
        }
        let now = kcore::time::boot_time_ns();
        if now >= until {
            return None;
        }
        match sess.read(&mut buf, ((until - now) / NS_PER_MS).clamp(1, POLL_MS)) {
            Read::Data(n) => inbuf.extend_from_slice(&buf[..n]),
            Read::Idle => {}
            Read::End | Read::Failed => return None,
        }
    }
}

/// Until the peer is up in `session`, at most ATTACH_MS: whether it is.
fn wait_up(shared: &Shared, peer: &Peer, session: u64) -> bool {
    let until = kcore::time::boot_time_ns() + ATTACH_MS * NS_PER_MS;
    loop {
        if peer.number(session).is_some() {
            return true;
        }
        if !peer.current(session) || shared.stop.load(Ordering::Acquire) || kcore::time::boot_time_ns() >= until {
            return false;
        }
        kcore::task::sleep_ms(ATTACH_POLL_MS);
    }
}

/// A message begun on this connection: its number here, and how much of
/// its own bytes have gone -- the piece and the offset in it next.
struct Sending {
    num: u32,
    msg: OutMsg,
    piece: usize,
    off: usize,
    left: usize,
}

/// What the loop keeps of what it sends.
struct Sender {
    open: Vec<Sending>,
    promised: usize,
    next_num: u32,
    turn: usize,
}

impl Sender {
    /// The next frame to go, encoded into `b`: small frames first, then a
    /// message begun -- when there is room for one -- or the next piece of
    /// one begun, in turn.
    fn next(&mut self, peer: &Peer, class: usize, b: &mut Vec<u8>) -> bool {
        b.clear();
        let started = peer.out[class].with(|o| {
            if let Some(f) = o.frames.pop_front() {
                *b = f;
                return None;
            }
            /* Room for it, and none of its pair under way: a second one begun
             * beside the first could end first, and be delivered first */
            let fits = |m: &OutMsg| {
                self.open.len() < OPEN_MAX && self.promised + m.len <= OPEN_BYTES_MAX
                    && !self.open.iter().any(|s| s.msg.from == m.from && s.msg.to == m.to)
            };
            if o.msgs.front().is_some_and(fits) {
                let m = o.msgs.pop_front()?;
                o.bytes = o.bytes.saturating_sub(m.len);
                return Some(m);
            }
            None
        });
        if !b.is_empty() {
            return true;
        }
        if let Some(msg) = started {
            /* A number no message open here has */
            let mut num = self.next_num;
            while self.open.iter().any(|s| s.num == num) {
                num = num.wrapping_add(1);
            }
            self.next_num = num.wrapping_add(1);
            let to = match &msg.to {
                Dest::Addr(a) => To::Addr(*a),
                Dest::Name(n) => To::Name(n),
            };
            let head = Frame::Head { msg: num, to, from: msg.from, tag: msg.tag, reference: msg.reference,
                                     len: msg.len as u32 };
            if ipcwire::encode(&head, b).is_err() {
                return false;
            }
            add(&peer.counts.sent, 1);
            if msg.len > 0 {
                self.promised += msg.len;
                let left = msg.len;
                let off = msg.skip;
                self.open.push(Sending { num, msg, piece: 0, off, left });
            }
            return true;
        }
        if self.open.is_empty() {
            return false;
        }
        /* The next piece of the message whose turn it is */
        let i = self.turn % self.open.len();
        let s = &mut self.open[i];
        while s.piece < s.msg.pieces.len() && s.off >= s.msg.pieces[s.piece].len() {
            s.piece += 1;
            s.off = 0;
        }
        let Some(p) = s.msg.pieces.get(s.piece) else {
            /* Fewer bytes than its head said: not the router's, never */
            let done = self.open.swap_remove(i);
            self.promised = self.promised.saturating_sub(done.msg.len);
            return false;
        };
        let n = (p.len() - s.off).min(ipcwire::BODY_MAX).min(s.left);
        let body = Frame::Body { msg: s.num, data: &p[s.off..s.off + n] };
        if ipcwire::encode(&body, b).is_err() {
            return false;
        }
        s.off += n;
        s.left -= n;
        add(&peer.counts.bytes_out, n as u64);
        if s.left == 0 {
            let done = self.open.swap_remove(i);
            self.promised = self.promised.saturating_sub(done.msg.len);
        } else {
            self.turn = self.turn.wrapping_add(1);
        }
        true
    }
}

/// A small frame for one of the peer's connections -- a Ping or a Pong, a
/// Down, a bounce, a monitor asked -- ahead of every message: whether it
/// was taken. Not while the peer is down, nor past FRAMES_MAX.
pub fn push_frame(peer: &Peer, class: usize, frame: &Frame<'_>) -> bool {
    let mut b = Vec::new();
    if ipcwire::encode(frame, &mut b).is_err() {
        return false;
    }
    let taken = peer.out[class].with(|o| {
        if o.number == 0 || o.frames.len() >= FRAMES_MAX || o.frames.try_reserve(1).is_err() {
            return None;
        }
        o.frames.push_back(b);
        Some(o.waker)
    });
    match taken {
        Some(waker) => {
            /* Its connection's task told, out of the lock */
            if let Some(w) = waker {
                w.wake();
            }
            true
        }
        None => {
            add(&peer.counts.dropped, 1);
            false
        }
    }
}

/// The connection's life once the peer is up: what goes out and what comes
/// in, until it ends -- why it did. While it runs, whoever queues for it
/// wakes it from its wait on the socket.
fn run(shared: &Arc<Shared>, sess: &mut Session, peer: &Arc<Peer>, class: usize, session: u64) -> &'static str {
    let waker = sess.waker();
    peer.out[class].with(|o| o.waker = Some(waker));
    let why = serve_link(shared, sess, peer, class, session);
    peer.out[class].with(|o| o.waker = None);
    why
}

fn serve_link(shared: &Arc<Shared>, sess: &mut Session, peer: &Arc<Peer>, class: usize, session: u64)
              -> &'static str {
    let Some(number) = peer.number(session) else { return "the session ended" };
    let mut sender = Sender { open: Vec::new(), promised: 0, next_num: 1, turn: 0 };
    let mut asm = Assembler::new(OPEN_MAX, OPEN_BYTES_MAX, PIECES_MAX);
    let mut inbuf: Vec<u8> = Vec::new();
    let mut buf = alloc::vec![0u8; READ_BYTES];
    let mut frame = Vec::new();
    let now = kcore::time::boot_time_ns();
    let mut heard = now;
    let mut next_ping = now;
    let mut ping_seq = 0u64;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return "stopping";
        }
        if !peer.current(session) {
            return "the session ended";
        }
        let now = kcore::time::boot_time_ns();
        if now.saturating_sub(heard) > LOST_MS * NS_PER_MS {
            return "nothing heard for 3 s";
        }
        if now >= next_ping {
            ping_seq += 1;
            push_frame(peer, class, &Frame::Ping { seq: ping_seq });
            next_ping = now + PING_MS * NS_PER_MS;
        }
        /* Out: frames into the session while it has room */
        let mut moved = false;
        while sess.pending() < MAX_PENDING && sender.next(peer, class, &mut frame) {
            if !sess.write(&frame, WRITE_ROOM) {
                return failed(&sess);
            }
            moved = true;
        }
        if sess.pending() > 0 {
            match sess.flush(1) {
                Flush::Failed => return failed(&sess),
                Flush::Done | Flush::More => {}
            }
        }
        /* In: what came -- for a millisecond while things go out, and while
         * nothing does until the next Ping is due: the socket's wait ends
         * the moment data comes, or something is queued here (`waker`).
         * And then, while the session holds more of it, the rest, up to a
         * receive buffer's worth: a reader that takes one record a turn,
         * and spends the rest of the turn on its flush, lets its buffer
         * fill behind it and shuts the peer's window -- and with both
         * directions busy the peer's flush waits on that, its own reading
         * with it: the two hosts went in step, a record each a turn, a
         * third slower than one message at a time. */
        let busy = moved || sess.pending() > 0 || !sender.open.is_empty();
        let mut wait = if busy {
            1
        } else {
            (next_ping.saturating_sub(kcore::time::boot_time_ns()) / NS_PER_MS).clamp(1, PING_MS)
        };
        for _ in 0..DRAIN_READS {
            match sess.read(&mut buf, wait) {
                Read::Data(n) => {
                    heard = kcore::time::boot_time_ns();
                    inbuf.extend_from_slice(&buf[..n]);
                    let mut at = 0;
                    loop {
                        match ipcwire::decode(&inbuf[at..]) {
                            Ok(Some((f, used))) => {
                                at += used;
                                if let Err(why) = take(shared, peer, class, number, &mut asm, &f) {
                                    return why;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => {
                                add(&shared.counters.wire_refused, 1);
                                return "a frame the wire refuses";
                            }
                        }
                    }
                    inbuf.drain(..at);
                }
                Read::Idle => break,
                Read::End => return "it closed its connection",
                Read::Failed => return failed(&sess),
            }
            if !sess.buffered() {
                break;
            }
            wait = 1;
        }
    }
}

/// A frame from the peer, done: Err ends the connection, for why.
fn take(shared: &Shared, peer: &Peer, class: usize, number: u16, asm: &mut Assembler, f: &Frame<'_>)
        -> Result<(), &'static str> {
    let router = &shared.router;
    match *f {
        Frame::Ping { seq } => {
            push_frame(peer, class, &Frame::Pong { seq });
        }
        Frame::Pong { .. } => {}
        Frame::Head { .. } | Frame::Body { .. } => {
            if let Frame::Body { data, .. } = f {
                add(&peer.counts.bytes_in, data.len() as u64);
            }
            match asm.take(f) {
                Ok(Some(m)) => deliver(shared, peer, number, m),
                Ok(None) => {}
                Err(_) => {
                    add(&shared.counters.wire_refused, 1);
                    return Err("a message past what the assembler takes");
                }
            }
        }
        Frame::Bounced { from, to, tag, reference, why } => {
            let why = Bounce::from_code(u32::from(why)).unwrap_or(Bounce::Unsent);
            let to = match to {
                To::Addr(a) => RemoteTo::Addr(Addr(a)),
                To::Name(n) => match core::str::from_utf8(n) {
                    Ok(n) => RemoteTo::Name(n),
                    Err(_) => return Ok(()),
                },
            };
            add(&peer.counts.bounced_in, 1);
            let _ = router.bounced(number, Addr(from), to, tag, reference, why);
        }
        Frame::Monitor { id, target } if class == CONTROL => monitor_for(shared, peer, id, target),
        Frame::Demonitor { id } if class == CONTROL => {
            let local = peer.st.with(|st| {
                let i = st.monitors.iter().position(|&(r, _)| r == id)?;
                Some(st.monitors.swap_remove(i).1)
            });
            if let Some(l) = local {
                router.demonitor(l);
            }
        }
        Frame::Down { id, reason } if class == CONTROL => {
            /* A reason nobody knows is the host's word that it lost track */
            let _ = router.remote_down(number, id, Reason::from_code(u32::from(reason)).unwrap_or(Reason::HostLost));
        }
        Frame::Hello { .. } | Frame::Monitor { .. } | Frame::Demonitor { .. } | Frame::Down { .. } => {
            add(&shared.counters.wire_refused, 1);
            return Err("a frame out of its place");
        }
    }
    Ok(())
}

/// A code of the channel's as a frame's byte: every one is under 256.
pub fn byte(code: u32) -> u8 {
    u8::try_from(code).unwrap_or(u8::MAX)
}

/// A message put back together, handed to the channel: delivered, or
/// bounced back -- full, gone -- or, for what the peer may not reach here,
/// dropped and counted.
fn deliver(shared: &Shared, peer: &Peer, number: u16, m: ipcwire::Message) {
    let (to, name) = match &m.to {
        Dest::Addr(a) => (Addr(*a), None),
        Dest::Name(n) => match core::str::from_utf8(n) {
            Ok(n) => (Addr::NONE, Some(n)),
            Err(_) => return,
        },
    };
    /* A host invited reaches the one name it was given, and nothing else
       until it has joined: dropped and counted, as what a peer may not
       reach is */
    if !peer.may_reach(name) {
        add(&peer.counts.refused, 1);
        return;
    }
    let pieces: Vec<&[u8]> = m.pieces.iter().map(|p| &p[..]).collect();
    add(&peer.counts.received, 1);
    let why = match shared.router.deliver_remote(number, m.from, to, name, m.tag, m.reference, &pieces) {
        Ok(()) => return,
        Err(kcore::ipc::Error::Full) => Bounce::Full,
        Err(kcore::ipc::Error::Gone) => Bounce::Gone,
        Err(_) => {
            add(&peer.counts.refused, 1);
            return;
        }
    };
    let to = match &m.to {
        Dest::Addr(a) => To::Addr(*a),
        Dest::Name(n) => To::Name(n),
    };
    add(&peer.counts.bounced_out, 1);
    push_frame(peer, CONTROL, &Frame::Bounced { from: m.from, to, tag: m.tag, reference: m.reference,
                                                why: byte(why.code()) });
}

/// The peer asks this host to watch its mailbox `target`, and say `Down`
/// with `id` when it goes: the router's monitor, held for the peer. One
/// it may not have -- past its share, or the channel's -- is a Down at
/// once, HostLost: this host will not say when that one goes.
fn monitor_for(shared: &Shared, peer: &Peer, id: u64, target: u64) {
    /* The record and the monitor made under the peer's lock, so that the
     * router, which takes the lock to find a Down's peer, never sees the
     * Down before the record */
    /* A host invited watches nothing here */
    let invited = peer.invited();
    let made = !invited && peer.st.with(|st| {
        if st.number == 0 || st.monitors.len() >= MONITORS_MAX || st.monitors.try_reserve(1).is_err() {
            return false;
        }
        match shared.router.monitor(Addr(target)) {
            Ok(local) => {
                st.monitors.push((id, local));
                true
            }
            Err(_) => false,
        }
    });
    if !made {
        add(&peer.counts.refused, 1);
        push_frame(peer, CONTROL, &Frame::Down { id, reason: byte(Reason::HostLost.code()) });
    }
}
