//! A running VM moved to another host's hv, and one taken from another's
//! (plans/vm-migration.md): the old host's side -- `hv migrate` -- and the
//! new one's, a task answering offers at `hv.migrate`, over migwire's
//! messages and its handover's decisions.
//!
//!     hv migrate <vm> <host>      paused, sent and run there -- or on here,
//!                                 said why
//!
//! The move is M1's, cold: the VM paused from the moment the other host has
//! built it to the moment one host runs it -- its folder's files, every page
//! of its memory and its state sent meanwhile -- and its disks' and its
//! port's handover after that. What moves is a VM of a folder whose written
//! disks are volumes of NBS's -- one at least, the first deciding the
//! handover -- whose read-only disks are volumes or files of its folder,
//! whose NIC, if it has one, is on a network of the cluster's, and with
//! nothing of its host's own: no `forward=`, no `hv forward`, no `public=`.
//! Anything else is refused and said, the VM left as it was.
//!
//! The decision is `migwire::handover`'s: one compare and set in NBS's raft,
//! the first written volume's writer. The new host (D) commits by taking it
//! from the generation the old one (S) names in `Done`; S, hearing nothing
//! in time, gives up by taking it anew from that same generation itself.
//! Whichever lands first decides and the other is refused, so the two never
//! both run the VM; a side that hears nothing it can go by asks again with
//! the same token -- answered as it was, where its take landed -- rather
//! than decide on a guess. A side that stops asking, or dies, runs nothing.
//!
//! D holds one coming VM at a time, and refuses an offer meanwhile.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::Ordering;

use hv::run::{LinuxGuest, Stop};
use kcore::ipc::{Addr, Limits, Mailbox, Scope};
use kcore::task::TaskHandle;
use kcore::time::Duration;
use kcore::trace;
use migwire::handover::{Dest, DestDo, Source, SourceDo, Take};
use migwire::{Msg, Offer, Page, PAGE};
use nbsdrive::NotTaken;

use crate::disk::QueuedDisk;
use crate::guest::Built;
use crate::store::{self, Folder};
use crate::vms::{Coming, Held, StateHold};
use crate::work::{self, Work};
use crate::State;

pub const USAGE: &str = "hv migrate <vm> <host> [cold]";

/// How long S waits for D's answer to its offer: D takes and zeroes the
/// guest's memory before it answers.
const OFFER_MS: u64 = 120_000;
/// How long a side waits for the other: S for an Ack with its window full,
/// D for S's next message before `Done`.
const QUIET_MS: u64 = 60_000;
/// How long S waits for D's word after `Done` before it takes the volume
/// back. D's commit asks NBS once or more, each ask waited out for as long
/// as its session moves (nbsdrive's two minutes), so a slow word is not a
/// lost one: S's take-back is what decides, either way.
const WORD_MS: u64 = 60_000;
/// Between a side's take of a volume and the next, where NBS gave nothing
/// to go by.
const AGAIN_MS: u64 = 1000;
/// How long a side asks for a take before it stops asking: D then runs
/// nothing, as if it had died, and S leaves the VM stopped.
const TAKE_FOR_MS: u64 = 600_000;
/// How long D asks vnet to move the VM's port before the VM runs without
/// its network.
const PORT_FOR_MS: u64 = 30_000;
/// A turn of a wait, at which a side looks whether the module is going.
const TURN_MS: u64 = 200;
/// D's mailbox: S's window twice over -- its bytes and its messages.
const D_LIMITS: Limits = Limits { messages: 2 * migwire::WINDOW_MESSAGES as u32, bytes: 2 * migwire::WINDOW_BYTES };
/// S's: D's answers alone.
const S_LIMITS: Limits = Limits { messages: 64, bytes: 64 * 1024 };
/// What S reads its guest's memory in, and its files.
const READ_PIECE: usize = 64 * 1024;
/// The VMs D remembers running, by their migrations: a late `Done` of one
/// answered `Running` again.
const REMEMBERED: usize = 8;
/// A line D says, at most.
const LINE_MAX: usize = 240;
/// The pause a live move aims at (plans/vm-migration.md, D4): the rounds go
/// on until what is left to send at the last one's rate fits in it.
const PAUSE_TARGET_MS: u64 = 300;
/// The rounds after which the guest is slowed, more each round, and those
/// after which the move is given up, the VM staying where it is.
const SLOW_AFTER: u32 = 5;
const ROUNDS_MAX: u32 = 30;
/// How slowed the guest is in its first slowed round, percent, and how
/// much more each round after (`LinuxGuest::throttle`, at most
/// `hv::run::THROTTLE_MAX`).
const THROTTLE_FIRST: u32 = 30;
const THROTTLE_STEP: u32 = 10;

fn now_ms() -> u64 {
    kcore::time::boot_time_ns() / kcore::consts::NS_PER_MS
}

/// A random number other than 0: a migration's id, a take's token.
fn random_nonzero() -> Result<u64, String> {
    kcore::random::random_u64().filter(|&n| n != 0).ok_or_else(|| String::from("no randomness"))
}

/// A flow of the VM's NIC's firewall, as the message carries it.
fn flow_out(c: &crate::net::Carried) -> migwire::Flow {
    migwire::Flow {
        proto: c.key.proto, local_port: c.key.local_port, peer: c.key.peer, peer_port: c.key.peer_port,
        inbound: c.inbound, seen: c.seen,
        left_ms: u32::try_from(c.left_ns / kcore::consts::NS_PER_MS).unwrap_or(u32::MAX).max(1),
    }
}

/// And as the switch takes it.
fn flow_in(f: &migwire::Flow) -> crate::net::Carried {
    crate::net::Carried {
        key: firewall::Key { proto: f.proto, local_port: f.local_port, peer: f.peer, peer_port: f.peer_port },
        inbound: f.inbound, seen: f.seen, left_ns: u64::from(f.left_ms) * kcore::consts::NS_PER_MS,
    }
}

/// `msg` to `to`.
fn send(mb: &Mailbox, to: Addr, msg: &Msg) -> Result<(), String> {
    let bytes = msg.encode().ok_or_else(|| String::from("a message past its bound, or no memory for it"))?;
    mb.send(to, migwire::TAG, 0, &bytes).map_err(|e| alloc::format!("not sent: {}", e.name()))
}

/// A reason said back: within a message's bound, cut on a character.
fn clip(why: &str) -> String {
    let mut end = why.len().min(migwire::WHY_MAX);
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    String::from(&why[..end])
}

/// What one look at a mailbox found from the other side.
enum Heard {
    /// A message of this migration, from whom it came.
    Msg(Msg, Addr),
    /// One of ours that did not get there, and why.
    Bounced(&'static str),
    Nothing,
}

/// The next message of migration `id` -- from `from`, where known -- waiting
/// up to `wait_ms` for one.
fn hear(mb: &Mailbox, from: Option<Addr>, id: u64, wait_ms: u64) -> Heard {
    let Some(m) = mb.recv(Duration::from_millis(wait_ms)) else { return Heard::Nothing };
    if let Some(b) = m.bounced() {
        return if b.tag == migwire::TAG { Heard::Bounced(b.why.name()) } else { Heard::Nothing };
    }
    if m.is_system() || m.tag != migwire::TAG || from.is_some_and(|f| f != m.from) {
        return Heard::Nothing;
    }
    match Msg::decode(&m.data) {
        Some(msg) if msg.id() == id => Heard::Msg(msg, m.from),
        _ => Heard::Nothing,
    }
}

/// What D says, into the log a line at a time: it has no shell to say it
/// in.
struct Log {
    line: String,
}

impl Log {
    fn new() -> Log {
        let mut line = String::new();
        let _ = line.try_reserve_exact(LINE_MAX);
        Log { line }
    }

    fn flush(&mut self) {
        if !self.line.is_empty() {
            kcore::klog!(0, "{}", self.line);
            self.line.clear();
        }
    }
}

impl Write for Log {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            if c == '\n' {
                self.flush();
            } else if self.line.len() + c.len_utf8() <= self.line.capacity() {
                /* Into the room taken at `new`: a longer line is cut */
                self.line.push(c);
            }
        }
        Ok(())
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        self.flush();
    }
}

/* ---------------------------------------------------------------------- */
/* The old host's side                                                    */
/* ---------------------------------------------------------------------- */

/// `hv migrate <vm> <host>`.
pub fn command(state: &State, args: &str, out: &mut kcore::cmd::Output) {
    let mut w = args.split_ascii_whitespace();
    let (Some(word), Some(host)) = (w.next(), w.next()) else {
        let _ = writeln!(out, "{}", USAGE);
        return;
    };
    let cold = match (w.next(), w.next()) {
        (None, _) => false,
        (Some("cold"), None) => true,
        _ => {
            let _ = writeln!(out, "{}", USAGE);
            return;
        }
    };
    let mut shell = work::Shell { out, unloading: &state.unloading };
    if let Err(e) = migrate(state, word, host, cold, &mut shell) {
        let _ = writeln!(shell, "hv: {}", e);
    }
}

/// What a move came to, for the job that asked it (the `vm` provider's
/// `migrate`): how many rounds its memory went in, how long the VM was
/// paused, the bytes of its memory sent, its firewall's flows carried.
pub struct Moved {
    pub rounds: u32,
    pub paused_ms: u64,
    pub sent_bytes: u64,
    pub flows: usize,
}

/// How far a move has got, as a job's progress: the pages sent of its
/// memory's -- each round's on top of the first's, never past the last
/// before it is done.
struct Progress {
    done: u64,
    total: u64,
}

impl Progress {
    fn add(&mut self, pages: u64, work: &mut dyn Work) {
        self.done = self.done.saturating_add(pages);
        work.progress(self.done.min(self.total.saturating_sub(1)), self.total);
    }
}

/// What a move stopped by its cancel -- or the module's going -- says: before
/// its last round's end, the VM runs on here as it was.
fn cancelled() -> String {
    work::stopped_leaving("it runs here still, as it was")
}

/// What S found of the VM, before it offers it.
struct Plan {
    folder: Folder,
    offer: Offer,
    /// Its written volumes, in its disks' order: a disk's index and word.
    written: Vec<(usize, String)>,
    /// The files of its folder that go with it, by name, path and size.
    files: Vec<(String, String, u64)>,
    /// Its RAM, each range's base and size, lowest first.
    ranges: Vec<(u64, u64)>,
}

