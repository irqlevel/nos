#![no_std]

//! nbs-mds: NBS's metadata service on this host (plans/nbs.md, N5 and N6) --
//! the state every member holds alike, through raft over ipc, of which NBS's
//! hosts are its members: the NBS cluster's identity and its members, each
//! host's chunk server and its devices, the volumes and where each chunk of
//! them is (nbsmeta).
//!
//!     insmod /lib/modules/ipcnet.ko, then cluster.ko: the hosts that trust each other
//!     insmod /lib/modules/nbs-mds.ko [dir=/nbs/mds] [compact=<bytes>] [out_after=<seconds>] [scrub=<seconds>]
//!     nbs init <name>            on the first host: an NBS cluster of it alone
//!     nbs host add <host>        on a member: <host> -- a host of the registry -- a member
//!     nbs host remove <host>     on a member: <host>'s member out, for good
//!     nbs host replace <host>    on a member: a host that lost its state, back as a new member
//!     nbs hosts                  every member: its incarnation, its vote, the leader
//!     nbs                        this host's member: role, term, leader, what it applied
//!     nbs devices add <id|name> [wipe]   a device of this host's for its chunk server
//!     nbs devices drain <id>     given no new chunks
//!     nbs replica bad <set> <host>   <host>'s cs taken down in <set>, for its join back to copy it again
//!     nbs cs                     every host's chunk server: its devices and room
//!     nbs volume create <name> <size> [host=<h>], delete <name>, resize <name> <size>
//!     nbs volumes, nbs volume <name>
//!     nbs io attach|write|read|flush|close|held|detach <volume> ...   its bytes, through the cs's (io.rs)
//!
//! The same is on the management bus as the `nbs` provider (provider.rs). A
//! device is recorded here, and written by its host's cs (nbs-cs.ko), which
//! asks this host's mds -- this module's member -- what it is to take in, and
//! whether a request it has not checked is the metadata's (member.rs).
//! `nbs io` is a client of the cs's, for the shell and the tests.
//!
//! `compact=` is how many bytes of entries the log holds past its snapshot
//! before it is compacted into a new one -- or the state's size, should that
//! be more: raft's 1 MiB unless given. A test gives it a few, so that a
//! member started again reads its state back from a snapshot, and a member
//! added is sent one.
//!
//! `scrub=` is the leader's scrub's pace (N9e): a chunk every so many
//! seconds, ten unless given, 0 for none -- each look reads the chunk whole on
//! its set's every member it is closed on. What it finds it says, in the log
//! and in `nbs status`, and takes nothing down: no checksum of its own data
//! does a cs keep, and two members alike are no proof which one holds the
//! data. `nbs replica bad` is a person's.
//!
//! Who the members are is two things (plans/nbs.md, "Who is in the cluster"):
//! the hosts that may talk at all are the cluster registry's, ipcnet pinning
//! each by its key; the members are the NBS cluster's Raft configuration,
//! each a host and the incarnation its mds made when it was offered the
//! cluster -- 64 random bits, never made again. A host whose `dir` was lost
//! comes back with none and is offered the cluster as a new member, never as
//! the one it was: Raft never counts the vote of a member that forgot its
//! log.
//!
//! While this host is in no NBS cluster, a listener holds `nbs.mds`: it takes
//! an offer -- making the incarnation, the member file and an empty log, and
//! starting the member, which raft then brings the log -- and answers a raft
//! message for a member it does not have with `TAG_NO_MEMBER`, which is how a
//! leader learns that a host's state is gone. The member is raftmember's with
//! NBS's hooks (member.rs), a task of its own the shell hands its commands to.

extern crate alloc;

mod files;
mod io;
mod member;
mod provider;
mod store;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::cmd::{Command as ShellCommand, Output};
use kcore::error::Error;
use kcore::mgmt::Provider;
use kcore::ipc::{Limits, Mailbox, Message, Scope, TAG_BULK};
use kcore::klog;
use kcore::task::TaskHandle;
use kcore::time::{boot_time_ns, Duration};
use nbsmeta::{Answer, Command, State};
use raft::{Config, LogConfig, Role, Rsm};
use raftmember::{KFiles, Locked};

use files::Known;
use member::{Lost, Offer, Shared, View};

