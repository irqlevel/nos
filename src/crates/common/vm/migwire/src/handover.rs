//! The handover's decision: one compare and set, in NBS's raft -- the
//! writer of the VM's first volume.
//!
//! Once S has paused the VM, drained it and sent all of it with `Done`,
//! one of the two hosts runs it from there, and never both:
//!
//! - **D commits** by taking the first volume for writing from the writer
//!   of the generation `Done` named -- S's (`nbsmeta`'s `Attach` with
//!   `expect`) -- and only then the others, the ports, and the run.
//! - **S gives up**, past its wait for D's word, by taking the first volume
//!   anew from that same generation, itself: the same compare and set.
//!
//! Whichever lands first decides, and the other is refused: D's take where
//! S took it anew, S's where D took it. A side that hears nothing it can go
//! by -- no leader, its asks unanswered -- asks again, and decides nothing
//! on a guess. What is here is each side's decision on what it hears, with
//! no I/O: hv's `migrate.rs` acts on it, and tests/fuzz/hv runs both sides
//! against a model of NBS's writer and of the way between them, messages
//! lost, late and twice, either host gone at any step.

/// What NBS answered a side's take of the first volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    /// Its writer is this side's now.
    Took,
    /// Its writer was another: the other side took it first.
    Refused,
    /// No answer to go by: asked again.
    Unknown,
}

/// What the old host does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceDo {
    /// Nothing yet: wait for D's word, or the wait's end.
    Wait,
    /// Take the first volume anew from its own generation: D has been
    /// silent past the wait.
    TakeBack,
    /// D runs it: the VM ends here, its memory given back.
    End,
    /// It goes on here -- D gave it up, or this side took the first volume
    /// back, whose new session the guest's first disk is to be given
    /// (`swap_first`).
    Resume { swap_first: bool },
}

/// The old host's side, from `Done` sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `Done` sent; D's word awaited.
    Waiting,
    /// Taking the first volume back.
    TakingBack,
    /// Decided: ended, or resumed.
    Ended,
    Resumed,
}

impl Source {
    pub fn new() -> Source {
        Source::Waiting
    }

    /// D said it runs the VM.
    pub fn running(&mut self) -> SourceDo {
        match *self {
            /* Its word before or while this side takes back: D took the
               volume first, and this side's take, if it is still asked,
               will be refused. */
            Source::Waiting | Source::TakingBack => {
                *self = Source::Ended;
                SourceDo::End
            }
            /* Late, after this side took the volume back: D's take was
               refused, and no D runs it -- a word that cannot be is taken
               for nothing. */
            Source::Resumed | Source::Ended => SourceDo::Wait,
        }
    }

    /// D gave the VM up before its commit: it never took the first volume.
    pub fn aborted(&mut self) -> SourceDo {
        match *self {
            Source::Waiting => {
                *self = Source::Resumed;
                SourceDo::Resume { swap_first: false }
            }
            /* Taking back already: what that take says decides -- D gave up,
               so it will be this side's. */
            Source::TakingBack | Source::Resumed | Source::Ended => SourceDo::Wait,
        }
    }

    /// The wait for D's word is over.
    pub fn timed_out(&mut self) -> SourceDo {
        match *self {
            Source::Waiting => {
                *self = Source::TakingBack;
                SourceDo::TakeBack
            }
            _ => SourceDo::Wait,
        }
    }

    /// What its take of the first volume came to.
    pub fn took_back(&mut self, t: Take) -> SourceDo {
        if *self != Source::TakingBack {
            return SourceDo::Wait;
        }
        match t {
            Take::Took => {
                *self = Source::Resumed;
                SourceDo::Resume { swap_first: true }
            }
            Take::Refused => {
                *self = Source::Ended;
                SourceDo::End
            }
            Take::Unknown => SourceDo::TakeBack,
        }
    }

    /// Whether it runs the VM -- resumed -- or may: not ended yet.
    pub fn may_run(&self) -> bool {
        !matches!(self, Source::Ended)
    }
}

impl Default for Source {
    fn default() -> Source {
        Source::new()
    }
}

/// What the new host does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestDo {
    /// Nothing yet.
    Wait,
    /// Take the first volume from the generation `Done` named.
    TakeFirst,
    /// Take the rest, the ports, and run it; then tell S.
    TakeRest,
    /// Give it up -- S has it -- and tell S.
    Abort,
}

/// The new host's side, from the state applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest {
    /// The state in; `Done` awaited.
    Ready,
    /// Taking the first volume.
    Taking,
    /// It took it: the VM is this side's from here.
    Committed,
    /// It gave up: the VM is S's.
    Aborted,
}

impl Dest {
    pub fn new() -> Dest {
        Dest::Ready
    }

    /// `Done` came.
    pub fn done(&mut self) -> DestDo {
        match *self {
            Dest::Ready => {
                *self = Dest::Taking;
                DestDo::TakeFirst
            }
            _ => DestDo::Wait,
        }
    }

    /// What its take of the first volume came to.
    pub fn took_first(&mut self, t: Take) -> DestDo {
        if *self != Dest::Taking {
            return DestDo::Wait;
        }
        match t {
            Take::Took => {
                *self = Dest::Committed;
                DestDo::TakeRest
            }
            Take::Refused => {
                *self = Dest::Aborted;
                DestDo::Abort
            }
            Take::Unknown => DestDo::TakeFirst,
        }
    }

    /// Something wrong before the commit -- a state it will not run, S's
    /// cancel, the wait for `Done` over: given up, S still the VM's.
    pub fn give_up(&mut self) -> DestDo {
        match *self {
            Dest::Ready => {
                *self = Dest::Aborted;
                DestDo::Abort
            }
            /* Taking the first volume: what the take says decides. Past the
               commit: the VM is this side's, whatever else comes. */
            _ => DestDo::Wait,
        }
    }

    /// Whether the VM is this side's to run.
    pub fn committed(&self) -> bool {
        *self == Dest::Committed
    }
}

impl Default for Dest {
    fn default() -> Dest {
        Dest::new()
    }
}
