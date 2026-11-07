//! The cluster's registry (src/crates/common/cluster/registry, plans/cluster.md) held to a
//! model of its own:
//!
//! - **Alone**: one registry and the model take the same commands --
//!   init, invitations, joins, removes, revokes, maintenance, addresses,
//!   keys added and dropped, bytes that are none -- from
//!   small pools of names, addresses, keys and invitation numbers, so that
//!   they meet, under a clock that goes on and now and then back (another
//!   leader's). After every command its answer and everything the registry
//!   shows -- the cluster, its hosts, the invitations open, the hosts
//!   removed, the voters and each host's peers -- must be the model's. Its
//!   snapshot must read back as itself; a snapshot changed -- a byte, cut
//!   short, more bytes, noise -- must be refused or read back as a state that
//!   holds to every rule; and a replica started from a snapshot part way
//!   must end as the registry does.
//! - **A group**: the registry as the cluster module runs it, through
//!   raft's rsm on three or five members over a network that loses,
//!   doubles, reorders and cuts, a client with a session proposing to
//!   whichever member it thinks leads and trying another after a timeout.
//!   Once calm, every member's registry must be the same, and the model's
//!   once it takes the commands in the order the log applied them --
//!   answering each as the members did.

use std::collections::BTreeMap;

use raft::{Config, Event, MemStorage, Message, NodeId, Rsm, StateMachine};
use registry::{Answer, Command, Fingerprint, Refusal, Role, State};

use crate::rng::{mix, Rng};

/* ---- the model ---- */

#[derive(Clone, Debug, PartialEq)]
struct MHost {
    name: String,
    node: u64,
    keys: Vec<Fingerprint>,
    /// The ipc address first, the rest by role.
    addrs: Vec<(Role, u32)>,
    maintenance: bool,
    joined: u64,
}

