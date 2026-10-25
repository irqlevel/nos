//! `terminal`: the SSH session's line editor (`ssh::terminal`), on its own.
//! On a session with a terminal every key the client presses reaches it as
//! it is pressed, and what it echoes is what the client's terminal shows --
//! so a client can send it anything: escape sequences cut short, lines past
//! the longest it takes, a search through a history past its end. The keys
//! here are anything, weighted to the ones it acts on and to a handful of
//! letters, so that lines repeat and a search finds them.
//!
//! Every key is held to a model of the editor written from its comments:
//! the line Enter hands the shell, a logout on ^D on an empty line and on
//! nothing else, and what the client's terminal shows once it has acted on
//! the echo -- a terminal of one row, which knows text, CR, LF, Backspace,
//! erase to the end of the line and clear the screen, and finds anything
//! else in the echo a finding: the prompt and the line, or the search's
//! prompt, its query and what it found, exactly, with nothing but blanks
//! after the cursor.

use ssh::terminal::{Action, Editor, HISTORY_MAX, LINE_MAX, SEARCH, SEARCH_END, SEARCH_FAILED};

use crate::Input;

const PROMPT: &str = "nos$ ";

const NUL: u8 = 0x00;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const CTRL_G: u8 = 0x07;
const BELL: u8 = 0x07;
const BS: u8 = 0x08;
const LF: u8 = 0x0A;
const CTRL_L: u8 = 0x0C;
const CR: u8 = 0x0D;
const CTRL_N: u8 = 0x0E;
const CTRL_P: u8 = 0x10;
const CTRL_R: u8 = 0x12;
const CTRL_U: u8 = 0x15;
const ESC: u8 = 0x1B;
const DEL: u8 = 0x7F;

fn printable(b: u8) -> bool {
    (0x20..=0x7E).contains(&b)
}

/// The client's terminal: one row, and the cursor on it.
struct Screen {
    row: Vec<u8>,
    col: usize,
}

impl Screen {
    fn put(&mut self, b: u8) {
        if self.col < self.row.len() {
            self.row[self.col] = b;
        } else {
            self.row.push(b);
        }
        self.col += 1;
    }

    /// The echo of one key, acted on.
    fn echo(&mut self, echo: &[u8]) {
        let mut i = 0;
        while i < echo.len() {
            let b = echo[i];
            i += 1;
            match b {
                BELL => {}
                BS => {
                    invariant!(self.col > 0, "a Backspace at the start of the row");
                    self.col -= 1;
                }
                CR => self.col = 0,
                LF => {
                    invariant!(self.col == 0, "a line end that is not CR LF");
                    self.row.clear();
                }
                ESC => {
                    let rest = &echo[i..];
                    if rest.starts_with(b"[K") {
                        self.row.truncate(self.col);
                        i += 2;
                    } else if rest.starts_with(b"[H") {
                        self.col = 0;
                        i += 2;
                    } else if rest.starts_with(b"[2J") {
                        self.row.clear();
                        i += 3;
                    } else {
                        panic!("invariant: an escape sequence the editor has no business sending: {:?}",
                               String::from_utf8_lossy(rest));
                    }
                }
                _ => {
                    invariant!(printable(b), "the editor echoed byte 0x{:02X}", b);
                    self.put(b);
                }
            }
        }
    }

    /// The row shows `want` up to the cursor and nothing after it.
    fn check(&self, want: &[u8], key: u8) {
        let shown = &self.row[..self.col.min(self.row.len())];
        invariant!(self.col <= self.row.len() && shown == want && self.row[self.col..].iter().all(|&b| b == b' '),
                   "after key 0x{:02X} the terminal shows {:?} (cursor at {}), the model {:?}", key,
                   String::from_utf8_lossy(&self.row), self.col, String::from_utf8_lossy(want));
    }
}

#[derive(PartialEq)]
enum Esc {
    None,
    Start,
    Sequence,
}

/// What the editor's comments say it does.
struct Model {
    esc: Esc,
    after_cr: bool,
    line: Vec<u8>,
    history: Vec<Vec<u8>>,
    browse: usize,
    draft: Vec<u8>,
    searching: bool,
    failed: bool,
    query: Vec<u8>,
    found: Option<usize>,
    saved: Vec<u8>,
}

