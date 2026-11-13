//! A member's own commands asked of its group -- a shell's, on whichever
//! member it runs -- each applied once, whoever leads: under a session of
//! its own, registered for it alone, and asked again under that session and
//! the same number, of whichever member leads by then, until what became of
//! it is known (the dissertation's §6.3: a client's retry is the same
//! number of the same session). A command whose first try is in a log as its
//! leader steps down is answered from the session when it is asked again,
//! never applied twice; an ask that is not heard of in time is told so, not
//! told it failed.
//!
//! What is to be sent is the caller's to send: done by the leader itself
//! ([`Rsm::register_for`](crate::Rsm::register_for), `submit`, `unregister`),
//! handed to the leader by any other member. What came of it is learnt as
//! the asking member's own rsm applies the log -- `Registered`, `Applied`,
//! `Expired`, `Lost` -- so a member hears of its command whichever leader
//! took it; and the leader that applies a registration or a command it
//! took for another member tells that member too ([`Tell`]) -- at once,
//! where the member may be far behind in the log, or never apply the entry
//! at all: a command that takes it out of the group. Whatever is not heard
//! of in time is sent again, and answered from the session where it was
//! applied already.
//!
//! As the rest of the crate: no clock and no network. The caller hands it
//! the time, the events of its rsm and the leader's changes, and sends what
//! it is told to.
//!
//! One gap, said rather than closed: a session evicted (past
//! `rsm::MAX_SESSIONS` newer ones) after its command was applied, where this
//! member did not see the application -- a snapshot taken in over it --
//! makes the next try `Expired`, and the command is asked again under a new
//! session. That takes `MAX_SESSIONS` registrations within one ask's life.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::rsm::Event;
use crate::types::NodeId;

/// The number an ask's command carries in its session: its one command.
pub const SEQ: u64 = 1;
/// Asks in flight at once, at most: past it, one is refused at once.
pub const MAX_ASKS: usize = 64;
/// Registrations and commands a leader owes word of at once, at most:
/// past it, one is let go, its asker left to its own log and its tries.
pub const MAX_OWED: usize = 1024;

/// How long things take, in the caller's unit of time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// An ask's whole life: past it, it is told what is known.
    pub total: u64,
    /// A registration or a command not heard of is sent again after this
    /// long -- and at once when the leader changes.
    pub resend: u64,
    /// A command the leader refused is sent again after this long.
    pub retry: u64,
}

/// What to send to the leader -- or, on the leader, to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Send {
    /// A session for an ask: `Rsm::register_for(the asking member, request)`.
    Register { request: u64 },
    /// The ask's command under its session: `Rsm::submit(client, SEQ,
    /// command)`, where the leader takes it.
    Command { client: u64, command: Vec<u8> },
    /// A session done with: `Rsm::unregister(client)`.
    Unregister { client: u64 },
}

/// What became of an ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Applied, once, at `index`: what applying it answered.
    Applied { index: u64, answer: Vec<u8> },
    /// Applied, once, and its answer gone: too long for its session to
    /// keep, and its application not seen here -- a snapshot taken in over
    /// it.
    Lost,
    /// Its time up, its command never sent: not applied, and never to be.
    NotAsked,
    /// Its time up, its command sent and not heard of: it may yet be
    /// applied, once. What the leader said where it last refused it.
    Unknown { refused: Option<String> },
}

/// An ask that ended: its waiter, and what became of it.
#[derive(Debug)]
pub struct Done<W> {
    pub waiter: W,
    pub outcome: Outcome,
}

/// What the leader that applied an ask's registration or command tells the
/// member that asked it: what that member's own log says once it applies
/// the entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Told {
    /// A session made for the member's request `request`.
    Registered { request: u64, client: u64 },
    /// The command of session `client` applied at `index`: its answer.
    Applied { client: u64, index: u64, answer: Vec<u8> },
    /// The command of session `client` applied, its answer too long to keep.
    Lost { client: u64 },
}

/// The leader's half: whom it owes word of what it did for another
/// member's ask -- that member, by its address `T`, told as the leader's
/// own rsm applies the entry.
pub struct Tell<T> {
    /// Commands submitted for another member: by session.
    owed: BTreeMap<u64, T>,
    /// Sessions asked for by another member: by its node and request.
    registering: BTreeMap<(NodeId, u64), T>,
}

impl<T> Default for Tell<T> {
    fn default() -> Self {
        Tell { owed: BTreeMap::new(), registering: BTreeMap::new() }
    }
}

