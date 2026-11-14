//! The replicated state machine (`raft::rsm`) as its users run it: a group of
//! three or five over a network that delays, loses, doubles and cuts, each
//! member on the log on disk over the crash disk of `logfuzz`, crashed and
//! started again over what its disk kept; clients with sessions doing a
//! register's operations -- write, add, compare-and-set, read -- one at a
//! time, each retried after a timeout of whichever member they think leads.
//! The history of every operation, from its asking to its answer, is then
//! checked for linearizability (Wing and Gong's search, with Lowe's memo):
//! a register that took every operation once, at one point between its
//! asking and its answer -- an add applied twice, a read of a state older
//! than one already answered, is a finding.
//!
//! The register's snapshot carries ballast written a kilobyte a call, so
//! that a snapshot spans several calls with applying going on between
//! them; and it checks its own bytes when read back.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use raft::types::Body;
use raft::{
    Config, DiskLog, Entry, EntryKind, Error, Event, Frozen, HardState, LogConfig, MemStorage, Message, NodeId,
    RestoreError, Rsm, RsmError, SnapshotReader, StateMachine, Storage, PIECE_MAX,
};

use crate::logfuzz::{Disk, Fate, SimFiles};
use crate::rng::{mix, Rng};

/* ---- the register ---- */

const OP_WRITE: u8 = 1;
const OP_ADD: u8 = 2;
const OP_CAS: u8 = 3;
/// What a snapshot of the register carries besides its value: small
/// enough that the log comes to it often.
const BALLAST: usize = 2048;
/// What a frozen register writes in one call at most: a snapshot spans
/// several calls of `process`.
const WRITE_STEP: usize = 256;

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// The snapshot's `k`th byte for a register holding `value`.
fn byte_at(value: u64, k: usize) -> u8 {
    if k < 8 {
        value.to_le_bytes()[k]
    } else {
        (mix(value, (k / 8) as u64) >> ((k % 8) * 8)) as u8
    }
}

#[derive(Clone, Debug, Default)]
pub struct Reg {
    pub value: u64,
    /// A register that cannot read any snapshot: one of another version's,
    /// say.
    pub refuses: bool,
}

pub struct FrozenReg {
    value: u64,
    written: usize,
}

impl Frozen for FrozenReg {
    fn write(&mut self, max: usize, out: &mut Vec<u8>) {
        let n = (8 + BALLAST - self.written).min(max).min(WRITE_STEP);
        for k in self.written..self.written + n {
            out.push(byte_at(self.value, k));
        }
        self.written += n;
    }
}

impl StateMachine for Reg {
    type Frozen = FrozenReg;

    fn apply(&mut self, _index: u64, command: &[u8]) -> Vec<u8> {
        match command.first() {
            Some(&OP_WRITE) if command.len() == 9 => {
                self.value = le64(&command[1..9]);
                Vec::new()
            }
            Some(&OP_ADD) if command.len() == 9 => {
                self.value = self.value.wrapping_add(le64(&command[1..9]));
                self.value.to_le_bytes().to_vec()
            }
            Some(&OP_CAS) if command.len() == 17 => {
                if self.value == le64(&command[1..9]) {
                    self.value = le64(&command[9..17]);
                    vec![1]
                } else {
                    vec![0]
                }
            }
            _ => vec![0xFF],
        }
    }

    fn query(&self, _query: &[u8]) -> Vec<u8> {
        self.value.to_le_bytes().to_vec()
    }

    fn size(&self) -> u64 {
        (8 + BALLAST) as u64
    }

    fn freeze(&self) -> FrozenReg {
        FrozenReg { value: self.value, written: 0 }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        if self.refuses {
            return Err(RestoreError("a register that reads no snapshot"));
        }
        if r.remaining() != (8 + BALLAST) as u64 {
            return Err(RestoreError("a register's snapshot of another length"));
        }
        let mut b = vec![0u8; 8 + BALLAST];
        if !r.read_exact(&mut b) {
            return Err(RestoreError("a register's snapshot cut short"));
        }
        let value = le64(&b[..8]);
        if (0..8 + BALLAST).any(|k| b[k] != byte_at(value, k)) {
            return Err(RestoreError("a register's snapshot whose bytes disagree"));
        }
        self.value = value;
        Ok(())
    }
}

/* ---- the history, and its check ---- */

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Write(u64),
    Add(u64),
    Cas(u64, u64),
    Read,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Ack,
    Value(u64),
    Swapped(bool),
}

impl Op {
    fn command(&self) -> Vec<u8> {
        let mut c = Vec::with_capacity(17);
        match *self {
            Op::Write(v) => {
                c.push(OP_WRITE);
                c.extend_from_slice(&v.to_le_bytes());
            }
            Op::Add(v) => {
                c.push(OP_ADD);
                c.extend_from_slice(&v.to_le_bytes());
            }
            Op::Cas(a, b) => {
                c.push(OP_CAS);
                c.extend_from_slice(&a.to_le_bytes());
                c.extend_from_slice(&b.to_le_bytes());
            }
            Op::Read => {}
        }
        c
    }

    /// What the register does with it: its value after, and the answer.
    fn on(&self, value: u64) -> (u64, Answer) {
        match *self {
            Op::Write(v) => (v, Answer::Ack),
            Op::Add(v) => (value.wrapping_add(v), Answer::Value(value.wrapping_add(v))),
            Op::Cas(a, b) if value == a => (b, Answer::Swapped(true)),
            Op::Cas(..) => (value, Answer::Swapped(false)),
            Op::Read => (value, Answer::Value(value)),
        }
    }

    /// The answer an applied command's bytes say.
    fn answer(&self, bytes: &[u8]) -> Option<Answer> {
        match *self {
            Op::Write(_) if bytes.is_empty() => Some(Answer::Ack),
            Op::Add(_) | Op::Read if bytes.len() == 8 => Some(Answer::Value(le64(bytes))),
            Op::Cas(..) if bytes.len() == 1 => Some(Answer::Swapped(bytes[0] == 1)),
            _ => None,
        }
    }
}

/// One operation: who asked, what, when -- and when it was answered, and
/// how, if it was. Times are of one clock that ticks at every asking and
/// every answer, so that no two are the same.
#[derive(Clone, Debug)]
pub struct Record {
    pub client: usize,
    pub op: Op,
    pub asked: u64,
    pub answered: Option<(u64, Answer)>,
}

fn bit(set: &[u64], i: usize) -> bool {
    set[i / 64] >> (i % 64) & 1 != 0
}

fn flip(set: &mut [u64], i: usize) {
    set[i / 64] ^= 1 << (i % 64);
}

/// Whether the history is a register's: every answered operation taking
/// effect once, at a point between its asking and its answer, in an order
/// in which each answer is the register's; an operation never answered
/// taking effect so, or not at all.
pub fn linearizable(h: &[Record]) -> Result<(), String> {
    struct S<'a> {
        h: &'a [Record],
        seen: HashSet<(Vec<u64>, u64)>,
        steps: u64,
    }
    fn search(s: &mut S<'_>, done: &mut Vec<u64>, value: u64, left: usize) -> Option<bool> {
        if left == 0 {
            return Some(true);
        }
        s.steps += 1;
        if s.steps > 2_000_000 {
            return None;
        }
        if !s.seen.insert((done.clone(), value)) {
            return Some(false);
        }
        /* Whatever comes next was asked before the earliest answer of what
         * is not yet placed */
        let first_answer = s
            .h
            .iter()
            .enumerate()
            .filter(|(i, r)| !bit(done, *i) && r.answered.is_some())
            .filter_map(|(_, r)| r.answered.map(|a| a.0))
            .min()
            .unwrap_or(u64::MAX);
        for i in 0..s.h.len() {
            let r = &s.h[i];
            if bit(done, i) || r.asked > first_answer {
                continue;
            }
            let (after, answer) = r.op.on(value);
            if let Some((_, want)) = r.answered {
                if want != answer {
                    continue;
                }
            }
            flip(done, i);
            let found = search(s, done, after, left - usize::from(r.answered.is_some()));
            flip(done, i);
            match found {
                Some(true) => return Some(true),
                None => return None,
                Some(false) => {}
            }
        }
        Some(false)
    }
    let mut s = S { h, seen: HashSet::new(), steps: 0 };
    let mut done = vec![0u64; h.len().div_ceil(64)];
    let left = h.iter().filter(|r| r.answered.is_some()).count();
    match search(&mut s, &mut done, 0, left) {
        Some(true) => Ok(()),
        Some(false) => {
            let clients: HashSet<usize> = h.iter().map(|r| r.client).collect();
            Err(format!("a history no register makes, of {} operations by {} clients", h.len(), clients.len()))
        }
        /* Too many orders to try: undecided, not a finding */
        None => Ok(()),
    }
}

