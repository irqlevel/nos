//! The mds's member: raftmember's engine -- raft's replicated state machine
//! over NBS's metadata (`nbsmeta::State`), a task of its own (`nbs/mds`),
//! `nbs.mds` on every member -- and what is NBS's own:
//!
//! - **The NBS cluster's id** before every message of the engine's, held to
//!   on each that comes in: an mds takes nothing of another cluster's.
//! - **The hosts its nodes are**: the metadata's members, each a host and
//!   its incarnation; before this host's log has caught up, the members it
//!   was offered the cluster by.
//! - **An offer** (`TAG_OFFER`) to a host that is a member already --
//!   `nbs host add` asked again -- is answered with its incarnation where it
//!   is this cluster's, and declined where it is another's.
//! - **A host that lost its state** answers raft's messages to the member it
//!   was with `TAG_NO_MEMBER` (lib.rs's listener, which runs while this host
//!   is in no cluster). Its claim is taken from that host alone, and kept to
//!   be said -- in the log once a minute, and by `nbs status` -- until the
//!   member is replaced: Raft must never count the vote of a member that
//!   forgot its log, and it does not, the host being no longer that member.
//! - **The state's digest held to the leader's**: every member takes its
//!   state's digest every `nbsmeta::CHECK_EVERY` entries; the leader proposes
//!   its own (`Command::Digest`) as it takes it, and a member whose own
//!   differs stops -- a determinism bug said, not a silent fork.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use codec::{Reader, Writer};
use kcore::ipc::{Message, TAG_BULK};
use kcore::klog;
use kcore::time::boot_time_ns;
use nbsmeta::{Answer, Command, DeviceState, State};
use nbswire::{Answered, Device as CsDevice, Identity as CsIdentity, Query, Question, Refusal as CsRefusal, Told};
use raft::{Role, Rsm};
use raftmember::{App, Core, Locked, Pending};

use crate::files::{self, Known};

/// The member's mailbox, the same on every host of the cluster.
pub const NAME: &str = "nbs.mds";
/// An offer of the cluster: its id and name, then its members (a host and
/// its incarnation each) -- what the host offered knows them by until its
/// log has caught up.
pub const TAG_OFFER: u32 = raftmember::TAG_APP;
/// Its answer: the cluster's id, then the incarnation the host's mds is.
pub const TAG_ACCEPTED: u32 = raftmember::TAG_APP + 1;
/// An offer declined: why, in words.
pub const TAG_DECLINED: u32 = raftmember::TAG_APP + 2;
/// From a host whose mds has no member of the incarnation a raft message
/// was for: the cluster's id, then that incarnation.
pub const TAG_NO_MEMBER: u32 = raftmember::TAG_APP + 3;
/// How often a host's lost member is said in the log at most.
const LOST_SAY_NS: u64 = 60_000_000_000;

pub type Member = raftmember::Member<Mds>;
pub type Shared = raftmember::Shared<Mds>;
pub type View = raftmember::View<Mds>;
/// The hosts that said their mds has no member of the incarnation the
/// metadata has for them: the host, that incarnation, and when it was last
/// said in the log. Kept by the member, read by the shell.
pub type Lost = Arc<Locked<BTreeMap<String, (u64, u64)>>>;

/// An offer of the cluster, as it goes between hosts.
pub struct Offer {
    pub cluster: [u8; 16],
    pub name: String,
    pub members: Vec<Known>,
}

impl Offer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.fixed(&self.cluster);
        w.str(&self.name);
        w.u8(self.members.len().min(nbsmeta::MEMBERS_MAX) as u8);
        for m in self.members.iter().take(nbsmeta::MEMBERS_MAX) {
            w.str(&m.host);
            w.u64(m.id);
        }
        w.buf
    }

    /// What a member offered, read: every count bounded, every name a
    /// host's, no member twice.
    pub fn decode(bytes: &[u8]) -> Option<Offer> {
        let mut r = Reader::new(bytes);
        let cluster = r.fixed()?;
        let name = String::from(r.str(nbsmeta::NAME_MAX)?);
        let n = usize::from(r.u8()?);
        if n == 0 || n > nbsmeta::MEMBERS_MAX || !nbsmeta::valid_name(&name) {
            return None;
        }
        let mut members: Vec<Known> = Vec::with_capacity(n);
        for _ in 0..n {
            let host = String::from(r.str(nbsmeta::NAME_MAX)?);
            let id = r.u64()?;
            if id == 0 || !nbsmeta::valid_name(&host) || members.iter().any(|m| m.host == host || m.id == id) {
                return None;
            }
            members.push(Known { host, id });
        }
        r.done().then_some(Offer { cluster, name, members })
    }
}

