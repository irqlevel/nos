#![no_std]

//! cluster: the registry of a nos cluster's hosts, as a module
//! (plans/cluster.md, C1) -- one place for the hosts' keys, addresses and
//! roles, kept by every host through raft, so that a host is let in once and
//! every other learns of it; ipcnet is told the peers it keeps.
//!
//!     insmod /lib/modules/ipcnet.ko              (first: this host's name and key are ipcnet's)
//!     insmod /lib/modules/cluster.ko [conf=/etc/cluster]
//!     cluster init <name>             on the first host: a cluster of it alone
//!     cluster invite <host> <ip> <key>   on a member: a token for `cluster join` on <host>, once --
//!                                     <key> is what `ipcnet id` says there
//!     cluster join <token>            on the new host: in, as the token says
//!     cluster hosts                   every host: its node, vote, address and key
//!     cluster remove <host>           a host out, for good
//!     cluster                         this member: its node, role, leader, what it applied
//!
//! The state is `registry::State`, raft's state machine; its log is in
//! `<conf>/raft`, and `<conf>/member` says which member of which cluster
//! this host is (files.rs). The member is a task of its own (member.rs),
//! which the shell hands its commands to.

extern crate alloc;

mod files;
mod member;
mod token;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use ipcwire::control::{self, Identity};
use kcore::cmd::{Command as ShellCommand, Output};
use kcore::error::Error;
use kcore::ipc::{Limits, Mailbox};
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use kcore::time::Duration;
use raft::{Config, LogConfig, Role, Rsm};
use registry::{Answer, Command, State};

use files::{Dir, KFiles};
use member::{Member, Request, Shared, Slot, View};
use token::Token;

const HELP: &str = "cluster [init | invite | join | hosts | remove] - this host in its cluster's registry";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: cluster                          this member: its node, role, leader, what it applied\n\
                     \x20      cluster init <name>             a cluster of this host alone, named <name>\n\
                     \x20      cluster invite <host> <ip> <key>   a token for `cluster join` on <host>, from <ip>,\n\
                     \x20                                     with <key> -- what `ipcnet id` says there -- once\n\
                     \x20      cluster join <token>            on the host invited: into the cluster, as the token says\n\
                     \x20      cluster hosts                   every host: node, vote, address, key\n\
                     \x20      cluster remove <host>           a host out, for good";

/// How long an invitation is good for, by the leader's clock.
const INVITE_SECS: u64 = 15 * 60;
/// How long the shell waits for ipcnet's answer: a list of peers may wait
/// out a dropped peer's dial.
const CTL_MS: u64 = 15_000;
/// How long a join is tried for -- the connection to the member coming up
/// first -- and how long each try waits for its answer.
const JOIN_TOTAL_MS: u64 = 60_000;
const JOIN_TRY_MS: u64 = 3000;
/// How often the shell asks again a command the member could not take yet
/// -- a leader being chosen, a session being made -- and how long it waits
/// for its answer.
const ASK_RETRY_MS: u64 = 250;
/// How long a command is asked for, all its tries together.
const ASK_TOTAL_MS: u64 = 30_000;
const ANSWER_POLL_MS: u64 = 20;

/// A value under a lock of the kernel's, taken for a call.
pub struct Locked<T>(Mutex<T>);

impl<T> Locked<T> {
    pub fn new(value: T) -> Option<Locked<T>> {
        Some(Locked(Mutex::new(value)?))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock())
    }
}

/// The module's: its folder, and the member while there is one.
struct ModState {
    dir: Dir,
    shared: Arc<Shared>,
    task: Locked<Option<TaskHandle>>,
    /// An `init` or a `join` under way: the one at a time that may start a
    /// member.
    starting: AtomicBool,
}

/// The right to start a member, held for an `init` or a `join`.
struct Starting<'a>(&'a AtomicBool);

