//! `nbs io`: a volume's bytes written and read through the cs's that hold
//! its chunks (plans/nbs.md, N7) -- a client of the cs's for the shell and
//! the tests, before hv's (N8).
//!
//!     nbs io attach <volume> [force]                  attached for writing here: a new generation
//!     nbs io write <volume> <offset> <bytes> <byte>   <bytes> of <byte>, to every replica
//!     nbs io read <volume> <offset> <bytes>           from one replica: this host's where it has one
//!     nbs io flush <volume>                           what was written durable on every replica
//!     nbs io close <volume>                           every chunk written closed on every replica
//!     nbs io held <volume>                            the volume's chunks each cs holds open
//!     nbs io detach <volume>
//!
//! The first write attaches the volume for writing on this host -- a new
//! generation -- unless `attach` did, and fences each cs it writes to at it
//! before its first write there; the attach is kept for the commands after,
//! until `detach`, or another `attach`. `force` takes the volume from a
//! writer on another host, whose writes the fences then refuse.
//! A request carries the entry of the metadata its map was read at, which
//! the cs's mds has applied before it vouches for the request. Offsets and
//! sizes are whole 4 KiB.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;
use kcore::ipc::{Bounce, Limits, Mailbox, TAG_BULK};
use kcore::time::{boot_time_ns, Duration};
use nbscs::{ChunkRef, Epoch, SLOT};
use nbsmeta::{Answer as MdsAnswer, Command, State, Volume};
use nbswire::{Answer, Op, Refusal, Reply, Request, CS_NAME, DATA_MAX, TAG_ANSWER, TAG_REQUEST};

use crate::member::View;
use crate::store::this_host;
use crate::{caught_up, member_view, ModState};

/// What a request's offset and size are whole multiples of.
const SECTOR: u64 = 4096;
/// How long a request is tried for, how long each try waits for its answer,
/// and how long before one refused for now -- the cs's mds behind, the cs
/// not serving yet -- is asked again.
const TOTAL_MS: u64 = 30_000;
const TRY_MS: u64 = 10_000;
const AGAIN_MS: u64 = 200;
const NS_PER_MS: u64 = 1_000_000;
/// A request's tag, as it goes: between hosts by the bulk connection, every
/// request of a client's, so that they keep their order.
const REQUEST: u32 = TAG_REQUEST | TAG_BULK;
/// The runs of a read shown at most.
const RUNS_SHOWN: usize = 16;

/// A volume this host's `nbs io` has attached for writing.
#[derive(Default)]
pub struct Attached {
    generation: u64,
    /// The hosts whose cs's are fenced at it.
    fenced: BTreeSet<String>,
    /// The places in the volume's map written.
    written: BTreeSet<u32>,
}

pub type Attachments = BTreeMap<String, Attached>;

/// What a cs answered, kept past its message.
enum Got {
    Fenced(u64),
    Done,
    Data(Vec<u8>),
    Closed(bool),
    Held(Vec<(u32, u32, u64)>),
    Refused(Refusal),
}

fn owned(r: Reply<'_>) -> Got {
    match r {
        Reply::Fenced { generation } => Got::Fenced(generation),
        Reply::Done => Got::Done,
        Reply::Data(d) => Got::Data(d.to_vec()),
        Reply::Closed(c) => Got::Closed(c),
        Reply::Held(h) => Got::Held(h),
        Reply::Refused(why) => Got::Refused(why),
    }
}

/// The cs of host `host`, as a name to send to.
fn cs_at(me: &str, host: &str) -> String {
    if host == me { String::from(CS_NAME) } else { alloc::format!("{}@{}", CS_NAME, host) }
}

/// What one try of a request came to.
enum Try {
    Answered(Got),
    /// The channel gave it back: why.
    Bounced(Bounce),
    Nothing,
}

