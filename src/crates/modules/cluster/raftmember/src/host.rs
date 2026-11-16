//! The member in its module: what every group's module does to start, stop
//! and recover its member, written once -- cluster.ko's, nbs-mds.ko's and
//! the next group's alike.
//!
//! - **A runner** ([`Runner`]): the member's task, the view the shell
//!   reads, the one `init`, join, offer or recover at a time that may start
//!   it (`Starting`), and why the member did not start as the module loaded,
//!   where it did not -- said by the module's status, not "no member".
//! - **The folder** a group keeps -- `member`, the module's mark that this
//!   host is a member, written last and in a form of its own; `raft/`, the
//!   log; `.sync` -- and with V1a **`begun`**, written and synced once the
//!   log's first segment is on the medium and before the member's task is
//!   started ([`open_log`]): the member cannot have voted or acknowledged a
//!   word without it. A member file beside a `raft/` that holds no log is
//!   then either a host that took its group and never ran -- no `begun` --
//!   or a member whose log is gone, its votes and what it acknowledged
//!   forgotten, which is never started as the member it was: raft never
//!   counts the vote of a member that forgot its log. (Both started, the
//!   second as the old incarnation; and with the mark written before the
//!   log, an open that failed left a host no load would start again -- the
//!   review of V1a.) `recover` holds a log to the same.
//! - **A probe** ([`Core::answer_probe`], [`probe`]): whether a member is
//!   there, which `recover` asks every other voter first.
//! - **`recover`** ([`recover`]): the last member standing made its group's
//!   one voter (`raft::recover::force_alone`), the others removed for good --
//!   refused while it follows a leader or any other voter answers.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::error::Error;
use kcore::ipc::{Limits, Mailbox, Message};
use kcore::klog;
use kcore::task::TaskHandle;
use kcore::time::{boot_time_ns, Duration};
use raft::log::{DiskLog, LogConfig};
use raft::rsm::StateMachine;
use raft::{Config, Role, Rsm};

use crate::{catch_up, snapshot_limit, App, Core, KFiles, Locked, Log, Member, Shared, View};

/// The file a group's module marks this host a member by, in its folder.
pub const MEMBER_FILE: &str = "member";
/// The mark that the member began its log: beside `member` and `raft/`.
pub const BEGUN_FILE: &str = "begun";
/// The log's folder, under the group's.
pub const LOG_DIR: &str = "raft";
/// How long `recover` waits for any other voter to answer its probe.
const PROBE_MS: u64 = 3000;
const ANSWER_POLL_MS: u64 = 20;
/// A probe's answer: the member's node, its term and its role's byte.
const PROBED_BYTES: usize = 17;

/// The right to start a member, held for an `init`, a join, an offer or a
/// recover: one at a time.
pub struct Starting<'a>(&'a AtomicBool);

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The member's task, and what the shell sees of it.
pub struct Runner<A: App> {
    pub shared: Arc<Shared<A>>,
    task: Locked<Option<TaskHandle>>,
    starting: AtomicBool,
    /// Why the member did not start as the module loaded, where it did not.
    held: Locked<Option<String>>,
}

impl<A: App> Runner<A> {
    pub fn new() -> Option<Runner<A>> {
        Some(Runner {
            shared: Arc::new(Shared::new()?),
            task: Locked::new(None)?,
            starting: AtomicBool::new(false),
            held: Locked::new(None)?,
        })
    }

    /// The right to start the member, where nothing else holds it: `busy`
    /// says what does.
    pub fn starting(&self, busy: &str) -> Result<Starting<'_>, String> {
        self.starting.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Starting(&self.starting))
            .map_err(|_| String::from(busy))
    }

    /// Whether the member's task is there.
    pub fn running(&self) -> bool {
        self.task.with(|t| t.is_some())
    }

    /// The member's task stopped and waited out -- taken from its lock
    /// first, and dropped off it -- and the shell shown no member.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        let task = self.task.with(|t| t.take());
        drop(task);
        self.shared.view.with(|v| *v = None);
    }

    /// `member`'s task started, as `task` -- shown to the shell before this
    /// returns, so that a command right after it finds the member.
    pub fn spawn(&self, task: &str, member: Member<A>) -> Result<(), String>
    where
        Member<A>: Send + 'static,
        Shared<A>: Send + Sync + 'static,
    {
        if self.running() {
            return Err(String::from("the member is running already"));
        }
        self.held.with(|h| *h = None);
        self.shared.stop.store(false, Ordering::Release);
        self.shared.view.with(|v| *v = Some(member.view(None)));
        let Some(handle) = kcore::task::spawn_with(task, (self.shared.clone(), member), crate::run::<A>) else {
            self.shared.view.with(|v| *v = None);
            return Err(String::from("could not start the member's task"));
        };
        self.task.with(|t| *t = Some(handle));
        Ok(())
    }

    /// Why the member did not start as the module loaded: kept to be said.
    pub fn hold(&self, why: &str) {
        self.held.with(|h| *h = Some(String::from(why)));
    }

    pub fn held(&self) -> Option<String> {
        self.held.with(|h| h.clone())
    }

    /// What is said of this host's member where there is none: why it did
    /// not start, where it did not -- or `none`.
    pub fn no_member(&self, none: &str) -> String {
        match self.held() {
            Some(why) => alloc::format!("this host's member did not start: {}", why),
            None => String::from(none),
        }
    }

    pub fn view(&self) -> Option<View<A>> {
        self.shared.view()
    }
}

