//! A group whose hosts are offered it -- NBS's metadata (nbs-mds.ko) and
//! vnet's state: what such a group's module and its member do of its
//! members, written once (plans/vnet.md, V1a).
//!
//! Who the members are is two things (plans/nbs.md, "Who is in the
//! cluster"): the hosts that may talk at all are the cluster registry's,
//! ipcnet pinning each by its key; the members are the group's raft
//! configuration, each a host and the incarnation its member made when it
//! was offered the group -- 64 random bits, never made again (`members`). A
//! host whose folder was lost comes back with none and is offered the group
//! as a new member, never as the one it was.
//!
//! - **While this host is in no group**, a listener holds the group's
//!   mailbox ([`listen`]): it takes an offer -- making the incarnation, the
//!   member file and an empty log, and starting the member, which raft then
//!   brings the log -- and answers a raft message for a member it does not
//!   have with `TAG_NO_MEMBER`, which is how a leader learns that a host's
//!   state is gone. What else comes is the group's to answer ([`Group::idle`]).
//! - **An offer** ([`offer`]): asked of the host until its connection is up
//!   and it answers, from that host alone -- accepted with its incarnation,
//!   made then or the one it was already, or declined, saying why.
//! - **The member's half** ([`Half`]): the hosts its nodes are -- the
//!   state's, or before this host's log has caught up, the members it was
//!   offered the group by; an offer to it, a member already, answered; a
//!   host's word that its member is gone, kept and said until the member is
//!   replaced; a probe answered.
//! - **The commands**: `init`, a host added (offered first), removed,
//!   replaced -- removed and offered anew -- and `recover`.
//!
//! The member file: its incarnation, the group's id, this host's name and
//! the group's name on its first line; then, for a host the group was
//! offered to, a line for each member it was offered by -- a host and its
//! incarnation. Written once, last, when the host makes the group or takes
//! an offer, and never written over.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use codec::{Reader, Writer};
use kcore::ipc::{Limits, Mailbox, Message, Scope, TAG_BULK};
use kcore::klog;
use kcore::task::TaskHandle;
use kcore::time::{boot_time_ns, Duration};
use members::Members;

use crate::host::{self, Recovered, Recovery, Runner};
use crate::{App, Core, Locked, Log, View, GROUP_BYTES, TAG_APP, TAG_RAFT};

/// An offer of the group: its id and name, then its members (a host and
/// its incarnation each) -- what the host offered knows them by until its
/// log has caught up.
pub const TAG_OFFER: u32 = TAG_APP;
/// Its answer: the group's id, then the incarnation the host's member is.
pub const TAG_ACCEPTED: u32 = TAG_APP + 1;
/// An offer declined: why, in words.
pub const TAG_DECLINED: u32 = TAG_APP + 2;
/// From a host that has no member of the incarnation a raft message was
/// for: the group's id, then that incarnation.
pub const TAG_NO_MEMBER: u32 = TAG_APP + 3;
/// Whether a member is there, from a member of the group: what `recover`
/// asks every other voter first. Its answer: the member's node, its term
/// and its role's byte.
pub const TAG_PROBE: u32 = TAG_APP + 4;
pub const TAG_PROBED: u32 = TAG_APP + 5;
/// The first tag a group of the kind gives its own messages.
pub const TAG_OWN: u32 = TAG_APP + 6;

/// How long an offer is tried for -- the connection to the host coming up
/// first -- and how long each try waits for its answer.
const OFFER_TOTAL_MS: u64 = 30_000;
const OFFER_TRY_MS: u64 = 3000;
/// How often the listener looks for the module's unload.
const IDLE_POLL_MS: u64 = 100;
/// How often a host is told it named a member this host does not have.
const NO_MEMBER_EVERY_NS: u64 = 5_000_000_000;
/// How long a change applied is waited for in the member's view.
const VIEW_WAIT_MS: u64 = 5000;
const NS_PER_MS: u64 = 1_000_000;
/// How often a host's lost member is said in the log at most.
const LOST_SAY_NS: u64 = 60_000_000_000;
/// What may wait in the listener's mailbox.
const IDLE_LIMITS: Limits = Limits { messages: 256, bytes: 1 << 20 };