impl MHost {
    fn ip(&self) -> u32 {
        self.addrs[0].1
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MRetired {
    key: Fingerprint,
    node: u64,
    at: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct MInvite {
    id: u64,
    host: String,
    ip: u32,
    key: Fingerprint,
    hash: [u8; 32],
    expires: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct MRemoved {
    name: String,
    node: u64,
    keys: Vec<Fingerprint>,
    at: u64,
}

#[derive(Clone, Debug, Default)]
struct Model {
    cluster: Option<([u8; 16], String, u64)>,
    hosts: Vec<MHost>,
    invites: Vec<MInvite>,
    removed: Vec<MRemoved>,
    retired: Vec<MRetired>,
    next: u64,
}

const M_HOSTS_MAX: usize = 16;
const M_REMOVED_MAX: usize = 1024;
const M_RETIRED_MAX: usize = 1024;
const M_KEYS_MAX: usize = 2;

/// A name, as the model has the rule: a DNS label, lowercase.
fn m_name(s: &str) -> bool {
    (1..=63).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

impl Model {
    fn key_gone(&self, key: &Fingerprint) -> bool {
        self.removed.iter().any(|r| r.keys.contains(key)) || self.retired.iter().any(|r| r.key == *key)
    }

    fn apply(&mut self, cmd: Option<&Command>) -> Answer {
        let Some(cmd) = cmd else { return Answer::Refused(Refusal::Malformed) };
        let now = cmd.now();
        self.invites.retain(|i| i.expires > now);
        let refuse = Answer::Refused;
        match cmd {
            Command::Init { now, cluster, name, host, key, ip } => {
                if self.cluster.is_some() {
                    return refuse(Refusal::Initialized);
                }
                if !m_name(name) || !m_name(host) {
                    return refuse(Refusal::BadName);
                }
                if *ip == 0 {
                    return refuse(Refusal::BadAddress);
                }
                self.cluster = Some((*cluster, name.clone(), *now));
                self.hosts.push(MHost { name: host.clone(), node: 1, keys: vec![*key], addrs: vec![(Role::Ipc, *ip)],
                                        maintenance: false, joined: *now });
                self.next = 2;
                Answer::Done { node: 1 }
            }
            _ if self.cluster.is_none() => refuse(Refusal::NotInitialized),
            Command::Invite { now, id, host, ip, key, secret_hash, expires } => {
                let key_used = self.hosts.iter().any(|h| h.keys.contains(key)) || self.invites.iter().any(|i| i.key == *key);
                if !m_name(host) {
                    refuse(Refusal::BadName)
                } else if *ip == 0 {
                    refuse(Refusal::BadAddress)
                } else if self.hosts.iter().any(|h| &h.name == host) || self.invites.iter().any(|i| &i.host == host) {
                    refuse(Refusal::HostExists)
                } else if self.invites.iter().any(|i| i.id == *id) {
                    refuse(Refusal::InvitationExists)
                } else if self.hosts.iter().any(|h| h.addrs.iter().any(|a| a.1 == *ip))
                    || self.invites.iter().any(|i| i.ip == *ip)
                {
                    refuse(Refusal::AddressInUse)
                } else if key_used {
                    refuse(Refusal::KeyInUse)
                } else if self.key_gone(key) {
                    refuse(Refusal::KeyRemoved)
                } else if *expires <= *now {
                    refuse(Refusal::Expired)
                } else if self.hosts.len() + self.invites.len() >= M_HOSTS_MAX {
                    refuse(Refusal::Full)
                } else {
                    self.invites.push(MInvite { id: *id, host: host.clone(), ip: *ip, key: *key, hash: *secret_hash,
                                                expires: *expires });
                    Answer::Done { node: 0 }
                }
            }
            Command::Join { now, invitation } => {
                let Some(at) = self.invites.iter().position(|i| i.id == *invitation) else {
                    return refuse(Refusal::InvitationGone);
                };
                let key = self.invites[at].key;
                if self.hosts.iter().any(|h| h.keys.contains(&key)) {
                    return refuse(Refusal::KeyInUse);
                }
                if self.key_gone(&key) {
                    return refuse(Refusal::KeyRemoved);
                }
                let inv = self.invites.remove(at);
                let node = self.next;
                self.next += 1;
                self.hosts.push(MHost { name: inv.host, node, keys: vec![key], addrs: vec![(Role::Ipc, inv.ip)],
                                        maintenance: false, joined: *now });
                Answer::Done { node }
            }
            Command::Remove { now, host } => {
                let Some(at) = self.hosts.iter().position(|h| &h.name == host) else {
                    return refuse(Refusal::NoSuchHost);
                };
                if self.hosts.len() == 1 {
                    return refuse(Refusal::LastHost);
                }
                if self.removed.len() >= M_REMOVED_MAX {
                    return refuse(Refusal::Full);
                }
                let h = self.hosts.remove(at);
                self.removed.push(MRemoved { name: h.name, node: h.node, keys: h.keys, at: *now });
                Answer::Done { node: h.node }
            }
            Command::Revoke { invitation, .. } => match self.invites.iter().position(|i| i.id == *invitation) {
                Some(at) => {
                    self.invites.remove(at);
                    Answer::Done { node: 0 }
                }
                None => refuse(Refusal::InvitationGone),
            },
            Command::Maintenance { host, on, .. } => match self.hosts.iter_mut().find(|h| &h.name == host) {
                Some(h) => {
                    h.maintenance = *on;
                    Answer::Done { node: h.node }
                }
                None => refuse(Refusal::NoSuchHost),
            },
            Command::Address { host, role, ip, .. } => {
                let Some(at) = self.hosts.iter().position(|h| &h.name == host) else {
                    return refuse(Refusal::NoSuchHost);
                };
                if *ip == 0 && *role == Role::Ipc {
                    return refuse(Refusal::BadAddress);
                }
                let taken = self.hosts.iter().enumerate().any(|(i, h)| i != at && h.addrs.iter().any(|a| a.1 == *ip))
                    || self.invites.iter().any(|i| i.ip == *ip);
                if *ip != 0 && taken {
                    return refuse(Refusal::AddressInUse);
                }
                let h = &mut self.hosts[at];
                h.addrs.retain(|a| a.0 != *role);
                if *ip != 0 {
                    h.addrs.push((*role, *ip));
                }
                h.addrs.sort();
                Answer::Done { node: h.node }
            }
            Command::AddKey { host, key, .. } => {
                let Some(at) = self.hosts.iter().position(|h| &h.name == host) else {
                    return refuse(Refusal::NoSuchHost);
                };
                if self.hosts[at].keys.contains(key) {
                    return Answer::Done { node: self.hosts[at].node };
                }
                if self.hosts.iter().any(|h| h.keys.contains(key)) || self.invites.iter().any(|i| i.key == *key) {
                    return refuse(Refusal::KeyInUse);
                }
                if self.key_gone(key) {
                    return refuse(Refusal::KeyRemoved);
                }
                if self.hosts[at].keys.len() >= M_KEYS_MAX {
                    return refuse(Refusal::KeysFull);
                }
                self.hosts[at].keys.push(*key);
                Answer::Done { node: self.hosts[at].node }
            }
            Command::DropKey { now, host, key } => {
                let Some(at) = self.hosts.iter().position(|h| &h.name == host) else {
                    return refuse(Refusal::NoSuchHost);
                };
                let node = self.hosts[at].node;
                if !self.hosts[at].keys.contains(key) {
                    return if self.retired.iter().any(|r| r.key == *key && r.node == node) {
                        Answer::Done { node }
                    } else {
                        refuse(Refusal::NotItsKey)
                    };
                }
                if self.hosts[at].keys.len() == 1 {
                    return refuse(Refusal::LastKey);
                }
                if self.retired.len() >= M_RETIRED_MAX {
                    return refuse(Refusal::Full);
                }
                self.hosts[at].keys.retain(|k| k != key);
                self.retired.push(MRetired { key: *key, node, at: *now });
                Answer::Done { node }
            }
        }
    }
}

/// Where the registry and the model differ, if they do: everything the
/// registry shows.
fn differs(s: &State, m: &Model) -> Option<String> {
    let cluster = s.cluster().map(|c| (c.id, c.name.clone(), c.created));
    if cluster != m.cluster {
        return Some(format!("the cluster {:?}, the model's {:?}", cluster, m.cluster));
    }
    let hosts: Vec<MHost> = s.hosts().iter()
        .map(|h| MHost { name: h.name.clone(), node: h.node, keys: h.keys.clone(),
                         addrs: h.addrs.iter().map(|a| (a.role, a.ip)).collect(), maintenance: h.maintenance,
                         joined: h.joined })
        .collect();
    if hosts != m.hosts {
        return Some(format!("the hosts {:?}, the model's {:?}", hosts, m.hosts));
    }
    if s.hosts().iter().any(|h| !h.labels.is_empty() || h.ipc() != h.addrs[0].ip || h.addrs[0].role != Role::Ipc) {
        return Some(String::from("a host with a label nothing gave it, or its ipc address not first"));
    }
    let retired: Vec<MRetired> = s.retired().iter().map(|r| MRetired { key: r.key, node: r.node, at: r.retired })
        .collect();
    if retired != m.retired {
        return Some(format!("the keys retired {:?}, the model's {:?}", retired, m.retired));
    }
    let invites: Vec<MInvite> = s.invitations().iter()
        .map(|i| MInvite { id: i.id, host: i.host.clone(), ip: i.ip, key: i.key, hash: i.secret_hash,
                           expires: i.expires })
        .collect();
    if invites != m.invites {
        return Some(format!("the invitations {:?}, the model's {:?}", invites, m.invites));
    }
    let removed: Vec<MRemoved> = s.removed().iter()
        .map(|r| MRemoved { name: r.name.clone(), node: r.node, keys: r.keys.clone(), at: r.removed })
        .collect();
    if removed != m.removed {
        return Some(format!("the hosts removed {:?}, the model's {:?}", removed, m.removed));
    }
    /* The policy: the first five to join vote, every other host a peer */
    let nodes: Vec<u64> = m.hosts.iter().map(|h| h.node).collect();
    let cut = nodes.len().min(5);
    if s.voters() != nodes[..cut] || s.learners() != nodes[cut..] {
        return Some(format!("voters {:?} and learners {:?} of hosts {:?}", s.voters(), s.learners(), nodes));
    }
    for h in &m.hosts {
        let peers = s.peers_of(&h.name);
        /* Every other host, and every host invited, by the key it will come with */
        let mut want: Vec<(String, u32, Vec<Fingerprint>)> = m.hosts.iter().filter(|o| o.name != h.name)
            .map(|o| (o.name.clone(), o.ip(), o.keys.clone())).collect();
        want.extend(m.invites.iter().filter(|i| i.host != h.name).map(|i| (i.host.clone(), i.ip, vec![i.key])));
        let got: Vec<(String, u32, Vec<Fingerprint>)> = peers.into_iter().map(|p| (p.name, p.ip, p.keys)).collect();
        if got != want {
            return Some(format!("{}'s peers {:?}, the model's {:?}", h.name, got, want));
        }
    }
    None
}

/* ---- what is proposed ---- */

const NAMES: [&str; 6] = ["ex44", "ax41", "box3", "node-4", "n5", "a"];
const BAD_NAMES: [&str; 5] = ["", "Ex44", "a b", "ex44.lan", "--"];
const IPS: [u32; 6] = [0x0a00_0001, 0x0a00_0002, 0x0a00_0003, 0x0a00_0004, 0xc0a8_0001, 0x4135_5dd5];
const IDS: [u64; 5] = [1, 2, 3, 0x7fff_ffff_ffff_ffff, u64::MAX];

fn key(k: u64) -> Fingerprint {
    let mut f = [0u8; 32];
    f[..8].copy_from_slice(&mix(0x6b65_79, k).to_le_bytes());
    f
}

/// What a run's commands draw from: narrow pools of names, addresses, keys
/// and invitation numbers, so that they meet -- or, on every third seed,
/// wide ones and few removes, so that the cluster fills to its limit and
/// has learners.
struct Pools {
    names: Vec<String>,
    ips: Vec<u32>,
    ids: Vec<u64>,
    keys: u64,
    wide: bool,
}

impl Pools {
    fn new(wide: bool) -> Pools {
        if wide {
            Pools {
                names: (0..24).map(|i| format!("h{}", i)).collect(),
                ips: (1..=30).map(|i| 0x0a00_0100 + i).collect(),
                ids: (1..=24).collect(),
                keys: 64,
                wide,
            }
        } else {
            Pools {
                names: NAMES.iter().map(|n| String::from(*n)).collect(),
                ips: IPS.to_vec(),
                ids: IDS.to_vec(),
                keys: 10,
                wide,
            }
        }
    }

    fn name(&self, r: &mut Rng) -> String {
        if r.chance(6) {
            if r.chance(25) {
                return "x".repeat(64);
            }
            return String::from(BAD_NAMES[r.below(BAD_NAMES.len() as u64) as usize]);
        }
        self.names[r.below(self.names.len() as u64) as usize].clone()
    }

    fn ip(&self, r: &mut Rng) -> u32 {
        if r.chance(4) { 0 } else { self.ips[r.below(self.ips.len() as u64) as usize] }
    }

    fn id(&self, r: &mut Rng) -> u64 {
        self.ids[r.below(self.ids.len() as u64) as usize]
    }

    fn key(&self, r: &mut Rng) -> Fingerprint {
        key(r.below(self.keys))
    }
}

/// The clock of whoever proposes: on, and now and then back -- a leader
/// whose clock is behind the last one's.
fn clock(r: &mut Rng, now: &mut u64) -> u64 {
    if r.chance(5) {
        *now = now.saturating_sub(r.below(900));
    } else {
        *now += r.below(120);
    }
    *now
}

/// A command, mostly one that can do something in `m`'s state.
fn command(r: &mut Rng, p: &Pools, m: &Model, now: &mut u64) -> Command {
    let now = clock(r, now);
    if m.cluster.is_none() && r.chance(70) {
        return Command::Init { now, cluster: mix(r.next(), 3).to_le_bytes().repeat(2).try_into().unwrap_or([0; 16]),
                               name: p.name(r), host: p.name(r), key: p.key(r), ip: p.ip(r) };
    }
    let invitation = |r: &mut Rng| match m.invites.get(r.below(m.invites.len() as u64 + 1) as usize) {
        Some(i) if r.chance(80) => i.id,
        _ => p.id(r),
    };
    let host = |r: &mut Rng| match m.hosts.get(r.below(m.hosts.len() as u64 + 1) as usize) {
        Some(h) if r.chance(80) => h.name.clone(),
        _ => p.name(r),
    };
    /* A fifth of the time, a change of a host's -- its maintenance, an
       address, a key added or dropped -- from the same narrow pools */
    if r.chance(20) {
        let h = host(r);
        return match r.below(4) {
            0 => Command::Maintenance { now, host: h, on: r.chance(50) },
            1 => {
                let role = [Role::Ipc, Role::Underlay, Role::Public][r.below(3) as usize];
                Command::Address { now, host: h, role, ip: p.ip(r) }
            }
            2 => Command::AddKey { now, host: h, key: p.key(r) },
            _ => {
                /* Mostly one of the host's own keys, its last one too */
                let own = m.hosts.iter().find(|x| x.name == h).and_then(|x| x.keys.get(r.below(2) as usize).copied());
                let key = match own {
                    Some(k) if r.chance(75) => k,
                    _ => p.key(r),
                };
                Command::DropKey { now, host: h, key }
            }
        };
    }
    /* Removes few in a wide run, so that it fills */
    let removes = if p.wide { 4 } else { 16 };
    let pick = r.below(100);
    if pick < 4 {
        Command::Init { now, cluster: [7; 16], name: p.name(r), host: p.name(r), key: p.key(r), ip: p.ip(r) }
    } else if pick < 39 {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&r.next().to_le_bytes());
        let expires = if r.chance(10) { now.saturating_sub(r.below(60)) } else { now + r.range(1, 1800) };
        Command::Invite { now, id: p.id(r), host: p.name(r), ip: p.ip(r), key: p.key(r), secret_hash: hash, expires }
    } else if pick < 73 {
        Command::Join { now, invitation: invitation(r) }
    } else if pick < 73 + removes {
        Command::Remove { now, host: host(r) }
    } else {
        Command::Revoke { now, invitation: invitation(r) }
    }
}

/// An entry's bytes: the command's, or now and then bytes that are none --
/// cut short, a byte changed, noise.
fn entry(r: &mut Rng, cmd: &Command) -> Vec<u8> {
    let mut b = cmd.encode();
    if r.chance(8) {
        match r.below(3) {
            0 => b.truncate(r.below(b.len() as u64) as usize),
            1 => {
                let at = r.below(b.len() as u64) as usize;
                b[at] ^= 1 << r.below(8);
            }
            _ => b = (0..r.below(40)).map(|_| r.next() as u8).collect(),
        }
    }
    b
}

/// A snapshot's bytes changed: a byte, cut short, more bytes, noise.
fn spoil(r: &mut Rng, mut b: Vec<u8>) -> Vec<u8> {
    match r.below(4) {
        0 if !b.is_empty() => {
            let at = r.below(b.len() as u64) as usize;
            b[at] = b[at].wrapping_add(1 + r.below(255) as u8);
        }
        1 => b.truncate(r.below(b.len() as u64 + 1) as usize),
        2 => b.extend((0..r.range(1, 16)).map(|_| r.next() as u8)),
        _ => b = (0..r.below(200)).map(|_| r.next() as u8).collect(),
    }
    b
}

/* ---- alone ---- */

/// `RAFT_REGISTRY_STATS=1`: what each run's commands were answered, and
/// how far its registry got -- what says the target reaches the states it
/// is for.
fn stats(what: &str, seed: u64, answers: &BTreeMap<String, u64>, hosts_max: usize, removed: usize) {
    if std::env::var_os("RAFT_REGISTRY_STATS").is_some() {
        eprintln!("registry {} seed {}: hosts at most {}, removed {}, answers {:?}", what, seed, hosts_max, removed,
                  answers);
    }
}

fn answer_name(a: &Answer) -> String {
    match a {
        Answer::Done { .. } => String::from("Done"),
        Answer::Refused(why) => format!("{:?}", why),
    }
}

fn alone(seed: u64, steps: u64) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x7265_6769));
    let mut answers: BTreeMap<String, u64> = BTreeMap::new();
    let mut hosts_max = 0;
    let pools = Pools::new(seed % 3 == 0);
    let mut s = State::new();
    let mut m = Model::default();
    let mut replica: Option<State> = None;
    let mut now = 1_000_000;
    for step in 0..steps {
        let cmd = command(&mut r, &pools, &m, &mut now);
        let bytes = entry(&mut r, &cmd);
        let decoded = Command::decode(&bytes);
        if bytes == cmd.encode() && decoded.as_ref() != Some(&cmd) {
            return Err(format!("step {}: {:?} does not read back from its bytes", step, cmd));
        }
        let got = Answer::decode(&s.apply(step, &bytes))
            .ok_or_else(|| format!("step {}: an answer that does not read back", step))?;
        let want = m.apply(decoded.as_ref());
        if got != want {
            return Err(format!("step {}: {:?} answered {:?}, the model {:?}", step, decoded, got, want));
        }
        *answers.entry(answer_name(&got)).or_default() += 1;
        hosts_max = hosts_max.max(s.hosts().len());
        if let Some(rep) = replica.as_mut() {
            rep.apply(step, &bytes);
        }
        if let Some(d) = differs(&s, &m) {
            return Err(format!("step {} after {:?}: {}", step, decoded, d));
        }
        /* Its snapshot read back as itself, and in pieces as one */
        if r.chance(10) {
            let bytes = s.encode();
            let back = State::decode(&bytes).map_err(|e| format!("step {}: its own snapshot refused: {:?}", step, e))?;
            if back != s {
                return Err(format!("step {}: its snapshot read back as another state", step));
            }
            let mut frozen = s.freeze();
            let mut pieces = Vec::new();
            loop {
                let before = pieces.len();
                raft::Frozen::write(&mut frozen, r.range(1, 64) as usize, &mut pieces);
                if pieces.len() == before {
                    break;
                }
            }
            if pieces != bytes {
                return Err(format!("step {}: the frozen state's pieces are not its snapshot", step));
            }
            if replica.is_none() && r.chance(30) {
                replica = Some(back);
            }
        }
        /* A snapshot changed: refused, or a state that holds to the rules
           -- which `decode` checks -- and reads back as itself */
        if r.chance(5) {
            let spoiled = spoil(&mut r, s.encode());
            if let Ok(t) = State::decode(&spoiled) {
                let again = State::decode(&t.encode())
                    .map_err(|e| format!("step {}: a state read from a changed snapshot refused as itself: {:?}", step, e))?;
                if again != t {
                    return Err(format!("step {}: a state read from a changed snapshot does not read back", step));
                }
            }
        }
    }
    if let Some(rep) = replica {
        if rep != s {
            return Err(String::from("a replica started from a snapshot part way ends another state"));
        }
    }
    stats("alone", seed, &answers, hosts_max, s.removed().len());
    Ok(())
}

/* ---- a group ---- */

/// How long the client waits for an answer, in steps, before it asks
/// another member.
const TIMEOUT: u64 = 40;
/// Steps of a group run that are calm at the end: no loss, no cut.
const CALM: u64 = 2500;

type Member = Rsm<MemStorage, State>;

struct Group {
    r: Rng,
    now: u64,
    members: Vec<(NodeId, Member)>,
    wire: Vec<(u64, NodeId, Message)>,
    cut: Option<NodeId>,
    calm: bool,
}

impl Group {
    fn member(&mut self, id: NodeId) -> Option<&mut Member> {
        self.members.iter_mut().find(|(i, _)| *i == id).map(|(_, m)| m)
    }

