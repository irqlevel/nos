//! The file logs' limiter (`kcore::logs::Limiter`, which `flog_limited!`
//! holds a place's lines to), held to what docs/logs.md says of it by a
//! model of its own -- nothing of the limiter's shared:
//!
//! - a place's window begins at its first line after the last one ended,
//!   and lets `LIMIT_PLACE` of its lines through; once they have gone,
//!   every line until the window ends is held;
//! - lines alike -- the same but for their digits -- have windows of their
//!   own, begun the same way, each letting `LIMIT_ALIKE` through;
//! - every line held is counted once, and said by the next line that goes:
//!   `[held back: N] ` before it, its room `LOG_LINE_MAX` bytes of whole
//!   characters, the rest cut and said so.
//!
//! An input with no more kinds of line than the limiter tells apart
//! (`LIMIT_SHAPES`) is held to the model line by line. One with more -- a
//! kind's count then starts again when it takes another's slot -- is held
//! to what stands whatever the slots do: the place's windows and their
//! bound, and the count of what was held.

use kcore::logs::{held_back, LineBuf, Limiter, LIMIT_ALIKE, LIMIT_PLACE, LIMIT_SHAPES, LIMIT_WINDOW_MS};

use crate::Input;

const WINDOW: u64 = LIMIT_WINDOW_MS * 1_000_000;
const LINE_MAX: usize = ffi::logs::LOG_LINE_MAX;
/// Lines an input offers, at most.
const OFFERED_MAX: usize = 4000;

/// The kinds of line an input says, by what is left of them without their
/// digits: some long enough to be cut, some with characters of more than a
/// byte where a cut may fall.
const KINDS: [&str; 13] = [
    "nbs: cs # of set # taken out of volume web's chunks there (version #)",
    "nbs: cs # of set # taken out of volume db's chunks there (version #)",
    "nbs: volume web: an operation not done in # s -- failed",
    "hv: vm # not started -- out of memory",
    "tunnel: host-b answered no echo for # s",
    "cs: set #'s volumes not fenced here: its join waits",
    "client: volume тест: a write not done in # s -- его реплики, может быть, разошлись ######",
    "scrub: chunk # of set # (version #): its # active members are not alike; `nbs replica bad` it",
    "migrate: vm # not moved to host-c (live) -- refused: ##",
    "a line long enough to be cut once the count is put before it: ###############################################\
     ############################################################################################################ é",
    "a line cut whatever is put before it -- more than a line holds on its own: ######################################\
     ################################################################################################################\
     ######## жжжж",
    "",
    "#",
];

/// A line of kind `kind`, its digits made of `seed`.
fn line_of(kind: &str, seed: u64) -> String {
    let mut out = String::new();
    let mut s = seed;
    for c in kind.chars() {
        if c == '#' {
            out.push(char::from(b'0' + (s % 10) as u8));
            s = s / 10 + 7;
        } else {
            out.push(c);
        }
    }
    out
}

/// What makes lines alike, as docs/logs.md says it: everything but digits.
fn shape(line: &str) -> String {
    line.chars().filter(|c| !c.is_ascii_digit()).collect()
}

/// A window: when it began, and how many went in it.
#[derive(Clone, Copy)]
struct Window {
    start: u64,
    went: u32,
}

impl Window {
    /// The window at `t`: this one, or one begun at `t` where it has ended.
    fn at(w: Option<Window>, t: u64) -> Window {
        match w {
            Some(w) if t - w.start < WINDOW => w,
            _ => Window { start: t, went: 0 },
        }
    }
}

/// The model: the place's window, each kind's, and what was held since the
/// last line that went.
#[derive(Default)]
struct Model {
    place: Option<Window>,
    kinds: Vec<(String, Window)>,
    held: u64,
}

impl Model {
    /// A line of `shape` at `t`: `Some` with what it says was held before
    /// it, or `None` -- held. `looked`: it was looked at as the place's
    /// lines all gone for this window are -- held at once, its kind's
    /// window untouched; one that came past that look as the place's last
    /// line went, on another CPU, is not.
    fn offer(&mut self, shape: &str, t: u64, looked: bool) -> Option<u64> {
        if let (true, Some(p)) = (looked, self.place) {
            if p.went >= LIMIT_PLACE && t - p.start < WINDOW {
                self.held += 1;
                return None;
            }
        }
        let mut place = Window::at(self.place, t);
        self.place = Some(place);
        let at = match self.kinds.iter().position(|(s, _)| s == shape) {
            Some(at) => at,
            None => {
                self.kinds.push((String::from(shape), Window { start: t, went: 0 }));
                self.kinds.len() - 1
            }
        };
        let mut kind = Window::at(Some(self.kinds[at].1), t);
        if kind.went >= LIMIT_ALIKE || place.went >= LIMIT_PLACE {
            self.kinds[at].1 = kind;
            self.held += 1;
            return None;
        }
        kind.went += 1;
        place.went += 1;
        self.kinds[at].1 = kind;
        self.place = Some(place);
        Some(core::mem::take(&mut self.held))
    }
}