/// `req` sent to `to` and answered -- sent again while the answer is that
/// it cannot be done yet, the way there was full, or no answer came. A
/// message to another host goes with the request's id as its reference, so
/// that the channel's bounce of it is told apart; one to a name that is not
/// there is said at once.
fn ask(mb: &Mailbox, to: &str, req: &Request<'_>) -> Result<Got, String> {
    let bytes = req.encode();
    let end = boot_time_ns() + TOTAL_MS * NS_PER_MS;
    let mut last = alloc::format!("no answer from {}", to);
    while boot_time_ns() < end {
        if let Err(e) = mb.send_to(to, REQUEST, req.id, &bytes) {
            last = alloc::format!("{} out of reach: {} -- is nbs-cs loaded there?", to, e.name());
            kcore::task::sleep_ms(AGAIN_MS);
            continue;
        }
        let until = (boot_time_ns() + TRY_MS * NS_PER_MS).min(end);
        let got = loop {
            let now = boot_time_ns();
            if now >= until {
                break Try::Nothing;
            }
            let Some(m) = mb.recv(Duration::from_nanos(until - now)) else { continue };
            if let Some(b) = m.bounced().filter(|b| m.reference == req.id && b.tag & !TAG_BULK == TAG_REQUEST) {
                break Try::Bounced(b.why);
            }
            if m.tag & !TAG_BULK != TAG_ANSWER {
                continue;
            }
            /* This request's, not a late one's of a request before it */
            if let Some(a) = Answer::decode(&m.data).filter(|a| a.id == req.id) {
                break Try::Answered(owned(a.reply));
            }
        };
        match got {
            Try::Answered(Got::Refused(why @ (Refusal::Behind { .. } | Refusal::NotReady))) => {
                last = alloc::format!("{}: {}", to, why.says());
                kcore::task::sleep_ms(AGAIN_MS);
            }
            Try::Answered(g) => return Ok(g),
            Try::Bounced(Bounce::Gone) => {
                return Err(alloc::format!("{} is not there -- is nbs-cs loaded on that host?", to));
            }
            Try::Bounced(why) => {
                last = alloc::format!("{} not reached: {}", to, why.name());
                kcore::task::sleep_ms(AGAIN_MS);
            }
            Try::Nothing => last = alloc::format!("no answer from {} in {} s", to, TRY_MS / 1000),
        }
    }
    Err(last)
}

/// Chunk place `position` of volume `v`: the chunk, as a request names it,
/// and the hosts of its replicas.
fn replicas(s: &State, v: &Volume, position: u32) -> Result<(ChunkRef, Vec<String>), String> {
    let c = *v.map.get(position as usize).ok_or_else(|| String::from("past the volume's map"))?;
    let ch = s.chunks().get(c as usize).ok_or_else(|| alloc::format!("chunk {}, which the metadata has not", c))?;
    let set = s.sets().get(ch.set as usize).ok_or_else(|| alloc::format!("set {}, which the metadata has not", ch.set))?;
    let hosts = set.cs.iter().filter_map(|&cs| s.cs().get(cs as usize).map(|c| c.host.clone())).collect();
    Ok((ChunkRef { chunk: c, incarnation: ch.incarnation, epoch: Epoch { set: ch.set, version: set.version } }, hosts))
}

fn number(word: &str) -> Result<u64, String> {
    word.parse::<u64>().map_err(|_| alloc::format!("\"{}\" is not a number", word))
}

/// A byte as it is typed: decimal, or hexadecimal after `0x`.
fn byte(word: &str) -> Result<u8, String> {
    let parsed = match word.strip_prefix("0x") {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => word.parse::<u8>(),
    };
    parsed.map_err(|_| alloc::format!("\"{}\" is not a byte", word))
}

/// `len` bytes at `offset`, inside volume `v` and whole 4 KiB.
fn span(v: &Volume, offset: u64, len: u64) -> Result<(), String> {
    if len == 0 || offset % SECTOR != 0 || len % SECTOR != 0 {
        return Err(String::from("an offset and a size of whole 4 KiB, and a size above 0"));
    }
    if offset.checked_add(len).is_none_or(|end| end > v.size_mib << 20) {
        return Err(alloc::format!("past the volume's {} MiB", v.size_mib));
    }
    Ok(())
}

const USAGE: &str = "usage: nbs io attach <volume> [force] | write <volume> <offset> <bytes> <byte> \
                     | read <volume> <offset> <bytes> | flush <volume> | close <volume> | held <volume> \
                     | detach <volume>";