    /// A step: arrivals, a tick and a `process` of every member: the events,
    /// by member.
    fn step(&mut self) -> Result<Vec<(NodeId, Event)>, String> {
        self.now += 1;
        let now = self.now;
        let mut due = Vec::new();
        let mut k = 0;
        while k < self.wire.len() {
            if self.wire[k].0 <= now {
                due.push(self.wire.swap_remove(k));
            } else {
                k += 1;
            }
        }
        while !due.is_empty() {
            let (_, to, m) = due.swap_remove(self.r.below(due.len() as u64) as usize);
            if self.cut.is_some_and(|c| (c == to) != (c == m.from)) {
                continue;
            }
            if let Some(rsm) = self.member(to) {
                let _ = rsm.step(m);
            }
        }
        let mut events = Vec::new();
        for i in 0..self.members.len() {
            let (id, rsm) = &mut self.members[i];
            let id = *id;
            rsm.tick();
            let out = rsm.process().map_err(|e| format!("member {} stopped: {:?}", id, e))?;
            for m in out.messages {
                if !self.calm && self.r.chance(5) {
                    continue;
                }
                let copies = if !self.calm && self.r.chance(2) { 2 } else { 1 };
                for _ in 0..copies {
                    let at = now + self.r.range(1, 3);
                    self.wire.push((at, m.to, m.clone()));
                }
            }
            events.extend(out.events.into_iter().map(|e| (id, e)));
        }
        Ok(events)
    }
}

/// What the client is doing.
enum Doing {
    Registering { request: u64, of: NodeId, until: u64 },
    Proposing { of: NodeId, until: u64 },
    Idle,
}

fn group(seed: u64, steps: u64) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x6772_6f75));
    let n: u64 = if r.chance(50) { 3 } else { 5 };
    let voters: Vec<NodeId> = (1..=n).collect();
    let mut members = Vec::new();
    for id in 1..=n {
        let mut cfg = Config::new(id);
        cfg.seed = mix(seed, id);
        let rsm = Rsm::new(cfg, MemStorage::bootstrap(&voters, &[]), State::new(), r.next())
            .map_err(|e| format!("member {}: {:?}", id, e))?;
        members.push((id, rsm));
    }
    let mut g = Group { r: Rng::new(mix(seed, 0x6e65_74)), now: 0, members, wire: Vec::new(), cut: None, calm: false };
    let pools = Pools::new(seed % 3 == 0);

    /* The client: one session, one command at a time, retried of another
       member after a timeout under the same number */
    let mut model_view = Model::default();
    let mut clock_now = 1_000_000u64;
    let mut session: Option<u64> = None;
    let mut seq = 0u64;
    let mut doing = Doing::Idle;
    let mut next_request = 1u64;
    let mut asked: BTreeMap<u64, Command> = BTreeMap::new();
    /* Each command's first application: its index, and what it was answered */
    let mut applied: BTreeMap<u64, (u64, Answer)> = BTreeMap::new();
    let mut leader: NodeId = 1;
    let total = steps + CALM;
    for step in 0..total {
        g.calm = step >= steps;
        if !g.calm && g.r.chance(1) {
            g.cut = if g.cut.is_none() { Some(g.r.range(1, n)) } else { None };
        }
        if g.calm {
            g.cut = None;
        }
        let events = g.step()?;
        for (id, e) in events {
            match e {
                Event::Registered { origin, request, client } => {
                    if let Doing::Registering { request: asked_for, of, .. } = doing {
                        if origin == of && request == asked_for {
                            session = Some(client);
                            doing = Doing::Idle;
                        }
                    }
                }
                Event::Applied { client, seq: s, index, answer } if Some(client) == session => {
                    let answer = Answer::decode(&answer)
                        .ok_or_else(|| format!("member {}: an answer that does not read back", id))?;
                    match applied.get(&s) {
                        Some(&(_, first)) if first != answer => {
                            return Err(format!("command {} answered {:?} and then {:?}", s, first, answer));
                        }
                        Some(_) => {}
                        None => {
                            applied.insert(s, (index, answer));
                        }
                    }
                    if s == seq && matches!(doing, Doing::Proposing { .. }) {
                        doing = Doing::Idle;
                    }
                }
                Event::Expired { client, .. } if Some(client) == session => {
                    return Err(String::from("the client's session expired"));
                }
                _ => {}
            }
        }
        if let Some(l) = g.members.iter().map(|(_, m)| m.leader()).find(|&l| l != 0) {
            leader = l;
        }
        match doing {
            Doing::Registering { until, .. } | Doing::Proposing { until, .. } if g.now >= until => {
                /* Asked of whoever leads now, or of anyone */
                leader = if g.r.chance(50) { leader } else { g.r.range(1, n) };
                doing = match doing {
                    Doing::Registering { .. } => Doing::Idle,
                    _ => Doing::Proposing { of: 0, until: 0 },
                };
            }
            _ => {}
        }
        match doing {
            Doing::Idle if session.is_none() => {
                let request = next_request;
                next_request += 1;
                if let Some(rsm) = g.member(leader) {
                    if rsm.register(request).is_ok() {
                        doing = Doing::Registering { request, of: leader, until: g.now + TIMEOUT };
                    }
                }
            }
            Doing::Idle if !g.calm || step < total - CALM / 2 => {
                if let Some(c) = session {
                    seq += 1;
                    let cmd = command(&mut g.r, &pools, &model_view, &mut clock_now);
                    /* The model's view of what may be there: a guess, only
                       to make commands that meet */
                    model_view.apply(Some(&cmd));
                    asked.insert(seq, cmd.clone());
                    if let Some(rsm) = g.member(leader) {
                        let _ = rsm.submit(c, seq, &cmd.encode());
                    }
                    doing = Doing::Proposing { of: leader, until: g.now + TIMEOUT };
                }
            }
            Doing::Proposing { of: 0, .. } => {
                if let (Some(c), Some(cmd)) = (session, asked.get(&seq)) {
                    let bytes = cmd.encode();
                    if let Some(rsm) = g.member(leader) {
                        let _ = rsm.submit(c, seq, &bytes);
                    }
                    doing = Doing::Proposing { of: leader, until: g.now + TIMEOUT };
                }
            }
            _ => {}
        }
    }

    /* Calm: every member the same registry, at the same entry */
    let first = g.members[0].1.state().clone();
    let at = g.members[0].1.applied();
    for (id, m) in &g.members {
        if m.applied() != at || *m.state() != first {
            return Err(format!("member {} applied to {} and member 1 to {}: their registries differ", id, m.applied(),
                               at));
        }
    }
    /* The model, taking the commands in the order the log applied them,
       answers each as the members did and ends as they did */
    let mut order: Vec<(u64, u64)> = applied.iter().map(|(&s, &(index, _))| (index, s)).collect();
    order.sort_unstable();
    let mut model = Model::default();
    for (index, s) in order {
        let cmd = asked.get(&s).ok_or_else(|| format!("command {} applied and never asked", s))?;
        let want = model.apply(Some(cmd));
        let got = applied[&s].1;
        if got != want {
            return Err(format!("command {} ({:?}) at {} answered {:?}, the model {:?}", s, cmd, index, got, want));
        }
    }
    if let Some(d) = differs(&first, &model) {
        return Err(format!("the group's registry, calm: {}", d));
    }
    if applied.is_empty() {
        return Err(String::from("nothing applied in the whole run"));
    }
    let mut answers: BTreeMap<String, u64> = BTreeMap::new();
    for (_, a) in applied.values() {
        *answers.entry(answer_name(a)).or_default() += 1;
    }
    stats("group", seed, &answers, first.hosts().len(), first.removed().len());
    Ok(())
}