enum Did {
    Nothing,
    Line(Vec<u8>),
    Logout,
}

impl Model {
    fn new() -> Model {
        Model { esc: Esc::None, after_cr: false, line: Vec::new(), history: Vec::new(), browse: 0, draft: Vec::new(),
                searching: false, failed: false, query: Vec::new(), found: None, saved: Vec::new() }
    }

    fn older(&mut self) {
        if self.browse == 0 {
            return;
        }
        if self.browse == self.history.len() {
            self.draft = self.line.clone();
        }
        self.browse -= 1;
        self.line = self.history[self.browse].clone();
    }

    fn newer(&mut self) {
        if self.browse >= self.history.len() {
            return;
        }
        self.browse += 1;
        self.line = if self.browse == self.history.len() { self.draft.clone() } else { self.history[self.browse].clone() };
    }

    /// The newest line older than `below` holding the query.
    fn find(&mut self, below: usize) {
        let q = String::from_utf8_lossy(&self.query).into_owned();
        for i in (0..below).rev() {
            if String::from_utf8_lossy(&self.history[i]).contains(q.as_str()) {
                self.found = Some(i);
                self.failed = false;
                self.line = self.history[i].clone();
                return;
            }
        }
        self.failed = true;
    }

    fn enter(&mut self) -> Did {
        let line = core::mem::take(&mut self.line);
        if line.iter().any(|&b| b != b' ') && self.history.last() != Some(&line) {
            if self.history.len() == HISTORY_MAX {
                self.history.remove(0);
            }
            self.history.push(line.clone());
        }
        self.draft.clear();
        self.browse = self.history.len();
        Did::Line(line)
    }

    fn key(&mut self, b: u8) -> Did {
        let after_cr = core::mem::replace(&mut self.after_cr, false);

        match self.esc {
            Esc::Start => {
                self.esc = Esc::None;
                if b == b'[' || b == b'O' {
                    self.esc = Esc::Sequence;
                    return Did::Nothing;
                }
                if printable(b) {
                    return Did::Nothing;
                }
            }
            Esc::Sequence => {
                if (0x20..=0x3F).contains(&b) {
                    return Did::Nothing;
                }
                self.esc = Esc::None;
                if (0x40..=0x7E).contains(&b) {
                    match b {
                        b'A' => self.older(),
                        b'B' => self.newer(),
                        _ => {}
                    }
                    return Did::Nothing;
                }
            }
            Esc::None => {}
        }

        if self.searching {
            let len = self.history.len();
            match b {
                _ if printable(b) => {
                    if self.query.len() < LINE_MAX {
                        self.query.push(b);
                        self.find(self.found.map_or(len, |i| i + 1));
                    }
                    return Did::Nothing;
                }
                CTRL_R => {
                    self.find(self.found.unwrap_or(len));
                    return Did::Nothing;
                }
                BS | DEL => {
                    if self.query.pop().is_some() {
                        self.line = self.saved.clone();
                        self.found = None;
                        self.failed = false;
                        if !self.query.is_empty() {
                            self.find(len);
                        }
                    }
                    return Did::Nothing;
                }
                CTRL_G => {
                    self.line = self.saved.clone();
                    self.searching = false;
                    return Did::Nothing;
                }
                _ => {
                    self.searching = false;
                    if let Some(i) = self.found {
                        if self.browse == len {
                            self.draft = self.saved.clone();
                        }
                        self.browse = i;
                    }
                }
            }
        }

        match b {
            ESC => self.esc = Esc::Start,
            CR => {
                self.after_cr = true;
                return self.enter();
            }
            LF | NUL if after_cr => {}
            LF => return self.enter(),
            BS | DEL => {
                self.line.pop();
            }
            CTRL_C => {
                self.line.clear();
                self.browse = self.history.len();
            }
            CTRL_D => {
                if self.line.is_empty() {
                    return Did::Logout;
                }
            }
            CTRL_U => self.line.clear(),
            CTRL_P => self.older(),
            CTRL_N => self.newer(),
            CTRL_R => {
                self.searching = true;
                self.failed = false;
                self.query.clear();
                self.found = None;
                self.saved = self.line.clone();
            }
            _ if printable(b) => {
                if self.line.len() < LINE_MAX {
                    self.line.push(b);
                }
            }
            _ => {}
        }
        Did::Nothing
    }

