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
//! - **This host's clients** (N8; `TAG_ASK`): a volume attached or let go
//!   of, a replica that did not answer taken out -- each a command, told its
//!   outcome and the entry it was applied at -- and a volume's map, told
//!   from the applied state once it has applied as far as the client read.
//! - **A replica taken out only once the leader finds its cs silent too**
//!   (decision 6): the leader looks at the cs (`Op::Ping`) before it
//!   proposes the change, and refuses it while the cs answers -- a client
//!   whose own way to a healthy cs is broken waits, rather than take a
//!   replica out of sets other volumes share.
//! - **The controller** (N9c): the leader looks at every cs once a second,
//!   and every ten seconds -- sooner, a second after the last plan at the
//!   soonest, where the state moved on or a cs went silent or came back --
//!   proposes what `nbsmeta::repair::plan` makes of the state and of what
//!   it saw: cs's down, up and out, and joins. A cs's silence is counted
//!   from when this leader began to watch it, so a new leader takes none for
//!   down at once. The registry's maintenance flag is not read yet: a host
//!   in maintenance goes out at `out_after` as any other.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use codec::{Reader, Writer};
use kcore::ipc::{Addr, Message, TAG_BOUNCED, TAG_BULK};
use kcore::klog;
use kcore::time::boot_time_ns;
use nbsmeta::repair::{self, Seen};
use nbsmeta::{Answer, Command, Refusal, Replica, State};
use nbswire::scrub::{self, Scrub};
use nbswire::{Answered, Ask, Asking, Op, Query, Question, Request, Said, Saying, Told};
use raft::{Role, Rsm};
use raftmember::{App, Core, Locked, Outcome, Pending, Request as Asked};

use crate::files::{self, Known};

/// The member's mailbox, the same on every host of the cluster.
pub const NAME: &str = nbswire::MDS_NAME;
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
/// How long the leader waits for a cs it looks at, and how long what it
/// found stands before a replica's taking out looks again.
const PROBE_NS: u64 = 250_000_000;
const VERDICT_NS: u64 = 2_000_000_000;
/// How often the leader looks at every cs; how long after its last answer
/// a cs is taken as silent; how often the controller plans at the least.
const WATCH_NS: u64 = 1_000_000_000;
const ANSWERED_NS: u64 = 2_500_000_000;
const PLAN_NS: u64 = 10_000_000_000;
/// The leader's look at a cs, as it goes.
const PROBE: u32 = nbswire::TAG_REQUEST | TAG_BULK;

pub type Member = raftmember::Member<Mds>;
pub type Shared = raftmember::Shared<Mds>;
pub type View = raftmember::View<Mds>;
/// The hosts that said their mds has no member of the incarnation the
/// metadata has for them: the host, that incarnation, and when it was last
/// said in the log. Kept by the member, read by the shell.
pub type Lost = Arc<Locked<BTreeMap<String, (u64, u64)>>>;

/// What the leader's scrub has done and found (N9e), kept by the member for
/// the shell: its counts, and the newest of its findings -- chunk, set, the
/// set's version, and the member unlike the two others where two agree.
#[derive(Default)]
pub struct Scrubs {
    pub stats: scrub::Stats,
    pub found: alloc::collections::VecDeque<(u32, u32, u32, Option<u32>)>,
}
pub type Scrubbed = Arc<Locked<Scrubs>>;
/// How many of the scrub's findings the shell is shown.
const FOUND_KEPT: usize = 16;

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
    /// The leader's looks at cs's in flight, and what each look found: the
    /// cs answered and serves, or not, and when.
    probes: Vec<Probe>,
    verdicts: BTreeMap<u32, (bool, u64)>,
    next_probe: u64,
    /// The controller's (N9c), as the leader: what it has seen of each cs --
    /// since when it watches it, when it last answered and served -- when it
    /// looks next; when it last planned, the entry it had applied then, and
    /// when it plans next at the latest; whether a proposal of its was
    /// refused since; when a silent cs is down and out (`out_after=`); and
    /// how many of its proposals are still to be applied: no plan is made
    /// while one is.
    watch: BTreeMap<u32, (u64, u64)>,
    watch_at: u64,
    planned: u64,
    planned_index: u64,
    plan_at: u64,
    refused: bool,
    after: repair::After,
    repairing: u32,
    /// The leader's scrub (N9e) while this member leads, its pace
    /// (`scrub=`, none where 0), and what it found, for the shell.
    scrub: Option<Scrub>,
    scrub_every_ns: u64,
    scrubbed: Scrubbed,
    /// Each volume's writer on this host, as its attach was applied and its
    /// asks since name it: its generation and its session's mailbox -- what
    /// a snapshot pauses (N10c2). A pause under way, one at a time, and the
    /// next one's number.
    sessions: BTreeMap<String, (u64, Addr)>,
    pausing: Option<Pausing>,
    next_pause: u64,
}

