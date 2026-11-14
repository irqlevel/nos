//! The `scrub` target (plans/nbs.md, N9e): the leader's scrub -- nbswire's,
//! as nbs-mds runs it -- over a set of three members whose copies of each
//! chunk are the model's: each granule's sum, whether the chunk is open
//! there, the version it was last written at. A writer's writes and closes
//! reach each member on a delay of its own, in order on their way -- a close
//! once every member has the writes before it, as a client closes a chunk
//! once each acknowledged them -- and the
//! scrub's asks on another way -- the client's channel and the leader's, no
//! order between them -- so that a write and a close fall between two
//! members' answers to one look. A member holds a chunk once its first
//! write reaches it, as a cs does, and answers `NotHeld` before; the writer
//! writes some chunks seldom, so that some are held by none for long. Now
//! and then a granule of a closed chunk rots on one member, or on two, or a
//! member loses a chunk it held -- what it reads as then is zeros; and a
//! person, told of a finding, has the members rotted copied again a few
//! seconds on -- taken down (`ReplicaBad`), joined back (`Join`, `Joined`)
//! through the metadata -- the scrub looking at the chunk meanwhile.
//! Now and then, of a randomness of its own, a member is taken down and
//! joined back a moment later, its copy the others' once it is back: the
//! set moves on under the looks in flight. And of another, the races random
//! runs seldom put together are set up: a write and a close raced through
//! a look -- one member's answer taken at once, the write and the close
//! then on every member, the others' answers after -- and a member's close
//! held back a few seconds, the chunk open there and closed on the others
//! through two looks.
//!
//! Held to:
//! - **every finding of the set as it is**: at the set's version now;
//! - **every finding a rot that is there**: a chunk with a member whose
//!   copy rotted, or that lost it, and has not been written since -- the one
//!   unlike the two others where it alone did, none named where two did --
//!   said once for the set's version;
//! - **every rot found**: at the end, the writer done and every chunk
//!   closed, each rot still there is said within a minute of the network's
//!   time;
//! - **a chunk held by none no turn of the pace**: the look after one every
//!   member answered `NotHeld` of begins well inside half a look's turn.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use nbsmeta::{Answer, Command, State};
use nbswire::scrub::{self, Scrub};
use nbswire::{Answer as CsAnswer, Op, Reply, Request};

use crate::rng::{mix, Rng};

const HOSTS: [&str; 3] = ["a", "b", "c"];
const CLUSTER: [u8; 16] = [7; 16];
const VOLUME: &str = "v";
/// The volume: six chunks, the first three written often, the others
/// seldom.
const VOLUME_MIB: u64 = 24;
const OFTEN: u64 = 3;
const GRANULES: usize = (nbscs::SLOT / nbscs::GRANULE as u64) as usize;
const MS: u64 = 1_000_000;
/// The scrub's pace here: a look every half second.
const EVERY: u64 = 500 * MS;
/// How long the end waits for every rot to be said.
const FOUND_WITHIN: u64 = 60_000 * MS;

/// A member's copy of a chunk: none until its first write reaches it.
#[derive(Clone)]
struct Copy {
    held: bool,
    open: bool,
    written: u32,
    sums: Vec<u32>,
}

/// What the writer sends a member, in order.
enum Msg {
    Write { chunk: usize, granule: usize, value: u32 },
    Close { chunk: usize },
}

struct Member {
    cs: u32,
    copies: Vec<Copy>,
    /// Until when the writer's messages wait on their way to it.
    stalled_until: u64,
    /// The writer's messages on their way, and the scrub's asks on theirs.
    inbox: VecDeque<Msg>,
    asks: VecDeque<Vec<u8>>,
}

/// A member taken down and joined in its own place again: its join's
/// version, and when it is back.
#[derive(Clone, Copy)]
struct Away {
    member: usize,
    since: u32,
    until: u64,
}

/// A finding a person acts on: the members found rotted in chunk `chunk`
/// taken down and copied again at `at`.
struct Mend {
    chunk: usize,
    set: u32,
    at: u64,
    rotted: BTreeSet<usize>,
}

/// A granule rotted on a member -- or lost with the chunk -- its value
/// before.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Rot {
    chunk: usize,
    member: usize,
    granule: usize,
    was: u32,
}

