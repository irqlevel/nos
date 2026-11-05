//! `wire`: ipcnet's wire (`ipcwire`) on its own -- no kernel in it, so no
//! machine under it.
//!
//! Bytes of every kind decoded: refused or a frame, never a panic, and a
//! frame decoded is one whose encoding is the bytes it came from, so that
//! nothing two hosts say has two meanings. Frames the input makes encoded,
//! cut where the input says and decoded again as they come: each the frame
//! it was. And messages cut into frames, their frames interleaved as a
//! sender may, with mistakes put among them -- a body for a message never
//! opened, a message opened twice, a body longer than what is left --
//! taken by an assembler of limits the input chooses, against a model:
//! every message whole and exact when its last byte comes, every mistake
//! and every limit refused where the model says, and nothing held past its
//! bound.

use std::collections::VecDeque;

use ipcwire::{decode, encode, frames, Assembler, Dest, Error, Frame, Message, To, BODY_MAX, FRAME_MAX, MESSAGE_MAX};

use crate::{reached, Input};

const NAMES: [&[u8]; 6] = [b"kernel.echo", b"nbs.mds", b"a", b"nbs.cs.0", b"Bad", b""];
const HOSTS: [&[u8]; 5] = [b"ex44", b"ax41", b"a-b-1", b"Ex44", b""];

fn frame<'a>(r: &mut Input, data: &'a [u8]) -> Frame<'a> {
    let to = |r: &mut Input| if r.bool() { To::Addr(r.u64()) } else { To::Name(r.pick(&NAMES)) };
    match r.u8() % 9 {
        0 => {
            let mut cluster = [0u8; 16];
            cluster.copy_from_slice(&r.bytes(16)[..16]);
            Frame::Hello { version: r.u16(), cluster, from: r.pick(&HOSTS), to: r.pick(&HOSTS) }
        }
        1 => Frame::Ping { seq: r.u64() },
        2 => Frame::Pong { seq: r.u64() },
        3 => {
            let any = r.u32();
            Frame::Head { msg: r.u32(), to: to(r), from: r.u64(), tag: r.u32(), reference: r.u64(),
                          len: r.pick(&[0u32, 1, 4096, MESSAGE_MAX as u32, MESSAGE_MAX as u32 + 1, any]) }
        }
        4 => Frame::Body { msg: r.u32(), data },
        5 => Frame::Bounced { from: r.u64(), to: to(r), tag: r.u32(), reference: r.u64(), why: r.u8() },
        6 => Frame::Monitor { id: r.u64(), target: r.u64() },
        7 => Frame::Demonitor { id: r.u64() },
        _ => Frame::Down { id: r.u64(), reason: r.u8() },
    }
}

/// Every frame the decoder gives back encodes to the bytes it came from.
fn canonical(bytes: &[u8]) {
    let mut at = 0;
    while at < bytes.len() {
        match decode(&bytes[at..]) {
            Ok(Some((f, n))) => {
                let mut again = Vec::new();
                invariant!(encode(&f, &mut again).is_ok(), "a frame decoded that does not encode: {:?}", f);
                invariant!(again[..] == bytes[at..at + n], "{:?} decoded from bytes it does not encode to", f);
                invariant!(n <= FRAME_MAX, "a frame of {} bytes", n);
                reached("wire: a frame decoded from bytes of the input's");
                at += n;
            }
            /* Waiting for more is right only for a frame that may be whole
             * one day: a head claiming more than a frame is refused at
             * once, or a peer has the reader keep up to 4 GiB */
            Ok(None) => {
                let rest = &bytes[at..];
                if rest.len() >= ipcwire::HEAD {
                    let len = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
                    invariant!(len <= ipcwire::PAYLOAD_MAX, "a head claiming {} bytes is waited on, not refused", len);
                }
                return;
            }
            Err(_) => return,
        }
    }
}

