//! An SSH client of the world's, for the machine's sshd: RFC 4253, 4252 and
//! 4254 as OpenSSH speaks them to it -- curve25519-sha256 with the strict key
//! exchange, ssh-ed25519, chacha20-poly1305@openssh.com -- over the world's
//! TCP, doing what its plan says: logging in with the key the server knows,
//! or another, or a signature over the wrong thing; running a command or a
//! shell; keeping the server to a window of its choosing; rekeying; and,
//! when the plan says, breaking the protocol at one point of it.
//!
//! Everything the server sends is checked as it is read: every packet whole,
//! padded as RFC 4253 6 says, its tag right; its key exchange signed by its
//! host key over the hash both ends made; no channel before a login, and no
//! login for a key it was not told of or a signature that is not the key's;
//! no more data than the window the client gave, in no bigger packets than
//! it said it takes.

use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::{ChaCha20Legacy, Key, LegacyNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use poly1305::universal_hash::KeyInit;
use poly1305::Poly1305;
use sha2::{Digest, Sha256};
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};

use super::tcpm::Tcp;
use super::Net;
use crate::machine::sched;

/* Message numbers (RFC 4250 4.1) */
pub const DISCONNECT: u8 = 1;
pub const IGNORE: u8 = 2;
pub const UNIMPLEMENTED: u8 = 3;
pub const DEBUG: u8 = 4;
pub const SERVICE_REQUEST: u8 = 5;
pub const SERVICE_ACCEPT: u8 = 6;
pub const KEXINIT: u8 = 20;
pub const NEWKEYS: u8 = 21;
pub const KEX_ECDH_INIT: u8 = 30;
pub const KEX_ECDH_REPLY: u8 = 31;
pub const USERAUTH_REQUEST: u8 = 50;
pub const USERAUTH_FAILURE: u8 = 51;
pub const USERAUTH_SUCCESS: u8 = 52;
pub const USERAUTH_PK_OK: u8 = 60;
pub const GLOBAL_REQUEST: u8 = 80;
pub const REQUEST_SUCCESS: u8 = 81;
pub const REQUEST_FAILURE: u8 = 82;
pub const CHANNEL_OPEN: u8 = 90;
pub const CHANNEL_OPEN_CONFIRMATION: u8 = 91;
pub const CHANNEL_OPEN_FAILURE: u8 = 92;
pub const CHANNEL_WINDOW_ADJUST: u8 = 93;
pub const CHANNEL_DATA: u8 = 94;
pub const CHANNEL_EXTENDED_DATA: u8 = 95;
pub const CHANNEL_EOF: u8 = 96;
pub const CHANNEL_CLOSE: u8 = 97;
pub const CHANNEL_REQUEST: u8 = 98;
pub const CHANNEL_SUCCESS: u8 = 99;
pub const CHANNEL_FAILURE: u8 = 100;

const TAG_LEN: usize = 16;
const PACKET_MAX: usize = 35000;
const BLOCK: usize = 8;
const ED25519: &[u8] = b"ssh-ed25519";
/// The client's number for its channel.
const OUR_CHANNEL: u32 = 7;
const SERVER_VERSION: &[u8] = b"SSH-2.0-nos_sshd";

/* ---- the wire ---- */

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

/// Reads a payload, None past its end.
pub struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, at: 0 }
    }
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.buf.get(self.at..end)?;
        self.at = end;
        Some(out)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }
    pub fn u32(&mut self) -> Option<u32> {
        let b = self.bytes(4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }
    pub fn done(&self) -> bool {
        self.at == self.buf.len()
    }
}

fn hash_string(h: &mut Sha256, s: &[u8]) {
    h.update((s.len() as u32).to_be_bytes());
    h.update(s);
}

/// K as an mpint (RFC 8731 3.1).
fn hash_mpint(h: &mut Sha256, magnitude: &[u8]) {
    let first = magnitude.iter().position(|&b| b != 0).unwrap_or(magnitude.len());
    let digits = &magnitude[first..];
    let pad = digits.first().is_some_and(|&b| b & 0x80 != 0);
    h.update(((digits.len() + pad as usize) as u32).to_be_bytes());
    if pad {
        h.update([0u8]);
    }
    h.update(digits);
}

/// A direction's 64 bytes of key (RFC 4253 7.2).
fn derive(shared: &[u8; 32], hash: &[u8; 32], letter: u8, session_id: &[u8; 32]) -> [u8; 64] {
    let mut h = Sha256::new();
    hash_mpint(&mut h, shared);
    h.update(hash);
    h.update([letter]);
    h.update(session_id);
    let first: [u8; 32] = h.finalize().into();
    let mut h = Sha256::new();
    hash_mpint(&mut h, shared);
    h.update(hash);
    h.update(first);
    let second: [u8; 32] = h.finalize().into();
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&first);
    out[32..].copy_from_slice(&second);
    out
}