    /// What the terminal shows once the key's echo is acted on.
    fn shown(&self) -> Vec<u8> {
        let mut v = Vec::new();
        if self.searching {
            v.extend_from_slice(if self.failed { SEARCH_FAILED } else { SEARCH });
            v.extend_from_slice(&self.query);
            v.extend_from_slice(SEARCH_END);
        } else {
            v.extend_from_slice(PROMPT.as_bytes());
        }
        v.extend_from_slice(&self.line);
        v
    }
}

/// The keys the editor acts on -- ^D, which ends the session, rarely --
/// and letters few enough to repeat.
const KEYS: &[u8] = &[CR, LF, NUL, BS, DEL, CTRL_C, CTRL_G, CTRL_L, CTRL_N, CTRL_P, CTRL_R, CTRL_R, CTRL_U, ESC, ESC,
                      b'[', b'O', b'A', b'B', b'C', b'D', b'1', b';', b'\t'];
const LETTERS: &[u8] = b"ab c";

fn next_key(r: &mut Input) -> u8 {
    match r.below(8) {
        0 | 1 => r.pick(KEYS),
        2 => {
            if r.chance(16) {
                CTRL_D
            } else {
                r.u8()
            }
        }
        _ => r.pick(LETTERS),
    }
}

pub fn terminal(r: &mut Input) {
    let mut ed = Editor::new();
    let mut m = Model::new();
    let mut screen = Screen { row: Vec::new(), col: 0 };
    let mut echo = Vec::new();

    /* The session shows the prompt before the first key */
    screen.echo(PROMPT.as_bytes());

    while r.more() {
        /* Now and then a run of one key: a line past LINE_MAX, a history
           past HISTORY_MAX, Up past its oldest line */
        let times = if r.chance(16) { r.range(1, 2 * LINE_MAX as u64) } else { 1 };
        let b = next_key(r);
        if times > 1 {
            crate::common::runner::reached("terminal: a run of one key");
        }
        for _ in 0..times {
            echo.clear();
            let action = ed.key(b, PROMPT, &mut echo);
            let did = m.key(b);
            screen.echo(&echo);
            match (action, did) {
                (Action::None, Did::Nothing) => {}
                (Action::Line(got), Did::Line(want)) => {
                    invariant!(got.as_bytes() == want.as_slice(), "Enter ran {:?}, the model {:?}", got,
                               String::from_utf8_lossy(&want));
                    invariant!(screen.row.is_empty() && screen.col == 0, "Enter left {:?} on the line it ended",
                               String::from_utf8_lossy(&screen.row));
                    crate::common::runner::reached("terminal: a line run");
                    if m.history.len() == HISTORY_MAX {
                        crate::common::runner::reached("terminal: a full history");
                    }
                    /* The session's next prompt */
                    screen.echo(PROMPT.as_bytes());
                }
                (Action::Logout, Did::Logout) => {
                    crate::common::runner::reached("terminal: a logout");
                    return;
                }
                (Action::None, _) => panic!("invariant: key 0x{:02X} did nothing, the model says it did", b),
                (Action::Line(got), _) => panic!("invariant: key 0x{:02X} ran {:?}, the model says it did not", b, got),
                (Action::Logout, _) => panic!("invariant: key 0x{:02X} logged out, the model says it did not", b),
            }
            screen.check(&m.shown(), b);

            if m.searching && m.failed {
                crate::common::runner::reached("terminal: a search that failed");
            }
            if m.searching && m.found.is_some() {
                crate::common::runner::reached("terminal: a search that found a line");
            }
            if !m.searching && m.browse < m.history.len() {
                crate::common::runner::reached("terminal: a line from the history");
            }
            if m.line.len() == LINE_MAX {
                crate::common::runner::reached("terminal: a line at LINE_MAX");
            }
        }
    }
}
