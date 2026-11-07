//! The editor: the text, the cursor and the selection, what each key does to
//! them, and the frame that shows them.
//!
//! The keys are a Mac's habits on Control, since a terminal keeps Command
//! for itself and sends Option as characters: ^S save, ^Q quit, ^F find, ^G
//! go to a line, ^Z and ^Y undo and redo, ^X ^C ^V cut, copy and paste --
//! the selection, or with none the line the cursor is on -- and ^A and ^E,
//! ^W and ^U, Option-arrows, as macOS's own text fields have them. Shift
//! with a movement selects. Nothing needs a key a Mac's keyboard lacks:
//! Home, End and the page keys work where a terminal sends them, and each
//! has a Control key or the trackpad in its place.

use alloc::vec::Vec;

use crate::glyph::{self, Glyph};
use crate::history::{History, Kind, Place};
use crate::keys::{Key, Mods};
use crate::screen::{push, push_num, Row, Screen, BOLD, PLAIN, REVERSE, UNDERLINE};
use crate::text::{self, Pos, Text};
use crate::Error;

/// A terminal's size: columns and rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
}

/// What a key asks of whoever runs the editor: the editor itself writes no
/// file and reads none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Write `contents` to the file and say `saved` -- or, unless `force`,
    /// when the file changed on disk since it was read, `ask_overwrite`
    /// instead. With `then_quit`, leave once it is written.
    Save { then_quit: bool, force: bool },
    /// Leave: nothing is left unsaved, or the person said to leave it.
    Quit,
}

/// The largest terminal drawn to; a larger one is drawn in its corner.
const COLS_MAX: usize = 1000;
const ROWS_MAX: usize = 500;
/// The smallest one the editor is drawn on: on a smaller one it says so.
const COLS_MIN: usize = 24;
const ROWS_MIN: usize = 3;
/// A paste goes into the text this much at a time.
const PASTE_PIECE: usize = 64 * 1024;
/// The longest search term.
const TERM_MAX: usize = 256;
/// The most digits a line number is typed with.
const DIGITS_MAX: usize = 10;