/// A group of the kind: its member's app, and what its module is called.
pub trait Group: App {
    /// The module, for `rmmod`.
    const MODULE: &'static str;
    /// What the group is called in what is said: "NBS cluster" -- with its
    /// article, "an NBS cluster" -- and what its name is a name of:
    /// "cluster".
    const WHAT: &'static str;
    const AN: &'static str;
    const KIND: &'static str;
    /// The shell's command: "nbs".
    const WORD: &'static str;
    /// What a host's member of the group is called: "mds".
    const MEMBER: &'static str;
    /// The module's folder unless it is given another.
    const DIR: &'static str;
    /// The task the member runs as.
    const TASK: &'static str;
    /// The listener's.
    const IDLE_TASK: &'static str;

    /// The group's members, as the state has them.
    fn members(state: &Self::State) -> &Members;
    /// The group's commands of its members.
    fn init(group: [u8; 16], name: &str, host: &str, member: u64) -> Self::Command;
    fn add(host: &str, member: u64) -> Self::Command;
    fn remove(host: &str) -> Self::Command;
    /// A command's answer: the incarnation it names, or why it was refused.
    fn done(answer: Self::Answer) -> Result<u64, String>;
    /// A message to the group's mailbox while this host is in no group --
    /// neither an offer nor raft's: NBS's cs asking its mds, told there is
    /// none rather than left to time out.
    fn idle(_mb: &Mailbox, _m: &Message) {}
}

/// A member as an offer named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    pub host: String,
    pub id: u64,
}

/// This host's member: what its member file says.
#[derive(Clone, Debug)]
pub struct MemberFile {
    /// Its incarnation: its raft node.
    pub id: u64,
    /// The group's id.
    pub group: [u8; 16],
    /// This host's ipc name.
    pub host: String,
    /// The group's name.
    pub name: String,
    /// The members the group was offered by, for a host that took an offer:
    /// none for the host that made it.
    pub known: Vec<Known>,
}

/// The member file's most bytes: its first line, and a line of each member
/// of as many as a group has.
const MEMBER_MAX: usize = 256 + members::MEMBERS_MAX * 96;

impl MemberFile {
    /// The member file of the group's folder `dir`, if there is one: None
    /// for a host in no group.
    pub fn read(dir: &str) -> Result<Option<MemberFile>, String> {
        let Some(bytes) = host::read_member_file(dir, MEMBER_MAX)? else { return Ok(None) };
        let path = alloc::format!("{}/{}", dir, host::MEMBER_FILE);
        let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not text", path))?;
        let mut lines = text.lines();
        let first = lines.next().unwrap_or("");
        let words: Vec<&str> = first.split_whitespace().collect();
        let bad = || alloc::format!("{}: \"{}\" is not `<incarnation> <cluster> <host> <name>`", path, first);
        let [id, group, host, name] = words.as_slice() else { return Err(bad()) };
        let id: u64 = id.parse().map_err(|_| bad())?;
        let group = codec::unhex::<16>(group).ok_or_else(bad)?;
        if id == 0 || !members::valid_name(host) || !members::valid_name(name) {
            return Err(bad());
        }
        let mut known = Vec::new();
        for line in lines.filter(|l| !l.trim().is_empty()) {
            let bad = || alloc::format!("{}: \"{}\" is not `<host> <incarnation>`", path, line);
            let words: Vec<&str> = line.split_whitespace().collect();
            let [h, n] = words.as_slice() else { return Err(bad()) };
            let n: u64 = n.parse().map_err(|_| bad())?;
            if n == 0 || !members::valid_name(h) || known.len() >= members::MEMBERS_MAX {
                return Err(bad());
            }
            known.push(Known { host: String::from(*h), id: n });
        }
        Ok(Some(MemberFile { id, group, host: String::from(*host), name: String::from(*name), known }))
    }