/// chacha20-poly1305@openssh.com, from the client's side.
struct Cipher {
    main: [u8; 32],
    header: [u8; 32],
}

impl Cipher {
    fn new(key: &[u8; 64]) -> Cipher {
        let mut main = [0u8; 32];
        let mut header = [0u8; 32];
        main.copy_from_slice(&key[..32]);
        header.copy_from_slice(&key[32..]);
        Cipher { main, header }
    }

    fn stream(key: &[u8; 32], seq: u32) -> ChaCha20Legacy {
        ChaCha20Legacy::new(Key::from_slice(key), LegacyNonce::from_slice(&u64::from(seq).to_be_bytes()))
    }

    fn payload_stream(&self, seq: u32) -> (Poly1305, ChaCha20Legacy) {
        let mut s = Self::stream(&self.main, seq);
        let mut poly_key = [0u8; 32];
        s.apply_keystream(&mut poly_key);
        s.seek(64u64);
        (Poly1305::new(poly1305::Key::from_slice(&poly_key)), s)
    }

    fn length(&self, seq: u32, wire: &[u8]) -> u32 {
        let mut len = [wire[0], wire[1], wire[2], wire[3]];
        Self::stream(&self.header, seq).apply_keystream(&mut len);
        u32::from_be_bytes(len)
    }

    fn open(&self, seq: u32, packet: &mut [u8], tag: &[u8]) -> bool {
        let (mac, mut s) = self.payload_stream(seq);
        if mac.compute_unpadded(packet).as_slice() != tag {
            return false;
        }
        s.apply_keystream(&mut packet[4..]);
        true
    }

    fn seal(&self, seq: u32, packet: &mut Vec<u8>) {
        Self::stream(&self.header, seq).apply_keystream(&mut packet[..4]);
        let (mac, mut s) = self.payload_stream(seq);
        s.apply_keystream(&mut packet[4..]);
        let tag = mac.compute_unpadded(packet);
        packet.extend_from_slice(tag.as_slice());
    }
}

/* ---- the plan ---- */

/// A key the client may log in with: the one the server knows, or not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Who {
    Known,
    Stranger,
}

/// One authentication request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Try {
    /// Method "none": how a client learns the methods.
    None,
    /// Asks whether a key would do, signing nothing.
    Query(Who),
    /// Logs in with the key, signing what RFC 4252 7 says.
    Sign(Who),
    /// Signs the right request with the wrong session id.
    BadSignature,
}

/// Where the client breaks the protocol, if it does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Break {
    None,
    /// A version line of garbage, no SSH in it.
    Version,
    /// A KEXINIT with no cipher the server has.
    NoCipher,
    /// An all-zero X25519 public key: a point of small order.
    SmallPoint,
    /// A packet whose length is past what any may be.
    Length,
    /// An IGNORE ahead of the first KEXINIT, the strict exchange asked for.
    IgnoreFirst,
    /// A byte flipped in the first encrypted packet: its tag fails.
    Tamper,
    /// A channel opened before the login.
    ChannelEarly,
    /// Data past the window the server gave.
    Overrun,
    /// Data for a channel that is not open.
    NoChannel,
    /// Nothing at all after the version: a client that went quiet.
    Silent,
}

/// What the client runs once logged in.
#[derive(Clone, Debug)]
pub enum Run {
    /// A command, and what the model knows it prints.
    Exec(String, Option<Vec<u8>>),
    /// A shell with no terminal: lines in, their output out, nothing else.
    Shell(Vec<(String, Option<Vec<u8>>)>),
    /// A shell with a terminal: typed as a person types.
    Pty(Vec<String>),
}

pub struct Plan {
    pub tries: Vec<Try>,
    pub brk: Break,
    pub run: Run,
    /// The window the client gives, and the most it takes in a packet.
    pub window: u32,
    pub max_packet: u32,
    /// How much the client lets pile up before it gives window back: never
    /// more than the window.
    pub adjust_after: u32,
    /// Rekeys after this many channel data packets, if at all.
    pub rekey_after: Option<usize>,
    /// Answers the server's keepalives.
    pub answers_keepalive: bool,
    /// Messages of no protocol, sent after the login.
    pub unknown: bool,
    pub global: bool,
}

/* ---- the client ---- */

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Waiting for the server's version line.
    Version,
    Kex,
    Auth,
    Channel,
    Running,
    /// The channel closed both ways, or the server disconnected: done.
    Over,
}

/// Why the connection ended, as far as the client saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The server said DISCONNECT, with this reason code.
    Disconnect(u32),
    /// The channel closed both ways.
    Closed,
    /// The TCP connection ended under it.
    Gone,
}