/// `map` given `key` -- past `MAX_OWED`, its first let go, its asker left
/// to its own log and its tries.
fn owe<K: Ord + Copy, T>(map: &mut BTreeMap<K, T>, key: K, to: T) {
    if map.len() >= MAX_OWED && !map.contains_key(&key) {
        if let Some(&first) = map.keys().next() {
            map.remove(&first);
        }
    }
    map.insert(key, to);
}

impl<T> Tell<T> {
    pub fn new() -> Tell<T> {
        Tell::default()
    }

    /// A session asked for by member `origin` as its `request`, proposed:
    /// the member at `to` told as it is made.
    pub fn registering(&mut self, origin: NodeId, request: u64, to: T) {
        owe(&mut self.registering, (origin, request), to);
    }

    /// The command of session `client` submitted for the member at `to`.
    pub fn submitted(&mut self, client: u64, to: T) {
        owe(&mut self.owed, client, to);
    }

    /// An event of the leader's rsm: whom to tell what, where it owes word.
    pub fn on_event(&mut self, e: &Event) -> Option<(T, Told)> {
        match *e {
            Event::Registered { origin, request, client } => {
                self.registering.remove(&(origin, request)).map(|to| (to, Told::Registered { request, client }))
            }
            Event::Applied { client, seq: SEQ, index, ref answer } => {
                self.owed.remove(&client).map(|to| (to, Told::Applied { client, index, answer: answer.clone() }))
            }
            Event::Lost { client, seq: SEQ } => self.owed.remove(&client).map(|to| (to, Told::Lost { client })),
            /* Not applied under it: nothing to tell -- the asker's own log
               says so, or its time does */
            Event::Expired { client, seq: SEQ } => {
                self.owed.remove(&client);
                None
            }
            _ => None,
        }
    }

    /// No longer the leader: what it owed left to the askers' own logs and
    /// tries, which reach the next.
    pub fn clear(&mut self) {
        self.owed.clear();
        self.registering.clear();
    }
}

struct Ask<W> {
    waiter: W,
    command: Vec<u8>,
    /// The request its session is asked under: a new one only once a
    /// session made for it is gone.
    request: u64,
    /// Its session, once made.
    client: Option<u64>,
    /// When it is next sent: 0 for at once.
    due: u64,
    /// When its time is up.
    until: u64,
    /// Whether its command was ever sent: one never sent cannot be applied.
    sent: bool,
    /// What the leader said when it last refused the command.
    refused: Option<String>,
}

/// A member's asks in flight.
pub struct Asks<W> {
    me: NodeId,
    timing: Timing,
    next_request: u64,
    asks: Vec<Ask<W>>,
    /// Every request number handed out, and when: a session made for one
    /// that no ask waits on any more -- an ask done, a registration sent
    /// twice -- is ended, and one made for another's is not touched.
    issued: BTreeMap<u64, u64>,
    /// Sessions to end, at the next send.
    ending: Vec<u64>,
    /// The time as last handed in.
    now: u64,
}

impl<W> Asks<W> {
    /// The asks of member `me`, their request numbers from `first` on --
    /// a start's own, so that a registration of an earlier start, applied
    /// again as the log is read back, is never taken for one of this one's.
    pub fn new(me: NodeId, first: u64, timing: Timing) -> Asks<W> {
        Asks { me, timing, next_request: first.max(1), asks: Vec::new(), issued: BTreeMap::new(), ending: Vec::new(),
               now: 0 }
    }

    /// Asks in flight.
    pub fn len(&self) -> usize {
        self.asks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.asks.is_empty()
    }

    /// Every ask in flight given up, its waiter handed back: the member
    /// stops, and each is told so rather than left to wait out its time.
    /// The sessions made for them are not ended -- the member that runs
    /// over the log next does not know them, and one left is evicted in
    /// time as any other is.
    pub fn drain(&mut self) -> Vec<W> {
        self.ending.clear();
        self.asks.drain(..).map(|a| a.waiter).collect()
    }

    fn take_request(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.issued.insert(request, self.now);
        request
    }

    /// `command` asked, `waiter` to be told what became of it -- sent at
    /// the next `poll` that knows a leader. The waiter handed back where
    /// [`MAX_ASKS`] are in flight already.
    pub fn ask(&mut self, command: Vec<u8>, waiter: W, now: u64) -> Result<(), W> {
        if self.asks.len() >= MAX_ASKS {
            return Err(waiter);
        }
        self.now = self.now.max(now);
        let request = self.take_request();
        self.asks.push(Ask { waiter, command, request, client: None, due: 0, until: now + self.timing.total,
                             sent: false, refused: None });
        Ok(())
    }

