#![no_std]
#![forbid(unsafe_code)]

//! ipcnet's wire: what two hosts say to each other inside their TLS session
//! (plans/ipc.md, I4) -- frames, and the channel's messages cut into them and
//! put back together.
//!
//! A frame is an 8-byte head -- its kind, flags (none yet: zero), two bytes
//! kept at zero, the length of what follows -- and at most [`FRAME_MAX`] in
//! all: a TLS record's worth, so that a megabyte is cut up and other
//! messages' frames go between its pieces. Numbers are big-endian.
//!
//! [`decode`] takes the bytes as they come and gives a frame once all of it
//! is there, every length checked before anything is kept; whatever it
//! refuses, the connection ends with. [`Assembler`] puts messages back
//! together from their `Head` and `Body` frames: a bounded number at once and
//! a bounded number of bytes, each body kept in pieces of a frame -- no
//! allocation larger than [`PAYLOAD_MAX`], whatever a message's size. Nothing
//! here trusts the other end: what it says is a host's, but a host can be
//! wrong, and a peer's mistake must cost a connection, not the kernel.

extern crate alloc;

use alloc::vec::Vec;

/// The version a `Hello` says, and the one it must say.
pub const VERSION: u16 = 1;
/// A frame's head: kind, flags, two zero bytes, the length of what follows.
pub const HEAD: usize = 8;
/// A frame at most, its head included: a TLS record's worth.
pub const FRAME_MAX: usize = 16 * 1024;
/// What follows a frame's head, at most.
pub const PAYLOAD_MAX: usize = FRAME_MAX - HEAD;
/// A `Body`'s bytes at most: a frame less its message's number.
pub const BODY_MAX: usize = PAYLOAD_MAX - 4;
/// A message's bytes at most: the channel's own limit.
pub const MESSAGE_MAX: usize = 1 << 20;
/// A mailbox's name at most, as the channel has it.
pub const NAME_MAX: usize = 48;
/// A host's name at most: a DNS label's.
pub const HOST_MAX: usize = 63;

/// What a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Ping = 2,
    Pong = 3,
    Head = 4,
    Body = 5,
    Bounced = 6,
    Monitor = 7,
    Demonitor = 8,
    Down = 9,
}

impl Kind {
    fn of(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Hello,
            2 => Kind::Ping,
            3 => Kind::Pong,
            4 => Kind::Head,
            5 => Kind::Body,
            6 => Kind::Bounced,
            7 => Kind::Monitor,
            8 => Kind::Demonitor,
            9 => Kind::Down,
            _ => return None,
        })
    }
}

/// Where a message goes, as the receiving host knows it: one of its
/// mailboxes by address, or by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum To<'a> {
    Addr(u64),
    Name(&'a [u8]),
}