/// A snapshot asked of an attached volume (N10c2): its writer, on this host,
/// paused first, and the snapshot taken at its generation. A generation no
/// attach has: the metadata refuses it, should it ever be proposed.
pub const PAUSE_FIRST: u64 = u64::MAX;

/// How long a pause is waited for before the snapshot is given up.
const PAUSE_WAIT_NS: u64 = 5_000_000_000;

/// A pause for a snapshot of an attached volume (N10c2): of which volume and
/// as what name, the session paused and its generation, the pause's number,
/// when it was asked, whether the snapshot is proposed -- answered then by
/// its outcome alone -- and the shell's request waiting.
struct Pausing {
    volume: String,
    name: String,
    generation: u64,
    session: Addr,
    pause: u64,
    asked: u64,
    proposed: bool,
    request: Asked<Mds>,
}

/// A look at a cs, waiting for its answer.
struct Probe {
    cs: u32,
    id: u64,
    until: u64,
}

/// What a command of NBS's own is waited for by.
pub enum Waiter {
    /// The leader's digest.
    Digest,
    /// A client of this host's, told the outcome as its ask's kind is -- and
    /// for an attach for writing or a detach, the volume's name (N10c2).
    Client { to: Addr, id: u64, kind: Kind, volume: Option<String> },
    /// The controller's proposal.
    Repair,
    /// This host's cs, told its join's end or its giving up as applied (N9).
    Cs { to: Addr, id: u64 },
    /// A snapshot of a writer paused (N10c2).
    Snapshot,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Attach,
    Detach,
    Down,
    /// A copy-up asked, or its end (N10).
    Copy,
}

/// The member over its log in `dir`, its mailbox named.
#[allow(clippy::too_many_arguments)]
pub fn new(rsm: Rsm<raftmember::Log, State>, me: files::Member, lost: Lost, dir: &str, out_after_ms: u64,
           scrub_ms: u64, scrubbed: Scrubbed) -> Result<Member, String> {
    let id = me.id;
    raftmember::Member::new(Mds { me, lost, dir: String::from(dir), proposed: 0, probes: Vec::new(),
                                  verdicts: BTreeMap::new(), next_probe: 1, watch: BTreeMap::new(), watch_at: 0,
                                  planned: 0, planned_index: 0, plan_at: 0, refused: false,
                                  after: repair::After { down_ms: repair::DOWN_AFTER_MS, out_ms: out_after_ms },
                                  repairing: 0, scrub: None, scrub_every_ns: scrub_ms * 1_000_000, scrubbed,
                                  sessions: BTreeMap::new(), pausing: None, next_pause: 1 },
                            rsm, id)
}