/// A run: the registry alone, and on every fourth seed a group.
pub fn run(seed: u64, steps: u64) -> Result<(), String> {
    alone(seed, steps)?;
    if seed % 4 == 0 {
        group(seed, steps)?;
    }
    Ok(())
}

/* ---- scenarios ---- */

fn init(s: &mut State) -> Result<(), String> {
    let a = s.apply_command(&Command::Init { now: 100, cluster: [1; 16], name: "test".into(), host: "ex44".into(),
                                             key: key(1), ip: IPS[0] });
    if a != (Answer::Done { node: 1 }) {
        return Err(format!("init answered {:?}", a));
    }
    Ok(())
}

fn invite(s: &mut State, now: u64, id: u64, host: &str, ip: u32, k: Fingerprint, expires: u64) -> Answer {
    s.apply_command(&Command::Invite { now, id, host: host.into(), ip, key: k, secret_hash: [9; 32], expires })
}

/// A token is good once: the second join on its invitation is refused; and
/// a host is invited only with a key nobody has, nor is invited with.
fn join_once() -> Result<(), String> {
    let mut s = State::new();
    init(&mut s)?;
    if invite(&mut s, 110, 5, "ax41", IPS[1], key(2), 1000) != (Answer::Done { node: 0 }) {
        return Err(String::from("the invitation refused"));
    }
    let first = s.apply_command(&Command::Join { now: 120, invitation: 5 });
    let again = s.apply_command(&Command::Join { now: 121, invitation: 5 });
    if first != (Answer::Done { node: 2 }) || again != Answer::Refused(Refusal::InvitationGone) {
        return Err(format!("a join and its token again answered {:?} and {:?}", first, again));
    }
    let taken = invite(&mut s, 130, 6, "box3", IPS[2], key(2), 1000);
    let _ = invite(&mut s, 131, 7, "box4", IPS[3], key(4), 1000);
    let twice = invite(&mut s, 132, 8, "box5", IPS[4], key(4), 1000);
    if taken != Answer::Refused(Refusal::KeyInUse) || twice != Answer::Refused(Refusal::KeyInUse) {
        return Err(format!("invitations with a host's key and an invitation's answered {:?} and {:?}", taken, twice));
    }
    Ok(())
}

