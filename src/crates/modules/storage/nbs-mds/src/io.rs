//! `nbs io`: a volume's bytes written and read through NBS's client
//! (plans/nbs.md, N8) -- nbsdrive's session over this host's mds and the
//! cs's, for the shell and the tests.
//!
//!     nbs io attach <volume> [force]                  attached for writing here: a new generation,
//!                                                     every cs fenced, the chunks left open reconciled
//!     nbs io write <volume> <offset> <bytes> <byte>   <bytes> of <byte>, to every replica in
//!     nbs io read <volume> <offset> <bytes>           from one replica: this host's where it has one
//!     nbs io flush <volume>                           what was written durable on every replica in
//!     nbs io close <volume>                           that, and every chunk written closed on its replicas
//!     nbs io held <volume>                            the volume's chunks each cs holds open
//!     nbs io detach <volume>                          flushed, closed, let go of
//!
//! The first write attaches the volume for writing on this host unless
//! `attach` did; the session is kept for the commands after, until `detach`
//! or another `attach` -- and driven only while one of them runs, or while
//! `nbs volume snapshot` asked here pauses it for a snapshot (N10c2). A read on a host with no session of the volume's
//! attaches read-only for itself and lets go after -- which the metadata
//! refuses while another host writes. `force` takes the volume from a
//! writer elsewhere, whose writes the fences then refuse. Offsets and sizes
//! are whole 4 KiB.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;
use kcore::ipc::{Bounce, Limits, Mailbox, TAG_BULK};
use kcore::time::{boot_time_ns, Duration};
use nbsdrive::Drive;
use nbsmeta::Volume;
use nbswire::{Answer, Op, Reply, Request, CS_NAME, TAG_ANSWER, TAG_REQUEST};

use crate::member::View;
use crate::store::{this_host, Failure};
use crate::{member_view, ModState};

/// What an offset and a size are whole multiples of.
const SECTOR: u64 = 4096;
/// The most a command writes or reads at once: a request's of the session's,
/// well under the most the kernel's heap gives at once (512 KiB) -- a bigger
/// one is several commands.
const IO_MAX: u64 = 256 << 10;
/// How long `held` waits for each cs's answer.
const HELD_MS: u64 = 10_000;
const NS_PER_MS: u64 = 1_000_000;
/// The runs of a read shown at most.
const RUNS_SHOWN: usize = 16;
/// How long a session a snapshot paused is driven after the snapshot's
/// answer, for it to go on -- the word to go on sent before the answer.
const ON_MS: u64 = 5_000;

/// The volumes this host's `nbs io` has a session of, by name: each taken
/// out while a command drives it, so that no lock is held while it waits.
pub type Attachments = BTreeMap<String, Drive>;

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

/// `len` bytes at `offset`: whole 4 KiB, above 0, and at most `IO_MAX`.
fn span(offset: u64, len: u64) -> Result<u32, String> {
    if len == 0 || offset % SECTOR != 0 || len % SECTOR != 0 {
        return Err(String::from("an offset and a size of whole 4 KiB, and a size above 0"));
    }
    if len > IO_MAX {
        return Err(alloc::format!("at most {} KiB at once", IO_MAX >> 10));
    }
    Ok(len as u32)
}

const USAGE: &str = "usage: nbs io attach <volume> [force] | write <volume> <offset> <bytes> <byte> \
                     | read <volume> <offset> <bytes> | flush <volume> | close <volume> | held <volume> \
                     | detach <volume>";

pub fn run(state: &ModState, words: &[&str], out: &mut Output) -> Result<(), String> {
    match words {
        ["attach", name] => attach(state, name, false, out),
        ["attach", name, "force"] => attach(state, name, true, out),
        ["write", name, offset, len, b] => write(state, name, number(offset)?, number(len)?, byte(b)?, out),
        ["read", name, offset, len] => read(state, name, number(offset)?, number(len)?, out),
        ["flush", name] => kept(state, name, out, |d, out| {
            d.flush()?;
            let _ = writeln!(out, "nbs: {} flushed", name);
            Ok(())
        }),
        ["close", name] => kept(state, name, out, |d, out| {
            d.close()?;
            let _ = writeln!(out, "nbs: {} flushed, and every chunk written here closed on its replicas", name);
            Ok(())
        }),
        ["held", name] => held(state, name, out),
        ["detach", name] => detach(state, name, out),
        _ => Err(String::from(USAGE)),
    }
}

/// This host's name, as the metadata has its member.
fn me(state: &ModState) -> Result<String, String> {
    let view = member_view(state)?;
    this_host(&view)
}