    /// The member file written, once and synced: refused where there is
    /// one. A write that failed part way leaves none.
    pub fn create(&self, dir: &str) -> Result<(), String> {
        let mut text = alloc::format!("{} {} {} {}\n", self.id, codec::hex(&self.group), self.host, self.name);
        for k in &self.known {
            let _ = writeln!(text, "{} {}", k.host, k.id);
        }
        host::create_member_file(dir, text.as_bytes())
    }
}

/// An offer of the group, as it goes between hosts.
pub struct Offer {
    pub group: [u8; 16],
    pub name: String,
    pub members: Vec<Known>,
}

impl Offer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.fixed(&self.group);
        w.str(&self.name);
        w.u8(self.members.len().min(members::MEMBERS_MAX) as u8);
        for m in self.members.iter().take(members::MEMBERS_MAX) {
            w.str(&m.host);
            w.u64(m.id);
        }
        w.buf
    }

    /// What a member offered, read: every count bounded, every name a
    /// host's, no member twice.
    pub fn decode(bytes: &[u8]) -> Option<Offer> {
        let mut r = Reader::new(bytes);
        let group = r.fixed()?;
        let name = String::from(r.str(members::NAME_MAX)?);
        let n = usize::from(r.u8()?);
        if n == 0 || n > members::MEMBERS_MAX || !members::valid_name(&name) {
            return None;
        }
        let mut list: Vec<Known> = Vec::with_capacity(n);
        for _ in 0..n {
            let host = String::from(r.str(members::NAME_MAX)?);
            let id = r.u64()?;
            if id == 0 || !members::valid_name(&host) || list.iter().any(|m| m.host == host || m.id == id) {
                return None;
            }
            list.push(Known { host, id });
        }
        r.done().then_some(Offer { group, name, members: list })
    }
}

/// The group's id and an incarnation: an acceptance's bytes, and a
/// no-member answer's.
pub fn encode_id(group: &[u8; 16], id: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.fixed(group);
    w.u64(id);
    w.buf
}

pub fn decode_id(bytes: &[u8]) -> Option<([u8; 16], u64)> {
    let mut r = Reader::new(bytes);
    let group = r.fixed()?;
    let id = r.u64()?;
    r.done().then_some((group, id))
}

/// The hosts that said their member has no member of the incarnation the
/// state has for them: the host, that incarnation, and when it was last
/// said in the log. Kept by the member, read by the shell.
pub type Lost = Arc<Locked<BTreeMap<String, (u64, u64)>>>;

/* ---- the member's half ---- */

/// What a group's member keeps of its membership: this host's member file,
/// and the hosts that said their member is gone.
pub struct Half {
    pub me: MemberFile,
    pub lost: Lost,
}

impl Half {
    /// The incarnation host `name`'s member is: the state's -- or, where the
    /// state applied here has neither the host nor the incarnation and has
    /// not removed it, the offer's. A member whose state is older than the
    /// coming of a member that leads -- restored from a snapshot taken
    /// before it -- takes that leader's messages by it.
    pub fn node_of<G: Group>(&self, core: &Core<G>, name: &str) -> Option<u64> {
        if let Some(m) = G::members(core.state()).member(name) {
            return Some(m.id);
        }
        self.offered(core, |k| k.host == name).map(|k| k.id)
    }

    pub fn host_of<G: Group>(&self, core: &Core<G>, node: u64) -> Option<String> {
        if let Some(m) = G::members(core.state()).member_of_id(node) {
            return Some(m.host.clone());
        }
        self.offered(core, |k| k.id == node).map(|k| k.host.clone())
    }

    /// The voters and learners the state names.
    pub fn wanted<G: Group>(&self, core: &Core<G>) -> (Vec<u64>, Vec<u64>) {
        let m = G::members(core.state());
        (m.voters(), m.learners())
    }

    /// A member the offer this host took named that `which` names, where
    /// the state applied here has neither its host nor its incarnation --
    /// it is older than that member's coming -- and has not removed it.
    fn offered<G: Group>(&self, core: &Core<G>, which: impl Fn(&Known) -> bool) -> Option<&Known> {
        let m = G::members(core.state());
        self.me.known.iter().find(|k| which(k))
            .filter(|k| m.member(&k.host).is_none() && m.member_of_id(k.id).is_none() && !m.id_removed(k.id))
    }

