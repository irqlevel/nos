#![no_std]

//! A member of a raft group over ipc: raft's replicated state machine
//! (`raft::rsm::Rsm`) over a log in a folder of the root, run by a task of
//! its own, which alone touches it -- for a module that keeps a state every
//! host of its group holds alike. cluster.ko's registry is one (plans/
//! cluster.md), nbs-mds.ko's metadata the next (plans/nbs.md). What is the
//! same in every such group is here; what is a group's own is its [`App`]'s.
//!
//! - **Its clock**: a tick every `TICK_MS`; an election after ten to twenty
//!   of them, a heartbeat at each.
//! - **Its messages** go over ipc to `<App::NAME>@<host>` -- a `Cluster`
//!   name, which ipcnet delivers from pinned hosts alone -- by the bulk
//!   connection when large, behind the group's id where the app has one. One
//!   that comes in is taken only from the host the app has for the node it
//!   says it is from: a host of the group cannot speak for another's node.
//! - **What it is asked**: the shell's commands come in a queue
//!   (`Shared::requests`). On the leader one is submitted under the member's
//!   session; on another member it is handed to the leader's (`TAG_PROPOSE`),
//!   which answers once it is applied. A group's commands are to be made so
//!   that asking twice is harmless: a request lost with a leader is asked
//!   again by whoever waits for it.
//! - **What the leader keeps in step**: raft's membership with the voters
//!   and learners the app's state names, a change at a time -- a node added
//!   as a learner, made a voter once it holds the log, a removed one taken
//!   out, the leader's own removal waiting for the lead handed to a voter
//!   that stays.

extern crate alloc;

mod files;

pub use files::KFiles;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::ipc::{Addr, Limits, Mailbox, Message, Scope, TAG_BULK};
use kcore::klog;
use kcore::sync::Mutex;
use kcore::time::{boot_time_ns, Duration};
use raft::log::DiskLog;
use raft::rsm::StateMachine;
use raft::{ChangeKind, ConfChange, ConfState, Event, NodeId, Role, Rsm};

pub const TICK_MS: u64 = 100;
const NS_PER_MS: u64 = 1_000_000;
/// How long a receive waits at most: how soon the shell's request is
/// taken, or the module's unload seen.
const POLL_MS: u64 = 20;
/// Messages taken in at a time before the member does its work.
const BATCH: usize = 64;
/// `process` rounds at a time: until it has nothing to do.
const ROUNDS: usize = 16;
/// A raft message: its bytes as raft::wire has them.
pub const TAG_RAFT: u32 = 0x100;
/// A command handed to the leader: a request's number, then the command.
pub const TAG_PROPOSE: u32 = 0x101;
/// The leader's answer to one: the request's number, a byte -- 1 applied, 0
/// not -- then the answer's bytes, or what went wrong in words.
pub const TAG_ANSWER: u32 = 0x102;
/// The first tag a group's own messages may have: below it the engine's.
pub const TAG_APP: u32 = 0x103;
/// A raft message past this goes by the bulk connection: a snapshot's piece,
/// appends that carry much.
const BULK_BYTES: usize = 4096;
/// A learner made a voter once it holds the log to within this many
/// entries of the leader's commit.
const CAUGHT_UP: u64 = 8;
/// A membership change not applied after this long is asked again.
const CHANGE_RETRY_MS: u64 = 5000;
/// How long a request handed to the leader waits for its answer.
pub const ANSWER_MS: u64 = 10_000;
/// How long the shell asks a command for, all its tries together, how often
/// again while the member cannot take it yet, and how often it looks for the
/// answer.
const ASK_TOTAL_MS: u64 = 30_000;
const ASK_RETRY_MS: u64 = 250;
const ANSWER_POLL_MS: u64 = 20;
/// How often a message of another group is said in the log at most.
const OTHER_GROUP_SAY_MS: u64 = 10_000;
/// A group's id, before every message of the engine's where the app has one.
pub const GROUP_BYTES: usize = 16;

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

pub type Log = DiskLog<KFiles>;