/* ---- the folder ---- */

fn path(dir: &str, name: &str) -> String {
    alloc::format!("{}/{}", dir.trim_end_matches('/'), name)
}

/// Whether `dir` has a file `name`: told by the folder's listing, so that a
/// file that cannot be read is no file not there.
fn present(dir: &str, name: &str) -> Result<bool, String> {
    let dir = dir.trim_end_matches('/');
    let at = if dir.is_empty() { "/" } else { dir };
    match kcore::fs::list(at) {
        Ok(entries) => Ok(entries.iter().any(|e| !e.dir && e.name == name.as_bytes())),
        Err(Error::NotFound) => Ok(false),
        Err(_) => Err(alloc::format!("cannot list {}", at)),
    }
}

/// The log in `<dir>/raft` gone, where the member had begun it: refused,
/// saying `remedy`, before anything is written there -- an open would begin
/// a segment, which the next look would take for a log. Read only.
fn held_to_begun(dir: &str, files: &mut KFiles, remedy: &str) -> Result<bool, String> {
    let begun = present(dir, BEGUN_FILE)?;
    let holds = raft::log::holds_log(files).map_err(|e| alloc::format!("cannot read {}: {:?}", files.dir(), e))?;
    if begun && !holds {
        return Err(alloc::format!("{} holds no log, and this host's member had begun one ({}): what it voted for and \
                                   acknowledged is gone, and it is never started as that member again -- {}",
                                  files.dir(), path(dir, BEGUN_FILE), remedy));
    }
    Ok(begun)
}

/// A file made with `bytes`, once and synced: refused where there is one.
/// A write that failed part way leaves none.
pub fn create_once(path: &str, bytes: &[u8]) -> Result<(), String> {
    match kcore::fs::create(path, bytes) {
        Ok(()) => Ok(()),
        Err(Error::Busy) => Err(alloc::format!("{} is there already", path)),
        Err(_) => {
            let _ = kcore::fs::remove(path);
            Err(alloc::format!("cannot write {}", path))
        }
    }
}

/// A file taken away, or none there.
pub fn remove_file(path: &str) -> Result<(), String> {
    match kcore::fs::remove(path) {
        Ok(()) | Err(Error::NotFound) => Ok(()),
        Err(_) => Err(alloc::format!("cannot remove {}", path)),
    }
}

/// The member file of the group's folder `dir`, as bytes of at most `max`:
/// None where there is none.
pub fn read_member_file(dir: &str, max: usize) -> Result<Option<Vec<u8>>, String> {
    let path = path(dir, MEMBER_FILE);
    match kcore::fs::read(&path, max) {
        Ok(b) => Ok(Some(b)),
        Err(Error::NotFound) => Ok(None),
        Err(_) => Err(alloc::format!("cannot read {}", path)),
    }
}

/// The member file written, once and synced: the mark that this host is a
/// member -- the folder made where it is not.
pub fn create_member_file(dir: &str, bytes: &[u8]) -> Result<(), String> {
    crate::create_dirs(dir.trim_end_matches('/'));
    create_once(&path(dir, MEMBER_FILE), bytes)
}

/// The member file taken away: an `init` undone.
pub fn remove_member_file(dir: &str) -> Result<(), String> {
    remove_file(&path(dir, MEMBER_FILE))
}

/// Every file of `raft/` removed, and `begun`: what an `init`, a join or an
/// offer that did not finish left, which no member file names -- never a
/// member's log.
pub fn clear_log(dir: &str) -> Result<(), String> {
    if present(dir, MEMBER_FILE)? {
        return Err(alloc::format!("{} names a member: its log is not cleared", path(dir, MEMBER_FILE)));
    }
    KFiles::new(dir, LOG_DIR)?.clear()?;
    remove_file(&path(dir, BEGUN_FILE))
}

