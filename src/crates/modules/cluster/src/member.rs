//! The member: raft's replicated state machine over the registry
//! (`raft::rsm::Rsm<DiskLog, registry::State>`), run by a task of its own
//! (`cluster/raft`), which alone touches it.
//!
//! - **Its clock**: a tick every `TICK_MS`; an election after ten to twenty
//!   of them, a heartbeat at each.
//! - **Its messages** go over ipc to `cluster.raft@<host>` -- a `Cluster`
//!   name, which ipcnet delivers from pinned hosts alone -- by the bulk
//!   connection when large. One that comes in is taken only from the host
//!   the registry has for the node it says it is from: a host of the
//!   cluster cannot speak for another's node.
//! - **What it is asked**: the shell's commands come in a queue
//!   (`Shared::requests`). On the leader one is submitted under the
//!   member's session; on another member it is handed to the leader's
//!   (`TAG_PROPOSE`), which answers once it is applied. Commands are made
//!   so that asking twice is harmless -- an invitation's number is random,
//!   a remove of a host gone is refused -- so a request lost with a leader
//!   is asked again by whoever waits for it.
//! - **What the leader keeps in step**: raft's membership with the
//!   registry's voters and learners, a change at a time -- a host added as
//!   a learner, made a voter once it holds the log, a removed host taken
//!   out.
//! - **What every member keeps in step**: ipcnet's peers with the
//!   registry's -- its hosts, and the hosts invited, by the key each will
//!   come with -- sent to `ipcnet.ctl` whenever they change: ipcnet writes
//!   hosts.toml and pins or drops them.
//! - **A host joining** (`TAG_JOIN_REQ`) is taken from the host an open
//!   invitation names alone -- whose key every member's ipcnet pinned from
//!   the invitation -- proving the invitation's secret. Its join committed,
//!   it is answered with its node and the hosts (`TAG_JOINED`), which it
//!   knows its peers and their nodes by until its registry has caught up:
//!   the leader adds it as a learner, and raft brings it the log.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ipcwire::control;
use kcore::ipc::{Addr, Limits, Mailbox, Message, Scope, TAG_BULK};
use kcore::klog;
use kcore::time::{boot_time_ns, Duration};
use raft::log::DiskLog;
use raft::{ChangeKind, ConfChange, ConfState, Event, NodeId, Role, Rsm};
use registry::{Answer, Command, State};

use crate::files::{self, KFiles};
use crate::Locked;

/// The member's mailbox.
pub const NAME: &str = "cluster.raft";
const TICK_MS: u64 = 100;
const NS_PER_MS: u64 = 1_000_000;
/// How long a receive waits at most: how soon the shell's request is
/// taken, or the module's unload seen.
const POLL_MS: u64 = 20;
/// Messages taken in at a time before the member does its work.
const BATCH: usize = 64;
/// How long after ipcnet refused a list of peers it is told again -- the
/// same list, should nothing have changed: a refusal said in the log at
/// most this often.
const PEERS_RETRY_MS: u64 = 10_000;
/* Every peer a host has -- the other hosts and the open invitations, which
   the registry holds to HOSTS_MAX together -- fits a list ipcnet takes */
const _: () = assert!(registry::HOSTS_MAX - 1 <= control::PEERS_MAX);
/// `process` rounds at a time: until it has nothing to do.
const ROUNDS: usize = 16;
/// What may wait in the mailbox: raft's messages, ipcnet's answers.
const LIMITS: Limits = Limits { messages: 1024, bytes: 8 << 20 };
/// A raft message: its bytes as raft::wire has them.
pub const TAG_RAFT: u32 = 0x100;
/// A command handed to the leader: a request's number, then the command.
const TAG_PROPOSE: u32 = 0x101;
/// The leader's answer to one: the request's number, a byte -- 1 applied, 0
/// not -- then the answer's bytes, or what went wrong in words.
const TAG_ANSWER: u32 = 0x102;
/// A host's join, from the host an invitation names: the invitation's
/// number, then its secret.
pub const TAG_JOIN_REQ: u32 = 0x103;
/// Its answer: a byte -- 1 joined, 0 not -- then `Joined`'s bytes, or why
/// not in words.
pub const TAG_JOINED: u32 = 0x104;
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

