//! The transport layer (RFC 4253): the version exchange, the binary packet
//! protocol, and key exchange -- the first, and any the client starts later.

use alloc::vec;
use alloc::vec::Vec;
use sha2::{Digest, Sha256};
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};
use zeroize::Zeroize;

use crate::cipher::{ChaChaPoly, Rng, KEY_LEN, TAG_LEN};
use crate::keys::{HostKey, ED25519};
use crate::wire::{self, put_name_list, put_string, put_u32, Reader};
use crate::{Error, Link, Recv, Result};

/* Message numbers (RFC 4250 4.1) */
pub const MSG_DISCONNECT: u8 = 1;
pub const MSG_IGNORE: u8 = 2;
pub const MSG_UNIMPLEMENTED: u8 = 3;
pub const MSG_DEBUG: u8 = 4;
pub const MSG_SERVICE_REQUEST: u8 = 5;
pub const MSG_SERVICE_ACCEPT: u8 = 6;
pub const MSG_KEXINIT: u8 = 20;
pub const MSG_NEWKEYS: u8 = 21;
pub const MSG_KEX_ECDH_INIT: u8 = 30;
pub const MSG_KEX_ECDH_REPLY: u8 = 31;

/* Why the server disconnects (RFC 4250 4.2.2) */
pub const DISCONNECT_PROTOCOL_ERROR: u32 = 2;
pub const DISCONNECT_KEY_EXCHANGE_FAILED: u32 = 3;
pub const DISCONNECT_MAC_ERROR: u32 = 5;
pub const DISCONNECT_BY_APPLICATION: u32 = 11;
pub const DISCONNECT_NO_MORE_AUTH_METHODS: u32 = 14;

/* What the server offers: one of each kind */
const KEX: &[&str] = &["curve25519-sha256", "curve25519-sha256@libssh.org"];
/* OpenSSH's strict key exchange, the answer to Terrapin: offered and asked
   for in the first KEXINIT only (OpenSSH's PROTOCOL, 1.9) */
const KEX_STRICT_SERVER: &str = "kex-strict-s-v00@openssh.com";
const KEX_STRICT_CLIENT: &str = "kex-strict-c-v00@openssh.com";
const HOST_KEYS: &[&str] = &[ED25519];
const CIPHERS: &[&str] = &["chacha20-poly1305@openssh.com"];
/* Named because a KEXINIT has to name a MAC. With an AEAD for the cipher
   nobody uses it, and OpenSSH does not even negotiate it. */
const MACS: &[&str] = &["hmac-sha2-256"];
const COMPRESSION: &[&str] = &["none"];

/// The longest packet_length taken (RFC 4253 6.1 asks for 35000 at least).
const PACKET_MAX: usize = 35000;
/// Packets are padded to a multiple of this, by 4 bytes at the least.
const BLOCK: usize = 8;
const PADDING_MIN: usize = 4;
/// padding_length, the padding and at least a message number.
const PACKET_MIN: usize = 1 + PADDING_MIN + 1;
/// A version line is 255 bytes at most, CR LF included (RFC 4253 4.2).
const VERSION_MAX: usize = 255;
/// Lines of anything before the version line: RFC 4253 4.2 allows them
/// from servers; from a client they are tolerated this far.
const PRE_VERSION_LINES: usize = 16;
/// Room for a whole packet as it comes, and more of the stream behind it.
const INPUT_SIZE: usize = 4 + PACKET_MAX + TAG_LEN + 4096;
/// The longest wait on the connection at a time: how soon a server that
/// is stopping is noticed.
const POLL_MS: u64 = 250;
/// A key exchange the client starts mid-session gets this long to finish.
const REKEY_MS: u64 = 30_000;

const BAD_KEXINIT: Error = Error::Protocol("a malformed KEXINIT");
const OUT_OF_ORDER: Error = Error::Protocol("an unexpected message in the key exchange");