struct World {
    r: Rng,
    /// The members taken out and back: a randomness of their own.
    moves: Rng,
    away: Option<Away>,
    /// The races set up: another -- while the writer writes; the end is
    /// quiet.
    focus: Rng,
    writing: bool,
    now: u64,
    trace: bool,
    state: State,
    index: u64,
    members: Vec<Member>,
    /// The volume's chunks by their number, in map order.
    chunks: Vec<u32>,
    set: u32,
    scrub: Scrub,
    scrub_id: u64,
    /// The scrub's answers on their way.
    answers: Vec<Vec<u8>>,
    rots: Vec<Rot>,
    /// Whether a member's copy of a chunk was rotted when it last answered
    /// the scrub of it: what a finding is held to -- a member may rot after
    /// its answer, or be written over, before the look's last answer is in.
    served: BTreeMap<(usize, usize), bool>,
    said: BTreeSet<(u32, u32)>,
    mends: Vec<Mend>,
    /// When a look every member answered `NotHeld` of was judged, until the
    /// next begins: inside half a look's turn.
    empty_at: Option<u64>,
    stats: BTreeMap<&'static str, u64>,
}

impl World {
    fn apply(&mut self, cmd: Command) -> Answer {
        self.index += 1;
        self.state.apply_command(self.index, &cmd)
    }

    fn count(&mut self, what: &'static str) {
        *self.stats.entry(what).or_default() += 1;
    }

    fn version(&self) -> u32 {
        self.state.sets().get(self.set as usize).map_or(0, |s| s.version)
    }

    /// The scrub's turn: its asks queued for their members.
    fn scrub_turn(&mut self) -> Result<(), String> {
        let mut out = Vec::new();
        self.scrub.tick(&self.state, self.now, &mut self.scrub_id, &mut out);
        let asked = out.iter().any(|o| matches!(o, scrub::Out::Send { .. }));
        let raced = out.iter().find_map(|o| match o {
            scrub::Out::Send { bytes, .. } => Request::decode(bytes).and_then(|q| match q.op {
                Op::Scrub { chunk } => self.chunks.iter().position(|&c| c == chunk.chunk),
                _ => None,
            }),
            _ => None,
        });
        if let Some(at) = self.empty_at {
            /* A set with a joiner has nothing to look at: the next waits a
               turn, as it should */
            if asked || self.away.is_some() {
                self.empty_at = None;
            } else if self.now >= at + EVERY / 2 {
                return Err(format!("[{} ms] a look at a chunk no member holds, at {} ms, held the next back for the \
                                    pace", self.now / MS, at / MS));
            }
        }
        self.outs(out)?;
        match raced {
            Some(k) if self.writing && self.focus.chance(10) => self.race(k),
            _ => Ok(()),
        }
    }

    fn outs(&mut self, out: Vec<scrub::Out>) -> Result<(), String> {
        for o in out {
            match o {
                scrub::Out::Send { host, bytes } => {
                    let Some(m) = HOSTS.iter().position(|&h| h == host) else {
                        return Err(format!("the scrub asked host {}, none", host));
                    };
                    self.members[m].asks.push_back(bytes);
                }
                scrub::Out::Found { chunk, set, version, odd } => self.found(chunk, set, version, odd)?,
                scrub::Out::Say(line) => {
                    if self.trace {
                        eprintln!("  [{} ms] {}", self.now / MS, line);
                    }
                }
            }
        }
        Ok(())
    }

    /// A finding held to the rots there are, then mended as a person would.
    fn found(&mut self, chunk: u32, set: u32, version: u32, odd: Option<u32>) -> Result<(), String> {
        if version != self.version() {
            return Err(format!("chunk {} of set {} found at version {}, the set at {}", chunk, set, version,
                               self.version()));
        }
        if !self.said.insert((chunk, version)) {
            return Err(format!("chunk {} at version {} said twice", chunk, version));
        }
        let Some(k) = self.chunks.iter().position(|&c| c == chunk) else {
            return Err(format!("a finding of chunk {}, none of the volume's", chunk));
        };
        let rotted: BTreeSet<usize> = (0..self.members.len())
            .filter(|&m| self.served.get(&(k, m)).copied().unwrap_or(false)).collect();
        let right = match rotted.len() {
            0 => false,
            1 => rotted.iter().next().map(|&m| self.members[m].cs) == odd,
            _ => odd.is_none(),
        };
        if !right || set != self.set {
            return Err(format!("chunk {} of set {} found with odd {:?}, where the members rotted are {:?}", chunk, set,
                               odd, rotted));
        }
        self.count(if rotted.len() == 1 { "rots of one member found" } else { "rots of two found" });
        let at = self.now + self.moves.range(5000, 15000) * MS;
        self.mends.push(Mend { chunk: k, set, at, rotted });
        Ok(())
    }