pub fn run(state: &ModState, words: &[&str], out: &mut Output) -> Result<(), String> {
    match words {
        ["attach", name] => attach_here(state, name, false, out),
        ["attach", name, "force"] => attach_here(state, name, true, out),
        ["write", name, offset, len, b] => write(state, name, number(offset)?, number(len)?, byte(b)?, out),
        ["read", name, offset, len] => read(state, name, number(offset)?, number(len)?, out),
        ["flush", name] => flush(state, name, out),
        ["close", name] => close(state, name, out),
        ["held", name] => held(state, name, out),
        ["detach", name] => detach(state, name, out),
        _ => Err(String::from(USAGE)),
    }
}

fn mailbox() -> Result<Mailbox, String> {
    Mailbox::new(Limits::DEFAULT).map_err(|e| alloc::format!("no mailbox: {}", e.name()))
}

/// Volume `name` attached for writing here -- taken from a writer on
/// another host with `force` -- at a new generation, which the attach kept
/// here is replaced by.
fn attach_new(state: &ModState, name: &str, force: bool) -> Result<u64, String> {
    let v = caught_up(state)?;
    let host = this_host(&v)?;
    let cmd = Command::Attach { name: String::from(name), host, writable: true, force };
    let generation = match crate::store::ask(state, &cmd).map_err(|f| f.says())? {
        MdsAnswer::Attached { generation } => generation,
        other => return Err(alloc::format!("answered {:?}", other)),
    };
    state.io.with(|io| {
        io.insert(String::from(name), Attached { generation, ..Attached::default() });
    });
    Ok(generation)
}

/// Volume `name`'s generation, attached for writing here: the attach kept,
/// or one made now.
fn attach(state: &ModState, name: &str) -> Result<u64, String> {
    match state.io.with(|io| io.get(name).map(|a| a.generation)) {
        Some(g) => Ok(g),
        None => attach_new(state, name, false),
    }
}

fn attach_here(state: &ModState, name: &str, force: bool, out: &mut Output) -> Result<(), String> {
    let generation = attach_new(state, name, force)?;
    let _ = writeln!(out, "nbs: {} attached for writing here, generation {}", name, generation);
    Ok(())
}

/// This host, and volume `name` in `view`.
fn volume_of<'v>(view: &'v View, name: &str) -> Result<(String, &'v Volume), String> {
    let me = this_host(view)?;
    let v = view.state.volume(name).ok_or_else(|| alloc::format!("no volume {}", name))?;
    Ok((me, v))
}

/// The pieces of `len` bytes at `offset`: each inside one chunk, and at most
/// what a request carries -- the chunk's place, where in it, how much.
fn pieces(offset: u64, len: u64) -> impl Iterator<Item = (u32, u32, u32)> {
    let end = offset + len;
    let mut at = offset;
    core::iter::from_fn(move || {
        if at >= end {
            return None;
        }
        let inside = at % SLOT;
        let n = (end - at).min(SLOT - inside).min(u64::from(DATA_MAX));
        let piece = ((at / SLOT) as u32, inside as u32, n as u32);
        at += n;
        Some(piece)
    })
}

