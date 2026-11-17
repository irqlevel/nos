//! A migration's handover (plans/vm-migration.md, "The handover"):
//! `migwire::handover`'s two sides, S and D, from S's `Done` on, over a
//! model of NBS's writer of the VM's first volume -- a compare and set by
//! the generation expected, each side's take asked again by its token and
//! answered as it was -- and of the way between them, where `Done`, D's
//! `Running` and `Aborted` are lost, come late and come twice, NBS's
//! answers are lost, and either host dies at any step.
//!
//! - **Never two**: at no step do both run the VM; D runs it only holding
//!   the volume, S only holding it -- its own generation still, or taken
//!   back.
//! - **One, once all is said**: with every message and answer let through
//!   after the input, each wait run out, a side alive has decided; both
//!   alive, exactly one runs the VM -- unless D died after its commit,
//!   when neither does, the volume D's.

use migwire::handover::{Dest, DestDo, Source, SourceDo, Take};

use crate::targets::Input;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    S,
    D,
}

/// NBS's writer of the first volume: whose session holds it, at what
/// generation, and the token of that session's take.
#[derive(Clone, Copy, Debug)]
struct Writer {
    side: Side,
    generation: u64,
    token: u64,
}

/// A take asked of NBS and not answered yet: applied or not, and its
/// answer then on its way back.
#[derive(Clone, Copy, Debug)]
struct Asked {
    token: u64,
    applied: Option<Take>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    Done,
    Running,
    Aborted,
}

struct World {
    writer: Writer,
    next_generation: u64,
    next_token: u64,
    /// The generation `Done` names: S's, as it paused.
    g0: u64,
    s: Source,
    s_alive: bool,
    s_runs: bool,
    s_ask: Option<Asked>,
    d: Dest,
    d_alive: bool,
    d_runs: bool,
    d_ask: Option<Asked>,
    /// D has `Done`, and the state: what its take is from.
    d_has_done: bool,
    /// On the way: to D (`Done`) and to S (its word).
    to_d: Vec<Wire>,
    to_s: Vec<Wire>,
}

impl World {
    fn new() -> World {
        World {
            writer: Writer { side: Side::S, generation: 7, token: 1 },
            next_generation: 8,
            next_token: 2,
            g0: 7,
            s: Source::new(),
            s_alive: true,
            s_runs: false,
            s_ask: None,
            d: Dest::new(),
            d_alive: true,
            d_runs: false,
            d_ask: None,
            d_has_done: false,
            to_d: vec![Wire::Done],
            to_s: Vec::new(),
        }
    }