pub struct Transport<'a> {
    link: &'a mut dyn Link,
    host: &'a HostKey,
    rng: Rng,

    /* What the client sent, from start to end, not yet taken as packets */
    input: Vec<u8>,
    start: usize,
    end: usize,
    recv_seq: u32,
    recv_key: Option<ChaChaPoly>,
    /* The packet_length of a packet only part here, decrypted once */
    pending: Option<usize>,

    output: Vec<u8>,
    send_seq: u32,
    send_key: Option<ChaChaPoly>,

    /* The exchange hash's inputs that outlive one message */
    v_c: Vec<u8>,
    v_s: Vec<u8>,
    /* Ours, once sent, for the exchange in progress */
    i_s: Option<Vec<u8>>,
    session_id: Option<[u8; 32]>,
    strict: bool,

    /* A deadline no wait may pass -- a key exchange the client starts
       included -- and what it is for: the login grace. u64::MAX when there
       is none. */
    limit: u64,
    limit_what: &'static str,
}

impl<'a> Transport<'a> {
    /// `seed` keys the session's random stream; `software` is what follows
    /// "SSH-2.0-" in the server's version line.
    pub fn new(link: &'a mut dyn Link, host: &'a HostKey, seed: &[u8; 32], software: &str) -> Self {
        let mut v_s = Vec::with_capacity(8 + software.len());
        v_s.extend_from_slice(b"SSH-2.0-");
        v_s.extend_from_slice(software.as_bytes());
        Self {
            link,
            host,
            rng: Rng::new(seed),
            input: vec![0u8; INPUT_SIZE],
            start: 0,
            end: 0,
            recv_seq: 0,
            recv_key: None,
            pending: None,
            output: Vec::new(),
            send_seq: 0,
            send_key: None,
            v_c: Vec::new(),
            v_s,
            i_s: None,
            session_id: None,
            strict: false,
            limit: u64::MAX,
            limit_what: "",
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.link.now_ms()
    }

    pub fn session_id(&self) -> &[u8] {
        match &self.session_id {
            Some(id) => id,
            None => &[],
        }
    }

    /// A deadline nothing may pass until `clear_limit`: a read running into
    /// it -- in a key exchange the client starts, too, and with a packet
    /// already buffered -- fails as a timeout of `what`. A client must not
    /// be able to stay logging in for good by rekeying.
    pub fn set_limit(&mut self, at: u64, what: &'static str) {
        self.limit = at;
        self.limit_what = what;
    }

    pub fn clear_limit(&mut self) {
        self.limit = u64::MAX;
    }

    /// The version exchange and the first key exchange, by `deadline`.
    pub fn handshake(&mut self, deadline: u64) -> Result<()> {
        let mut hello = self.v_s.clone();
        hello.extend_from_slice(b"\r\n");
        if !self.link.send(&hello) {
            return Err(Error::Closed);
        }
        self.read_version(deadline)?;
        self.send_kexinit()?;

        /* The client's KEXINIT: first of all in a strict key exchange --
           which the KEXINIT itself says -- and after any IGNOREs and DEBUGs
           in one that is not */
        let mut payload = Vec::new();
        let mut before = 0;
        loop {
            self.read_by(&mut payload, deadline, "the client's KEXINIT")?;
            match payload[0] {
                MSG_KEXINIT => break,
                MSG_IGNORE | MSG_DEBUG => before += 1,
                MSG_DISCONNECT => return Err(Error::Disconnected),
                _ => return Err(Error::Protocol("a first packet that is not a KEXINIT")),
            }
        }
        self.key_exchange(payload, deadline, before)
    }

    fn read_version(&mut self, deadline: u64) -> Result<()> {
        for _ in 0..=PRE_VERSION_LINES {
            let newline = loop {
                if let Some(i) = self.input[self.start..self.end].iter().position(|&b| b == b'\n') {
                    break self.start + i;
                }
                if self.end - self.start >= VERSION_MAX {
                    return Err(Error::Protocol("a version line longer than 255 bytes"));
                }
                if !self.fill(deadline)? {
                    return Err(Error::Timeout("the client's version line"));
                }
            };

            let mut line = &self.input[self.start..newline];
            if let Some(stripped) = line.strip_suffix(b"\r") {
                line = stripped;
            }
            let version = line.starts_with(b"SSH-");
            let text = line.to_vec();
            self.start = newline + 1;

            if version {
                if !text.starts_with(b"SSH-2.0-") && !text.starts_with(b"SSH-1.99-") {
                    return Err(Error::Protocol("a client that does not speak SSH 2.0"));
                }
                self.v_c = text;
                return Ok(());
            }
        }
        Err(Error::Protocol("no version line"))
    }

    /// Reads more of what the client sent, waiting until `deadline` at the
    /// most: false if nothing came by then.
    fn fill(&mut self, deadline: u64) -> Result<bool> {
        if self.start != 0 {
            self.input.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        if self.end == self.input.len() {
            return Err(Error::Protocol("a packet larger than the server takes"));
        }

        loop {
            if self.link.stopping() {
                return Err(Error::Stopped);
            }
            let now = self.link.now_ms();
            if now >= deadline {
                return Ok(false);
            }
            let wait = core::cmp::min(POLL_MS, deadline - now);
            match self.link.recv(&mut self.input[self.end..], wait) {
                Recv::Data(n) if n != 0 => {
                    self.end += n;
                    return Ok(true);
                }
                Recv::Data(_) | Recv::Timeout => {}
                Recv::Closed => return Err(Error::Closed),
            }
        }
    }

    /// The next packet's payload, into `payload` -- its first byte is the
    /// message number -- whatever the message: false when `deadline`
    /// passes before a whole packet is here, which leaves any part of one
    /// that is for the next call to finish.
    pub fn read(&mut self, payload: &mut Vec<u8>, deadline: u64) -> Result<bool> {
        loop {
            /* Before a packet already here as well: what arrived in one
               write must not carry a client past the limit */
            if self.link.now_ms() >= self.limit {
                return Err(Error::Timeout(self.limit_what));
            }
            if self.parse(payload)? {
                return Ok(true);
            }
            if !self.fill(core::cmp::min(deadline, self.limit))? {
                if self.link.now_ms() >= self.limit {
                    return Err(Error::Timeout(self.limit_what));
                }
                return Ok(false);
            }
        }
    }

    /// `read`, for a wait that must not run out: `what` it was for if it
    /// does.
    fn read_by(&mut self, payload: &mut Vec<u8>, deadline: u64, what: &'static str) -> Result<()> {
        if self.read(payload, deadline)? {
            Ok(())
        } else {
            Err(Error::Timeout(what))
        }
    }

    /// Takes a packet out of the input if a whole one is there.
    fn parse(&mut self, payload: &mut Vec<u8>) -> Result<bool> {
        if self.end - self.start < 4 {
            return Ok(false);
        }

        let len = match &self.recv_key {
            None => {
                let b = &self.input[self.start..self.start + 4];
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
            }
            Some(key) => match self.pending {
                Some(len) => len,
                None => {
                    let len = key.length(self.recv_seq, &self.input[self.start..self.start + 4]) as usize;
                    self.pending = Some(len);
                    len
                }
            },
        };
        if !(PACKET_MIN..=PACKET_MAX).contains(&len) {
            return Err(Error::Protocol("a bad packet length"));
        }

        let total = 4 + len + if self.recv_key.is_some() { TAG_LEN } else { 0 };
        if self.end - self.start < total {
            return Ok(false);
        }

        let packet = &mut self.input[self.start..self.start + total];
        if let Some(key) = &self.recv_key {
            let (body, tag) = packet.split_at_mut(4 + len);
            if !key.open(self.recv_seq, body, tag) {
                return Err(Error::Mac);
            }
        }
        let padding = packet[4] as usize;
        if padding < PADDING_MIN || padding + 1 >= len {
            return Err(Error::Protocol("bad padding"));
        }
        payload.clear();
        payload.extend_from_slice(&packet[5..4 + len - padding]);

        self.start += total;
        self.pending = None;
        self.recv_seq = self.recv_seq.wrapping_add(1);
        Ok(true)
    }

    /// Sends one packet: `payload` padded, and encrypted and tagged once
    /// there are keys.
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        /* With the AEAD the length is not part of what is padded */
        let unpadded = if self.send_key.is_some() { 1 } else { 4 + 1 } + payload.len();
        let mut padding = BLOCK - unpadded % BLOCK;
        if padding < PADDING_MIN {
            padding += BLOCK;
        }
        let len = 1 + payload.len() + padding;

        self.output.clear();
        self.output.extend_from_slice(&(len as u32).to_be_bytes());
        self.output.push(padding as u8);
        self.output.extend_from_slice(payload);
        let at = self.output.len();
        self.output.resize(at + padding, 0);
        self.rng.fill(&mut self.output[at..]);
        if let Some(key) = &self.send_key {
            let tag = key.seal(self.send_seq, &mut self.output);
            self.output.extend_from_slice(&tag);
        }
        self.send_seq = self.send_seq.wrapping_add(1);

        if self.link.send(&self.output) {
            Ok(())
        } else if self.link.stopping() {
            Err(Error::Stopped)
        } else {
            Err(Error::Closed)
        }
    }

    /// The next message for the layers above, into `payload`. The
    /// transport's own are dealt with here: IGNORE, DEBUG and UNIMPLEMENTED
    /// passed over, a KEXINIT answered with a key exchange run to its end,
    /// a DISCONNECT the end. False when `deadline` passes first.
    pub fn next(&mut self, payload: &mut Vec<u8>, deadline: u64) -> Result<bool> {
        loop {
            if !self.read(payload, deadline)? {
                return Ok(false);
            }
            match payload[0] {
                MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED => {}
                MSG_DISCONNECT => return Err(Error::Disconnected),
                MSG_KEXINIT => {
                    let i_c = core::mem::take(payload);
                    let deadline = core::cmp::min(self.link.now_ms().saturating_add(REKEY_MS), self.limit);
                    self.key_exchange(i_c, deadline, 0)?;
                }
                _ => return Ok(true),
            }
        }
    }

    /// Tells the client its last message is not one the server knows (RFC
    /// 4253 11.4).
    pub fn unimplemented(&mut self) -> Result<()> {
        let mut p = Vec::with_capacity(5);
        p.push(MSG_UNIMPLEMENTED);
        put_u32(&mut p, self.recv_seq.wrapping_sub(1));
        self.send(&p)
    }

    /// Says why the server ends the connection (RFC 4253 11.1): as far as it
    /// gets, the connection being on its way out or gone.
    pub fn disconnect(&mut self, reason: u32, text: &str) {
        let mut p = Vec::with_capacity(16 + text.len());
        p.push(MSG_DISCONNECT);
        put_u32(&mut p, reason);
        put_string(&mut p, text.as_bytes());
        put_string(&mut p, b"");
        let _ = self.send(&p);
    }

    fn send_kexinit(&mut self) -> Result<()> {
        let mut p = Vec::with_capacity(256);
        p.push(MSG_KEXINIT);
        let mut cookie = [0u8; 16];
        self.rng.fill(&mut cookie);
        p.extend_from_slice(&cookie);
        if self.session_id.is_none() {
            let mut kex = KEX.to_vec();
            kex.push(KEX_STRICT_SERVER);
            put_name_list(&mut p, &kex);
        } else {
            put_name_list(&mut p, KEX);
        }
        put_name_list(&mut p, HOST_KEYS);
        put_name_list(&mut p, CIPHERS);
        put_name_list(&mut p, CIPHERS);
        put_name_list(&mut p, MACS);
        put_name_list(&mut p, MACS);
        put_name_list(&mut p, COMPRESSION);
        put_name_list(&mut p, COMPRESSION);
        put_name_list(&mut p, &[]);
        put_name_list(&mut p, &[]);
        wire::put_bool(&mut p, false);
        put_u32(&mut p, 0);

        self.send(&p)?;
        self.i_s = Some(p);
        Ok(())
    }

    /// A key exchange, from the client's KEXINIT (`i_c`) to the NEWKEYS
    /// both ways; ours goes out first unless it has already. The first
    /// makes the session id, and a later one keeps it. `skipped` is the
    /// packets that came before the first KEXINIT.
    fn key_exchange(&mut self, i_c: Vec<u8>, deadline: u64, skipped: usize) -> Result<()> {
        if self.i_s.is_none() {
            self.send_kexinit()?;
        }
        let i_s = self.i_s.take().unwrap_or_default();
        let first = self.session_id.is_none();

        let mut r = Reader::new(&i_c[1..]);
        r.bytes(16).ok_or(BAD_KEXINIT)?;
        let kex = r.string().ok_or(BAD_KEXINIT)?;
        let host_keys = r.string().ok_or(BAD_KEXINIT)?;
        let cipher_in = r.string().ok_or(BAD_KEXINIT)?;
        let cipher_out = r.string().ok_or(BAD_KEXINIT)?;
        r.string().ok_or(BAD_KEXINIT)?;
        r.string().ok_or(BAD_KEXINIT)?;
        let compression_in = r.string().ok_or(BAD_KEXINIT)?;
        let compression_out = r.string().ok_or(BAD_KEXINIT)?;
        r.string().ok_or(BAD_KEXINIT)?;
        r.string().ok_or(BAD_KEXINIT)?;
        let guess_follows = r.bool().ok_or(BAD_KEXINIT)?;

        if first && wire::has_name(kex, KEX_STRICT_CLIENT) {
            self.strict = true;
            if skipped != 0 {
                return Err(Error::Protocol("packets before the KEXINIT of a strict key exchange"));
            }
        }
        let kex_alg = wire::choose(kex, KEX).ok_or(Error::NoAlgorithm("key exchange method"))?;
        let host_alg = wire::choose(host_keys, HOST_KEYS).ok_or(Error::NoAlgorithm("host key type"))?;
        wire::choose(cipher_in, CIPHERS).ok_or(Error::NoAlgorithm("cipher"))?;
        wire::choose(cipher_out, CIPHERS).ok_or(Error::NoAlgorithm("cipher"))?;
        wire::choose(compression_in, COMPRESSION).ok_or(Error::NoAlgorithm("compression"))?;
        wire::choose(compression_out, COMPRESSION).ok_or(Error::NoAlgorithm("compression"))?;

        /* A client that guessed the method and guessed wrong has sent its
           first packet for the method it guessed: that one is dropped (RFC
           4253 7) */
        let mut drop_guess = guess_follows
            && (wire::names(kex).next() != Some(kex_alg.as_bytes())
                || wire::names(host_keys).next() != Some(host_alg.as_bytes()));

        let mut payload = Vec::new();
        loop {
            self.read_by(&mut payload, deadline, "the key exchange")?;
            if core::mem::take(&mut drop_guess) {
                continue;
            }
            match payload[0] {
                MSG_KEX_ECDH_INIT => break,
                MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED if !(first && self.strict) => {}
                MSG_DISCONNECT => return Err(Error::Disconnected),
                _ => return Err(OUT_OF_ORDER),
            }
        }

        let mut r = Reader::new(&payload[1..]);
        let q_c = match r.string() {
            Some(q) if q.len() == 32 => q,
            _ => return Err(Error::Protocol("a malformed ECDH public key")),
        };
        let mut client_public = [0u8; 32];
        client_public.copy_from_slice(q_c);

        let mut secret = [0u8; 32];
        self.rng.fill(&mut secret);
        let server_public = x25519(secret, X25519_BASEPOINT_BYTES);
        let mut shared = x25519(secret, client_public);
        secret.zeroize();
        /* RFC 8731 3: all zeros means the client's point has small order */
        if shared.iter().all(|&b| b == 0) {
            return Err(Error::Protocol("a small-order ECDH public key"));
        }

        let host_blob = self.host.public().blob();
        let mut h = Sha256::new();
        hash_string(&mut h, &self.v_c);
        hash_string(&mut h, &self.v_s);
        hash_string(&mut h, &i_c);
        hash_string(&mut h, &i_s);
        hash_string(&mut h, &host_blob);
        hash_string(&mut h, &client_public);
        hash_string(&mut h, &server_public);
        hash_mpint(&mut h, &shared);
        let exchange_hash: [u8; 32] = h.finalize().into();
        let session_id = *self.session_id.get_or_insert(exchange_hash);

        /* Both directions' keys at once, and the shared secret and the key
           material wiped as soon as they are in the ciphers -- which wipe
           theirs when dropped */
        let mut key_out = derive(&shared, &exchange_hash, b'D', &session_id);
        let mut key_in = derive(&shared, &exchange_hash, b'C', &session_id);
        shared.zeroize();
        let send_next = ChaChaPoly::new(&key_out);
        let recv_next = ChaChaPoly::new(&key_in);
        key_out.zeroize();
        key_in.zeroize();

        let mut reply = Vec::with_capacity(256);
        reply.push(MSG_KEX_ECDH_REPLY);
        put_string(&mut reply, &host_blob);
        put_string(&mut reply, &server_public);
        put_string(&mut reply, &self.host.sign(&exchange_hash));
        self.send(&reply)?;
        self.send(&[MSG_NEWKEYS])?;

        /* Ours in use from the packet after our NEWKEYS, the client's from
           the packet after its own */
        self.send_key = Some(send_next);
        if self.strict {
            self.send_seq = 0;
        }

        loop {
            self.read_by(&mut payload, deadline, "the key exchange")?;
            match payload[0] {
                MSG_NEWKEYS => break,
                MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED if !(first && self.strict) => {}
                MSG_DISCONNECT => return Err(Error::Disconnected),
                _ => return Err(OUT_OF_ORDER),
            }
        }
        self.recv_key = Some(recv_next);
        if self.strict {
            self.recv_seq = 0;
        }
        Ok(())
    }
}

fn hash_string(h: &mut Sha256, s: &[u8]) {
    h.update((s.len() as u32).to_be_bytes());
    h.update(s);
}

/// K goes into the hashes as an mpint: RFC 8731 3.1 reads the 32 bytes of
/// X25519 output as a big-endian number, and an mpint drops leading zero
/// bytes and puts a 0 in front of a top bit that is set. Getting it wrong
/// fails about one connection in two, and one in 256 -- by turns.
fn hash_mpint(h: &mut Sha256, magnitude: &[u8]) {
    let (pad, digits) = wire::mpint_parts(magnitude);
    h.update(((digits.len() + pad as usize) as u32).to_be_bytes());
    if pad {
        h.update([0u8]);
    }
    h.update(digits);
}

/// Key material for one direction (RFC 4253 7.2): HASH(K || H || letter ||
/// session_id), carried on with HASH(K || H || what came before) to the 64
/// bytes the cipher takes.
fn derive(shared: &[u8; 32], exchange_hash: &[u8; 32], letter: u8, session_id: &[u8; 32]) -> [u8; KEY_LEN] {
    let mut h = Sha256::new();
    hash_mpint(&mut h, shared);
    h.update(exchange_hash);
    h.update([letter]);
    h.update(session_id);
    let first: [u8; 32] = h.finalize().into();

    let mut h = Sha256::new();
    hash_mpint(&mut h, shared);
    h.update(exchange_hash);
    h.update(first);
    let second: [u8; 32] = h.finalize().into();

    let mut out = [0u8; KEY_LEN];
    out[..32].copy_from_slice(&first);
    out[32..].copy_from_slice(&second);
    out
}