/// An invitation past its time, by the entry's clock, is gone at the next
/// entry -- whatever the entry is, a refused one included -- and the same
/// on every member that applies it.
fn expiry() -> Result<(), String> {
    let mut s = State::new();
    init(&mut s)?;
    let _ = invite(&mut s, 200, 5, "ax41", IPS[1], key(2), 500);
    /* Refused, and its clock lets the invitation go */
    let _ = s.apply_command(&Command::Remove { now: 500, host: "nobody".into() });
    if s.invitation(5).is_some() {
        return Err(String::from("an invitation past its time kept"));
    }
    let late = s.apply_command(&Command::Join { now: 300, invitation: 5 });
    if late != Answer::Refused(Refusal::InvitationGone) {
        return Err(format!("a join on an invitation let go of answered {:?}", late));
    }
    Ok(())
}

/// A removed host's key is never invited again, under a new name; its node
/// is never given again.
fn removed_key() -> Result<(), String> {
    let mut s = State::new();
    init(&mut s)?;
    let _ = invite(&mut s, 110, 5, "ax41", IPS[1], key(2), 1000);
    let _ = s.apply_command(&Command::Join { now: 120, invitation: 5 });
    if s.apply_command(&Command::Remove { now: 130, host: "ax41".into() }) != (Answer::Done { node: 2 }) {
        return Err(String::from("the remove refused"));
    }
    let back = invite(&mut s, 140, 6, "ax41-new", IPS[1], key(2), 1000);
    if back != Answer::Refused(Refusal::KeyRemoved) {
        return Err(format!("a removed key invited again answered {:?}", back));
    }
    let _ = invite(&mut s, 141, 6, "ax41-new", IPS[1], key(3), 1000);
    let fresh = s.apply_command(&Command::Join { now: 151, invitation: 6 });
    if fresh != (Answer::Done { node: 3 }) {
        return Err(format!("a new key's join answered {:?}, node 3 expected", fresh));
    }
    if s.apply_command(&Command::Remove { now: 160, host: "ex44".into() }) != (Answer::Done { node: 1 })
        || s.apply_command(&Command::Remove { now: 161, host: "ax41-new".into() }) != Answer::Refused(Refusal::LastHost)
    {
        return Err(String::from("the last host removed, or the one before refused"));
    }
    Ok(())
}