/// What a group's module gives the engine: its state, its commands and
/// answers, how its hosts are its nodes, and what it does besides.
pub trait App: Sized {
    /// raft's state machine: what the group keeps.
    type State: StateMachine + Clone;
    type Command: Clone;
    type Answer;
    /// Who waits for a command of the group's own asking -- a host joining.
    type Waiter;

    /// The member's mailbox, the same `Cluster` name on every host.
    const NAME: &'static str;
    /// What the member's lines in the log begin with.
    const LOG: &'static str;
    /// What may wait in the mailbox.
    const LIMITS: Limits = Limits { messages: 1024, bytes: 8 << 20 };

    fn encode_command(cmd: &Self::Command) -> Vec<u8>;
    fn decode_command(bytes: &[u8]) -> Option<Self::Command>;
    fn encode_answer(answer: &Self::Answer) -> Vec<u8>;
    fn decode_answer(bytes: &[u8]) -> Option<Self::Answer>;

    /// The group's id, carried before every message of the engine's and
    /// held to on each that comes in: a message of another group is
    /// dropped and said. None for a group whose connections say it already.
    fn group(&self, _core: &Core<Self>) -> Option<[u8; GROUP_BYTES]> {
        None
    }

    /// The node host `name` is, and the host node `node` is: by the state,
    /// or by what the app knew of the group before its state had it.
    fn node_of(&self, core: &Core<Self>, name: &str) -> Option<u64>;
    fn host_of(&self, core: &Core<Self>, node: u64) -> Option<String>;

    /// The voters and the learners the state names: what the leader keeps
    /// raft's membership to.
    fn wanted(&self, core: &Core<Self>) -> (Vec<u64>, Vec<u64>);

    /// A message of a tag the engine does not take: the group's own.
    fn on_message(&mut self, _core: &mut Core<Self>, _m: &Message) {}

    /// A command about to be submitted, as the leader: `Err` refuses it,
    /// saying why -- the leader's own removal, say, waiting for the lead
    /// handed on first.
    fn before_submit(&mut self, _core: &mut Core<Self>, _cmd: &Self::Command) -> Result<(), String> {
        Ok(())
    }

    /// The outcome of a command a waiter of the group's own waits for.
    fn finish(&mut self, _core: &mut Core<Self>, _w: Self::Waiter, _outcome: Outcome<Self>) {}

    /// What the group does at every turn, after raft's work.
    fn turn(&mut self, _core: &mut Core<Self>) {}
}

/// What a request is answered: the command's answer, or what went wrong.
pub type Outcome<A> = Result<<A as App>::Answer, String>;
/// Where a request's outcome goes, for whoever waits for it.
pub type Slot<A> = Arc<Locked<Option<Outcome<A>>>>;

/// A command the shell asks for.
pub struct Request<A: App> {
    pub cmd: A::Command,
    pub done: Slot<A>,
}

/// What the shell may see of the member: copied at every turn.
pub struct View<A: App> {
    pub state: A::State,
    pub node: u64,
    pub role: Role,
    pub leader: NodeId,
    pub term: u64,
    pub applied: u64,
    pub membership: ConfState,
    /// Why it stopped, once it has.
    pub stopped: Option<String>,
    /// Messages refused: not from the host their node is, of another group,
    /// or no message.
    pub refused: u64,
}

impl<A: App> Clone for View<A> {
    fn clone(&self) -> View<A> {
        View {
            state: self.state.clone(),
            node: self.node,
            role: self.role,
            leader: self.leader,
            term: self.term,
            applied: self.applied,
            membership: self.membership.clone(),
            stopped: self.stopped.clone(),
            refused: self.refused,
        }
    }
}

/// Between the shell and the member's task.
pub struct Shared<A: App> {
    pub stop: AtomicBool,
    pub requests: Locked<VecDeque<Request<A>>>,
    pub view: Locked<Option<View<A>>>,
}

impl<A: App> Shared<A> {
    pub fn new() -> Option<Shared<A>> {
        Some(Shared { stop: AtomicBool::new(false), requests: Locked::new(VecDeque::new())?,
                      view: Locked::new(None)? })
    }

    pub fn view(&self) -> Option<View<A>> {
        self.view.with(|v| v.clone())
    }
}