    /// A finding acted on: each member found rotted taken down and joined
    /// back, its copy the others' again -- whatever of the rot is left.
    fn mend(&mut self, mend: Mend) -> Result<(), String> {
        let Mend { chunk: k, set, rotted, .. } = mend;
        if self.trace {
            eprintln!("  [{} ms] a person has chunk {}'s members {:?} copied again", self.now / MS, k, rotted);
        }
        self.back()?;
        for m in rotted {
            let cs = self.members[m].cs;
            let place = self.state.sets().get(set as usize).and_then(|s| s.cs.iter().position(|&c| c == cs))
                .ok_or_else(|| format!("cs {} is not in set {}", cs, set))?;
            let v = self.version();
            let since = match self.apply(Command::ReplicaBad { set, cs, version: v }) {
                Answer::Set { version } => match self.apply(Command::Join { set, place: place as u8, cs, version }) {
                    Answer::Set { version } => version,
                    other => return Err(format!("cs {}'s join back: {:?}", cs, other)),
                },
                other => return Err(format!("ReplicaBad of cs {}: {:?}", cs, other)),
            };
            for r in self.rots.iter().filter(|x| x.chunk == k && x.member == m) {
                self.members[m].copies[k].sums[r.granule] = r.was;
            }
            self.members[m].copies[k].held = true;
            self.rots.retain(|x| !(x.chunk == k && x.member == m));
            if let Answer::Refused(why) = self.apply(Command::Joined { set, cs, since }) {
                return Err(format!("cs {}'s join's end refused: {:?}", cs, why));
            }
        }
        Ok(())
    }

    /// Member `m`'s next message from the writer, if one is there.
    fn deliver(&mut self, m: usize) {
        let Some(msg) = self.members[m].inbox.pop_front() else { return };
        let version = self.version();
        if self.trace {
            match &msg {
                Msg::Write { chunk, granule, value } => {
                    eprintln!("  [{} ms] member {}: chunk {}'s granule {} written {:08x}", self.now / MS, m, chunk, granule,
                              value);
                }
                Msg::Close { chunk } => eprintln!("  [{} ms] member {}: chunk {} closed", self.now / MS, m, chunk),
            }
        }
        match msg {
            Msg::Write { chunk, granule, value } => {
                let c = &mut self.members[m].copies[chunk];
                c.held = true;
                c.open = true;
                c.written = version;
                c.sums[granule] = value;
                /* Written over: no longer rotted there */
                self.rots.retain(|x| !(x.chunk == chunk && x.member == m && x.granule == granule));
            }
            Msg::Close { chunk } => self.members[m].copies[chunk].open = false,
        }
    }

    /// Member `m`'s next ask from the scrub answered, if one is there.
    fn ask(&mut self, m: usize) -> Result<(), String> {
        let Some(bytes) = self.members[m].asks.pop_front() else { return Ok(()) };
        let Some(req) = Request::decode(&bytes) else { return Err(String::from("a scrub ask unread")) };
        let Op::Scrub { chunk } = req.op else { return Err(format!("the scrub asked {:?}", req.op)) };
        let reply = match self.chunks.iter().position(|&c| c == chunk.chunk) {
            Some(k) if !self.members[m].copies[k].held => {
                let rotted = self.rots.iter().any(|x| x.chunk == k && x.member == m);
                self.served.insert((k, m), rotted);
                Reply::Refused(nbswire::Refusal::NotHeld)
            }
            Some(k) => {
                let c = &self.members[m].copies[k];
                if self.trace {
                    let digest = c.sums.iter().fold(0u32, |a, &x| a.rotate_left(5) ^ x);
                    eprintln!("  [{} ms] member {} answers chunk {}: open {}, sums {:08x}", self.now / MS, m, k, c.open,
                              digest);
                }
                let reply = Reply::Scrub { open: c.open, written: c.written,
                                           sums: if c.open { Vec::new() } else { c.sums.clone() } };
                let rotted = self.rots.iter().any(|x| x.chunk == k && x.member == m);
                self.served.insert((k, m), rotted);
                reply
            }
            None => Reply::Refused(nbswire::Refusal::NotHeld),
        };
        self.answers.push(CsAnswer { id: req.id, reply }.encode());
        Ok(())
    }