/// Snapshots that break the state's rules -- each made by changing a sound
/// one -- are refused: a key twice, a node past the next, an invitation of
/// an address a host has.
fn snapshot_rules() -> Result<(), String> {
    let mut s = State::new();
    init(&mut s)?;
    let _ = invite(&mut s, 110, 5, "ax41", IPS[1], key(2), 1000);
    let _ = s.apply_command(&Command::Join { now: 120, invitation: 5 });
    let _ = invite(&mut s, 130, 6, "box3", IPS[2], key(3), 1000);
    let sound = s.encode();
    if State::decode(&sound).as_ref() != Ok(&s) {
        return Err(String::from("a sound snapshot refused"));
    }
    /* Each change made at the bytes of a field found in the sound one */
    let find = |what: &[u8]| sound.windows(what.len()).position(|w| w == what);
    let mut changes: Vec<(&str, Vec<u8>)> = Vec::new();
    if let Some(at) = find(&key(2)) {
        let mut b = sound.clone();
        b[at..at + 32].copy_from_slice(&key(1));
        changes.push(("a key twice", b));
    }
    if let Some(at) = find(&3u64.to_le_bytes()) {
        let mut b = sound.clone();
        b[at..at + 8].copy_from_slice(&2u64.to_le_bytes());
        changes.push(("the next node not past every node", b));
    }
    if let Some(at) = find(&IPS[2].to_le_bytes()) {
        let mut b = sound.clone();
        b[at..at + 4].copy_from_slice(&IPS[1].to_le_bytes());
        changes.push(("an invitation at a host's address", b));
    }
    if changes.len() != 3 {
        return Err(String::from("the snapshot's fields not found where they should be"));
    }
    for (what, b) in changes {
        if State::decode(&b).is_ok() {
            return Err(format!("a snapshot with {} taken", what));
        }
    }
    Ok(())
}