/// The engine's half of the member: raft, the mailbox, the requests in
/// flight. The app's hooks are handed it.
pub struct Core<A: App> {
    rsm: Rsm<Log, A::State>,
    mb: Mailbox,
    node: u64,
    session: Option<u64>,
    registering: Option<u64>,
    seq: u64,
    pending: BTreeMap<u64, Pending<A>>,
    /// Requests handed to the leader, by their number: who waits, until
    /// when, and the leader's node, which alone may answer.
    forwarded: BTreeMap<u64, (Pending<A>, u64, u64)>,
    next_request: u64,
    /// The voter this leader, removed, hands the lead to.
    handing_to: Option<u64>,
    /// The membership change proposed and not yet in force, and when.
    change: Option<(ConfChange, u64)>,
    refused: u64,
    /// When a message of another group was last said in the log.
    other_group_said: u64,
}

/// Who waits for a command submitted here.
pub enum Pending<A: App> {
    /// The shell, on this host.
    Local(Slot<A>),
    /// Another member, which handed the command to this one.
    Remote { to: Addr, request: u64 },
    /// One of the group's own: a host joining.
    App(A::Waiter),
}

/// The member: the app's half and the engine's.
pub struct Member<A: App> {
    pub app: A,
    pub core: Core<A>,
}

impl<A: App> Member<A> {
    /// The member over `rsm`, node `node`, its mailbox named.
    pub fn new(app: A, rsm: Rsm<Log, A::State>, node: u64) -> Result<Member<A>, String> {
        Ok(Member { app, core: Core::new(rsm, node)? })
    }

    pub fn view(&self, stopped: Option<String>) -> View<A> {
        self.core.view(stopped)
    }
}

impl<A: App> Core<A> {
    fn new(rsm: Rsm<Log, A::State>, node: u64) -> Result<Core<A>, String> {
        /* Request numbers from a random start, never 0: the log may hold this
           member's own session registrations of an earlier start, applied
           again as it starts, and one of the same number would be taken for
           this start's -- a session long used, whose next command is stale */
        let first = kcore::random::random_u64().ok_or_else(|| String::from("no randomness for the member's start"))?;
        let mb = Mailbox::new(A::LIMITS).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
        mb.register(A::NAME, Scope::Cluster).map_err(|e| alloc::format!("{} not named: {}", A::NAME, e.name()))?;
        Ok(Core {
            rsm,
            mb,
            node,
            session: None,
            registering: None,
            seq: 0,
            pending: BTreeMap::new(),
            forwarded: BTreeMap::new(),
            next_request: first | 1,
            handing_to: None,
            change: None,
            refused: 0,
            other_group_said: 0,
        })
    }

    /* ---- what the app may look at, and do ---- */

    pub fn rsm(&self) -> &Rsm<Log, A::State> {
        &self.rsm
    }

    pub fn state(&self) -> &A::State {
        self.rsm.state()
    }

    /// This member's node.
    pub fn node(&self) -> u64 {
        self.node
    }

    pub fn role(&self) -> Role {
        self.rsm.role()
    }

    pub fn leader(&self) -> NodeId {
        self.rsm.leader()
    }

    pub fn membership(&self) -> ConfState {
        self.rsm.node().membership()
    }

    pub fn mailbox(&self) -> &Mailbox {
        &self.mb
    }

    /// A message the app took for no one's.
    pub fn note_refused(&mut self) {
        self.refused += 1;
    }

    /// The lead handed to `to` -- asked again at every turn by whoever
    /// wants it, raft taking the same hand-over once: None where raft
    /// refuses it, `Some(true)` the first time it is asked for `to`, for the
    /// caller to say so once.
    pub fn hand_lead_to(&mut self, to: u64) -> Option<bool> {
        self.rsm.transfer_leader(to).ok()?;
        let first = self.handing_to != Some(to);
        self.handing_to = Some(to);
        Some(first)
    }

    pub fn view(&self, stopped: Option<String>) -> View<A> {
        let node = self.rsm.node();
        View {
            state: self.rsm.state().clone(),
            node: self.node,
            role: self.rsm.role(),
            leader: self.rsm.leader(),
            term: node.term(),
            applied: self.rsm.applied(),
            membership: node.membership(),
            stopped,
            refused: self.refused,
        }
    }

