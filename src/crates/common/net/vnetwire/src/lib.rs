//! vnet's tunnel between hosts (plans/vnet.md, "The tunnel"): what a packet
//! of a cluster network's crosses the underlay in, from the host its sender
//! runs on to the host its receiver does.
//!
//! ```text
//! Ethernet | IPv4, DF | UDP 39241 to 39241, checksum 0 | kind 1, network 3, key 4, counter 8 | the IPv4 packet, sealed | tag 16
//! ```
//!
//! What is here is the UDP datagram's payload -- the header, the packet
//! sealed, the tag -- and what it is checked by; the outer headers are the
//! sender's to write (`netwire::udp`), and what a kind may carry is the
//! receiver's to hold to its table.
//!
//! - **Sealed** with ChaCha20-Poly1305 (RFC 8439) under a key of 32 bytes:
//!   the counter is the nonce -- four zero bytes and its eight, big-endian
//!   -- and the header is the associated data, so a datagram whose kind,
//!   network, key or counter was changed on the way does not open. A key
//!   is one sender's: each host gives every other the key to seal *to it*
//!   with, and a sender's counter goes up by one a datagram, so no nonce is
//!   used twice under a key -- the sender's to keep, reserving a counter
//!   under its lock and sealing outside it (`seal`), and never past the
//!   counter's last.
//! - **Opened** where it lies, the tag checked before a byte is deciphered
//!   (`open`), and only after a key's window (`Window`) said its counter is
//!   neither seen nor too old -- 2048 counters back from the newest -- and
//!   moved on only once the tag held: a datagram sent again, or forged with
//!   a counter of the past, is refused without the cost of an open.
//! - **Keys** go in messages between the hosts' hv (`Message`): one made
//!   anew for every `Key`, its number random, carrying the instance of hv
//!   that made it and the instance it is for -- taken only by that one, a
//!   number had before not taken again -- and a host's new instance has
//!   every key to it and from it dropped.
//!
//! No kernel in it and nothing that allocates: a datagram is sealed and
//! opened in the caller's buffer.

#![no_std]
#![forbid(unsafe_code)]

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};

/// The tunnel's UDP port, from and to: one NAT never gives a guest's flow.
pub const PORT: u16 = 39241;
/// The header's bytes: kind, network, key, counter.
pub const HEADER: usize = 16;
/// The tag's bytes.
pub const TAG: usize = 16;
/// What the tunnel adds to a packet on the underlay: an IPv4 header of 20
/// bytes, UDP's 8, the header and the tag -- what a cluster network's MTU
/// is short of the underlay's.
pub const OVERHEAD: usize = 20 + 8 + HEADER + TAG;
/// A key's bytes.
pub const KEY: usize = 32;
/// The counters a window remembers back from the newest it took.
pub const WINDOW: u64 = 2048;
const WORDS: usize = (WINDOW / 64) as usize;
/// The largest network number: 24 bits.
pub const NETWORK_MAX: u32 = (1 << 24) - 1;

/// What a datagram carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A packet from a port of a network on the sending host to a port of
    /// the same network on the receiving one.
    PortToPort = 1,
    /// A packet from a port on the sending host whose public address is
    /// routed to the receiving one, to go out from it.
    PortToGateway = 2,
    /// A packet the world sent to a public address routed to the sending
    /// host, for the port on the receiving host it is mapped to.
    GatewayToPort = 3,
    /// An echo through the tunnel, and its answer: what a host's silence
    /// and an underlay that does not carry the MTU are found by.
    Echo = 4,
    EchoAnswer = 5,
}

impl Kind {
    pub fn of(byte: u8) -> Option<Kind> {
        match byte {
            1 => Some(Kind::PortToPort),
            2 => Some(Kind::PortToGateway),
            3 => Some(Kind::GatewayToPort),
            4 => Some(Kind::Echo),
            5 => Some(Kind::EchoAnswer),
            _ => None,
        }
    }
}

/// A datagram's header: what it carries, the network it is of, the key it
/// is sealed with -- the receiver's number for it -- and its counter under
/// that key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub network: u32,
    pub key: u32,
    pub counter: u64,
}

impl Header {
    /// The header's bytes; None for a network past 24 bits.
    pub fn bytes(&self) -> Option<[u8; HEADER]> {
        if self.network > NETWORK_MAX {
            return None;
        }
        let mut b = [0u8; HEADER];
        b[0] = self.kind as u8;
        b[1..4].copy_from_slice(&self.network.to_be_bytes()[1..]);
        b[4..8].copy_from_slice(&self.key.to_be_bytes());
        b[8..16].copy_from_slice(&self.counter.to_be_bytes());
        Some(b)
    }