/// The first five hosts to join vote and the rest learn; a voter removed,
/// the next host to have joined votes in its place.
fn voters() -> Result<(), String> {
    let mut s = State::new();
    init(&mut s)?;
    for k in 0..6u64 {
        let host = format!("h{}", k);
        let ip = 0x0a00_0100 + k as u32;
        if invite(&mut s, 110 + k, 10 + k, &host, ip, key(100 + k), 10_000) != (Answer::Done { node: 0 })
            || s.apply_command(&Command::Join { now: 120 + k, invitation: 10 + k }) != (Answer::Done { node: 2 + k })
        {
            return Err(format!("{} not let in", host));
        }
    }
    if s.voters() != [1, 2, 3, 4, 5] || s.learners() != [6, 7] {
        return Err(format!("seven hosts: voters {:?}, learners {:?}", s.voters(), s.learners()));
    }
    let _ = s.apply_command(&Command::Remove { now: 200, host: "h1".into() });
    if s.voters() != [1, 2, 4, 5, 6] || s.learners() != [7] {
        return Err(format!("node 3 removed: voters {:?}, learners {:?}", s.voters(), s.learners()));
    }
    Ok(())
}

/// Two hosts, ex44 (node 1, key 1, IPS[0]) and ax41 (node 2, key 3,
/// IPS[1]).
fn two_hosts(s: &mut State) -> Result<(), String> {
    init(s)?;
    if invite(s, 110, 5, "ax41", IPS[1], key(3), 1000) != (Answer::Done { node: 0 })
        || s.apply_command(&Command::Join { now: 120, invitation: 5 }) != (Answer::Done { node: 2 })
    {
        return Err(String::from("ax41 not let in"));
    }
    Ok(())
}