/// When the next line comes, after one at `t`: at once, a little after,
/// half a window on, on either side of a window's end -- the place's,
/// which began at `place`, or a kind's -- or far on. In a `flood`, mostly
/// within a millisecond: a window's place fills, and closes.
fn next(r: &mut Input, t: u64, place: Option<u64>, m: &Model, flood: bool) -> u64 {
    /* A window of a flood holds some fifty lines: past the place's own */
    if flood && r.below(32) != 0 {
        return t + r.below(1_000_000);
    }
    let edge = |start: u64, r: &mut Input| (start + WINDOW - 1 + r.below(3)).max(t);
    match r.below(8) {
        0 => t,
        1 => t + r.below(1_000_000_000),
        2 => t + WINDOW / 2 + r.below(1_000_000),
        3 => match place {
            Some(start) => edge(start, r),
            None => t + WINDOW,
        },
        4 => match m.kinds.get(r.below(LIMIT_SHAPES as u64) as usize) {
            Some((_, w)) => edge(w.start, r),
            None => t + 1,
        },
        5 => t + WINDOW - 1 - r.below(2),
        6 => t + WINDOW + r.below(2),
        _ => t + r.below(4 * WINDOW),
    }
}

/// `held_back(held, line)` as the line it writes: the count before the line,
/// in the room of one, whole characters, a cut said.
fn check_said(held: u64, line: &str) {
    let mut buf = LineBuf::new();
    let _ = core::fmt::Write::write_str(&mut buf, line);
    let cut_before = buf.cut();
    let out = held_back(held, buf);
    let bytes = out.bytes();
    invariant!(bytes.len() <= LINE_MAX, "a line of {} bytes, past the {} a line holds", bytes.len(), LINE_MAX);
    invariant!(core::str::from_utf8(bytes).is_ok(), "a line cut inside a character: {:?}", bytes);
    let prefix = if held == 0 { String::new() } else { format!("[held back: {}] ", held) };
    let whole = format!("{}{}", prefix, line);
    invariant!(whole.as_bytes().starts_with(bytes), "{:?} is not the start of {:?}", String::from_utf8_lossy(bytes),
               whole);
    let fits = whole.len() <= LINE_MAX;
    invariant!(out.cut() == !fits, "cut {} for a line of {} bytes (cut before the count: {})", out.cut(), whole.len(),
               cut_before);
    /* As much of it as fits, but for a character a cut would split */
    invariant!(fits || bytes.len() + 4 > LINE_MAX, "a cut line of {} bytes where {} fit", bytes.len(), LINE_MAX);
}

pub fn limiter(r: &mut Input) {
    let limiter = Limiter::new();
    let kinds_n = 1 + r.below(KINDS.len() as u64) as usize;
    let first = r.below(KINDS.len() as u64) as usize;
    let exact = kinds_n <= LIMIT_SHAPES;
    let flood = r.bool();
    let mut model = Model::default();
    /* The clock's start: never 0, which the limiter takes for no window */
    let mut t = 1 + r.below(1 << 40);
    let mut offered = 0u64;
    let mut went = 0u64;
    let mut said_held = 0u64;
    /* Held since the last line that went: what the next one must say */
    let mut held = 0u64;
    /* The place's window, which the lines' times alone make -- whatever
     * their kinds -- and what went in it */
    let mut place: Option<(u64, u32)> = None;
    while r.more() && (offered as usize) < OFFERED_MAX {
        t = next(r, t, place.map(|(start, _)| start), &model, flood);
        let kind = KINDS[(first + r.below(kinds_n as u64) as usize) % KINDS.len()];
        let line = line_of(kind, r.u64());
        offered += 1;
        let (start, in_window) = match place {
            Some((start, n)) if t - start < WINDOW => (start, n),
            _ => (t, 0),
        };

        /* What `flog_limited!` does: the closed place's look, then the
         * line's -- or, now and then, the line's alone: one that passed the
         * look on another CPU as the place's last line went on this one */
        let looked = !r.chance(8);
        let got = if !looked || limiter.open_at(t) {
            let mut buf = LineBuf::new();
            let _ = core::fmt::Write::write_str(&mut buf, &line);
            limiter.admit_at(buf.bytes(), t)
        } else {
            None
        };
        let want = model.offer(&shape(&line), t, looked);
        if exact {
            invariant!(got == want, "line {} at {} ns ({:?}): the limiter says {:?}, the rules {:?}", offered, t, line,
                       got, want);
        }
        match got {
            Some(n) => {
                invariant!(n == held, "line {} went saying {} held before it, of {}", offered, n, held);
                invariant!(in_window < LIMIT_PLACE, "line {} went, the {}th of the place's window begun at {} ns",
                           offered, in_window + 1, start);
                held = 0;
                went += 1;
                said_held += n;
                check_said(n, &line);
                place = Some((start, in_window + 1));
            }
            None => {
                held += 1;
                place = Some((start, in_window));
            }
        }
    }
    /* Every line held counted once: what went said every one held before
     * the last line that went, and the rest wait for the next */
    invariant!(went + said_held + held == offered, "{} lines offered, {} went, {} said held and {} held since",
               offered, went, said_held, held);
}