    /// The node a message from another host comes from, if the app has the
    /// host for one.
    pub fn sender_node(&self, app: &A, m: &Message) -> Option<u64> {
        let name = kcore::ipc::peer_name(m.from)?;
        app.node_of(self, &name)
    }

    /// `cmd`, asked: submitted here as the leader, handed to the leader
    /// otherwise -- `w` told its outcome.
    pub fn ask(&mut self, app: &mut A, cmd: A::Command, w: Pending<A>) {
        if self.rsm.role() == Role::Leader {
            self.submit(app, cmd, w);
        } else {
            self.hand_to_leader(app, cmd, w);
        }
    }

    /* ---- what goes out ---- */

    /// `data` behind the group's id, where the app has one.
    fn framed(&self, app: &A, data: &[u8]) -> Vec<u8> {
        match app.group(self) {
            Some(g) => {
                let mut b = Vec::with_capacity(GROUP_BYTES + data.len());
                b.extend_from_slice(&g);
                b.extend_from_slice(data);
                b
            }
            None => Vec::from(data),
        }
    }

    /// A message of the engine's that came in, its group's id taken off and
    /// held to -- None, and said, where it is another's.
    fn unframed<'m>(&mut self, app: &A, m: &'m Message) -> Option<&'m [u8]> {
        let Some(g) = app.group(self) else { return Some(&m.data) };
        if m.data.len() >= GROUP_BYTES && m.data[..GROUP_BYTES] == g {
            return Some(&m.data[GROUP_BYTES..]);
        }
        self.refused += 1;
        let now = boot_time_ns();
        if now >= self.other_group_said {
            self.other_group_said = now + OTHER_GROUP_SAY_MS * NS_PER_MS;
            let from = kcore::ipc::peer_name(m.from).unwrap_or_else(|| String::from("this host"));
            klog!(0, "{}: a message from {} of another group, dropped -- not this group's member there", A::LOG,
                  from);
        }
        None
    }

    fn answer_remote(&self, app: &A, to: Addr, request: u64, outcome: Outcome<A>) {
        let mut data = Vec::new();
        data.extend_from_slice(&request.to_le_bytes());
        match outcome {
            Ok(a) => {
                data.push(1);
                data.extend_from_slice(&A::encode_answer(&a));
            }
            Err(why) => {
                data.push(0);
                data.extend_from_slice(why.as_bytes());
            }
        }
        let _ = self.mb.send(to, TAG_ANSWER, 0, &self.framed(app, &data));
    }

    fn finish(&mut self, app: &mut A, w: Pending<A>, outcome: Outcome<A>) {
        match w {
            Pending::Local(slot) => slot.with(|s| *s = Some(outcome)),
            Pending::Remote { to, request } => self.answer_remote(app, to, request, outcome),
            Pending::App(w) => app.finish(self, w, outcome),
        }
    }

    /// `cmd` handed to the leader's member, `w` told its answer.
    fn hand_to_leader(&mut self, app: &mut A, cmd: A::Command, w: Pending<A>) {
        let leader = self.rsm.leader();
        let Some(host) = app.host_of(self, leader) else {
            self.finish(app, w, Err(String::from("no leader known yet -- ask again in a moment")));
            return;
        };
        let request = self.take_request();
        let mut data = Vec::new();
        data.extend_from_slice(&request.to_le_bytes());
        data.extend_from_slice(&A::encode_command(&cmd));
        let to = alloc::format!("{}@{}", A::NAME, host);
        let framed = self.framed(app, &data);
        if let Err(e) = self.mb.send_to(&to, TAG_PROPOSE, 0, &framed) {
            self.finish(app, w, Err(alloc::format!("the leader, {}, out of reach: {}", host, e.name())));
            return;
        }
        self.forwarded.insert(request, (w, boot_time_ns() + ANSWER_MS * NS_PER_MS, leader));
    }

    /// `cmd` submitted under the member's session, as the leader.
    fn submit(&mut self, app: &mut A, cmd: A::Command, w: Pending<A>) {
        if self.rsm.role() != Role::Leader {
            self.finish(app, w, Err(String::from("not the leader any more -- ask again")));
            return;
        }
        if let Err(why) = app.before_submit(self, &cmd) {
            self.finish(app, w, Err(why));
            return;
        }
        let Some(client) = self.session else {
            self.finish(app, w, Err(String::from("the leader is not ready yet -- ask again in a moment")));
            return;
        };
        self.seq += 1;
        match self.rsm.submit(client, self.seq, &A::encode_command(&cmd)) {
            Ok(_) => {
                self.pending.insert(self.seq, w);
            }
            Err(e) => self.finish(app, w, Err(alloc::format!("not taken: {:?}", e))),
        }
    }

    fn send_raft(&self, app: &A, m: raft::Message) {
        let Some(host) = app.host_of(self, m.to) else { return };
        let mut bytes = Vec::new();
        if raft::wire::encode(&m, &mut bytes).is_err() {
            return;
        }
        let bytes = self.framed(app, &bytes);
        let tag = if bytes.len() > BULK_BYTES { TAG_RAFT | TAG_BULK } else { TAG_RAFT };
        let _ = self.mb.send_to(&alloc::format!("{}@{}", A::NAME, host), tag, 0, &bytes);
    }

    /* ---- what comes in ---- */

    fn on_message(&mut self, app: &mut A, m: Message) {
        match m.tag & !TAG_BULK {
            TAG_RAFT => self.on_raft(app, &m),
            TAG_PROPOSE => self.on_propose(app, &m),
            TAG_ANSWER => self.on_answer(app, &m),
            _ => app.on_message(self, &m),
        }
    }

    fn on_raft(&mut self, app: &mut A, m: &Message) {
        let Some(data) = self.unframed(app, m) else { return };
        let Ok(rm) = raft::wire::decode(data) else {
            self.refused += 1;
            return;
        };
        /* From the host whose node it says it is from, to this one */
        if self.sender_node(app, m) != Some(rm.from) || rm.to != self.node {
            self.refused += 1;
            return;
        }
        let _ = self.rsm.step(rm);
    }

    /// A command another member hands this one, as the leader.
    fn on_propose(&mut self, app: &mut A, m: &Message) {
        let Some(data) = self.unframed(app, m) else { return };
        if data.len() < 8 || self.sender_node(app, m).is_none() {
            self.refused += 1;
            return;
        }
        let request = u64::from_le_bytes(data[..8].try_into().unwrap_or([0; 8]));
        let Some(cmd) = A::decode_command(&data[8..]) else {
            self.answer_remote(app, m.from, request, Err(String::from("not a command")));
            return;
        };
        self.submit(app, cmd, Pending::Remote { to: m.from, request });
    }

    /// The leader's answer to a request this host handed it.
    fn on_answer(&mut self, app: &mut A, m: &Message) {
        let Some(data) = self.unframed(app, m) else { return };
        let sender = self.sender_node(app, m);
        if data.len() < 9 || sender.is_none() {
            self.refused += 1;
            return;
        }
        let request = u64::from_le_bytes(data[..8].try_into().unwrap_or([0; 8]));
        /* From the leader it was handed to, and no other host */
        match self.forwarded.get(&request) {
            Some(&(_, _, node)) if sender == Some(node) => {}
            Some(_) => {
                self.refused += 1;
                return;
            }
            None => return,
        }
        let outcome = if data[8] == 1 {
            A::decode_answer(&data[9..]).ok_or_else(|| String::from("an answer that is none"))
        } else {
            Err(String::from_utf8_lossy(&data[9..]).into_owned())
        };
        let Some((w, _, _)) = self.forwarded.remove(&request) else { return };
        self.finish(app, w, outcome);
    }

    /* ---- what the member does ---- */

    fn on_event(&mut self, app: &mut A, e: Event) {
        match e {
            Event::Registered { origin, request, client } => {
                if origin == self.node && Some(request) == self.registering {
                    self.registering = None;
                    self.session = Some(client);
                }
            }
            Event::Applied { client, seq, answer, .. } if Some(client) == self.session => {
                if let Some(w) = self.pending.remove(&seq) {
                    let outcome = A::decode_answer(&answer).ok_or_else(|| String::from("an answer that is none"));
                    self.finish(app, w, outcome);
                }
            }
            Event::Expired { client, seq } | Event::Stale { client, seq } | Event::Lost { client, seq }
                if Some(client) == self.session =>
            {
                if let Some(w) = self.pending.remove(&seq) {
                    self.finish(app, w, Err(String::from("the member's session went -- ask again")));
                }
                /* A leader is never left without one */
                if self.rsm.role() == Role::Leader {
                    self.register_session();
                } else {
                    self.session = None;
                }
            }
            _ => {}
        }
    }

    /// A request's number, this start's own.
    fn take_request(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        request
    }

    /// A session of its own to submit under, asked at once -- as it comes to
    /// lead, and as one it had goes.
    fn register_session(&mut self) {
        self.session = None;
        let request = self.take_request();
        self.registering = self.rsm.register(request).is_ok().then_some(request);
    }

    fn on_role(&mut self, app: &mut A, role: Role) {
        if role == Role::Leader {
            self.register_session();
        } else {
            /* What waited here is asked again by whoever waits */
            let waiting = core::mem::take(&mut self.pending);
            for (_, w) in waiting {
                self.finish(app, w, Err(String::from("the leader changed -- ask again")));
            }
            self.session = None;
            self.registering = None;
            self.change = None;
            self.handing_to = None;
        }
    }

    /// A leader known that is not the one a request was handed to: the
    /// request asked again by whoever waits, of the new one, rather than
    /// waited out.
    fn on_leader(&mut self, app: &mut A, leader: NodeId) {
        if leader == raft::NONE {
            return;
        }
        let gone: Vec<u64> = self.forwarded.iter().filter(|(_, &(_, _, to))| to != leader).map(|(&r, _)| r).collect();
        for r in gone {
            if let Some((w, _, _)) = self.forwarded.remove(&r) {
                self.finish(app, w, Err(String::from("the leader changed -- ask again")));
            }
        }
    }

    /// Everything raft has to do, done.
    fn process(&mut self, app: &mut A) -> Result<(), String> {
        for _ in 0..ROUNDS {
            let out = self.rsm.process().map_err(|e| alloc::format!("{:?}", e))?;
            let idle = out.messages.is_empty() && out.events.is_empty() && out.soft_state.is_none();
            for m in out.messages {
                self.send_raft(app, m);
            }
            if let Some(ss) = out.soft_state {
                self.on_role(app, ss.role);
                self.on_leader(app, ss.leader);
            }
            for e in out.events {
                self.on_event(app, e);
            }
            if idle && !self.rsm.node().has_ready() {
                break;
            }
        }
        Ok(())
    }

    /// As the leader, raft's membership a step nearer the voters and
    /// learners the app's state names.
    fn keep_membership(&mut self, app: &mut A) {
        if self.rsm.role() != Role::Leader {
            return;
        }
        let now = boot_time_ns();
        let conf = self.rsm.node().membership();
        if let Some((cc, at)) = self.change {
            let done = match cc.kind {
                ChangeKind::AddVoter => conf.voters.contains(&cc.node),
                ChangeKind::AddLearner => conf.learners.contains(&cc.node),
                ChangeKind::Remove => !conf.voters.contains(&cc.node) && !conf.learners.contains(&cc.node),
            };
            if !done && now < at + CHANGE_RETRY_MS * NS_PER_MS {
                return;
            }
            self.change = None;
        }
        let (voters, learners) = app.wanted(self);
        let member = |n: &u64| conf.voters.contains(n) || conf.learners.contains(n);
        let next = if let Some(&n) = conf.voters.iter().chain(conf.learners.iter())
            .find(|n| !voters.contains(n) && !learners.contains(n))
        {
            /* A removed node taken out -- never the leader itself, which
               hands the lead to a voter that stays, whose own turn here then
               takes it out. Asked again at every turn: raft takes the same
               hand-over once */
            if n == self.node {
                if let Some(&to) = conf.voters.iter().find(|&&v| v != n && voters.contains(&v)) {
                    if self.hand_lead_to(to) == Some(true) {
                        klog!(0, "{}: this host is removed: the lead handed to node {}", A::LOG, to);
                    }
                }
                None
            } else {
                Some(ConfChange { kind: ChangeKind::Remove, node: n })
            }
        } else if let Some(&n) = voters.iter().chain(learners.iter()).find(|n| !member(n)) {
            /* A node added: a learner first, whatever it will be */
            Some(ConfChange { kind: ChangeKind::AddLearner, node: n })
        } else if let Some(&n) = voters.iter().find(|n| conf.learners.contains(n)) {
            /* A voter to be, once it holds the log */
            let committed = self.rsm.node().committed();
            let held = self.rsm.node().matched(n).unwrap_or(0);
            (held + CAUGHT_UP >= committed).then_some(ConfChange { kind: ChangeKind::AddVoter, node: n })
        } else {
            learners.iter().find(|n| conf.voters.contains(n))
                .map(|&n| ConfChange { kind: ChangeKind::AddLearner, node: n })
        };
        if let Some(cc) = next {
            if self.rsm.propose_change(cc).is_ok() {
                klog!(0, "{}: membership: {:?} node {}", A::LOG, cc.kind, cc.node);
                self.change = Some((cc, now));
            }
        }
    }

    /// Requests handed to the leader that have waited past their time.
    fn expire_forwarded(&mut self, app: &mut A) {
        let now = boot_time_ns();
        let late: Vec<u64> = self.forwarded.iter().filter(|(_, (_, until, _))| *until <= now).map(|(&r, _)| r).collect();
        for r in late {
            if let Some((w, _, _)) = self.forwarded.remove(&r) {
                self.finish(app, w, Err(String::from("no answer from the leader -- ask again")));
            }
        }
    }
}