/// Volume `name` attached for writing here, at a new generation -- taken
/// from a writer elsewhere with `force` -- its session kept; one kept
/// before let go of first.
fn open(state: &ModState, name: &str, force: bool) -> Result<u64, String> {
    if let Some(old) = state.io.with(|io| io.remove(name)) {
        let _ = old.stop();
    } else if !force {
        /* Attached for writing on this host by a session not this command's
           -- a guest's: the metadata lets this host's attach take it with no
           force, for a session that died with its module, and `nbs io write`
           took a running VM's volume from it in silence (the review of
           2026-11). Refused here unless forced */
        let view = member_view(state)?;
        let here = this_host(&view)?;
        if view.state.volume(name).and_then(|v| v.writer.as_ref()).is_some_and(|w| w.host == here) {
            return Err(alloc::format!("{} is attached for writing on this host by another session -- a guest's? \
                                       `nbs io attach {} force` takes it from that one", name, name));
        }
    }
    /* Its writes to each replica directly, as before N13: nbs-test's
       path for those, nbsload's and hv's along chains */
    let d = Drive::open(name, &me(state)?, true, force, None)?;
    let generation = d.session().generation();
    state.io.with(|io| {
        io.insert(String::from(name), d);
    });
    Ok(generation)
}

fn attach(state: &ModState, name: &str, force: bool, out: &mut Output) -> Result<(), String> {
    let generation = open(state, name, force)?;
    let _ = writeln!(out, "nbs: {} attached for writing here, generation {}", name, generation);
    Ok(())
}

/// `f` done with volume `name`'s session kept here -- taken out meanwhile,
/// put back unless the session failed: then it is said, and dropped.
fn kept(state: &ModState, name: &str, out: &mut Output,
        f: impl FnOnce(&mut Drive, &mut Output) -> Result<(), String>) -> Result<(), String> {
    let mut d = state.io.with(|io| io.remove(name))
        .ok_or_else(|| alloc::format!("{} is not attached here -- or a command of its is under way", name))?;
    let result = f(&mut d, out);
    if let nbsclient::Phase::Failed(why) = d.session().phase() {
        let why = why.clone();
        return Err(match result {
            Err(e) if e == why => why,
            Err(e) => alloc::format!("{}; the session failed: {}", e, why),
            Ok(()) => alloc::format!("the session failed: {}", why),
        });
    }
    state.io.with(|io| {
        io.insert(String::from(name), d);
    });
    result
}

/// The hosts of the cs's that answered reads since `before`.
fn read_from(d: &Drive, before: &BTreeMap<u32, u64>) -> Vec<String> {
    let s = d.session();
    let mut hosts = BTreeSet::new();
    for (cs, &n) in s.reads() {
        if before.get(cs).copied().unwrap_or(0) < n {
            if let Some(h) = s.host_of(*cs) {
                hosts.insert(String::from(h));
            }
        }
    }
    hosts.into_iter().collect()
}

fn write(state: &ModState, name: &str, offset: u64, len: u64, b: u8, out: &mut Output) -> Result<(), String> {
    let n = span(offset, len)?;
    if !state.io.with(|io| io.contains_key(name)) {
        open(state, name, false)?;
    }
    kept(state, name, out, |d, out| {
        let mut data = Vec::new();
        data.try_reserve_exact(n as usize).map_err(|_| String::from("no memory for the data"))?;
        data.resize(n as usize, b);
        d.write(offset, data)?;
        let chunks = (offset + len - 1) / nbscs::SLOT - offset / nbscs::SLOT + 1;
        let _ = writeln!(out, "nbs: {} bytes of {:#04x} written at {} of {}, generation {}: {} chunks, to every \
                               replica in", len, b, offset, name, d.session().generation(), chunks);
        Ok(())
    })
}

/// A read's bytes said as runs of one value.
fn runs(data: &[u8]) -> String {
    let mut runs: Vec<(u8, u64)> = Vec::new();
    for &x in data {
        match runs.last_mut() {
            Some((y, k)) if *y == x => *k += 1,
            _ => runs.push((x, 1)),
        }
    }
    let mut line = String::new();
    for (k, (x, count)) in runs.iter().enumerate() {
        if k == RUNS_SHOWN {
            line.push_str(" ...");
            break;
        }
        let _ = write!(line, " {:#04x}x{}", x, count);
    }
    line
}

fn read(state: &ModState, name: &str, offset: u64, len: u64, out: &mut Output) -> Result<(), String> {
    let n = span(offset, len)?;
    let show = |d: &mut Drive, out: &mut Output| -> Result<(), String> {
        let before = d.session().reads().clone();
        let data = d.read(offset, n)?;
        let _ = writeln!(out, "nbs: {} bytes at {} of {}, from {}:{}", len, offset, name,
                         read_from(d, &before).join(", "), runs(&data));
        Ok(())
    };
    if state.io.with(|io| io.contains_key(name)) {
        return kept(state, name, out, show);
    }
    /* No session here: one read-only, for this read alone */
    let mut d = Drive::open(name, &me(state)?, false, false, None)?;
    let result = show(&mut d, out);
    let stopped = d.stop();
    result.and(stopped)
}

