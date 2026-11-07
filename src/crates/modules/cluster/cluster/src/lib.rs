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
//! this host is (files.rs). The member is raftmember's, a task of its own
//! with the registry's hooks (member.rs), which the shell hands its
//! commands to.

extern crate alloc;

mod files;
mod member;
mod provider;
mod token;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use ipcwire::control::{self, Identity};
use kcore::cmd::{Command as ShellCommand, Output};
use kcore::error::Error;
use kcore::ipc::{Limits, Mailbox};
use kcore::klog;
use kcore::task::TaskHandle;
use kcore::time::Duration;
use raft::{Config, LogConfig, Role, Rsm};
use raftmember::{KFiles, Locked};
use registry::{Answer, Command, Role as AddrRole, State};

use files::Dir;
use member::{Shared, View};
use token::Token;

const HELP: &str = "cluster [init | invite | join | hosts | remove | ...] - this host in its cluster's registry";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: cluster                          this member: its node, role, leader, what it applied\n\
                     \x20      cluster init <name>             a cluster of this host alone, named <name>\n\
                     \x20      cluster invite <host> <ip> <key>   a token for `cluster join` on <host>, from <ip>,\n\
                     \x20                                     with <key> -- what `ipcnet id` says there -- once\n\
                     \x20      cluster join <token>            on the host invited: into the cluster, as the token says\n\
                     \x20      cluster hosts                   every host: node, vote, addresses, key\n\
                     \x20      cluster remove <host>           a host out, for good\n\
                     \x20      cluster revoke <host>           its invitation taken back, unused\n\
                     \x20      cluster maintenance <host> on|off   a host being worked on, or back\n\
                     \x20      cluster address <host> <ipc|underlay|public> <ip|none>   a host's address for a role\n\
                     \x20      cluster rekey                   this host moved to a new key, no connection lost\n\
                     \x20      cluster recover [<cluster>]     on the last host standing: what it does, then -- the\n\
                     \x20                                     cluster's name given -- this host the one voter";

/// How long an invitation is good for, by the leader's clock.
const INVITE_SECS: u64 = 15 * 60;
/// How long the shell waits for ipcnet's answer: a list of peers may wait
/// out a dropped peer's dial.
const CTL_MS: u64 = 15_000;
/// How long a join is tried for -- the connection to the member coming up
/// first -- and how long each try waits for its answer.
const JOIN_TOTAL_MS: u64 = 60_000;
const JOIN_TRY_MS: u64 = 3000;
/// How often a join is asked again while the member could not take it yet
/// -- a leader being chosen, a session being made.
const ASK_RETRY_MS: u64 = 250;
/// How long `recover` waits for any other voter to answer its probe.
const PROBE_MS: u64 = 3000;
const ANSWER_POLL_MS: u64 = 20;
/// What a command asked of a host that is no member is told.
const NO_MEMBER: &str = "this host is no member -- `cluster init`, or a join";

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

/* Dropped in this order: the provider, waiting out a call of it; the
   member's task, stopped and waited out -- its log closed with it -- then
   the command, waiting out a call of it */
struct Cluster {
    _cmd: ShellCommand,
    provider: Option<kcore::mgmt::Provider>,
    state: Arc<ModState>,
}

impl kmod::Module for Cluster {}