pub struct Client {
    pub ep: usize,
    pub plan: Plan,
    rng: u64,
    fed: usize,
    buf: Vec<u8>,
    pub phase: Phase,
    version_sent: bool,
    v_c: Vec<u8>,
    v_s: Vec<u8>,
    i_c: Vec<u8>,
    i_s: Vec<u8>,
    secret: [u8; 32],
    session_id: Option<[u8; 32]>,
    pub strict: bool,
    first_kex: bool,
    send_seq: u32,
    recv_seq: u32,
    send_key: Option<Cipher>,
    recv_key: Option<Cipher>,
    /* Keys made, waiting for the NEWKEYS they follow */
    next_recv: Option<Cipher>,
    rekeying: bool,
    pub host_key: Option<[u8; 32]>,
    try_at: usize,
    pub logged_in: bool,
    /// The login the server granted: with which try.
    pub granted_by: Option<Try>,
    /* The channel */
    server_channel: Option<u32>,
    server_window: u64,
    server_max: u32,
    our_window: u32,
    unadjusted: u32,
    pub output: Vec<u8>,
    pub exit_status: Option<u32>,
    pub eof_in: bool,
    pub close_in: bool,
    close_out: bool,
    lines_sent: usize,
    data_packets: usize,
    pub ending: Option<Ending>,
    /// The messages the server sent, by number, in order.
    pub heard: Vec<u8>,
    pub unimplemented: usize,
    pub keepalives: usize,
    tampered: bool,
    /// The client closed the channel first, cutting the session short.
    pub cut: bool,
}

impl Client {
    pub fn new(ep: usize, plan: Plan, seed: u64) -> Client {
        Client { ep, plan, rng: seed | 1, fed: 0, buf: Vec::new(), phase: Phase::Version, version_sent: false,
                 v_c: b"SSH-2.0-fuzz_client".to_vec(), v_s: Vec::new(), i_c: Vec::new(), i_s: Vec::new(),
                 secret: [0; 32], session_id: None, strict: false, first_kex: true, send_seq: 0, recv_seq: 0,
                 send_key: None, recv_key: None, next_recv: None, rekeying: false, host_key: None, try_at: 0,
                 logged_in: false, granted_by: None, server_channel: None, server_window: 0, server_max: 0,
                 our_window: 0, unadjusted: 0, output: Vec::new(), exit_status: None, eof_in: false,
                 close_in: false, close_out: false, lines_sent: 0, data_packets: 0, ending: None, heard: Vec::new(),
                 unimplemented: 0, keepalives: 0, tampered: false, cut: false }
    }