/// What a request is answered: the command's answer, or what went wrong.
pub type Outcome = Result<Answer, String>;
/// Where a request's outcome goes, for whoever waits for it.
pub type Slot = Arc<Locked<Option<Outcome>>>;

/// A command the shell asks for.
pub struct Request {
    pub cmd: Command,
    pub done: Slot,
}

/// What the shell may see of the member: copied at every turn.
#[derive(Clone)]
pub struct View {
    pub state: State,
    pub node: u64,
    pub role: Role,
    pub leader: NodeId,
    pub term: u64,
    pub applied: u64,
    pub membership: ConfState,
    /// Why it stopped, once it has.
    pub stopped: Option<String>,
    /// Messages refused: not from the host their node is, or no message.
    pub refused: u64,
}

/// Between the shell and the member's task.
pub struct Shared {
    pub stop: AtomicBool,
    pub requests: Locked<VecDeque<Request>>,
    pub view: Locked<Option<View>>,
}

/// Who waits for a command submitted here.
enum Waiting {
    /// The shell, on this host.
    Local(Slot),
    /// Another member, which handed the command to this one.
    Remote { to: Addr, request: u64 },
    /// A host joining by an invitation: answered with its node and the
    /// hosts.
    Joiner { to: Addr, invitation: u64 },
}

/// What a host that joined is told: its node, the cluster, and the hosts --
/// itself among them -- by name, node, address and keys.
pub struct Joined {
    pub node: u64,
    pub cluster: [u8; 16],
    pub hosts: Vec<files::Known>,
}

impl Joined {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.node.to_le_bytes());
        b.extend_from_slice(&self.cluster);
        b.push(self.hosts.len().min(registry::HOSTS_MAX) as u8);
        for h in self.hosts.iter().take(registry::HOSTS_MAX) {
            b.push(h.name.len() as u8);
            b.extend_from_slice(h.name.as_bytes());
            b.extend_from_slice(&h.node.to_le_bytes());
            b.extend_from_slice(&h.ip.to_le_bytes());
            b.push(h.keys.len().min(registry::KEYS_MAX) as u8);
            for k in h.keys.iter().take(registry::KEYS_MAX) {
                b.extend_from_slice(k);
            }
        }
        b
    }

    /// What a member said, read: every count bounded, every name a host's.
    pub fn decode(b: &[u8]) -> Option<Joined> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let got = b.get(at..at.checked_add(n)?)?;
            at += n;
            Some(got)
        };
        let node = u64::from_le_bytes(take(8)?.try_into().ok()?);
        let cluster: [u8; 16] = take(16)?.try_into().ok()?;
        let n = usize::from(take(1)?[0]);
        if n > registry::HOSTS_MAX {
            return None;
        }
        let mut hosts = Vec::with_capacity(n);
        for _ in 0..n {
            let len = usize::from(take(1)?[0]);
            let name = core::str::from_utf8(take(len)?).ok()?;
            let node = u64::from_le_bytes(take(8)?.try_into().ok()?);
            let ip = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let k = usize::from(take(1)?[0]);
            if !registry::valid_name(name) || node == 0 || ip == 0 || k == 0 || k > registry::KEYS_MAX
                || hosts.iter().any(|h: &files::Known| h.name == name || h.node == node)
            {
                return None;
            }
            let mut keys = Vec::with_capacity(k);
            for _ in 0..k {
                keys.push(take(32)?.try_into().ok()?);
            }
            hosts.push(files::Known { name: String::from(name), node, ip, keys });
        }
        (at == b.len() && node != 0).then_some(Joined { node, cluster, hosts })
    }
}

pub type Log = DiskLog<KFiles>;