    /// A message of the membership's: an offer, a host's word that its
    /// member is gone, a probe. False for any other, the group's own.
    pub fn on_message<G: Group>(&self, app: &G, core: &mut Core<G>, m: &Message) -> bool {
        match m.tag & !TAG_BULK {
            TAG_OFFER => self.on_offer::<G>(core, m),
            TAG_NO_MEMBER => self.on_no_member::<G>(core, m),
            TAG_PROBE => core.answer_probe(app, m, TAG_PROBED),
            _ => return false,
        }
        true
    }

    /// An offer to this host, a member already: answered with its
    /// incarnation where it is the group's -- an add asked again -- and
    /// declined where it is another's.
    fn on_offer<G: Group>(&self, core: &mut Core<G>, m: &Message) {
        let Some(o) = Offer::decode(&m.data) else {
            core.note_refused();
            return;
        };
        if o.group == self.me.group {
            let _ = core.mailbox().send(m.from, TAG_ACCEPTED, m.reference, &encode_id(&self.me.group, self.me.id));
        } else {
            let why = alloc::format!("{}'s {} is a member of another {}, {} ({}...), as {}", self.me.host, G::MEMBER,
                                     G::WHAT, self.me.name, codec::hex(&self.me.group[..4]), self.me.id);
            let _ = core.mailbox().send(m.from, TAG_DECLINED, m.reference, why.as_bytes());
        }
    }

    /// A host that says it has no member of the incarnation the state has
    /// for it: its claim, about itself alone, kept and said.
    fn on_no_member<G: Group>(&self, core: &mut Core<G>, m: &Message) {
        let (Some((group, id)), Some(host)) = (decode_id(&m.data), kcore::ipc::peer_name(m.from)) else {
            core.note_refused();
            return;
        };
        if group != self.me.group || G::members(core.state()).member(&host).map(|x| x.id) != Some(id) {
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
            klog!(0, "{}: {} says its {} has no member {}: its state is gone -- `{} host replace {}` takes it back as \
                      a new member", G::LOG, host, G::MEMBER, id, G::WORD, host);
        }
    }

    /// A host whose member is no longer the one that said it lost its
    /// state -- replaced, or removed -- forgotten: at every turn.
    pub fn turn<G: Group>(&self, core: &Core<G>) {
        let m = G::members(core.state());
        self.lost.with(|l| l.retain(|host, &mut (id, _)| m.member(host).is_some_and(|x| x.id == id)));
    }
}

/* ---- the module's half ---- */

/// A group's module: its member, the listener while there is none, its
/// folder and the hosts said to be lost.
pub struct Offered<G: Group> {
    pub runner: Runner<G>,
    pub dir: String,
    pub lost: Lost,
    idle: Locked<Option<TaskHandle>>,
    idle_stop: AtomicBool,
}

/// What starts a group's member: the module's state, which makes the app's
/// half over raft's member.
pub trait Starter<G: Group>: Send + Sync + 'static {
    fn offered(&self) -> &Offered<G>;
    /// The member started over `log` -- or, where none is given, over the
    /// log in the folder: made new, this host its one voter, when `fresh`
    /// -- standing for election at once when `campaign`.
    fn start(&self, me: MemberFile, log: Option<Log>, fresh: bool, campaign: bool) -> Result<(), String>;
}

/// An NBS cluster made of this host alone, or a group: its id, this host's
/// name and its member's incarnation.
pub struct Made {
    pub group: [u8; 16],
    pub host: String,
    pub member: u64,
}

impl<G: Group> Offered<G> {
    pub fn new(dir: &str) -> Option<Offered<G>> {
        Some(Offered {
            runner: Runner::new()?,
            dir: String::from(dir.trim_end_matches('/')),
            lost: Arc::new(Locked::new(BTreeMap::new())?),
            idle: Locked::new(None)?,
            idle_stop: AtomicBool::new(false),
        })
    }