/// Why `taken` does not move, or what moving it takes: its folder read, its
/// disks, NIC and files looked at, the offer made of them.
fn plan(state: &State, taken: &StateHold<'_>, me: &str) -> Result<Plan, String> {
    let oom = || String::from("out of memory");
    let folder = Folder { name: taken.name.clone(), path: taken.folder.clone() };
    let (text, config) = store::load_text(&folder)?;
    let o = store::options(&folder, &config)?;
    if o.disks.len() != taken.disks.len() || o.disks.iter().zip(&taken.disks).any(|((p, _), q)| p != q) {
        return Err(String::from("its vm.toml names other disks than it runs with -- hv restart it first"));
    }
    let raw: Vec<String> = config.options.disks.iter().map(|(w, _)| w.clone()).collect();
    let spec = crate::guest::spec(o)?;
    if !spec.forwards.is_empty() {
        return Err(String::from("its vm.toml has forward=: a port of this host's address is this host's -- take \
                                 them out first"));
    }
    if state.forwards.rows().iter().any(|r| r.vm == taken.id) {
        return Err(String::from("hv forward relays a port of this host's to it -- hv forward del it first"));
    }
    if spec.public.is_some() {
        return Err(String::from("its public= address is this host's NAT's"));
    }
    let cluster = spec.network.as_deref().and_then(crate::config::cluster_network);
    if spec.net && cluster.is_none() {
        return Err(String::from("its NIC is on this host's network: only a network of the cluster's goes with it"));
    }

    /* Its disks: a volume, written or not, or a file of its folder's read
     * alone -- which goes with the folder */
    let mut written = Vec::new();
    for (i, (path, ro)) in spec.disks.iter().enumerate() {
        match crate::devices::source(path) {
            crate::devices::Source::Volume(_) => {
                if !ro {
                    written.try_reserve(1).map_err(|_| oom())?;
                    written.push((i, path.clone()));
                }
            }
            crate::devices::Source::File(p) => {
                if !ro {
                    return Err(alloc::format!("its disk {} is a file of this host's that it writes: only volumes of \
                                               NBS's go with it", path));
                }
                let inside = p.strip_prefix(folder.path.as_str()).and_then(|r| r.strip_prefix('/'))
                    .is_some_and(migwire::file_name);
                if !inside {
                    return Err(alloc::format!("its disk {} is a file outside its folder", path));
                }
            }
            _ => return Err(alloc::format!("its disk {} is a device of this host's", path)),
        }
    }
    if written.is_empty() {
        return Err(String::from("it writes no volume of NBS's: nothing decides which host runs it"));
    }

    /* Its folder's files: each a file, not too big, and not too many */
    let entries = kcore::fs::list(&folder.path).map_err(|e| alloc::format!("{}: {}", folder.path, e))?;
    let mut files = Vec::new();
    for e in entries {
        let Ok(name) = core::str::from_utf8(&e.name) else {
            return Err(alloc::format!("{} has a file whose name is no text", folder.path));
        };
        if e.dir {
            return Err(alloc::format!("{}/{} is a folder: only files go with a VM", folder.path, name));
        }
        if name == store::CONFIG {
            continue;
        }
        if !migwire::file_name(name) || name.len() > migwire::NAME_MAX {
            return Err(alloc::format!("{}/{}: a name another host would not take", folder.path, name));
        }
        let path = store::join(&folder.path, name).ok_or_else(oom)?;
        let size = kcore::fs::size(&path).map_err(|e| alloc::format!("{}: {}", path, e))?;
        if size > migwire::FILE_MAX {
            return Err(alloc::format!("{} is more than {} MiB -- take it out of its folder first", path,
                                      migwire::FILE_MAX >> 20));
        }
        if files.len() == migwire::FILES_MAX {
            return Err(alloc::format!("its folder has more than {} files", migwire::FILES_MAX));
        }
        files.try_reserve(1).map_err(|_| oom())?;
        files.push((String::from(name), path, size));
    }

    let guest = &taken.guest;
    let shapes = guest.disk_shapes();
    if shapes.len() != spec.disks.len() || raw.len() != spec.disks.len() {
        return Err(String::from("its disks are not its vm.toml's"));
    }
    let mut disks = Vec::new();
    disks.try_reserve_exact(shapes.len()).map_err(|_| oom())?;
    for (s, word) in shapes.iter().zip(&raw) {
        let end = s.id.iter().position(|&b| b == 0).unwrap_or(s.id.len());
        let serial = core::str::from_utf8(&s.id[..end]).map_err(|_| String::from("a disk's id that is no text"))?;
        disks.push(migwire::Disk { word: word.clone(), bytes: s.bytes, block: s.block, read_only: s.read_only,
                                   discards: s.discards, writes_zeroes: s.writes_zeroes, serial: String::from(serial) });
    }
    let mut nics = Vec::new();
    if let Some(network) = cluster {
        let uuid = spec.uuid.ok_or_else(|| String::from("it has no UUID, which its port of vnet's is had by"))?;
        let a = crate::vnet::attach(&uuid, network, None, None, false)?;
        let shapes = guest.nic_shapes();
        let [(mac, mtu)] = shapes.as_slice() else { return Err(String::from("its NIC is not its vm.toml's")) };
        nics.try_reserve_exact(1).map_err(|_| oom())?;
        nics.push(migwire::Nic { mac: *mac, mtu: *mtu, port: a.port,
                                 network: spec.network.clone().unwrap_or_default() });
    }
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for r in guest.memory().ranges() {
        ranges.try_reserve(1).map_err(|_| oom())?;
        ranges.push(r);
    }
    ranges.sort_unstable_by_key(|r| r.0);
    let pages = ranges.iter().map(|r| r.1 / PAGE).sum();
    let (vendor, model, tsc_hz, (n, smt, llc)) = guest.facts();
    let mut cpuid = Vec::new();
    cpuid.try_reserve_exact(migwire::LEAVES_MAX).map_err(|_| oom())?;
    hv::policy::model_answers(hv::place::Shape::from_parts(n, smt, llc), &mut |a| {
        if cpuid.len() < migwire::LEAVES_MAX {
            cpuid.push(migwire::Leaf { leaf: a.leaf, sub: a.sub, regs: a.regs });
        }
    });
    let mut listed = Vec::new();
    listed.try_reserve_exact(files.len()).map_err(|_| oom())?;
    listed.extend(files.iter().map(|(n, _, s)| (n.clone(), *s)));
    let offer = Offer {
        id: random_nonzero()?, name: taken.name.clone(), from: String::from(me), mem_bytes: spec.mem_bytes,
        cpus: spec.cpus, vendor: vendor.code(), model, tsc_hz, smt, llc, config: text, cpuid, disks, nics,
        files: listed, pages,
    };
    Ok(Plan { folder, offer, written, files, ranges })
}

/// Messages to D, a window of them unanswered at most: each answered by an
/// Ack once D has written it.
struct Sender<'a> {
    mb: &'a Mailbox,
    d: Addr,
    id: u64,
    host: &'a str,
    state: &'a State,
    /// The last sequence number sent, and what is out unanswered.
    seq: u64,
    out: VecDeque<(u64, usize)>,
    bytes: usize,
}

impl Sender<'_> {
    /// The message `make` makes of the next sequence number, sent -- once
    /// the window has room for it.
    fn send(&mut self, make: impl FnOnce(u64) -> Msg) -> Result<(), String> {
        let seq = self.seq + 1;
        let bytes = make(seq).encode().ok_or_else(|| String::from("a message past its bound, or no memory for it"))?;
        while !self.out.is_empty()
            && (self.bytes + bytes.len() > migwire::WINDOW_BYTES || self.out.len() >= migwire::WINDOW_MESSAGES)
        {
            self.answer()?;
        }
        self.out.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        self.mb.send(self.d, migwire::TAG, 0, &bytes).map_err(|e| alloc::format!("not sent: {}", e.name()))?;
        self.seq = seq;
        self.out.push_back((seq, bytes.len()));
        self.bytes += bytes.len();
        Ok(())
    }

    /// D's next Ack, and what it answers let go of.
    fn answer(&mut self) -> Result<(), String> {
        let until = now_ms() + QUIET_MS;
        loop {
            if self.state.unloading.load(Ordering::Acquire) {
                return Err(String::from("the module is being unloaded"));
            }
            match hear(self.mb, Some(self.d), self.id, TURN_MS) {
                Heard::Msg(Msg::Ack { seq, .. }, _) => {
                    while self.out.front().is_some_and(|&(s, _)| s <= seq) {
                        if let Some((_, n)) = self.out.pop_front() {
                            self.bytes -= n;
                        }
                    }
                    return Ok(());
                }
                Heard::Msg(Msg::Aborted { why, .. }, _) => return Err(alloc::format!("{} gave it up: {}", self.host, why)),
                Heard::Bounced(why) => return Err(alloc::format!("a message to {} did not get there: {}", self.host, why)),
                _ => {
                    if now_ms() > until {
                        return Err(alloc::format!("{} answered nothing in {} s", self.host, QUIET_MS / 1000));
                    }
                }
            }
        }
    }

    /// Everything out answered.
    fn drain(&mut self) -> Result<(), String> {
        while !self.out.is_empty() {
            self.answer()?;
        }
        Ok(())
    }
}

/// What S sent of its guest's memory: its pages, those of them zeros.
#[derive(Default)]
struct Sent {
    pages: u64,
    zeros: u64,
}

impl Sent {
    /// The bytes it took on the way: a page of data its 4 KiB, one of zeros
    /// its number.
    fn bytes(&self) -> u64 {
        (self.pages - self.zeros) * PAGE + self.pages * 8
    }
}

/// `pages`, the addresses of pages, as runs of them: each a first page's
/// address and the bytes from it, lowest first.
fn runs_of(pages: &mut [u64]) -> Result<Vec<(u64, u64)>, String> {
    pages.sort_unstable();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &gpa in pages.iter() {
        match runs.last_mut() {
            Some((base, size)) if *base + *size == gpa => *size += PAGE,
            _ => {
                runs.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                runs.push((gpa, PAGE));
            }
        }
    }
    Ok(runs)
}

/* A migration's records (docs/tracing.md, group `migrate`): each asks
 * whether its event is on first, so that off it is a load and a branch. */

fn trace_round(id: u64, round: u32, sent: &Sent, ms: u64, marked: u64, throttle: u32, last: bool) {
    if trace::on::<trace::MigRound>() {
        let u = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        trace::emit(&trace::MigRound {
            id, round, pages: u(sent.pages), zeros: u(sent.zeros), ms: u(ms), marked: u(marked),
            throttle: u8::try_from(throttle).unwrap_or(u8::MAX), last: u8::from(last), _pad: [0; 2],
        });
    }
}

fn trace_take(id: u64, generation: u64, ms: u64, side: trace::MigSide, took: Take, disk: usize) {
    if trace::on::<trace::MigTake>() {
        let took = match took {
            Take::Took => trace::MigTook::Took,
            Take::Refused => trace::MigTook::Refused,
            Take::Unknown => trace::MigTook::Unknown,
        };
        trace::emit(&trace::MigTake {
            id, generation, ms: u32::try_from(ms).unwrap_or(u32::MAX), side: side as u8, took: took as u8,
            disk: u8::try_from(disk).unwrap_or(u8::MAX), _pad: 0,
        });
    }
}

fn trace_end(id: u64, side: trace::MigSide, how: trace::MigHow, paused_ms: u64, rounds: u32, bytes: u64) {
    if trace::on::<trace::MigEnd>() {
        trace::emit(&trace::MigEnd {
            id, paused_ms: u32::try_from(paused_ms).unwrap_or(u32::MAX), rounds,
            mib: u32::try_from(bytes >> 20).unwrap_or(u32::MAX), side: side as u8, how: how as u8, _pad: [0; 2],
        });
    }
}

fn trace_recv(id: u64, seq: u64, bytes: usize, ns: u64, kind: trace::MigKind) {
    if trace::on::<trace::MigRecv>() {
        trace::emit(&trace::MigRecv {
            id, seq: seq as u32, bytes: u32::try_from(bytes).unwrap_or(u32::MAX),
            ns: u32::try_from(ns).unwrap_or(u32::MAX), kind: kind as u8, _pad: [0; 3],
        });
    }
}