pub fn run(r: &mut Input) {
    /* Bytes as they come: of no frame at all, and a frame of the input's
     * broken -- a bit flipped, its length changed, bytes added (and the
     * length made to say so, or not) or cut off */
    let n = r.below(100) as usize;
    let raw = r.bytes(n);
    canonical(&raw);
    let n = r.below(64) as usize;
    let small = r.bytes(n);
    let mut b = Vec::new();
    let f = frame(r, if small.is_empty() { b"x" } else { &small });
    if encode(&f, &mut b).is_ok() {
        for _ in 0..1 + r.below(3) {
            match r.u8() % 4 {
                0 => {
                    let at = r.below(b.len() as u64) as usize;
                    b[at] ^= 1 << (r.u8() % 8);
                }
                1 if b.len() >= ipcwire::HEAD => {
                    let now = (b.len() - ipcwire::HEAD) as u32;
                    let len = r.pick(&[now + 1, now.wrapping_sub(1), ipcwire::PAYLOAD_MAX as u32 + 1, u32::MAX]);
                    b[4..8].copy_from_slice(&len.to_be_bytes());
                }
                2 => {
                    let n = 1 + r.below(16) as usize;
                    let more = r.bytes(n);
                    b.extend_from_slice(&more);
                    if r.bool() && b.len() >= ipcwire::HEAD {
                        let len = (b.len() - ipcwire::HEAD) as u32;
                        b[4..8].copy_from_slice(&len.to_be_bytes());
                    }
                }
                _ => {
                    let keep = r.below(b.len() as u64 + 1) as usize;
                    b.truncate(keep);
                }
            }
            if b.is_empty() {
                break;
            }
        }
        canonical(&b);
    }

    /* Frames made, encoded, cut and decoded again */
    let mut stream = Vec::new();
    let mut sent = Vec::new();
    let bodies: Vec<Vec<u8>> = (0..4).map(|i| vec![b'a' + i as u8; [1, 100, BODY_MAX, BODY_MAX + 1][i]]).collect();
    for _ in 0..r.below(12) {
        let data = &bodies[r.below(4) as usize];
        let f = frame(r, data);
        let before = stream.len();
        match encode(&f, &mut stream) {
            Ok(()) => sent.push(stream[before..].to_vec()),
            Err(_) => invariant!(stream.len() == before, "a frame refused left {} bytes", stream.len() - before),
        }
    }
    let mut got = Vec::new();
    let mut buf = Vec::new();
    let mut at = 0;
    while at < stream.len() {
        let cut = (1 + r.below(3000) as usize).min(stream.len() - at);
        buf.extend_from_slice(&stream[at..at + cut]);
        at += cut;
        loop {
            match decode(&buf) {
                Ok(Some((_, n))) => {
                    got.push(buf[..n].to_vec());
                    buf.drain(..n);
                }
                Ok(None) => break,
                Err(e) => {
                    invariant!(false, "a frame that encoded was refused: {:?}", e);
                    return;
                }
            }
        }
    }
    invariant!(buf.is_empty() && got == sent, "{} frames sent, {} came back as sent", sent.len(), got.len());

    assemble(r);
}

/// A message of the model: where it is in being sent.
struct Out {
    msg: u32,
    data: Vec<u8>,
    to: Dest,
    from: u64,
    /// Its frames not yet sent: the next first.
    frames: VecDeque<Vec<u8>>,
    /// Whether the assembler took its head.
    opened: bool,
}