fn write(state: &ModState, name: &str, offset: u64, len: u64, b: u8, out: &mut Output) -> Result<(), String> {
    let generation = attach(state, name)?;
    let view = member_view(state)?;
    let (me, v) = volume_of(&view, name)?;
    span(v, offset, len)?;
    let mb = mailbox()?;
    let mut data = Vec::new();
    data.try_reserve_exact(DATA_MAX as usize).map_err(|_| String::from("no memory for the data"))?;
    data.resize(DATA_MAX as usize, b);
    let mut id = 0u64;
    let mut places = BTreeSet::new();
    for (position, inside, n) in pieces(offset, len) {
        let (chunk, hosts) = replicas(&view.state, v, position)?;
        for h in &hosts {
            let to = cs_at(&me, h);
            if !state.io.with(|io| io.get(name).is_some_and(|a| a.fenced.contains(h))) {
                id += 1;
                match ask(&mb, &to, &Request { id, volume: v.id, generation, op: Op::Fence })? {
                    Got::Fenced(g) if g == generation => {
                        state.io.with(|io| {
                            if let Some(a) = io.get_mut(name) {
                                a.fenced.insert(h.clone());
                            }
                        });
                    }
                    Got::Fenced(g) => return Err(alloc::format!("{} is fenced at generation {}, past this one's {}", h,
                                                                g, generation)),
                    Got::Refused(why) => return Err(alloc::format!("{} would not fence {}: {}", h, name, why.says())),
                    _ => return Err(alloc::format!("{} answered a fence with no fence", h)),
                }
            }
            id += 1;
            let op = Op::Write { chunk, position, index: view.applied, offset: inside, data: &data[..n as usize] };
            match ask(&mb, &to, &Request { id, volume: v.id, generation, op })? {
                Got::Done => {}
                Got::Refused(why) => {
                    return Err(alloc::format!("{} refused the write to chunk {} of {}: {}", h, position, name,
                                              why.says()));
                }
                _ => return Err(alloc::format!("{} answered a write with no write", h)),
            }
        }
        places.insert(position);
        state.io.with(|io| {
            if let Some(a) = io.get_mut(name) {
                a.written.insert(position);
            }
        });
    }
    let _ = writeln!(out, "nbs: {} bytes of {:#04x} written at {} of {}, generation {}: {} chunks, each to its 3 \
                           replicas", len, b, offset, name, generation, places.len());
    Ok(())
}

fn read(state: &ModState, name: &str, offset: u64, len: u64, out: &mut Output) -> Result<(), String> {
    let view = member_view(state)?;
    let (me, v) = volume_of(&view, name)?;
    span(v, offset, len)?;
    /* As this host's writer where it is one; else read-only, generation 0 */
    let generation = state.io.with(|io| io.get(name).map_or(0, |a| a.generation));
    let mb = mailbox()?;
    let mut runs: Vec<(u8, u64)> = Vec::new();
    let mut from = BTreeSet::new();
    for (id, (position, inside, n)) in pieces(offset, len).enumerate() {
        let (chunk, hosts) = replicas(&view.state, v, position)?;
        let h = hosts.iter().find(|h| **h == me).or(hosts.first()).ok_or_else(|| String::from("a chunk on no host"))?;
        let op = Op::Read { chunk, position, index: view.applied, offset: inside, len: n };
        match ask(&mb, &cs_at(&me, h), &Request { id: id as u64 + 1, volume: v.id, generation, op })? {
            Got::Data(d) if d.len() == n as usize => {
                for &x in &d {
                    match runs.last_mut() {
                        Some((y, k)) if *y == x => *k += 1,
                        _ => runs.push((x, 1)),
                    }
                }
            }
            Got::Refused(why) => {
                return Err(alloc::format!("{} refused the read of chunk {} of {}: {}", h, position, name,
                                          why.says()));
            }
            _ => return Err(alloc::format!("{} answered a read with no data, or not all of it", h)),
        }
        from.insert(h.clone());
    }
    let mut line = String::new();
    for (k, (x, count)) in runs.iter().enumerate() {
        if k == RUNS_SHOWN {
            line.push_str(" ...");
            break;
        }
        let _ = write!(line, " {:#04x}x{}", x, count);
    }
    let hosts: Vec<&str> = from.iter().map(|h| h.as_str()).collect();
    let _ = writeln!(out, "nbs: {} bytes at {} of {}, from {}:{}", len, offset, name, hosts.join(", "), line);
    Ok(())
}

/// A chunk this host wrote: its place, the chunk, the hosts of its replicas.
type Written = (u32, ChunkRef, Vec<String>);

/// What this host wrote of volume `name`: its generation, the view, and
/// each chunk written.
fn written(state: &ModState, name: &str) -> Result<(u64, View, Vec<Written>), String> {
    let (generation, places) = state.io
        .with(|io| io.get(name).map(|a| (a.generation, a.written.iter().copied().collect::<Vec<u32>>())))
        .ok_or_else(|| alloc::format!("{} is not attached here: `nbs io write` it first", name))?;
    let view = member_view(state)?;
    let mut chunks = Vec::new();
    {
        let (_, v) = volume_of(&view, name)?;
        for p in places {
            let (c, hosts) = replicas(&view.state, v, p)?;
            chunks.push((p, c, hosts));
        }
    }
    Ok((generation, view, chunks))
}

