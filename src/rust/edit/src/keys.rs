//! What a terminal sends, made keys: xterm's sequences, as Terminal.app,
//! iTerm2 and Linux's terminals send them.
//!
//! A key that is a sequence of bytes -- an arrow is `ESC [ A`, with Shift
//! `ESC [ 1 ; 2 A` -- may arrive split across reads, so the decoder holds
//! what it has of one until the rest comes. An ESC alone is both the Esc key
//! and the start of every sequence: the decoder cannot tell which until more
//! comes or nothing does, so the caller says when input has paused
//! (`Decoder::pause`), and what is held then is taken for what it is.
//!
//! A paste is bracketed (`ESC [ 200 ~` ... `ESC [ 201 ~`, which the editor
//! asks the terminal for): what is between is text, every byte of it --
//! a ^S in what was pasted is not a save, nor an ESC in it a key.

/// Modifier keys held with a key, as xterm reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    /// Alt -- Option on a Mac -- or Meta.
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { shift: false, alt: false, ctrl: false };
    pub const ALT: Mods = Mods { shift: false, alt: true, ctrl: false };

    /// xterm's modifier parameter: 1, plus 1 for Shift, 2 for Alt, 4 for
    /// Control and 8 for Meta.
    fn xterm(p: u32) -> Mods {
        let bits = p.saturating_sub(1);
        Mods { shift: bits & 1 != 0, alt: bits & (2 | 8) != 0, ctrl: bits & 4 != 0 }
    }
}

/// A character typed: one UTF-8 sequence, or one byte that is not part of
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ch {
    bytes: [u8; 4],
    len: u8,
}

impl Ch {
    fn one(b: u8) -> Ch {
        Ch { bytes: [b, 0, 0, 0], len: 1 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(Ch),
    Enter,
    Tab,
    /// The Mac's delete key, a PC's Backspace: with Alt (Option), a word.
    Backspace(Mods),
    /// Delete forward (fn-delete on a Mac).
    Delete(Mods),
    Esc,
    /// Control and a letter, the letter in lower case: ^A is `Ctrl(b'a')`.
    Ctrl(u8),
    Up(Mods),
    Down(Mods),
    Left(Mods),
    Right(Mods),
    Home(Mods),
    End(Mods),
    PageUp(Mods),
    PageDown(Mods),
    /// A paste begins: until `PasteEnd` comes only `Char`, `Enter` and
    /// `Tab`, the text pasted.
    PasteStart,
    PasteEnd,
}

const ESC: u8 = 0x1B;
/// What ends a paste, after its ESC.
const PASTE_END: &[u8] = b"[201~";
/// The longest run of parameter bytes a sequence of ours has; a longer one
/// is not one of ours, and is read to its end and dropped.
const PARAMS_MAX: usize = 16;
/// The most keys one byte can complete: a character cut short (its three
/// bytes) and the key the byte that cut it starts, or a paste's end marker
/// that was not one (its five bytes, as text).
const QUEUE_MAX: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// Inside a UTF-8 character, `need` bytes long.
    Utf8 { need: u8 },
    /// After an ESC.
    Esc,
    /// A control sequence (`ESC [`), its parameters held.
    Csi,
    /// A control sequence that is not one of ours: read to its end.
    CsiSkip,
    /// `ESC O`: the arrows in the terminal's application mode.
    Ss3,
    /// In a paste, after an ESC: whether `[201~` follows.
    PasteEsc,
}

pub struct Decoder {
    state: State,
    held: [u8; PARAMS_MAX],
    held_len: usize,
    paste: bool,
    /// The last byte was a CR: an LF right after it ends the same line.
    after_cr: bool,
    queue: [Option<Key>; QUEUE_MAX],
    queue_head: usize,
    queue_len: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Decoder {
        Decoder {
            state: State::Ground,
            held: [0; PARAMS_MAX],
            held_len: 0,
            paste: false,
            after_cr: false,
            queue: [None; QUEUE_MAX],
            queue_head: 0,
            queue_len: 0,
        }
    }