    /// The listener stopped and waited out, the group's mailbox let go of.
    pub fn stop_listening(&self) {
        self.idle_stop.store(true, Ordering::Release);
        let task = self.idle.with(|t| t.take());
        drop(task);
    }

    /// The module going: the listener, then the member's task -- its log
    /// closed with it.
    pub fn shut(&self) {
        self.stop_listening();
        self.runner.stop();
    }

    pub fn view(&self) -> Option<View<G>> {
        self.runner.view()
    }

    /// This host's member's view, while it runs: `none` said where there is
    /// none, why it did not start where it did not.
    pub fn member_view(&self, none: &str) -> Result<View<G>, String> {
        self.view().filter(|v| v.stopped.is_none()).ok_or_else(|| self.runner.no_member(none))
    }

    /// This member's view once its log has the group: a member just added
    /// holds none until raft has brought it the log, which it is given a
    /// few seconds for.
    pub fn caught_up(&self, none: &str) -> Result<View<G>, String> {
        let end = boot_time_ns() + VIEW_WAIT_MS * NS_PER_MS;
        loop {
            let v = self.member_view(none)?;
            if G::members(&v.state).group().is_some() {
                return Ok(v);
            }
            if boot_time_ns() >= end {
                return Err(alloc::format!("no {} yet -- this member's log is behind: ask again in a moment", G::WHAT));
            }
            kcore::task::sleep_ms(50);
        }
    }

    /// `cmd`, as the state answers it: the incarnation it names, or why not.
    pub fn ask(&self, cmd: &G::Command, none: &str) -> Result<u64, String> {
        G::done(crate::ask(&self.runner.shared, cmd, &self.runner.no_member(none))?)
    }

    /// `host` made a member: offered the group, then added -- its
    /// incarnation. Asked of the offered host until it answers, or `stop`
    /// says to give up.
    pub fn add(&self, host: &str, stop: &dyn Fn() -> bool, none: &str) -> Result<u64, String> {
        let v = self.caught_up(none)?;
        if !members::valid_name(host) {
            return Err(alloc::format!("\"{}\" is not a host's name", host));
        }
        let id = offer::<G>(&v, host, stop)?;
        self.ask(&G::add(host, id), none)
    }

    /// `host`'s member out, for good: the incarnation it took out.
    pub fn remove(&self, host: &str, none: &str) -> Result<u64, String> {
        self.member_view(none)?;
        self.ask(&G::remove(host), none)
    }

    /// A host that lost its state taken back: its old member removed -- for
    /// good, its incarnation never taken again -- and the host offered the
    /// group as a new one: the old incarnation and the new.
    pub fn replace(&self, host: &str, stop: &dyn Fn() -> bool, none: &str) -> Result<(u64, u64), String> {
        let v = self.caught_up(none)?;
        let removed = match G::members(&v.state).member(host) {
            Some(_) => self.remove(host, none)?,
            None => return Err(alloc::format!("{} is no member -- `{} host add {}`", host, G::WORD, host)),
        };
        /* The removal in the member's view before the offer is made of it:
           an offer that named the host a member still would be declined
           there */
        let end = boot_time_ns() + VIEW_WAIT_MS * NS_PER_MS;
        let mut v = self.member_view(none)?;
        while G::members(&v.state).member(host).is_some() && boot_time_ns() < end {
            kcore::task::sleep_ms(20);
            v = self.member_view(none)?;
        }
        let id = offer::<G>(&v, host, stop).map_err(|why| {
            alloc::format!("{}'s old member {} is removed, and the offer of a new one failed: {} -- `{} host add {}` \
                            once it answers", host, removed, why, G::WORD, host)
        })?;
        if id == removed {
            return Err(alloc::format!("{}'s {} is still member {}, the one removed: it did not lose its state -- \
                                       `rmmod {}`, `rm {}` and `insmod` it there, then `{} host add {}`", host,
                                      G::MEMBER, id, G::MODULE, G::DIR, G::WORD, host));
        }
        let id = self.ask(&G::add(host, id), none)?;
        Ok((removed, id))
    }
}