impl Drop for Cluster {
    fn drop(&mut self) {
        drop(self.provider.take());
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
        shared: Arc::new(Shared::new().ok_or(Error::NoMemory)?),
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
    let provider = match provider::register(state.clone()) {
        Ok(p) => Some(p),
        Err(s) => {
            let _ = writeln!(out, "cluster: not on the management bus: {}", s.name());
            None
        }
    };
    Ok(Box::new(Cluster { _cmd: cmd, provider, state }))
}

kmod::module!(name: "cluster", init_with_params: init);

/// The member started over its log: made new, with this host its one
/// voter, when `fresh`. The shell sees it before this returns.
fn start(state: &ModState, me: files::Member, fresh: bool) -> Result<(), String> {
    if state.task.with(|t| t.is_some()) {
        return Err(String::from("the member is running already"));
    }
    let files = KFiles::new(&state.dir.root, "raft")?;
    let log = if fresh {
        raft::DiskLog::bootstrap(files, LogConfig::default(), &[me.node], &[])
    } else {
        raft::DiskLog::open(files, LogConfig::default())
    }
    .map_err(|e| alloc::format!("the log in {}: {:?}", state.dir.path("raft"), e))?;
    run_member(state, me, log, fresh)
}

/// The member started over `log`, standing for election at once when
/// `campaign` -- its group's one voter.
fn run_member(state: &ModState, me: files::Member, log: member::Log, campaign: bool) -> Result<(), String> {
    if state.task.with(|t| t.is_some()) {
        return Err(String::from("the member is running already"));
    }
    let mut cfg = Config::new(me.node);
    cfg.seed = kcore::random::random_u64().unwrap_or(me.node);
    let nonce = kcore::random::random_u64().ok_or_else(|| String::from("no randomness for the member's start"))?;
    let mut rsm = Rsm::new(cfg, log, State::new(), nonce).map_err(|e| alloc::format!("the member: {:?}", e))?;
    if campaign {
        /* Its group's one voter: leading at once rather than at a timeout */
        let _ = rsm.campaign();
    }
    let member = member::new(rsm, me)?;
    state.shared.stop.store(false, Ordering::Release);
    /* Shown before the task's first turn, so that a command right after
       this one finds the member */
    state.shared.view.with(|v| *v = Some(member.view(None)));
    let Some(task) = kcore::task::spawn_with("cluster/raft", (state.shared.clone(), member),
                                             raftmember::run::<member::Registry>) else {
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
    state.shared.view()
}

/// `cmd`, as the member's registry answers it -- asked again while the
/// member cannot take it yet.
fn ask(state: &ModState, cmd: &Command) -> Result<Answer, String> {
    raftmember::ask(&state.shared, cmd, NO_MEMBER)
}

fn said(a: Answer, done: impl FnOnce(u64) -> String) -> Result<String, String> {
    match a {
        Answer::Done { node } => Ok(done(node)),
        Answer::Refused(why) => Err(String::from(why.says())),
    }
}

/// A command's answer as a result: the node it names, or why it was refused.
fn done(a: Answer) -> Result<u64, String> {
    match a {
        Answer::Done { node } => Ok(node),
        Answer::Refused(why) => Err(String::from(why.says())),
    }
}

fn now() -> u64 {
    kcore::time::wall_clock_secs()
}

fn member_view(state: &ModState) -> Result<View, String> {
    view(state).ok_or_else(|| String::from(NO_MEMBER))
}

/* ---- what the shell and the bus both ask: the commands, each a value ---- */

fn op_remove(state: &ModState, host: &str) -> Result<u64, String> {
    done(ask(state, &Command::Remove { now: now(), host: String::from(host) })?)
}

fn op_maintenance(state: &ModState, host: &str, on: bool) -> Result<u64, String> {
    done(ask(state, &Command::Maintenance { now: now(), host: String::from(host), on })?)
}

fn op_address(state: &ModState, host: &str, role: AddrRole, ip: u32) -> Result<u64, String> {
    done(ask(state, &Command::Address { now: now(), host: String::from(host), role, ip })?)
}

/// The open invitation of `host`, taken back.
fn op_revoke(state: &ModState, host: &str) -> Result<(), String> {
    let v = member_view(state)?;
    let inv = v.state.invitations().iter().find(|i| i.host == host)
        .ok_or_else(|| alloc::format!("no open invitation of {}", host))?;
    done(ask(state, &Command::Revoke { now: now(), invitation: inv.id })?).map(|_| ())
}

/// `host` invited from `ip` with `key`: the token, which `cluster join`
/// takes there -- shown once, and kept nowhere.
fn op_invite(state: &ModState, host: &str, ip: u32, key: [u8; 32]) -> Result<String, String> {
    let v = member_view(state)?;
    let id = identity()?;
    let me = v.state.host(&id.name).ok_or_else(|| String::from("this host is not in the registry yet"))?;
    let cluster = v.state.cluster().ok_or_else(|| String::from("no cluster yet"))?.id;
    let mut secret = [0u8; 32];
    let invitation = kcore::random::random_u64().filter(|&i| i != 0);
    let (Some(invitation), true) = (invitation, kcore::random::fill_random(&mut secret)) else {
        return Err(String::from("no randomness for an invitation"));
    };
    let now = now();
    done(ask(state, &Command::Invite { now, id: invitation, host: String::from(host), ip, key,
                                        secret_hash: token::secret_hash(&secret), expires: now + INVITE_SECS })?)?;
    let t = Token { cluster, member: id.name.clone(), ip: me.ipc(), port: id.port, key: id.key,
                    host: String::from(host), invitation, secret };
    Ok(t.encode())
}

/// This host moved to a new key, from wherever an earlier rekey stopped:
/// a key staged by ipcnet, the registry told of it -- every member's
/// ipcnet pinning both -- ipcnet switched to it, and the old one dropped
/// and retired. No connection is lost: one made with the old key goes on
/// until it ends. The old key and the new.
fn op_rekey(state: &ModState) -> Result<([u8; 32], [u8; 32]), String> {
    let _one = Starting::take(state)?;
    let v = member_view(state)?;
    let id = identity()?;
    let me = v.state.host(&id.name).ok_or_else(|| String::from("this host is not in the registry yet"))?;
    let (host, cur) = (id.name.clone(), id.key);
    let add = |key| ask(state, &Command::AddKey { now: now(), host: host.clone(), key }).and_then(done);
    let drop = |key| ask(state, &Command::DropKey { now: now(), host: host.clone(), key }).and_then(done);
    /* The registry's keys in the order they came: a key added is the last */
    let registered = match me.keys.as_slice() {
        [k] if *k == cur => None,
        [old, new] if *new == cur => {
            /* Switched already: the old one is left to drop */
            let old = *old;
            drop(old)?;
            return Ok((old, cur));
        }
        [k, other] if *k == cur && id.next == Some(*other) => Some(*other),
        [k, other] if *k == cur => {
            /* Registered, and the key staged for it is gone: taken back */
            drop(*other)?;
            None
        }
        _ => {
            return Err(alloc::format!("ipcnet's key, {}..., is none of this host's in the registry: the other \
                                       hosts refuse it -- `cluster remove {}` on a member, and invite it again",
                                      files::hex(&cur[..8]), host));
        }
    };
    let new = match registered {
        Some(k) => k,
        None => {
            let k = key_of(&ctl(control::TAG_KEY_STAGE, &[])?)?;
            add(k)?;
            k
        }
    };
    let used = key_of(&ctl(control::TAG_KEY_USE, &[])?)?;
    if used != new {
        return Err(alloc::format!("ipcnet moved to {}..., where {}... was staged", files::hex(&used[..8]),
                                  files::hex(&new[..8])));
    }
    drop(cur)?;
    Ok((cur, new))
}

fn key_of(data: &[u8]) -> Result<[u8; 32], String> {
    data.try_into().map_err(|_| String::from("ipcnet answered with no key"))
}

/// The other voters of `hosts` that answer a probe within PROBE_MS, by
/// name -- each answer from the host it names, as ipcnet has it.
fn probe(hosts: &[(String, u64)]) -> Result<alloc::vec::Vec<String>, String> {
    let mb = Mailbox::new(Limits { messages: 32, bytes: 8192 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    for (name, _) in hosts {
        let _ = mb.send_to(&alloc::format!("{}@{}", member::NAME, name), member::TAG_PROBE, 0, &[]);
    }
    let end = kcore::time::boot_time_ns() + PROBE_MS * 1_000_000;
    let mut answered: alloc::vec::Vec<String> = alloc::vec::Vec::new();
    while kcore::time::boot_time_ns() < end && answered.len() < hosts.len() {
        let Some(m) = mb.recv(Duration::from_millis(ANSWER_POLL_MS)) else { continue };
        if m.tag != member::TAG_PROBED || m.data.len() != 17 {
            continue;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&m.data[..8]);
        let node = u64::from_le_bytes(b);
        let from = kcore::ipc::peer_name(m.from);
        if let Some((name, _)) = hosts.iter().find(|(n, nd)| *nd == node && from.as_deref() == Some(n.as_str())) {
            if !answered.contains(name) {
                answered.push(name.clone());
            }
        }
    }
    Ok(answered)
}

/// The last host standing made its cluster's one voter (plans/cluster.md,
/// "When the majority is gone"): refused while it follows a leader or any
/// other voter answers; said first, done once the cluster's name is given.
/// Unsafe by design -- what the lost hosts had committed and this one had
/// not is gone -- so the lost hosts are removed for good, their keys with
/// them: none of them comes back with a history that went another way.
fn recover(state: &ModState, confirm: Option<&str>, out: &mut Output) -> Result<(), String> {
    let _one = Starting::take(state)?;
    let v = member_view(state)?;
    let cluster = v.state.cluster().ok_or_else(|| String::from("no cluster yet"))?.clone();
    let me = v.state.host_of_node(v.node).map(|h| h.name.clone())
        .ok_or_else(|| String::from("this host is not in the registry"))?;
    if v.leader != raft::NONE {
        let leader = v.state.host_of_node(v.leader).map_or("a member", |h| h.name.as_str());
        return Err(alloc::format!("this member follows a leader, {}: the majority answers -- recover is for a \
                                   cluster whose majority is gone for good", leader));
    }
    let others: alloc::vec::Vec<(String, u64)> = v.state.hosts().iter().filter(|h| h.node != v.node)
        .map(|h| (h.name.clone(), h.node)).collect();
    let voters: alloc::vec::Vec<(String, u64)> = others.iter().filter(|(_, n)| v.membership.voters.contains(n))
        .cloned().collect();
    let answering = probe(&voters)?;
    if !answering.is_empty() {
        return Err(alloc::format!("{} answers: the majority may not be gone -- recover refused",
                                  answering.join(", ")));
    }
    let names: alloc::vec::Vec<&str> = others.iter().map(|(n, _)| n.as_str()).collect();
    let Some(given) = confirm else {
        let _ = writeln!(out, "cluster: recover makes {} (node {}) the one voter of cluster {}, and removes every \
                               other host for good -- their keys refused from then on; whatever they had committed \
                               that {} had not is gone. No other voter answers. To go ahead: cluster recover {}", me,
                         v.node, cluster.name, me, cluster.name);
        let _ = writeln!(out, "cluster: the hosts removed: {}",
                         if names.is_empty() { String::from("none") } else { names.join(", ") });
        return Ok(());
    };
    if given != cluster.name {
        return Err(alloc::format!("\"{}\" is not this cluster's name -- it is {}", given, cluster.name));
    }
    let member = files::read_member(&state.dir)?.ok_or_else(|| String::from("this host has no member file"))?;
    stop(state);
    let stopped = |why: String| alloc::format!("{} -- the member is stopped: `rmmod cluster` and `insmod` start it \
                                               again", why);
    let files = KFiles::new(&state.dir.root, "raft").map_err(stopped)?;
    let log = raft::DiskLog::open(files, LogConfig::default())
        .map_err(|e| stopped(alloc::format!("the log in {}: {:?}", state.dir.path("raft"), e)))?;
    let (log, forced) = match raft::recover::force_alone(log, member.node) {
        Ok(f) => f,
        Err(e) => {
            /* Refused before writing, or a write that failed: the member
               started again over what the folder holds */
            let again = start(state, member, false).err().map(|w| alloc::format!(", and the member not started \
                                                                                 again: {}", w)).unwrap_or_default();
            return Err(alloc::format!("recover refused: {:?}{}", e, again));
        }
    };
    klog!(0, "cluster: recover: {} (node {}) alone, {:?} taken out, the log committed from {} to {}", me,
          member.node, forced.removed, forced.committed_before, forced.last);
    run_member(state, member, log, true)?;
    let mut removed = alloc::vec::Vec::new();
    for (name, _) in &others {
        match op_remove(state, name) {
            Ok(_) => removed.push(name.as_str()),
            Err(why) => {
                let _ = writeln!(out, "cluster: {} not removed: {} -- `cluster remove {}` again", name, why, name);
            }
        }
    }
    let _ = writeln!(out, "cluster: {} is cluster {}'s one voter now; removed for good: {}", me, cluster.name,
                     if removed.is_empty() { String::from("none") } else { removed.join(", ") });
    Ok(())
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
            let node = op_remove(state, host)?;
            let _ = writeln!(out, "cluster: {} (node {}) removed, for good", host, node);
            Ok(())
        }
        ["revoke", host] => {
            op_revoke(state, host)?;
            let _ = writeln!(out, "cluster: {}'s invitation taken back", host);
            Ok(())
        }
        ["maintenance", host, on @ ("on" | "off")] => {
            let on = *on == "on";
            op_maintenance(state, host, on)?;
            let _ = writeln!(out, "cluster: {} {}", host, if on { "being worked on" } else { "back" });
            Ok(())
        }
        ["address", host, role, ip] => {
            let role = AddrRole::from_name(role)
                .ok_or_else(|| alloc::format!("\"{}\" is not a role: ipc, underlay or public", role))?;
            let ip = match *ip {
                "none" => 0,
                text => parse_ip(text).ok_or_else(|| alloc::format!("\"{}\" is not an IPv4 address", text))?,
            };
            op_address(state, host, role, ip)?;
            if ip == 0 {
                let _ = writeln!(out, "cluster: {}'s {} address taken away", host, role.name());
            } else {
                let _ = writeln!(out, "cluster: {}'s {} address is {}", host, role.name(), Ip(ip));
            }
            Ok(())
        }
        ["rekey"] => {
            let (old, new) = op_rekey(state)?;
            let _ = writeln!(out, "cluster: this host's key is {}... now; the old one, {}..., retired -- refused from \
                                   now on, the connections made with it going on until they end",
                             files::hex(&new[..8]), files::hex(&old[..8]));
            Ok(())
        }
        ["recover"] => recover(state, None, out),
        ["recover", name] => recover(state, Some(name), out),
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
    let token = op_invite(state, host, ip, key)?;
    let _ = writeln!(out, "cluster: {} invited from {} for {} minutes -- on it: cluster join {}", host, Ip(ip),
                     INVITE_SECS / 60, token);
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
    let v = view(state).ok_or_else(|| String::from(NO_MEMBER))?;
    for h in v.state.hosts() {
        let vote = if v.membership.voters.contains(&h.node) {
            "voter"
        } else if v.membership.learners.contains(&h.node) {
            "learner"
        } else {
            "joining"
        };
        let key = h.keys.first().map(|k| files::hex(&k[..8])).unwrap_or_default();
        let mut more = String::new();
        if h.node == v.leader {
            more.push_str(" (leader)");
        }
        if h.maintenance {
            more.push_str(" maintenance");
        }
        if let Some(next) = h.keys.get(1) {
            let _ = write!(more, " moving to key {}...", files::hex(&next[..8]));
        }
        for a in h.addrs.iter().skip(1) {
            let _ = write!(more, " {} {}", a.role.name(), Ip(a.ip));
        }
        let _ = writeln!(out, "  {:<16} node {:<4} {:<8} {:<15} key {}...{}", h.name, h.node, vote,
                         alloc::format!("{}", Ip(h.ipc())), key, more);
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