const HELP: &str = "nbs [init | host | hosts | devices | cs | volume | volumes | io] - NBS's metadata service";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: nbs                          this host's member: incarnation, role, leader, what it applied\n\
                     \x20      nbs init <name>            an NBS cluster of this host alone, named <name>\n\
                     \x20      nbs hosts                  every member: incarnation, vote, the leader\n\
                     \x20      nbs host add <host>        <host>, a host of the cluster registry, a member: a learner\n\
                     \x20                                 until it holds the log, then a voter while fewer than five vote\n\
                     \x20      nbs host remove <host>     <host>'s member out, for good\n\
                     \x20      nbs host replace <host>    <host>, which lost its state, back as a new member\n\
                     \x20      nbs devices add <id|name> [wipe]   a device of this host's for its chunk server: \
                     with wipe,\n\
                     \x20                                 even one with something else on it\n\
                     \x20      nbs devices drain <id>     given no new chunks\n\
                     \x20      nbs replica bad <set> <host>   <host>'s cs taken down in <set>, to copy its chunks \
                     again\n\
                     \x20                                 -- the member the scrub found unlike the two others\n\
                     \x20      nbs cs                     every host's chunk server: its devices and room\n\
                     \x20      nbs volumes                every volume\n\
                     \x20      nbs volume <name>          one, and which hosts hold its replicas\n\
                     \x20      nbs volume create <name> <size> [host=<host>]   size in MiB, or with M, G, T\n\
                     \x20      nbs volume delete <name>\n\
                     \x20      nbs volume resize <name> <size>\n\
                     \x20      nbs volume snapshot <volume> <name>   on the host it is attached on, its writer paused\n\
                     \x20      nbs io write <volume> <offset> <bytes> <byte>   through the cs's; read <volume> \
                     <offset> <bytes>,\n\
                     \x20                                 attach <volume> [force], flush, close, held and detach \
                     <volume>";
pub(crate) const NO_MEMBER: &str = "this host is in no NBS cluster -- `nbs init <name>`, or `nbs host add <this host>` \
                                    on a member";

/// How long an offer is tried for -- the connection to the host coming up
/// first -- and how long each try waits for its answer.
const OFFER_TOTAL_MS: u64 = 30_000;
const OFFER_TRY_MS: u64 = 3000;
/// How often the listener looks for the module's unload.
const IDLE_POLL_MS: u64 = 100;
/// How often a host is told it named a member this host's mds does not have.
const NO_MEMBER_EVERY_NS: u64 = 5_000_000_000;
/// How long a change applied is waited for in the member's view.
const VIEW_WAIT_MS: u64 = 5000;
const NS_PER_MS: u64 = 1_000_000;
/// The leader's scrub's pace unless `scrub=` is given: a chunk every ten
/// seconds, which reads it whole on each of its set's members (N9e).
const SCRUB_MS: u64 = 10_000;

/// The module's: its folder, the member while there is one, the listener
/// while there is not.
struct ModState {
    dir: String,
    /// The log's bytes past its snapshot before it is compacted: `compact=`.
    compact: u64,
    /// How long a cs is silent before the leader's controller has it out
    /// and its places taken by others: `out_after=`, ten minutes unless
    /// given (N9c).
    out_after_ms: u64,
    /// The leader's scrub's pace, a chunk every so long: `scrub=`, ten
    /// seconds unless given, 0 for none (N9e); and what it found.
    scrub_ms: u64,
    scrubbed: member::Scrubbed,
    shared: Arc<Shared>,
    lost: Lost,
    task: Locked<Option<TaskHandle>>,
    idle: Locked<Option<TaskHandle>>,
    idle_stop: AtomicBool,
    /// An `init` or an offer under way: the one at a time that may start a
    /// member.
    starting: AtomicBool,
    /// The volumes `nbs io` attached here for writing.
    io: Locked<io::Attachments>,
}

/// The right to start a member, held for an `init` or an offer.
struct Starting<'a>(&'a AtomicBool);