/// The log in `<dir>/raft`, for the member of node `node` to start over:
/// made -- a group of that node alone, committed -- when `fresh`, opened
/// otherwise. A log gone -- `raft/` holding no segment begun whole and no
/// snapshot -- beside a `begun` is refused, saying `remedy`: the member had
/// run over it, and starting it as the member it was would count a vote it
/// forgot. `begun` is written and synced once the log is open -- its first
/// segment on the medium -- where it is not there yet: before the member
/// runs, and so before it says a word.
pub fn open_log(dir: &str, node: u64, fresh: bool, remedy: &str) -> Result<Log, String> {
    let mut files = KFiles::new(dir, LOG_DIR)?;
    let begun = if fresh { present(dir, BEGUN_FILE)? } else { held_to_begun(dir, &mut files, remedy)? };
    let log = if fresh {
        DiskLog::bootstrap(files, LogConfig::default(), &[node], &[])
    } else {
        DiskLog::open(files, LogConfig::default())
    };
    let log = log.map_err(|e| alloc::format!("the log in {}: {:?}", path(dir, LOG_DIR), e))?;
    if !begun {
        create_once(&path(dir, BEGUN_FILE), b"")?;
    }
    Ok(log)
}

/// raft's member of node `node` over `log`, what the log holds committed
/// applied ([`catch_up`]), its snapshots past `floor` bytes of entries
/// where one is given; standing for election at once when `campaign` --
/// its group's one voter, leading at once rather than at a timeout.
pub fn member_rsm<S: StateMachine + Default>(log: Log, node: u64, campaign: bool, floor: Option<u64>)
    -> Result<Rsm<Log, S>, String>
{
    let mut cfg = Config::new(node);
    cfg.seed = kcore::random::random_u64().unwrap_or(node);
    cfg.max_snapshot_bytes = snapshot_limit();
    let nonce = kcore::random::random_u64().ok_or_else(|| String::from("no randomness for the member's start"))?;
    let mut rsm = Rsm::new(cfg, log, S::default(), nonce).map_err(|e| alloc::format!("the member: {:?}", e))?;
    if let Some(bytes) = floor {
        rsm.set_snapshot_floor(bytes);
    }
    catch_up(&mut rsm).map_err(|e| alloc::format!("the member: {}", e))?;
    if campaign {
        let _ = rsm.campaign();
    }
    Ok(rsm)
}

/* ---- the probe ---- */

impl<A: App> Core<A> {
    /// A member asking whether this one is there -- what `recover` asks
    /// every other voter first: answered with this member's node, its term
    /// and its role, as `tag`, to a host of the group alone.
    pub fn answer_probe(&mut self, app: &A, m: &Message, tag: u32) {
        if self.sender_node(app, m).is_none() {
            self.note_refused();
            return;
        }
        let mut data = Vec::with_capacity(PROBED_BYTES);
        data.extend_from_slice(&self.node().to_le_bytes());
        data.extend_from_slice(&self.rsm().node().term().to_le_bytes());
        data.push(match self.role() {
            Role::Leader => 2,
            Role::Follower => 1,
            Role::Candidate | Role::PreCandidate => 0,
        });
        let _ = self.mailbox().send(m.from, tag, m.reference, &data);
    }
}