pub struct Member {
    rsm: Rsm<Log, State>,
    mb: Mailbox,
    me: files::Member,
    session: Option<u64>,
    registering: Option<u64>,
    seq: u64,
    pending: BTreeMap<u64, Waiting>,
    /// Requests handed to the leader, by their number: who waits, until
    /// when, and the leader's node, which alone may answer.
    forwarded: BTreeMap<u64, (Waiting, u64, u64)>,
    next_request: u64,
    /// What ipcnet was last told its peers are.
    peers_told: Option<Vec<control::Peer>>,
    /// Not before when ipcnet is told again, after it refused.
    peers_retry: u64,
    /// The voter this leader, removed, hands the lead to.
    handing_to: Option<u64>,
    /// The invitations whose joins this member is taking now: a join asked
    /// again meanwhile is not taken twice -- the second would find the
    /// invitation used by the first, and its refusal could reach the host
    /// before the first's answer.
    joining: Vec<u64>,
    /// The membership change proposed and not yet in force, and when.
    change: Option<(ConfChange, u64)>,
    refused: u64,
}

impl Member {
    /// The member over its log, its mailbox named. A host that joined knows
    /// the cluster by `me.hosts`, as its join was answered, until its
    /// registry has the cluster.
    pub fn new(rsm: Rsm<Log, State>, me: files::Member) -> Result<Member, String> {
        let mb = Mailbox::new(LIMITS).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
        mb.register(NAME, Scope::Cluster).map_err(|e| alloc::format!("{} not named: {}", NAME, e.name()))?;
        Ok(Member {
            rsm,
            mb,
            me,
            session: None,
            registering: None,
            seq: 0,
            pending: BTreeMap::new(),
            forwarded: BTreeMap::new(),
            next_request: 1,
            peers_told: None,
            peers_retry: 0,
            handing_to: None,
            joining: Vec::new(),
            change: None,
            refused: 0,
        })
    }

    pub fn view(&self, stopped: Option<String>) -> View {
        let node = self.rsm.node();
        View {
            state: self.rsm.state().clone(),
            node: self.me.node,
            role: self.rsm.role(),
            leader: self.rsm.leader(),
            term: node.term(),
            applied: self.rsm.applied(),
            membership: node.membership(),
            stopped,
            refused: self.refused,
        }
    }

    /* ---- what comes in ---- */

    fn on_message(&mut self, m: Message) {
        match m.tag & !TAG_BULK {
            TAG_RAFT => self.on_raft(&m),
            TAG_PROPOSE => self.on_propose(&m),
            TAG_ANSWER => self.on_answer(&m),
            TAG_JOIN_REQ => self.on_join(&m),
            /* ipcnet's answers, from this host alone */
            control::TAG_OK if m.from.host() == 0 => {}
            control::TAG_REFUSED if m.from.host() == 0 => {
                klog!(0, "cluster: ipcnet refused this host's peers: {} -- told again in {} s",
                      String::from_utf8_lossy(&m.data), PEERS_RETRY_MS / 1000);
                self.peers_told = None;
                self.peers_retry = boot_time_ns() + PEERS_RETRY_MS * NS_PER_MS;
            }
            _ => {}
        }
    }

    /// The node host `name` is: the registry's, or -- before it has a
    /// cluster -- what this host's join was answered.
    fn node_of(&self, name: &str) -> Option<u64> {
        if self.rsm.state().cluster().is_some() {
            return self.rsm.state().host(name).map(|h| h.node);
        }
        self.me.hosts.iter().find(|h| h.name == name).map(|h| h.node)
    }

    /// The host node `node` is, as `node_of` has them.
    fn host_of(&self, node: u64) -> Option<String> {
        if self.rsm.state().cluster().is_some() {
            return self.rsm.state().host_of_node(node).map(|h| h.name.clone());
        }
        self.me.hosts.iter().find(|h| h.node == node).map(|h| h.name.clone())
    }

    /// The host a message from another host came from, if the registry has
    /// it: its node.
    fn sender_node(&self, m: &Message) -> Option<u64> {
        let name = kcore::ipc::peer_name(m.from)?;
        self.node_of(&name)
    }