/// The cluster's id and an incarnation: an acceptance's bytes, and a
/// no-member answer's.
pub fn encode_id(cluster: &[u8; 16], id: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.fixed(cluster);
    w.u64(id);
    w.buf
}

pub fn decode_id(bytes: &[u8]) -> Option<([u8; 16], u64)> {
    let mut r = Reader::new(bytes);
    let cluster = r.fixed()?;
    let id = r.u64()?;
    r.done().then_some((cluster, id))
}

/// NBS's half of the member.
pub struct Mds {
    me: files::Member,
    lost: Lost,
    /// The module's folder, for what a member that halts says to do.
    dir: String,
    /// The newest checkpoint whose digest this member proposed as leader.
    proposed: u64,
}

/// The member over its log in `dir`, its mailbox named.
pub fn new(rsm: Rsm<raftmember::Log, State>, me: files::Member, lost: Lost, dir: &str) -> Result<Member, String> {
    let id = me.id;
    raftmember::Member::new(Mds { me, lost, dir: String::from(dir), proposed: 0 }, rsm, id)
}

impl App for Mds {
    type State = State;
    type Command = Command;
    type Answer = Answer;
    type Waiter = ();

    const NAME: &'static str = NAME;
    const LOG: &'static str = "nbs";

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

    fn group(&self, _core: &Core<Mds>) -> Option<[u8; 16]> {
        Some(self.me.cluster)
    }

    /// The incarnation host `name`'s member is: the metadata's, or -- before
    /// this host's log has caught up -- the offer's.
    fn node_of(&self, core: &Core<Mds>, name: &str) -> Option<u64> {
        if core.state().cluster().is_some() {
            return core.state().member(name).map(|m| m.id);
        }
        self.me.known.iter().find(|k| k.host == name).map(|k| k.id)
    }

    fn host_of(&self, core: &Core<Mds>, node: u64) -> Option<String> {
        if core.state().cluster().is_some() {
            return core.state().member_of_id(node).map(|m| m.host.clone());
        }
        self.me.known.iter().find(|k| k.id == node).map(|k| k.host.clone())
    }

    fn wanted(&self, core: &Core<Mds>) -> (Vec<u64>, Vec<u64>) {
        (core.state().voters(), core.state().learners())
    }

    fn on_message(&mut self, core: &mut Core<Mds>, m: &Message) {
        match m.tag & !TAG_BULK {
            TAG_OFFER => self.on_offer(core, m),
            TAG_NO_MEMBER => self.on_no_member(core, m),
            nbswire::TAG_QUERY => self.on_cs_query(core, m),
            _ => {}
        }
    }

    /// A host whose member is no longer the one that said it lost its state
    /// -- replaced, or removed -- forgotten; and as the leader, the newest
    /// checkpoint's digest proposed, for every member to hold its own to.
    fn turn(&mut self, core: &mut Core<Mds>) {
        let state = core.state();
        self.lost.with(|l| l.retain(|host, &mut (id, _)| state.member(host).is_some_and(|m| m.id == id)));
        if core.role() != Role::Leader {
            return;
        }
        if let Some((at, digest)) = core.state().checks().latest() {
            if at > self.proposed {
                self.proposed = at;
                core.ask(self, Command::Digest { at, digest }, Pending::App(()));
            }
        }
    }

    /// A state whose digest is not the leader's: this member serves nothing
    /// more -- what it would answer from is not what the others hold.
    fn halt(&self, core: &Core<Mds>) -> Option<String> {
        let d = core.state().checks().diverged()?;
        Some(alloc::format!("this host's state at entry {} is not the leader's (digest {}..., the leader's {}..., seen \
                             at entry {}): a determinism bug, said rather than forked -- its member serves nothing; \
                             `rmmod nbs-mds`, `rm {}` and `insmod` it, then `nbs host replace {}` on another member",
                            d.at, codec::hex(&d.mine[..6]), codec::hex(&d.theirs[..6]), d.seen, self.dir, self.me.host))
    }
}

impl Mds {
    /// A question of this host's cs, answered from the member's applied
    /// state (plans/nbs.md, "The cs"): this host's alone -- another host's
    /// cs asks its own host's mds.
    fn on_cs_query(&mut self, core: &mut Core<Mds>, m: &Message) {
        if m.from.host() != 0 {
            return;
        }
        let Some(q) = Query::decode(&m.data) else { return };
        let told = self.cs_answer(core, &q.question);
        let _ = core.mailbox().send(m.from, nbswire::TAG_TOLD, 0, &Answered { id: q.id, told }.encode());
    }