/// One frame, borrowing the bytes it was decoded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// The first frame each way: the version, the cluster both say they
    /// are in, the session -- the dialing host's number for the pair of
    /// connections this is one of, which the other end pairs them by and
    /// says back -- the sender's name and the name it called the other by.
    Hello { version: u16, cluster: [u8; 16], session: u64, from: &'a [u8], to: &'a [u8] },
    Ping { seq: u64 },
    Pong { seq: u64 },
    /// A message begins: its number on this connection, where it goes,
    /// who sent it (an address of the sender's host), its tag and
    /// reference, and how many bytes its `Body` frames bring.
    Head { msg: u32, to: To<'a>, from: u64, tag: u32, reference: u64, len: u32 },
    /// The next of a message's bytes.
    Body { msg: u32, data: &'a [u8] },
    /// A message the receiving host could not deliver -- its mailbox full,
    /// or gone -- told back to its sender.
    Bounced { from: u64, to: To<'a>, tag: u32, reference: u64, why: u8 },
    /// Watch the mailbox `target` (the receiver's), and say `Down` with
    /// `id` when it goes.
    Monitor { id: u64, target: u64 },
    Demonitor { id: u64 },
    Down { id: u64, reason: u8 },
}

/// Why bytes were refused: the connection ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A kind nobody knows, or flags or kept bytes that are not zero.
    Kind,
    /// A length past a limit, or a frame whose fields do not fill it
    /// exactly.
    Length,
    /// A name that is not one: empty, too long, or of bytes no name has.
    Name,
    /// A message's number open twice, or a body for one never opened.
    Message,
    /// More messages open at once, or more of their bytes, than allowed.
    Limit,
}

/// A name a mailbox may have: lowercase letters, digits, `_`, `-` and dots.
fn is_name(b: &[u8]) -> bool {
    !b.is_empty() && b.len() <= NAME_MAX
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-' | b'.'))
}

/// A host's name: a DNS label, lowercase.
fn is_host(b: &[u8]) -> bool {
    !b.is_empty() && b.len() <= HOST_MAX
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Whether `name` is a host's name as the wire carries one -- a DNS label's
/// letters, lowercase, digits and dashes, up to `HOST_MAX` of them: the one
/// rule for hosts.toml, the cluster's registry and the frames alike.
pub fn valid_host(name: &str) -> bool {
    is_host(name.as_bytes())
}

/// What a frame's bytes are read through: each read checked, none past the
/// end.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.buf.len() {
            return Err(Error::Length);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    /// A length byte, then that many bytes.
    fn short(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u8()?;
        self.take(usize::from(n))
    }

    fn to(&mut self) -> Result<To<'a>, Error> {
        match self.u8()? {
            0 => Ok(To::Addr(self.u64()?)),
            1 => {
                let name = self.short()?;
                if is_name(name) { Ok(To::Name(name)) } else { Err(Error::Name) }
            }
            _ => Err(Error::Kind),
        }
    }

    /// Nothing may be left: a frame's fields fill it exactly.
    fn end(&self) -> Result<(), Error> {
        if self.buf.is_empty() { Ok(()) } else { Err(Error::Length) }
    }
}

/// The frame at the start of `buf`, and how many bytes it took; None while
/// all of it has not come.
pub fn decode(buf: &[u8]) -> Result<Option<(Frame<'_>, usize)>, Error> {
    if buf.len() < HEAD {
        return Ok(None);
    }
    let kind = Kind::of(buf[0]).ok_or(Error::Kind)?;
    if buf[1] != 0 || buf[2] != 0 || buf[3] != 0 {
        return Err(Error::Kind);
    }
    let len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if len > PAYLOAD_MAX {
        return Err(Error::Length);
    }
    if buf.len() < HEAD + len {
        return Ok(None);
    }
    let mut r = Reader { buf: &buf[HEAD..HEAD + len] };
    let frame = match kind {
        Kind::Hello => {
            let version = r.u16()?;
            let mut cluster = [0u8; 16];
            cluster.copy_from_slice(r.take(16)?);
            let session = r.u64()?;
            let from = r.short()?;
            let to = r.short()?;
            if !is_host(from) || !is_host(to) {
                return Err(Error::Name);
            }
            Frame::Hello { version, cluster, session, from, to }
        }
        Kind::Ping => Frame::Ping { seq: r.u64()? },
        Kind::Pong => Frame::Pong { seq: r.u64()? },
        Kind::Head => {
            let msg = r.u32()?;
            let to = r.to()?;
            let from = r.u64()?;
            let tag = r.u32()?;
            let reference = r.u64()?;
            let len = r.u32()?;
            if len as usize > MESSAGE_MAX {
                return Err(Error::Length);
            }
            Frame::Head { msg, to, from, tag, reference, len }
        }
        Kind::Body => {
            let msg = r.u32()?;
            let data = r.buf;
            r.buf = &[];
            if data.is_empty() {
                return Err(Error::Length);
            }
            Frame::Body { msg, data }
        }
        Kind::Bounced => {
            let from = r.u64()?;
            let to = r.to()?;
            let tag = r.u32()?;
            let reference = r.u64()?;
            let why = r.u8()?;
            Frame::Bounced { from, to, tag, reference, why }
        }
        Kind::Monitor => Frame::Monitor { id: r.u64()?, target: r.u64()? },
        Kind::Demonitor => Frame::Demonitor { id: r.u64()? },
        Kind::Down => Frame::Down { id: r.u64()?, reason: r.u8()? },
    };
    r.end()?;
    Ok(Some((frame, HEAD + len)))
}

