//! The registry's member: raftmember's engine -- raft's replicated state
//! machine over `registry::State`, a task of its own (`cluster/raft`),
//! `cluster.raft` on every host -- and what is the registry's own:
//!
//! - **The hosts its nodes are**: the registry's; before it has the cluster,
//!   on a host that joined, those its join was answered with.
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
//! - **A probe** (`TAG_PROBE`): whether a member is there, what `cluster
//!   recover` asks every other voter first.
//! - **The leader's own removal** waits for the lead handed to a voter the
//!   registry keeps, while every host still reaches this one -- the removal
//!   drops its connections as each member applies it, and a group of two
//!   would lose the one the hand-over needs.
//!
//! Commands are made so that asking twice is harmless -- an invitation's
//! number is random, a remove of a host gone is refused -- so a request lost
//! with a leader is asked again by whoever waits for it.

use alloc::string::String;
use alloc::vec::Vec;

use ipcwire::control;
use kcore::ipc::{Addr, Message, TAG_BULK};
use kcore::klog;
use kcore::time::boot_time_ns;
use raft::{Role, Rsm};
use raftmember::{App, Core, Handing, Outcome, Pending};
use registry::{Answer, Command, State};

use crate::files;

/// The member's mailbox.
pub const NAME: &str = "cluster.raft";
const NS_PER_MS: u64 = 1_000_000;
/// How long after ipcnet refused a list of peers it is told again -- the
/// same list, should nothing have changed: a refusal said in the log at
/// most this often.
const PEERS_RETRY_MS: u64 = 10_000;
/// How long a host just removed stays a peer, held to this member's
/// mailbox: what the member owes it -- the answer to the removal it asked,
/// the commit of it -- reaches it before the link goes (#92).
const LEAVING_MS: u64 = 5_000;
/* Every peer a host has -- the other hosts and the open invitations, which
   the registry holds to HOSTS_MAX together -- fits a list ipcnet takes */
const _: () = assert!(registry::HOSTS_MAX - 1 <= control::PEERS_MAX);
/// A host's join, from the host an invitation names: the invitation's
/// number, then its secret.
pub const TAG_JOIN_REQ: u32 = raftmember::TAG_APP;
/// Its answer: a byte -- 1 joined, 0 not -- then `Joined`'s bytes, or why
/// not in words.
pub const TAG_JOINED: u32 = raftmember::TAG_APP + 1;
/// Whether a member is there, from a host of the cluster: what `cluster
/// recover` asks every other voter first, refusing while any answers.
pub const TAG_PROBE: u32 = raftmember::TAG_APP + 2;
/// Its answer: the member's node, its term and its role's byte.
pub const TAG_PROBED: u32 = raftmember::TAG_APP + 3;

pub type Log = raftmember::Log;
pub type Member = raftmember::Member<Registry>;
pub type Shared = raftmember::Shared<Registry>;
pub type View = raftmember::View<Registry>;

/// A host joining by an invitation: answered with its node and the hosts.
pub struct Joiner {
    to: Addr,
    invitation: u64,
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
            b.extend_from_slice(&h.port.to_le_bytes());
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
            let port = u16::from_le_bytes(take(2)?.try_into().ok()?);
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
            hosts.push(files::Known { name: String::from(name), node, ip, port, keys });
        }
        (at == b.len() && node != 0).then_some(Joined { node, cluster, hosts })
    }
}

/// The registry's half of the member.
pub struct Registry {
    me: files::Member,
    /// What ipcnet was last told its peers are.
    peers_told: Option<Vec<control::Peer>>,
    /// Not before when ipcnet is told again, after it refused.
    peers_retry: u64,
    /// The invitations whose joins this member is taking now: a join asked
    /// again meanwhile is not taken twice -- the second would find the
    /// invitation used by the first, and its refusal could reach the host
    /// before the first's answer.
    joining: Vec<u64>,
    /// Hosts just removed, kept peers until then (`LEAVING_MS`), held to
    /// this member's mailbox -- where a removed host is refused whatever it
    /// says, as no host of the registry's: the answer this member owes one
    /// gets out before its link goes. A leader's own removal is asked
    /// again of the leader it handed the lead to, whose apply of it took
    /// the host off at once, its answer lost with the link (#92).
    leaving: Vec<(control::Peer, u64)>,
}

/// The member over its log, its mailbox named. A host that joined knows
/// the cluster by `me.hosts`, as its join was answered, until its registry
/// has the cluster.
pub fn new(rsm: Rsm<Log, State>, me: files::Member) -> Result<Member, String> {
    let node = me.node;
    raftmember::Member::new(Registry { me, peers_told: None, peers_retry: 0, joining: Vec::new(), leaving: Vec::new() },
                            rsm, node)
}

impl App for Registry {
    type State = State;
    type Command = Command;
    type Answer = Answer;
    type Waiter = Joiner;