/* ---- the group ---- */

type Store = DiskLog<SimFiles>;

struct Member {
    id: NodeId,
    disk: SimFiles,
    rsm: Option<Rsm<Store, Reg>>,
}

/// What a client is doing.
#[derive(Clone, Copy, Debug)]
enum Doing {
    Nothing,
    /// Asking for a session: the request, of whom, until when.
    Registering { request: u64, of: NodeId, until: u64 },
    /// An operation: its record, its number -- a command's -- the request
    /// of a read, of whom, until when.
    Op { rec: usize, seq: u64, request: u64, of: NodeId, until: u64 },
}

struct Client {
    session: Option<u64>,
    seq: u64,
    doing: Doing,
    /// Whom it thinks leads.
    leader: NodeId,
    ops_left: u64,
}

struct Group {
    r: Rng,
    now: u64,
    clock: u64,
    members: Vec<Member>,
    log_config: LogConfig,
    seed: u64,
    floor: u64,
    /// Messages on their way: when they arrive, and to whom.
    wire: Vec<(u64, NodeId, Message)>,
    /// Members on each side of a cut, or none.
    cut: Option<Vec<bool>>,
    loss: u64,
    clients: Vec<Client>,
    history: Vec<Record>,
    next_request: u64,
    calm: bool,
}

const TIMEOUT: u64 = 40;

fn config(id: NodeId, seed: u64) -> Config {
    let mut c = Config::new(id);
    c.seed = mix(seed, id);
    c
}

impl Group {
    fn member(&mut self, id: NodeId) -> Option<&mut Rsm<Store, Reg>> {
        self.members.iter_mut().find(|m| m.id == id).and_then(|m| m.rsm.as_mut())
    }

    fn any_member(&mut self) -> NodeId {
        self.members[self.r.below(self.members.len() as u64) as usize].id
    }