impl<'a> Starting<'a> {
    fn take(state: &'a ModState) -> Result<Starting<'a>, String> {
        state.starting.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Starting(&state.starting))
            .map_err(|_| String::from("another `cluster init` or `cluster join` is under way"))
    }
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/* Dropped in this order: the member's task, stopped and waited out -- its
   log closed with it -- then the command, waiting out a call of it */
struct Cluster {
    _cmd: ShellCommand,
    state: Arc<ModState>,
}

impl kmod::Module for Cluster {}

impl Drop for Cluster {
    fn drop(&mut self) {
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

fn init(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    let mut root = None;
    for (key, value) in params.words() {
        match (key, value) {
            ("conf", Some(path)) if root.is_none() && path.starts_with('/') && path.len() < 200 => root = Some(path),
            _ => {
                let _ = writeln!(out, "cluster: \"{}\" is not a parameter of cluster's -- conf=<folder> is, given \
                                       once", key);
                return Err(Error::InvalidValue);
            }
        }
    }
    let state = Arc::new(ModState {
        dir: Dir::new(root.unwrap_or(files::DEFAULT_DIR)),
        shared: Arc::new(Shared {
            stop: AtomicBool::new(false),
            requests: Locked::new(VecDeque::new()).ok_or(Error::NoMemory)?,
            view: Locked::new(None).ok_or(Error::NoMemory)?,
        }),
        task: Locked::new(None).ok_or(Error::NoMemory)?,
        starting: AtomicBool::new(false),
    });
    /* A member already: started over its log */
    match files::read_member(&state.dir) {
        Ok(Some(m)) => {
            if let Err(why) = start(&state, m, false) {
                let _ = writeln!(out, "cluster: {}", why);
            }
        }
        Ok(None) => {}
        Err(why) => {
            let _ = writeln!(out, "cluster: {}", why);
        }
    }
    let st = state.clone();
    let cmd = ShellCommand::register("cluster", HELP, move |args, out| {
        if let Err(problem) = run(&st, args, out) {
            let _ = writeln!(out, "cluster: {}", problem);
        }
    })?;
    Ok(Box::new(Cluster { _cmd: cmd, state }))
}

kmod::module!(name: "cluster", init_with_params: init);

/// The member started over its log: made new, with this host its one
/// voter, when `fresh`. The shell sees it before this returns.
fn start(state: &ModState, me: files::Member, fresh: bool) -> Result<(), String> {
    if state.task.with(|t| t.is_some()) {
        return Err(String::from("the member is running already"));
    }
    let files = KFiles::new(&state.dir)?;
    let log = if fresh {
        raft::DiskLog::bootstrap(files, LogConfig::default(), &[me.node], &[])
    } else {
        raft::DiskLog::open(files, LogConfig::default())
    }
    .map_err(|e| alloc::format!("the log in {}: {:?}", state.dir.path("raft"), e))?;
    let mut cfg = Config::new(me.node);
    cfg.seed = kcore::random::random_u64().unwrap_or(me.node);
    let nonce = kcore::random::random_u64().ok_or_else(|| String::from("no randomness for the member's start"))?;
    let mut rsm = Rsm::new(cfg, log, State::new(), nonce).map_err(|e| alloc::format!("the member: {:?}", e))?;
    if fresh {
        /* Its group's one voter: leading at once rather than at a timeout */
        let _ = rsm.campaign();
    }
    let member = Member::new(rsm, me)?;
    state.shared.stop.store(false, Ordering::Release);
    /* Shown before the task's first turn, so that a command right after
       this one finds the member */
    state.shared.view.with(|v| *v = Some(member.view(None)));
    let Some(task) = kcore::task::spawn_with("cluster/raft", (state.shared.clone(), member), member::run) else {
        state.shared.view.with(|v| *v = None);
        return Err(String::from("could not start the member's task"));
    };
    state.task.with(|t| *t = Some(task));
    Ok(())
}

/// A request of ipcnet's control mailbox: its answer's bytes, or why not.
fn ctl(tag: u32, data: &[u8]) -> Result<alloc::vec::Vec<u8>, String> {
    let mb = Mailbox::new(Limits { messages: 4, bytes: 4096 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    let to = kcore::ipc::lookup(control::NAME)
        .ok_or_else(|| String::from("ipcnet is not loaded -- `insmod ipcnet.ko`, and `ipcnet init` its hosts.toml"))?;
    let answer = mb.call(to, tag, data, Duration::from_millis(CTL_MS))
        .map_err(|_| String::from("ipcnet did not answer"))?;
    if answer.tag == control::TAG_REFUSED {
        return Err(alloc::format!("ipcnet: {}", String::from_utf8_lossy(&answer.data)));
    }
    Ok(answer.data)
}

/// Who this host is, as ipcnet has it.
fn identity() -> Result<Identity, String> {
    let data = ctl(control::TAG_ID, &[])?;
    Identity::decode(&data).ok_or_else(|| String::from("ipcnet said who this host is in no form it has"))
}

fn view(state: &ModState) -> Option<View> {
    state.shared.view.with(|v| v.clone())
}

/// `cmd`, as the member's registry answers it -- asked again while the
/// member cannot take it yet.
fn ask(state: &ModState, cmd: &Command) -> Result<Answer, String> {
    let mut last = String::from("no answer");
    let end = kcore::time::boot_time_ns() + ASK_TOTAL_MS * 1_000_000;
    while kcore::time::boot_time_ns() < end {
        if view(state).is_none_or(|v| v.stopped.is_some()) {
            return Err(String::from("this host is no member -- `cluster init`, or a join"));
        }
        let slot: Slot = Arc::new(Locked::new(None).ok_or_else(|| String::from("out of memory"))?);
        state.shared.requests.with(|q| q.push_back(Request { cmd: cmd.clone(), done: slot.clone() }));
        let until = (kcore::time::boot_time_ns() + (member::ANSWER_MS + 2000) * 1_000_000).min(end);
        let outcome = loop {
            if let Some(o) = slot.with(|s| s.take()) {
                break o;
            }
            if kcore::time::boot_time_ns() >= until {
                break Err(String::from("no answer -- ask again"));
            }
            kcore::task::sleep_ms(ANSWER_POLL_MS);
        };
        match outcome {
            Ok(a) => return Ok(a),
            Err(why) if why.ends_with("ask again") || why.ends_with("ask again in a moment") => {
                last = why;
                kcore::task::sleep_ms(ASK_RETRY_MS);
            }
            Err(why) => return Err(why),
        }
    }
    Err(last)
}

fn said(a: Answer, done: impl FnOnce(u64) -> String) -> Result<String, String> {
    match a {
        Answer::Done { node } => Ok(done(node)),
        Answer::Refused(why) => Err(String::from(why.says())),
    }
}

fn run(state: &ModState, args: &str, out: &mut Output) -> Result<(), String> {
    let words: alloc::vec::Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => status(state, out),
        ["init", name] => init_cluster(state, name, out),
        ["hosts"] => hosts(state, out),
        ["invite", host, ip, key] => invite(state, host, ip, key, out),
        ["join", token] => join_cluster(state, token, out),
        ["remove", host] => {
            let a = ask(state, &Command::Remove { now: kcore::time::wall_clock_secs(), host: String::from(*host) })?;
            let line = said(a, |node| alloc::format!("cluster: {} (node {}) removed, for good", host, node))?;
            let _ = writeln!(out, "{}", line);
            Ok(())
        }
        _ => Err(String::from(USAGE)),
    }
}

fn init_cluster(state: &ModState, name: &str, out: &mut Output) -> Result<(), String> {
    if !registry::valid_name(name) {
        return Err(alloc::format!("\"{}\" is not a cluster's name: lowercase letters, digits and -", name));
    }
    let _starting = Starting::take(state)?;
    if files::read_member(&state.dir)?.is_some() || view(state).is_some() {
        return Err(String::from("this host is a member of a cluster already"));
    }
    let id = identity()?;
    if id.ip == 0 {
        return Err(alloc::format!("{} has no address yet: the other hosts would have nowhere to reach this one",
                                  id.nic));
    }
    /* What an init or a join that did not finish left */
    files::clear_log(&state.dir)?;
    let me = files::Member { node: 1, cluster: id.cluster, name: id.name.clone(), hosts: alloc::vec::Vec::new() };
    start(state, me.clone(), true)?;
    /* The member file last: the mark that this host is a member */
    let made = ask(state, &Command::Init { now: kcore::time::wall_clock_secs(), cluster: id.cluster,
                                           name: String::from(name), host: id.name.clone(), key: id.key, ip: id.ip })
        .and_then(|a| said(a, |node| alloc::format!("cluster: {} made, {} its first host (node {}) -- `cluster \
                                                     invite` the next", name, id.name, node)))
        .and_then(|line| files::create_member(&state.dir, &me).map(|()| line));
    match made {
        Ok(line) => {
            let _ = writeln!(out, "{}", line);
            Ok(())
        }
        Err(why) => {
            /* Nobody else knows of the cluster yet: undone, for an init again */
            stop(state);
            match files::remove_member(&state.dir).and_then(|()| files::clear_log(&state.dir)) {
                Ok(()) => Err(why),
                Err(left) => Err(alloc::format!("{} -- and undoing it: {}", why, left)),
            }
        }
    }
}

fn invite(state: &ModState, host: &str, ip: &str, key: &str, out: &mut Output) -> Result<(), String> {
    let ip = parse_ip(ip).ok_or_else(|| alloc::format!("\"{}\" is not an IPv4 address", ip))?;
    let key = files::unhex::<32>(key)
        .ok_or_else(|| alloc::format!("\"{}\" is not a key: 64 hex digits, as `ipcnet id` says them", key))?;
    let v = view(state).ok_or_else(|| String::from("this host is no member -- `cluster init`, or a join"))?;
    let id = identity()?;
    let me = v.state.host(&id.name).ok_or_else(|| String::from("this host is not in the registry yet"))?;
    let cluster = v.state.cluster().ok_or_else(|| String::from("no cluster yet"))?.id;
    let mut secret = [0u8; 32];
    let invitation = kcore::random::random_u64().filter(|&i| i != 0);
    let (Some(invitation), true) = (invitation, kcore::random::fill_random(&mut secret)) else {
        return Err(String::from("no randomness for an invitation"));
    };
    let now = kcore::time::wall_clock_secs();
    let a = ask(state, &Command::Invite { now, id: invitation, host: String::from(host), ip, key,
                                           secret_hash: token::secret_hash(&secret), expires: now + INVITE_SECS })?;
    said(a, |_| String::new())?;
    let t = Token { cluster, member: id.name.clone(), ip: me.ipc(), port: id.port, key: id.key,
                    host: String::from(host), invitation, secret };
    let _ = writeln!(out, "cluster: {} invited from {} for {} minutes -- on it: cluster join {}", host, Ip(ip),
                     INVITE_SECS / 60, t.encode());
    Ok(())
}

/// `cluster join <token>`: this host into the cluster the token is of --
/// ipcnet made that cluster's and the inviting member pinned, the secret
/// proved to that member, and this host's member started over an empty log
/// that raft fills.
fn join_cluster(state: &ModState, text: &str, out: &mut Output) -> Result<(), String> {
    let t = Token::decode(text).ok_or_else(|| String::from("not a token: `cluster invite` prints one"))?;
    let _starting = Starting::take(state)?;
    if files::read_member(&state.dir)?.is_some() || view(state).is_some() {
        return Err(String::from("this host is a member of a cluster already"));
    }
    let id = identity()?;
    if id.name != t.host {
        return Err(alloc::format!("this host is {} to ipcnet, and the invitation is for {}", id.name, t.host));
    }
    /* What an init or a join that did not finish left */
    files::clear_log(&state.dir)?;
    ctl(control::TAG_JOIN, &t.cluster)?;
    let member = control::Peer { name: t.member.clone(), ip: t.ip, keys: alloc::vec![t.key], only: None };
    ctl(control::TAG_PEERS, &control::Peers { cluster: t.cluster, peers: alloc::vec![member] }.encode())?;
    if !identity()?.serving {
        return Err(String::from("ipcnet is not serving -- `ipcnet start`, then join again"));
    }
    let joined = ask_join(&t)?;
    /* In the cluster from here on: what fails now says the way back. The
       member file holds the hosts the join was answered with, which the
       member tells ipcnet and knows the others by until its log has caught
       up -- after a reboot too */
    let node = joined.node;
    let hosts = joined.hosts.len();
    let me = files::Member { node, cluster: t.cluster, name: id.name.clone(), hosts: joined.hosts };
    if let Err(why) = files::create_member(&state.dir, &me) {
        return Err(alloc::format!("{} joined the cluster as node {}, but {} -- `cluster remove {}` on a member, and \
                                   invite it again", id.name, node, why, id.name));
    }
    if let Err(why) = start(state, me, false) {
        return Err(alloc::format!("{} joined the cluster as node {}, but its member did not start: {} -- `rmmod \
                                   cluster` and `insmod` start it again", id.name, node, why));
    }
    let _ = writeln!(out, "cluster: {} joined as node {}, with {} hosts -- the registry follows from the leader",
                     id.name, node, hosts);
    Ok(())
}

/// The join asked of the inviting member, again until its connection is up
/// and it answers.
fn ask_join(t: &Token) -> Result<member::Joined, String> {
    let mb = Mailbox::new(Limits { messages: 8, bytes: 16 * 1024 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    let mut data = alloc::vec::Vec::with_capacity(40);
    data.extend_from_slice(&t.invitation.to_le_bytes());
    data.extend_from_slice(&t.secret);
    let to = alloc::format!("{}@{}", member::NAME, t.member);
    let end = kcore::time::boot_time_ns() + JOIN_TOTAL_MS * 1_000_000;
    let mut last = String::from("no answer");
    'tries: while kcore::time::boot_time_ns() < end {
        if let Err(e) = mb.send_to(&to, member::TAG_JOIN_REQ, 0, &data) {
            last = alloc::format!("{} out of reach: {}", t.member, e.name());
            kcore::task::sleep_ms(1000);
            continue;
        }
        let until = kcore::time::boot_time_ns() + JOIN_TRY_MS * 1_000_000;
        while kcore::time::boot_time_ns() < until {
            let Some(m) = mb.recv(Duration::from_millis(250)) else { continue };
            if m.tag != member::TAG_JOINED || m.data.is_empty() {
                continue;
            }
            if m.data[0] != 1 {
                let why = String::from_utf8_lossy(&m.data[1..]).into_owned();
                /* Not taken yet -- no leader, or one without its session:
                   asked again. Taken, and the answer lost with a leader:
                   made or not, which only the registry says */
                if why.ends_with("ask again in a moment") || why.starts_with("not the leader any more") {
                    last = alloc::format!("{}: {}", t.member, why);
                    kcore::task::sleep_ms(ASK_RETRY_MS);
                    continue 'tries;
                }
                if why.ends_with("ask again") {
                    return Err(alloc::format!("{}: {} -- the join may have been made all the same: `cluster hosts` \
                                               on a member says; if {} is there, `cluster remove {}` and invite it \
                                               again", t.member, why, t.host, t.host));
                }
                return Err(alloc::format!("{} refused the join: {}", t.member, why));
            }
            return member::Joined::decode(&m.data[1..])
                .ok_or_else(|| alloc::format!("{} answered the join in no form it has", t.member));
        }
        last = alloc::format!("no answer from {} yet", t.member);
    }
    Err(last)
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
        let _ = writeln!(out, "cluster: this host is no member -- `cluster init <name>`, or a join");
        return Ok(());
    };
    if let Some(why) = &v.stopped {
        let _ = writeln!(out, "cluster: the member stopped: {}", why);
    }
    if v.state.cluster().is_some() && v.state.host_of_node(v.node).is_none() {
        let _ = writeln!(out, "cluster: this host (node {}) is removed from the cluster: no member takes it in again \
                               -- `rmmod cluster`; its folder goes with a new install", v.node);
    }
    let name = v.state.cluster().map_or("(none yet)", |c| c.name.as_str());
    let leader = v.state.host_of_node(v.leader).map_or("none", |h| h.name.as_str());
    let _ = writeln!(out, "cluster: {} -- node {}, {}, term {}, leader {}, applied {}, {} hosts, {} voters, {} \
                           learners, {} messages refused", name, v.node, role_name(v.role), v.term, leader, v.applied,
                     v.state.hosts().len(), v.membership.voters.len(), v.membership.learners.len(), v.refused);
    Ok(())
}

fn hosts(state: &ModState, out: &mut Output) -> Result<(), String> {
    let v = view(state).ok_or_else(|| String::from("this host is no member -- `cluster init`, or a join"))?;
    for h in v.state.hosts() {
        let vote = if v.membership.voters.contains(&h.node) {
            "voter"
        } else if v.membership.learners.contains(&h.node) {
            "learner"
        } else {
            "joining"
        };
        let key = h.keys.first().map(|k| files::hex(&k[..8])).unwrap_or_default();
        let _ = writeln!(out, "  {:<16} node {:<4} {:<8} {:<15} key {}...{}", h.name, h.node, vote,
                         alloc::format!("{}", Ip(h.ipc())), key,
                         if h.node == v.leader { " (leader)" } else { "" });
    }
    for i in v.state.invitations() {
        let _ = writeln!(out, "  {:<16} invited  from {}", i.host, Ip(i.ip));
    }
    if !v.state.removed().is_empty() {
        let _ = writeln!(out, "  {} hosts removed", v.state.removed().len());
    }
    Ok(())
}

/// An IPv4 address in dotted form, host order.
fn parse_ip(text: &str) -> Option<u32> {
    let mut parts = text.split('.');
    let mut ip = 0u32;
    for _ in 0..4 {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u32 = p.parse().ok()?;
        if v > 255 {
            return None;
        }
        ip = (ip << 8) | v;
    }
    if parts.next().is_some() || ip == 0 {
        return None;
    }
    Some(ip)
}

/// An address as `a.b.c.d`.
struct Ip(u32);

impl core::fmt::Display for Ip {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}