    /// The header at the head of `datagram`, if it has one: None for one
    /// too short to hold a header and a tag, or of no kind there is.
    pub fn read(datagram: &[u8]) -> Option<Header> {
        if datagram.len() < HEADER + TAG {
            return None;
        }
        let kind = Kind::of(datagram[0])?;
        let network = u32::from_be_bytes([0, datagram[1], datagram[2], datagram[3]]);
        let key = u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]);
        let mut c = [0u8; 8];
        c.copy_from_slice(&datagram[8..16]);
        Some(Header { kind, network, key, counter: u64::from_be_bytes(c) })
    }
}

/// The nonce a counter is: four zero bytes and the counter's eight.
fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(n)
}

/// What a datagram's tag holds besides the packet: all of its header, so a
/// datagram whose kind, network, key or counter changed on the way does not
/// open -- one place for both sides, sealing and opening.
fn associated(head: &[u8; HEADER]) -> &[u8] {
    head
}

/// Why a datagram was not sealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealError {
    /// The room given is short of the datagram.
    Room,
    /// A network past 24 bits.
    Network,
}

/// `packet`, under the header `header` -- its key's number and counter the
/// caller's to keep unique: a counter is used once under a key -- sealed
/// with key `key` into `out` as a datagram: its length, or why not. What a
/// sender that reserves a counter under its lock and seals outside it
/// calls.
pub fn seal(key: &[u8; KEY], header: &Header, packet: &[u8], out: &mut [u8]) -> Result<usize, SealError> {
    let len = HEADER + packet.len() + TAG;
    if out.len() < len {
        return Err(SealError::Room);
    }
    let head = header.bytes().ok_or(SealError::Network)?;
    out[..HEADER].copy_from_slice(&head);
    out[HEADER..HEADER + packet.len()].copy_from_slice(packet);
    let cipher = ChaCha20Poly1305::new(key.into());
    let tag = cipher
        .encrypt_in_place_detached(&nonce(header.counter), associated(&head), &mut out[HEADER..HEADER + packet.len()])
        .map_err(|_| SealError::Room)?;
    out[HEADER + packet.len()..len].copy_from_slice(&tag);
    Ok(len)
}

/// `datagram` opened where it lies with key `key`, its counter held to no
/// window: its header and the packet it carried, deciphered in place, or
/// why not -- the tag checked before a byte is deciphered. What a receiver
/// that holds the window under its lock and opens outside it calls, the
/// window asked before (`Window::fresh`) and moved after (`Window::take`).
pub fn open<'a>(key: &[u8; KEY], datagram: &'a mut [u8]) -> Result<(Header, &'a [u8]), OpenError> {
    let header = Header::read(datagram).ok_or(OpenError::Malformed)?;
    let end = datagram.len() - TAG;
    let mut head = [0u8; HEADER];
    head.copy_from_slice(&datagram[..HEADER]);
    let tag = Tag::clone_from_slice(&datagram[end..]);
    let cipher = ChaCha20Poly1305::new(key.into());
    cipher.decrypt_in_place_detached(&nonce(header.counter), associated(&head), &mut datagram[HEADER..end], &tag)
        .map_err(|_| OpenError::Forged)?;
    Ok((header, &datagram[HEADER..end]))
}

/// Why a datagram was not opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// Too short, or of no kind there is.
    Malformed,
    /// Its tag does not hold: forged, changed on the way, or sealed with
    /// another key.
    Forged,
}

/// The counters a key has opened, `WINDOW` back from the newest: one is
/// fresh where it is past the newest, or within the window and not seen.
#[derive(Clone, Debug)]
pub struct Window {
    /// The newest counter taken, and whether any was.
    top: u64,
    any: bool,
    /// Bit `i` of the whole: counter `top - i` taken.
    seen: [u64; WORDS],
}

impl Default for Window {
    fn default() -> Window {
        Window::new()
    }
}

impl Window {
    pub const fn new() -> Window {
        Window { top: 0, any: false, seen: [0; WORDS] }
    }

    fn bit(&self, back: u64) -> bool {
        let i = back as usize;
        self.seen[i / 64] & (1 << (i % 64)) != 0
    }