fn flush(state: &ModState, name: &str, out: &mut Output) -> Result<(), String> {
    let (generation, view, chunks) = written(state, name)?;
    let (me, v) = volume_of(&view, name)?;
    let hosts: BTreeSet<String> = chunks.iter().flat_map(|(_, _, h)| h.iter().cloned()).collect();
    let mb = mailbox()?;
    for (k, h) in hosts.iter().enumerate() {
        match ask(&mb, &cs_at(&me, h), &Request { id: k as u64 + 1, volume: v.id, generation, op: Op::Flush })? {
            Got::Done => {}
            Got::Refused(why) => return Err(alloc::format!("{} refused the flush: {}", h, why.says())),
            _ => return Err(alloc::format!("{} answered a flush with no flush", h)),
        }
    }
    let _ = writeln!(out, "nbs: {} flushed on {} hosts", name, hosts.len());
    Ok(())
}

fn close(state: &ModState, name: &str, out: &mut Output) -> Result<(), String> {
    let (generation, view, chunks) = written(state, name)?;
    let (me, v) = volume_of(&view, name)?;
    let mb = mailbox()?;
    let (mut id, mut closed, mut asked) = (0u64, 0u32, 0u32);
    for (position, chunk, hosts) in &chunks {
        for h in hosts {
            id += 1;
            asked += 1;
            let op = Op::Close { chunk: *chunk, position: *position, index: view.applied };
            match ask(&mb, &cs_at(&me, h), &Request { id, volume: v.id, generation, op })? {
                Got::Closed(true) => closed += 1,
                Got::Closed(false) => {}
                Got::Refused(why) => return Err(alloc::format!("{} refused the close: {}", h, why.says())),
                _ => return Err(alloc::format!("{} answered a close with no close", h)),
            }
        }
    }
    let _ = writeln!(out, "nbs: {} of the {} replicas of the chunks of {} written here were open, and are closed",
                     closed, asked, name);
    Ok(())
}

fn held(state: &ModState, name: &str, out: &mut Output) -> Result<(), String> {
    let view = member_view(state)?;
    let (me, v) = volume_of(&view, name)?;
    let mut hosts = BTreeSet::new();
    for p in 0..v.map.len() as u32 {
        hosts.extend(replicas(&view.state, v, p)?.1);
    }
    let mb = mailbox()?;
    /* Every host's said, whatever another's came to */
    for (k, h) in hosts.iter().enumerate() {
        let req = Request { id: k as u64 + 1, volume: v.id, generation: 0, op: Op::Held };
        match ask(&mb, &cs_at(&me, h), &req) {
            Ok(Got::Held(list)) => {
                let mut line = String::new();
                for (c, inc, g) in list.iter().take(RUNS_SHOWN) {
                    let _ = write!(line, " {}.{}@{}", c, inc, g);
                }
                let _ = writeln!(out, "  {:<16} {} chunks of {} open:{}", h, list.len(), name, line);
            }
            Ok(Got::Refused(why)) => {
                let _ = writeln!(out, "  {:<16} {}", h, why.says());
            }
            Ok(_) => {
                let _ = writeln!(out, "  {:<16} answered with no list", h);
            }
            Err(why) => {
                let _ = writeln!(out, "  {:<16} {}", h, why);
            }
        }
    }
    Ok(())
}

fn detach(state: &ModState, name: &str, out: &mut Output) -> Result<(), String> {
    let Some(generation) = state.io.with(|io| io.get(name).map(|a| a.generation)) else {
        return Err(alloc::format!("{} is not attached here", name));
    };
    let v = caught_up(state)?;
    let host = this_host(&v)?;
    crate::store::ask(state, &Command::Detach { name: String::from(name), host, generation }).map_err(|f| f.says())?;
    state.io.with(|io| {
        io.remove(name);
    });
    let _ = writeln!(out, "nbs: {} detached (generation {})", name, generation);
    Ok(())
}