/// A host moves to a new key: the new one beside the old -- both pinned by
/// the others -- then the old dropped and retired, never taken again by
/// anyone; two keys at most, never none; asked again, each answers as it
/// did. A snapshot that gives a host a retired key is refused.
fn keys() -> Result<(), String> {
    let mut s = State::new();
    two_hosts(&mut s)?;
    let add = |s: &mut State, now: u64, host: &str, k: u64| {
        s.apply_command(&Command::AddKey { now, host: host.into(), key: key(k) })
    };
    let drop = |s: &mut State, now: u64, host: &str, k: u64| {
        s.apply_command(&Command::DropKey { now, host: host.into(), key: key(k) })
    };
    if add(&mut s, 130, "ex44", 2) != (Answer::Done { node: 1 }) || add(&mut s, 131, "ex44", 2) != (Answer::Done { node: 1 }) {
        return Err(String::from("a new key, and the same again, not taken"));
    }
    let peers = s.peers_of("ax41");
    if peers.len() != 1 || peers[0].keys != [key(1), key(2)] {
        return Err(format!("ax41 does not pin both of ex44's keys: {:?}", peers));
    }
    let full = add(&mut s, 132, "ex44", 4);
    let other = add(&mut s, 133, "ex44", 3);
    if full != Answer::Refused(Refusal::KeysFull) || other != Answer::Refused(Refusal::KeyInUse) {
        return Err(format!("a third key and another host's answered {:?} and {:?}", full, other));
    }
    if drop(&mut s, 140, "ex44", 1) != (Answer::Done { node: 1 }) || drop(&mut s, 141, "ex44", 1) != (Answer::Done { node: 1 }) {
        return Err(String::from("the old key's drop, or the same again, refused"));
    }
    if s.retired().len() != 1 || s.retired()[0].key != key(1) || s.retired()[0].node != 1 {
        return Err(format!("the dropped key not retired as ex44's: {:?}", s.retired()));
    }
    let back = add(&mut s, 150, "ax41", 1);
    let invited = invite(&mut s, 151, 9, "box3", IPS[2], key(1), 1000);
    if back != Answer::Refused(Refusal::KeyRemoved) || invited != Answer::Refused(Refusal::KeyRemoved) {
        return Err(format!("a retired key taken again: {:?}, invited {:?}", back, invited));
    }
    let last = drop(&mut s, 160, "ex44", 2);
    let none = drop(&mut s, 161, "ex44", 9);
    if last != Answer::Refused(Refusal::LastKey) || none != Answer::Refused(Refusal::NotItsKey) {
        return Err(format!("the last key's drop and a key not the host's answered {:?} and {:?}", last, none));
    }
    /* ex44's one key, key 2, made the retired key 1 in the snapshot's bytes */
    let sound = s.encode();
    let Some(at) = sound.windows(32).position(|w| w == key(2)) else {
        return Err(String::from("ex44's key not found in the snapshot"));
    };
    let mut b = sound.clone();
    b[at..at + 32].copy_from_slice(&key(1));
    if State::decode(&b).is_ok() {
        return Err(String::from("a snapshot giving a host a retired key taken"));
    }
    Ok(())
}

/// A host's addresses: the ipc one first and never taken away, the others
/// set and taken away by role, none another host's or an invitation's;
/// its peers call it at its ipc address as it moves.
fn addresses() -> Result<(), String> {
    let mut s = State::new();
    two_hosts(&mut s)?;
    let set = |s: &mut State, now: u64, host: &str, role: Role, ip: u32| {
        s.apply_command(&Command::Address { now, host: host.into(), role, ip })
    };
    if set(&mut s, 130, "ex44", Role::Public, IPS[3]) != (Answer::Done { node: 1 })
        || set(&mut s, 131, "ex44", Role::Underlay, IPS[2]) != (Answer::Done { node: 1 })
    {
        return Err(String::from("ex44's public and underlay addresses refused"));
    }
    let roles: Vec<Role> = s.host("ex44").map(|h| h.addrs.iter().map(|a| a.role).collect()).unwrap_or_default();
    if roles != [Role::Ipc, Role::Underlay, Role::Public] {
        return Err(format!("ex44's addresses out of order: {:?}", roles));
    }
    let taken = set(&mut s, 132, "ax41", Role::Public, IPS[2]);
    let ipc_none = set(&mut s, 133, "ex44", Role::Ipc, 0);
    let nobody = set(&mut s, 134, "nobody", Role::Public, IPS[4]);
    if taken != Answer::Refused(Refusal::AddressInUse) || ipc_none != Answer::Refused(Refusal::BadAddress)
        || nobody != Answer::Refused(Refusal::NoSuchHost)
    {
        return Err(format!("another's address, no ipc address and no host answered {:?}, {:?}, {:?}", taken,
                           ipc_none, nobody));
    }
    if set(&mut s, 140, "ex44", Role::Underlay, 0) != (Answer::Done { node: 1 })
        || s.host("ex44").is_some_and(|h| h.addrs.iter().any(|a| a.role == Role::Underlay))
    {
        return Err(String::from("ex44's underlay address not taken away"));
    }
    /* Moved: ax41 calls it at its new address, and the old is free */
    if set(&mut s, 150, "ex44", Role::Ipc, IPS[4]) != (Answer::Done { node: 1 })
        || s.peers_of("ax41").first().map(|p| p.ip) != Some(IPS[4])
    {
        return Err(format!("ex44 moved, and ax41's peers {:?}", s.peers_of("ax41")));
    }
    if invite(&mut s, 151, 9, "box3", IPS[0], key(5), 1000) != (Answer::Done { node: 0 }) {
        return Err(String::from("an invitation at the address ex44 left refused"));
    }
    Ok(())
}

/// A host's maintenance, on and off: asked twice, answered alike.
fn maintenance() -> Result<(), String> {
    let mut s = State::new();
    two_hosts(&mut s)?;
    let set = |s: &mut State, host: &str, on: bool| {
        s.apply_command(&Command::Maintenance { now: 130, host: host.into(), on })
    };
    if set(&mut s, "ax41", true) != (Answer::Done { node: 2 }) || set(&mut s, "ax41", true) != (Answer::Done { node: 2 })
        || !s.host("ax41").is_some_and(|h| h.maintenance) || s.host("ex44").is_some_and(|h| h.maintenance)
    {
        return Err(String::from("ax41's maintenance not on, or ex44's on"));
    }
    if set(&mut s, "ax41", false) != (Answer::Done { node: 2 }) || s.host("ax41").is_some_and(|h| h.maintenance)
        || set(&mut s, "nobody", true) != Answer::Refused(Refusal::NoSuchHost)
    {
        return Err(String::from("ax41's maintenance not off, or a host nobody is answered"));
    }
    Ok(())
}

pub const SCENARIOS: &[(&str, fn() -> Result<(), String>)] = &[
    ("registry_keys", keys),
    ("registry_addresses", addresses),
    ("registry_maintenance", maintenance),
    ("registry_voters", voters),
    ("registry_join_once", join_once),
    ("registry_expiry", expiry),
    ("registry_removed_key", removed_key),
    ("registry_snapshot_rules", snapshot_rules),
];