/// What a frame's fields are written through.
struct Writer<'a> {
    out: &'a mut Vec<u8>,
}

impl Writer<'_> {
    fn bytes(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }

    fn short(&mut self, b: &[u8]) {
        /* Checked by the caller: a name's length fits a byte */
        self.out.push(b.len() as u8);
        self.out.extend_from_slice(b);
    }

    fn to(&mut self, to: &To<'_>) {
        match to {
            To::Addr(a) => {
                self.out.push(0);
                self.bytes(&a.to_be_bytes());
            }
            To::Name(n) => {
                self.out.push(1);
                self.short(n);
            }
        }
    }
}

/// `frame`, appended to `out` -- refused, nothing appended, where it breaks
/// a limit the decoder would hold it to. `out` is grown by exactly what the
/// frame takes, at most a frame: never a doubling past the kernel heap's
/// largest block, whatever `out` held.
pub fn encode(frame: &Frame<'_>, out: &mut Vec<u8>) -> Result<(), Error> {
    let to_ok = |to: &To<'_>| match to {
        To::Addr(_) => true,
        To::Name(n) => is_name(n),
    };
    let (kind, len) = match frame {
        Frame::Hello { from, to, .. } => {
            if !is_host(from) || !is_host(to) {
                return Err(Error::Name);
            }
            (Kind::Hello, 2 + 16 + 8 + 1 + from.len() + 1 + to.len())
        }
        Frame::Ping { .. } => (Kind::Ping, 8),
        Frame::Pong { .. } => (Kind::Pong, 8),
        Frame::Head { to, len, .. } => {
            if !to_ok(to) {
                return Err(Error::Name);
            }
            if *len as usize > MESSAGE_MAX {
                return Err(Error::Length);
            }
            (Kind::Head, 4 + to_len(to) + 8 + 4 + 8 + 4)
        }
        Frame::Body { data, .. } => {
            if data.is_empty() || data.len() > BODY_MAX {
                return Err(Error::Length);
            }
            (Kind::Body, 4 + data.len())
        }
        Frame::Bounced { to, .. } => {
            if !to_ok(to) {
                return Err(Error::Name);
            }
            (Kind::Bounced, 8 + to_len(to) + 4 + 8 + 1)
        }
        Frame::Monitor { .. } => (Kind::Monitor, 16),
        Frame::Demonitor { .. } => (Kind::Demonitor, 8),
        Frame::Down { .. } => (Kind::Down, 9),
    };
    debug_assert!(len <= PAYLOAD_MAX);
    out.reserve_exact(HEAD + len);
    let start = out.len();
    out.extend_from_slice(&[kind as u8, 0, 0, 0]);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    let mut w = Writer { out };
    match frame {
        Frame::Hello { version, cluster, session, from, to } => {
            w.bytes(&version.to_be_bytes());
            w.bytes(cluster);
            w.bytes(&session.to_be_bytes());
            w.short(from);
            w.short(to);
        }
        Frame::Ping { seq } | Frame::Pong { seq } => w.bytes(&seq.to_be_bytes()),
        Frame::Head { msg, to, from, tag, reference, len } => {
            w.bytes(&msg.to_be_bytes());
            w.to(to);
            w.bytes(&from.to_be_bytes());
            w.bytes(&tag.to_be_bytes());
            w.bytes(&reference.to_be_bytes());
            w.bytes(&len.to_be_bytes());
        }
        Frame::Body { msg, data } => {
            w.bytes(&msg.to_be_bytes());
            w.bytes(data);
        }
        Frame::Bounced { from, to, tag, reference, why } => {
            w.bytes(&from.to_be_bytes());
            w.to(to);
            w.bytes(&tag.to_be_bytes());
            w.bytes(&reference.to_be_bytes());
            w.out.push(*why);
        }
        Frame::Monitor { id, target } => {
            w.bytes(&id.to_be_bytes());
            w.bytes(&target.to_be_bytes());
        }
        Frame::Demonitor { id } => w.bytes(&id.to_be_bytes()),
        Frame::Down { id, reason } => {
            w.bytes(&id.to_be_bytes());
            w.out.push(*reason);
        }
    }
    debug_assert_eq!(out.len() - start, HEAD + len);
    Ok(())
}