    const NAME: &'static str = NAME;
    const LOG: &'static str = "cluster";

    fn encode_command(cmd: &Command) -> Vec<u8> {
        cmd.encode()
    }

    fn decode_command(bytes: &[u8]) -> Option<Command> {
        Command::decode(bytes)
    }

    fn encode_answer(answer: &Answer) -> Vec<u8> {
        answer.encode()
    }

    fn decode_answer(bytes: &[u8]) -> Option<Answer> {
        Answer::decode(bytes)
    }

    /// The node host `name` is: the registry's -- or, where the registry
    /// this member has applied has neither the host nor its node and has
    /// not removed it, what this host's join was answered. A member whose
    /// registry is older than the joining of a host that leads -- restored
    /// from a snapshot taken before it -- takes that leader's messages by
    /// it. (Only a member with no cluster at all did: one restored so
    /// refused all its leader sent, and stayed on that snapshot.)
    fn node_of(&self, core: &Core<Registry>, name: &str) -> Option<u64> {
        let state = core.state();
        if let Some(h) = state.host(name) {
            return Some(h.node);
        }
        self.joined(core, |h| h.name == name).map(|h| h.node)
    }

    /// The host node `node` is, as `node_of` has them.
    fn host_of(&self, core: &Core<Registry>, node: u64) -> Option<String> {
        if let Some(h) = core.state().host_of_node(node) {
            return Some(h.name.clone());
        }
        self.joined(core, |h| h.node == node).map(|h| h.name.clone())
    }

    fn wanted(&self, core: &Core<Registry>) -> (Vec<u64>, Vec<u64>) {
        (core.state().voters(), core.state().learners())
    }