/// The member's task: until the module goes, or its log fails.
pub fn run<A: App>((shared, m): (Arc<Shared<A>>, Member<A>)) {
    let Member { mut app, mut core } = m;
    let tick = TICK_MS * NS_PER_MS;
    let mut next_tick = boot_time_ns() + tick;
    shared.view.with(|v| *v = Some(core.view(None)));
    while !shared.stop.load(Ordering::Acquire) {
        let wait = (next_tick.saturating_sub(boot_time_ns()) / NS_PER_MS).clamp(1, POLL_MS);
        if let Some(msg) = core.mb.recv(Duration::from_millis(wait)) {
            core.on_message(&mut app, msg);
            for _ in 0..BATCH {
                match core.mb.try_recv() {
                    Some(more) => core.on_message(&mut app, more),
                    None => break,
                }
            }
        }
        if boot_time_ns() >= next_tick {
            core.rsm.tick();
            next_tick += tick;
            core.expire_forwarded(&mut app);
        }
        let requests: Vec<Request<A>> = shared.requests.with(|q| q.drain(..).collect());
        for r in requests {
            core.ask(&mut app, r.cmd, Pending::Local(r.done));
        }
        if let Err(why) = core.process(&mut app) {
            klog!(0, "{}: the member stopped: {} -- `rmmod` and `insmod` start it again", A::LOG, why);
            shared.view.with(|v| *v = Some(core.view(Some(why))));
            return;
        }
        app.turn(&mut core);
        core.keep_membership(&mut app);
        shared.view.with(|v| *v = Some(core.view(None)));
    }
}

/// `cmd`, as the member's state answers it -- asked of the member's task by
/// the shell, again while the member cannot take it yet. `no_member` is
/// what is said where there is no member to ask.
pub fn ask<A: App>(shared: &Shared<A>, cmd: &A::Command, no_member: &str) -> Result<A::Answer, String> {
    let mut last = String::from("no answer");
    let end = boot_time_ns() + ASK_TOTAL_MS * NS_PER_MS;
    while boot_time_ns() < end {
        if shared.view().is_none_or(|v| v.stopped.is_some()) {
            return Err(String::from(no_member));
        }
        let slot: Slot<A> = Arc::new(Locked::new(None).ok_or_else(|| String::from("out of memory"))?);
        shared.requests.with(|q| q.push_back(Request { cmd: cmd.clone(), done: slot.clone() }));
        let until = (boot_time_ns() + (ANSWER_MS + 2000) * NS_PER_MS).min(end);
        let outcome = loop {
            if let Some(o) = slot.with(|s| s.take()) {
                break o;
            }
            if boot_time_ns() >= until {
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
