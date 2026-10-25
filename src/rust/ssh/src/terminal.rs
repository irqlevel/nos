//! The line editor in front of the shell. With a terminal (a pty-req) the
//! client's side is raw: every key arrives as it is pressed, and the echo,
//! the erasing and the history are the server's to do. Without one the
//! client sends whole lines and nothing is echoed; the same editor splits
//! them.
//!
//! Up and Down (^P and ^N) go back through the last lines run and forward
//! again to the one being typed; ^R searches back through them for a line
//! that holds what is typed after it -- bash's reverse-i-search: ^R again
//! finds the next older one, Backspace takes a character off what is
//! searched for, Enter runs what was found, ^G puts the line from before
//! the search back, and any other key keeps what was found, to be edited,
//! and is then the key it is. The console's editor
//! (src/cpp/kernel/line_editor.h) does the same on a screen that knows only
//! Backspace.

use alloc::string::String;
use alloc::vec::Vec;

/// The longest line taken: what the kernel runs a command line of at most.
pub const LINE_MAX: usize = 255;
/// Lines Up, Down and ^R go back through.
pub const HISTORY_MAX: usize = 32;

const NUL: u8 = 0x00;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const CTRL_G: u8 = 0x07;
const BELL: u8 = 0x07;
const BACKSPACE: u8 = 0x08;
const LF: u8 = 0x0A;
const CTRL_L: u8 = 0x0C;
const CR: u8 = 0x0D;
const CTRL_N: u8 = 0x0E;
const CTRL_P: u8 = 0x10;
const CTRL_R: u8 = 0x12;
const CTRL_U: u8 = 0x15;
const ESC: u8 = 0x1B;
const DEL: u8 = 0x7F;

/// Printable ASCII: all a line is ever made of.
const PRINTABLE: core::ops::RangeInclusive<u8> = 0x20..=0x7E;
/// ECMA-48's control sequence: ESC [, parameter and intermediate bytes, and
/// a final byte that says which key -- A up, B down.
const CSI_MIDDLE: core::ops::RangeInclusive<u8> = 0x20..=0x3F;
const CSI_FINAL: core::ops::RangeInclusive<u8> = 0x40..=0x7E;

/// Erase the character left of the cursor; go to the start of the line
/// and clear it; clear the screen.
const ERASE: &[u8] = b"\x08 \x08";
const CLEAR_LINE: &[u8] = b"\r\x1b[K";
const CLEAR_SCREEN: &[u8] = b"\x1b[H\x1b[2J";

/// What a search shows in place of the prompt, before what it searches for
/// and after it -- bash's words.
pub const SEARCH: &[u8] = b"(reverse-i-search)'";
pub const SEARCH_FAILED: &[u8] = b"(failed reverse-i-search)'";
pub const SEARCH_END: &[u8] = b"': ";

/// What a key did.
pub enum Action {
    None,
    /// A line, ended by Enter.
    Line(String),
    /// ^D on an empty line.
    Logout,
}

/// Where the editor is in an escape sequence: the terminal's arrow keys
/// are ESC [ A and the like (ESC O A in application mode).
enum Escape {
    None,
    Esc,
    /// Inside a sequence, until its final byte.
    Sequence,
}

/// The search ^R began.
struct Search {
    query: Vec<u8>,
    /// The history line it found: `None` before it found one.
    found: Option<usize>,
    /// Nothing older holds the query: the line is the last that did.
    failed: bool,
    /// The line from before the search, for ^G.
    saved: Vec<u8>,
}