fn to_len(to: &To<'_>) -> usize {
    match to {
        To::Addr(_) => 1 + 8,
        To::Name(n) => 1 + 1 + n.len(),
    }
}

/// Where a message goes, owned: what an assembled message keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dest {
    Addr(u64),
    Name(Vec<u8>),
}

/// A message put back together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub to: Dest,
    pub from: u64,
    pub tag: u32,
    pub reference: u64,
    /// Its bytes, in the pieces they came in: none larger than a frame.
    pub pieces: Vec<Vec<u8>>,
}

impl Message {
    pub fn len(&self) -> usize {
        self.pieces.iter().map(|p| p.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.iter().all(|p| p.is_empty())
    }
}

/// A message being put together.
struct Open {
    msg: u32,
    left: u32,
    message: Message,
}

/// Messages put back together from their frames: at most `max_open` at
/// once, and `max_bytes` of theirs -- what each one's head promised, held
/// or still to come -- so that the bound is on what may be held at all, not
/// on what has come yet; and kept in at most `max_pieces` pieces between
/// them, a piece a body, so that a sender of a byte a frame is refused long
/// before its pieces cost more than their bytes.
pub struct Assembler {
    open: Vec<Open>,
    bytes: usize,
    pieces: usize,
    max_open: usize,
    max_bytes: usize,
    max_pieces: usize,
}

impl Assembler {
    pub fn new(max_open: usize, max_bytes: usize, max_pieces: usize) -> Assembler {
        Assembler { open: Vec::new(), bytes: 0, pieces: 0, max_open, max_bytes, max_pieces }
    }

    /// The pieces its open messages are kept in.
    pub fn pieces(&self) -> usize {
        self.pieces
    }

    /// What its open messages promised, in bytes.
    pub fn held(&self) -> usize {
        self.bytes
    }

    /// How many messages are being put together.
    pub fn open(&self) -> usize {
        self.open.len()
    }