    fn random(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            *b = self.rng as u8;
        }
    }

    /// The key a try logs in with.
    pub fn key(who: Who) -> SigningKey {
        SigningKey::from_bytes(match who {
            Who::Known => &[0x11; 32],
            Who::Stranger => &[0x22; 32],
        })
    }

    pub fn blob(key: &SigningKey) -> Vec<u8> {
        let mut b = Vec::new();
        put_string(&mut b, ED25519);
        put_string(&mut b, &key.verifying_key().to_bytes());
        b
    }

    /// Bytes to the server, as they are.
    fn raw(tcp: &mut Tcp, net: &mut Net, ep: usize, bytes: &[u8]) {
        tcp.eps[ep].write_bytes(bytes);
        tcp.output(net, ep);
    }

    /// One packet to the server: padded, and sealed once there are keys.
    fn send(&mut self, tcp: &mut Tcp, net: &mut Net, payload: &[u8]) {
        let unpadded = if self.send_key.is_some() { 1 } else { 5 } + payload.len();
        let mut padding = BLOCK - unpadded % BLOCK;
        if padding < 4 {
            padding += BLOCK;
        }
        let len = 1 + payload.len() + padding;
        let mut p = Vec::with_capacity(4 + len + TAG_LEN);
        p.extend_from_slice(&(len as u32).to_be_bytes());
        p.push(padding as u8);
        p.extend_from_slice(payload);
        let mut pad = vec![0u8; padding];
        self.random(&mut pad);
        p.extend_from_slice(&pad);
        if let Some(key) = &self.send_key {
            key.seal(self.send_seq, &mut p);
            if self.plan.brk == Break::Tamper && !self.tampered {
                self.tampered = true;
                let at = 4 + (self.rng as usize % (p.len() - 4));
                p[at] ^= 0x10;
            }
        }
        self.send_seq = self.send_seq.wrapping_add(1);
        Self::raw(tcp, net, self.ep, &p);
    }

    fn kexinit(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let mut p = vec![KEXINIT];
        let mut cookie = [0u8; 16];
        self.random(&mut cookie);
        p.extend_from_slice(&cookie);
        let kex: &[u8] = if self.first_kex { b"curve25519-sha256,kex-strict-c-v00@openssh.com" }
                         else { b"curve25519-sha256" };
        put_string(&mut p, kex);
        put_string(&mut p, ED25519);
        let cipher: &[u8] = if self.plan.brk == Break::NoCipher { b"aes128-ctr" }
                            else { b"chacha20-poly1305@openssh.com" };
        put_string(&mut p, cipher);
        put_string(&mut p, cipher);
        put_string(&mut p, b"hmac-sha2-256");
        put_string(&mut p, b"hmac-sha2-256");
        put_string(&mut p, b"none");
        put_string(&mut p, b"none");
        put_string(&mut p, b"");
        put_string(&mut p, b"");
        p.push(0);
        put_u32(&mut p, 0);
        self.i_c = p.clone();
        self.send(tcp, net, &p);
    }

    /// The first thing: the version line, and the KEXINIT right behind it.
    pub fn start(&mut self, tcp: &mut Tcp, net: &mut Net) {
        if self.version_sent {
            return;
        }
        self.version_sent = true;
        if self.plan.brk == Break::Version {
            Self::raw(tcp, net, self.ep, b"GET / HTTP/1.1\r\nHost: sshd\r\n\r\n");
            return;
        }
        let mut v = self.v_c.clone();
        v.extend_from_slice(b"\r\n");
        Self::raw(tcp, net, self.ep, &v);
        if self.plan.brk == Break::Silent {
            return;
        }
        if self.plan.brk == Break::IgnoreFirst {
            self.send(tcp, net, &[IGNORE, 0, 0, 0, 0]);
        }
        if self.plan.brk == Break::Length {
            Self::raw(tcp, net, self.ep, &[0x7F, 0xFF, 0xFF, 0xFF, 4, 0, 0, 0, 0]);
            return;
        }
        self.kexinit(tcp, net);
    }

    /// What the server sent since the last call, taken in and answered.
    pub fn pump(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let Some(data) = tcp.eps[self.ep].data.as_ref() else { return };
        if data.rx.len() > self.fed {
            let new = data.rx[self.fed..].to_vec();
            self.fed = data.rx.len();
            self.buf.extend_from_slice(&new);
        }
        let unread = tcp.eps[self.ep].unread;
        if unread != 0 {
            tcp.read(net, self.ep, unread);
        }
        if self.phase == Phase::Over {
            return;
        }
        /* A client that broke the protocol before its key exchange says no
         * more: it only listens for how the server takes it. */
        let mute = matches!(self.plan.brk, Break::Version | Break::Length | Break::Silent);
        if self.v_s.is_empty() {
            let Some(nl) = self.buf.iter().position(|&b| b == b'\n') else {
                invariant!(self.buf.len() < 256, "a server version line of {} bytes and no end", self.buf.len());
                return;
            };
            let mut line = self.buf[..nl].to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.buf.drain(..=nl);
            invariant!(line == SERVER_VERSION, "the server's version line {:?}", String::from_utf8_lossy(&line));
            self.v_s = line;
            self.phase = Phase::Kex;
        }
        while self.phase != Phase::Over {
            let Some(payload) = self.packet() else { break };
            invariant!(!payload.is_empty(), "a server packet with no message in it");
            self.heard.push(payload[0]);
            if mute && payload[0] != DISCONNECT {
                continue;
            }
            self.on_message(tcp, net, &payload);
        }
    }

    /// The next whole packet from the server, checked and opened.
    fn packet(&mut self) -> Option<Vec<u8>> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = match &self.recv_key {
            None => u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]),
            Some(key) => key.length(self.recv_seq, &self.buf[..4]),
        } as usize;
        invariant!((6..=PACKET_MAX).contains(&len), "a server packet of length {}", len);
        let total = 4 + len + if self.recv_key.is_some() { TAG_LEN } else { 0 };
        if self.buf.len() < total {
            return None;
        }
        let mut packet: Vec<u8> = self.buf.drain(..total).collect();
        if let Some(key) = &self.recv_key {
            let (body, tag) = packet.split_at_mut(4 + len);
            let tag = tag.to_vec();
            invariant!(key.open(self.recv_seq, body, &tag), "a server packet (number {}) whose tag is wrong",
                       self.recv_seq);
        }
        let padding = packet[4] as usize;
        let aligned = if self.recv_key.is_some() { len } else { 4 + len };
        invariant!(padding >= 4 && padding + 1 < len && aligned % BLOCK == 0,
                   "a server packet of length {} padded {}", len, padding);
        self.recv_seq = self.recv_seq.wrapping_add(1);
        Some(packet[5..4 + len - padding].to_vec())
    }

    fn on_message(&mut self, tcp: &mut Tcp, net: &mut Net, p: &[u8]) {
        let mut r = Reader::new(&p[1..]);
        match p[0] {
            DISCONNECT => {
                let reason = r.u32().unwrap_or(0);
                self.ending = Some(Ending::Disconnect(reason));
                self.phase = Phase::Over;
            }
            IGNORE | DEBUG => {}
            UNIMPLEMENTED => self.unimplemented += 1,
            KEXINIT => self.on_kexinit(tcp, net, p),
            KEX_ECDH_REPLY => self.on_ecdh_reply(tcp, net, &mut r),
            NEWKEYS => {
                invariant!(self.next_recv.is_some(), "a NEWKEYS before the key exchange's reply");
                self.recv_key = self.next_recv.take();
                /* The strict exchange resets the numbers at every NEWKEYS,
                 * for the life of the connection (OpenSSH's PROTOCOL 1.10). */
                if self.strict {
                    self.recv_seq = 0;
                }
                self.first_kex = false;
                if self.rekeying {
                    self.rekeying = false;
                    self.give_window(tcp, net);
                    self.close_back(tcp, net);
                    if self.phase == Phase::Over {
                        return;
                    }
                    self.proceed(tcp, net);
                } else {
                    self.phase = Phase::Auth;
                    let mut m = vec![SERVICE_REQUEST];
                    put_string(&mut m, b"ssh-userauth");
                    if self.plan.brk == Break::ChannelEarly {
                        self.open_channel(tcp, net);
                        return;
                    }
                    self.send(tcp, net, &m);
                }
            }
            SERVICE_ACCEPT => {
                invariant!(self.phase == Phase::Auth && r.string() == Some(b"ssh-userauth".as_slice()),
                           "a SERVICE_ACCEPT out of place");
                self.next_try(tcp, net);
            }
            USERAUTH_FAILURE => {
                invariant!(!self.logged_in, "a USERAUTH_FAILURE after the login");
                invariant!(r.string() == Some(b"publickey".as_slice()), "a USERAUTH_FAILURE offering other than \
                           publickey");
                self.try_at += 1;
                self.next_try(tcp, net);
            }
            USERAUTH_PK_OK => {
                let asked = self.plan.tries.get(self.try_at).copied();
                invariant!(asked == Some(Try::Query(Who::Known)), "a USERAUTH_PK_OK for {:?}", asked);
                self.try_at += 1;
                self.next_try(tcp, net);
            }
            USERAUTH_SUCCESS => {
                let asked = self.plan.tries.get(self.try_at).copied();
                invariant!(asked == Some(Try::Sign(Who::Known)), "logged in with {:?}", asked);
                self.logged_in = true;
                self.granted_by = asked;
                self.phase = Phase::Channel;
                self.open_channel(tcp, net);
            }
            GLOBAL_REQUEST => {
                let name = r.string().unwrap_or_default().to_vec();
                let want = r.u8().unwrap_or(0) != 0;
                invariant!(self.logged_in, "a global request {:?} before the login", String::from_utf8_lossy(&name));
                self.keepalives += 1;
                if want && self.plan.answers_keepalive {
                    self.send(tcp, net, &[REQUEST_FAILURE]);
                }
            }
            REQUEST_SUCCESS | REQUEST_FAILURE => {}
            CHANNEL_OPEN_CONFIRMATION => {
                invariant!(self.logged_in, "a channel opened before the login");
                let (us, them, window, max) = (r.u32(), r.u32(), r.u32(), r.u32());
                invariant!(us == Some(OUR_CHANNEL) && them.is_some() && window.is_some() && max.is_some(),
                           "a CHANNEL_OPEN_CONFIRMATION of {:?}", p);
                self.server_channel = them;
                self.server_window = u64::from(window.unwrap_or(0));
                self.server_max = max.unwrap_or(0);
                self.phase = Phase::Running;
                self.begin(tcp, net);
            }
            CHANNEL_OPEN_FAILURE => {
                invariant!(false, "the channel refused: {:?}", p);
            }
            CHANNEL_SUCCESS | CHANNEL_FAILURE => {
                invariant!(r.u32() == Some(OUR_CHANNEL), "a channel answer for another channel");
            }
            CHANNEL_WINDOW_ADJUST => {
                invariant!(r.u32() == Some(OUR_CHANNEL), "a window adjust for another channel");
            }
            CHANNEL_DATA => self.on_data(tcp, net, &mut r),
            CHANNEL_EXTENDED_DATA => invariant!(false, "stderr from the server"),
            CHANNEL_EOF => {
                invariant!(r.u32() == Some(OUR_CHANNEL) && !self.eof_in, "an EOF out of place");
                self.eof_in = true;
            }
            CHANNEL_CLOSE => {
                invariant!(r.u32() == Some(OUR_CHANNEL) && !self.close_in, "a CLOSE out of place");
                self.close_in = true;
                self.close_back(tcp, net);
            }
            CHANNEL_REQUEST => {
                invariant!(r.u32() == Some(OUR_CHANNEL), "a channel request for another channel");
                let kind = r.string().unwrap_or_default().to_vec();
                let want = r.u8().unwrap_or(1);
                invariant!(kind == b"exit-status" && want == 0 && self.exit_status.is_none(),
                           "a channel request {:?}", String::from_utf8_lossy(&kind));
                self.exit_status = r.u32();
            }
            n => invariant!(false, "a server message of number {}", n),
        }
    }

    fn on_kexinit(&mut self, tcp: &mut Tcp, net: &mut Net, p: &[u8]) {
        let mut r = Reader::new(&p[1..]);
        let parsed = (|| {
            r.bytes(16)?;
            let kex = r.string()?;
            let host = r.string()?;
            let c1 = r.string()?;
            let c2 = r.string()?;
            Some((kex.to_vec(), host.to_vec(), c1.to_vec(), c2.to_vec()))
        })();
        let Some((kex, host, c1, c2)) = parsed else {
            invariant!(false, "a malformed KEXINIT from the server");
            return;
        };
        let has = |list: &[u8], name: &[u8]| list.split(|&b| b == b',').any(|n| n == name);
        invariant!(has(&kex, b"curve25519-sha256") && has(&host, ED25519)
                   && has(&c1, b"chacha20-poly1305@openssh.com") && has(&c2, b"chacha20-poly1305@openssh.com"),
                   "a server KEXINIT without what it serves");
        invariant!(has(&kex, b"kex-strict-s-v00@openssh.com") == self.first_kex,
                   "the strict key exchange offered in a KEXINIT that is {}the first", if self.first_kex { "" }
                   else { "not " });
        self.i_s = p.to_vec();
        if !self.first_kex && !self.rekeying {
            invariant!(false, "a KEXINIT from the server, the client having asked for no rekey");
        }
        if self.first_kex {
            self.strict = has(&kex, b"kex-strict-s-v00@openssh.com");
        }
        /* The ephemeral key: or a point of small order. */
        let mut public = [0u8; 32];
        if self.plan.brk == Break::SmallPoint {
            self.secret = [0; 32];
        } else {
            let mut s = [0u8; 32];
            self.random(&mut s);
            self.secret = s;
            public = x25519(self.secret, X25519_BASEPOINT_BYTES);
        }
        let mut m = vec![KEX_ECDH_INIT];
        put_string(&mut m, &public);
        self.send(tcp, net, &m);
    }

    fn on_ecdh_reply(&mut self, tcp: &mut Tcp, net: &mut Net, r: &mut Reader) {
        invariant!(self.plan.brk != Break::SmallPoint, "a key exchange answered to a point of small order");
        let (Some(k_s), Some(q_s), Some(sig)) = (r.string(), r.string(), r.string()) else {
            invariant!(false, "a malformed KEX_ECDH_REPLY");
            return;
        };
        let (k_s, q_s, sig) = (k_s.to_vec(), q_s.to_vec(), sig.to_vec());
        let mut kr = Reader::new(&k_s);
        let (Some(alg), Some(key)) = (kr.string(), kr.string()) else {
            invariant!(false, "a host key blob of {:02x?}", k_s);
            return;
        };
        invariant!(alg == ED25519 && key.len() == 32 && kr.done() && q_s.len() == 32, "a host key or ECDH key of \
                   the wrong kind");
        let mut host = [0u8; 32];
        host.copy_from_slice(key);
        if let Some(first) = self.host_key {
            invariant!(first == host, "a rekey signed by another host key");
        }
        self.host_key = Some(host);
        let mut q = [0u8; 32];
        q.copy_from_slice(&q_s);
        let shared = x25519(self.secret, q);
        let our_public = x25519(self.secret, X25519_BASEPOINT_BYTES);

        let mut h = Sha256::new();
        hash_string(&mut h, &self.v_c);
        hash_string(&mut h, &self.v_s);
        hash_string(&mut h, &self.i_c);
        hash_string(&mut h, &self.i_s);
        hash_string(&mut h, &k_s);
        hash_string(&mut h, &our_public);
        hash_string(&mut h, &q_s);
        hash_mpint(&mut h, &shared);
        let hash: [u8; 32] = h.finalize().into();

        let mut sr = Reader::new(&sig);
        let (Some(salg), Some(sbytes)) = (sr.string(), sr.string()) else {
            invariant!(false, "a signature blob of {:02x?}", sig);
            return;
        };
        let verifies = salg == ED25519 && sbytes.len() == 64 && sr.done()
            && VerifyingKey::from_bytes(&host).ok().is_some_and(|k| {
                let mut b = [0u8; 64];
                b.copy_from_slice(sbytes);
                k.verify_strict(&hash, &Signature::from_bytes(&b)).is_ok()
            });
        invariant!(verifies, "the server's key exchange is not signed by its host key over the exchange hash");

        let session_id = *self.session_id.get_or_insert(hash);
        let to_server = derive(&shared, &hash, b'C', &session_id);
        let to_client = derive(&shared, &hash, b'D', &session_id);
        self.send(tcp, net, &[NEWKEYS]);
        self.send_key = Some(Cipher::new(&to_server));
        if self.strict {
            self.send_seq = 0;
        }
        self.next_recv = Some(Cipher::new(&to_client));
    }

    /// The next authentication try, or -- none left -- goodbye.
    fn next_try(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let Some(t) = self.plan.tries.get(self.try_at).copied() else {
            self.disconnect(tcp, net);
            return;
        };
        let user = b"root";
        let mut m = vec![USERAUTH_REQUEST];
        put_string(&mut m, user);
        put_string(&mut m, b"ssh-connection");
        match t {
            Try::None => put_string(&mut m, b"none"),
            Try::Query(who) => {
                put_string(&mut m, b"publickey");
                m.push(0);
                put_string(&mut m, ED25519);
                put_string(&mut m, &Self::blob(&Self::key(who)));
            }
            Try::Sign(_) | Try::BadSignature => {
                let key = Self::key(if let Try::Sign(who) = t { who } else { Who::Known });
                let blob = Self::blob(&key);
                put_string(&mut m, b"publickey");
                m.push(1);
                put_string(&mut m, ED25519);
                put_string(&mut m, &blob);
                let mut id = self.session_id.unwrap_or([0; 32]);
                if t == Try::BadSignature {
                    id[0] ^= 1;
                }
                let mut signed = Vec::new();
                put_string(&mut signed, &id);
                signed.push(USERAUTH_REQUEST);
                put_string(&mut signed, user);
                put_string(&mut signed, b"ssh-connection");
                put_string(&mut signed, b"publickey");
                signed.push(1);
                put_string(&mut signed, ED25519);
                put_string(&mut signed, &blob);
                let sig = key.sign(&signed).to_bytes();
                let mut sb = Vec::new();
                put_string(&mut sb, ED25519);
                put_string(&mut sb, &sig);
                put_string(&mut m, &sb);
            }
        }
        self.send(tcp, net, &m);
    }

    fn open_channel(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let mut m = vec![CHANNEL_OPEN];
        put_string(&mut m, b"session");
        put_u32(&mut m, OUR_CHANNEL);
        put_u32(&mut m, self.plan.window);
        put_u32(&mut m, self.plan.max_packet);
        self.our_window = self.plan.window;
        self.send(tcp, net, &m);
    }

    fn channel_message(&mut self, tcp: &mut Tcp, net: &mut Net, msg: u8) {
        let mut m = vec![msg];
        put_u32(&mut m, self.server_channel.unwrap_or(0));
        self.send(tcp, net, &m);
    }

    fn request(&mut self, tcp: &mut Tcp, net: &mut Net, kind: &[u8], extra: &[u8]) {
        let mut m = vec![CHANNEL_REQUEST];
        put_u32(&mut m, self.server_channel.unwrap_or(0));
        put_string(&mut m, kind);
        m.push(1);
        m.extend_from_slice(extra);
        self.send(tcp, net, &m);
    }

    fn data(&mut self, tcp: &mut Tcp, net: &mut Net, bytes: &[u8]) {
        let mut m = vec![CHANNEL_DATA];
        put_u32(&mut m, if self.plan.brk == Break::NoChannel { 99 } else { self.server_channel.unwrap_or(0) });
        put_string(&mut m, bytes);
        self.send(tcp, net, &m);
    }

    /// The session begins: a terminal if the plan has one, then the command
    /// or the shell -- and the mischief of after a login.
    fn begin(&mut self, tcp: &mut Tcp, net: &mut Net) {
        if self.plan.unknown {
            self.send(tcp, net, &[200, 1, 2, 3]);
        }
        if self.plan.global {
            let mut m = vec![GLOBAL_REQUEST];
            put_string(&mut m, b"tcpip-forward");
            m.push(1);
            put_string(&mut m, b"0.0.0.0");
            put_u32(&mut m, 8080);
            self.send(tcp, net, &m);
        }
        match self.plan.run.clone() {
            Run::Exec(cmd, _) => {
                let mut e = Vec::new();
                put_string(&mut e, cmd.as_bytes());
                self.request(tcp, net, b"exec", &e);
            }
            Run::Shell(_) => self.request(tcp, net, b"shell", &[]),
            Run::Pty(_) => {
                let mut t = Vec::new();
                put_string(&mut t, b"xterm");
                for v in [80u32, 24, 0, 0] {
                    put_u32(&mut t, v);
                }
                put_string(&mut t, b"");
                self.request(tcp, net, b"pty-req", &t);
                self.request(tcp, net, b"shell", &[]);
            }
        }
        if self.plan.brk == Break::Overrun {
            let n = self.server_window as usize + 1;
            self.data(tcp, net, &vec![b'x'; n.min(PACKET_MAX - 64)]);
            let rest = (n as u64).saturating_sub((PACKET_MAX - 64) as u64);
            if rest > 0 {
                self.data(tcp, net, &vec![b'y'; rest as usize]);
            }
        }
        self.proceed(tcp, net);
    }

    /// Types the next line, and after the last closes the input -- as far as
    /// the plan goes.
    fn proceed(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let lines: Vec<String> = match &self.plan.run {
            Run::Exec(..) => Vec::new(),
            Run::Shell(l) => l.iter().map(|x| x.0.clone()).collect(),
            Run::Pty(l) => l.clone(),
        };
        while self.lines_sent < lines.len() {
            let mut text = lines[self.lines_sent].clone().into_bytes();
            text.push(if matches!(self.plan.run, Run::Pty(_)) { b'\r' } else { b'\n' });
            self.lines_sent += 1;
            self.data(tcp, net, &text);
        }
        if !matches!(self.plan.run, Run::Exec(..)) && self.lines_sent == lines.len() {
            self.lines_sent += 1;
            let bye: &[u8] = if matches!(self.plan.run, Run::Pty(_)) { b"exit\r" } else { b"exit\n" };
            self.data(tcp, net, bye);
        }
    }

    fn on_data(&mut self, tcp: &mut Tcp, net: &mut Net, r: &mut Reader) {
        invariant!(self.logged_in, "channel data before the login");
        let (Some(ch), Some(data)) = (r.u32(), r.string()) else {
            invariant!(false, "a malformed CHANNEL_DATA");
            return;
        };
        invariant!(ch == OUR_CHANNEL, "data for channel {}", ch);
        invariant!(data.len() as u32 <= self.plan.max_packet, "{} bytes of data in a packet, the client taking {}",
                   data.len(), self.plan.max_packet);
        invariant!(data.len() as u32 <= self.our_window, "{} bytes of data into a window of {}", data.len(),
                   self.our_window);
        invariant!(!self.eof_in && !self.close_in, "data after the server's EOF or CLOSE");
        self.our_window -= data.len() as u32;
        self.output.extend_from_slice(data);
        self.unadjusted += data.len() as u32;
        self.data_packets += 1;
        self.give_window(tcp, net);
        if self.plan.rekey_after == Some(self.data_packets) && !self.rekeying {
            self.rekeying = true;
            self.kexinit(tcp, net);
        }
    }

    /// Window back to the server, as the plan has it -- but not while a rekey
    /// the client started is under way, when nothing but the exchange may go
    /// (RFC 4253 7.1).
    fn give_window(&mut self, tcp: &mut Tcp, net: &mut Net) {
        if self.rekeying || self.unadjusted == 0 || (self.unadjusted < self.plan.adjust_after && self.our_window != 0) {
            return;
        }
        let give = self.unadjusted;
        self.unadjusted = 0;
        self.our_window += give;
        let mut m = vec![CHANNEL_WINDOW_ADJUST];
        put_u32(&mut m, self.server_channel.unwrap_or(0));
        put_u32(&mut m, give);
        self.send(tcp, net, &m);
    }

    /// The server closed the channel: closed back -- once the rekey the client
    /// started is done, if one is under way -- and the session is over.
    fn close_back(&mut self, tcp: &mut Tcp, net: &mut Net) {
        if !self.close_in || self.rekeying {
            return;
        }
        if !self.close_out {
            self.close_out = true;
            self.channel_message(tcp, net, CHANNEL_CLOSE);
        }
        self.ending = Some(Ending::Closed);
        self.phase = Phase::Over;
    }

    /// Leaves as a client does: DISCONNECT, then the connection closed.
    pub fn disconnect(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let mut m = vec![DISCONNECT];
        put_u32(&mut m, 11);
        put_string(&mut m, b"bye");
        put_string(&mut m, b"");
        self.send(tcp, net, &m);
        self.phase = Phase::Over;
        tcp.close(net, self.ep);
    }

    /// Whether the connection has ended, one way or another.
    pub fn over(&self) -> bool {
        self.phase == Phase::Over
    }

    /// The plan's time is up: a session still going is closed, a login
    /// still going given up.
    pub fn finish(&mut self, tcp: &mut Tcp, net: &mut Net) {
        if self.phase == Phase::Over {
            return;
        }
        if self.phase == Phase::Running && !self.close_out && !self.rekeying {
            self.close_out = true;
            self.cut = true;
            self.channel_message(tcp, net, CHANNEL_CLOSE);
            return;
        }
        if self.phase == Phase::Running {
            return;
        }
        self.disconnect(tcp, net);
    }

    pub fn session_id(&self) -> Option<[u8; 32]> {
        self.session_id
    }

    #[allow(dead_code)]
    pub fn now() -> u64 {
        sched::now()
    }
}
