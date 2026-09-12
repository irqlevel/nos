//! The line editor in front of the shell. With a terminal (a pty-req) the
//! client's side is raw: every key arrives as it is pressed, and the echo,
//! the erasing and the history are the server's to do. Without one the
//! client sends whole lines and nothing is echoed; the same editor splits
//! them.

use alloc::string::String;
use alloc::vec::Vec;

/// The longest line taken: what the kernel runs a command line of at most.
pub const LINE_MAX: usize = 255;
/// Lines Up and Down go back through.
const HISTORY_MAX: usize = 32;

const NUL: u8 = 0x00;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const BELL: u8 = 0x07;
const BACKSPACE: u8 = 0x08;
const LF: u8 = 0x0A;
const CTRL_L: u8 = 0x0C;
const CR: u8 = 0x0D;
const CTRL_U: u8 = 0x15;
const ESC: u8 = 0x1B;
const DEL: u8 = 0x7F;

/// Erase the character left of the cursor; go to the start of the line
/// and clear it; clear the screen.
const ERASE: &[u8] = b"\x08 \x08";
const CLEAR_LINE: &[u8] = b"\r\x1b[K";
const CLEAR_SCREEN: &[u8] = b"\x1b[H\x1b[2J";

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

pub struct Editor {
    line: Vec<u8>,
    history: Vec<Vec<u8>>,
    /// Where Up and Down have got to in the history: its length when not in
    /// it.
    browse: usize,
    /// The line being typed when Up took it away, for Down to give back.
    draft: Vec<u8>,
    escape: Escape,
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
            after_cr: false,
        }
    }

    /// One byte from the client. What its terminal should show for it goes
    /// on `echo`; a line redrawn starts with `prompt`.
    pub fn key(&mut self, b: u8, prompt: &str, echo: &mut Vec<u8>) -> Action {
        let after_cr = core::mem::replace(&mut self.after_cr, false);

        match self.escape {
            Escape::Esc => {
                self.escape = if b == b'[' || b == b'O' { Escape::Sequence } else { Escape::None };
                return Action::None;
            }
            Escape::Sequence => {
                if (0x40..=0x7E).contains(&b) {
                    self.escape = Escape::None;
                    match b {
                        b'A' => self.older(prompt, echo),
                        b'B' => self.newer(prompt, echo),
                        _ => {}
                    }
                }
                return Action::None;
            }
            Escape::None => {}
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

    fn enter(&mut self, echo: &mut Vec<u8>) -> Action {
        echo.extend_from_slice(b"\r\n");
        let line = core::mem::take(&mut self.line);
        self.draft.clear();
        if !line.is_empty() && self.history.last() != Some(&line) {
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