    fn on_raft(&mut self, m: &Message) {
        let Ok(rm) = raft::wire::decode(&m.data) else {
            self.refused += 1;
            return;
        };
        /* From the host whose node it says it is from, to this one */
        if self.sender_node(m) != Some(rm.from) || rm.to != self.me.node {
            self.refused += 1;
            return;
        }
        let _ = self.rsm.step(rm);
    }

    /// A command another member hands this one, as the leader.
    fn on_propose(&mut self, m: &Message) {
        if m.data.len() < 8 || self.sender_node(m).is_none() {
            self.refused += 1;
            return;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&m.data[..8]);
        let request = u64::from_le_bytes(b);
        let Some(cmd) = Command::decode(&m.data[8..]) else {
            self.answer_remote(m.from, request, Err(String::from("not a command")));
            return;
        };
        self.submit(cmd, Waiting::Remote { to: m.from, request });
    }

    /// The leader's answer to a request this host handed it.
    fn on_answer(&mut self, m: &Message) {
        if m.data.len() < 9 || self.sender_node(m).is_none() {
            self.refused += 1;
            return;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&m.data[..8]);
        let request = u64::from_le_bytes(b);
        /* From the leader it was handed to, and no other host */
        match self.forwarded.get(&request) {
            Some(&(_, _, node)) if self.sender_node(m) == Some(node) => {}
            Some(_) => {
                self.refused += 1;
                return;
            }
            None => return,
        }
        let Some((w, _, _)) = self.forwarded.remove(&request) else { return };
        let outcome = if m.data[8] == 1 {
            Answer::decode(&m.data[9..]).ok_or_else(|| String::from("an answer that is none"))
        } else {
            Err(String::from_utf8_lossy(&m.data[9..]).into_owned())
        };
        self.finish(w, outcome);
    }