/// Pages of the guest's memory, round `round`, a message's worth at a time:
/// each page's number, and its bytes where it is not zeros -- `runs`, each
/// the address of its first page and the bytes from there, read a piece at
/// a time. A page of zeros after the first round is one the other host
/// writes zeros over.
fn send_pages(tx: &mut Sender<'_>, guest: &LinuxGuest, runs: &[(u64, u64)], round: u32, work: &mut dyn Work,
              progress: &mut Progress) -> Result<Sent, String> {
    let oom = || String::from("out of memory");
    let page = PAGE as usize;
    let mut buf = Vec::new();
    buf.try_reserve_exact(READ_PIECE).map_err(|_| oom())?;
    buf.resize(READ_PIECE, 0);
    let mut pages: Vec<Page> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    pages.try_reserve_exact(migwire::PAGES_MAX).map_err(|_| oom())?;
    data.try_reserve_exact(migwire::PAGES_DATA_MAX * page).map_err(|_| oom())?;
    let mut sent = Sent::default();
    let flush = |tx: &mut Sender<'_>, pages: &mut Vec<Page>, data: &mut Vec<u8>| -> Result<(), String> {
        if pages.is_empty() {
            return Ok(());
        }
        let id = tx.id;
        let msg_pages = core::mem::take(pages);
        let msg_data = core::mem::take(data);
        let r = tx.send(|seq| Msg::Pages { id, seq, round, pages: msg_pages, data: msg_data });
        pages.try_reserve_exact(migwire::PAGES_MAX).map_err(|_| oom())?;
        data.try_reserve_exact(migwire::PAGES_DATA_MAX * page).map_err(|_| oom())?;
        r
    };
    for &(base, size) in runs {
        let mut gpa = base;
        let end = base + size;
        while gpa < end {
            if work.cancelled() {
                return Err(cancelled());
            }
            let n = ((end - gpa).min(READ_PIECE as u64)) as usize;
            guest.memory().read(gpa, &mut buf[..n]).map_err(|e| alloc::format!("its memory at {:#x}: {:?}", gpa, e))?;
            for (k, bytes) in buf[..n].chunks(page).enumerate() {
                let number = (gpa / PAGE) + k as u64;
                let zero = bytes.iter().all(|&b| b == 0);
                pages.push(Page { number, zero });
                sent.pages += 1;
                if zero {
                    sent.zeros += 1;
                } else {
                    data.extend_from_slice(bytes);
                }
                if pages.len() == migwire::PAGES_MAX || data.len() == migwire::PAGES_DATA_MAX * page {
                    let n = pages.len() as u64;
                    flush(tx, &mut pages, &mut data)?;
                    progress.add(n, work);
                }
            }
            gpa += n as u64;
        }
    }
    let n = pages.len() as u64;
    flush(tx, &mut pages, &mut data)?;
    progress.add(n, work);
    Ok(sent)
}

/// The folder's files, each in pieces: no more of one than it was offered
/// as -- a console's log grown since is cut there, and its tail is this
/// host's.
fn send_files(tx: &mut Sender<'_>, files: &[(String, String, u64)]) -> Result<u64, String> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(migwire::PIECE_MAX).map_err(|_| String::from("out of memory"))?;
    buf.resize(migwire::PIECE_MAX, 0);
    let mut total = 0;
    for (name, path, size) in files {
        let file = kcore::fs::File::open(path, false).map_err(|e| alloc::format!("{}: {}", path, e))?;
        let mut offset = 0u64;
        while offset < *size {
            let want = ((*size - offset).min(migwire::PIECE_MAX as u64)) as usize;
            let n = file.read_at(offset, &mut buf[..want]).map_err(|e| alloc::format!("{}: {}", path, e))?;
            if n == 0 {
                break;
            }
            let mut data = Vec::new();
            data.try_reserve_exact(n).map_err(|_| String::from("out of memory"))?;
            data.extend_from_slice(&buf[..n]);
            let (id, name) = (tx.id, name.clone());
            tx.send(|seq| Msg::File { id, seq, name, offset, data })?;
            offset += n as u64;
        }
        total += offset;
    }
    Ok(total)
}

/// The guest's state, in pieces.
fn send_state(tx: &mut Sender<'_>, bytes: &[u8]) -> Result<(), String> {
    let total = u32::try_from(bytes.len()).map_err(|_| String::from("a state past its bound"))?;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let n = (bytes.len() - offset).min(migwire::PIECE_MAX);
        let mut data = Vec::new();
        data.try_reserve_exact(n).map_err(|_| String::from("out of memory"))?;
        data.extend_from_slice(&bytes[offset..offset + n]);
        let id = tx.id;
        let at = offset as u32;
        tx.send(|seq| Msg::State { id, seq, offset: at, total, data })?;
        offset += n;
    }
    Ok(())
}

/// The paused guest's state, as `vmstate` writes it: taken now, of the guest
/// as it paused.
fn state_bytes(guest: &LinuxGuest) -> Result<Vec<u8>, String> {
    let captured = guest.capture().map_err(|e| alloc::format!("its state not taken: {}", e))?;
    captured.encode().filter(|b| b.len() <= migwire::STATE_MAX)
        .ok_or_else(|| String::from("its state not written: past its bound, or out of memory"))
}

/// The VM `word` names moved to `host` -- `cold`, paused for all of it, or
/// live, its memory copied as it runs -- or left running here and why not.
pub fn migrate(state: &State, word: &str, host: &str, cold: bool, work: &mut dyn Work) -> Result<Moved, String> {
    let me = nbsdrive::this_host()?;
    if host == me {
        return Err(alloc::format!("{} is this host", host));
    }
    if host.is_empty() || host.contains('@') || host.len() > migwire::NAME_MAX {
        return Err(alloc::format!("\"{}\" names no host", host));
    }
    let taken = state.vms.take_for_state(word, Held::Move)?;
    let p = match plan(state, &taken, &me) {
        Ok(p) => p,
        Err(e) => {
            let name = taken.name.clone();
            taken.give_back();
            return Err(alloc::format!("{} not moved -- {}", name, e));
        }
    };
    let name = taken.name.clone();
    let mb = match Mailbox::new(S_LIMITS) {
        Ok(mb) => mb,
        Err(e) => {
            taken.give_back();
            return Err(alloc::format!("{} not moved -- no mailbox: {}", name, e.name()));
        }
    };
    let id = p.offer.id;
    let mem_mib = p.offer.mem_bytes >> 20;
    let _ = writeln!(work, "hv: {} offered to {}: {} MiB, {} cpu{}, {} file{} of its folder", name, host, mem_mib,
                     p.offer.cpus, if p.offer.cpus == 1 { "" } else { "s" }, p.files.len(),
                     if p.files.len() == 1 { "" } else { "s" });

    /* The offer, and D's answer: nothing is paused yet, and a refusal
     * leaves the VM as it was */
    let to = alloc::format!("{}@{}", migwire::MAILBOX, host);
    let d = match offer(&mb, &to, host, &p.offer, state, &*work) {
        Ok(d) => d,
        Err(e) => {
            trace_end(id, trace::MigSide::Old, trace::MigHow::Refused, 0, 0, 0);
            taken.give_back();
            return Err(alloc::format!("{} not moved -- {}", name, e));
        }
    };

    /* From here anything that fails before `Done` leaves it running here
     * -- its dirty log stopped, resumed where it was paused -- and D lets go
     * of what it built */
    let guest = taken.guest.clone();
    let mut tx = Sender { mb: &mb, d, id, host, state, seq: 0, out: VecDeque::new(), bytes: 0 };
    let mut paused_at = None;
    let mut logging = false;
    let mut rounds = 0u32;
    let mut total = Sent::default();
    let mut progress = Progress { done: 0, total: (p.offer.mem_bytes / PAGE).max(1) + 1 };
    let moved = (|| -> Result<(u64, usize, Vec<(String, u64)>, usize), String> {
        if cold {
            guest.pause().map_err(|e| alloc::format!("not paused: {}", e))?;
            paused_at = Some(now_ms());
            guest.settle().map_err(|e| alloc::format!("its disks not drained: {}", e))?;
        } else {
            /* Every page read-only to it from here, and each it writes
             * marked: what a round sends again */
            guest.log_start()?;
            logging = true;
        }
        /* Its state, cold: as it paused -- its TSC as it stood; a state
         * taken after the pages would have moved it on by the seconds
         * they took, and the guest's clock with it */
        let cold_state = if cold { Some(state_bytes(&guest)?) } else { None };
        /* Its NIC's firewall's flows, at the same moment: what lets the
         * answers to its connections in on the other host */
        let mut flows = if cold { taken.port.map(|p| state.vms.port_flows(p)).unwrap_or_default() } else { Vec::new() };

        /* The first round: its folder's files, then every page */
        let at = now_ms();
        let file_bytes = send_files(&mut tx, &p.files)?;
        let first = send_pages(&mut tx, &guest, &p.ranges, 0, work, &mut progress)?;
        let mut took = now_ms() - at;
        let mut marked = guest.log_marked();
        trace_round(id, 0, &first, took, marked, 0, cold);
        let _ = writeln!(work, "hv: {} round 0: {} MiB of its memory ({} pages of zeros) and {} KiB of files in {} ms{}",
                         name, (first.pages - first.zeros) * PAGE >> 20, first.zeros, file_bytes >> 10, took,
                         if cold { "" } else { ", the pages it wrote meanwhile marked" });
        let mut rate = (first.bytes() / took.max(1)).max(1);
        total.pages += first.pages;
        total.zeros += first.zeros;

        /* Then, live, the pages it wrote since, round after round, until
         * what is left fits the pause at the rate the last round went --
         * slowed past SLOW_AFTER rounds, given up past ROUNDS_MAX */
        let mut round = 0u32;
        while !cold {
            if work.cancelled() {
                return Err(cancelled());
            }
            let left_ms = marked * (PAGE + 8) / rate;
            if left_ms <= PAUSE_TARGET_MS {
                break;
            }
            round += 1;
            if round > ROUNDS_MAX {
                return Err(alloc::format!("it writes its memory faster than it goes: {} pages left after {} rounds",
                                          marked, ROUNDS_MAX));
            }
            let throttle = if round > SLOW_AFTER {
                (THROTTLE_FIRST + THROTTLE_STEP * (round - SLOW_AFTER - 1)).min(hv::run::THROTTLE_MAX)
            } else {
                0
            };
            guest.throttle(throttle);
            let mut pages: Vec<u64> = Vec::new();
            let _ = pages.try_reserve(marked as usize + 64);
            let mut lost = false;
            guest.log_round(&mut |gpa| {
                if pages.try_reserve(1).is_ok() {
                    pages.push(gpa);
                } else {
                    lost = true;
                }
            })?;
            if lost {
                return Err(String::from("out of memory for a round's pages"));
            }
            let at = now_ms();
            let sent = send_pages(&mut tx, &guest, &runs_of(&mut pages)?, round, work, &mut progress)?;
            took = now_ms() - at;
            rate = (sent.bytes() / took.max(1)).max(1);
            marked = guest.log_marked();
            trace_round(id, round, &sent, took, marked, throttle, false);
            let _ = writeln!(work, "hv: {} round {}: {} pages ({} of zeros) in {} ms{}; {} written meanwhile", name,
                             round, sent.pages, sent.zeros, took,
                             if throttle != 0 { alloc::format!(", slowed {}%", throttle) } else { String::new() },
                             marked);
            total.pages += sent.pages;
            total.zeros += sent.zeros;
        }

        /* The last: paused, drained, its state taken as it paused, the
         * pages it wrote since the round before and its queues' rings --
         * which its devices write through mappings of their own, unmarked
         * -- then the state */
        let bytes = match cold_state {
            Some(b) => b,
            None => {
                guest.pause().map_err(|e| alloc::format!("not paused: {}", e))?;
                paused_at = Some(now_ms());
                guest.settle().map_err(|e| alloc::format!("its disks not drained: {}", e))?;
                let bytes = state_bytes(&guest)?;
                flows = taken.port.map(|p| state.vms.port_flows(p)).unwrap_or_default();
                let mut pages: Vec<u64> = Vec::new();
                let mut lost = false;
                let mut note = |gpa: u64| {
                    if pages.try_reserve(1).is_ok() {
                        pages.push(gpa);
                    } else {
                        lost = true;
                    }
                };
                guest.log_last(&mut note);
                guest.ring_ranges(&mut |base, len| {
                    let mut gpa = base & !(PAGE - 1);
                    while gpa < base + len {
                        note(gpa);
                        gpa += PAGE;
                    }
                });
                if lost {
                    return Err(String::from("out of memory for the last round's pages"));
                }
                /* A ring's page may be one the log marked too */
                pages.sort_unstable();
                pages.dedup();
                round += 1;
                let at = now_ms();
                let sent = send_pages(&mut tx, &guest, &runs_of(&mut pages)?, round, work, &mut progress)?;
                trace_round(id, round, &sent, now_ms() - at, 0, 0, true);
                total.pages += sent.pages;
                total.zeros += sent.zeros;
                bytes
            }
        };
        rounds = round + 1;
        send_state(&mut tx, &bytes)?;
        /* Those with the least left first: the new port's table lets the
         * oldest go first, as this one would have */
        flows.sort_unstable_by_key(|c| c.left_ns);
        let carried = flows.len().min(migwire::FLOWS_MAX);
        if carried != 0 {
            let mut wire = Vec::new();
            wire.try_reserve_exact(carried).map_err(|_| String::from("out of memory"))?;
            wire.extend(flows[flows.len() - carried..].iter().map(flow_out));
            tx.send(|seq| Msg::Flows { id, seq, flows: wire })?;
        }
        tx.drain()?;
        /* Each written volume's writer: its session's, as the guest stands
         * drained -- the first is what D's commit takes from */
        let mut writers: Vec<(usize, String, u64)> = Vec::new();
        guest.disk_writers(&mut |i, volume, generation| {
            if writers.try_reserve(1).is_ok() {
                writers.push((i, String::from(volume), generation));
            }
        });
        let mut volumes = Vec::new();
        volumes.try_reserve_exact(p.written.len()).map_err(|_| String::from("out of memory"))?;
        for (i, word) in &p.written {
            let (_, v, g) = writers.iter().find(|(k, _, _)| k == i)
                .ok_or_else(|| alloc::format!("its disk {} has no writer's generation", word))?;
            volumes.push((v.clone(), *g));
        }
        Ok((file_bytes, bytes.len(), volumes, carried))
    })();
    /* Given up before `Done`: the VM goes on here */
    let undo = |why: &str| {
        let _ = send(&mb, d, &Msg::Cancel { id, why: clip(why) });
        if logging {
            guest.log_stop();
        }
        if paused_at.is_some() {
            guest.resume();
        }
        trace_end(id, trace::MigSide::Old, trace::MigHow::Cancelled, paused_at.map_or(0, |at| now_ms() - at),
                  rounds, total.pages * PAGE);
    };
    let (file_bytes, state_len, volumes, flows) = match moved {
        Ok(m) => m,
        Err(e) => {
            undo(&e);
            taken.give_back();
            /* A stop said as one, for a job to end Cancelled by */
            if work::is_stop(&e) {
                return Err(e);
            }
            return Err(alloc::format!("{} not moved, and runs here still -- {}", name, e));
        }
    };
    let paused_at = paused_at.unwrap_or_else(now_ms);
    let g0 = volumes[0].1;
    let done_seq = tx.seq + 1;
    if let Err(e) = send(&mb, d, &Msg::Done { id, seq: done_seq, volumes }) {
        undo(&e);
        taken.give_back();
        return Err(alloc::format!("{} not moved, and runs here still -- {}", name, e));
    }
    let data_mib = (total.pages - total.zeros) * PAGE >> 20;
    let flows_said = if flows == 0 { String::new() } else { alloc::format!(", {} flows of its firewall", flows) };
    let _ = writeln!(work, "hv: {} sent in {} round{}: {} MiB of its memory, {} KiB of files, its state {} KiB{} -- \
                           paused {} ms so far", name, rounds, if rounds == 1 { "" } else { "s" }, data_mib,
                     file_bytes >> 10, state_len >> 10, flows_said, now_ms() - paused_at);

    /* The handover: D's word, or the volume taken back */
    let first = p.written[0].clone();
    let handed = handover(state, &taken, &mb, d, id, host, &first, g0);
    let ms = now_ms() - paused_at;
    let how = match handed {
        Handed::There => trace::MigHow::There,
        Handed::NotHere(_) => trace::MigHow::Stopped,
        Handed::Here(_) => trace::MigHow::Here,
    };
    trace_end(id, trace::MigSide::Old, how, ms, rounds, total.pages * PAGE);
    match handed {
        Handed::There => {
            /* Its port's frames carried on to the other host for a while --
             * the other hosts' tables send them here until their keepers'
             * next turn -- from now, its VM ending here still (M3) */
            if let Some((vni, addr)) = taken.port.and_then(|p| state.vms.port_vnet(p)) {
                if !state.vms.moved_away(vni, addr, host) {
                    let _ = writeln!(work, "hv: {}'s frames still sent here not carried on: the tunnel knows no host \
                                           {}", name, host);
                }
            }
            /* The VM's last hold on its guest the list's, so that its end
             * lets go of its memory and its disks -- a file of its folder
             * among them -- before the folder goes */
            drop(guest);
            let gone = taken.end(Stop::Migrated, &state.forwards);
            if let Err(e) = gone {
                let _ = writeln!(work, "hv: {}: {}", name, e);
            }
            /* vnet's table had now: its port elsewhere */
            state.vms.keeper_now();
            match remove_folder(&p.folder) {
                Ok(()) => {
                    let _ = writeln!(work, "hv: {} runs on {} -- paused {} ms; its folder here taken away", name, host, ms);
                }
                Err(e) => {
                    let _ = writeln!(work, "hv: {} runs on {} -- paused {} ms; its folder here left: {}", name, host, ms, e);
                }
            }
            work.progress(progress.total, progress.total);
            Ok(Moved { rounds, paused_ms: ms, sent_bytes: total.pages * PAGE, flows })
        }
        Handed::NotHere(why) => {
            drop(guest);
            let gone = taken.end(Stop::Migrated, &state.forwards);
            if let Err(e) = gone {
                let _ = writeln!(work, "hv: {}: {}", name, e);
            }
            Err(alloc::format!("{} stopped here -- {}: its folder is kept, its volume's writer is {}'s if its take \
                                landed -- hv start {} force where it is to run", name, why, host, name))
        }
        Handed::Here(why) => {
            if !cold {
                guest.log_stop();
            }
            guest.resume();
            drop(guest);
            taken.give_back();
            Err(alloc::format!("{} not moved, and runs here still, paused {} ms -- {}", name, ms, why))
        }
    }
}