    /// A step: time on, the writer now and then, a rot now and then, a few
    /// deliveries, the scrub's turn.
    fn step(&mut self, writing: bool) -> Result<(), String> {
        self.writing = writing;
        while let Some(i) = self.mends.iter().position(|x| self.now >= x.at) {
            let mend = self.mends.swap_remove(i);
            self.mend(mend)?;
        }
        self.now += if self.r.chance(3) { self.r.range(300, 1500) * MS } else { self.r.range(1, 20) * MS };
        if writing && self.r.chance(20) {
            let chunk = if self.r.chance(97) { self.r.below(OFTEN) as usize }
                        else { self.r.below(self.chunks.len() as u64) as usize };
            /* Nothing written to a chunk whose close is on its way: the
               client's writes to it wait for the close's end */
            let closing = self.members.iter().any(|x| x.inbox.iter().any(|msg| {
                matches!(msg, Msg::Close { chunk: c } if *c == chunk)
            }));
            if self.r.chance(70) {
                if !closing {
                    let granule = self.r.below(GRANULES as u64) as usize;
                    let value = self.r.below(1 << 32) as u32;
                    for m in 0..self.members.len() {
                        self.members[m].inbox.push_back(Msg::Write { chunk, granule, value });
                    }
                }
            } else if !self.members.iter().any(|x| x.inbox.iter().any(|msg| {
                matches!(msg, Msg::Write { chunk: c, .. } if *c == chunk)
            })) {
                /* A close only once every member has the writes before it --
                   the client closes a chunk once each acknowledged them */
                for m in 0..self.members.len() {
                    self.members[m].inbox.push_back(Msg::Close { chunk });
                }
                /* Now and then held back on its way to a member it is open on:
                   open there through two looks, closed on the others */
                let open: Vec<usize> = (0..self.members.len()).filter(|&m| self.members[m].copies[chunk].open).collect();
                if let Some(m) = self.focus.chance(10).then(|| self.focus.pick(&open)).flatten() {
                    self.members[m].stalled_until = self.now + self.focus.range(3000, 5000) * MS;
                    self.count("closes held back");
                    if self.trace {
                        eprintln!("  [{} ms] chunk {}'s close held back on its way to member {} until {} ms",
                                  self.now / MS, chunk, m, self.members[m].stalled_until / MS);
                    }
                }
            }
        }
        /* Rot while the writer writes: the end only finds */
        if writing && self.r.chance(1) {
            if self.r.chance(25) {
                self.lose();
            } else {
                self.rot();
            }
        }
        if writing {
            self.move_member()?;
        }
        for m in 0..self.members.len() {
            if self.r.chance(30) && self.now >= self.members[m].stalled_until {
                self.deliver(m);
            }
            if self.r.chance(30) {
                self.ask(m)?;
            }
        }
        if !self.answers.is_empty() && self.r.chance(30) {
            let k = self.r.below(self.answers.len() as u64) as usize;
            let bytes = self.answers.swap_remove(k);
            if let Some(a) = CsAnswer::decode(&bytes) {
                let mut out = Vec::new();
                let empty = self.scrub.stats.empty;
                self.scrub.answer(a.id, &a.reply, &self.state, self.now, &mut out);
                if self.scrub.stats.empty > empty {
                    self.empty_at = Some(self.now);
                }
                self.outs(out)?;
            }
        }
        self.scrub_turn()
    }

    /// A granule of a chunk closed on a member rotted there -- now and then
    /// on a second member too, elsewhere in the chunk.
    fn rot(&mut self) {
        let chunk = self.r.below(self.chunks.len() as u64) as usize;
        let first = self.r.below(self.members.len() as u64) as usize;
        let mut on = vec![first];
        if self.r.chance(25) {
            on.push((first + 1 + self.r.below(2) as usize) % self.members.len());
        }
        for (n, &m) in on.iter().enumerate() {
            let c = &self.members[m].copies[chunk];
            if !c.held || c.open || self.rots.iter().any(|x| x.chunk == chunk && x.member == m) {
                continue;
            }
            let half = GRANULES / 2;
            let granule = n * half + self.r.below(half as u64) as usize;
            let was = self.members[m].copies[chunk].sums[granule];
            self.members[m].copies[chunk].sums[granule] = was ^ (1 + self.r.below(u32::MAX as u64) as u32);
            self.rots.push(Rot { chunk, member: m, granule, was });
            self.count("granules rotted");
        }
    }
}