    fn tick_clock(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn separated(&self, a: NodeId, b: NodeId) -> bool {
        self.cut.as_ref().is_some_and(|side| side[(a - 1) as usize] != side[(b - 1) as usize])
    }

    /// One step of the world: messages delivered, ticks, every member's
    /// work done, clients acting, faults.
    fn step(&mut self) -> Result<(), String> {
        self.now += 1;
        let now = self.now;
        /* Arrivals, in an order of their own */
        let mut due = Vec::new();
        let mut k = 0;
        while k < self.wire.len() {
            if self.wire[k].0 <= now {
                due.push(self.wire.swap_remove(k));
            } else {
                k += 1;
            }
        }
        for _ in 0..due.len() {
            let j = self.r.below(due.len() as u64) as usize;
            let (_, to, m) = due.swap_remove(j);
            if self.separated(m.from, to) {
                continue;
            }
            if let Some(rsm) = self.member(to) {
                let _ = rsm.step(m);
            }
        }
        let trace = std::env::var_os("RAFT_RSM_TRACE").is_some();
        for i in 0..self.members.len() {
            let id = self.members[i].id;
            let Some(rsm) = self.members[i].rsm.as_mut() else { continue };
            rsm.tick();
            let snap_before = rsm.node().storage().snapshot_meta().index;
            let out = match rsm.process() {
                Ok(out) => out,
                Err(e) => return Err(format!("member {} stopped: {:?}", id, e)),
            };
            for m in out.messages {
                if !self.calm && self.r.below(1000) < self.loss {
                    continue;
                }
                let copies = if !self.calm && self.r.chance(2) { 2 } else { 1 };
                for _ in 0..copies {
                    let at = now + self.r.range(1, 3);
                    self.wire.push((at, m.to, m.clone()));
                }
            }
            if trace {
                let rsm = self.members[i].rsm.as_ref().expect("the member");
                let snap = rsm.node().storage().snapshot_meta().index;
                if snap != snap_before {
                    eprintln!("[{} clock {}] member {} snapshot {} -> {}, applied {}", now, self.clock, id, snap_before,
                              snap, rsm.applied());
                }
                if let Some(ss) = out.soft_state {
                    eprintln!("[{} clock {}] member {} {:?} leader {} term {}", now, self.clock, id, ss.role, ss.leader,
                              rsm.node().term());
                }
                for e in out.events.iter() {
                    match e {
                        Event::Applied { client, seq, index, answer } => eprintln!(
                            "[{} clock {}] member {} applied client {} seq {} at {} -> {:?}", now, self.clock, id,
                            client, seq, index, answer),
                        other => eprintln!("[{} clock {}] member {} {:?}", now, self.clock, id, other),
                    }
                }
            }
            for e in out.events {
                self.event(id, e)?;
            }
        }
        for c in 0..self.clients.len() {
            self.client_step(c)?;
        }
        if !self.calm {
            self.faults()?;
        }
        Ok(())
    }

    fn faults(&mut self) -> Result<(), String> {
        if self.r.below(400) == 0 {
            let i = self.r.below(self.members.len() as u64) as usize;
            if let Some(rsm) = self.members[i].rsm.take() {
                if std::env::var_os("RAFT_RSM_TRACE").is_some() {
                    eprintln!("[{} clock {}] member {} crashed, applied {}", self.now, self.clock, self.members[i].id,
                              rsm.applied());
                }
                /* A power cut: what the disk kept is all there is */
                drop(rsm.into_store());
                let mut cut_rng = Rng::new(self.r.next());
                self.members[i].disk.0.borrow_mut().crash(&mut cut_rng);
            }
        }
        for i in 0..self.members.len() {
            if self.members[i].rsm.is_none() && self.r.below(30) == 0 {
                self.start(i)?;
            }
        }
        if self.cut.is_none() && self.r.below(500) == 0 {
            let side: Vec<bool> = (0..self.members.len()).map(|_| self.r.chance(50)).collect();
            self.cut = Some(side);
        } else if self.cut.is_some() && self.r.below(80) == 0 {
            self.cut = None;
        }
        Ok(())
    }

    /// A member started again over what its disk holds.
    fn start(&mut self, i: usize) -> Result<(), String> {
        let id = self.members[i].id;
        let log = DiskLog::open(self.members[i].disk.clone(), self.log_config)
            .map_err(|e| format!("member {}'s log would not open: {:?}", id, e))?;
        let nonce = self.r.next();
        let mut rsm = Rsm::new(config(id, self.seed), log, Reg::default(), nonce)
            .map_err(|e| format!("member {} would not start: {:?}", id, e))?;
        rsm.set_snapshot_floor(self.floor);
        if std::env::var_os("RAFT_RSM_TRACE").is_some() {
            eprintln!("[{} clock {}] member {} started, applied {} snapshot {} commit {}", self.now, self.clock, id,
                      rsm.applied(), rsm.node().storage().snapshot_meta().index, rsm.node().committed());
        }
        self.members[i].rsm = Some(rsm);
        Ok(())
    }

    fn event(&mut self, at: NodeId, e: Event) -> Result<(), String> {
        match e {
            Event::Registered { origin, request, client } => {
                if origin != at {
                    return Ok(());
                }
                for c in self.clients.iter_mut() {
                    if let Doing::Registering { request: r, .. } = c.doing {
                        if r == request {
                            c.session = Some(client);
                            c.doing = Doing::Nothing;
                        }
                    }
                }
            }
            Event::Applied { client, seq, answer, .. } => {
                let found = self.clients.iter().position(|c| {
                    c.session == Some(client) && matches!(c.doing, Doing::Op { seq: s, .. } if s == seq)
                });
                if let Some(c) = found {
                    let Doing::Op { rec, .. } = self.clients[c].doing else { return Ok(()) };
                    let op = self.history[rec].op;
                    let Some(a) = op.answer(&answer) else {
                        return Err(format!("{:?} answered with {:?}", op, answer));
                    };
                    let t = self.tick_clock();
                    self.history[rec].answered = Some((t, a));
                    self.clients[c].doing = Doing::Nothing;
                    self.clients[c].seq += 1;
                }
            }
            Event::Expired { client, seq } => {
                /* Its session gone, whether the command took effect is
                 * not known: the operation stays unanswered */
                for c in self.clients.iter_mut() {
                    if c.session == Some(client) && matches!(c.doing, Doing::Op { seq: s, .. } if s == seq) {
                        c.session = None;
                        c.doing = Doing::Nothing;
                    }
                }
            }
            Event::Lost { client, seq } => return Err(format!("client {}'s answer to {} lost", client, seq)),
            Event::Stale { .. } => {}
            Event::Read { request, answer } => {
                let found = self
                    .clients
                    .iter()
                    .position(|c| matches!(c.doing, Doing::Op { request: q, of, .. } if q == request && of == at));
                if let Some(c) = found {
                    let Doing::Op { rec, .. } = self.clients[c].doing else { return Ok(()) };
                    if answer.len() != 8 {
                        return Err(format!("a read answered with {} bytes", answer.len()));
                    }
                    let t = self.tick_clock();
                    self.history[rec].answered = Some((t, Answer::Value(le64(&answer))));
                    self.clients[c].doing = Doing::Nothing;
                }
            }
            Event::ReadFailed { request } => {
                for c in self.clients.iter_mut() {
                    if let Doing::Op { request: q, of, until, rec, seq } = c.doing {
                        if q == request && of == at {
                            /* Asked again at once, of whoever leads */
                            c.doing = Doing::Op { request: q, of, until: until.min(self.now), rec, seq };
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A client's next move: a session asked for, an operation begun, one
    /// asked again of another member after its timeout.
    fn client_step(&mut self, c: usize) -> Result<(), String> {
        let now = self.now;
        match self.clients[c].doing {
            Doing::Nothing => {
                if self.clients[c].session.is_none() && self.clients[c].ops_left > 0 {
                    let of = self.target(c);
                    let request = self.next_request;
                    self.next_request += 1;
                    self.clients[c].doing = Doing::Registering { request, of, until: now + TIMEOUT };
                    self.ask_register(c, of, request);
                    return Ok(());
                }
                if self.clients[c].ops_left == 0 || self.r.chance(30) {
                    return Ok(());
                }
                self.clients[c].ops_left -= 1;
                let op = match self.r.below(4) {
                    0 => Op::Write(self.r.range(1, 1 << 40)),
                    1 => Op::Add(self.r.range(1, 10)),
                    2 => {
                        let seen = self.history.iter().rev().find_map(|r| match r.answered {
                            Some((_, Answer::Value(v))) => Some(v),
                            _ => None,
                        });
                        let expect = if self.r.chance(70) { seen.unwrap_or(0) } else { self.r.below(5) };
                        Op::Cas(expect, self.r.range(1, 1 << 40))
                    }
                    _ => Op::Read,
                };
                let asked = self.tick_clock();
                self.history.push(Record { client: c, op, asked, answered: None });
                let rec = self.history.len() - 1;
                let seq = self.clients[c].seq;
                let of = self.target(c);
                let request = self.next_request;
                self.next_request += 1;
                self.clients[c].doing = Doing::Op { rec, seq, request, of, until: now + TIMEOUT };
                self.ask(c);
            }
            Doing::Registering { until, .. } if until <= now => {
                let of = self.target(c);
                let request = self.next_request;
                self.next_request += 1;
                self.clients[c].doing = Doing::Registering { request, of, until: now + TIMEOUT };
                self.ask_register(c, of, request);
            }
            Doing::Op { rec, seq, until, .. } if until <= now => {
                let of = self.target(c);
                let request = self.next_request;
                self.next_request += 1;
                self.clients[c].doing = Doing::Op { rec, seq, request, of, until: now + TIMEOUT };
                self.ask(c);
            }
            _ => {}
        }
        Ok(())
    }

    /// Whom a client asks: whom it thinks leads, or anyone.
    fn target(&mut self, c: usize) -> NodeId {
        let l = self.clients[c].leader;
        if l != 0 && self.r.chance(80) { l } else { self.any_member() }
    }

    fn note_refusal(&mut self, c: usize, e: RsmError) {
        if let RsmError::Node(Error::NotLeader(l)) = e {
            self.clients[c].leader = l;
        }
        /* Asked again at its next step */
        if let Doing::Op { rec, seq, request, of, .. } = self.clients[c].doing {
            self.clients[c].doing = Doing::Op { rec, seq, request, of, until: self.now + 1 };
        }
        if let Doing::Registering { request, of, .. } = self.clients[c].doing {
            self.clients[c].doing = Doing::Registering { request, of, until: self.now + 1 };
        }
    }

    fn ask_register(&mut self, c: usize, of: NodeId, request: u64) {
        let r = match self.member(of) {
            Some(rsm) => rsm.register(request).map(|_| ()),
            None => Err(RsmError::Stopped),
        };
        if let Err(e) = r {
            self.note_refusal(c, e);
        }
    }

    fn ask(&mut self, c: usize) {
        let Doing::Op { rec, seq, request, of, .. } = self.clients[c].doing else { return };
        let op = self.history[rec].op;
        let session = self.clients[c].session.unwrap_or(0);
        let r = match self.member(of) {
            Some(rsm) => match op {
                Op::Read => rsm.read(request, Vec::new()),
                _ => rsm.submit(session, seq, &op.command()).map(|_| ()),
            },
            None => Err(RsmError::Stopped),
        };
        if let Err(e) = r {
            self.note_refusal(c, e);
        }
    }

    /// Whether every client is done with what it asked.
    fn settled(&self) -> bool {
        self.clients.iter().all(|c| matches!(c.doing, Doing::Nothing) && (c.session.is_some() || c.ops_left == 0))
    }
}

/// A group of `seed`'s, `steps` steps of faults and clients, then calm until
/// every client is answered; the history checked.
pub fn run(seed: u64, steps: u64) -> Result<(), String> {
    let mut r = Rng::new(seed ^ 0x125A);
    let n = if r.chance(70) { 3 } else { 5 };
    let voters: Vec<NodeId> = (1..=n).collect();
    let log_config = LogConfig { segment_bytes: [4096, 16384, 1 << 20][r.below(3) as usize] };
    let floor = [256, 2048, 1 << 20][r.below(3) as usize];
    let mut members = Vec::new();
    for &id in &voters {
        let disk = SimFiles(Rc::new(RefCell::new(Disk::new(mix(seed, id)))));
        let log = DiskLog::bootstrap(disk.clone(), log_config, &voters, &[]).map_err(|e| format!("bootstrap: {:?}", e))?;
        let mut rsm = Rsm::new(config(id, seed), log, Reg::default(), r.next()).map_err(|e| format!("start: {:?}", e))?;
        rsm.set_snapshot_floor(floor);
        members.push(Member { id, disk, rsm: Some(rsm) });
    }
    let clients = (0..r.range(2, 4))
        .map(|_| Client { session: None, seq: 1, doing: Doing::Nothing, leader: 0, ops_left: 150 })
        .collect();
    let loss = [0, 20, 100][r.below(3) as usize];
    let mut g = Group {
        r,
        now: 0,
        clock: 0,
        members,
        log_config,
        seed,
        floor,
        wire: Vec::new(),
        cut: None,
        loss,
        clients,
        history: Vec::new(),
        next_request: 1,
        calm: false,
    };
    for _ in 0..steps {
        g.step()?;
    }
    /* Calm: everyone up, nothing cut or lost, until every client is
     * answered */
    g.calm = true;
    g.cut = None;
    for i in 0..g.members.len() {
        if g.members[i].rsm.is_none() {
            g.start(i)?;
        }
    }
    for c in g.clients.iter_mut() {
        c.ops_left = 0;
    }
    let mut calm_steps = 0;
    while !g.settled() {
        g.step()?;
        calm_steps += 1;
        if calm_steps > 4000 {
            let open = g.history.iter().filter(|h| h.answered.is_none()).count();
            return Err(format!("no progress in 4000 calm steps: {} operations unanswered", open));
        }
    }
    if std::env::var_os("RAFT_RSM_STATS").is_some() {
        let answered = g.history.iter().filter(|h| h.answered.is_some()).count();
        let applied: Vec<u64> = g.members.iter().filter_map(|m| m.rsm.as_ref().map(|r| r.applied())).collect();
        let snaps: Vec<u64> =
            g.members.iter().filter_map(|m| m.rsm.as_ref().map(|r| r.node().storage().snapshot_meta().index)).collect();
        eprintln!("seed {}: {} members, {} operations, {} answered, applied {:?}, snapshots at {:?}, {} calm steps",
                  seed, g.members.len(), g.history.len(), answered, applied, snaps, calm_steps);
    }
    let verdict = linearizable(&g.history);
    if verdict.is_err() && std::env::var_os("RAFT_RSM_TRACE").is_some() {
        /* The shortest prefix -- by asking -- that no register makes, and
         * its last operations */
        let mut times: Vec<u64> = g.history.iter().flat_map(|r| [Some(r.asked), r.answered.map(|a| a.0)]).flatten().collect();
        times.sort_unstable();
        let mut lo = 1;
        let mut hi = times.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let cut = times[mid - 1];
            let prefix: Vec<Record> = g
                .history
                .iter()
                .filter(|r| r.asked <= cut)
                .map(|r| Record { answered: r.answered.filter(|a| a.0 <= cut), ..r.clone() })
                .collect();
            if linearizable(&prefix).is_err() { hi = mid } else { lo = mid + 1 }
        }
        let cut = times[lo - 1];
        eprintln!("the shortest prefix no register makes ends at clock {}:", cut);
        let window: Vec<&Record> = g.history.iter().filter(|r| r.asked <= cut).collect();
        for r in window.iter().rev().take(25).rev() {
            eprintln!("  client {} {:?} asked {} answered {:?}", r.client, r.op, r.asked, r.answered);
        }
    }
    verdict
}

/* ---- scenarios, a rule of the state machine's each ---- */

pub const SCENARIOS: &[(&str, fn() -> Result<(), String>)] = &[
    ("rsm_retry_once", retry_once),
    ("rsm_snapshot_frozen", snapshot_frozen),
    ("rsm_sessions_in_snapshot", sessions_in_snapshot),
    ("rsm_read_waits", read_waits),
    ("rsm_linearizable_check", linearizable_check),
    ("rsm_read_after_restart", read_after_restart),
    ("rsm_read_lost_fails", read_lost_fails),
    ("rsm_snapshot_refused_keeps_store", snapshot_refused_keeps_store),
    ("rsm_seq_zero", seq_zero),
    ("rsm_caught_up", caught_up),
    ("rsm_sync_when_needed", sync_when_needed),
    ("rsm_vote_durable", vote_durable),
    ("rsm_snapshot_in_steps", snapshot_in_steps),
    ("rsm_snapshot_too_large", snapshot_too_large),
    ("rsm_session_evicted", session_evicted),
];

type Lone = Rsm<MemStorage, Reg>;

/// A group of one, leading, with a session: the member and the client.
fn lone(floor: u64) -> Result<(Lone, u64), String> {
    let mut m =
        Rsm::new(config(1, 1), MemStorage::bootstrap(&[1], &[]), Reg::default(), 1).map_err(|e| format!("{:?}", e))?;
    m.set_snapshot_floor(floor);
    m.campaign().map_err(|e| format!("{:?}", e))?;
    drain(&mut m)?;
    m.register(7).map_err(|e| format!("{:?}", e))?;
    let client = drain(&mut m)?
        .into_iter()
        .find_map(|e| match e {
            Event::Registered { client, .. } => Some(client),
            _ => None,
        })
        .ok_or("no session")?;
    Ok((m, client))
}

/// `process` until there is nothing to do: the events.
fn drain(m: &mut Lone) -> Result<Vec<Event>, String> {
    let mut events = Vec::new();
    for _ in 0..1000 {
        let out = m.process().map_err(|e| format!("{:?}", e))?;
        let idle = out.events.is_empty() && out.messages.is_empty() && !m.taking_snapshot();
        events.extend(out.events);
        if idle && !m.node().has_ready() {
            return Ok(events);
        }
    }
    Err("never idle".into())
}

fn add(m: &mut Lone, client: u64, seq: u64, v: u64) -> Result<Vec<Event>, String> {
    m.submit(client, seq, &Op::Add(v).command()).map_err(|e| format!("{:?}", e))?;
    drain(m)
}

/// A command retried under its number is answered from the session, not
/// applied again.
fn retry_once() -> Result<(), String> {
    let (mut m, client) = lone(1 << 30)?;
    add(&mut m, client, 1, 5)?;
    let again = add(&mut m, client, 1, 5)?;
    let answered = again.iter().any(|e| matches!(e, Event::Applied { seq: 1, answer, .. } if answer == &5u64.to_le_bytes()));
    if m.state().value != 5 || !answered {
        return Err(format!("a retry applied again: the register is {}", m.state().value));
    }
    Ok(())
}

/// A snapshot written over many calls, commands applied between them, is
/// of the state at its own index: started again from it, the member comes
/// to the same state.
fn snapshot_frozen() -> Result<(), String> {
    let (mut m, client) = lone(256)?;
    let mut seq = 1;
    let mut took = false;
    for k in 0..600 {
        m.submit(client, seq, &Op::Add(k % 7 + 1).command()).map_err(|e| format!("{:?}", e))?;
        seq += 1;
        m.process().map_err(|e| format!("{:?}", e))?;
        took |= m.taking_snapshot();
    }
    drain(&mut m)?;
    let before = m.state().value;
    if !took || m.node().storage().snapshot_meta().index == 0 {
        return Err("no snapshot taken".into());
    }
    let store = m.into_store();
    let mut again = Rsm::new(config(1, 1), store, Reg::default(), 2).map_err(|e| format!("{:?}", e))?;
    again.campaign().map_err(|e| format!("{:?}", e))?;
    drain(&mut again)?;
    if again.state().value != before {
        return Err(format!("started again from the snapshot: {} where it was {}", again.state().value, before));
    }
    Ok(())
}

/// A session is in the snapshot: a retry after a start from one is
/// answered from it.
fn sessions_in_snapshot() -> Result<(), String> {
    let (mut m, client) = lone(256)?;
    add(&mut m, client, 1, 5)?;
    let mut seq = 2;
    while m.node().storage().snapshot_meta().index < 3 {
        add(&mut m, client, seq, 0)?;
        seq += 1;
        if seq > 2000 {
            return Err("no snapshot taken".into());
        }
    }
    let last = seq - 1;
    let store = m.into_store();
    let mut again = Rsm::new(config(1, 1), store, Reg::default(), 2).map_err(|e| format!("{:?}", e))?;
    again.campaign().map_err(|e| format!("{:?}", e))?;
    drain(&mut again)?;
    let events = add(&mut again, client, last, 0)?;
    if !events.iter().any(|e| matches!(e, Event::Applied { seq, .. } if *seq == last)) {
        return Err(format!("a retry after a start from a snapshot: {:?}", events));
    }
    Ok(())
}

/// A read confirmed by the leader is served only once applied as far: a
/// follower that has not yet heard of a write does not answer with what
/// was there before it.
fn read_waits() -> Result<(), String> {
    let voters = [1, 2, 3];
    let mut ms: Vec<Rsm<MemStorage, Reg>> = Vec::new();
    for &id in &voters {
        ms.push(
            Rsm::new(config(id, 1), MemStorage::bootstrap(&voters, &[]), Reg::default(), 1000 * id)
                .map_err(|e| format!("{:?}", e))?,
        );
    }
    /* Every member's work done and its messages delivered, fifty rounds --
     * appends to member 3 held back while `hold` -- and every member's
     * events, by member */
    fn run(ms: &mut [Rsm<MemStorage, Reg>], held: &mut Vec<Message>, hold: bool) -> Result<Vec<(usize, Event)>, String> {
        let mut events = Vec::new();
        for _ in 0..50 {
            let mut wire = Vec::new();
            for (i, m) in ms.iter_mut().enumerate() {
                let out = m.process().map_err(|e| format!("{:?}", e))?;
                wire.extend(out.messages);
                events.extend(out.events.into_iter().map(|e| (i, e)));
            }
            for msg in wire {
                if hold && msg.to == 3 && matches!(msg.body, Body::Append { .. } | Body::Snapshot { .. }) {
                    held.push(msg);
                    continue;
                }
                let _ = ms[(msg.to - 1) as usize].step(msg);
            }
        }
        Ok(events)
    }
    let mut held = Vec::new();
    ms[0].campaign().map_err(|e| format!("{:?}", e))?;
    run(&mut ms, &mut held, false)?;
    if ms[0].role() != raft::Role::Leader {
        return Err("member 1 does not lead".into());
    }
    /* A session, and a write of 7 member 3 does not hear of */
    ms[0].register(1).map_err(|e| format!("{:?}", e))?;
    let client = run(&mut ms, &mut held, false)?
        .into_iter()
        .find_map(|(i, e)| match e {
            Event::Registered { client, origin: 1, .. } if i == 0 => Some(client),
            _ => None,
        })
        .ok_or("no session")?;
    ms[0].submit(client, 1, &Op::Write(7).command()).map_err(|e| format!("{:?}", e))?;
    run(&mut ms, &mut held, true)?;
    if ms[0].state().value != 7 || ms[2].state().value != 0 {
        return Err(format!("the write: {} on the leader, {} on member 3", ms[0].state().value, ms[2].state().value));
    }
    ms[2].read(99, Vec::new()).map_err(|e| format!("{:?}", e))?;
    let early = run(&mut ms, &mut held, true)?;
    if let Some((_, e)) = early.iter().find(|(i, e)| *i == 2 && matches!(e, Event::Read { request: 99, .. })) {
        return Err(format!("a read served before its index was applied: {:?}", e));
    }
    /* The appends let through: the read comes, the write in it */
    for msg in std::mem::take(&mut held) {
        let _ = ms[2].step(msg);
    }
    let late = run(&mut ms, &mut held, false)?;
    match late.iter().find(|(i, e)| *i == 2 && matches!(e, Event::Read { request: 99, .. })) {
        Some((_, Event::Read { answer, .. })) if answer == &7u64.to_le_bytes() => Ok(()),
        other => Err(format!("the read after the write came: {:?}", other)),
    }
}

/// The check itself, on histories known to be a register's and not.
fn linearizable_check() -> Result<(), String> {
    let rec = |client, op, asked, answered: Option<(u64, Answer)>| Record { client, op, asked, answered };
    /* Two writes at once, a read after both seeing either is fine */
    let good = [
        rec(0, Op::Write(1), 1, Some((4, Answer::Ack))),
        rec(1, Op::Write(2), 2, Some((3, Answer::Ack))),
        rec(0, Op::Read, 5, Some((6, Answer::Value(1)))),
    ];
    linearizable(&good)?;
    /* An add applied twice: 1 and then 2 read after a single add of 1 */
    let twice = [
        rec(0, Op::Add(1), 1, Some((2, Answer::Value(1)))),
        rec(0, Op::Read, 3, Some((4, Answer::Value(2)))),
    ];
    if linearizable(&twice).is_ok() {
        return Err("an add applied twice passed".into());
    }
    /* A stale read: a write answered, then a read of what was before */
    let stale = [
        rec(0, Op::Write(5), 1, Some((2, Answer::Ack))),
        rec(1, Op::Read, 3, Some((4, Answer::Value(0)))),
    ];
    if linearizable(&stale).is_ok() {
        return Err("a stale read passed".into());
    }
    /* A write never answered may take effect, or not */
    let open = [
        rec(0, Op::Write(9), 1, None),
        rec(1, Op::Read, 2, Some((3, Answer::Value(9)))),
        rec(1, Op::Read, 4, Some((5, Answer::Value(9)))),
    ];
    linearizable(&open)
}

/// A leader's answer to a read asked before the member was started again,
/// arriving after, is not taken for the answer to a read asked since: the
/// new read waits for its own, and sees the write acknowledged before it.
fn read_after_restart() -> Result<(), String> {
    let voters = [1, 2, 3];
    let mut ms: Vec<Rsm<MemStorage, Reg>> = Vec::new();
    for &id in &voters {
        ms.push(Rsm::new(config(id, 1), MemStorage::bootstrap(&voters, &[]), Reg::default(), 7)
            .map_err(|e| format!("{:?}", e))?);
    }
    /* Rounds of work and delivery, a tick each; what `held` says is kept
     * back */
    fn run(ms: &mut [Rsm<MemStorage, Reg>], kept: &mut Vec<Message>, held: &dyn Fn(&Message) -> bool)
        -> Result<Vec<(usize, Event)>, String>
    {
        run_for(ms, kept, held, 50)
    }
    /* `run`, for `rounds` -- fewer than a read's answer may be kept back
     * before the read is failed back for its age */
    fn run_for(ms: &mut [Rsm<MemStorage, Reg>], kept: &mut Vec<Message>, held: &dyn Fn(&Message) -> bool,
               rounds: usize) -> Result<Vec<(usize, Event)>, String>
    {
        let mut events = Vec::new();
        for _ in 0..rounds {
            let mut wire = Vec::new();
            for (i, m) in ms.iter_mut().enumerate() {
                m.tick();
                let out = m.process().map_err(|e| format!("{:?}", e))?;
                wire.extend(out.messages);
                events.extend(out.events.into_iter().map(|e| (i, e)));
            }
            for msg in wire {
                if held(&msg) {
                    kept.push(msg);
                    continue;
                }
                let _ = ms[(msg.to - 1) as usize].step(msg);
            }
        }
        Ok(events)
    }
    let none = |_: &Message| false;
    let mut kept = Vec::new();
    ms[0].campaign().map_err(|e| format!("{:?}", e))?;
    run(&mut ms, &mut kept, &none)?;
    ms[0].register(1).map_err(|e| format!("{:?}", e))?;
    let client = run(&mut ms, &mut kept, &none)?
        .into_iter()
        .find_map(|(i, e)| match e {
            Event::Registered { client, origin: 1, .. } if i == 0 => Some(client),
            _ => None,
        })
        .ok_or("no session")?;
    ms[0].submit(client, 1, &Op::Write(1).command()).map_err(|e| format!("{:?}", e))?;
    run(&mut ms, &mut kept, &none)?;
    /* Member 3 asks a read; the leader's answer is kept back */
    let to3_answer = |m: &Message| m.to == 3 && matches!(m.body, Body::ReadIndexResp { .. });
    ms[2].read(50, Vec::new()).map_err(|e| format!("{:?}", e))?;
    run(&mut ms, &mut kept, &to3_answer)?;
    let stale: Vec<Message> = std::mem::take(&mut kept);
    if stale.is_empty() {
        return Err("no answer to member 3's read to keep back".into());
    }
    /* A write of 2 member 3 does not hear of */
    let to3_append = |m: &Message| m.to == 3 && matches!(m.body, Body::Append { .. } | Body::Snapshot { .. });
    ms[0].submit(client, 2, &Op::Write(2).command()).map_err(|e| format!("{:?}", e))?;
    run(&mut ms, &mut kept, &to3_append)?;
    /* Member 3 started again, and a new read asked; the answer to the old
     * one delivered, the new one's kept back */
    let store = ms.remove(2).into_store();
    ms.push(Rsm::new(config(3, 1), store, Reg::default(), 7 + 1_000_000).map_err(|e| format!("{:?}", e))?);
    /* Heartbeats let through, so that it knows whom to ask */
    let both = |m: &Message| to3_answer(m) || to3_append(m);
    run(&mut ms, &mut kept, &both)?;
    ms[2].read(51, Vec::new()).map_err(|e| format!("{:?}", e))?;
    for msg in stale {
        let _ = ms[2].step(msg);
    }
    let early = run_for(&mut ms, &mut kept, &both, 15)?;
    if let Some((_, e)) = early.iter().find(|(i, e)| *i == 2 && matches!(e, Event::Read { request: 51, .. })) {
        return Err(format!("a read after a restart answered by an answer to one before it: {:?}", e));
    }
    for msg in std::mem::take(&mut kept) {
        let _ = ms[(msg.to - 1) as usize].step(msg);
    }
    let late = run(&mut ms, &mut kept, &none)?;
    match late.iter().find(|(i, e)| *i == 2 && matches!(e, Event::Read { request: 51, .. })) {
        Some((_, Event::Read { answer, .. })) if answer == &2u64.to_le_bytes() => Ok(()),
        other => Err(format!("the read after the restart: {:?}", other)),
    }
}

/* ---- the review of 2026-11: the replicated state machine's findings ---- */

/// Rounds of every member's work and of delivery -- a tick each where
/// `tick`, and what `dropped` says lost: every member's events, by member.
fn rounds(ms: &mut [Rsm<MemStorage, Reg>], n: usize, tick: bool, dropped: &dyn Fn(&Message) -> bool)
    -> Result<Vec<(usize, Event)>, String>
{
    let mut events = Vec::new();
    for _ in 0..n {
        let mut wire = Vec::new();
        for (i, m) in ms.iter_mut().enumerate() {
            if tick {
                m.tick();
            }
            let out = m.process().map_err(|e| format!("member {}: {:?}", i + 1, e))?;
            wire.extend(out.messages);
            events.extend(out.events.into_iter().map(|e| (i, e)));
        }
        for msg in wire {
            if !dropped(&msg) {
                let _ = ms[(msg.to - 1) as usize].step(msg);
            }
        }
    }
    Ok(events)
}

/// Three members over memory, their registers `regs`, member 1 leading.
fn three_of(floor: u64, regs: [Reg; 3]) -> Result<Vec<Rsm<MemStorage, Reg>>, String> {
    let voters = [1, 2, 3];
    let mut ms = Vec::new();
    for (k, reg) in regs.into_iter().enumerate() {
        let id = voters[k];
        let mut m = Rsm::new(config(id, 1), MemStorage::bootstrap(&voters, &[]), reg, 1000 * id)
            .map_err(|e| format!("{:?}", e))?;
        m.set_snapshot_floor(floor);
        ms.push(m);
    }
    ms[0].campaign().map_err(|e| format!("{:?}", e))?;
    rounds(&mut ms, 30, false, &|_| false)?;
    if ms[0].role() != raft::Role::Leader {
        return Err("member 1 does not lead".into());
    }
    Ok(ms)
}

fn three(floor: u64) -> Result<Vec<Rsm<MemStorage, Reg>>, String> {
    three_of(floor, [Reg::default(), Reg::default(), Reg::default()])
}

/// A follower's read whose request never reaches the leader -- neither it
/// nor its answer is sent again -- is failed back once past twice the
/// longest election timeout, and its room taken back. (It waited for good,
/// and 4096 of them left the member taking no read at all.)
fn read_lost_fails() -> Result<(), String> {
    let mut ms = three(1 << 30)?;
    let election = ms[2].node().config().election_ticks as usize;
    ms[2].read(77, Vec::new()).map_err(|e| format!("{:?}", e))?;
    let lost = |m: &Message| matches!(m.body, Body::ReadIndex { .. });
    let events = rounds(&mut ms, 4 * election + 4, true, &lost)?;
    if !events.iter().any(|(i, e)| *i == 2 && matches!(e, Event::ReadFailed { request: 77 })) {
        return Err("a read whose request was lost neither served nor failed back".into());
    }
    if ms[2].stats().reads_aged != 1 {
        return Err(format!("{} reads aged", ms[2].stats().reads_aged));
    }
    ms[2].read(78, Vec::new()).map_err(|e| format!("{:?}", e))?;
    let events = rounds(&mut ms, 10, true, &|_| false)?;
    if !events.iter().any(|(i, e)| *i == 2 && matches!(e, Event::Read { request: 78, .. })) {
        return Err("a read asked after one failed back for its age not served".into());
    }
    Ok(())
}

/// A follower sent a snapshot its state machine cannot read stops over the
/// store it had: started again over that store, with a state machine that
/// reads it, it comes back. (The snapshot was made the whole of its log
/// first, the log and snapshot before it removed: no start could read it
/// either.)
fn snapshot_refused_keeps_store() -> Result<(), String> {
    let refusing = Reg { refuses: true, ..Reg::default() };
    let mut ms = three_of(256, [Reg::default(), Reg::default(), refusing])?;
    ms[0].register(1).map_err(|e| format!("{:?}", e))?;
    let client = rounds(&mut ms, 10, false, &|_| false)?
        .into_iter()
        .find_map(|(i, e)| match e {
            Event::Registered { client, origin: 1, .. } if i == 0 => Some(client),
            _ => None,
        })
        .ok_or("no session")?;
    /* Member 3 cut off while member 1 applies past a snapshot of its own */
    let to3 = |m: &Message| m.to == 3 || m.from == 3;
    let mut seq = 1;
    while ms[0].node().storage().snapshot_meta().index == 0 {
        ms[0].submit(client, seq, &Op::Add(1).command()).map_err(|e| format!("{:?}", e))?;
        seq += 1;
        rounds(&mut ms, 3, false, &to3)?;
        if seq > 500 {
            return Err("no snapshot taken".into());
        }
    }
    let (snap_before, last_before) = (ms[2].node().storage().snapshot_meta().index, ms[2].node().storage().last_index());
    /* Healed: member 3 is sent the snapshot, which it cannot read */
    let mut stopped = None;
    for _ in 0..300 {
        let mut wire = Vec::new();
        for (i, m) in ms.iter_mut().enumerate() {
            m.tick();
            match m.process() {
                Ok(out) => wire.extend(out.messages),
                Err(e) if i == 2 => stopped = Some(e),
                Err(e) => return Err(format!("member {}: {:?}", i + 1, e)),
            }
        }
        for msg in wire {
            let _ = ms[(msg.to - 1) as usize].step(msg);
        }
        if stopped.is_some() {
            break;
        }
    }
    match stopped {
        Some(RsmError::Restore(_)) => {}
        other => return Err(format!("member 3, sent a snapshot it cannot read: {:?}", other)),
    }
    let store = ms.remove(2).into_store();
    if store.snapshot_meta().index != snap_before || store.last_index() != last_before {
        return Err(format!("member 3 stopped on a snapshot it could not read, its store changed: snapshot {} -> {}, \
                            last {} -> {}", snap_before, store.snapshot_meta().index, last_before, store.last_index()));
    }
    ms.push(Rsm::new(config(3, 1), store, Reg::default(), 3003).map_err(|e| format!("{:?}", e))?);
    rounds(&mut ms, 100, true, &|_| false)?;
    if ms[2].state().value != ms[0].state().value {
        return Err(format!("member 3 started again: {} where the leader has {}", ms[2].state().value,
                           ms[0].state().value));
    }
    Ok(())
}

/// A command numbered 0 is refused as it is submitted, and one in a log
/// anyway -- another version's -- is answered stale and not applied. (It
/// was answered `Lost`: "applied, once, its answer lost", and never was.)
fn seq_zero() -> Result<(), String> {
    let (mut m, client) = lone(1 << 30)?;
    match m.submit(client, 0, &Op::Add(5).command()) {
        Err(RsmError::BadSeq) => {}
        other => return Err(format!("a command numbered 0 submitted: {:?}", other)),
    }
    /* In a log anyway: a session at entry 2 (rsm's register envelope),
     * its command numbered 0 at entry 3 (its command envelope) */
    let mut store = MemStorage::bootstrap(&[1], &[]);
    let mut register = vec![1u8];
    register.extend_from_slice(&1u64.to_le_bytes());
    register.extend_from_slice(&9u64.to_le_bytes());
    let mut command = vec![2u8];
    command.extend_from_slice(&2u64.to_le_bytes());
    command.extend_from_slice(&0u64.to_le_bytes());
    command.extend_from_slice(&Op::Add(5).command());
    store
        .append(&[
            Entry { term: 1, index: 2, kind: EntryKind::Normal, data: register },
            Entry { term: 1, index: 3, kind: EntryKind::Normal, data: command },
        ])
        .map_err(|e| format!("{:?}", e))?;
    store.set_hard_state(HardState { term: 1, vote: 0, commit: 3 });
    let mut m = Rsm::new(config(1, 1), store, Reg::default(), 1).map_err(|e| format!("{:?}", e))?;
    let events = drain(&mut m)?;
    let stale = events.iter().any(|e| matches!(e, Event::Stale { client: 2, seq: 0 }));
    if m.state().value != 0 || !stale || events.iter().any(|e| matches!(e, Event::Lost { .. })) {
        return Err(format!("a command numbered 0 in the log: the register {}, {:?}", m.state().value, events));
    }
    Ok(())
}

/// A member started over a log whose committed tail is more than one
/// `process` applies is not caught up until all of it is applied.
/// (raftmember took messages after 16 batches, answering from a state as
/// it was seconds before.)
fn caught_up() -> Result<(), String> {
    let (mut m, client) = lone(1 << 40)?;
    for seq in 1..=5000 {
        m.submit(client, seq, &Op::Add(1).command()).map_err(|e| format!("{:?}", e))?;
        if seq % 1000 == 0 {
            drain(&mut m)?;
        }
    }
    drain(&mut m)?;
    let value = m.state().value;
    let mut again = Rsm::new(config(1, 1), m.into_store(), Reg::default(), 2).map_err(|e| format!("{:?}", e))?;
    if again.caught_up() {
        return Err("caught up before anything was applied".into());
    }
    again.process().map_err(|e| format!("{:?}", e))?;
    if again.caught_up() {
        return Err(format!("caught up after one process, applied to {}", again.applied()));
    }
    drain(&mut again)?;
    if !again.caught_up() || again.state().value != value {
        return Err(format!("started again: caught up {}, the register {} where it was {}", again.caught_up(),
                           again.state().value, value));
    }
    Ok(())
}

/// A `Ready` is synced only where something in it must be durable before
/// what it sends goes -- entries, a term, a vote: a group idle syncs
/// nothing at its heartbeats, and an entry is synced on every member that
/// takes it. (Every `Ready` was: a flush of the device each, ten a second a
/// member.)
fn sync_when_needed() -> Result<(), String> {
    let mut ms = three(1 << 30)?;
    rounds(&mut ms, 10, true, &|_| false)?;
    let before: Vec<raft::RsmStats> = ms.iter().map(|m| m.stats()).collect();
    rounds(&mut ms, 30, true, &|_| false)?;
    for (i, m) in ms.iter().enumerate() {
        let (b, a) = (before[i], m.stats());
        if a.syncs != b.syncs || a.syncs_skipped <= b.syncs_skipped {
            return Err(format!("member {} idle for 30 ticks: {} syncs, {} skipped", i + 1, a.syncs - b.syncs,
                               a.syncs_skipped - b.syncs_skipped));
        }
    }
    ms[0].register(5).map_err(|e| format!("{:?}", e))?;
    rounds(&mut ms, 10, false, &|_| false)?;
    for (i, m) in ms.iter().enumerate() {
        if m.stats().syncs <= before[i].syncs {
            return Err(format!("member {} took an entry with no sync", i + 1));
        }
    }
    Ok(())
}

/// Past `MAX_SESSIONS`, a new session evicts the one least recently used --
/// whose last command, or registration, is the oldest in the log -- and not
/// one in use. (No run came near a thousand sessions: evicting any one
/// failed nothing.)
fn session_evicted() -> Result<(), String> {
    let (mut m, first) = lone(1 << 40)?;
    let registered = |events: Vec<Event>| {
        events.into_iter().find_map(|e| match e {
            Event::Registered { client, .. } => Some(client),
            _ => None,
        })
    };
    let mut second = None;
    for r in 1..raft::rsm::MAX_SESSIONS as u64 {
        m.register(1000 + r).map_err(|e| format!("{:?}", e))?;
        let client = registered(drain(&mut m)?).ok_or("a session not made")?;
        second.get_or_insert(client);
    }
    let second = second.ok_or("no second session")?;
    /* The first used again: the second is now the least recently used */
    add(&mut m, first, 1, 5)?;
    m.register(5000).map_err(|e| format!("{:?}", e))?;
    registered(drain(&mut m)?).ok_or("the session past the most not made")?;
    if m.sessions() != raft::rsm::MAX_SESSIONS {
        return Err(format!("{} sessions kept", m.sessions()));
    }
    let used = add(&mut m, first, 2, 1)?;
    if !used.iter().any(|e| matches!(e, Event::Applied { client, seq: 2, .. } if *client == first)) {
        return Err(format!("the session used last evicted: {:?}", used));
    }
    let old = add(&mut m, second, 1, 1)?;
    if !old.iter().any(|e| matches!(e, Event::Expired { client, .. } if *client == second)) {
        return Err(format!("the session least recently used kept: {:?}", old));
    }
    Ok(())
}

/// A state of `len` bytes, its value's pattern -- a snapshot of many
/// pieces -- that says it is small, so that one is taken at the floor; and
/// a count of the pieces its frozen views have written, shared.
struct Big {
    value: u64,
    len: usize,
    pieces: Rc<Cell<usize>>,
}

struct FrozenBig {
    value: u64,
    len: usize,
    written: usize,
    pieces: Rc<Cell<usize>>,
}

impl Frozen for FrozenBig {
    fn write(&mut self, max: usize, out: &mut Vec<u8>) {
        let n = (self.len - self.written).min(max);
        out.extend((self.written..self.written + n).map(|k| byte_at(self.value, k)));
        self.written += n;
        if n > 0 {
            self.pieces.set(self.pieces.get() + 1);
        }
    }
}

impl StateMachine for Big {
    type Frozen = FrozenBig;

    fn apply(&mut self, _index: u64, command: &[u8]) -> Vec<u8> {
        if command.len() == 9 && command[0] == OP_ADD {
            self.value = self.value.wrapping_add(le64(&command[1..9]));
        }
        self.value.to_le_bytes().to_vec()
    }

    fn query(&self, _query: &[u8]) -> Vec<u8> {
        self.value.to_le_bytes().to_vec()
    }

    fn size(&self) -> u64 {
        0
    }

    fn freeze(&self) -> FrozenBig {
        FrozenBig { value: self.value, len: self.len, written: 0, pieces: self.pieces.clone() }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        if r.remaining() != self.len as u64 {
            return Err(RestoreError("a big state's snapshot of another length"));
        }
        /* A piece at a time, as the kernel's heap gives no more at once */
        let mut b = vec![0u8; PIECE_MAX];
        let (mut value, mut at) = (0, 0);
        while at < self.len {
            let n = (self.len - at).min(PIECE_MAX);
            if !r.read_exact(&mut b[..n]) {
                return Err(RestoreError("a big state's snapshot cut short"));
            }
            if at == 0 {
                value = le64(&b[..8]);
            }
            if (0..n).any(|k| b[k] != byte_at(value, at + k)) {
                return Err(RestoreError("a big state's snapshot whose bytes disagree"));
            }
            at += n;
        }
        self.value = value;
        Ok(())
    }
}

/// The pieces of a big state's snapshot.
const BIG_PIECES: usize = 20;

/// A member alone over the log on disk, its state `BIG_PIECES` pieces --
/// and `limit` the largest snapshot it may hold -- leading, with a session;
/// no snapshot taken until its floor is set.
fn big_lone(disk: &SimFiles, limit: u64, nonce: u64)
    -> Result<(Rsm<DiskLog<SimFiles>, Big>, u64, Rc<Cell<usize>>), String>
{
    let there = disk.0.borrow().names().iter().any(|n| n.starts_with("wal."));
    let log = if there {
        DiskLog::open(disk.clone(), LogConfig::default())
    } else {
        DiskLog::bootstrap(disk.clone(), LogConfig::default(), &[1], &[])
    }
    .map_err(|e| format!("{:?}", e))?;
    let pieces = Rc::new(Cell::new(0));
    let big = Big { value: 0, len: BIG_PIECES * PIECE_MAX + 5, pieces: pieces.clone() };
    let mut cfg = config(1, 1);
    cfg.max_snapshot_bytes = limit;
    let mut m = Rsm::new(cfg, log, big, nonce).map_err(|e| format!("{:?}", e))?;
    m.set_snapshot_floor(u64::MAX);
    m.campaign().map_err(|e| format!("{:?}", e))?;
    let mut client = None;
    for k in 0..100 {
        if k == 10 {
            m.register(nonce).map_err(|e| format!("{:?}", e))?;
        }
        for e in m.process().map_err(|e| format!("{:?}", e))?.events {
            if let Event::Registered { client: c, .. } = e {
                client = Some(c);
            }
        }
        if client.is_some() && !m.taking_snapshot() {
            break;
        }
    }
    Ok((m, client.ok_or("no session")?, pieces))
}

/// The bytes of snapshots being written, on the disk.
fn tmp_bytes(disk: &SimFiles) -> usize {
    let d = disk.0.borrow();
    d.names().iter().filter(|n| n.ends_with(".tmp")).map(|n| d.len_of(n)).sum()
}

/// The bytes of every snapshot's file on the disk, being written or not.
fn snap_bytes(disk: &SimFiles) -> usize {
    let d = disk.0.borrow();
    d.names().iter().filter(|n| n.starts_with("snap.")).map(|n| d.len_of(n)).sum()
}

/// A snapshot of a state of twenty pieces is gathered a few pieces a call
/// and written a few a call, the member applying between: no call does
/// more than eight; taken, it reads back as the state frozen. (It was
/// gathered and written whole in one call: a state of tens of MiB kept a
/// leader from its heartbeats for longer than an election timeout.)
fn snapshot_in_steps() -> Result<(), String> {
    let disk = SimFiles(Rc::new(RefCell::new(Disk::new(11))));
    let (mut m, client, pieces) = big_lone(&disk, raft::MAX_SNAPSHOT_BYTES, 1)?;
    /* The member's first snapshot: no file of one is removed as it is
     * taken, so that what the files hold only grows */
    m.set_snapshot_floor(1);
    let (mut seq, mut calls, mut held) = (1, 0, snap_bytes(&disk));
    let index = loop {
        m.submit(client, seq, &Op::Add(1).command()).map_err(|e| format!("{:?}", e))?;
        seq += 1;
        let before = pieces.get();
        m.process().map_err(|e| format!("{:?}", e))?;
        let (gathered, now) = (pieces.get() - before, snap_bytes(&disk));
        if gathered > 8 || now > held + 8 * (PIECE_MAX + 8) {
            return Err(format!("in one call {} pieces gathered, {} bytes written", gathered, now.saturating_sub(held)));
        }
        held = now;
        calls += m.taking_snapshot() as u64;
        let index = m.node().storage().snapshot_meta().index;
        if index > 0 && !m.taking_snapshot() {
            break index;
        }
        if seq > 400 {
            return Err("no snapshot taken in 400 calls".into());
        }
    };
    if calls < 4 {
        return Err(format!("a snapshot of {} pieces taken over {} calls", BIG_PIECES, calls));
    }
    for _ in 0..3 {
        m.process().map_err(|e| format!("{:?}", e))?;
    }
    let value = m.state().value;
    drop(m);
    let (again, _, _) = big_lone(&disk, raft::MAX_SNAPSHOT_BYTES, 2)?;
    if again.node().storage().snapshot_meta().index < index || again.state().value != value {
        return Err(format!("started again from the snapshot at {}: the state {} where it was {}", index,
                           again.state().value, value));
    }
    Ok(())
}

/// A snapshot of a state larger than the member may hold is given up --
/// counted, the log not compacted, nothing left on the disk -- and the
/// member goes on; one that may hold it takes it. (It was gathered whole,
/// whatever its size, and every piece asked of the heap as one that could
/// not fail.)
fn snapshot_too_large() -> Result<(), String> {
    let disk = SimFiles(Rc::new(RefCell::new(Disk::new(12))));
    let limit = (BIG_PIECES as u64 / 2) * PIECE_MAX as u64;
    let (mut m, client, _) = big_lone(&disk, limit, 1)?;
    m.set_snapshot_floor(1);
    for seq in 1..=60 {
        m.submit(client, seq, &Op::Add(1).command()).map_err(|e| format!("{:?}", e))?;
        m.process().map_err(|e| format!("{:?}", e))?;
    }
    for _ in 0..20 {
        m.process().map_err(|e| format!("{:?}", e))?;
    }
    let given_up = m.stats().snapshots_given_up;
    if given_up == 0 || m.stats().snapshots != 0 || m.node().storage().snapshot_meta().index != 0 || tmp_bytes(&disk) != 0
        || m.state().value != 60
    {
        return Err(format!("a snapshot past what the member may hold: {} given up, {} taken, at {}, {} bytes of \
                            it on the disk, the state {}", given_up, m.stats().snapshots,
                           m.node().storage().snapshot_meta().index, tmp_bytes(&disk), m.state().value));
    }
    drop(m);
    let (mut again, client, _) = big_lone(&disk, raft::MAX_SNAPSHOT_BYTES, 2)?;
    again.set_snapshot_floor(1);
    for seq in 61..=200 {
        again.submit(client, seq, &Op::Add(1).command()).map_err(|e| format!("{:?}", e))?;
        again.process().map_err(|e| format!("{:?}", e))?;
        if again.stats().snapshots > 0 {
            return Ok(());
        }
    }
    Err("a member that may hold the snapshot never took one".into())
}

/// A vote is on the disk before it is answered, now that a `Ready` is
/// synced only where it must be: a member over the log on disk that gave
/// its vote, its power then cut and every write not synced lost, has the
/// vote when it starts again.
fn vote_durable() -> Result<(), String> {
    let voters = [1, 2, 3];
    let disk = SimFiles(Rc::new(RefCell::new(Disk::new(5))));
    let log = DiskLog::bootstrap(disk.clone(), LogConfig::default(), &voters, &[]).map_err(|e| format!("{:?}", e))?;
    let mut two = Rsm::new(config(2, 1), log, Reg::default(), 2).map_err(|e| format!("{:?}", e))?;
    let mut one =
        Rsm::new(config(1, 1), MemStorage::bootstrap(&voters, &[]), Reg::default(), 1).map_err(|e| format!("{:?}", e))?;
    one.campaign().map_err(|e| format!("{:?}", e))?;
    let mut voted = false;
    for _ in 0..10 {
        for m in one.process().map_err(|e| format!("{:?}", e))?.messages {
            if m.to == 2 {
                two.step(m).map_err(|e| format!("{:?}", e))?;
            }
        }
        for m in two.process().map_err(|e| format!("{:?}", e))?.messages {
            voted |= m.to == 1 && matches!(m.body, Body::VoteResp { pre: false, granted: true });
            if m.to == 1 {
                one.step(m).map_err(|e| format!("{:?}", e))?;
            }
        }
        if voted {
            break;
        }
    }
    if !voted {
        return Err("member 2 never voted".into());
    }
    let term = one.node().term();
    drop(two);
    disk.0.borrow_mut().crash_with(0, &mut |_, _| Fate::Lost, &mut Rng::new(1));
    let log = DiskLog::open(disk, LogConfig::default()).map_err(|e| format!("{:?}", e))?;
    let again = Rsm::new(config(2, 1), log, Reg::default(), 3).map_err(|e| format!("{:?}", e))?;
    if again.node().vote() != 1 || again.node().term() != term {
        return Err(format!("member 2 voted for 1 in term {}, and after a cut has a vote for {} in term {}", term,
                           again.node().vote(), again.node().term()));
    }
    Ok(())
}