/// The offer sent to `to`, and D's acceptance: the address it answered
/// from, which the rest goes to.
fn offer(mb: &Mailbox, to: &str, host: &str, o: &Offer, state: &State, work: &dyn Work) -> Result<Addr, String> {
    let bytes = Msg::Offer(o.clone()).encode().ok_or_else(|| String::from("its offer is past a message's bound"))?;
    mb.send_to(to, migwire::TAG, 0, &bytes).map_err(|e| alloc::format!("{} not reached: {}", to, e.name()))?;
    let until = now_ms() + OFFER_MS;
    loop {
        if state.unloading.load(Ordering::Acquire) || work.cancelled() {
            /* D lets go of what it built at the cancel -- or, it lost,
             * when it hears nothing more (QUIET_MS) */
            if let Some(bytes) = (Msg::Cancel { id: o.id, why: clip("cancelled") }).encode() {
                let _ = mb.send_to(to, migwire::TAG, 0, &bytes);
            }
            return Err(cancelled());
        }
        match hear(mb, None, o.id, TURN_MS) {
            Heard::Msg(Msg::Accept { .. }, from) => return Ok(from),
            Heard::Msg(Msg::Refuse { why, .. }, _) => return Err(alloc::format!("{} refused it: {}", host, why)),
            Heard::Bounced(why) => {
                return Err(alloc::format!("{} not reached -- {}: is hv loaded there, and ipcnet between?", to, why));
            }
            _ => {
                if now_ms() > until {
                    return Err(alloc::format!("{} did not answer its offer in {} s", host, OFFER_MS / 1000));
                }
            }
        }
    }
}

/// How the handover ended, for S.
enum Handed {
    /// D runs it.
    There,
    /// Not here: D took its first volume, or may have -- it was not to be
    /// had back -- and why.
    NotHere(String),
    /// Here: resumed, D having given it up or its volume taken back -- and
    /// why.
    Here(String),
}

/// S's side of the handover, from `Done` sent: D's word, or past its wait
/// the first volume taken back from its generation `g0` -- `first`, its
/// disk's index and word -- and the guest's first disk given the session
/// that took it.
#[allow(clippy::too_many_arguments)]
fn handover(state: &State, taken: &StateHold<'_>, mb: &Mailbox, d: Addr, id: u64, host: &str,
            first: &(usize, String), g0: u64) -> Handed {
    let mut s = Source::new();
    let done_at = now_ms();
    let mut why_d = String::new();
    let mut taking_since = 0;
    let mut next_take = 0;
    let token = match random_nonzero() {
        Ok(t) => t,
        Err(e) => return Handed::NotHere(alloc::format!("no token to take its volume back with: {}", e)),
    };
    let Some(runner) = taken.runner() else {
        return Handed::NotHere(String::from("no CPU to serve its disk on, were its volume taken back"));
    };
    let mut taken_back: Option<QueuedDisk> = None;
    /* Whether D's word is what decided: an end by its `Running` is a VM
     * that runs there, one by a take refused a VM whose volume D took --
     * and which runs there unless D died since */
    let mut by_word = false;
    loop {
        let mut act = match hear(mb, Some(d), id, TURN_MS) {
            Heard::Msg(Msg::Running { .. }, _) => {
                by_word = true;
                s.running()
            }
            Heard::Msg(Msg::Aborted { why, .. }, _) => {
                why_d = why;
                s.aborted()
            }
            _ => SourceDo::Wait,
        };
        if act == SourceDo::Wait {
            let now = now_ms();
            match s {
                Source::Waiting if now >= done_at + WORD_MS || state.unloading.load(Ordering::Acquire) => {
                    act = s.timed_out();
                    taking_since = now;
                }
                Source::TakingBack if now >= next_take => {
                    if state.unloading.load(Ordering::Acquire) || now >= taking_since + TAKE_FOR_MS {
                        return Handed::NotHere(alloc::format!("{} said nothing, and its volume could not be had back \
                                                               in {} s", host, (now - taking_since) / 1000));
                    }
                    let asked = now_ms();
                    let t = match QueuedDisk::open_handover(&first.1, g0, token, &runner, first.0 as u8) {
                        Ok(disk) => {
                            taken_back = Some(disk);
                            Take::Took
                        }
                        Err(NotTaken::Refused) => Take::Refused,
                        Err(NotTaken::Unknown(why)) => {
                            kcore::klog!(0, "hv: migrate: {}'s volume not taken back yet -- {}: asked again", taken.name,
                                         why);
                            Take::Unknown
                        }
                    };
                    trace_take(id, g0, now_ms() - asked, trace::MigSide::Old, t, first.0);
                    act = s.took_back(t);
                    next_take = now_ms() + AGAIN_MS;
                }
                _ => {}
            }
        }
        match act {
            SourceDo::Wait | SourceDo::TakeBack => {}
            SourceDo::End => {
                drop(taken_back.take());
                return if by_word {
                    Handed::There
                } else {
                    Handed::NotHere(alloc::format!("{} said nothing, and took its first volume before it could be had \
                                                    back", host))
                };
            }
            SourceDo::Resume { swap_first } => {
                if swap_first {
                    let Some(disk) = taken_back.take() else {
                        return Handed::NotHere(String::from("its volume taken back, and its disk gone"));
                    };
                    match taken.guest.set_disk_backend(first.0, Box::new(disk)) {
                        /* The old session -- fenced by the take -- let go of
                         * off the guest's lock */
                        Ok(old) => drop(old),
                        Err(new) => {
                            drop(new);
                            return Handed::NotHere(String::from("its volume taken back, but its disk would not take \
                                                                 the new session"));
                        }
                    }
                }
                let why = if why_d.is_empty() {
                    alloc::format!("{} said nothing in {} s, and its volume was taken back", host, WORD_MS / 1000)
                } else {
                    alloc::format!("{} gave it up: {}", host, why_d)
                };
                return Handed::Here(why);
            }
        }
    }
}