    /// A host's join: taken from the host its invitation names, with the
    /// invitation's secret -- and committed by the leader.
    fn on_join(&mut self, m: &Message) {
        let refuse = |why: &str| {
            let mut data = alloc::vec![0u8];
            data.extend_from_slice(why.as_bytes());
            let _ = self.mb.send(m.from, TAG_JOINED, m.reference, &data);
        };
        let (Some(name), true) = (kcore::ipc::peer_name(m.from), m.data.len() == 40) else {
            self.refused += 1;
            return;
        };
        let mut b = [0u8; 8];
        b.copy_from_slice(&m.data[..8]);
        let invitation = u64::from_le_bytes(b);
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&m.data[8..]);
        if self.joining.contains(&invitation) {
            /* The first's answer is on its way */
            return;
        }
        let Some(inv) = self.rsm.state().invitation(invitation) else {
            /* Never answered again with its node: a host whose folder was
               wiped would come back under its old node with nothing of what
               it voted for */
            match self.rsm.state().host(&name) {
                Some(h) => refuse(&alloc::format!("{} joined already, as node {} -- the answer lost on the way: \
                                                   `cluster remove {}` on a member and invite it again", name, h.node,
                                                  name)),
                None => refuse("no such invitation: used, taken back, past its time or never made"),
            }
            return;
        };
        /* The host it names, over a connection pinned by its key, and its
           secret: anything else is not this invitation's */
        if inv.host != name || crate::token::secret_hash(&secret) != inv.secret_hash {
            self.refused += 1;
            refuse("not the host invited, or not its secret");
            return;
        }
        let cmd = Command::Join { now: kcore::time::wall_clock_secs(), invitation };
        self.joining.push(invitation);
        let w = Waiting::Joiner { to: m.from, invitation };
        if self.rsm.role() == Role::Leader {
            self.submit(cmd, w);
        } else {
            self.hand_to_leader(cmd, w);
        }
    }

    fn answer_remote(&self, to: Addr, request: u64, outcome: Outcome) {
        let mut data = Vec::new();
        data.extend_from_slice(&request.to_le_bytes());
        match outcome {
            Ok(a) => {
                data.push(1);
                data.extend_from_slice(&a.encode());
            }
            Err(why) => {
                data.push(0);
                data.extend_from_slice(why.as_bytes());
            }
        }
        let _ = self.mb.send(to, TAG_ANSWER, 0, &data);
    }

    fn finish(&mut self, w: Waiting, outcome: Outcome) {
        match w {
            Waiting::Local(slot) => slot.with(|s| *s = Some(outcome)),
            Waiting::Remote { to, request } => self.answer_remote(to, request, outcome),
            Waiting::Joiner { to, invitation } => {
                self.joining.retain(|&i| i != invitation);
                let joined = match outcome {
                    Ok(Answer::Done { node }) => self.rsm.state().cluster().map(|c| Joined {
                        node,
                        cluster: c.id,
                        hosts: self.rsm.state().hosts().iter()
                            .map(|h| files::Known { name: h.name.clone(), node: h.node, ip: h.ipc(),
                                                    keys: h.keys.clone() })
                            .collect(),
                    }),
                    _ => None,
                };
                let data = match (joined, outcome) {
                    (Some(j), _) => {
                        let mut d = alloc::vec![1u8];
                        d.extend_from_slice(&j.encode());
                        d
                    }
                    (None, Ok(Answer::Refused(why))) => [&[0u8][..], why.says().as_bytes()].concat(),
                    (None, Err(why)) => [&[0u8][..], why.as_bytes()].concat(),
                    (None, Ok(_)) => [&[0u8][..], b"joined, and no cluster to say"].concat(),
                };
                let _ = self.mb.send(to, TAG_JOINED, 0, &data);
            }
        }
    }

    /// `cmd` handed to the leader's member, `w` told its answer.
    fn hand_to_leader(&mut self, cmd: Command, w: Waiting) {
        let leader = self.rsm.leader();
        let Some(host) = self.host_of(leader) else {
            self.finish(w, Err(String::from("no leader known yet -- ask again in a moment")));
            return;
        };
        let request = self.next_request;
        self.next_request += 1;
        let mut data = Vec::new();
        data.extend_from_slice(&request.to_le_bytes());
        data.extend_from_slice(&cmd.encode());
        let to = alloc::format!("{}@{}", NAME, host);
        if let Err(e) = self.mb.send_to(&to, TAG_PROPOSE, 0, &data) {
            self.finish(w, Err(alloc::format!("the leader, {}, out of reach: {}", host, e.name())));
            return;
        }
        self.forwarded.insert(request, (w, boot_time_ns() + ANSWER_MS * NS_PER_MS, leader));
    }

    /// A command of the shell's: submitted here as the leader, handed to the
    /// leader otherwise.
    fn on_request(&mut self, r: Request) {
        if self.rsm.role() == Role::Leader {
            self.submit(r.cmd, Waiting::Local(r.done));
        } else {
            self.hand_to_leader(r.cmd, Waiting::Local(r.done));
        }
    }

    /// `cmd` submitted under the member's session, as the leader.
    fn submit(&mut self, cmd: Command, w: Waiting) {
        if self.rsm.role() != Role::Leader {
            self.finish(w, Err(String::from("not the leader any more -- ask again")));
            return;
        }
        /* The leader's own removal: the lead handed first to a voter the
           registry keeps, while every host still reaches this one -- the
           removal drops its connections as each member applies it, and a
           group of two would lose the one the hand-over needs. The command
           asked again reaches the new leader */
        if let Command::Remove { host, .. } = &cmd {
            if self.rsm.state().host(host).is_some_and(|h| h.node == self.me.node) {
                let conf = self.rsm.node().membership();
                let keeps = self.rsm.state().voters();
                let to = conf.voters.iter().copied().find(|&v| v != self.me.node && keeps.contains(&v));
                let why = match to.map(|to| (to, self.rsm.transfer_leader(to))) {
                    Some((to, Ok(()))) => {
                        if self.handing_to != Some(to) {
                            klog!(0, "cluster: asked to remove this host, the leader: the lead handed to node {} first",
                                  to);
                            self.handing_to = Some(to);
                        }
                        "this host leads: the lead is handed on first -- ask again in a moment"
                    }
                    _ => "this host leads, and no other host votes yet to take the lead -- ask again in a moment",
                };
                self.finish(w, Err(String::from(why)));
                return;
            }
        }
        let Some(client) = self.session else {
            self.finish(w, Err(String::from("the leader is not ready yet -- ask again in a moment")));
            return;
        };
        self.seq += 1;
        match self.rsm.submit(client, self.seq, &cmd.encode()) {
            Ok(_) => {
                self.pending.insert(self.seq, w);
            }
            Err(e) => self.finish(w, Err(alloc::format!("not taken: {:?}", e))),
        }
    }

    /* ---- what the member does ---- */

    fn on_event(&mut self, e: Event) {
        match e {
            Event::Registered { origin, request, client } => {
                if origin == self.me.node && Some(request) == self.registering {
                    self.registering = None;
                    self.session = Some(client);
                }
            }
            Event::Applied { client, seq, answer, .. } if Some(client) == self.session => {
                if let Some(w) = self.pending.remove(&seq) {
                    let outcome = Answer::decode(&answer).ok_or_else(|| String::from("an answer that is none"));
                    self.finish(w, outcome);
                }
            }
            Event::Expired { client, seq } | Event::Stale { client, seq } | Event::Lost { client, seq }
                if Some(client) == self.session =>
            {
                if let Some(w) = self.pending.remove(&seq) {
                    self.finish(w, Err(String::from("the member's session went -- ask again")));
                }
                self.session = None;
            }
            _ => {}
        }
    }

    fn on_role(&mut self, role: Role) {
        if role == Role::Leader {
            /* A session of its own to submit under, asked at once */
            self.session = None;
            let request = self.next_request;
            self.next_request += 1;
            if self.rsm.register(request).is_ok() {
                self.registering = Some(request);
            }
        } else {
            /* What waited here is asked again by whoever waits */
            let waiting = core::mem::take(&mut self.pending);
            for (_, w) in waiting {
                self.finish(w, Err(String::from("the leader changed -- ask again")));
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
    fn on_leader(&mut self, leader: NodeId) {
        if leader == raft::NONE {
            return;
        }
        let gone: Vec<u64> = self.forwarded.iter().filter(|(_, &(_, _, to))| to != leader).map(|(&r, _)| r).collect();
        for r in gone {
            if let Some((w, _, _)) = self.forwarded.remove(&r) {
                self.finish(w, Err(String::from("the leader changed -- ask again")));
            }
        }
    }

    fn send_raft(&self, m: raft::Message) {
        let Some(host) = self.host_of(m.to) else { return };
        let mut bytes = Vec::new();
        if raft::wire::encode(&m, &mut bytes).is_err() {
            return;
        }
        let tag = if bytes.len() > BULK_BYTES { TAG_RAFT | TAG_BULK } else { TAG_RAFT };
        let _ = self.mb.send_to(&alloc::format!("{}@{}", NAME, host), tag, 0, &bytes);
    }

    /// Everything raft has to do, done.
    fn process(&mut self) -> Result<(), String> {
        for _ in 0..ROUNDS {
            let out = self.rsm.process().map_err(|e| alloc::format!("{:?}", e))?;
            let idle = out.messages.is_empty() && out.events.is_empty() && out.soft_state.is_none();
            for m in out.messages {
                self.send_raft(m);
            }
            if let Some(ss) = out.soft_state {
                self.on_role(ss.role);
                self.on_leader(ss.leader);
            }
            for e in out.events {
                self.on_event(e);
            }
            if idle && !self.rsm.node().has_ready() {
                break;
            }
        }
        Ok(())
    }

    /// ipcnet's peers made the registry's, when they are not.
    fn tell_peers(&mut self) {
        /* The registry's peers once it has the cluster; until then, a host
           that joined has those its join was answered with -- and the host
           that made the cluster has its `init` on the way */
        let state = self.rsm.state();
        let (cluster, peers): (_, Vec<control::Peer>) = match state.cluster() {
            /* A peer the registry has no host of is a host invited: it
               reaches this member alone, by its name, until it has joined */
            Some(c) => (c.id, state.peers_of(&self.me.name).into_iter()
                .map(|p| {
                    let only = state.host(&p.name).is_none().then(|| String::from(NAME));
                    control::Peer { name: p.name, ip: p.ip, keys: p.keys, only }
                })
                .collect()),
            None if !self.me.hosts.is_empty() => (self.me.cluster, self.me.hosts.iter()
                .filter(|h| h.name != self.me.name)
                .map(|h| control::Peer { name: h.name.clone(), ip: h.ip, keys: h.keys.clone(), only: None })
                .collect()),
            None => return,
        };
        if self.peers_told.as_ref() == Some(&peers) || boot_time_ns() < self.peers_retry {
            return;
        }
        let msg = control::Peers { cluster, peers: peers.clone() };
        if self.mb.send_to(control::NAME, control::TAG_PEERS, 0, &msg.encode()).is_ok() {
            self.peers_told = Some(peers);
        }
    }

    /// As the leader, raft's membership a step nearer the registry's
    /// voters and learners.
    fn keep_membership(&mut self) {
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
        let state = self.rsm.state();
        let (voters, learners) = (state.voters(), state.learners());
        let member = |n: &u64| conf.voters.contains(n) || conf.learners.contains(n);
        let next = if let Some(&n) = conf.voters.iter().chain(conf.learners.iter())
            .find(|n| !voters.contains(n) && !learners.contains(n))
        {
            /* A removed host taken out -- never the leader itself, which
               hands the lead to a voter the registry keeps, whose own
               turn here then takes it out. Asked again at every turn: raft
               takes the same hand-over once */
            if n == self.me.node {
                if let Some(&to) = conf.voters.iter().find(|&&v| v != n && voters.contains(&v)) {
                    if self.rsm.transfer_leader(to).is_ok() && self.handing_to != Some(to) {
                        klog!(0, "cluster: this host is removed: the lead handed to node {}", to);
                        self.handing_to = Some(to);
                    }
                }
                None
            } else {
                Some(ConfChange { kind: ChangeKind::Remove, node: n })
            }
        } else if let Some(&n) = voters.iter().chain(learners.iter()).find(|n| !member(n)) {
            /* A host added: a learner first, whatever it will be */
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
                klog!(0, "cluster: membership: {:?} node {}", cc.kind, cc.node);
                self.change = Some((cc, now));
            }
        }
    }

    /// Requests handed to the leader that have waited past their time.
    fn expire_forwarded(&mut self) {
        let now = boot_time_ns();
        let late: Vec<u64> = self.forwarded.iter().filter(|(_, (_, until, _))| *until <= now).map(|(&r, _)| r).collect();
        for r in late {
            if let Some((w, _, _)) = self.forwarded.remove(&r) {
                self.finish(w, Err(String::from("no answer from the leader -- ask again")));
            }
        }
    }
}

/// The member's task: until the module goes, or its log fails.
pub fn run((shared, mut m): (Arc<Shared>, Member)) {
    let tick = TICK_MS * NS_PER_MS;
    let mut next_tick = boot_time_ns() + tick;
    shared.view.with(|v| *v = Some(m.view(None)));
    while !shared.stop.load(Ordering::Acquire) {
        let wait = (next_tick.saturating_sub(boot_time_ns()) / NS_PER_MS).clamp(1, POLL_MS);
        if let Some(msg) = m.mb.recv(Duration::from_millis(wait)) {
            m.on_message(msg);
            for _ in 0..BATCH {
                match m.mb.try_recv() {
                    Some(more) => m.on_message(more),
                    None => break,
                }
            }
        }
        if boot_time_ns() >= next_tick {
            m.rsm.tick();
            next_tick += tick;
            m.expire_forwarded();
        }
        let requests: Vec<Request> = shared.requests.with(|q| q.drain(..).collect());
        for r in requests {
            m.on_request(r);
        }
        if let Err(why) = m.process() {
            klog!(0, "cluster: the member stopped: {} -- `rmmod` and `insmod` start it again", why);
            shared.view.with(|v| *v = Some(m.view(Some(why))));
            return;
        }
        m.tell_peers();
        m.keep_membership();
        shared.view.with(|v| *v = Some(m.view(None)));
    }
}