    /// An event of this member's rsm, as it applies the log: an ask that
    /// ended put in `done`.
    pub fn on_event(&mut self, e: &Event, done: &mut Vec<Done<W>>) {
        match *e {
            Event::Registered { origin, request, client } if origin == self.me => {
                if let Some(a) = self.asks.iter_mut().find(|a| a.request == request) {
                    match a.client {
                        None => {
                            a.client = Some(client);
                            a.due = 0;
                        }
                        /* Its registration sent twice and made twice: the
                           second session ended */
                        Some(c) if c != client => self.ending.push(client),
                        Some(_) => {}
                    }
                } else if self.issued.contains_key(&request) {
                    self.ending.push(client);
                }
            }
            Event::Applied { client, seq: SEQ, index, ref answer } => {
                if let Some(at) = self.asks.iter().position(|a| a.client == Some(client)) {
                    let a = self.asks.remove(at);
                    self.ending.push(client);
                    done.push(Done { waiter: a.waiter, outcome: Outcome::Applied { index, answer: answer.clone() } });
                }
            }
            Event::Lost { client, seq: SEQ } => {
                if let Some(at) = self.asks.iter().position(|a| a.client == Some(client)) {
                    let a = self.asks.remove(at);
                    self.ending.push(client);
                    done.push(Done { waiter: a.waiter, outcome: Outcome::Lost });
                }
            }
            Event::Expired { client, seq: SEQ } => {
                /* Its session gone before its command was applied under it:
                   not applied by this try, and asked again under a new one */
                if let Some(at) = self.asks.iter().position(|a| a.client == Some(client)) {
                    let request = self.take_request();
                    let a = &mut self.asks[at];
                    a.client = None;
                    a.request = request;
                    a.due = 0;
                }
            }
            _ => {}
        }
    }

    /// What the leader that applied an entry of this member's asks told
    /// it: as its own log will say it.
    pub fn told(&mut self, told: Told, done: &mut Vec<Done<W>>) {
        let e = match told {
            Told::Registered { request, client } => Event::Registered { origin: self.me, request, client },
            Told::Applied { client, index, answer } => Event::Applied { client, seq: SEQ, index, answer },
            Told::Lost { client } => Event::Lost { client, seq: SEQ },
        };
        self.on_event(&e, done);
    }

    /// The leader refused the command of session `client` -- `why` in its
    /// words: sent again after the retry time, the refusal kept for what
    /// the ask is told if its time runs out first. A refusal is never final
    /// here: an earlier try may be in a log already, and the next is
    /// answered from the session where it was applied.
    pub fn refused(&mut self, client: u64, why: String, now: u64) {
        self.now = self.now.max(now);
        if let Some(a) = self.asks.iter_mut().find(|a| a.client == Some(client)) {
            a.refused = Some(why);
            a.due = now + self.timing.retry;
        }
    }

    /// Another leader: everything not heard of sent again to it at once.
    pub fn leader_changed(&mut self) {
        for a in self.asks.iter_mut() {
            a.due = 0;
        }
    }

    /// The time handed in: what is due put in `out` -- where a leader is
    /// known to send it to -- and every ask whose time is up in `done`.
    pub fn poll(&mut self, now: u64, leader_known: bool, out: &mut Vec<Send>, done: &mut Vec<Done<W>>) {
        self.now = self.now.max(now);
        let mut k = 0;
        while k < self.asks.len() {
            if now >= self.asks[k].until {
                let a = self.asks.remove(k);
                if let Some(c) = a.client {
                    /* Ended: a try still on its way is then never applied,
                       where the end is applied before it */
                    self.ending.push(c);
                }
                let outcome = if a.sent { Outcome::Unknown { refused: a.refused } } else { Outcome::NotAsked };
                done.push(Done { waiter: a.waiter, outcome });
                continue;
            }
            let a = &mut self.asks[k];
            if leader_known && now >= a.due {
                match a.client {
                    None => out.push(Send::Register { request: a.request }),
                    Some(client) => {
                        out.push(Send::Command { client, command: a.command.clone() });
                        a.sent = true;
                    }
                }
                a.due = now + self.timing.resend;
            }
            k += 1;
        }
        if leader_known {
            out.extend(self.ending.drain(..).map(|client| Send::Unregister { client }));
        }
        /* A request number is kept twice an ask's life: past that, no
           registration of it is on its way any more */
        let horizon = now.saturating_sub(self.timing.total.saturating_mul(2));
        self.issued.retain(|_, at| *at >= horizon);
    }
}