fn assemble(r: &mut Input) {
    let max_open = r.pick(&[1usize, 2, 8]);
    let max_bytes = r.pick(&[1usize << 10, 256 << 10, MESSAGE_MAX]);
    let mut a = Assembler::new(max_open, max_bytes);
    /* The model's count of what open messages promised */
    let mut promised = 0usize;
    let mut outs: Vec<Out> = Vec::new();
    for i in 0..r.below(6) {
        /* Under the machine's 512 KiB a block, as the kernel's heap holds
         * everything here to: the megabyte's limit is the heads' to test */
        let len = r.pick(&[0usize, 1, BODY_MAX, BODY_MAX + 1, 100_000, 400_000]);
        let data: Vec<u8> = (0..len).map(|k| (k as u8).wrapping_mul(31).wrapping_add(i as u8)).collect();
        let to = if r.bool() { To::Addr(r.u64()) } else { To::Name(b"nbs.mds") };
        let dest = match to {
            To::Addr(x) => Dest::Addr(x),
            To::Name(n) => Dest::Name(n.to_vec()),
        };
        let msg = i as u32 * 7 + 1;
        let from = r.u64();
        let mut fs = VecDeque::new();
        for f in frames(msg, to, from, 3, 4, &data).expect("a message within the limit") {
            let mut b = Vec::new();
            encode(&f, &mut b).expect("a message's frame");
            fs.push_back(b);
        }
        outs.push(Out { msg, data, to: dest, from, frames: fs, opened: false });
    }
    loop {
        /* One of those with frames left -- whatever the input has left */
        let live: Vec<usize> = (0..outs.len()).filter(|&i| !outs[i].frames.is_empty()).collect();
        if live.is_empty() {
            break;
        }
        let i = live[r.below(live.len() as u64) as usize];
        let Some(bytes) = outs[i].frames.front().cloned() else { continue };
        /* A mistake now and then, in place of the frame due: a body for a
         * message never opened, a message opened twice, a body one byte
         * longer than its message has left */
        let over = vec![0u8; (remaining(&outs[i]) + 1).min(BODY_MAX)];
        if r.chance(8) {
            let mut b = Vec::new();
            let o = &outs[i];
            let mistake = match r.u8() % 3 {
                0 => Frame::Body { msg: 999_999, data: b"x" },
                1 if o.opened => Frame::Head { msg: o.msg, to: To::Addr(1), from: 2, tag: 3, reference: 4, len: 1 },
                _ => Frame::Body { msg: o.msg, data: &over },
            };
            encode(&mistake, &mut b).expect("a frame of the right shape");
            let (f, _) = decode(&b).expect("decodes").expect("whole");
            let left = if o.opened { remaining(o) } else { 0 };
            let expect = match mistake {
                Frame::Body { msg: 999_999, .. } => Some(Error::Message),
                Frame::Head { .. } => Some(Error::Message),
                Frame::Body { .. } if !o.opened => Some(Error::Message),
                Frame::Body { data, .. } if data.len() > left => Some(Error::Length),
                /* One it would take: no mistake, and not made */
                _ => None,
            };
            if let Some(e) = expect {
                let res = a.take(&f);
                invariant!(res == Err(e), "a mistake {:?} answered {:?}, not {:?}", mistake, res, e);
                reached("wire: a mistake refused");
                /* What a connection does with a refusal is end; here the
                 * assembler goes on, as a refusal took nothing */
            }
            continue;
        }
        outs[i].frames.pop_front();
        let (f, _) = decode(&bytes).expect("a frame that encoded").expect("whole");
        let res = a.take(&f);
        match f {
            Frame::Head { len, .. } => {
                if len == 0 {
                    invariant!(matches!(&res, Ok(Some(m)) if m.is_empty()), "an empty message's head gave {:?}", res);
                    continue;
                }
                let fits = a_open(&outs) < max_open && promised + len as usize <= max_bytes;
                if fits {
                    invariant!(res == Ok(None), "a head the limits allowed answered {:?}", res);
                    outs[i].opened = true;
                    promised += len as usize;
                } else {
                    invariant!(res == Err(Error::Limit), "a head past the limits ({} open, {} of {} bytes \
                               promised) answered {:?}", a_open(&outs), promised, max_bytes, res);
                    reached("wire: a head past the limits refused");
                    /* Not taken: its bodies would be a message nobody
                     * opened -- the sender is told nothing more of it */
                    outs[i].frames.clear();
                }
            }
            Frame::Body { .. } => {
                let o = &outs[i];
                if !o.opened {
                    invariant!(res == Err(Error::Message), "a body of a message never opened answered {:?}", res);
                    continue;
                }
                if o.frames.is_empty() {
                    let want = Message { to: o.to.clone(), from: o.from, tag: 3, reference: 4, pieces: Vec::new() };
                    match &res {
                        Ok(Some(m)) => {
                            invariant!(m.to == want.to && m.from == want.from && m.tag == 3 && m.reference == 4
                                           && m.pieces.concat() == o.data && m.pieces.iter().all(|p| p.len() <= BODY_MAX),
                                       "a message of {} bytes came back other than sent", o.data.len());
                            reached("wire: a message put back together");
                        }
                        _ => invariant!(false, "a message's last body answered {:?}", res),
                    }
                    promised -= o.data.len();
                    outs[i].opened = false;
                } else {
                    invariant!(res == Ok(None), "a body before the last answered {:?}", res);
                }
            }
            _ => {}
        }
        invariant!(a.held() == promised && a.held() <= max_bytes, "the assembler holds {} bytes, the model {}",
                   a.held(), promised);
    }
}

/// What a message being sent has left to come, of its bytes.
fn remaining(o: &Out) -> usize {
    o.frames.iter().map(|f| f.len() - 12).sum()
}

/// Messages the model has open.
fn a_open(outs: &[Out]) -> usize {
    outs.iter().filter(|o| o.opened).count()
}