    /// Takes the next byte of input. The keys it completes come out of
    /// `next`, which is to be emptied before the next byte goes in.
    pub fn push(&mut self, b: u8) {
        match self.state {
            State::Ground => self.ground(b),
            State::Utf8 { need } => {
                if b & 0xC0 == 0x80 {
                    self.hold(b);
                    if self.held_len >= usize::from(need) {
                        self.state = State::Ground;
                        let mut ch = Ch { bytes: [0; 4], len: need };
                        for (d, s) in ch.bytes.iter_mut().zip(self.held.iter()) {
                            *d = *s;
                        }
                        self.held_len = 0;
                        self.emit(Key::Char(ch));
                    }
                } else {
                    /* Cut short: what came of it is bytes, and this one is
                       something else */
                    self.state = State::Ground;
                    self.flush_held_bytes();
                    self.ground(b);
                }
            }
            State::Esc => {
                self.state = State::Ground;
                match b {
                    b'[' => {
                        self.state = State::Csi;
                        self.held_len = 0;
                    }
                    b'O' => self.state = State::Ss3,
                    ESC => {
                        /* The first was the key itself */
                        self.emit(Key::Esc);
                        self.state = State::Esc;
                    }
                    /* Option-Left, Option-Right and Option-Delete, as
                       Terminal.app sends them (and readline's word keys) */
                    b'b' => self.emit(Key::Left(Mods::ALT)),
                    b'f' => self.emit(Key::Right(Mods::ALT)),
                    0x7F | 0x08 => self.emit(Key::Backspace(Mods::ALT)),
                    /* Another key with Alt: none of ours */
                    0x20..=0x7E => {}
                    _ => {
                        self.emit(Key::Esc);
                        self.ground(b);
                    }
                }
            }
            State::Csi | State::CsiSkip => match b {
                0x30..=0x3F => {
                    if self.state == State::Csi && self.held_len < PARAMS_MAX {
                        self.hold(b);
                    } else {
                        self.state = State::CsiSkip;
                    }
                }
                /* Intermediate bytes: no key of ours has them */
                0x20..=0x2F => self.state = State::CsiSkip,
                0x40..=0x7E => {
                    let ours = self.state == State::Csi;
                    self.state = State::Ground;
                    if ours {
                        self.csi(b);
                    }
                    self.held_len = 0;
                }
                /* A control byte: the sequence was cut short */
                _ => {
                    self.state = State::Ground;
                    self.held_len = 0;
                    self.ground(b);
                }
            },
            State::Ss3 => {
                self.state = State::Ground;
                match b {
                    b'A' => self.emit(Key::Up(Mods::NONE)),
                    b'B' => self.emit(Key::Down(Mods::NONE)),
                    b'C' => self.emit(Key::Right(Mods::NONE)),
                    b'D' => self.emit(Key::Left(Mods::NONE)),
                    b'H' => self.emit(Key::Home(Mods::NONE)),
                    b'F' => self.emit(Key::End(Mods::NONE)),
                    b'M' => self.emit(Key::Enter),
                    0x20..=0x7E => {}
                    _ => self.ground(b),
                }
            }
            State::PasteEsc => {
                if PASTE_END.get(self.held_len) == Some(&b) {
                    self.hold(b);
                    if self.held_len == PASTE_END.len() {
                        self.state = State::Ground;
                        self.held_len = 0;
                        self.paste = false;
                        self.after_cr = false;
                        self.emit(Key::PasteEnd);
                    }
                } else {
                    /* Not the end: the ESC was pasted, and is not text; what
                       came after it is */
                    self.state = State::Ground;
                    self.flush_held_paste();
                    self.ground(b);
                }
            }
        }
    }

    /// Input has paused: what is held is what it is. An ESC alone is the Esc
    /// key, a character cut short its bytes; a sequence cut short is
    /// dropped.
    pub fn pause(&mut self) {
        match self.state {
            State::Ground => {}
            State::Esc => {
                self.state = State::Ground;
                self.emit(Key::Esc);
            }
            State::Utf8 { .. } => {
                self.state = State::Ground;
                self.flush_held_bytes();
            }
            State::PasteEsc => {
                self.state = State::Ground;
                self.flush_held_paste();
            }
            State::Csi | State::CsiSkip | State::Ss3 => {
                self.state = State::Ground;
                self.held_len = 0;
            }
        }
    }

    /// Whether something is held that `pause` would decide: the caller waits
    /// a moment for more before it says input has paused.
    pub fn pending(&self) -> bool {
        self.state != State::Ground
    }

    /// Whether a paste has begun and not ended.
    pub fn in_paste(&self) -> bool {
        self.paste
    }