/// The folder of a VM that runs elsewhere now, taken away: its files, its
/// vm.toml last, then itself.
fn remove_folder(folder: &Folder) -> Result<(), String> {
    let entries = kcore::fs::list(&folder.path).map_err(|e| alloc::format!("{}: {}", folder.path, e))?;
    for e in &entries {
        let Ok(name) = core::str::from_utf8(&e.name) else { continue };
        if e.dir || name == store::CONFIG {
            continue;
        }
        let path = store::join(&folder.path, name).ok_or_else(|| String::from("out of memory"))?;
        kcore::fs::remove(&path).map_err(|e| alloc::format!("{}: {}", path, e))?;
    }
    let config = store::join(&folder.path, store::CONFIG).ok_or_else(|| String::from("out of memory"))?;
    kcore::fs::remove(&config).map_err(|e| alloc::format!("{}: {}", config, e))?;
    kcore::fs::remove(&folder.path).map_err(|e| alloc::format!("{}: {}", folder.path, e))
}

/* ---------------------------------------------------------------------- */
/* The dirty log, checked where it runs                                   */
/* ---------------------------------------------------------------------- */

pub const LOGCHECK_USAGE: &str = "hv logcheck <vm> [rounds=N]";
/// The rounds `hv logcheck` copies as the guest runs, by default and at most.
const LOGCHECK_ROUNDS: u32 = 3;
/// How long the guest runs between them.
const LOGCHECK_GAP_MS: u64 = 300;

/// `hv logcheck <vm> [rounds=N]`: the dirty log held to the guest on this
/// host's own CPUs -- what a migration's rounds rely on, checked on the
/// silicon it runs on, without another host: every page of the running
/// guest copied, the pages it writes meanwhile copied again round after
/// round, then the guest paused and the last round's pages and its rings
/// copied, and the copy held to its memory page by page. A page that
/// differs is a write the log let by: a translation still writable after a
/// round, a write the host made unmarked. The guest runs on as it was; the
/// copy costs as much memory as it has.
pub fn logcheck_command(state: &State, args: &str, out: &mut dyn Write) {
    let mut w = args.split_ascii_whitespace();
    let Some(word) = w.next() else {
        let _ = writeln!(out, "{}", LOGCHECK_USAGE);
        return;
    };
    let rounds = match (w.next(), w.next()) {
        (None, _) => LOGCHECK_ROUNDS,
        (Some(r), None) => match r.strip_prefix("rounds=").and_then(|n| n.parse::<u32>().ok()) {
            Some(n) if (1..=ROUNDS_MAX).contains(&n) => n,
            _ => {
                let _ = writeln!(out, "hv: rounds= must be 1..{}", ROUNDS_MAX);
                return;
            }
        },
        _ => {
            let _ = writeln!(out, "{}", LOGCHECK_USAGE);
            return;
        }
    };
    let taken = match state.vms.take_for_state(word, Held::Check) {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(out, "hv: {}", e);
            return;
        }
    };
    let guest = taken.guest.clone();
    let checked = logcheck(state, &guest, rounds, out);
    drop(guest);
    taken.give_back();
    match checked {
        Ok(said) => {
            let _ = writeln!(out, "hv: {}", said);
        }
        Err(e) => {
            let _ = writeln!(out, "hv: {} -- {}", word, e);
        }
    }
}

/// A copy of a guest's RAM, run by run, in frames of its own -- as the
/// guest's memory is: no heap's (the heap holds a class of blocks to a window
/// of its own, an eighth of 70% of RAM, and a copy as big as a guest is past
/// it), mapped nowhere, a page reached by copying in and out.
struct Copy {
    runs: Vec<(u64, u64, kcore::frame::Run)>,
}

impl Copy {
    fn new(ranges: &[(u64, u64)]) -> Result<Copy, String> {
        let oom = || String::from("not as many free pages as it has, for its copy");
        let mut runs = Vec::new();
        runs.try_reserve_exact(ranges.len()).map_err(|_| oom())?;
        for &(base, size) in ranges {
            let pages = usize::try_from(size / PAGE).map_err(|_| oom())?;
            runs.push((base, size, kcore::frame::Run::new(pages).ok_or_else(oom)?));
        }
        Ok(Copy { runs })
    }

    /// The copy's frame of the page at `gpa`.
    fn frame(&self, gpa: u64) -> Option<kcore::frame::FramePage<'_>> {
        let (base, _, run) = self.runs.iter().find(|(b, s, _)| gpa >= *b && gpa < *b + *s)?;
        run.page(usize::try_from((gpa - base) / PAGE).ok()?)
    }

    /// Page `gpa` copied from the guest, through `buf`, a page.
    fn take(&self, guest: &LinuxGuest, gpa: u64, buf: &mut [u8]) -> Result<(), String> {
        guest.memory().read(gpa, buf).map_err(|e| alloc::format!("its memory at {:#x}: {:?}", gpa, e))?;
        let frame = self.frame(gpa).ok_or_else(|| alloc::format!("page {:#x} is not its RAM", gpa))?;
        if frame.write(0, buf) { Ok(()) } else { Err(alloc::format!("its copy at {:#x} not written", gpa)) }
    }

    /// Whether the copy of page `gpa` is `page`, through `buf`, a page.
    fn same(&self, gpa: u64, page: &[u8], buf: &mut [u8]) -> bool {
        self.frame(gpa).is_some_and(|f| f.read(0, buf)) && buf == page
    }
}

/// The check itself: its line, or why it could not be made.
fn logcheck(state: &State, guest: &LinuxGuest, rounds: u32, out: &mut dyn Write) -> Result<String, String> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for r in guest.memory().ranges() {
        ranges.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        ranges.push(r);
    }
    ranges.sort_unstable_by_key(|r| r.0);
    let copy = Copy::new(&ranges)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(2 * PAGE as usize).map_err(|_| String::from("out of memory"))?;
    buf.resize(2 * PAGE as usize, 0);
    let (buf, page) = buf.split_at_mut(PAGE as usize);
    let started = now_ms();
    guest.log_start()?;
    let mut paused = false;
    let checked = (|| -> Result<String, String> {
        /* Round 0: all of it, a page at a time, as the guest runs */
        for &(base, size) in &ranges {
            let mut gpa = base;
            while gpa < base + size {
                copy.take(guest, gpa, buf)?;
                gpa += PAGE;
            }
        }
        let mut again = 0u64;
        let note = |pages: &mut Vec<u64>, lost: &mut bool, gpa: u64| {
            if pages.try_reserve(1).is_ok() {
                pages.push(gpa);
            } else {
                *lost = true;
            }
        };
        for round in 1..=rounds {
            if state.unloading.load(Ordering::Acquire) {
                return Err(String::from("the module is being unloaded"));
            }
            kcore::task::sleep_ms(LOGCHECK_GAP_MS);
            let (mut pages, mut lost) = (Vec::new(), false);
            guest.log_round(&mut |gpa| note(&mut pages, &mut lost, gpa))?;
            if lost {
                return Err(String::from("out of memory for a round's pages"));
            }
            for &gpa in &pages {
                copy.take(guest, gpa, buf)?;
            }
            again += pages.len() as u64;
            let _ = writeln!(out, "hv: round {}: {} pages written since the round before, copied again", round,
                             pages.len());
        }
        /* The last: paused and drained, the pages written since and the
           rings, as a migration's last round sends them */
        guest.pause().map_err(|e| alloc::format!("not paused: {}", e))?;
        paused = true;
        let paused_at = now_ms();
        guest.settle().map_err(|e| alloc::format!("its disks not drained: {}", e))?;
        let (mut pages, mut lost) = (Vec::new(), false);
        guest.log_last(&mut |gpa| note(&mut pages, &mut lost, gpa));
        guest.ring_ranges(&mut |base, len| {
            let mut gpa = base & !(PAGE - 1);
            while gpa < base + len {
                note(&mut pages, &mut lost, gpa);
                gpa += PAGE;
            }
        });
        if lost {
            return Err(String::from("out of memory for the last round's pages"));
        }
        pages.sort_unstable();
        pages.dedup();
        for &gpa in &pages {
            copy.take(guest, gpa, buf)?;
        }
        /* Then the copy held to the memory, page by page */
        let mut checked = 0u64;
        let mut differ = 0u64;
        let mut first: Option<u64> = None;
        for &(base, size) in &ranges {
            let mut gpa = base;
            while gpa < base + size {
                guest.memory().read(gpa, page).map_err(|e| alloc::format!("its memory at {:#x}: {:?}", gpa, e))?;
                if !copy.same(gpa, page, buf) {
                    differ += 1;
                    first.get_or_insert(gpa);
                }
                checked += 1;
                gpa += PAGE;
            }
        }
        let paused_ms = now_ms() - paused_at;
        if differ != 0 {
            return Err(alloc::format!("the dirty log let writes by: {} of its {} pages differ from their copy, the \
                                       first at {:#x}", differ, checked, first.unwrap_or(0)));
        }
        Ok(alloc::format!("its dirty log held: {} pages copied as it ran, {} again in {} round{} and {} at the last, \
                           paused {} ms -- every one the same as its memory, in {} ms", checked, again, rounds,
                          if rounds == 1 { "" } else { "s" }, pages.len(), paused_ms, now_ms() - started))
    })();
    guest.log_stop();
    if paused {
        guest.resume();
    }
    checked
}

/* ---------------------------------------------------------------------- */
/* The new host's side                                                    */
/* ---------------------------------------------------------------------- */

/// The task that answers offers at `hv.migrate`: started with the module,
/// stopped before its VMs are.
pub fn start(state: Arc<State>) -> Option<TaskHandle> {
    kcore::task::spawn_with("hv/migrate", state, serve)
}

fn serve(state: Arc<State>) {
    let mb = match Mailbox::new(D_LIMITS) {
        Ok(mb) => mb,
        Err(e) => {
            kcore::klog!(0, "hv: migrate: no mailbox ({}): no VM can move here", e.name());
            return;
        }
    };
    if let Err(e) = mb.register(migwire::MAILBOX, Scope::Cluster) {
        kcore::klog!(0, "hv: migrate: {} not named ({}): no VM can move here", migwire::MAILBOX, e.name());
        return;
    }
    let mut ran: VecDeque<u64> = VecDeque::new();
    if ran.try_reserve_exact(REMEMBERED).is_err() {
        kcore::klog!(0, "hv: migrate: out of memory: no VM can move here");
        return;
    }
    while !kcore::task::stopping() {
        let Some(m) = mb.recv(Duration::from_millis(TURN_MS)) else { continue };
        if m.is_system() || m.tag != migwire::TAG {
            continue;
        }
        match Msg::decode(&m.data) {
            Some(Msg::Offer(offer)) => {
                if let Some(id) = incoming(&state, &mb, m.from, offer) {
                    if ran.len() == REMEMBERED {
                        ran.pop_front();
                    }
                    /* Into the room taken above */
                    ran.push_back(id);
                }
            }
            /* A `Done` again, of a VM that runs here: its word again, which
             * S may not have had */
            Some(Msg::Done { id, .. }) if ran.contains(&id) => {
                let _ = send(&mb, m.from, &Msg::Running { id });
            }
            /* Anything else is of a migration that is over, or never was:
             * never answered, least of all with `Aborted`, which only a D
             * that has not taken the volume may say */
            _ => {}
        }
    }
}