    fn on_message(&mut self, core: &mut Core<Registry>, m: &Message) {
        match m.tag & !TAG_BULK {
            TAG_JOIN_REQ => self.on_join(core, m),
            TAG_PROBE => self.on_probe(core, m),
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

    /// The leader's own removal: the lead handed first to a voter the
    /// registry keeps, while every host still reaches this one. The command
    /// asked again reaches the new leader.
    fn before_submit(&mut self, core: &mut Core<Registry>, cmd: &Command) -> Result<(), String> {
        let Command::Remove { host, .. } = cmd else { return Ok(()) };
        if !core.state().host(host).is_some_and(|h| h.node == core.node()) {
            return Ok(());
        }
        let keeps = core.state().voters();
        let why = match core.hand_lead_on(&keeps) {
            Handing::To { node, first } => {
                if first {
                    klog!(0, "cluster: asked to remove this host, the leader: the lead handed to node {} first", node);
                }
                "this host leads: the lead is handed on first -- ask again in a moment"
            }
            Handing::Waiting => "this host leads, and the lead could not be handed on just now -- ask again in a moment",
            Handing::NoOne => "this host leads, and no other host that votes is heard from to take the lead -- ask \
                               again in a moment",
        };
        Err(String::from(why))
    }

    fn finish(&mut self, core: &mut Core<Registry>, w: Joiner, outcome: Outcome<Registry>, _index: u64) {
        let Joiner { to, invitation } = w;
        self.joining.retain(|&i| i != invitation);
        let joined = match outcome {
            Ok(Answer::Done { node }) => core.state().cluster().map(|c| Joined {
                node,
                cluster: c.id,
                hosts: core.state().hosts().iter()
                    .map(|h| files::Known { name: h.name.clone(), node: h.node, ip: h.ipc(), port: h.ipc_port(),
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
        let _ = core.mailbox().send(to, TAG_JOINED, 0, &data);
    }

    fn turn(&mut self, core: &mut Core<Registry>) {
        self.tell_peers(core);
    }
}

impl Registry {
    /// A host this host's join was answered with that `which` names, where
    /// the registry applied here has neither its name nor its node -- it is
    /// older than that host's joining -- and has not removed it.
    fn joined(&self, core: &Core<Registry>, which: impl Fn(&files::Known) -> bool) -> Option<&files::Known> {
        let state = core.state();
        self.me.hosts.iter().find(|h| which(h)).filter(|h| {
            state.host(&h.name).is_none() && state.host_of_node(h.node).is_none()
                && !state.removed().iter().any(|r| r.name == h.name)
        })
    }

    /// A member asking whether this one is there: answered for a host of
    /// the cluster alone.
    fn on_probe(&mut self, core: &mut Core<Registry>, m: &Message) {
        if core.sender_node(self, m).is_none() {
            core.note_refused();
            return;
        }
        let mut data = Vec::with_capacity(17);
        data.extend_from_slice(&core.node().to_le_bytes());
        data.extend_from_slice(&core.rsm().node().term().to_le_bytes());
        data.push(match core.role() {
            Role::Leader => 2,
            Role::Follower => 1,
            Role::Candidate | Role::PreCandidate => 0,
        });
        let _ = core.mailbox().send(m.from, TAG_PROBED, m.reference, &data);
    }

    /// A host's join: taken from the host its invitation names, with the
    /// invitation's secret -- and committed by the leader.
    fn on_join(&mut self, core: &mut Core<Registry>, m: &Message) {
        let refuse = |core: &Core<Registry>, why: &str| {
            let mut data = alloc::vec![0u8];
            data.extend_from_slice(why.as_bytes());
            let _ = core.mailbox().send(m.from, TAG_JOINED, m.reference, &data);
        };
        let (Some(name), true) = (kcore::ipc::peer_name(m.from), m.data.len() == 40) else {
            core.note_refused();
            return;
        };
        let invitation = u64::from_le_bytes(m.data[..8].try_into().unwrap_or([0; 8]));
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&m.data[8..]);
        if self.joining.contains(&invitation) {
            /* The first's answer is on its way */
            return;
        }
        let Some(inv) = core.state().invitation(invitation) else {
            /* Never answered again with its node: a host whose folder was
               wiped would come back under its old node with nothing of what
               it voted for */
            match core.state().host(&name) {
                Some(h) => refuse(core, &alloc::format!("{} joined already, as node {} -- the answer lost on the way: \
                                                         `cluster remove {}` on a member and invite it again",
                                                        name, h.node, name)),
                None => refuse(core, "no such invitation: used, taken back, past its time or never made"),
            }
            return;
        };
        /* The host it names, over a connection pinned by its key, and its
           secret: anything else is not this invitation's */
        if inv.host != name || crate::token::secret_hash(&secret) != inv.secret_hash {
            core.note_refused();
            refuse(core, "not the host invited, or not its secret");
            return;
        }
        let cmd = Command::Join { now: kcore::time::wall_clock_secs(), invitation };
        self.joining.push(invitation);
        core.ask(self, cmd, Pending::App(Joiner { to: m.from, invitation }));
    }

    /// ipcnet's peers made the registry's, when they are not.
    fn tell_peers(&mut self, core: &mut Core<Registry>) {
        /* The registry's peers once it has the cluster; until then, a host
           that joined has those its join was answered with -- and the host
           that made the cluster has its `init` on the way */
        let state = core.state();
        let (cluster, peers): (_, Vec<control::Peer>) = match state.cluster() {
            /* A peer the registry has no host of is a host invited: it
               reaches this member alone, by its name, until it has joined */
            Some(c) => {
                let mut peers: Vec<control::Peer> = state.peers_of(&self.me.name).into_iter()
                    .map(|p| {
                        let only = state.host(&p.name).is_none().then(|| String::from(NAME));
                        control::Peer { name: p.name, ip: p.ip, port: p.port, keys: p.keys, only }
                    })
                    .collect();
                /* A host removed since ipcnet was told: kept a while, held to
                   this member's mailbox, so that its own removal's answer
                   reaches it */
                let now = boot_time_ns();
                for p in self.peers_told.iter().flatten() {
                    let removed = state.removed().iter().any(|r| r.name == p.name);
                    if removed && !peers.iter().any(|q| q.name == p.name)
                        && !self.leaving.iter().any(|(l, _)| l.name == p.name)
                    {
                        let held = control::Peer { only: Some(String::from(NAME)), ..p.clone() };
                        self.leaving.push((held, now + LEAVING_MS * NS_PER_MS));
                    }
                }
                self.leaving.retain(|(p, until)| *until > now && state.host(&p.name).is_none());
                for (p, _) in &self.leaving {
                    if !peers.iter().any(|q| q.name == p.name) {
                        peers.push(p.clone());
                    }
                }
                /* A host this host's join was answered with that the
                   registry applied here does not have -- older than its
                   joining -- and has not removed: kept, held to this
                   member's mailbox, so that its raft messages reach the
                   member until the registry has it (`node_of`) */
                for h in self.me.hosts.iter() {
                    if h.name != self.me.name && !peers.iter().any(|q| q.name == h.name)
                        && self.joined(core, |k| k.name == h.name).is_some()
                    {
                        peers.push(control::Peer { name: h.name.clone(), ip: h.ip, port: h.port, keys: h.keys.clone(),
                                                   only: Some(String::from(NAME)) });
                    }
                }
                (c.id, peers)
            }
            None if !self.me.hosts.is_empty() => (self.me.cluster, self.me.hosts.iter()
                .filter(|h| h.name != self.me.name)
                .map(|h| control::Peer { name: h.name.clone(), ip: h.ip, port: h.port, keys: h.keys.clone(),
                                         only: None })
                .collect()),
            None => return,
        };
        if self.peers_told.as_ref() == Some(&peers) || boot_time_ns() < self.peers_retry {
            return;
        }
        let msg = control::Peers { cluster, peers: peers.clone() };
        if core.mailbox().send_to(control::NAME, control::TAG_PEERS, 0, &msg.encode()).is_ok() {
            self.peers_told = Some(peers);
        }
    }
}