    fn set(&mut self, back: u64) {
        let i = back as usize;
        self.seen[i / 64] |= 1 << (i % 64);
    }

    /// Whether `counter` may be taken: past the newest, or within the
    /// window and not seen.
    pub fn fresh(&self, counter: u64) -> bool {
        if !self.any || counter > self.top {
            return true;
        }
        let back = self.top - counter;
        back < WINDOW && !self.bit(back)
    }

    /// `counter` taken: the window moved on where it is the newest. A
    /// counter that is not fresh changes nothing.
    pub fn take(&mut self, counter: u64) {
        if !self.fresh(counter) {
            return;
        }
        if !self.any {
            self.any = true;
            self.top = counter;
            self.seen = [0; WORDS];
            self.set(0);
            return;
        }
        if counter > self.top {
            let shift = counter - self.top;
            if shift >= WINDOW {
                self.seen = [0; WORDS];
            } else {
                self.shift(shift as usize);
            }
            self.top = counter;
            self.set(0);
        } else {
            self.set(self.top - counter);
        }
    }

    /// Every bit moved `by` places older -- those past the window let go.
    fn shift(&mut self, by: usize) {
        let words = by / 64;
        let bits = by % 64;
        let mut moved = [0u64; WORDS];
        for (i, slot) in moved.iter_mut().enumerate() {
            let Some(from) = i.checked_sub(words) else { continue };
            let mut w = self.seen[from] << bits;
            if bits != 0 && from > 0 {
                w |= self.seen[from - 1] >> (64 - bits);
            }
            *slot = w;
        }
        self.seen = moved;
    }

    pub fn newest(&self) -> Option<u64> {
        self.any.then_some(self.top)
    }
}

/* ---- the messages keys go in, between the hosts' hv ---- */

const MSG_HELLO: u8 = 1;
const MSG_KEY: u8 = 2;
const MSG_GONE: u8 = 3;
const HELLO_BYTES: usize = 1 + 8;
const KEY_BYTES: usize = 1 + 8 + 8 + 4 + KEY;
const GONE_BYTES: usize = 1 + 8;

/// What one host's hv says to another's about the tunnel's keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// This instance of hv is here: ask it for a key, and drop every key
    /// of an instance before it.
    Hello { instance: u64 },
    /// A key to seal to the sender with, made by instance `made_by` for
    /// instance `made_for` of the receiver's: taken only by that instance,
    /// its number one not had before.
    Key { made_by: u64, made_for: u64, number: u32, key: [u8; KEY] },
    /// This instance is going: every key to it and from it dropped.
    Gone { instance: u64 },
}

impl Message {
    pub fn encode(&self, out: &mut [u8; KEY_BYTES]) -> usize {
        match self {
            Message::Hello { instance } => {
                out[0] = MSG_HELLO;
                out[1..9].copy_from_slice(&instance.to_be_bytes());
                HELLO_BYTES
            }
            Message::Key { made_by, made_for, number, key } => {
                out[0] = MSG_KEY;
                out[1..9].copy_from_slice(&made_by.to_be_bytes());
                out[9..17].copy_from_slice(&made_for.to_be_bytes());
                out[17..21].copy_from_slice(&number.to_be_bytes());
                out[21..21 + KEY].copy_from_slice(key);
                KEY_BYTES
            }
            Message::Gone { instance } => {
                out[0] = MSG_GONE;
                out[1..9].copy_from_slice(&instance.to_be_bytes());
                GONE_BYTES
            }
        }
    }

    /// The message `bytes` are, all of them: None for anything else.
    pub fn decode(bytes: &[u8]) -> Option<Message> {
        let u64_at = |at: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[at..at + 8]);
            u64::from_be_bytes(b)
        };
        match (bytes.first()?, bytes.len()) {
            (&MSG_HELLO, HELLO_BYTES) => Some(Message::Hello { instance: u64_at(1) }),
            (&MSG_KEY, KEY_BYTES) => {
                let mut key = [0u8; KEY];
                key.copy_from_slice(&bytes[21..21 + KEY]);
                Some(Message::Key {
                    made_by: u64_at(1), made_for: u64_at(9),
                    number: u32::from_be_bytes([bytes[17], bytes[18], bytes[19], bytes[20]]), key,
                })
            }
            (&MSG_GONE, GONE_BYTES) => Some(Message::Gone { instance: u64_at(1) }),
            _ => None,
        }
    }
}

/// The most a message is.
pub const MESSAGE_MAX: usize = KEY_BYTES;