/// Where the CPU a guest was shown on the other host -- `theirs`, its
/// answers -- differs from this host's, for a line.
fn model_difference(shape: hv::place::Shape, theirs: &[migwire::Leaf], there: &str, here: &str) -> String {
    let theirs: Vec<hv::policy::Answer> =
        theirs.iter().map(|l| hv::policy::Answer { leaf: l.leaf, sub: l.sub, regs: l.regs }).collect();
    let said = |a: &hv::policy::Answer| if a.sub == hv::policy::MSR_ANSWER {
        alloc::format!("MSR {:#x} {:#x}", a.leaf, u64::from(a.regs[0]) | u64::from(a.regs[1]) << 32)
    } else {
        alloc::format!("leaf {:#x}.{} {:08x} {:08x} {:08x} {:08x}", a.leaf, a.sub, a.regs[0], a.regs[1], a.regs[2],
                       a.regs[3])
    };
    match hv::policy::model_differs(shape, &theirs) {
        Some((Some(t), Some(m))) => alloc::format!("{} {}, {} {}", there, said(&t), here, said(&m)),
        Some((None, Some(m))) => alloc::format!("{} no answer where {} {}", there, here, said(&m)),
        Some((Some(t), None)) => alloc::format!("{} {} past what {} answers", there, said(&t), here),
        _ => alloc::format!("{} and {} answer alike, and their digests differ: one's software is another's", there,
                            here),
    }
}

/// What two CPUs of this host are asked for the first leaf they answer a
/// guest otherwise: the one's answers, taken on it, and what the other
/// makes of them on it -- each by a task pinned there.
struct Asked {
    shape: hv::place::Shape,
    answers: Vec<migwire::Leaf>,
    there: String,
    said: String,
}

/// Its CPU's answers, into `Asked` (`answers_on`).
fn answers_on(asked: Arc<kcore::sync::Mutex<Asked>>) {
    let mut a = asked.lock();
    let shape = a.shape;
    if a.answers.try_reserve_exact(migwire::LEAVES_MAX).is_err() {
        return;
    }
    let answers = &mut a.answers;
    hv::policy::model_answers(shape, &mut |x| {
        if answers.len() < migwire::LEAVES_MAX {
            answers.push(migwire::Leaf { leaf: x.leaf, sub: x.sub, regs: x.regs });
        }
    });
}

/// Where its CPU answers otherwise than they say (`differs_on`).
fn differs_on(asked: Arc<kcore::sync::Mutex<Asked>>) {
    let mut a = asked.lock();
    let said = model_difference(a.shape, &a.answers, &a.there, "here");
    a.said = said;
}

/// The first leaf CPUs `a` and `b` answer a guest of one CPU otherwise.
fn which_leaf(a: u32, b: u32) -> Option<String> {
    let asked = Asked {
        shape: hv::place::Shape::from_parts(1, false, false), answers: Vec::new(),
        there: alloc::format!("cpu {}", a), said: String::new(),
    };
    let asked = Arc::new(kcore::sync::Mutex::new(asked)?);
    drop(kcore::task::spawn_on_with("hv/model", 1u64 << a, asked.clone(), answers_on)?);
    drop(kcore::task::spawn_on_with("hv/model", 1u64 << b, asked.clone(), differs_on)?);
    let said = core::mem::take(&mut asked.lock().said);
    Some(alloc::format!("{} on cpu {}", said, b))
}

/// What `hv info` says of the CPU a guest moving here is shown: its
/// digest, for a guest of one CPU, as every CPU of this host answers -- one
/// a task pinned there -- and whether they are alike: a host whose cores
/// answer otherwise refuses a VM's state taken on one of its other kind.
pub fn model_report(out: &mut dyn Write) {
    let mask = kcore::cpu::online_mask();
    let Some(seen) = kcore::sync::Mutex::new(Vec::<(u32, [u8; 32])>::new()).map(Arc::new) else { return };
    if seen.lock().try_reserve_exact(64).is_err() {
        return;
    }
    let mut tasks = Vec::new();
    for cpu in 0..64u32 {
        if mask & (1u64 << cpu) != 0 {
            if let Some(t) = kcore::task::spawn_on_with("hv/model", 1u64 << cpu, (cpu, seen.clone()), model_on) {
                if tasks.try_reserve(1).is_ok() {
                    tasks.push(t);
                }
            }
        }
    }
    drop(tasks);
    let seen = seen.lock();
    let Some(&(_, first)) = seen.first() else { return };
    let hex = |d: &[u8; 32]| d[..8].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{:02x}", b);
        s
    });
    match seen.iter().find(|(_, d)| *d != first) {
        None => {
            let _ = writeln!(out, "  cpu model          {} -- what a guest moving here is held to, alike on the {} \
                                   CPUs", hex(&first), seen.len());
        }
        Some(&(cpu, d)) => {
            let first_cpu = seen[0].0;
            drop(seen);
            let _ = writeln!(out, "  cpu model          {} on cpu {}, {} on cpu {}: its CPUs answer a guest otherwise -- \
                                   {}", hex(&first), first_cpu, hex(&d), cpu,
                             which_leaf(first_cpu, cpu).unwrap_or_else(|| String::from("which leaf, not found")));
        }
    }
}

fn model_on(ctx: (u32, Arc<kcore::sync::Mutex<Vec<(u32, [u8; 32])>>>)) {
    let (cpu, seen) = ctx;
    let d = hv::policy::model_digest(hv::place::Shape::from_parts(1, false, false));
    let mut seen = seen.lock();
    /* Into the room taken before the tasks */
    if seen.len() < seen.capacity() {
        seen.push((cpu, d));
    }
}

/// A VM offered by `s`, taken in: built, its files, pages and state taken,
/// handed over and run -- or refused or given up. Its migration's id where
/// it runs here.
fn incoming(state: &State, mb: &Mailbox, s: Addr, offer: Offer) -> Option<u64> {
    let (id, name) = (offer.id, offer.name.clone());
    let from = offer.from.clone();
    let c = match Incoming::new(state, mb, s, offer) {
        Ok(c) => c,
        Err(why) => {
            trace_end(id, trace::MigSide::New, trace::MigHow::Refused, 0, 0, 0);
            kcore::klog!(0, "hv: migrate: vm {} from {} refused -- {}", name, from, why);
            let _ = send(mb, s, &Msg::Refuse { id, why: clip(&why) });
            return None;
        }
    };
    if let Err(e) = send(mb, s, &Msg::Accept { id }) {
        kcore::klog!(0, "hv: migrate: vm {} from {}: its acceptance {}", name, from, e);
        c.let_go(false);
        return None;
    }
    kcore::klog!(0, "hv: migrate: vm {} coming from {}", name, from);
    c.run()
}

/// A VM coming from `s`, until it runs here or is let go of.
struct Incoming<'a> {
    state: &'a State,
    mb: &'a Mailbox,
    s: Addr,
    offer: Offer,
    coming: Coming<'a>,
    built: Built,
    folder: Folder,
    logged: bool,
    /// Each offered file's bytes written so far.
    written: Vec<u64>,
    /// The last message's sequence number taken.
    seq: u64,
    /// The state's bytes as they come, and whether it is in the guest.
    state_bytes: Vec<u8>,
    restored: bool,
    /// The pages of the first round taken: every page of its memory, once.
    first_round: u64,
    /// A page of zeros, for a page that is zeros again in a later round.
    zeros: Vec<u8>,
    /// Its written volumes, and its disks read alone: each disk's index and
    /// word, or path.
    written_disks: Vec<(usize, String)>,
    /// Its NIC's firewall's flows as it paused there, for its port here.
    flows: Vec<crate::net::Carried>,
    read_disks: Vec<(usize, String)>,
    dest: Dest,
}

impl<'a> Incoming<'a> {
    /// What D checks before it takes anything, and what it makes: its name
    /// held, its CPUs placed, its guest built over stand-ins, its folder
    /// made with the offered files in it, empty.
    fn new(state: &'a State, mb: &'a Mailbox, s: Addr, offer: Offer) -> Result<Incoming<'a>, String> {
        let oom = || String::from("out of memory");
        if let Some(why) = store::name_refused(&offer.name) {
            return Err(why);
        }
        let path = store::join(&state.vms_root, &offer.name).ok_or_else(oom)?;
        if kcore::fs::list(&path).is_ok() || kcore::fs::size(&path).is_ok() {
            return Err(alloc::format!("{} is here already", path));
        }
        if offer.vendor != LinuxGuest::vendor_of(&state.machine).code() {
            return Err(String::from("its CPU's extension is not this host's: between AMD-V and VT-x no VM moves"));
        }
        let shape = hv::place::Shape::from_parts(offer.cpus, offer.smt, offer.llc);
        if hv::policy::model_digest(shape) != offer.model {
            return Err(alloc::format!("its CPU is not this host's: the CPUID it has read would change under it -- {}",
                                      model_difference(shape, &offer.cpuid, "there", "here")));
        }
        let hz = kcore::time::cycle_counter_hz().filter(|&hz| hz != 0)
            .ok_or_else(|| String::from("this host's TSC rate is not known"))?;
        if u128::from(hz.abs_diff(offer.tsc_hz)) * 1_000_000
            > u128::from(hv::run::TSC_TOLERANCE_PPM) * u128::from(offer.tsc_hz)
        {
            return Err(alloc::format!("its TSC ran at {} Hz, this host's at {}: more than {} ppm apart", offer.tsc_hz,
                                      hz, hv::run::TSC_TOLERANCE_PPM));
        }
        let folder = Folder { name: offer.name.clone(), path };
        let config = crate::config::parse(&offer.config).map_err(|e| alloc::format!("its vm.toml: {}", e))?;
        let spec = crate::guest::spec(store::options(&folder, &config)?)?;
        if spec.mem_bytes != offer.mem_bytes || spec.cpus != offer.cpus {
            return Err(String::from("its vm.toml says other memory or CPUs than it has"));
        }
        if !spec.forwards.is_empty() || spec.public.is_some() || spec.port.is_some() {
            return Err(String::from("forward=, public= and port= are a host's own, and do not move"));
        }
        match (spec.net, offer.nics.as_slice()) {
            (false, []) => {}
            (true, [nic]) => {
                let cluster = spec.network.as_deref().and_then(crate::config::cluster_network)
                    .ok_or_else(|| String::from("its NIC is on a host's network"))?;
                if spec.network.as_deref() != Some(nic.network.as_str()) || spec.uuid.is_none() {
                    return Err(String::from("its NIC is not its vm.toml's"));
                }
                if !crate::vnet::networks().iter().any(|n| n.name == cluster) {
                    return Err(alloc::format!("vnet here has no network {} -- or there is no vnet here", cluster));
                }
            }
            _ => return Err(String::from("its NIC is not its vm.toml's")),
        }
        if offer.disks.len() != spec.disks.len() {
            return Err(String::from("its disks are not its vm.toml's"));
        }
        let mut written_disks = Vec::new();
        let mut read_disks = Vec::new();
        let mut shapes = Vec::new();
        shapes.try_reserve_exact(spec.disks.len()).map_err(|_| oom())?;
        for (i, ((path, ro), d)) in spec.disks.iter().zip(&offer.disks).enumerate() {
            if d.read_only != *ro {
                return Err(alloc::format!("its disk {} is read alone on one host and written on the other", path));
            }
            match crate::devices::source(path) {
                crate::devices::Source::Volume(_) => {
                    let list = if *ro { &mut read_disks } else { &mut written_disks };
                    list.try_reserve(1).map_err(|_| oom())?;
                    list.push((i, path.clone()));
                }
                crate::devices::Source::File(p) => {
                    let file = p.strip_prefix(folder.path.as_str()).and_then(|r| r.strip_prefix('/'));
                    if !*ro || !file.is_some_and(|f| offer.files.iter().any(|(n, _)| n == f)) {
                        return Err(alloc::format!("its disk {} is no file of its folder read alone", path));
                    }
                    read_disks.try_reserve(1).map_err(|_| oom())?;
                    read_disks.push((i, String::from(p)));
                }
                _ => return Err(alloc::format!("its disk {} is a device of a host's own", path)),
            }
            let mut id = [0u8; 20];
            if d.serial.len() > id.len() {
                return Err(alloc::format!("its disk {}'s id is longer than a disk's", path));
            }
            id[..d.serial.len()].copy_from_slice(d.serial.as_bytes());
            shapes.push(hv::disk::Shape { bytes: d.bytes, block: d.block, read_only: d.read_only,
                                          discards: d.discards, writes_zeroes: d.writes_zeroes, id });
        }
        if written_disks.is_empty() {
            return Err(String::from("it writes no volume of NBS's: nothing decides which host runs it"));
        }
        let mut written = Vec::new();
        written.try_reserve_exact(offer.files.len()).map_err(|_| oom())?;
        written.resize(offer.files.len(), 0);
        let mut zeros = Vec::new();
        zeros.try_reserve_exact(PAGE as usize).map_err(|_| oom())?;
        zeros.resize(PAGE as usize, 0);