    /// The compare and set: a take by `side`'s session `token`, from the
    /// writer of `expect` -- asked again by its token, answered as it was.
    fn take(&mut self, side: Side, token: u64, expect: u64) -> Take {
        if self.writer.side == side && self.writer.token == token {
            return Take::Took;
        }
        if self.writer.generation != expect {
            return Take::Refused;
        }
        self.writer = Writer { side, generation: self.next_generation, token };
        self.next_generation += 1;
        Take::Took
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    fn act_s(&mut self, d: SourceDo) {
        match d {
            SourceDo::Wait => {}
            SourceDo::TakeBack => {
                /* Asked again by the same session, with the same token. */
                let token = match self.s_ask {
                    Some(a) => a.token,
                    None => self.token(),
                };
                self.s_ask = Some(Asked { token, applied: None });
            }
            SourceDo::End => {
                self.s_runs = false;
                self.s_ask = None;
            }
            SourceDo::Resume { .. } => {
                self.s_runs = true;
                self.s_ask = None;
            }
        }
    }

    fn act_d(&mut self, d: DestDo) {
        match d {
            DestDo::Wait => {}
            DestDo::TakeFirst => {
                let token = match self.d_ask {
                    Some(a) => a.token,
                    None => self.token(),
                };
                self.d_ask = Some(Asked { token, applied: None });
            }
            DestDo::TakeRest => {
                self.d_ask = None;
                self.d_runs = true;
                self.to_s.push(Wire::Running);
            }
            DestDo::Abort => {
                self.d_ask = None;
                self.to_s.push(Wire::Aborted);
            }
        }
    }

    /// One step, `what` of the steps there are: false where it had nothing
    /// to do.
    fn step(&mut self, what: u64, r: &mut Input, lossy: bool) -> bool {
        match what {
            /* A message to D, taken -- or lost, or kept to come again. */
            0 if !self.to_d.is_empty() => {
                let i = r.below(self.to_d.len() as u64) as usize;
                let m = if lossy && r.below(3) == 0 { self.to_d[i] } else { self.to_d.remove(i) };
                if lossy && r.below(4) == 0 {
                    return true;
                }
                if self.d_alive && m == Wire::Done {
                    self.d_has_done = true;
                    let d = self.d.done();
                    self.act_d(d);
                }
                true
            }
            1 if !self.to_s.is_empty() => {
                let i = r.below(self.to_s.len() as u64) as usize;
                let m = if lossy && r.below(3) == 0 { self.to_s[i] } else { self.to_s.remove(i) };
                if lossy && r.below(4) == 0 {
                    return true;
                }
                if self.s_alive {
                    let d = match m {
                        Wire::Running => self.s.running(),
                        Wire::Aborted => self.s.aborted(),
                        Wire::Done => SourceDo::Wait,
                    };
                    self.act_s(d);
                }
                true
            }
            /* S's wait for D's word run out. */
            2 if self.s_alive && self.s == Source::Waiting => {
                let d = self.s.timed_out();
                self.act_s(d);
                true
            }
            /* S's take applied by NBS, its answer on its way. */
            3 if self.s_alive && self.s_ask.is_some_and(|a| a.applied.is_none()) => {
                let token = self.s_ask.map(|a| a.token).unwrap_or(0);
                let t = self.take(Side::S, token, self.g0);
                if let Some(a) = self.s_ask.as_mut() {
                    a.applied = Some(t);
                }
                true
            }
            /* Its answer heard -- or lost, and so nothing to go by. */
            4 if self.s_alive && self.s_ask.is_some() => {
                let Some(a) = self.s_ask else { return false };
                let heard = match a.applied {
                    Some(t) if !(lossy && r.below(3) == 0) => t,
                    _ => Take::Unknown,
                };
                if heard == Take::Unknown {
                    self.s_ask = Some(Asked { token: a.token, applied: None });
                }
                let d = self.s.took_back(heard);
                self.act_s(d);
                true
            }
            5 if self.d_alive && self.d_ask.is_some_and(|a| a.applied.is_none()) => {
                let token = self.d_ask.map(|a| a.token).unwrap_or(0);
                let t = self.take(Side::D, token, self.g0);
                if let Some(a) = self.d_ask.as_mut() {
                    a.applied = Some(t);
                }
                true
            }
            6 if self.d_alive && self.d_ask.is_some() => {
                let Some(a) = self.d_ask else { return false };
                let heard = match a.applied {
                    Some(t) if !(lossy && r.below(3) == 0) => t,
                    _ => Take::Unknown,
                };
                if heard == Take::Unknown {
                    self.d_ask = Some(Asked { token: a.token, applied: None });
                }
                let d = self.d.took_first(heard);
                self.act_d(d);
                true
            }
            /* D's wait for `Done` run out, or its state refused. */
            7 if self.d_alive && self.d == Dest::Ready => {
                let d = self.d.give_up();
                self.act_d(d);
                true
            }
            /* A host dies: whatever it ran stops, its asks with it. */
            8 if lossy && self.s_alive && r.below(8) == 0 => {
                self.s_alive = false;
                self.s_runs = false;
                self.s_ask = None;
                true
            }
            9 if lossy && self.d_alive && r.below(8) == 0 => {
                self.d_alive = false;
                self.d_runs = false;
                self.d_ask = None;
                true
            }
            _ => false,
        }
    }

    fn check(&self) {
        invariant!(!(self.s_runs && self.d_runs), "both hosts run the VM: {:?} {:?}", self.s, self.d);
        if self.d_runs {
            invariant!(self.writer.side == Side::D, "D runs the VM without its volume: {:?}", self.writer);
        }
        if self.s_runs {
            invariant!(self.writer.side == Side::S, "S runs the VM without its volume: {:?}", self.writer);
        }
    }
}

pub fn handover_target(r: &mut Input) {
    let mut w = World::new();
    while let Some(op) = r.op(10) {
        w.step(u64::from(op), r, true);
        w.check();
    }
    /* Everything let through from here, every wait run out. */
    for _ in 0..1000 {
        let mut moved = false;
        for what in 0..8 {
            moved |= w.step(what, r, false);
            w.check();
        }
        if !moved {
            break;
        }
    }
    if w.s_alive {
        invariant!(matches!(w.s, Source::Ended | Source::Resumed), "S never decided: {:?}", w.s);
    }
    if w.d_alive {
        invariant!(matches!(w.d, Dest::Committed | Dest::Aborted), "D never decided: {:?}", w.d);
    }
    if w.s_alive && w.d_alive {
        invariant!(w.s_runs != w.d_runs, "both alive, and {} runs the VM: {:?} {:?}",
                   if w.s_runs { "each" } else { "neither" }, w.s, w.d);
    }
    if w.s_alive && !w.s_runs {
        invariant!(w.writer.side == Side::D, "S gave the VM up and D does not hold its volume");
    }
}