impl App for Mds {
    type State = State;
    type Command = Command;
    type Answer = Answer;
    type Waiter = Waiter;

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
            nbswire::TAG_ASK => self.on_client(core, m),
            nbswire::TAG_ANSWER => self.on_answer(core, m),
            TAG_BOUNCED => self.on_bounce(m),
            _ => {}
        }
    }

    /// As the leader, a replica's taking out proposed only once its cs is
    /// found silent: a look at it started, and the change refused for now,
    /// asked again by the client once the look has an answer.
    fn before_submit(&mut self, core: &mut Core<Mds>, cmd: &Command) -> Result<(), String> {
        /* The cs's own refusal as failed is taken as it is: it answers, and
           what it refused is what is wrong */
        let Command::ReplicaDown { set, cs, refused: false, name, generation } = cmd else { return Ok(()) };
        /* What applying refuses or does again anyway -- not the writer's, no
           such set or replica, one out already, the last active one -- is
           applied as it is, with no look */
        let s = core.state();
        if s.volume(name).and_then(|v| v.writer.as_ref()).is_none_or(|w| w.generation != *generation) {
            return Ok(());
        }
        let Some(st) = s.sets().get(*set as usize) else { return Ok(()) };
        /* An active member, not the last -- or the joiner (N9), whose join a
           client with a broken way to it would otherwise drop at every try */
        let joining = st.joiner.is_some_and(|j| j.cs == *cs);
        let active = st.cs.iter().position(|c| c == cs)
            .is_some_and(|at| st.replicas[at] == Replica::Active && st.active() > 1);
        if !joining && !active {
            return Ok(());
        }
        let now = boot_time_ns();
        match self.verdicts.get(cs) {
            Some(&(false, when)) if now < when + VERDICT_NS => return Ok(()),
            Some(&(true, when)) if now < when + VERDICT_NS => {
                return Err(alloc::format!("cs {} answers the leader: the client's own way to it is broken, and it is \
                                           not taken out -- the client waits", cs));
            }
            _ => {}
        }
        self.probe(core, *cs);
        Err(alloc::format!("the leader is looking at cs {} -- ask again", cs))
    }

    fn finish(&mut self, core: &mut Core<Mds>, w: Waiter, outcome: Outcome<Mds>, index: u64) {
        let (to, id, kind) = match w {
            Waiter::Client { to, id, kind, volume } => {
                /* The writer's session on this host as its attach, or its
                   detach, is applied (N10c2) */
                if let (Some(name), Ok(Answer::Attached { generation })) = (volume, &outcome) {
                    match kind {
                        Kind::Attach => {
                            self.sessions.insert(name, (*generation, to));
                        }
                        Kind::Detach if *generation != 0 => {
                            self.sessions.remove(&name);
                        }
                        _ => {}
                    }
                }
                (to, id, kind)
            }
            /* Applied -- taken or refused -- the session goes on, its map read
               anew at this entry; not applied, a word now could let it write
               before the snapshot is applied late: none, and it attaches again
               past its wait */
            Waiter::Snapshot => {
                let Some(p) = self.pausing.take() else { return };
                if index > 0 {
                    let said = Said::GoOn { pause: p.pause, index };
                    let _ = core.mailbox().send(p.session, nbswire::TAG_SAID, 0, &Saying { id: 0, said }.encode());
                }
                p.request.done.with(|s| *s = Some((outcome, index)));
                return;
            }
            Waiter::Cs { to, id } => {
                let told = match outcome {
                    Ok(Answer::Set { .. }) => Told::Verified,
                    /* Not the join it was: ended or given up meanwhile */
                    Ok(_) => Told::Refused(nbswire::Refusal::NotTheJoiner),
                    /* No leader, or not ready: asked again */
                    Err(_) => Told::Refused(nbswire::Refusal::NoMds),
                };
                let _ = core.mailbox().send(to, nbswire::TAG_TOLD, 0, &Answered { id, told }.encode());
                return;
            }
            Waiter::Digest => return,
            Waiter::Repair => {
                self.repairing = self.repairing.saturating_sub(1);
                match outcome {
                    Ok(Answer::Refused(why)) => {
                        klog!(1, "nbs: a repair's proposal refused: {}", why.says());
                        self.refused = true;
                    }
                    Err(why) => klog!(1, "nbs: a repair's proposal not applied: {}", why),
                    Ok(_) => {}
                }
                return;
            }
        };
        let said = match (outcome, kind) {
            (Ok(Answer::Attached { generation }), Kind::Attach) => Said::Attached { generation, index },
            (Ok(Answer::Attached { .. }), Kind::Detach) => Said::Detached { index },
            (Ok(Answer::Set { version }), Kind::Down) => Said::Down { version, index },
            (Ok(Answer::Refused(Refusal::LastReplica)), Kind::Down) => Said::LastReplica,
            (Ok(Answer::Refused(Refusal::NotTheWriter)), Kind::Down | Kind::Copy) => Said::NotTheWriter,
            /* A copy-up's chunk (N10); a set being joined asked again */
            (Ok(Answer::Chunk { chunk, incarnation }), Kind::Copy) => Said::Chunk { chunk, incarnation, index },
            (Ok(Answer::Refused(why @ (Refusal::Joining | Refusal::Draining))), Kind::Copy) => {
                Said::NotNow(String::from(why.says()))
            }
            (Ok(Answer::Refused(why)), _) => Said::Refused(String::from(why.says())),
            (Ok(other), _) => Said::Refused(alloc::format!("answered {:?}", other)),
            /* The engine's: no leader, the leader not ready, its look */
            (Err(why), _) => Said::NotNow(why),
        };
        let _ = core.mailbox().send(to, nbswire::TAG_SAID, 0, &Saying { id, said }.encode());
    }

    /// A snapshot asked with its writer to pause first (N10c2): with no
    /// writer, asked as it is, at no generation; with one on another host,
    /// refused, naming it; with one here, its session paused -- the one that
    /// last asked at its generation -- and the snapshot proposed once it
    /// answers, at that generation. Anything else asked as it is.
    fn take(&mut self, core: &mut Core<Mds>, r: Asked<Mds>) -> Option<Asked<Mds>> {
        let Command::Snapshot { volume, name, generation: PAUSE_FIRST } = &r.cmd else { return Some(r) };
        let (volume, name) = (volume.clone(), name.clone());
        let writer = core.state().volume(&volume).and_then(|v| v.writer.as_ref()).map(|w| (w.host.clone(), w.generation));
        let refuse = |r: Asked<Mds>, why: String| -> Option<Asked<Mds>> {
            r.done.with(|s| *s = Some((Err(why), 0)));
            None
        };
        let Some((host, generation)) = writer else {
            return Some(Asked { cmd: Command::Snapshot { volume, name, generation: 0 }, done: r.done });
        };
        if host != self.me.host {
            return refuse(r, alloc::format!("volume {} is attached on host {}: its snapshot is asked there, where its \
                                             writer is paused for it", volume, host));
        }
        if self.pausing.is_some() {
            return refuse(r, String::from("a snapshot is being taken already: one at a time"));
        }
        let Some(&(_, session)) = self.sessions.get(&volume).filter(|&&(g, _)| g == generation) else {
            return refuse(r, alloc::format!("volume {}'s writer here has not been heard from since this mds started -- \
                                             ask again once it writes", volume));
        };
        let pause = self.next_pause;
        self.next_pause += 1;
        let _ = core.mailbox().send(session, nbswire::TAG_SAID, 0,
                                    &Saying { id: 0, said: Said::Pause { pause } }.encode());
        self.pausing = Some(Pausing { volume, name, generation, session, pause, asked: boot_time_ns(), proposed: false,
                                      request: r });
        None
    }

    /// A pause not answered in time given up (N10c2); a host whose member is
    /// no longer the one that said it lost its state -- replaced, or removed
    /// -- forgotten; and as the leader, the newest checkpoint's digest
    /// proposed, for every member to hold its own to.
    fn turn(&mut self, core: &mut Core<Mds>) {
        /* A pause not answered in time given up -- no snapshot proposed, so
           the session may go on at once (N10c2) */
        if self.pausing.as_ref().is_some_and(|p| !p.proposed && boot_time_ns() >= p.asked + PAUSE_WAIT_NS) {
            if let Some(p) = self.pausing.take() {
                let said = Said::GoOn { pause: p.pause, index: core.rsm().applied() };
                let _ = core.mailbox().send(p.session, nbswire::TAG_SAID, 0, &Saying { id: 0, said }.encode());
                let why = alloc::format!("volume {}'s writer did not pause in {} s: no snapshot taken", p.volume,
                                         PAUSE_WAIT_NS / 1_000_000_000);
                p.request.done.with(|s| *s = Some((Err(why), 0)));
            }
        }
        let state = core.state();
        self.lost.with(|l| l.retain(|host, &mut (id, _)| state.member(host).is_some_and(|m| m.id == id)));
        if core.role() != Role::Leader {
            /* What this member saw as a leader is of no use to the next */
            self.watch.clear();
            self.repairing = 0;
            self.refused = false;
            self.scrub = None;
            return;
        }
        /* A look not answered in time: the cs is silent */
        let now = boot_time_ns();
        let verdicts = &mut self.verdicts;
        self.probes.retain(|p| {
            if now < p.until {
                return true;
            }
            verdicts.insert(p.cs, (false, now));
            false
        });
        self.verdicts.retain(|_, &mut (_, when)| now < when + VERDICT_NS);
        if let Some((at, digest)) = core.state().checks().latest() {
            if at > self.proposed {
                self.proposed = at;
                core.ask(self, Command::Digest { at, digest }, Pending::App(Waiter::Digest));
            }
        }
        self.control(core, now);
        /* The scrub's turn, as the leader, where it has a pace (N9e) */
        if self.scrub_every_ns > 0 {
            let every = self.scrub_every_ns;
            let Mds { scrub, next_probe, .. } = self;
            let s = scrub.get_or_insert_with(|| Scrub::new(every));
            let mut out = Vec::new();
            s.tick(core.state(), now, next_probe, &mut out);
            self.scrub_out(core, out);
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
    /// The controller's turn, as the leader: every cs looked at once a
    /// second, and a plan made every ten -- sooner where the state moved on
    /// or a cs went silent or came back -- and proposed, once what it
    /// proposed last is applied.
    fn control(&mut self, core: &mut Core<Mds>, now: u64) {
        let ids: Vec<u32> = core.state().cs().iter().map(|c| c.id).collect();
        self.watch.retain(|cs, _| ids.contains(cs));
        for &cs in &ids {
            self.watch.entry(cs).or_insert((now, 0));
        }
        if now >= self.watch_at {
            self.watch_at = now + WATCH_NS;
            for &cs in &ids {
                self.probe(core, cs);
            }
        }
        let seen: Vec<Seen> = self.watch.iter().map(|(&cs, &(since, ok_at))| {
            let answering = ok_at != 0 && now < ok_at + ANSWERED_NS;
            let silent_ns = if answering { 0 } else { now.saturating_sub(ok_at.max(since)) };
            Seen { cs, answering, silent_ms: silent_ns / 1_000_000, maintenance: false }
        }).collect();
        /* Planned now, a second after the last plan at the soonest: where the
           state moved on since -- a cs changed, a join done or dropped, so
           that the next joins are not kept ten seconds -- or a cs's
           answering is not what the state has it as. Not after a proposal
           of the controller's was refused: that would be made again each
           second; the ten seconds' plan stands, and says each refusal */
        let applied = core.rsm().applied();
        let changed = applied != self.planned_index || seen.iter().any(|v| {
            core.state().cs().get(v.cs as usize).is_some_and(|c| {
                (c.state == nbsmeta::CsState::Up) != v.answering
                    && (v.answering || v.silent_ms >= self.after.down_ms)
            })
        });
        let early = changed && !self.refused && now >= self.planned + WATCH_NS;
        if self.repairing != 0 || (now < self.plan_at && !early) {
            return;
        }
        self.planned = now;
        self.planned_index = applied;
        self.refused = false;
        self.plan_at = now + PLAN_NS;
        let plan = repair::plan(core.state(), &seen, self.after);
        for cmd in plan {
            klog!(1, "nbs: the controller proposes {:?}", cmd);
            self.repairing += 1;
            core.ask(self, cmd, Pending::App(Waiter::Repair));
        }
    }

    /// What the scrub asks done: its requests sent from the member's
    /// mailbox, as a look at a cs is; what it found said, and kept for the
    /// shell with its counts.
    fn scrub_out(&mut self, core: &Core<Mds>, out: Vec<scrub::Out>) {
        for o in out {
            match o {
                scrub::Out::Send { host, bytes } => {
                    let to = if host == self.me.host {
                        String::from(nbswire::CS_NAME)
                    } else {
                        alloc::format!("{}@{}", nbswire::CS_NAME, host)
                    };
                    /* Not sent -- the cs not there: the look waits out its
                       answer, and the chunk is left to a later round */
                    let _ = core.mailbox().send_to(&to, PROBE, 0, &bytes);
                }
                scrub::Out::Found { chunk, set, version, odd } => {
                    self.scrubbed.with(|x| {
                        if x.found.len() >= FOUND_KEPT {
                            x.found.pop_front();
                        }
                        x.found.push_back((chunk, set, version, odd));
                    });
                }
                scrub::Out::Say(line) => klog!(0, "{}", line),
            }
        }
        if let Some(s) = self.scrub.as_ref() {
            let stats = s.stats;
            self.scrubbed.with(|x| x.stats = stats);
        }
    }

    /// A look at cs `cs` started, unless one is under way: its `Ping`, from
    /// the member's mailbox, the answer looked for in `on_probe_answer`.
    fn probe(&mut self, core: &Core<Mds>, cs: u32) {
        if self.probes.iter().any(|p| p.cs == cs) {
            return;
        }
        let now = boot_time_ns();
        let Some(host) = core.state().cs().get(cs as usize).map(|c| c.host.clone()) else { return };
        let to = if host == self.me.host {
            String::from(nbswire::CS_NAME)
        } else {
            alloc::format!("{}@{}", nbswire::CS_NAME, host)
        };
        let id = self.next_probe;
        self.next_probe += 1;
        let ping = Request { id, volume: 0, generation: 0, op: Op::Ping }.encode();
        if core.mailbox().send_to(&to, PROBE, id, &ping).is_err() {
            /* Nothing there to look at: silent */
            self.verdicts.insert(cs, (false, now));
            return;
        }
        self.probes.push(Probe { cs, id, until: now + PROBE_NS });
    }

    /// A cs's answer: to the scrub's ask (N9e), or to the leader's look.
    fn on_answer(&mut self, core: &Core<Mds>, m: &Message) {
        let Some(a) = nbswire::Answer::decode(&m.data) else { return };
        if let Some(s) = self.scrub.as_mut().filter(|s| s.asked(a.id)) {
            let mut out = Vec::new();
            s.answer(a.id, &a.reply, core.state(), boot_time_ns(), &mut out);
            self.scrub_out(core, out);
            return;
        }
        self.on_probe_answer(&a);
    }

    /// A cs's answer to the leader's look: there, and serving, or not.
    fn on_probe_answer(&mut self, a: &nbswire::Answer<'_>) {
        let Some(at) = self.probes.iter().position(|p| p.id == a.id) else { return };
        let p = self.probes.swap_remove(at);
        let serving = matches!(a.reply, nbswire::Reply::Done);
        let now = boot_time_ns();
        self.verdicts.insert(p.cs, (serving, now));
        if serving {
            if let Some(w) = self.watch.get_mut(&p.cs) {
                w.1 = now;
            }
        }
    }

    /// A look the channel gave back: the cs is not there.
    fn on_bounce(&mut self, m: &Message) {
        let Some(b) = m.bounced() else { return };
        if b.tag != PROBE {
            return;
        }
        let Some(at) = self.probes.iter().position(|p| p.id == m.reference) else { return };
        let p = self.probes.swap_remove(at);
        self.verdicts.insert(p.cs, (false, boot_time_ns()));
    }

    /// An ask of a client of this host's (nbswire's `Asking`): a command
    /// asked, told its outcome by `finish` -- or a map told now.
    fn on_client(&mut self, core: &mut Core<Mds>, m: &Message) {
        if m.from.host() != 0 {
            return;
        }
        let Some(a) = Asking::decode(&m.data) else { return };
        let host = self.me.host.clone();
        /* The writer's session of a volume on this host, by the asks that
           name its generation: the one a snapshot pauses (N10c2). Only the
           volume's writer here as applied -- a session taken over on this
           host asks at a generation before it, and is not the one to pause */
        match &a.ask {
            Ask::Down { name, generation, .. } | Ask::CopyUp { name, generation, .. }
            | Ask::CopyDone { name, generation, .. } | Ask::Paused { name, generation, .. } => {
                let writer = core.state().volume(name).and_then(|v| v.writer.as_ref())
                    .is_some_and(|w| w.generation == *generation && w.host == host);
                if writer {
                    self.sessions.insert(name.clone(), (*generation, m.from));
                }
            }
            _ => {}
        }
        let volume = match &a.ask {
            Ask::Attach { name, writable: true, .. } | Ask::Detach { name, .. } | Ask::Reattach { name, .. } => {
                Some(name.clone())
            }
            _ => None,
        };
        let (cmd, kind) = match a.ask {
            /* A pause answered: the snapshot proposed at the writer's
               generation. One this member does not know -- given up, done, or
               of before its start -- is answered by none: the session goes on
               only by a word, or by attaching again */
            Ask::Paused { name, generation, pause } => {
                let Some(p) = self.pausing.as_mut()
                    .filter(|p| p.pause == pause && p.volume == name && p.generation == generation && !p.proposed)
                else {
                    return;
                };
                p.proposed = true;
                let cmd = Command::Snapshot { volume: p.volume.clone(), name: p.name.clone(), generation };
                core.ask(self, cmd, Pending::App(Waiter::Snapshot));
                return;
            }
            Ask::Attach { name, writable, force, token } => {
                (Command::Attach { name, host, writable, force, token }, Kind::Attach)
            }
            Ask::Detach { name, generation } => (Command::Detach { name, host, generation }, Kind::Detach),
            Ask::Reattach { name, generation, token } => {
                (Command::Reattach { name, host, generation, token }, Kind::Attach)
            }
            Ask::Down { set, cs, refused, name, generation } => {
                (Command::ReplicaDown { set, cs, refused, name, generation }, Kind::Down)
            }
            Ask::CopyUp { name, place, generation } => (Command::CopyUp { name, place, generation }, Kind::Copy),
            Ask::CopyDone { name, place, generation, chunk, incarnation } => {
                (Command::CopyDone { name, place, generation, chunk, incarnation }, Kind::Copy)
            }
            Ask::Map { name, from, count, index } => {
                let said = nbswire::mds::map(core.state(), core.rsm().applied(), &name, from, count, index);
                let _ = core.mailbox().send(m.from, nbswire::TAG_SAID, 0, &Saying { id: a.id, said }.encode());
                return;
            }
        };
        core.ask(self, cmd, Pending::App(Waiter::Client { to: m.from, id: a.id, kind, volume }));
    }

    /// A question of this host's cs, answered from the member's applied
    /// state (plans/nbs.md, "The cs"): this host's alone -- another host's
    /// cs asks its own host's mds.
    fn on_cs_query(&mut self, core: &mut Core<Mds>, m: &Message) {
        if m.from.host() != 0 {
            return;
        }
        let Some(q) = Query::decode(&m.data) else { return };
        /* A join's end, or its giving up: proposed for this host's cs, and
           told once applied (N9) */
        if let Question::JoinDone { set, since } | Question::GiveUp { set, since } = q.question {
            let Some(cs) = core.state().cs_of(&self.me.host).map(|c| c.id) else {
                let told = Told::Refused(nbswire::Refusal::NotHere);
                let _ = core.mailbox().send(m.from, nbswire::TAG_TOLD, 0, &Answered { id: q.id, told }.encode());
                return;
            };
            let cmd = match q.question {
                Question::JoinDone { .. } => Command::Joined { set, cs, since },
                _ => Command::Unjoin { set, cs, since },
            };
            core.ask(self, cmd, Pending::App(Waiter::Cs { to: m.from, id: q.id }));
            return;
        }
        let told = nbswire::mds::told(core.state(), &self.me.host, core.rsm().applied(), &q.question);
        let _ = core.mailbox().send(m.from, nbswire::TAG_TOLD, 0, &Answered { id: q.id, told }.encode());
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