        let logged = config.console_log;
        let mut coming = state.vms.coming(&state.machine, &state.cores, &offer.name, spec, logged)?;
        /* The shape its CPUs were told, whatever this host's placement
         * would make of them: what its CPUID says at its next boot here too */
        coming.spec.shape = shape;
        let nic = offer.nics.first().map(|n| (n.mac, n.mtu));
        let built = coming.build(&state.machine, &shapes, nic)?;
        /* Its folder, and the VMs' folder above it where this host has
         * made no VM yet */
        store::make_dirs(&folder.path).map_err(|e| alloc::format!("{}: {}", folder.path, e))?;
        let c = Incoming {
            state, mb, s, offer, coming, built, folder, logged, written, seq: 0, state_bytes: Vec::new(),
            restored: false, first_round: 0, zeros, written_disks, read_disks, flows: Vec::new(), dest: Dest::new(),
        };
        for (name, _) in &c.offer.files {
            let made = store::join(&c.folder.path, name).ok_or_else(oom)
                .and_then(|p| kcore::fs::create(&p, &[]).map_err(|e| alloc::format!("{}: {}", p, e)));
            if let Err(e) = made {
                c.let_go(false);
                return Err(e);
            }
        }
        Ok(c)
    }

    /// Its messages taken until `Done`, and the handover after it. Its
    /// migration's id where it runs here.
    fn run(mut self) -> Option<u64> {
        let mut heard_at = now_ms();
        loop {
            if kcore::task::stopping() {
                self.give_up("the module is being unloaded there");
                return None;
            }
            let Some(m) = self.mb.recv(Duration::from_millis(TURN_MS)) else {
                if now_ms() > heard_at + QUIET_MS {
                    let why = alloc::format!("nothing from {} in {} s", self.offer.from, QUIET_MS / 1000);
                    self.give_up(&why);
                    return None;
                }
                continue;
            };
            if m.is_system() || m.tag != migwire::TAG {
                continue;
            }
            let Some(msg) = Msg::decode(&m.data) else { continue };
            if msg.id() != self.offer.id || m.from != self.s {
                if let Msg::Offer(o) = msg {
                    let _ = send(self.mb, m.from, &Msg::Refuse { id: o.id, why: String::from(
                        "another VM is moving here -- once it has, try again") });
                }
                continue;
            }
            heard_at = now_ms();
            let began = kcore::time::boot_time_ns();
            let (kind, seq) = match &msg {
                Msg::File { seq, .. } => (trace::MigKind::File, *seq),
                Msg::Pages { seq, .. } => (trace::MigKind::Pages, *seq),
                Msg::State { seq, .. } => (trace::MigKind::State, *seq),
                Msg::Flows { seq, .. } => (trace::MigKind::Flows, *seq),
                Msg::Done { seq, .. } => (trace::MigKind::Done, *seq),
                Msg::Cancel { .. } => (trace::MigKind::Cancel, 0),
                _ => (trace::MigKind::Offer, 0),
            };
            let bytes = m.data.len();
            if matches!(kind, trace::MigKind::Done | trace::MigKind::Cancel) {
                /* What it starts -- the handover, the end -- is recorded
                 * on its own */
                trace_recv(self.offer.id, seq, bytes, 0, kind);
            }
            let step = match msg {
                Msg::File { seq, name, offset, data, .. } => self.next(seq).and_then(|()| self.file(&name, offset, &data))
                    .and_then(|()| self.ack(seq)),
                Msg::Pages { seq, round, pages, data, .. } => self.next(seq)
                    .and_then(|()| self.pages(round, &pages, &data)).and_then(|()| self.ack(seq)),
                Msg::State { seq, offset, total, data, .. } => self.next(seq)
                    .and_then(|()| self.state_piece(offset, total, &data)).and_then(|()| self.ack(seq)),
                Msg::Flows { seq, flows, .. } => self.next(seq).and_then(|()| {
                    self.flows.clear();
                    self.flows.try_reserve_exact(flows.len()).map_err(|_| String::from("out of memory"))?;
                    self.flows.extend(flows.iter().map(flow_in));
                    self.ack(seq)
                }),
                Msg::Done { seq, volumes, .. } => {
                    return match self.next(seq).and_then(|()| self.ready(&volumes)) {
                        Ok(tokens) => self.take(&volumes, &tokens),
                        Err(why) => {
                            self.give_up(&why);
                            None
                        }
                    };
                }
                Msg::Cancel { why, .. } => {
                    kcore::klog!(0, "hv: migrate: vm {} given up by {}: {}", self.offer.name, self.offer.from, why);
                    trace_end(self.offer.id, trace::MigSide::New, trace::MigHow::Cancelled, 0, 0, self.offer.mem_bytes);
                    self.let_go(false);
                    return None;
                }
                _ => Ok(()),
            };
            trace_recv(self.offer.id, seq, bytes, kcore::time::boot_time_ns().saturating_sub(began), kind);
            if let Err(why) = step {
                self.give_up(&why);
                return None;
            }
        }
    }

    /// Message `seq`, the one after the last: none missed.
    fn next(&mut self, seq: u64) -> Result<(), String> {
        if seq != self.seq + 1 {
            return Err(alloc::format!("message {} came after {}: one was lost", seq, self.seq));
        }
        self.seq = seq;
        Ok(())
    }

    fn ack(&self, seq: u64) -> Result<(), String> {
        send(self.mb, self.s, &Msg::Ack { id: self.offer.id, seq })
    }

    /// A piece of file `name`, the next of it.
    fn file(&mut self, name: &str, offset: u64, data: &[u8]) -> Result<(), String> {
        let k = self.offer.files.iter().position(|(n, _)| n == name)
            .ok_or_else(|| alloc::format!("{}: a file it did not offer", name))?;
        let end = offset.checked_add(data.len() as u64).filter(|&e| e <= self.offer.files[k].1);
        if offset != self.written[k] || end.is_none() {
            return Err(alloc::format!("{}: a piece out of its place", name));
        }
        let path = store::join(&self.folder.path, name).ok_or_else(|| String::from("out of memory"))?;
        let f = kcore::fs::File::open(&path, true).map_err(|e| alloc::format!("{}: {}", path, e))?;
        /* A write never grows a file (`File::write_at`): grown first, to
         * the piece's end -- the pieces come in order */
        let grown = offset + data.len() as u64;
        f.set_len(grown).and_then(|()| f.write_at(offset, data)).map_err(|e| alloc::format!("{}: {}", path, e))?;
        self.written[k] += data.len() as u64;
        Ok(())
    }

    /// Pages of round `round` into the guest's memory: a page of zeros left
    /// as its fresh memory has it in the first round, and written in a
    /// later one.
    fn pages(&mut self, round: u32, pages: &[Page], data: &[u8]) -> Result<(), String> {
        let page = PAGE as usize;
        let memory = self.built.guest.memory();
        let mut at = 0usize;
        for p in pages {
            let gpa = p.number.checked_mul(PAGE).filter(|&g| memory.is_ram(g, PAGE))
                .ok_or_else(|| alloc::format!("page {:#x} is not its memory's", p.number))?;
            if p.zero {
                if round > 0 {
                    memory.write(gpa, &self.zeros).map_err(|e| alloc::format!("its memory at {:#x}: {:?}", gpa, e))?;
                }
            } else {
                let bytes = data.get(at..at + page).ok_or_else(|| String::from("pages short of their data"))?;
                memory.write(gpa, bytes).map_err(|e| alloc::format!("its memory at {:#x}: {:?}", gpa, e))?;
                at += page;
            }
            if round == 0 {
                self.first_round += 1;
            }
        }
        Ok(())
    }

    /// A piece of the state; the guest made it once all of it is here.
    fn state_piece(&mut self, offset: u32, total: u32, data: &[u8]) -> Result<(), String> {
        if self.restored {
            return Err(String::from("a state after its state"));
        }
        if offset == 0 && self.state_bytes.is_empty() {
            self.state_bytes.try_reserve_exact(total as usize).map_err(|_| String::from("out of memory"))?;
        }
        if offset as usize != self.state_bytes.len() || self.state_bytes.capacity() < total as usize {
            return Err(String::from("a piece of its state out of its place"));
        }
        self.state_bytes.extend_from_slice(data);
        if self.state_bytes.len() < total as usize {
            return Ok(());
        }
        let s = vmstate::VmState::decode(&self.state_bytes).map_err(|e| alloc::format!("its state: {}", e))?;
        self.built.guest.restore(&s, &mut self.built.cpus).map_err(|e| alloc::format!("its state: {}", e))?;
        self.restored = true;
        self.state_bytes = Vec::new();
        Ok(())
    }

    /// Everything that can still be refused before the commit, at `Done`:
    /// the state in, every page had, its files written, the volumes it
    /// names its own, and its disks read alone opened and given to the
    /// guest. A token for each written volume's take.
    fn ready(&mut self, volumes: &[(String, u64)]) -> Result<Vec<u64>, String> {
        if !self.restored {
            return Err(String::from("Done before its state"));
        }
        if self.first_round != self.offer.pages {
            return Err(alloc::format!("{} of its {} pages came", self.first_round, self.offer.pages));
        }
        let names = volumes.len() == self.written_disks.len()
            && volumes.iter().zip(&self.written_disks).all(|((v, _), (_, word))| {
                crate::devices::source(word) == crate::devices::Source::Volume(v.as_str())
            });
        if !names {
            return Err(String::from("Done names other volumes than its disks"));
        }
        for (name, _) in &self.offer.files {
            if let Some(path) = store::join(&self.folder.path, name) {
                if let Ok(f) = kcore::fs::File::open(&path, true) {
                    let _ = f.sync();
                }
            }
        }
        for (i, word) in &self.read_disks {
            let disk = QueuedDisk::open(word, true, false, &self.coming.runner, *i as u8)?;
            match self.built.guest.set_disk_backend(*i, Box::new(disk)) {
                Ok(standin) => drop(standin),
                Err(_) => return Err(alloc::format!("its disk {} is not as it was there", word)),
            }
        }
        let mut tokens = Vec::new();
        tokens.try_reserve_exact(volumes.len()).map_err(|_| String::from("out of memory"))?;
        for _ in volumes {
            tokens.push(random_nonzero()?);
        }
        Ok(tokens)
    }

    /// The handover: its first written volume taken from the writer `Done`
    /// names -- the commit -- and the rest after it; or given up where S
    /// took it back first.
    fn take(mut self, volumes: &[(String, u64)], tokens: &[u64]) -> Option<u64> {
        if self.dest.done() != DestDo::TakeFirst {
            self.let_go(false);
            return None;
        }
        let (i0, word0) = self.written_disks[0].clone();
        let since = now_ms();
        let first = loop {
            if kcore::task::stopping() || now_ms() > since + TAKE_FOR_MS {
                /* As if this host had died: S takes the volume back, or
                 * finds it was taken -- nothing runs here either way */
                kcore::klog!(0, "hv: migrate: vm {} given up while its volume was asked for: it runs on {} if its \
                                 take back lands there, and nowhere if this host's did", self.offer.name,
                             self.offer.from);
                trace_end(self.offer.id, trace::MigSide::New, trace::MigHow::Here, 0, 0, self.offer.mem_bytes);
                self.let_go(false);
                return None;
            }
            let asked = now_ms();
            let (t, disk) = match QueuedDisk::open_handover(&word0, volumes[0].1, tokens[0], &self.coming.runner,
                                                            i0 as u8) {
                Ok(d) => (Take::Took, Some(d)),
                Err(NotTaken::Refused) => (Take::Refused, None),
                Err(NotTaken::Unknown(why)) => {
                    kcore::klog!(0, "hv: migrate: vm {}'s volume not had yet -- {}: asked again", self.offer.name, why);
                    (Take::Unknown, None)
                }
            };
            trace_take(self.offer.id, volumes[0].1, now_ms() - asked, trace::MigSide::New, t, i0);
            match self.dest.took_first(t) {
                DestDo::TakeRest => match disk {
                    Some(d) => break d,
                    None => {
                        self.let_go(true);
                        return None;
                    }
                },
                DestDo::Abort => {
                    let why = alloc::format!("its first volume was taken back by {} first", self.offer.from);
                    kcore::klog!(0, "hv: migrate: vm {} given up -- {}", self.offer.name, why);
                    trace_end(self.offer.id, trace::MigSide::New, trace::MigHow::Here, 0, 0, self.offer.mem_bytes);
                    let _ = send(self.mb, self.s, &Msg::Aborted { id: self.offer.id, why });
                    self.let_go(false);
                    return None;
                }
                DestDo::TakeFirst => kcore::task::sleep_ms(AGAIN_MS),
                DestDo::Wait => {
                    self.let_go(false);
                    return None;
                }
            }
        };
        self.commit(first, volumes, tokens)
    }

    /// The VM this host's from here: its folder a VM's -- its vm.toml
    /// written -- its other volumes taken, its disks and its port given to
    /// it, and it run; S told.
    fn commit(mut self, first: QueuedDisk, volumes: &[(String, u64)], tokens: &[u64]) -> Option<u64> {
        let name = self.offer.name.clone();
        /* Its folder a VM's first: whatever fails after this, it is
         * started from here again, its volume this host's */
        let config = store::join(&self.folder.path, store::CONFIG)
            .ok_or_else(|| String::from("out of memory"))
            .and_then(|p| kcore::fs::create(&p, self.offer.config.as_bytes()).map_err(|e| alloc::format!("{}: {}", p, e)));
        if let Err(e) = config {
            kcore::klog!(0, "hv: migrate: vm {}: its vm.toml not written -- {}: write it by hand", name, e);
        }
        let mut disks = Vec::new();
        if disks.try_reserve_exact(self.written_disks.len()).is_err() {
            return self.failed("out of memory");
        }
        disks.push((self.written_disks[0].0, first));
        for (k, (i, word)) in self.written_disks.clone().iter().enumerate().skip(1) {
            let since = now_ms();
            let disk = loop {
                let asked = now_ms();
                let got = QueuedDisk::open_handover(word, volumes[k].1, tokens[k], &self.coming.runner, *i as u8);
                let t = match &got {
                    Ok(_) => Take::Took,
                    Err(NotTaken::Refused) => Take::Refused,
                    Err(NotTaken::Unknown(_)) => Take::Unknown,
                };
                trace_take(self.offer.id, volumes[k].1, now_ms() - asked, trace::MigSide::New, t, *i);
                match got {
                    Ok(d) => break Some(d),
                    Err(NotTaken::Refused) => break None,
                    Err(NotTaken::Unknown(why)) => {
                        if kcore::task::stopping() || now_ms() > since + TAKE_FOR_MS {
                            break None;
                        }
                        kcore::klog!(0, "hv: migrate: vm {}'s {} not had yet -- {}: asked again", name, word, why);
                        kcore::task::sleep_ms(AGAIN_MS);
                    }
                }
            };
            match disk {
                Some(d) => disks.push((*i, d)),
                None => return self.failed(&alloc::format!("its disk {} could not be had", word)),
            }
        }
        for (i, d) in disks {
            match self.built.guest.set_disk_backend(i, Box::new(d)) {
                Ok(standin) => drop(standin),
                Err(_) => return self.failed("a volume is not the disk the guest had"),
            }
        }
        let nic = if self.coming.spec.net { self.move_port() } else { None };
        if self.coming.spec.net && nic.is_none() {
            kcore::klog!(0, "hv: migrate: vm {} runs here without its network -- its port could not be moved: hv \
                             restart it once vnet answers", name);
        }
        let Incoming { state, mb, s, offer, coming, built, folder, logged, .. } = self;
        let mut log = Log::new();
        let what = alloc::format!("{}, moved", offer.from);
        match state.vms.arrived(&state.machine, &state.forwards, coming, built, folder, logged, nic, &what, &mut log) {
            Ok(n) => {
                drop(log);
                /* vnet's table had now, the port here in it */
                state.vms.keeper_now();
                trace_end(offer.id, trace::MigSide::New, trace::MigHow::There, 0, 0, offer.mem_bytes);
                kcore::klog!(0, "hv: migrate: vm {} ({}) runs here, moved from {}", n, name, offer.from);
                let _ = send(mb, s, &Msg::Running { id: offer.id });
                Some(offer.id)
            }
            Err(e) => {
                drop(log);
                trace_end(offer.id, trace::MigSide::New, trace::MigHow::Stopped, 0, 0, offer.mem_bytes);
                kcore::klog!(0, "hv: migrate: vm {} could not run here after its handover -- {}: hv start {} force", name,
                             e, name);
                None
            }
        }
    }

    /// Its port of vnet's moved here, had on the switch and given to its
    /// NIC: what `arrived` keeps -- the port of the switch, the switch, its
    /// UUID and the attach -- or None where it could not be.
    #[allow(clippy::type_complexity)]
    fn move_port(&mut self) -> Option<(usize, Arc<crate::net::Switch>, [u8; 16], crate::vnet::Attached)> {
        let nic = self.offer.nics.first()?;
        let uuid = self.coming.spec.uuid?;
        /* `vnet:<name>`, as its vm.toml says it and a claim takes it */
        let network = self.coming.spec.network.as_deref().filter(|n| crate::config::cluster_network(n).is_some())?;
        let me = nbsdrive::this_host().ok()?;
        let since = now_ms();
        loop {
            match crate::vnet::port_move(nic.port, &me) {
                Ok(_) => break,
                Err(why) => {
                    if kcore::task::stopping() || now_ms() > since + PORT_FOR_MS {
                        kcore::klog!(0, "hv: migrate: vm {}'s port {} not moved -- {}", self.offer.name, nic.port, why);
                        return None;
                    }
                    kcore::task::sleep_ms(AGAIN_MS);
                }
            }
        }
        let (port, switch, a) = match self.state.vms.claim_cluster_port(&self.coming, &uuid, network) {
            Ok(c) => c,
            Err(why) => {
                kcore::klog!(0, "hv: migrate: vm {}'s port {} not had on the switch -- {}", self.offer.name, nic.port,
                             why);
                return None;
            }
        };
        if switch.mac_of(port) != nic.mac {
            kcore::klog!(0, "hv: migrate: vm {}'s port gives it another MAC than it had: its NIC keeps its own",
                         self.offer.name);
        }
        /* What its old host carries on to it a while taken from that host
         * (`Switch::came_here`), and its firewall's flows as they were
         * there: the answers to its connections let in here before it sends
         * anything (M3) */
        if let (Some((vni, addr)), Some(node)) = (switch.vnet_address(port), self.state.vms.node_of(&self.offer.from)) {
            switch.came_here(vni, addr, node);
        }
        let adopted = switch.adopt_flows(port, &self.flows);
        if adopted != self.flows.len() {
            kcore::klog!(0, "hv: migrate: vm {}: {} of its {} flows had here", self.offer.name, adopted,
                         self.flows.len());
        }
        match self.built.guest.set_nic_backend(0, Box::new(switch.backend(port))) {
            Ok(standin) => drop(standin),
            Err(_) => {
                switch.release(port);
                return None;
            }
        }
        Some((port, switch, uuid, a))
    }

    /// Given up after the commit: the VM runs nowhere -- its folder and its
    /// volume this host's -- said, and what was built let go of.
    fn failed(self, why: &str) -> Option<u64> {
        trace_end(self.offer.id, trace::MigSide::New, trace::MigHow::Stopped, 0, 0, self.offer.mem_bytes);
        kcore::klog!(0, "hv: migrate: vm {} could not run here after its handover -- {}: its folder is here; hv start \
                         {} force once that is put right", self.offer.name, why, self.offer.name);
        self.let_go(true);
        None
    }

    /// Given up before the commit: S told -- it runs the VM on -- and what
    /// was built let go of.
    fn give_up(mut self, why: &str) {
        kcore::klog!(0, "hv: migrate: vm {} from {} given up -- {}", self.offer.name, self.offer.from, why);
        trace_end(self.offer.id, trace::MigSide::New, trace::MigHow::Here, 0, 0, self.offer.mem_bytes);
        if self.dest.give_up() == DestDo::Abort {
            let _ = send(self.mb, self.s, &Msg::Aborted { id: self.offer.id, why: clip(why) });
        }
        self.let_go(false);
    }

    /// What was built let go of -- the guest, its CPUs, its name -- and its
    /// folder taken away, unless `keep`: a VM's from the commit on.
    fn let_go(self, keep: bool) {
        let Incoming { built, coming, folder, offer, .. } = self;
        drop(built);
        drop(coming);
        if keep {
            return;
        }
        for (name, _) in &offer.files {
            if let Some(p) = store::join(&folder.path, name) {
                let _ = kcore::fs::remove(&p);
            }
        }
        let _ = kcore::fs::remove(&folder.path);
    }
}