    fn cs_answer(&self, core: &Core<Mds>, q: &Question) -> Told {
        let s = core.state();
        let Some(cluster) = s.cluster() else { return Told::Refused(CsRefusal::NoMds) };
        let mine = s.cs_of(&self.me.host);
        match q {
            Question::Identity => Told::Identity(mine.map(|cs| CsIdentity {
                cluster: cluster.id,
                cs: cs.id,
                devices: cs.devices.iter().map(|d| CsDevice { ident: d.ident.clone(), device: d.id, slots: d.slots,
                                                                draining: d.state != DeviceState::Active })
                    .collect(),
            })),
            Question::Verify { volume, generation, chunk, position, index, writing } => {
                let applied = core.rsm().applied();
                if applied < *index {
                    return Told::Refused(CsRefusal::Behind { applied });
                }
                let Some(v) = s.volumes().iter().find(|v| v.id == *volume) else {
                    return Told::Refused(CsRefusal::NotTheMap);
                };
                if v.map.get(*position as usize) != Some(&chunk.chunk) {
                    return Told::Refused(CsRefusal::NotTheMap);
                }
                let Some(ch) = s.chunks().get(chunk.chunk as usize).filter(|c| !c.is_free()) else {
                    return Told::Refused(CsRefusal::NotTheMap);
                };
                if ch.incarnation != chunk.incarnation {
                    return Told::Refused(CsRefusal::NotTheMap);
                }
                let (Some(set), Some(me)) = (s.sets().get(ch.set as usize), mine) else {
                    return Told::Refused(CsRefusal::NotHere);
                };
                if !set.cs.contains(&me.id) {
                    return Told::Refused(CsRefusal::NotHere);
                }
                let known = nbscs::Epoch { set: ch.set, version: set.version };
                if known != chunk.epoch {
                    return Told::Refused(CsRefusal::Epoch { known });
                }
                /* A write, or a read of a writer's, at the volume's attach for
                   writing; a read of generation 0 is a read-only one's */
                if (*writing || *generation != 0) && v.writer.as_ref().is_none_or(|w| w.generation != *generation) {
                    return Told::Refused(CsRefusal::NotAttached);
                }
                Told::Verified
            }
            Question::Live { chunks } => Told::Gone(chunks.iter()
                .filter(|(c, inc)| s.chunks().get(*c as usize).is_none_or(|ch| ch.is_free() || ch.incarnation != *inc))
                .map(|&(c, _)| c).collect()),
            Question::Volumes { volumes } => Told::Gone(volumes.iter()
                .filter(|&&v| !s.volumes().iter().any(|x| x.id == v)).copied().collect()),
        }
    }

    /// An offer to this host, a member already: answered with its
    /// incarnation where it is the cluster's -- an add asked again -- and
    /// declined where it is another's.
    fn on_offer(&mut self, core: &mut Core<Mds>, m: &Message) {
        let Some(o) = Offer::decode(&m.data) else {
            core.note_refused();
            return;
        };
        if o.cluster == self.me.cluster {
            let _ = core.mailbox().send(m.from, TAG_ACCEPTED, m.reference, &encode_id(&self.me.cluster, self.me.id));
        } else {
            let why = alloc::format!("{}'s mds is a member of another NBS cluster, {} ({}...), as {}", self.me.host,
                                     self.me.name, codec::hex(&self.me.cluster[..4]), self.me.id);
            let _ = core.mailbox().send(m.from, TAG_DECLINED, m.reference, why.as_bytes());
        }
    }

    /// A host that says its mds has no member of the incarnation the
    /// metadata has for it: its claim, about itself alone, kept and said.
    fn on_no_member(&mut self, core: &mut Core<Mds>, m: &Message) {
        let (Some((cluster, id)), Some(host)) = (decode_id(&m.data), kcore::ipc::peer_name(m.from)) else {
            core.note_refused();
            return;
        };
        if cluster != self.me.cluster || core.state().member(&host).map(|x| x.id) != Some(id) {
            core.note_refused();
            return;
        }
        let now = boot_time_ns();
        let say = self.lost.with(|l| {
            let e = l.entry(host.clone()).or_insert((id, 0));
            let due = e.1 == 0 || now >= e.1 + LOST_SAY_NS;
            if due {
                e.1 = now;
            }
            due
        });
        if say {
            klog!(0, "nbs: {} says its mds has no member {}: its state is gone -- `nbs host replace {}` takes it back as \
                      a new member", host, id, host);
        }
    }
}