/// A snapshot of volume `volume` as `name`, asked on this host (N10c2):
/// with `nbs io` writing the volume here, its session driven while the
/// snapshot is asked -- as a guest's disk is by a task of its own: left
/// alone between commands, it would not pause, and the snapshot would be
/// given up -- and then until it goes on, its map read anew.
pub fn snapshot(state: &ModState, volume: &str, name: &str) -> Result<u32, Failure> {
    /* Taken out while it is driven, as a command of its own takes it: none
       here, or one under way -- asked as it is */
    let Some(mut d) = state.io.with(|io| io.remove(volume)) else {
        return crate::store::volume_snapshot(state, volume, name, &mut |ms| kcore::task::sleep_ms(ms));
    };
    let asked = crate::store::volume_snapshot(state, volume, name, &mut |ms| d.turn(ms * NS_PER_MS));
    let end = boot_time_ns() + ON_MS * NS_PER_MS;
    while d.session().paused() && boot_time_ns() < end {
        d.turn(nbsdrive::TURN_NS);
    }
    /* Put back unless the session failed: then it is said, and dropped */
    if let nbsclient::Phase::Failed(why) = d.session().phase() {
        let why = alloc::format!("the session of {} here failed: {}", volume, why);
        return Err(Failure::Other(match asked {
            Err(e) => alloc::format!("{}; {}", e.says(), why),
            Ok(_) => why,
        }));
    }
    state.io.with(|io| {
        io.insert(String::from(volume), d);
    });
    asked
}

fn detach(state: &ModState, name: &str, out: &mut Output) -> Result<(), String> {
    let d = state.io.with(|io| io.remove(name))
        .ok_or_else(|| alloc::format!("{} is not attached here -- or a command of its is under way", name))?;
    let generation = d.session().generation();
    d.stop()?;
    let _ = writeln!(out, "nbs: {} detached (generation {})", name, generation);
    Ok(())
}

/// The hosts of the cs's of volume `v`'s chunks, as `view` has them.
fn hosts_of(view: &View, v: &Volume) -> BTreeSet<String> {
    let s = &view.state;
    let mut hosts = BTreeSet::new();
    for &c in v.map.iter() {
        let Some(set) = s.chunks().get(c as usize).and_then(|ch| s.sets().get(ch.set as usize)) else { continue };
        for &cs in &set.cs {
            if let Some(x) = s.cs().get(cs as usize) {
                hosts.insert(x.host.clone());
            }
        }
    }
    hosts
}

/// What each cs of volume `name`'s holds open of it -- asked of each, not
/// of a session: a look at the cs's, for the tests.
fn held(state: &ModState, name: &str, out: &mut Output) -> Result<(), String> {
    let view = member_view(state)?;
    let me = this_host(&view)?;
    let v = view.state.volume(name).ok_or_else(|| alloc::format!("no volume {}", name))?;
    let mb = Mailbox::new(Limits::DEFAULT).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    /* Every host's said, whatever another's came to */
    for (k, h) in hosts_of(&view, v).iter().enumerate() {
        let to = if *h == me { String::from(CS_NAME) } else { alloc::format!("{}@{}", CS_NAME, h) };
        let id = k as u64 + 1;
        let req = Request { id, volume: v.id, generation: 0, op: Op::Held { from: 0 } }.encode();
        let line = match mb.send_to(&to, TAG_REQUEST | TAG_BULK, id, &req) {
            Err(e) => alloc::format!("{} out of reach: {}", to, e.name()),
            Ok(()) => held_answer(&mb, &to, id, name),
        };
        let _ = writeln!(out, "  {:<16} {}", h, line);
    }
    Ok(())
}

/// The answer to `held`'s request `id` of `to`, about volume `name`, said.
fn held_answer(mb: &Mailbox, to: &str, id: u64, name: &str) -> String {
    let end = boot_time_ns() + HELD_MS * NS_PER_MS;
    loop {
        let now = boot_time_ns();
        if now >= end {
            return alloc::format!("no answer from {} in {} s", to, HELD_MS / 1000);
        }
        let Some(m) = mb.recv(Duration::from_nanos(end - now)) else { continue };
        if let Some(b) = m.bounced().filter(|_| m.reference == id) {
            return match b.why {
                Bounce::Gone => alloc::format!("{} is not there -- is nbs-cs loaded on that host?", to),
                why => alloc::format!("{} not reached: {}", to, why.name()),
            };
        }
        if m.tag & !TAG_BULK != TAG_ANSWER {
            continue;
        }
        let Some(a) = Answer::decode(&m.data).filter(|a| a.id == id) else { continue };
        return match a.reply {
            Reply::Held(list) => {
                let more = if list.len() >= nbswire::PAGE { " or more" } else { "" };
                let mut line = alloc::format!("{}{} chunks of {} open:", list.len(), more, name);
                for (c, inc, g) in list.iter().take(RUNS_SHOWN) {
                    let _ = write!(line, " {}.{}@{}", c, inc, g);
                }
                line
            }
            Reply::Refused(why) => String::from(why.says()),
            _ => String::from("answered with no list"),
        };
    }
}