impl<'a> Starting<'a> {
    fn take(state: &'a ModState) -> Result<Starting<'a>, String> {
        state.starting.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Starting(&state.starting))
            .map_err(|_| String::from("an `nbs init` or an offer is under way"))
    }
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/* Dropped in this order: the command and the provider -- the loader has
   waited out every call of them before this drop, the member's task they
   hand their changes to among what such a call may have been waiting on --
   then the listener stopped and waited out, then the member's task, its log
   closed with it */
struct Nbs {
    cmd: Option<ShellCommand>,
    provider: Option<Provider>,
    state: Arc<ModState>,
}

impl kmod::Module for Nbs {}

impl Drop for Nbs {
    fn drop(&mut self) {
        drop(self.provider.take());
        drop(self.cmd.take());
        stop_idle(&self.state);
        stop(&self.state);
    }
}

/// The member's task stopped and waited out -- taken from its lock first,
/// and dropped off it -- and the shell shown no member.
fn stop(state: &ModState) {
    state.shared.stop.store(true, Ordering::Release);
    let task = state.task.with(|t| t.take());
    drop(task);
    state.shared.view.with(|v| *v = None);
}

/// The listener stopped and waited out: `nbs.mds` let go of.
fn stop_idle(state: &ModState) {
    state.idle_stop.store(true, Ordering::Release);
    let task = state.idle.with(|t| t.take());
    drop(task);
}

fn init(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    let mut dir = None;
    let mut compact = None;
    let mut out_after = None;
    let mut scrub = None;
    for (key, value) in params.words() {
        match (key, value) {
            ("dir", Some(path)) if dir.is_none() && path.starts_with('/') && path.len() < 200 => dir = Some(path),
            ("compact", Some(bytes)) if compact.is_none() && bytes.parse::<u64>().is_ok() => {
                compact = bytes.parse::<u64>().ok();
            }
            ("out_after", Some(secs)) if out_after.is_none() && secs.parse::<u64>().is_ok_and(|s| (10..=86_400).contains(&s)) => {
                out_after = secs.parse::<u64>().ok();
            }
            ("scrub", Some(secs)) if scrub.is_none() && secs.parse::<u64>().is_ok_and(|s| s <= 86_400) => {
                scrub = secs.parse::<u64>().ok();
            }
            _ => {
                let _ = writeln!(out, "nbs: \"{}\" is not a parameter of nbs-mds's -- dir=<folder>, \
                                       compact=<bytes>, out_after=<seconds, 10 to 86400> and scrub=<seconds a chunk, \
                                       0 for none, to 86400> are, each given once", key);
                return Err(Error::InvalidValue);
            }
        }
    }
    let state = Arc::new(ModState {
        dir: String::from(dir.unwrap_or(files::DEFAULT_DIR).trim_end_matches('/')),
        compact: compact.unwrap_or(raft::rsm::SNAPSHOT_FLOOR),
        out_after_ms: out_after.map_or(nbsmeta::repair::OUT_AFTER_MS, |s| s * 1000),
        scrub_ms: scrub.map_or(SCRUB_MS, |s| s * 1000),
        scrubbed: Arc::new(Locked::new(member::Scrubs::default()).ok_or(Error::NoMemory)?),
        shared: Arc::new(Shared::new().ok_or(Error::NoMemory)?),
        lost: Arc::new(Locked::new(BTreeMap::new()).ok_or(Error::NoMemory)?),
        task: Locked::new(None).ok_or(Error::NoMemory)?,
        idle: Locked::new(None).ok_or(Error::NoMemory)?,
        idle_stop: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        io: Locked::new(BTreeMap::new()).ok_or(Error::NoMemory)?,
    });
    /* A member already: started over its log. None: the listener, for an
       offer. A member file that cannot be read: neither -- the host is said
       to be broken rather than offered the cluster as somebody new */
    match files::read_member(&state.dir) {
        Ok(Some(m)) => {
            if let Err(why) = start(&state, m, false) {
                let _ = writeln!(out, "nbs: {}", why);
            }
        }
        Ok(None) => {
            if let Err(why) = start_idle(&state) {
                let _ = writeln!(out, "nbs: {}", why);
            }
        }
        Err(why) => {
            let _ = writeln!(out, "nbs: {} -- this host takes no offer until it is put right", why);
        }
    }
    let st = state.clone();
    let cmd = ShellCommand::register("nbs", HELP, move |args, out| {
        if let Err(problem) = run(&st, args, out) {
            let _ = writeln!(out, "nbs: {}", problem);
        }
    })?;
    let provider = provider::register(state.clone()).map_err(|_| Error::Busy)?;
    Ok(Box::new(Nbs { cmd: Some(cmd), provider: Some(provider), state }))
}

kmod::module!(name: "nbs-mds", init_with_params: init);

/// The member started over its log: made new, with this host its one
/// voter, when `fresh`. The shell sees it before this returns.
fn start(state: &ModState, me: files::Member, fresh: bool) -> Result<(), String> {
    if state.task.with(|t| t.is_some()) {
        return Err(String::from("the member is running already"));
    }
    let files = KFiles::new(&state.dir, "raft")?;
    let log = if fresh {
        raft::DiskLog::bootstrap(files, LogConfig::default(), &[me.id], &[])
    } else {
        raft::DiskLog::open(files, LogConfig::default())
    }
    .map_err(|e| alloc::format!("the log in {}/raft: {:?}", state.dir, e))?;
    let mut cfg = Config::new(me.id);
    cfg.seed = kcore::random::random_u64().unwrap_or(me.id);
    let nonce = kcore::random::random_u64().ok_or_else(|| String::from("no randomness for the member's start"))?;
    let mut rsm = Rsm::new(cfg, log, State::new(), nonce).map_err(|e| alloc::format!("the member: {:?}", e))?;
    rsm.set_snapshot_floor(state.compact);
    if fresh {
        /* Its group's one voter: leading at once rather than at a timeout */
        let _ = rsm.campaign();
    }
    let member = member::new(rsm, me, state.lost.clone(), &state.dir, state.out_after_ms, state.scrub_ms,
                             state.scrubbed.clone())?;
    state.shared.stop.store(false, Ordering::Release);
    /* Shown before the task's first turn, so that a command right after
       this one finds the member */
    state.shared.view.with(|v| *v = Some(member.view(None)));
    let Some(task) = kcore::task::spawn_with("nbs/mds", (state.shared.clone(), member),
                                             raftmember::run::<member::Mds>) else {
        state.shared.view.with(|v| *v = None);
        return Err(String::from("could not start the member's task"));
    };
    state.task.with(|t| *t = Some(task));
    Ok(())
}

/* ---- while this host is in no cluster: the listener ---- */

fn start_idle(state: &Arc<ModState>) -> Result<(), String> {
    state.idle_stop.store(false, Ordering::Release);
    let mb = Mailbox::new(Limits { messages: 256, bytes: 1 << 20 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    mb.register(member::NAME, Scope::Cluster).map_err(|e| alloc::format!("{} not named: {}", member::NAME, e.name()))?;
    let Some(task) = kcore::task::spawn_with("nbs/idle", (state.clone(), mb), idle_run) else {
        return Err(String::from("could not start the listener's task"));
    };
    state.idle.with(|t| *t = Some(task));
    Ok(())
}

/// The listener: an offer taken, and the member started -- or until the
/// module goes.
fn idle_run((state, mb): (Arc<ModState>, Mailbox)) {
    let mut told: BTreeMap<String, u64> = BTreeMap::new();
    while !state.idle_stop.load(Ordering::Acquire) {
        let Some(m) = mb.recv(Duration::from_millis(IDLE_POLL_MS)) else { continue };
        match m.tag & !TAG_BULK {
            member::TAG_OFFER => {
                if let Some(me) = take_offer(&state, &mb, &m) {
                    /* `nbs.mds` let go of, for the member to take */
                    drop(mb);
                    if let Err(why) = start(&state, me, false) {
                        klog!(0, "nbs: the offer was taken and the member did not start: {} -- `rmmod nbs-mds` and \
                                  `insmod` start it again", why);
                    }
                    return;
                }
            }
            raftmember::TAG_RAFT => no_member(&mb, &m, &mut told),
            nbswire::TAG_QUERY => no_mds(&mb, &m),
            _ => {}
        }
    }
}

/// A raft message to a member this host does not have -- it lost its state,
/// or never was it -- answered so, at most every few seconds a host.
fn no_member(mb: &Mailbox, m: &Message, told: &mut BTreeMap<String, u64>) {
    let Some(host) = kcore::ipc::peer_name(m.from) else { return };
    if m.data.len() < raftmember::GROUP_BYTES {
        return;
    }
    let mut cluster = [0u8; 16];
    cluster.copy_from_slice(&m.data[..raftmember::GROUP_BYTES]);
    let Ok(rm) = raft::wire::decode(&m.data[raftmember::GROUP_BYTES..]) else { return };
    let now = boot_time_ns();
    if told.get(&host).is_some_and(|&at| now < at + NO_MEMBER_EVERY_NS) {
        return;
    }
    if told.len() >= nbsmeta::MEMBERS_MAX && !told.contains_key(&host) {
        told.clear();
    }
    told.insert(host, now);
    let _ = mb.send(m.from, member::TAG_NO_MEMBER, 0, &member::encode_id(&cluster, rm.to));
}

/// A question of this host's cs, while there is no member to answer it:
/// answered so, rather than left to time out.
fn no_mds(mb: &Mailbox, m: &Message) {
    if m.from.host() != 0 {
        return;
    }
    let Some(q) = nbswire::Query::decode(&m.data) else { return };
    let told = nbswire::Told::Refused(nbswire::Refusal::NoMds);
    let _ = mb.send(m.from, nbswire::TAG_TOLD, 0, &nbswire::Answered { id: q.id, told }.encode());
}

/// An offer taken, where it can be: this host's incarnation made, its
/// member file written, its folder's log cleared -- the member to start.
/// Answered either way.
fn take_offer(state: &ModState, mb: &Mailbox, m: &Message) -> Option<files::Member> {
    let decline = |why: &str| {
        let _ = mb.send(m.from, member::TAG_DECLINED, m.reference, why.as_bytes());
    };
    let Some(o) = Offer::decode(&m.data) else { return None };
    let Ok(_one) = Starting::take(state) else {
        decline("an `nbs init` is under way on this host");
        return None;
    };
    let id = match raftmember::ipcnet::identity() {
        Ok(id) => id,
        Err(why) => {
            decline(&alloc::format!("this host does not know who it is: {}", why));
            return None;
        }
    };
    /* The cluster has this host as a member already: it lost its state, and
       is taken back as a new member alone -- the old one removed first */
    if let Some(old) = o.members.iter().find(|k| k.host == id.name) {
        decline(&alloc::format!("{}'s mds has no state, and the NBS cluster has it as member {} -- `nbs host replace \
                                 {}` takes it back as a new member", id.name, old.id, id.name));
        return None;
    }
    let mut me = files::Member { id: 0, cluster: o.cluster, host: id.name.clone(), name: o.name.clone(),
                                 known: o.members.clone() };
    /* An incarnation no member of the offer has */
    while me.id == 0 || o.members.iter().any(|k| k.id == me.id) {
        match kcore::random::random_u64() {
            Some(r) => me.id = r,
            None => {
                decline("no randomness for an incarnation");
                return None;
            }
        }
    }
    let made = files::clear_log(&state.dir).and_then(|()| files::create_member(&state.dir, &me));
    if let Err(why) = made {
        decline(&alloc::format!("{} could not take the offer: {}", id.name, why));
        return None;
    }
    klog!(0, "nbs: {} offered NBS cluster {}: member {} -- the log follows from the leader", id.name, o.name, me.id);
    let _ = mb.send(m.from, member::TAG_ACCEPTED, m.reference, &member::encode_id(&me.cluster, me.id));
    Some(me)
}

/* ---- the shell ---- */

pub(crate) fn view(state: &ModState) -> Option<View> {
    state.shared.view()
}

pub(crate) fn member_view(state: &ModState) -> Result<View, String> {
    view(state).filter(|v| v.stopped.is_none()).ok_or_else(|| String::from(NO_MEMBER))
}

/// `cmd`, as the metadata answers it -- asked again while the member cannot
/// take it yet.
fn ask(state: &ModState, cmd: &Command) -> Result<u64, String> {
    match raftmember::ask(&state.shared, cmd, NO_MEMBER)? {
        Answer::Done { member } => Ok(member),
        Answer::Refused(why) => Err(String::from(why.says())),
        other => Err(alloc::format!("answered {:?}", other)),
    }
}

/// This member's view once its log has the cluster: a member just added
/// holds none until raft has brought it the log, which it is given a few
/// seconds for.
pub(crate) fn caught_up(state: &ModState) -> Result<View, String> {
    let end = boot_time_ns() + VIEW_WAIT_MS * NS_PER_MS;
    loop {
        let v = member_view(state)?;
        if v.state.cluster().is_some() {
            return Ok(v);
        }
        if boot_time_ns() >= end {
            return Err(String::from("no NBS cluster yet -- this member's log is behind: ask again in a moment"));
        }
        kcore::task::sleep_ms(50);
    }
}

/// `host` offered the cluster: the incarnation its mds is -- made now, or
/// the one it was already -- asked again until its connection is up and it
/// answers.
fn offer(v: &View, host: &str) -> Result<u64, String> {
    let c = v.state.cluster().ok_or_else(|| String::from("no NBS cluster yet -- this member's log is behind"))?;
    let o = Offer {
        cluster: c.id,
        name: c.name.clone(),
        members: v.state.members().iter().map(|m| Known { host: m.host.clone(), id: m.id }).collect(),
    };
    let mb = Mailbox::new(Limits { messages: 8, bytes: 16 * 1024 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    let to = alloc::format!("{}@{}", member::NAME, host);
    let data = o.encode();
    let end = boot_time_ns() + OFFER_TOTAL_MS * NS_PER_MS;
    let mut last = alloc::format!("no answer from {}", host);
    while boot_time_ns() < end {
        if let Err(e) = mb.send_to(&to, member::TAG_OFFER, 0, &data) {
            last = alloc::format!("{} out of reach: {} -- is it a host of the cluster registry, with nbs-mds loaded?",
                                  host, e.name());
            kcore::task::sleep_ms(1000);
            continue;
        }
        let until = boot_time_ns() + OFFER_TRY_MS * NS_PER_MS;
        while boot_time_ns() < until {
            let Some(m) = mb.recv(Duration::from_millis(250)) else { continue };
            /* From the host offered, and no other */
            if kcore::ipc::peer_name(m.from).as_deref() != Some(host) {
                continue;
            }
            match m.tag {
                member::TAG_ACCEPTED => {
                    let (cluster, id) = member::decode_id(&m.data)
                        .ok_or_else(|| alloc::format!("{} accepted in no form an acceptance has", host))?;
                    if cluster != c.id {
                        return Err(alloc::format!("{} accepted as a member of another cluster", host));
                    }
                    return Ok(id);
                }
                member::TAG_DECLINED => return Err(String::from_utf8_lossy(&m.data).into_owned()),
                _ => {}
            }
        }
        last = alloc::format!("no answer from {} yet", host);
    }
    Err(last)
}

fn op_add(state: &ModState, host: &str) -> Result<u64, String> {
    let v = caught_up(state)?;
    if !nbsmeta::valid_name(host) {
        return Err(alloc::format!("\"{}\" is not a host's name", host));
    }
    let id = offer(&v, host)?;
    ask(state, &Command::Add { host: String::from(host), member: id })
}

fn op_remove(state: &ModState, host: &str) -> Result<u64, String> {
    member_view(state)?;
    ask(state, &Command::Remove { host: String::from(host) })
}

/// A host that lost its state taken back: its old member removed -- for
/// good, its incarnation never taken again -- and the host offered the
/// cluster as a new one.
fn op_replace(state: &ModState, host: &str) -> Result<(u64, u64), String> {
    let v = caught_up(state)?;
    let old = v.state.member(host).map(|m| m.id);
    let removed = match old {
        Some(_) => op_remove(state, host)?,
        None => return Err(alloc::format!("{} is no member -- `nbs host add {}`", host, host)),
    };
    /* The removal in the member's view before the offer is made of it: an
       offer that named the host a member still would be declined there */
    let end = boot_time_ns() + VIEW_WAIT_MS * NS_PER_MS;
    let mut v = member_view(state)?;
    while v.state.member(host).is_some() && boot_time_ns() < end {
        kcore::task::sleep_ms(20);
        v = member_view(state)?;
    }
    let id = offer(&v, host).map_err(|why| {
        alloc::format!("{}'s old member {} is removed, and the offer of a new one failed: {} -- `nbs host add {}` \
                        once it answers", host, removed, why, host)
    })?;
    if id == removed {
        return Err(alloc::format!("{}'s mds is still member {}, the one removed: it did not lose its state -- \
                                   `rmmod nbs-mds`, `rm {}` and `insmod` it there, then `nbs host add {}`", host, id,
                                  files::DEFAULT_DIR, host));
    }
    let id = ask(state, &Command::Add { host: String::from(host), member: id })?;
    Ok((removed, id))
}

fn run(state: &Arc<ModState>, args: &str, out: &mut Output) -> Result<(), String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => status(state, out),
        ["init", name] => init_cluster(state, name, out),
        ["hosts"] => hosts(state, out),
        ["host", "add", host] => {
            let id = op_add(state, host)?;
            let _ = writeln!(out, "nbs: {} is member {} -- a learner until it holds the log, then a voter while fewer \
                                   than five vote", host, id);
            Ok(())
        }
        ["host", "remove", host] => {
            let id = op_remove(state, host)?;
            let _ = writeln!(out, "nbs: {}'s member {} removed, for good", host, id);
            Ok(())
        }
        ["host", "replace", host] => {
            let (old, new) = op_replace(state, host)?;
            let _ = writeln!(out, "nbs: {}'s member {} removed, and {} is member {} now", host, old, host, new);
            Ok(())
        }
        ["devices", "add", word, rest @ ..] => {
            let wipe = match rest {
                [] => false,
                ["wipe"] => true,
                _ => return Err(String::from(USAGE)),
            };
            let (ident, slots, id, wiped) = store::device_add(state, word, wipe).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: device {} of this host's chunk server, {} -- device {} in the metadata{}; its \
                                   cs takes it in", ident, store::Mib(u64::from(slots) * u64::from(nbsmeta::CHUNK_MIB)),
                             id, if wiped { ", its first pages wiped" } else { "" });
            Ok(())
        }
        ["replica", "bad", set, host] => {
            let set = set.parse::<u32>().map_err(|_| alloc::format!("\"{}\" is no set's number", set))?;
            let version = store::replica_bad(state, set, host).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: {}'s cs taken down in set {} (now at version {}): it joins its own place \
                                   again, its copy held to another member's by sums", host, set, version);
            Ok(())
        }
        ["devices", "drain", ident] => {
            let id = store::device_drain(state, ident).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: device {} ({}) takes no new chunks", ident, id);
            Ok(())
        }
        ["devices"] | ["cs"] => {
            store::show_cs(&member_view(state)?, out);
            Ok(())
        }
        ["volumes"] => {
            store::show_volumes(&member_view(state)?, out);
            Ok(())
        }
        ["volume", "create", name, size, rest @ ..] => {
            let size_mib = store::parse_size(size).ok_or_else(|| alloc::format!("\"{}\" is not a size: MiB, or with \
                                                                                 M, G or T after it", size))?;
            let host = match rest {
                [] => None,
                [h] => Some(h.strip_prefix("host=").ok_or_else(|| String::from(USAGE))?),
                _ => return Err(String::from(USAGE)),
            };
            let id = store::volume_create(state, name, size_mib, host).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: volume {} ({}), {}", name, id, store::Mib(size_mib));
            Ok(())
        }
        ["volume", "delete", name] => {
            match store::volume_delete(state, name).map_err(|f| f.says())? {
                0 => {
                    let _ = writeln!(out, "nbs: no volume {}: gone already", name);
                }
                id => {
                    let _ = writeln!(out, "nbs: volume {} ({}) deleted", name, id);
                }
            }
            Ok(())
        }
        ["volume", "resize", name, size] => {
            let size_mib = store::parse_size(size).ok_or_else(|| alloc::format!("\"{}\" is not a size", size))?;
            let id = store::volume_resize(state, name, size_mib).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: volume {} ({}) is {} now", name, id, store::Mib(size_mib));
            Ok(())
        }
        ["volume", "snapshot", volume, name] => io::snapshot(state, volume, name, out),
        ["volume", name] => store::show_volume(&member_view(state)?, name, out),
        ["io", rest @ ..] => io::run(state, rest, out),
        _ => Err(String::from(USAGE)),
    }
}

fn init_cluster(state: &Arc<ModState>, name: &str, out: &mut Output) -> Result<(), String> {
    if !nbsmeta::valid_name(name) {
        return Err(alloc::format!("\"{}\" is not a cluster's name: lowercase letters, digits and -", name));
    }
    let _starting = Starting::take(state)?;
    if files::read_member(&state.dir)?.is_some() || view(state).is_some() {
        return Err(String::from("this host is a member of an NBS cluster already"));
    }
    let id = raftmember::ipcnet::identity()?;
    let mut cluster = [0u8; 16];
    let member = kcore::random::random_u64().filter(|&m| m != 0);
    let (Some(member), true) = (member, kcore::random::fill_random(&mut cluster)) else {
        return Err(String::from("no randomness for the cluster's id"));
    };
    /* `nbs.mds` taken from the listener, for the member */
    stop_idle(state);
    let undo = |why: String| -> String {
        stop(state);
        let left = files::remove_member(&state.dir).and_then(|()| files::clear_log(&state.dir)).err();
        let back = start_idle(state).err();
        match (left, back) {
            (None, None) => why,
            (left, back) => alloc::format!("{} -- and undoing it: {}", why,
                                           [left, back].into_iter().flatten().collect::<Vec<_>>().join("; ")),
        }
    };
    /* What an init or an offer that did not finish left */
    if let Err(why) = files::clear_log(&state.dir) {
        return Err(undo(why));
    }
    let me = files::Member { id: member, cluster, host: id.name.clone(), name: String::from(name), known: Vec::new() };
    if let Err(why) = start(state, me.clone(), true) {
        return Err(undo(why));
    }
    /* The member file last: the mark that this host is a member */
    let made = ask(state, &Command::Init { cluster, name: String::from(name), host: id.name.clone(), member })
        .and_then(|_| files::create_member(&state.dir, &me));
    match made {
        Ok(()) => {
            let _ = writeln!(out, "nbs: NBS cluster {} made ({}), {} its first member ({}) -- `nbs host add` the next",
                             name, codec::hex(&cluster), id.name, member);
            Ok(())
        }
        Err(why) => Err(undo(why)),
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Leader => "leader",
        Role::Follower => "follower",
        Role::Candidate | Role::PreCandidate => "candidate",
    }
}

fn status(state: &ModState, out: &mut Output) -> Result<(), String> {
    let Some(v) = view(state) else {
        let _ = writeln!(out, "nbs: {}", NO_MEMBER);
        return Ok(());
    };
    if let Some(why) = &v.stopped {
        let _ = writeln!(out, "nbs: the member stopped: {}", why);
    }
    if v.state.cluster().is_some() && v.state.member_of_id(v.node).is_none() {
        let _ = writeln!(out, "nbs: this host's member ({}) is removed from the NBS cluster -- `rmmod nbs-mds`, \
                               `rm {}` and `insmod` it, then `nbs host add` it again", v.node, state.dir);
    }
    let (name, id) = v.state.cluster().map_or((String::from("(no state yet)"), String::new()),
                                              |c| (c.name.clone(), alloc::format!(" ({})", codec::hex(&c.id))));
    let leader = v.state.member_of_id(v.leader).map_or("none", |m| m.host.as_str());
    let _ = writeln!(out, "nbs: {}{} -- member {}, {}, term {}, leader {}, applied {}, {} members, {} voters, {} \
                           learners, {} messages refused", name, id, v.node, role_name(v.role), v.term, leader,
                     v.applied, v.state.members().len(), v.membership.voters.len(), v.membership.learners.len(),
                     v.refused);
    /* How far from three the chunks are (N9) */
    let h = nbsmeta::repair::health(&v.state);
    let joins = v.state.sets().iter().filter(|s| s.joiner.is_some()).count();
    let _ = writeln!(out, "nbs: chunks at three replicas {}, at two {}, at one {} -- {} being copied, by {} joins; \
                           cs's down {}, out {}", h.by_active[3], h.by_active[2], h.by_active[1], h.joining, joins,
                     h.down, h.out);
    /* The scrub's, as this host's member led it (N9e) */
    let (stats, found) = state.scrubbed.with(|x| (x.stats, x.found.clone()));
    if state.scrub_ms == 0 {
        let _ = writeln!(out, "nbs: no scrub here (scrub=0)");
    } else {
        let _ = writeln!(out, "nbs: the scrub, a chunk every {} s as this host's member leads: {} alike, {} held by none, \
                               {} let be, {} unlike -- {} with a member unlike the two others", state.scrub_ms / 1000,
                         stats.alike, stats.empty, stats.skipped, stats.odd + stats.differ, stats.odd);
    }
    for (chunk, set, version, odd) in found {
        let who = odd.and_then(|cs| v.state.cs().get(cs as usize)).map(|c| c.host.as_str());
        /* A finding of a set that has moved on since -- the member taken
           down, joined back -- is history, and says so */
        let now = v.state.sets().get(set as usize).map(|s| s.version).filter(|&now| now != version);
        match (who, now) {
            (Some(host), None) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): {}'s copy is unlike the two others' \
                                       -- `nbs replica bad {} {}` has it copied again", chunk, set, version, host, set,
                                 host);
            }
            (Some(host), Some(now)) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): {}'s copy was unlike the two \
                                       others' -- the set is at version {} now", chunk, set, version, host, now);
            }
            (None, None) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): its members' copies differ, no two \
                                       alike", chunk, set, version);
            }
            (None, Some(now)) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): its members' copies differed, no \
                                       two alike -- the set is at version {} now", chunk, set, version, now);
            }
        }
    }
    match v.state.checks().latest() {
        Some((at, d)) => {
            let _ = writeln!(out, "nbs: the state's digest at entry {}: {}... -- {} of the leader's held to it, {} \
                                   too old to", at, codec::hex(&d[..8]), v.state.checks().held,
                             v.state.checks().unheld);
        }
        None => {
            let _ = writeln!(out, "nbs: no digest of the state yet -- one every {} entries", nbsmeta::CHECK_EVERY);
        }
    }
    let lost: Vec<(String, u64)> = state.lost.with(|l| l.iter().map(|(h, &(i, _))| (h.clone(), i)).collect());
    for (host, id) in lost {
        let _ = writeln!(out, "nbs: {} says its mds has no member {}: its state is gone -- `nbs host replace {}`",
                         host, id, host);
    }
    Ok(())
}

fn hosts(state: &ModState, out: &mut Output) -> Result<(), String> {
    let v = member_view(state)?;
    let lost: Vec<String> = state.lost.with(|l| l.keys().cloned().collect());
    for m in v.state.members() {
        let vote = if v.membership.voters.contains(&m.id) {
            "voter"
        } else if v.membership.learners.contains(&m.id) {
            "learner"
        } else {
            "joining"
        };
        let mut more = String::new();
        if m.id == v.leader {
            more.push_str(" (leader)");
        }
        if m.id == v.node {
            more.push_str(" (this host)");
        }
        if lost.contains(&m.host) {
            more.push_str(" lost its state");
        }
        let _ = writeln!(out, "  {:<16} member {:<20} {:<8}{}", m.host, m.id, vote, more);
    }
    if !v.state.removed().is_empty() {
        let _ = writeln!(out, "  {} members removed", v.state.removed().len());
    }
    Ok(())
}