/// At the module's load: the member started over its log where this host
/// is one; the listener, for an offer, where it is none. A member file that
/// cannot be read, or a member that will not start: neither -- the host is
/// said to be broken rather than offered the group as somebody new.
pub fn load<G: Group, S: Starter<G>>(s: &Arc<S>) -> Result<(), String> {
    let o = s.offered();
    let fail = |why: String| {
        o.runner.hold(&why);
        Err(why)
    };
    match MemberFile::read(&o.dir) {
        Ok(Some(me)) => match s.start(me, None, false, false) {
            Ok(()) => Ok(()),
            Err(why) => fail(why),
        },
        Ok(None) => listen::<G, S>(s),
        Err(why) => fail(alloc::format!("{} -- this host takes no offer until it is put right", why)),
    }
}

/// The listener started, holding the group's mailbox until an offer is
/// taken or the module goes.
pub fn listen<G: Group, S: Starter<G>>(s: &Arc<S>) -> Result<(), String> {
    let o = s.offered();
    o.idle_stop.store(false, Ordering::Release);
    let mb = Mailbox::new(IDLE_LIMITS).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    mb.register(G::NAME, Scope::Cluster).map_err(|e| alloc::format!("{} not named: {}", G::NAME, e.name()))?;
    let Some(task) = kcore::task::spawn_with(G::IDLE_TASK, (s.clone(), mb), idle_run::<G, S>) else {
        return Err(String::from("could not start the listener's task"));
    };
    o.idle.with(|t| *t = Some(task));
    Ok(())
}

/// The listener: an offer taken, and the member started -- or until the
/// module goes.
fn idle_run<G: Group, S: Starter<G>>((s, mb): (Arc<S>, Mailbox)) {
    let o = s.offered();
    let mut told: BTreeMap<String, u64> = BTreeMap::new();
    while !o.idle_stop.load(Ordering::Acquire) {
        let Some(m) = mb.recv(Duration::from_millis(IDLE_POLL_MS)) else { continue };
        match m.tag & !TAG_BULK {
            TAG_OFFER => {
                if let Some(me) = take_offer::<G>(o, &mb, &m) {
                    /* The group's mailbox let go of, for the member to take */
                    drop(mb);
                    if let Err(why) = s.start(me, None, false, false) {
                        o.runner.hold(&why);
                        klog!(0, "{}: the offer was taken and the member did not start: {} -- `rmmod {}` and \
                                  `insmod` start it again", G::LOG, why, G::MODULE);
                    }
                    return;
                }
            }
            TAG_RAFT => no_member(&mb, &m, &mut told),
            _ => G::idle(&mb, &m),
        }
    }
}

/// A raft message to a member this host does not have -- it lost its state,
/// or never was it -- answered so, at most every few seconds a host.
fn no_member(mb: &Mailbox, m: &Message, told: &mut BTreeMap<String, u64>) {
    let Some(host) = kcore::ipc::peer_name(m.from) else { return };
    if m.data.len() < GROUP_BYTES {
        return;
    }
    let mut group = [0u8; GROUP_BYTES];
    group.copy_from_slice(&m.data[..GROUP_BYTES]);
    let Ok(rm) = raft::wire::decode(&m.data[GROUP_BYTES..]) else { return };
    let now = boot_time_ns();
    if told.get(&host).is_some_and(|&at| now < at + NO_MEMBER_EVERY_NS) {
        return;
    }
    if told.len() >= members::MEMBERS_MAX && !told.contains_key(&host) {
        told.clear();
    }
    told.insert(host, now);
    let _ = mb.send(m.from, TAG_NO_MEMBER, 0, &encode_id(&group, rm.to));
}