    /// The next key completed, oldest first.
    pub fn next(&mut self) -> Option<Key> {
        if self.queue_len == 0 {
            return None;
        }
        let key = self.queue.get_mut(self.queue_head).and_then(Option::take);
        self.queue_head = (self.queue_head + 1) % QUEUE_MAX;
        self.queue_len -= 1;
        key
    }

    fn emit(&mut self, k: Key) {
        /* `next` is emptied after every byte, so the queue holds what one
           byte makes at most; were it full, the oldest would go */
        if self.queue_len == QUEUE_MAX {
            self.queue_head = (self.queue_head + 1) % QUEUE_MAX;
            self.queue_len -= 1;
        }
        let at = (self.queue_head + self.queue_len) % QUEUE_MAX;
        if let Some(slot) = self.queue.get_mut(at) {
            *slot = Some(k);
            self.queue_len += 1;
        }
    }

    fn hold(&mut self, b: u8) {
        if let Some(slot) = self.held.get_mut(self.held_len) {
            *slot = b;
            self.held_len += 1;
        }
    }

    fn flush_held_bytes(&mut self) {
        let (held, len) = (self.held, self.held_len);
        self.held_len = 0;
        for &b in held.iter().take(len) {
            self.emit(Key::Char(Ch::one(b)));
        }
    }

    fn flush_held_paste(&mut self) {
        let (held, len) = (self.held, self.held_len);
        self.held_len = 0;
        for &b in held.iter().take(len) {
            self.ground(b);
        }
    }

    fn ground(&mut self, b: u8) {
        let after_cr = self.after_cr;
        self.after_cr = b == b'\r';
        if self.paste {
            match b {
                ESC => {
                    self.state = State::PasteEsc;
                    self.held_len = 0;
                }
                b'\r' => self.emit(Key::Enter),
                b'\n' => {
                    if !after_cr {
                        self.emit(Key::Enter);
                    }
                }
                b'\t' => self.emit(Key::Tab),
                /* Other control bytes are not text */
                0x00..=0x1F | 0x7F => {}
                _ => self.text(b),
            }
            return;
        }
        match b {
            ESC => self.state = State::Esc,
            b'\r' => self.emit(Key::Enter),
            b'\n' => {
                if !after_cr {
                    self.emit(Key::Enter);
                }
            }
            b'\t' => self.emit(Key::Tab),
            0x7F | 0x08 => self.emit(Key::Backspace(Mods::NONE)),
            0x01..=0x1A => self.emit(Key::Ctrl(b - 1 + b'a')),
            0x00 | 0x1C..=0x1F => {}
            _ => self.text(b),
        }
    }

    /* A byte of text: a character of one byte, or the start of a longer one */
    fn text(&mut self, b: u8) {
        let need = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => {
                self.emit(Key::Char(Ch::one(b)));
                return;
            }
        };
        self.held_len = 0;
        self.hold(b);
        self.state = State::Utf8 { need };
    }

    /* A control sequence ended by `fin`, its parameters held */
    fn csi(&mut self, fin: u8) {
        let held = self.held;
        let params = held.get(..self.held_len).unwrap_or(&[]);
        /* A private sequence (`ESC [ ? ...`, `ESC [ < ...`) is not a key */
        if params.first().is_some_and(|&c| matches!(c, b'<' | b'=' | b'>' | b'?')) {
            return;
        }
        let mut nums = [0u32; 2];
        let mut given = [false; 2];
        let mut index = 0;
        for &c in params {
            match c {
                b'0'..=b'9' => {
                    if let (Some(n), Some(g)) = (nums.get_mut(index), given.get_mut(index)) {
                        *n = n.saturating_mul(10).saturating_add(u32::from(c - b'0'));
                        *g = true;
                    }
                }
                b';' => index += 1,
                _ => return,
            }
        }
        let first = if given[0] { nums[0] } else { 1 };
        let mods = Mods::xterm(if given[1] { nums[1] } else { 1 });
        let key = match fin {
            b'A' => Key::Up(mods),
            b'B' => Key::Down(mods),
            b'C' => Key::Right(mods),
            b'D' => Key::Left(mods),
            b'H' => Key::Home(mods),
            b'F' => Key::End(mods),
            b'~' => match first {
                1 | 7 => Key::Home(mods),
                4 | 8 => Key::End(mods),
                3 => Key::Delete(mods),
                5 => Key::PageUp(mods),
                6 => Key::PageDown(mods),
                200 => {
                    self.paste = true;
                    Key::PasteStart
                }
                _ => return,
            },
            _ => return,
        };
        self.emit(key);
    }
}