/// The members of `hosts` -- each a host and its node -- whose member at
/// `name` answers a probe within PROBE_MS, by host: each answer from the
/// host it names, of the node it is.
pub fn probe(name: &str, tags: (u32, u32), hosts: &[(String, u64)]) -> Result<Vec<String>, String> {
    let mb = Mailbox::new(Limits { messages: 32, bytes: 8192 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    for (host, _) in hosts {
        let _ = mb.send_to(&alloc::format!("{}@{}", name, host), tags.0, 0, &[]);
    }
    let end = boot_time_ns() + PROBE_MS * 1_000_000;
    let mut answered: Vec<String> = Vec::new();
    while boot_time_ns() < end && answered.len() < hosts.len() {
        let Some(m) = mb.recv(Duration::from_millis(ANSWER_POLL_MS)) else { continue };
        if m.tag != tags.1 || m.data.len() != PROBED_BYTES {
            continue;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&m.data[..8]);
        let node = u64::from_le_bytes(b);
        let from = kcore::ipc::peer_name(m.from);
        if let Some((host, _)) = hosts.iter().find(|(h, n)| *n == node && from.as_deref() == Some(h.as_str())) {
            if !answered.contains(host) {
                answered.push(host.clone());
            }
        }
    }
    Ok(answered)
}

/* ---- recover ---- */

/// What a group's `recover` is told of it.
#[derive(Clone)]
pub struct Recovery {
    /// What to do where this member's own log is gone: said as `open_log`
    /// says it.
    pub remedy: String,
    /// The module, for `rmmod`.
    pub module: &'static str,
    /// What the group's name is a name of -- "cluster" -- and the group in
    /// what is said, with its article: "a cluster", "an NBS cluster".
    pub what: &'static str,
    pub an: &'static str,
    /// The group's name: what confirms.
    pub group: String,
    /// This host's member: its host and node.
    pub me: (String, u64),
    /// Every other member: its host and node.
    pub others: Vec<(String, u64)>,
    /// raft's voters, as this member has them.
    pub voters: Vec<u64>,
    /// The leader this member follows, by host: "a member" where it does
    /// not know which.
    pub leader: Option<String>,
    /// The members' mailbox, and the probe's tags: asked, answered.
    pub name: &'static str,
    pub tags: (u32, u32),
}

/// What `recover` did.
pub enum Recovered {
    /// Nothing: no other voter answers, and what it does is for the person
    /// to confirm by the group's name.
    Asked,
    /// This host its group's one voter: the members removed, and those that
    /// could not be, each with why.
    Done { removed: Vec<String>, kept: Vec<(String, String)> },
}

/// The last member standing made its group's one voter (plans/cluster.md,
/// "When the majority is gone"): refused while it follows a leader or any
/// other voter answers; done once `confirm` is the group's name. Unsafe by
/// design -- what the lost members had committed and this one had not is
/// gone -- so they are removed for good: none comes back with a history that
/// went another way. Called with the right to start the member held --
/// taken by the caller before it read what `r` says. Once confirmed,
/// `prepare` takes what a start needs -- the member file -- before anything
/// is stopped; `restart` starts the member again with it -- over the log
/// made its group's alone, or, where that was refused, over what the folder
/// holds -- and `remove` asks a member's removal of the group. A log gone
/// beside `begun` is refused, as `open_log` refuses it, before the member is
/// stopped: forced, it would be a group of a member that forgot its votes.
#[allow(clippy::too_many_arguments)]
pub fn recover<A: App, M>(runner: &Runner<A>, _held: &Starting<'_>, dir: &str, r: Recovery, confirm: Option<&str>,
                          prepare: &dyn Fn() -> Result<M, String>,
                          restart: &dyn Fn(&M, Option<Log>) -> Result<(), String>,
                          remove: &dyn Fn(&str) -> Result<u64, String>) -> Result<Recovered, String> {
    if let Some(leader) = &r.leader {
        return Err(alloc::format!("this member follows a leader, {}: the majority answers -- recover is for {} whose \
                                   majority is gone for good", leader, r.an));
    }
    let voters: Vec<(String, u64)> = r.others.iter().filter(|(_, n)| r.voters.contains(n)).cloned().collect();
    let answering = probe(r.name, r.tags, &voters)?;
    if !answering.is_empty() {
        return Err(alloc::format!("{} answers: the majority may not be gone -- recover refused", answering.join(", ")));
    }
    let Some(given) = confirm else { return Ok(Recovered::Asked) };
    if given != r.group {
        return Err(alloc::format!("\"{}\" is not this {}'s name -- it is {}", given, r.what, r.group));
    }
    let (me, node) = r.me;
    let start = prepare()?;
    let mut files = KFiles::new(dir, LOG_DIR)?;
    held_to_begun(dir, &mut files, &r.remedy)?;
    runner.stop();
    let stopped = |why: String| {
        let why = alloc::format!("{} -- the member is stopped: `rmmod {}` and `insmod` start it again", why, r.module);
        runner.hold(&why);
        why
    };
    let log = DiskLog::open(files, LogConfig::default())
        .map_err(|e| stopped(alloc::format!("the log in {}: {:?}", path(dir, LOG_DIR), e)))?;
    let (log, forced) = match raft::recover::force_alone(log, node) {
        Ok(f) => f,
        Err(e) => {
            /* Refused before writing, or a write that failed: the member
               started again over what the folder holds */
            let again = match restart(&start, None) {
                Ok(()) => String::new(),
                Err(w) => {
                    runner.hold(&w);
                    alloc::format!(", and the member not started again: {}", w)
                }
            };
            return Err(alloc::format!("recover refused: {:?}{}", e, again));
        }
    };
    klog!(0, "{}: recover: {} (node {}) alone, {:?} taken out, the log committed from {} to {}", A::LOG, me, node,
          forced.removed, forced.committed_before, forced.last);
    restart(&start, Some(log)).map_err(stopped)?;
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for (host, _) in &r.others {
        match remove(host) {
            Ok(_) => removed.push(host.clone()),
            Err(why) => kept.push((host.clone(), why)),
        }
    }
    Ok(Recovered::Done { removed, kept })
}