/// The keys the bottom row lists.
const HELP: &[(&[u8], &[u8])] = &[
    (b"^S", b"Save"),
    (b"^Q", b"Quit"),
    (b"^F", b"Find"),
    (b"^G", b"Line"),
    (b"^Z", b"Undo"),
    (b"^Y", b"Redo"),
    (b"^X", b"Cut"),
    (b"^C", b"Copy"),
    (b"^V", b"Paste"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Question {
    /// ^Q with changes not saved.
    SaveBeforeQuit,
    /// A save, and the file changed on disk since it was read.
    Overwrite { then_quit: bool },
}

#[derive(Clone, Copy)]
enum Mode {
    Edit,
    /// The search term typed at the bottom; where the cursor was when the
    /// search began, which it goes back to while nothing is found.
    Find { origin: Place },
    Line { digits: [u8; DIGITS_MAX], len: usize },
    Ask(Question),
}

pub struct Editor {
    /// The file's name, as the status bar shows it.
    name: Vec<u8>,
    text: Text,
    cur: Pos,
    /// The other end of the selection, when there is one.
    anchor: Option<Pos>,
    /// The column moving up and down keeps to, through shorter lines.
    want: Option<usize>,
    /// The first line shown, and the first column.
    top: usize,
    left: usize,
    history: History,
    clip: Vec<u8>,
    /// What was cut or copied is whole lines: pasted above the cursor's line.
    clip_lines: bool,
    /// The last key cut a line: another cut adds its line to the clip.
    cut_run: bool,
    /// The last search term, and whether it was found.
    term: Vec<u8>,
    found: bool,
    mode: Mode,
    /// Said on the bottom row until the next key.
    message: Vec<u8>,
    /// A paste taken in, not yet put in the text; and whether a paste is
    /// arriving at all -- into a prompt, or past what memory there was.
    paste: Option<Vec<u8>>,
    pasting: bool,
    new_file: bool,
    /// Rows of text the last frame had: what a page is.
    page: usize,
    screen: Screen,
}

impl Editor {
    /// An editor on a file with this content -- `None` for one that does
    /// not exist yet -- shown under `name`.
    pub fn open(name: &[u8], content: Option<&[u8]>) -> Result<Editor, Error> {
        let text = match content {
            Some(c) => Text::from_bytes(c)?,
            None => Text::new_file()?,
        };
        Ok(Editor {
            name: text::copy(name)?,
            text,
            cur: Pos::default(),
            anchor: None,
            want: None,
            top: 0,
            left: 0,
            history: History::new(),
            clip: Vec::new(),
            clip_lines: false,
            cut_run: false,
            term: Vec::new(),
            found: true,
            mode: Mode::Edit,
            message: Vec::new(),
            paste: None,
            pasting: false,
            new_file: content.is_none(),
            page: 1,
            screen: Screen::new(),
        })
    }

    /// Whether there are changes not saved.
    pub fn modified(&self) -> bool {
        self.history.modified()
    }

    /// What the file would be written as.
    pub fn contents(&self) -> Result<Vec<u8>, Error> {
        self.text.to_bytes()
    }

    /// The `contents` asked for last are in the file.
    pub fn saved(&mut self) {
        self.history.mark_saved();
        self.new_file = false;
        self.say(b"saved");
    }

    /// A line on the bottom row, until the next key.
    pub fn say(&mut self, text: &[u8]) {
        self.message.clear();
        if self.message.try_reserve(text.len()).is_ok() {
            self.message.extend_from_slice(text);
        }
    }

    /// The file changed on disk since it was read: ask whether to write over
    /// it, which a yes answers with `Action::Save { force: true, .. }`.
    pub fn ask_overwrite(&mut self, then_quit: bool) {
        self.mode = Mode::Ask(Question::Overwrite { then_quit });
    }

    /// The next frame draws everything.
    pub fn redraw(&mut self) {
        self.screen.invalidate();
    }

    /// Input has paused: a paste that has not ended is shown as far as it
    /// came.
    pub fn idle(&mut self) {
        self.flush_paste();
    }

    pub fn cursor(&self) -> Pos {
        self.cur
    }

    /// The selection, start first; none when its ends meet.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor.filter(|&a| a != self.cur)?;
        Some(if a < self.cur { (a, self.cur) } else { (self.cur, a) })
    }

    pub fn clipboard(&self) -> &[u8] {
        &self.clip
    }

    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// Whether a question or a prompt is up, rather than the text.
    pub fn prompting(&self) -> bool {
        !matches!(self.mode, Mode::Edit)
    }

    /// Whether everything done since the file was read can still be undone.
    pub fn history_complete(&self) -> bool {
        self.history.complete()
    }

    /// Checks what the editor promises: the text's and the history's, and
    /// the cursor and the selection in the text where glyphs start. For the
    /// tests.
    pub fn check(&self) -> Result<(), &'static str> {
        self.text.check()?;
        self.history.check()?;
        for p in [Some(self.cur), self.anchor].into_iter().flatten() {
            if p.line > self.text.last_line() || p.col > self.text.line(p.line).len() {
                return Err("editor: a position outside the text");
            }
            if !glyph::is_boundary(self.text.line(p.line), p.col) {
                return Err("editor: a position inside a glyph");
            }
        }
        Ok(())
    }

    /// What a key does.
    pub fn key(&mut self, k: Key) -> Action {
        match k {
            Key::PasteStart => {
                self.pasting = true;
                if matches!(self.mode, Mode::Edit) {
                    self.message.clear();
                    self.paste = Some(Vec::new());
                }
                return Action::None;
            }
            Key::PasteEnd => {
                self.flush_paste();
                self.paste = None;
                self.pasting = false;
                return Action::None;
            }
            _ => {}
        }
        if self.paste.is_some() {
            self.take_paste(k);
            return Action::None;
        }
        if self.pasting {
            /* Into a prompt: its characters, and no key in it is a command
               or an answer */
            return match (self.mode, k) {
                (Mode::Find { .. } | Mode::Line { .. }, Key::Char(_)) => self.prompt_key(k),
                _ => Action::None,
            };
        }
        self.message.clear();
        let cut_run = core::mem::replace(&mut self.cut_run, false);
        match self.mode {
            Mode::Edit => self.edit(k, cut_run),
            Mode::Find { .. } | Mode::Line { .. } => self.prompt_key(k),
            Mode::Ask(q) => self.answer(q, k),
        }
    }

    /// Draws the frame for a terminal of `size` into `out`: the rows that
    /// changed since the last one, and the cursor where it is.
    pub fn render(&mut self, size: Size, out: &mut Vec<u8>) -> Result<(), Error> {
        let cols = size.cols.clamp(1, COLS_MAX);
        let rows = size.rows.clamp(1, ROWS_MAX);
        self.screen.begin(cols, rows, out)?;
        if cols < COLS_MIN || rows < ROWS_MIN {
            let mut row = self.screen.row();
            draw_text(&mut row, b"edit: the terminal is too small", cols - 1, PLAIN)?;
            let (w, a) = (row.width(), row.attr());
            self.screen.commit(0, w, a, out)?;
            for r in 1..rows {
                self.screen.row();
                self.screen.commit(r, 0, PLAIN, out)?;
            }
            return self.screen.end(0, 0, out);
        }

        let page = rows - 2;
        self.page = page;
        self.scroll(page, cols);
        let sel = self.selection();
        for r in 0..page {
            let l = self.top + r;
            let Editor { screen, text, left, .. } = self;
            let mut row = screen.row();
            if l < text.line_count() {
                let line = text.line(l);
                draw_line(&mut row, line, *left, cols, line_selection(sel, l, line.len()))?;
            }
            let (w, a) = (row.width(), row.attr());
            screen.commit(r, w, a, out)?;
        }
        self.status(rows - 2, cols, out)?;
        let prompt_end = self.bottom(rows - 1, cols, out)?;
        let (x, y) = match self.mode {
            Mode::Edit => (self.column(self.cur).saturating_sub(self.left), self.cur.line - self.top),
            _ => (prompt_end, rows - 1),
        };
        self.screen.end(x.min(cols - 1), y, out)
    }

    /* ---- the keys ---- */

    fn edit(&mut self, k: Key, cut_run: bool) -> Action {
        match k {
            Key::Char(c) => self.type_in(c.as_bytes(), Kind::Typing),
            Key::Enter => self.type_in(b"\n", Kind::Other),
            Key::Tab => self.type_in(b"\t", Kind::Typing),
            Key::Backspace(m) => {
                if m.alt || m.ctrl {
                    self.delete_word(false)
                } else {
                    self.backspace()
                }
            }
            Key::Delete(m) => {
                if m.alt || m.ctrl {
                    self.delete_word(true)
                } else {
                    self.delete_forward()
                }
            }
            Key::Esc => self.anchor = None,
            Key::Left(m) => self.left(m),
            Key::Right(m) => self.right(m),
            Key::Up(m) => {
                if m.alt {
                    self.page_by(false, m.shift)
                } else {
                    self.vertical(false, m.shift)
                }
            }
            Key::Down(m) => {
                if m.alt {
                    self.page_by(true, m.shift)
                } else {
                    self.vertical(true, m.shift)
                }
            }
            Key::Home(m) => {
                if m.ctrl {
                    self.go(Pos::default(), m.shift)
                } else {
                    self.home(m.shift)
                }
            }
            Key::End(m) => {
                if m.ctrl {
                    self.go(self.text.end(), m.shift)
                } else {
                    self.end(m.shift)
                }
            }
            Key::PageUp(m) => self.page_by(false, m.shift),
            Key::PageDown(m) => self.page_by(true, m.shift),
            Key::Ctrl(c) => return self.command(c, cut_run),
            Key::PasteStart | Key::PasteEnd => {}
        }
        Action::None
    }

    fn command(&mut self, c: u8, cut_run: bool) -> Action {
        match c {
            b'a' => self.home(false),
            b'e' => self.end(false),
            b'd' => self.delete_forward(),
            b'w' => self.delete_word(false),
            b'u' => self.delete_to_line_start(),
            b'x' | b'k' => self.cut(cut_run),
            b'c' => self.copy(),
            b'v' => self.paste_clip(),
            b'z' => self.undo(),
            b'y' => self.redo(),
            b's' | b'o' => {
                self.history.seal();
                return Action::Save { then_quit: false, force: false };
            }
            b'q' => {
                if self.modified() {
                    self.mode = Mode::Ask(Question::SaveBeforeQuit);
                    return Action::None;
                }
                return Action::Quit;
            }
            b'f' => {
                self.mode = Mode::Find { origin: (self.cur, self.anchor) };
                self.found = true;
            }
            b'g' => self.mode = Mode::Line { digits: [0; DIGITS_MAX], len: 0 },
            b'l' => self.screen.invalidate(),
            _ => {}
        }
        Action::None
    }

    fn answer(&mut self, q: Question, k: Key) -> Action {
        let (yes, no) = match k {
            Key::Char(c) => (matches!(c.as_bytes(), b"y" | b"Y"), matches!(c.as_bytes(), b"n" | b"N")),
            _ => (false, false),
        };
        if yes {
            self.mode = Mode::Edit;
            return match q {
                Question::SaveBeforeQuit => Action::Save { then_quit: true, force: false },
                Question::Overwrite { then_quit } => Action::Save { then_quit, force: true },
            };
        }
        if no {
            self.mode = Mode::Edit;
            return match q {
                Question::SaveBeforeQuit => Action::Quit,
                Question::Overwrite { .. } => {
                    self.say(b"not saved");
                    Action::None
                }
            };
        }
        if matches!(k, Key::Esc | Key::Ctrl(b'c')) {
            self.mode = Mode::Edit;
        }
        Action::None
    }

    fn prompt_key(&mut self, k: Key) -> Action {
        match self.mode {
            Mode::Find { origin } => self.find_key(origin, k),
            Mode::Line { digits, len } => self.line_key(digits, len, k),
            _ => Action::None,
        }
    }

    fn find_key(&mut self, origin: Place, k: Key) -> Action {
        match k {
            Key::Char(c) => {
                let c = c.as_bytes();
                if self.term.len() + c.len() <= TERM_MAX && self.term.try_reserve(c.len()).is_ok() {
                    self.term.extend_from_slice(c);
                }
                self.incremental(origin);
            }
            Key::Backspace(_) => {
                let start = glyph::start_before(&self.term, self.term.len());
                self.term.truncate(start);
                self.incremental(origin);
            }
            Key::Ctrl(b'u') => {
                self.term.clear();
                self.incremental(origin);
            }
            Key::Enter | Key::Down(_) | Key::Ctrl(b'f') => self.find_next(true),
            Key::Up(_) => self.find_next(false),
            Key::Esc => self.mode = Mode::Edit,
            Key::Tab | Key::PasteStart | Key::PasteEnd => {}
            /* Anything else ends the search, and does what it does */
            other => {
                self.mode = Mode::Edit;
                return self.edit(other, false);
            }
        }
        Action::None
    }

    fn line_key(&mut self, mut digits: [u8; DIGITS_MAX], mut len: usize, k: Key) -> Action {
        match k {
            Key::Char(c) => {
                if let [d @ b'0'..=b'9'] = c.as_bytes() {
                    if let Some(slot) = digits.get_mut(len) {
                        *slot = *d;
                        len += 1;
                    }
                }
                self.mode = Mode::Line { digits, len };
            }
            Key::Backspace(_) => self.mode = Mode::Line { digits, len: len.saturating_sub(1) },
            Key::Enter => {
                self.mode = Mode::Edit;
                let n = digits
                    .iter()
                    .take(len)
                    .fold(0usize, |n, d| n.saturating_mul(10).saturating_add(usize::from(d - b'0')));
                if n > 0 {
                    let l = (n - 1).min(self.text.last_line());
                    self.go(Pos::new(l, 0), false);
                    self.top = l.saturating_sub(self.page / 2);
                }
            }
            Key::Esc | Key::Ctrl(b'c') | Key::Ctrl(b'g') => self.mode = Mode::Edit,
            _ => {}
        }
        Action::None
    }

    /* ---- moving ---- */

    /// The cursor to `to`: with `shift` the selection stretched to it,
    /// without it gone.
    fn go(&mut self, to: Pos, shift: bool) {
        if shift {
            if self.anchor.is_none() {
                self.anchor = Some(self.cur);
            }
        } else {
            self.anchor = None;
        }
        self.cur = self.text.clamp(to);
        self.want = None;
        self.history.seal();
    }

    fn left(&mut self, m: Mods) {
        /* A selection is left at its start, as macOS does */
        if !m.shift {
            if let Some((start, _)) = self.selection() {
                return self.go(start, false);
            }
        }
        let to = if m.alt || m.ctrl { self.word_left(self.cur) } else { self.text.prev(self.cur) };
        self.go(to, m.shift);
    }

    fn right(&mut self, m: Mods) {
        if !m.shift {
            if let Some((_, end)) = self.selection() {
                return self.go(end, false);
            }
        }
        let to = if m.alt || m.ctrl { self.word_right(self.cur) } else { self.text.next(self.cur) };
        self.go(to, m.shift);
    }

    fn vertical(&mut self, down: bool, shift: bool) {
        let from = match (shift, self.selection()) {
            (false, Some((start, end))) => {
                if down {
                    end
                } else {
                    start
                }
            }
            _ => self.cur,
        };
        let want = self.want.unwrap_or_else(|| self.column(from));
        let to = if down {
            if from.line >= self.text.last_line() {
                Pos::new(from.line, self.text.line(from.line).len())
            } else {
                Pos::new(from.line + 1, self.col_at(from.line + 1, want))
            }
        } else if from.line == 0 {
            Pos::new(0, 0)
        } else {
            Pos::new(from.line - 1, self.col_at(from.line - 1, want))
        };
        self.go(to, shift);
        self.want = Some(want);
    }

    fn page_by(&mut self, down: bool, shift: bool) {
        let n = self.page.max(1);
        let want = self.want.unwrap_or_else(|| self.column(self.cur));
        let line = if down {
            self.top = self.top.saturating_add(n);
            self.cur.line.saturating_add(n).min(self.text.last_line())
        } else {
            self.top = self.top.saturating_sub(n);
            self.cur.line.saturating_sub(n)
        };
        let col = self.col_at(line, want);
        self.go(Pos::new(line, col), shift);
        self.want = Some(want);
    }

    /// The line's first character that is not a blank -- or, there
    /// already, the line's start.
    fn home(&mut self, shift: bool) {
        let line = self.text.line(self.cur.line);
        let indent = line.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
        let col = if self.cur.col == indent { 0 } else { indent };
        self.go(Pos::new(self.cur.line, col), shift);
    }

    fn end(&mut self, shift: bool) {
        let len = self.text.line(self.cur.line).len();
        self.go(Pos::new(self.cur.line, len), shift);
    }

    fn word_left(&self, p: Pos) -> Pos {
        if p.col == 0 {
            return self.text.prev(p);
        }
        let line = self.text.line(p.line);
        let mut i = p.col;
        while i > 0 && !line.get(i - 1).copied().is_some_and(is_word) {
            i = glyph::start_before(line, i);
        }
        while i > 0 && line.get(i - 1).copied().is_some_and(is_word) {
            i = glyph::start_before(line, i);
        }
        Pos::new(p.line, i)
    }

    fn word_right(&self, p: Pos) -> Pos {
        let line = self.text.line(p.line);
        if p.col >= line.len() {
            return self.text.next(p);
        }
        let mut i = p.col;
        while i < line.len() && !line.get(i).copied().is_some_and(is_word) {
            i += glyph::at(line, i).1;
        }
        while i < line.len() && line.get(i).copied().is_some_and(is_word) {
            i += glyph::at(line, i).1;
        }
        Pos::new(p.line, i)
    }

    /// The column `p` is shown at, from the line's start.
    fn column(&self, p: Pos) -> usize {
        let line = self.text.line(p.line);
        let mut col = 0;
        let mut i = 0;
        while i < p.col.min(line.len()) {
            let (g, len) = glyph::at(line, i);
            col += glyph::width(g, col);
            i += len;
        }
        col
    }

    /// Where on line `l` column `want` is: the glyph shown there, or the
    /// line's end.
    fn col_at(&self, l: usize, want: usize) -> usize {
        let line = self.text.line(l);
        let mut col = 0;
        let mut i = 0;
        while i < line.len() {
            let (g, len) = glyph::at(line, i);
            let w = glyph::width(g, col);
            if col + w > want {
                break;
            }
            col += w;
            i += len;
        }
        i
    }

    /* ---- changing ---- */

    /// The text from `a` to `b` made `new`, recorded for undo; the cursor
    /// after it. False, and said why, when it could not be done.
    fn change(&mut self, a: Pos, b: Pos, new: &[u8], kind: Kind) -> bool {
        let before = (self.cur, self.anchor);
        match self.history.change(&mut self.text, a, b, new, kind, before) {
            Ok(end) => {
                self.cur = self.text.clamp(end);
                self.anchor = None;
                self.want = None;
                true
            }
            Err(e) => {
                self.report(e);
                false
            }
        }
    }

    fn report(&mut self, e: Error) {
        self.say(match e {
            Error::NoMemory => b"out of memory: not done",
            Error::TooBig => b"not done: the file would be over 16 MiB",
        });
    }

    /// Text typed: in the selection's place, or at the cursor.
    fn type_in(&mut self, bytes: &[u8], kind: Kind) {
        let (a, b) = self.selection().unwrap_or((self.cur, self.cur));
        self.change(a, b, bytes, kind);
    }

    fn backspace(&mut self) {
        if let Some((a, b)) = self.selection() {
            self.change(a, b, &[], Kind::Other);
            return;
        }
        let from = self.text.prev(self.cur);
        if from != self.cur {
            self.change(from, self.cur, &[], Kind::DeleteBack);
        }
    }

    fn delete_forward(&mut self) {
        if let Some((a, b)) = self.selection() {
            self.change(a, b, &[], Kind::Other);
            return;
        }
        let to = self.text.next(self.cur);
        if to != self.cur {
            self.change(self.cur, to, &[], Kind::DeleteForward);
        }
    }

    fn delete_word(&mut self, forward: bool) {
        if let Some((a, b)) = self.selection() {
            self.change(a, b, &[], Kind::Other);
            return;
        }
        let (a, b) = if forward {
            (self.cur, self.word_right(self.cur))
        } else {
            (self.word_left(self.cur), self.cur)
        };
        if a != b {
            self.change(a, b, &[], Kind::Other);
        }
    }

    fn delete_to_line_start(&mut self) {
        if let Some((a, b)) = self.selection() {
            self.change(a, b, &[], Kind::Other);
            return;
        }
        if self.cur.col > 0 {
            self.change(Pos::new(self.cur.line, 0), self.cur, &[], Kind::Other);
        }
    }

    /// ^X: the selection into the clip; with none, the cursor's line, added
    /// to the clip while cut after cut takes line after line.
    fn cut(&mut self, cut_run: bool) {
        if let Some((a, b)) = self.selection() {
            match self.text.slice(a, b) {
                Ok(bytes) => {
                    self.clip = bytes;
                    self.clip_lines = false;
                    self.change(a, b, &[], Kind::Other);
                }
                Err(e) => self.report(e),
            }
            return;
        }
        let l = self.cur.line;
        let last = l >= self.text.last_line();
        let a = Pos::new(l, 0);
        let b = if last { Pos::new(l, self.text.line(l).len()) } else { Pos::new(l + 1, 0) };
        let mut bytes = match self.text.slice(a, b) {
            Ok(v) => v,
            Err(e) => return self.report(e),
        };
        /* The last line has no newline to take: the clip gets one */
        if last {
            if bytes.try_reserve(1).is_err() {
                return self.report(Error::NoMemory);
            }
            bytes.push(b'\n');
        }
        if cut_run && self.clip_lines {
            if self.clip.try_reserve(bytes.len()).is_err() {
                return self.report(Error::NoMemory);
            }
            self.clip.extend_from_slice(&bytes);
        } else {
            self.clip = bytes;
            self.clip_lines = true;
        }
        if a != b {
            self.change(a, b, &[], Kind::Other);
        }
        self.cur = self.text.clamp(a);
        self.anchor = None;
        self.cut_run = true;
    }

    /// ^C: the selection into the clip; with none, the cursor's line.
    fn copy(&mut self) {
        if let Some((a, b)) = self.selection() {
            match self.text.slice(a, b) {
                Ok(bytes) => {
                    self.clip = bytes;
                    self.clip_lines = false;
                    self.say(b"copied");
                }
                Err(e) => self.report(e),
            }
            return;
        }
        let line = self.text.line(self.cur.line);
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(line.len() + 1).is_err() {
            return self.report(Error::NoMemory);
        }
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
        self.clip = bytes;
        self.clip_lines = true;
        self.say(b"line copied");
    }

    /// ^V: the clip in the selection's place or at the cursor -- or, when
    /// it is lines and nothing is selected, above the cursor's line.
    fn paste_clip(&mut self) {
        if self.clip.is_empty() {
            return self.say(b"nothing to paste: ^X or ^C first");
        }
        let clip = core::mem::take(&mut self.clip);
        if self.clip_lines && self.selection().is_none() {
            let keep = self.cur;
            let at = Pos::new(keep.line, 0);
            if self.change(at, at, &clip, Kind::Other) {
                let lines = clip.iter().filter(|&&b| b == b'\n').count();
                self.cur = self.text.clamp(Pos::new(keep.line + lines, keep.col));
            }
        } else {
            self.type_in(&clip, Kind::Other);
        }
        self.clip = clip;
    }

    fn undo(&mut self) {
        match self.history.undo(&mut self.text) {
            Ok(Some((cur, anchor))) => self.restore(cur, anchor),
            Ok(None) => self.say(b"nothing to undo"),
            Err(e) => self.report(e),
        }
    }

    fn redo(&mut self) {
        match self.history.redo(&mut self.text) {
            Ok(Some((cur, anchor))) => self.restore(cur, anchor),
            Ok(None) => self.say(b"nothing to redo"),
            Err(e) => self.report(e),
        }
    }

    fn restore(&mut self, cur: Pos, anchor: Option<Pos>) {
        self.cur = self.text.clamp(cur);
        self.anchor = anchor.map(|a| self.text.clamp(a));
        self.want = None;
    }

    /* A key of a paste: its text kept, and put in the text a piece at a
       time */
    fn take_paste(&mut self, k: Key) {
        let ch;
        let bytes: &[u8] = match k {
            Key::Char(c) => {
                ch = c;
                ch.as_bytes()
            }
            Key::Enter => b"\n",
            Key::Tab => b"\t",
            _ => return,
        };
        let full = match self.paste.as_mut() {
            Some(buf) => match buf.try_reserve(bytes.len()) {
                Ok(()) => {
                    buf.extend_from_slice(bytes);
                    Some(buf.len() >= PASTE_PIECE)
                }
                Err(_) => None,
            },
            None => Some(false),
        };
        match full {
            Some(true) => self.flush_paste(),
            Some(false) => {}
            None => {
                /* What has come goes in; the rest of it is dropped as it
                   arrives, not taken for keys */
                self.flush_paste();
                self.paste = None;
                self.say(b"out of memory: the rest of the paste was left out");
            }
        }
    }

    fn flush_paste(&mut self) {
        let Some(buf) = self.paste.as_mut() else { return };
        if buf.is_empty() {
            return;
        }
        let mut piece = core::mem::take(buf);
        self.type_in(&piece, Kind::Paste);
        piece.clear();
        if let Some(buf) = self.paste.as_mut() {
            *buf = piece;
        }
    }

    /* ---- finding ---- */

    /// The search term changed: the first match from where the search
    /// began, or back there when there is none.
    fn incremental(&mut self, origin: Place) {
        let from = origin.1.map_or(origin.0, |a| a.min(origin.0));
        match self.search(from, true) {
            Some((start, end, _)) => {
                self.select(start, end);
                self.found = true;
            }
            None => {
                self.restore(origin.0, origin.1);
                self.found = self.term.is_empty();
            }
        }
    }

    fn find_next(&mut self, forward: bool) {
        if self.term.is_empty() {
            return;
        }
        let from = if forward { self.cur } else { self.selection().map_or(self.cur, |(start, _)| start) };
        match self.search(from, forward) {
            Some((start, end, wrapped)) => {
                self.select(start, end);
                self.found = true;
                if wrapped {
                    self.say(b"search wrapped");
                }
            }
            None => self.found = false,
        }
    }

    fn select(&mut self, start: Pos, end: Pos) {
        self.anchor = Some(start);
        self.cur = end;
        self.want = None;
        self.history.seal();
    }

    /// The next match of the term from `from` -- forward, or backward before
    /// it -- round past the end of the text and back to `from`: its start,
    /// its end and whether the search went round. A term with no capital
    /// letter in it matches either case.
    fn search(&self, from: Pos, forward: bool) -> Option<(Pos, Pos, bool)> {
        let term = self.term.as_slice();
        if term.is_empty() {
            return None;
        }
        let fold = !term.iter().any(u8::is_ascii_uppercase);
        let n = self.text.line_count();
        let from = self.text.clamp(from);
        for step in 0..=n {
            let (l, wrapped) = if forward {
                ((from.line + step) % n, from.line + step >= n)
            } else {
                ((from.line + n - step % n) % n, step > from.line)
            };
            let line = self.text.line(l);
            let all = line.len() + 1;
            let found = match (forward, step) {
                (true, 0) => find_in(line, from.col, all, term, fold),
                (true, s) if s == n => find_in(line, 0, from.col, term, fold),
                (true, _) => find_in(line, 0, all, term, fold),
                (false, 0) => rfind_in(line, 0, from.col, term, fold),
                (false, s) if s == n => rfind_in(line, from.col, all, term, fold),
                (false, _) => rfind_in(line, 0, all, term, fold),
            };
            if let Some(i) = found {
                return Some((Pos::new(l, i), Pos::new(l, i + term.len()), wrapped));
            }
        }
        None
    }

    /* ---- drawing ---- */

    /// The view moved to keep the cursor in it: `page` rows of `cols`
    /// columns, a column kept free at the right edge for the mark of a line
    /// that goes on past it.
    fn scroll(&mut self, page: usize, cols: usize) {
        self.top = self.top.min(self.text.line_count().saturating_sub(page));
        if self.cur.line < self.top {
            self.top = self.cur.line;
        }
        if self.cur.line >= self.top + page {
            self.top = self.cur.line + 1 - page;
        }
        let x = self.column(self.cur);
        let line = self.text.line(self.cur.line);
        let w = if self.cur.col < line.len() { glyph::width(glyph::at(line, self.cur.col).0, x).max(1) } else { 1 };
        if x < self.left {
            self.left = x;
        }
        if x + w + 1 > self.left + cols {
            self.left = x + w + 1 - cols;
        }
    }

    /// The status bar: the file's name, whether it is new or changed, and
    /// where the cursor is.
    fn status(&mut self, r: usize, cols: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        let mut right = Vec::new();
        push(&mut right, b"line ")?;
        push_num(&mut right, self.cur.line + 1)?;
        push(&mut right, b"/")?;
        push_num(&mut right, self.text.line_count())?;
        push(&mut right, b", col ")?;
        push_num(&mut right, self.column(self.cur) + 1)?;
        push(&mut right, b" ")?;
        let flags: &[u8] = if self.modified() {
            b" [modified]"
        } else if self.new_file {
            b" [new]"
        } else {
            b""
        };
        let Editor { screen, name, .. } = self;
        let mut row = screen.row();
        row.blank(1, REVERSE)?;
        let room = cols.saturating_sub(right.len() + flags.len() + 2);
        draw_text(&mut row, name, room, REVERSE)?;
        if row.width() + flags.len() <= cols {
            row.text(flags, flags.len(), REVERSE)?;
        }
        let pad = cols.saturating_sub(row.width() + right.len());
        row.blank(pad, REVERSE)?;
        if row.width() + right.len() <= cols {
            row.text(&right, right.len(), REVERSE)?;
        }
        let (w, a) = (row.width(), row.attr());
        screen.commit(r, w, a, out)
    }

    /// The bottom row: the prompt, the question, what was said, or the keys.
    /// Where the cursor goes in it, for a prompt. Never the last column:
    /// writing the screen's last cell may scroll it.
    fn bottom(&mut self, r: usize, cols: usize, out: &mut Vec<u8>) -> Result<usize, Error> {
        let max = cols - 1;
        let Editor { screen, mode, term, found, message, .. } = self;
        let mut row = screen.row();
        let mut cursor = 0;
        match *mode {
            Mode::Find { .. } => {
                draw_text(&mut row, b"Find: ", max, PLAIN)?;
                draw_text(&mut row, term, max, PLAIN)?;
                cursor = row.width();
                if !*found {
                    draw_text(&mut row, b"  [not found]", max, BOLD)?;
                } else if !message.is_empty() {
                    draw_text(&mut row, b"  [", max, PLAIN)?;
                    draw_text(&mut row, message, max, PLAIN)?;
                    draw_text(&mut row, b"]", max, PLAIN)?;
                }
            }
            Mode::Line { digits, len } => {
                draw_text(&mut row, b"Go to line: ", max, PLAIN)?;
                draw_text(&mut row, digits.get(..len).unwrap_or(&[]), max, PLAIN)?;
                cursor = row.width();
            }
            Mode::Ask(q) => {
                let text: &[u8] = match q {
                    Question::SaveBeforeQuit => b"Save the changes? y yes, n no, Esc back ",
                    Question::Overwrite { .. } => b"The file changed on disk since it was read. Write over it? y/n ",
                };
                draw_text(&mut row, text, max, BOLD)?;
                cursor = row.width();
            }
            Mode::Edit if !message.is_empty() => draw_text(&mut row, message, max, PLAIN)?,
            Mode::Edit => {
                for (i, (k, what)) in HELP.iter().enumerate() {
                    let gap = if i == 0 { 0 } else { 2 };
                    if row.width() + gap + k.len() + 1 + what.len() > max {
                        break;
                    }
                    row.blank(gap, PLAIN)?;
                    row.text(k, k.len(), REVERSE)?;
                    row.blank(1, PLAIN)?;
                    row.text(what, what.len(), PLAIN)?;
                }
            }
        }
        let (w, a) = (row.width(), row.attr());
        screen.commit(r, w, a, out)?;
        Ok(cursor.min(max))
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Where the selection is on line `l`, `len` bytes long: from, to, and
/// whether the newline after it is in it.
fn line_selection(sel: Option<(Pos, Pos)>, l: usize, len: usize) -> Option<(usize, usize, bool)> {
    let (s, e) = sel?;
    if l < s.line || l > e.line {
        return None;
    }
    let from = if l == s.line { s.col } else { 0 };
    let (to, newline) = if l == e.line { (e.col, false) } else { (len, true) };
    Some((from, to, newline))
}

/// A line of the text from column `left`, `cols` columns of it; a line that
/// goes on past them ends in a `>`.
fn draw_line(row: &mut Row, line: &[u8], left: usize, cols: usize, sel: Option<(usize, usize, bool)>) -> Result<(), Error> {
    let mut total = 0;
    let mut i = 0;
    while i < line.len() {
        let (g, len) = glyph::at(line, i);
        total += glyph::width(g, total);
        i += len;
    }
    let overflow = total > left + cols;
    let limit = if overflow { cols - 1 } else { cols };

    let mut col = 0;
    let mut i = 0;
    while i < line.len() {
        let (g, len) = glyph::at(line, i);
        let w = glyph::width(g, col);
        let end = col + w;
        let selected = sel.is_some_and(|(s, e, _)| i >= s && i < e);
        let attr = if selected { REVERSE } else { PLAIN };
        if end <= left {
            /* Left of the view, a mark with no width at its edge too: it
               would join whatever is drawn there */
        } else if col < left {
            /* Cut by the left edge: what shows of it is blank */
            row.blank(end - left, attr)?;
        } else if end - left > limit {
            break;
        } else {
            draw_glyph(row, g, line.get(i..i + len).unwrap_or(&[]), w, selected)?;
        }
        col = end;
        i += len;
    }
    if let Some((_, _, true)) = sel {
        if i >= line.len() && row.width() < limit {
            row.blank(1, REVERSE)?;
        }
    }
    if overflow {
        let pad = (cols - 1).saturating_sub(row.width());
        row.blank(pad, PLAIN)?;
        row.text(b">", 1, BOLD)?;
    }
    Ok(())
}

/// Bytes shown glyph by glyph, as far as `max` columns: a name, a prompt, a
/// message -- whatever is in them shown for what it is.
fn draw_text(row: &mut Row, text: &[u8], max: usize, attr: u8) -> Result<(), Error> {
    let mut i = 0;
    while i < text.len() {
        let (g, len) = glyph::at(text, i);
        let w = glyph::width(g, row.width());
        if row.width() + w > max {
            break;
        }
        match g.stand_in() {
            Some(_) => draw_glyph(row, g, &[], w, attr & REVERSE != 0)?,
            None if matches!(g, Glyph::Tab) => row.blank(w, attr)?,
            None => row.text(text.get(i..i + len).unwrap_or(&[]), w, attr)?,
        }
        i += len;
    }
    Ok(())
}

/// One glyph: a character as itself, a tab as blanks, anything else as what
/// stands in for it, in reverse -- underlined as well when selected.
fn draw_glyph(row: &mut Row, g: Glyph, bytes: &[u8], w: usize, selected: bool) -> Result<(), Error> {
    let attr = if selected { REVERSE } else { PLAIN };
    match (g, g.stand_in()) {
        (Glyph::Tab, _) => row.blank(w, attr),
        (_, None) => row.text(bytes, w, attr),
        (_, Some(s)) => {
            let shown = s.get(..w.min(2)).unwrap_or(&[]);
            row.text(shown, shown.len(), if selected { REVERSE | UNDERLINE } else { REVERSE })
        }
    }
}

/// Where the term is in `line`, first, starting at `lo` and before `hi`.
fn find_in(line: &[u8], lo: usize, hi: usize, term: &[u8], fold: bool) -> Option<usize> {
    (lo..hi.min(line.len() + 1)).find(|&i| matches_at(line, i, term, fold))
}

/// Where the term is in `line`, last, starting at `lo` and before `hi`.
fn rfind_in(line: &[u8], lo: usize, hi: usize, term: &[u8], fold: bool) -> Option<usize> {
    (lo..hi.min(line.len() + 1)).rev().find(|&i| matches_at(line, i, term, fold))
}

fn matches_at(line: &[u8], i: usize, term: &[u8], fold: bool) -> bool {
    let Some(part) = line.get(i..i + term.len()) else {
        return false;
    };
    let same = if fold { part.eq_ignore_ascii_case(term) } else { part == term };
    same && glyph::is_boundary(line, i) && glyph::is_boundary(line, i + term.len())
}