/// An offer taken, where it can be: this host's incarnation made, its
/// member file written, its folder's log cleared -- the member to start.
/// Answered either way.
fn take_offer<G: Group>(o: &Offered<G>, mb: &Mailbox, m: &Message) -> Option<MemberFile> {
    let decline = |why: &str| {
        let _ = mb.send(m.from, TAG_DECLINED, m.reference, why.as_bytes());
    };
    let off = Offer::decode(&m.data)?;
    let Ok(_one) = o.runner.starting("") else {
        decline(&alloc::format!("an `{} init` is under way on this host", G::WORD));
        return None;
    };
    let id = match crate::ipcnet::identity() {
        Ok(id) => id,
        Err(why) => {
            decline(&alloc::format!("this host does not know who it is: {}", why));
            return None;
        }
    };
    /* The group has this host as a member already: it lost its state, and
       is taken back as a new member alone -- the old one removed first */
    if let Some(old) = off.members.iter().find(|k| k.host == id.name) {
        decline(&alloc::format!("{}'s {} has no state, and the {} has it as member {} -- `{} host replace {}` takes it \
                                 back as a new member", id.name, G::MEMBER, G::WHAT, old.id, G::WORD, id.name));
        return None;
    }
    let mut me = MemberFile { id: 0, group: off.group, host: id.name.clone(), name: off.name.clone(),
                              known: off.members.clone() };
    /* An incarnation no member of the offer has */
    while me.id == 0 || off.members.iter().any(|k| k.id == me.id) {
        match kcore::random::random_u64() {
            Some(r) => me.id = r,
            None => {
                decline("no randomness for an incarnation");
                return None;
            }
        }
    }
    let made = host::clear_log(&o.dir).and_then(|()| me.create(&o.dir));
    if let Err(why) = made {
        decline(&alloc::format!("{} could not take the offer: {}", id.name, why));
        return None;
    }
    klog!(0, "{}: {} offered {} {}: member {} -- the log follows from the leader", G::LOG, id.name, G::WHAT, off.name,
          me.id);
    let _ = mb.send(m.from, TAG_ACCEPTED, m.reference, &encode_id(&me.group, me.id));
    Some(me)
}