pub struct Editor {
    line: Vec<u8>,
    history: Vec<Vec<u8>>,
    /// Where Up and Down have got to in the history: its length when not in
    /// it.
    browse: usize,
    /// The line being typed when Up took it away, for Down to give back.
    draft: Vec<u8>,
    escape: Escape,
    search: Option<Search>,
    /// The last key was CR: an LF or a NUL right after it is the rest of
    /// the same Enter.
    after_cr: bool,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            line: Vec::new(),
            history: Vec::new(),
            browse: 0,
            draft: Vec::new(),
            escape: Escape::None,
            search: None,
            after_cr: false,
        }
    }

    /// One byte from the client. What its terminal should show for it goes
    /// on `echo`; a line redrawn starts with `prompt`.
    pub fn key(&mut self, b: u8, prompt: &str, echo: &mut Vec<u8>) -> Action {
        let after_cr = core::mem::replace(&mut self.after_cr, false);

        match self.escape {
            Escape::Esc => {
                self.escape = Escape::None;
                if b == b'[' || b == b'O' {
                    self.escape = Escape::Sequence;
                    return Action::None;
                }
                /* ESC and a character is Alt and it, which nothing here has
                   a use for; a control key is itself */
                if PRINTABLE.contains(&b) {
                    return Action::None;
                }
            }
            Escape::Sequence => {
                if CSI_MIDDLE.contains(&b) {
                    return Action::None;
                }
                self.escape = Escape::None;
                if CSI_FINAL.contains(&b) {
                    match b {
                        b'A' => self.older(prompt, echo),
                        b'B' => self.newer(prompt, echo),
                        _ => {}
                    }
                    return Action::None;
                }
                /* A control key cut the sequence short: it is a key of its
                   own */
            }
            Escape::None => {}
        }

        if self.search.is_some() && self.search_key(b, prompt, echo) {
            return Action::None;
        }

        match b {
            ESC => self.escape = Escape::Esc,
            CR => {
                self.after_cr = true;
                return self.enter(echo);
            }
            LF | NUL if after_cr => {}
            LF => return self.enter(echo),
            DEL | BACKSPACE => {
                if self.line.pop().is_some() {
                    echo.extend_from_slice(ERASE);
                }
            }
            CTRL_C => {
                self.line.clear();
                self.browse = self.history.len();
                echo.extend_from_slice(b"^C\r\n");
                echo.extend_from_slice(prompt.as_bytes());
            }
            CTRL_D => {
                if self.line.is_empty() {
                    return Action::Logout;
                }
            }
            CTRL_U => {
                self.line.clear();
                self.redraw(prompt, echo);
            }
            CTRL_L => {
                echo.extend_from_slice(CLEAR_SCREEN);
                echo.extend_from_slice(prompt.as_bytes());
                echo.extend_from_slice(&self.line);
            }
            CTRL_P => self.older(prompt, echo),
            CTRL_N => self.newer(prompt, echo),
            CTRL_R => {
                self.search = Some(Search { query: Vec::new(), found: None, failed: false, saved: self.line.clone() });
                self.show_search(echo);
            }
            0x20..=0x7E => {
                if self.line.len() < LINE_MAX {
                    self.line.push(b);
                    echo.push(b);
                } else {
                    echo.push(BELL);
                }
            }
            /* Tab, the other control keys, and the bytes of any character
               past ASCII: no command has a use for them */
            _ => {}
        }
        Action::None
    }

    /// A key during a search: true when it was the search's. Any other key
    /// ends the search keeping what it found, shown as the line again, and
    /// is then the line's.
    fn search_key(&mut self, b: u8, prompt: &str, echo: &mut Vec<u8>) -> bool {
        let Some(mut s) = self.search.take() else {
            return false;
        };
        match b {
            0x20..=0x7E => {
                if s.query.len() < LINE_MAX {
                    s.query.push(b);
                    /* The line found so far may hold the longer query too */
                    let below = s.found.map_or(self.history.len(), |i| i + 1);
                    self.find(&mut s, below, echo);
                } else {
                    echo.push(BELL);
                }
            }
            CTRL_R => {
                let below = s.found.unwrap_or(self.history.len());
                self.find(&mut s, below, echo);
            }
            DEL | BACKSPACE => {
                /* A shorter query is searched for again from the newest line */
                if s.query.pop().is_some() {
                    self.line = s.saved.clone();
                    s.found = None;
                    s.failed = false;
                    if !s.query.is_empty() {
                        self.find(&mut s, self.history.len(), echo);
                    }
                } else {
                    echo.push(BELL);
                }
            }
            CTRL_G => {
                self.line = s.saved;
                self.redraw(prompt, echo);
                return true;
            }
            _ => {
                if let Some(i) = s.found {
                    /* Up and Down go on from what was found, and Down past
                       the newest line still gives back the one being typed
                       before it all */
                    if self.browse == self.history.len() {
                        self.draft = s.saved;
                    }
                    self.browse = i;
                }
                self.redraw(prompt, echo);
                return false;
            }
        }
        self.search = Some(s);
        self.show_search(echo);
        true
    }

    /// The newest line older than `below` that holds the query is the
    /// line, or the search has failed and the line stays the last it found.
    fn find(&mut self, s: &mut Search, below: usize, echo: &mut Vec<u8>) {
        let below = below.min(self.history.len());
        match (0..below).rev().find(|&i| holds(&self.history[i], &s.query)) {
            Some(i) => {
                s.found = Some(i);
                s.failed = false;
                self.line = self.history[i].clone();
            }
            None => {
                s.failed = true;
                echo.push(BELL);
            }
        }
    }

    fn show_search(&self, echo: &mut Vec<u8>) {
        let Some(s) = &self.search else {
            return;
        };
        echo.extend_from_slice(CLEAR_LINE);
        echo.extend_from_slice(if s.failed { SEARCH_FAILED } else { SEARCH });
        echo.extend_from_slice(&s.query);
        echo.extend_from_slice(SEARCH_END);
        echo.extend_from_slice(&self.line);
    }

    fn enter(&mut self, echo: &mut Vec<u8>) -> Action {
        echo.extend_from_slice(b"\r\n");
        let line = core::mem::take(&mut self.line);
        self.draft.clear();
        /* A line of nothing but spaces is not worth going back to, and the
           same line twice running is kept once */
        if line.iter().any(|&c| c != b' ') && self.history.last() != Some(&line) {
            if self.history.len() == HISTORY_MAX {
                self.history.remove(0);
            }
            self.history.push(line.clone());
        }
        self.browse = self.history.len();
        /* Printable ASCII is all that ever gets in */
        Action::Line(String::from_utf8(line).unwrap_or_default())
    }

    fn redraw(&self, prompt: &str, echo: &mut Vec<u8>) {
        echo.extend_from_slice(CLEAR_LINE);
        echo.extend_from_slice(prompt.as_bytes());
        echo.extend_from_slice(&self.line);
    }

    fn older(&mut self, prompt: &str, echo: &mut Vec<u8>) {
        if self.browse == 0 {
            echo.push(BELL);
            return;
        }
        if self.browse == self.history.len() {
            self.draft = self.line.clone();
        }
        self.browse -= 1;
        self.line = self.history[self.browse].clone();
        self.redraw(prompt, echo);
    }

    fn newer(&mut self, prompt: &str, echo: &mut Vec<u8>) {
        if self.browse >= self.history.len() {
            echo.push(BELL);
            return;
        }
        self.browse += 1;
        self.line = if self.browse == self.history.len() {
            core::mem::take(&mut self.draft)
        } else {
            self.history[self.browse].clone()
        };
        self.redraw(prompt, echo);
    }
}

/// Whether `line` holds `query`: every line holds the empty one.
fn holds(line: &[u8], query: &[u8]) -> bool {
    query.is_empty() || line.windows(query.len()).any(|w| w == query)
}