impl World {
    /// Now and then a member taken down -- as a writer's ask-out takes one
    /// -- and joined in its own place at once; or the one away back, once
    /// its time is up. The writer goes on writing it, as a joiner is
    /// written.
    fn move_member(&mut self) -> Result<(), String> {
        match self.away {
            Some(a) if self.now >= a.until => self.back(),
            None if self.moves.chance(1) => {
                let m = self.moves.below(self.members.len() as u64) as usize;
                let (set, cs) = (self.set, self.members[m].cs);
                let place = self.state.sets().get(set as usize).and_then(|s| s.cs.iter().position(|&c| c == cs))
                    .ok_or_else(|| format!("cs {} is not in set {}", cs, set))?;
                let v = self.version();
                let version = match self.apply(Command::ReplicaBad { set, cs, version: v }) {
                    Answer::Set { version } => version,
                    other => return Err(format!("cs {} not taken out: {:?}", cs, other)),
                };
                let since = match self.apply(Command::Join { set, place: place as u8, cs, version }) {
                    Answer::Set { version } => version,
                    other => return Err(format!("cs {} not joined back: {:?}", cs, other)),
                };
                let until = self.now + self.moves.range(100, 2500) * MS;
                self.away = Some(Away { member: m, since, until });
                self.count("members out and back");
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A write and a close of chunk `k` raced through the look just asked:
    /// one member's asks answered now, the write and the close then on
    /// every member, the others' answers to come after -- a difference of
    /// one look that no member holds, for the second look to see through.
    fn race(&mut self, k: usize) -> Result<(), String> {
        let first = self.focus.below(self.members.len() as u64) as usize;
        if self.trace {
            eprintln!("  [{} ms] a write of chunk {} raced through a look, member {} answering first", self.now / MS, k,
                      first);
        }
        while !self.members[first].asks.is_empty() {
            self.ask(first)?;
        }
        let granule = self.focus.below(GRANULES as u64) as usize;
        let value = self.focus.below(1 << 32) as u32;
        for m in 0..self.members.len() {
            self.members[m].inbox.push_back(Msg::Write { chunk: k, granule, value });
        }
        for m in 0..self.members.len() {
            while !self.members[m].inbox.is_empty() {
                self.deliver(m);
            }
        }
        for m in 0..self.members.len() {
            self.members[m].inbox.push_back(Msg::Close { chunk: k });
            while !self.members[m].inbox.is_empty() {
                self.deliver(m);
            }
            self.members[m].stalled_until = 0;
        }
        self.count("writes raced through a look");
        Ok(())
    }

    /// The member away back: what rotted on it, or it lost, copied again --
    /// a member back holds every chunk to a source's by sums -- and its join
    /// ended.
    fn back(&mut self) -> Result<(), String> {
        let Some(a) = self.away.take() else { return Ok(()) };
        let m = a.member;
        let World { rots, members, .. } = self;
        for r in rots.iter().filter(|x| x.member == m) {
            members[m].copies[r.chunk].sums[r.granule] = r.was;
            members[m].copies[r.chunk].held = true;
        }
        rots.retain(|x| x.member != m);
        let cs = self.members[m].cs;
        match self.apply(Command::Joined { set: self.set, cs, since: a.since }) {
            Answer::Refused(why) => Err(format!("cs {}'s join back refused: {:?}", cs, why)),
            _ => Ok(()),
        }
    }

    /// A chunk closed on a member lost there -- its record gone, as a
    /// device's might be: what it reads as is zeros, each granule that was
    /// not a rot of it. Only where no member's copy of it is rotted or lost
    /// already: two members lost alike would be two agreeing, which no scrub
    /// can tell from the third's being the one wrong.
    fn lose(&mut self) {
        let chunk = self.r.below(self.chunks.len() as u64) as usize;
        let m = self.r.below(self.members.len() as u64) as usize;
        let c = &self.members[m].copies[chunk];
        if !c.held || c.open || self.rots.iter().any(|x| x.chunk == chunk) {
            return;
        }
        let zero = nbscs::zero_sums()[0];
        for granule in 0..GRANULES {
            let was = self.members[m].copies[chunk].sums[granule];
            if was != zero {
                self.rots.push(Rot { chunk, member: m, granule, was });
            }
        }
        let c = &mut self.members[m].copies[chunk];
        c.held = false;
        c.sums = vec![zero; GRANULES];
        self.count("chunks lost on a member");
    }
}

fn setup(r: &mut Rng) -> Result<World, String> {
    let mut w = World { r: Rng::new(r.below(u64::MAX)), moves: Rng::new(r.below(u64::MAX)), away: None,
                        focus: Rng::new(r.below(u64::MAX)), writing: false, now: 0,
                        trace: false, state: State::new(), index: 0,
                        members: Vec::new(), chunks: Vec::new(), set: 0, scrub: Scrub::new(EVERY), scrub_id: 1,
                        answers: Vec::new(), rots: Vec::new(), served: BTreeMap::new(), said: BTreeSet::new(),
                        mends: Vec::new(),
                        empty_at: None, stats: BTreeMap::new() };
    w.apply(Command::Init { cluster: CLUSTER, name: String::from("nbs"), host: String::from(HOSTS[0]), member: 1 });
    for (k, h) in HOSTS.iter().enumerate().skip(1) {
        w.apply(Command::Add { host: h.to_string(), member: 1 + k as u64 });
    }
    let slots = nbscs::device_slots(8 * nbscs::SLOT);
    for h in HOSTS {
        match w.apply(Command::DeviceAdd { host: h.to_string(), ident: format!("disk:{}", h), slots }) {
            Answer::Device { .. } => {}
            other => return Err(format!("device of {} not added: {:?}", h, other)),
        }
    }
    match w.apply(Command::VolumeCreate { name: String::from(VOLUME), size_mib: VOLUME_MIB, host: HOSTS[0].to_string() }) {
        Answer::Volume { .. } => {}
        other => return Err(format!("the volume not made: {:?}", other)),
    }
    let v = w.state.volume(VOLUME).ok_or("no volume")?;
    w.chunks = v.map.iter().copied().collect();
    let sets: BTreeSet<u32> = w.chunks.iter().filter_map(|&c| w.state.chunks().get(c as usize).map(|x| x.set)).collect();
    if sets.len() != 1 {
        return Err(format!("the volume's chunks in {} sets, not one", sets.len()));
    }
    w.set = sets.into_iter().next().unwrap_or(0);
    let members = w.state.sets().get(w.set as usize).map(|s| s.cs).ok_or("no set")?;
    let version = w.version();
    for h in HOSTS {
        let cs = w.state.cs_of(h).map(|c| c.id).ok_or_else(|| format!("no cs of {}", h))?;
        if !members.contains(&cs) {
            return Err(format!("{}'s cs is not in the set", h));
        }
        w.members.push(Member { cs, stalled_until: 0, inbox: VecDeque::new(), asks: VecDeque::new(),
                                copies: vec![Copy { held: false, open: false, written: version,
                                                    sums: nbscs::zero_sums() }; w.chunks.len()] });
    }
    Ok(w)
}

pub fn run(seed: u64, steps: u64, trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x7363_7262));
    let mut w = setup(&mut r)?;
    w.trace = trace;
    for _ in 0..steps {
        w.step(true)?;
    }
    w.back()?;
    /* The end: the writer done -- what it sent delivered, then every chunk
       closed, as a client closes once every member has its writes -- and
       every rot there said within a minute */
    let drained = w.now + FOUND_WITHIN;
    while w.members.iter().any(|m| !m.inbox.is_empty()) {
        if w.now >= drained {
            return Err(String::from("the writer's messages not delivered a minute after its end"));
        }
        w.step(false)?;
    }
    for chunk in 0..w.chunks.len() {
        for m in 0..w.members.len() {
            w.members[m].inbox.push_back(Msg::Close { chunk });
        }
    }
    let until = w.now + FOUND_WITHIN;
    loop {
        /* Said: a finding of the chunk names the member, its mend to come */
        let unsaid: Vec<(usize, usize, usize)> = w.rots.iter()
            .filter(|x| !w.mends.iter().any(|m| m.chunk == x.chunk && m.rotted.contains(&x.member)))
            .map(|x| (x.chunk, x.member, x.granule)).collect();
        if unsaid.is_empty() && w.members.iter().all(|m| m.inbox.is_empty() && m.asks.is_empty()) {
            break;
        }
        if w.now >= until {
            return Err(format!("rots never said, a minute after the end: (chunk, member, granule) {:?} -- {:?}",
                               unsaid, w.scrub.stats));
        }
        w.step(false)?;
    }
    /* What was said mended, as the person would: every copy alike */
    for mend in std::mem::take(&mut w.mends) {
        w.mend(mend)?;
    }
    if !w.rots.is_empty() {
        return Err(format!("rots left once every finding was acted on: {:?}",
                           w.rots.iter().map(|x| (x.chunk, x.member, x.granule)).collect::<Vec<_>>()));
    }
    if std::env::var_os("NBS_SCRUB_STATS").is_some() {
        eprintln!("scrub seed {}: {:?} {:?}", seed, w.stats, w.scrub.stats);
    }
    Ok(())
}