/// `host` offered the group: the incarnation its member is -- made now, or
/// the one it was already -- asked again until its connection is up and it
/// answers, or `stop` says to give up (a provider's job told to: its module
/// going, a cancel).
pub fn offer<G: Group>(v: &View<G>, host: &str, stop: &dyn Fn() -> bool) -> Result<u64, String> {
    let all = G::members(&v.state);
    let g = all.group().ok_or_else(|| alloc::format!("no {} yet -- this member's log is behind", G::WHAT))?;
    let o = Offer {
        group: g.id,
        name: g.name.clone(),
        members: all.members().iter().map(|m| Known { host: m.host.clone(), id: m.id }).collect(),
    };
    let mb = Mailbox::new(Limits { messages: 8, bytes: 16 * 1024 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    let to = alloc::format!("{}@{}", G::NAME, host);
    let data = o.encode();
    let end = boot_time_ns() + OFFER_TOTAL_MS * NS_PER_MS;
    let mut last = alloc::format!("no answer from {}", host);
    while boot_time_ns() < end {
        if stop() {
            return Err(alloc::format!("the offer to {} stopped: {}", host, last));
        }
        if let Err(e) = mb.send_to(&to, TAG_OFFER, 0, &data) {
            last = alloc::format!("{} out of reach: {} -- is it a host of the cluster registry, with {} loaded?", host,
                                  e.name(), G::MODULE);
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
                TAG_ACCEPTED => {
                    let (group, id) = decode_id(&m.data)
                        .ok_or_else(|| alloc::format!("{} accepted in no form an acceptance has", host))?;
                    if group != g.id {
                        return Err(alloc::format!("{} accepted as a member of another {}", host, G::KIND));
                    }
                    return Ok(id);
                }
                TAG_DECLINED => return Err(String::from_utf8_lossy(&m.data).into_owned()),
                _ => {}
            }
        }
        last = alloc::format!("no answer from {} yet", host);
    }
    Err(last)
}

/// A group named `name` made of this host alone -- the shell's `init` and
/// the provider's: undone where it does not finish, for an init again.
pub fn init<G: Group, S: Starter<G>>(s: &Arc<S>, name: &str, none: &str) -> Result<Made, String> {
    let o = s.offered();
    if !members::valid_name(name) {
        return Err(alloc::format!("\"{}\" is not a {}'s name: lowercase letters, digits and -", name, G::KIND));
    }
    let busy = alloc::format!("an `{} init` or an offer is under way", G::WORD);
    let _starting = o.runner.starting(&busy)?;
    if MemberFile::read(&o.dir)?.is_some() || o.view().is_some() {
        return Err(alloc::format!("this host is a member of {} already", G::AN));
    }
    let id = crate::ipcnet::identity()?;
    let mut group = [0u8; 16];
    let member = kcore::random::random_u64().filter(|&m| m != 0);
    let (Some(member), true) = (member, kcore::random::fill_random(&mut group)) else {
        return Err(alloc::format!("no randomness for the {}'s id", G::KIND));
    };
    /* The group's mailbox taken from the listener, for the member */
    o.stop_listening();
    let undo = |why: String| -> String {
        o.runner.stop();
        let left = host::remove_member_file(&o.dir).and_then(|()| host::clear_log(&o.dir)).err();
        let back = listen::<G, S>(s).err();
        match (left, back) {
            (None, None) => why,
            (left, back) => alloc::format!("{} -- and undoing it: {}", why,
                                           [left, back].into_iter().flatten().collect::<Vec<_>>().join("; ")),
        }
    };
    /* What an init or an offer that did not finish left */
    if let Err(why) = host::clear_log(&o.dir) {
        return Err(undo(why));
    }
    let me = MemberFile { id: member, group, host: id.name.clone(), name: String::from(name), known: Vec::new() };
    if let Err(why) = s.start(me.clone(), None, true, true) {
        return Err(undo(why));
    }
    /* The member file last: the mark that this host is a member */
    let made = o.ask(&G::init(group, name, &id.name, member), none).and_then(|_| me.create(&o.dir));
    match made {
        Ok(()) => Ok(Made { group, host: id.name, member }),
        Err(why) => Err(undo(why)),
    }
}

/// The last member standing made its group's one voter (`host::recover`):
/// the others removed for good. Asked first, and done once `confirm` is the
/// group's name: what it was told, and what it did. The right to start the
/// member is taken before anything is read, as every start takes it.
pub fn recover<G: Group, S: Starter<G>>(s: &Arc<S>, confirm: Option<&str>, none: &str, remedy: &str)
    -> Result<(Recovery, Recovered), String>
{
    let o = s.offered();
    let busy = alloc::format!("an `{} init` or an offer is under way", G::WORD);
    let held = o.runner.starting(&busy)?;
    let v = o.member_view(none)?;
    let all = G::members(&v.state);
    let group = all.group().ok_or_else(|| alloc::format!("no {} yet", G::WHAT))?.name.clone();
    let me = all.member_of_id(v.node).map(|m| m.host.clone())
        .ok_or_else(|| alloc::format!("this host's member is not in the {}", G::WHAT))?;
    let leader = (v.leader != raft::NONE)
        .then(|| all.member_of_id(v.leader).map_or(String::from("a member"), |m| m.host.clone()));
    let others: Vec<(String, u64)> = all.members().iter().filter(|m| m.id != v.node)
        .map(|m| (m.host.clone(), m.id)).collect();
    let r = Recovery { remedy: String::from(remedy), module: G::MODULE, what: G::KIND, an: G::AN, group,
                       me: (me, v.node), others, voters: v.membership.voters.clone(), leader, name: G::NAME,
                       tags: (TAG_PROBE, TAG_PROBED) };
    let told = r.clone();
    let prepare = || MemberFile::read(&o.dir)?.ok_or_else(|| String::from("this host has no member file"));
    let restart = |me: &MemberFile, log: Option<Log>| {
        let campaign = log.is_some();
        s.start(me.clone(), log, false, campaign)
    };
    let remove = |host: &str| o.ask(&G::remove(host), none);
    let done = host::recover(&o.runner, &held, &o.dir, r, confirm, &prepare, &restart, &remove)?;
    Ok((told, done))
}