    /// A `Head` or `Body` frame taken in: the message it finishes, if it
    /// does. Any other frame is not the assembler's: `Error::Kind`.
    pub fn take(&mut self, frame: &Frame<'_>) -> Result<Option<Message>, Error> {
        match *frame {
            Frame::Head { msg, to, from, tag, reference, len } => {
                if self.open.iter().any(|o| o.msg == msg) {
                    return Err(Error::Message);
                }
                let to = match to {
                    To::Addr(a) => Dest::Addr(a),
                    To::Name(n) => Dest::Name(n.to_vec()),
                };
                let message = Message { to, from, tag, reference, pieces: Vec::new() };
                if len == 0 {
                    return Ok(Some(message));
                }
                if self.open.len() >= self.max_open || self.bytes + len as usize > self.max_bytes {
                    return Err(Error::Limit);
                }
                self.bytes += len as usize;
                self.open.push(Open { msg, left: len, message });
                Ok(None)
            }
            Frame::Body { msg, data } => {
                let at = self.open.iter().position(|o| o.msg == msg).ok_or(Error::Message)?;
                let o = &mut self.open[at];
                /* Bodies come at most a frame long; never more than the
                 * message said it had left */
                if data.len() > o.left as usize {
                    return Err(Error::Length);
                }
                if self.pieces >= self.max_pieces {
                    return Err(Error::Limit);
                }
                o.message.pieces.push(data.to_vec());
                self.pieces += 1;
                o.left -= data.len() as u32;
                if o.left > 0 {
                    return Ok(None);
                }
                let done = self.open.swap_remove(at);
                self.bytes -= done.message.len();
                self.pieces -= done.message.pieces.len();
                Ok(Some(done.message))
            }
            _ => Err(Error::Kind),
        }
    }
}

/// A message's frames, in order: its `Head`, then its `Body` frames of at
/// most `BODY_MAX` each -- for the sender to put on the connection, other
/// messages' frames between them as it likes.
pub fn frames<'a>(msg: u32, to: To<'a>, from: u64, tag: u32, reference: u64, data: &'a [u8])
    -> Result<impl Iterator<Item = Frame<'a>>, Error>
{
    if data.len() > MESSAGE_MAX {
        return Err(Error::Length);
    }
    let head = Frame::Head { msg, to, from, tag, reference, len: data.len() as u32 };
    Ok(core::iter::once(head).chain(data.chunks(BODY_MAX).map(move |data| Frame::Body { msg, data })))
}

/// What the channel hands the router -- ipcnet's mailbox -- for another
/// host: a message of the channel's tag `TAG_ROUTE` whose payload begins
/// with this head, the message's own bytes after it. Between the channel and
/// the router on one host, not on the wire: the router reads it and says
/// the frames. Little-endian, fixed at [`route::HEAD`] bytes.
pub mod route {
    use super::{is_name, Error, To, NAME_MAX};

    pub const HEAD: usize = 72;
    /// A message to send there.
    pub const SEND: u8 = 1;
    /// Watch a mailbox there, by the id the watcher's host gave the monitor.
    pub const MONITOR: u8 = 2;
    /// Stop watching it.
    pub const DEMONITOR: u8 = 3;

    /// A routed message's head.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Head<'a> {
        pub kind: u8,
        /// The peer, by this host's number for it.
        pub peer: u16,
        pub tag: u32,
        /// Where it goes: the peer's own address of a mailbox, or a name
        /// there.
        pub to: To<'a>,
        /// The message's reference, or the monitor's id.
        pub reference: u64,
    }

    pub fn encode(h: &Head<'_>) -> Result<[u8; HEAD], Error> {
        if !matches!(h.kind, SEND | MONITOR | DEMONITOR) {
            return Err(Error::Kind);
        }
        let mut b = [0u8; HEAD];
        b[0] = h.kind;
        b[2..4].copy_from_slice(&h.peer.to_le_bytes());
        b[4..8].copy_from_slice(&h.tag.to_le_bytes());
        match h.to {
            To::Addr(a) => b[8..16].copy_from_slice(&a.to_le_bytes()),
            To::Name(n) => {
                if !is_name(n) {
                    return Err(Error::Name);
                }
                /* At most NAME_MAX: is_name says so */
                b[1] = n.len() as u8;
                b[24..24 + n.len()].copy_from_slice(n);
            }
        }
        b[16..24].copy_from_slice(&h.reference.to_le_bytes());
        Ok(b)
    }

    pub fn decode(b: &[u8]) -> Result<Head<'_>, Error> {
        if b.len() < HEAD {
            return Err(Error::Length);
        }
        let kind = b[0];
        if !matches!(kind, SEND | MONITOR | DEMONITOR) {
            return Err(Error::Kind);
        }
        let name_len = usize::from(b[1]);
        let u64_at = |at: usize| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[at..at + 8]);
            u64::from_le_bytes(a)
        };
        let to = if name_len == 0 {
            To::Addr(u64_at(8))
        } else {
            if name_len > NAME_MAX {
                return Err(Error::Name);
            }
            let n = &b[24..24 + name_len];
            if !is_name(n) {
                return Err(Error::Name);
            }
            To::Name(n)
        };
        Ok(Head { kind, peer: u16::from_le_bytes([b[2], b[3]]), tag: u32::from_le_bytes([b[4], b[5], b[6], b[7]]), to,
                  reference: u64_at(16) })
    }
}
