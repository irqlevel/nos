//! A terminal of the test's own: what the editor sends, done as xterm does
//! it -- the cursor addressed, rows erased, attributes set, text written cell
//! by cell, wrapping at the right edge and scrolling at the bottom as a real
//! terminal would -- so that what the editor believes the screen shows can be
//! held to what a terminal would show.
//!
//! Anything sent that is not one of the sequences the editor means to send is
//! an error, and so is what a terminal would do that the editor never means it
//! to: text wrapping past the right edge, the screen scrolling, an erase while
//! the cursor waits to wrap -- which takes the last column's character with it
//! -- or a character the editor shows a stand-in for, sent as it is.

use edit::glyph::{self, Glyph};

pub const REVERSE: u8 = 1;
pub const UNDERLINE: u8 = 2;
pub const BOLD: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub attr: u8,
    /// The right half of a wide character.
    pub tail: bool,
}

impl Cell {
    fn blank() -> Cell {
        Cell { text: String::from(" "), attr: 0, tail: false }
    }
}

enum Parse {
    Ground,
    Esc,
    Csi,
    Utf8 { need: usize },
}

pub struct Vt {
    pub cols: usize,
    pub rows: usize,
    pub grid: Vec<Vec<Cell>>,
    pub x: usize,
    pub y: usize,
    pub cursor_visible: bool,
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub errors: Vec<String>,
    wrap_pending: bool,
    attr: u8,
    state: Parse,
    params: Vec<u8>,
    utf8: Vec<u8>,
}

impl Vt {
    pub fn new(cols: usize, rows: usize) -> Vt {
        Vt {
            cols,
            rows,
            grid: vec![vec![Cell::blank(); cols]; rows],
            x: 0,
            y: 0,
            cursor_visible: true,
            alt_screen: false,
            bracketed_paste: false,
            errors: Vec::new(),
            wrap_pending: false,
            attr: 0,
            state: Parse::Ground,
            params: Vec::new(),
            utf8: Vec::new(),
        }
    }

    /// A new size: what was on it stays where it fits, as a terminal keeps it.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.grid.resize(rows, vec![Cell::blank(); cols]);
        for row in self.grid.iter_mut() {
            row.resize(cols, Cell::blank());
            if let Some(last) = row.last_mut() {
                if !last.tail && glyph_width(&last.text) == 2 {
                    *last = Cell::blank();
                }
            }
            if let Some(first) = row.first_mut() {
                if first.tail {
                    *first = Cell::blank();
                }
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.x = self.x.min(cols.saturating_sub(1));
        self.y = self.y.min(rows.saturating_sub(1));
        self.wrap_pending = false;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    /// Row `y` as text, a wide character's right half left out.
    pub fn row_text(&self, y: usize) -> String {
        self.grid.get(y).map_or_else(String::new, |row| {
            row.iter().filter(|c| !c.tail).map(|c| c.text.as_str()).collect()
        })
    }

    pub fn row_trimmed(&self, y: usize) -> String {
        self.row_text(y).trim_end().to_string()
    }

    fn error(&mut self, what: String) {
        if self.errors.len() < 16 {
            self.errors.push(what);
        }
    }

    fn byte(&mut self, b: u8) {
        match self.state {
            Parse::Ground => match b {
                0x1B => self.state = Parse::Esc,
                b'\r' => {
                    self.x = 0;
                    self.wrap_pending = false;
                }
                0x00..=0x1F | 0x7F => self.error(format!("a control byte 0x{:02x} sent as text", b)),
                0x20..=0x7E => self.put(&[b]),
                _ => {
                    let need = match b {
                        0xC2..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF4 => 4,
                        _ => {
                            self.error(format!("a byte 0x{:02x} that starts no UTF-8 character", b));
                            return;
                        }
                    };
                    self.utf8.clear();
                    self.utf8.push(b);
                    self.state = Parse::Utf8 { need };
                }
            },
            Parse::Utf8 { need } => {
                if b & 0xC0 != 0x80 {
                    self.error(format!("a UTF-8 character cut short by 0x{:02x}", b));
                    self.state = Parse::Ground;
                    self.byte(b);
                    return;
                }
                self.utf8.push(b);
                if self.utf8.len() == need {
                    self.state = Parse::Ground;
                    let ch = std::mem::take(&mut self.utf8);
                    self.put(&ch);
                }
            }
            Parse::Esc => {
                if b == b'[' {
                    self.params.clear();
                    self.state = Parse::Csi;
                } else {
                    self.error(format!("ESC {:?}: not a sequence the editor sends", b as char));
                    self.state = Parse::Ground;
                }
            }
            Parse::Csi => match b {
                0x30..=0x3F => self.params.push(b),
                0x40..=0x7E => {
                    self.state = Parse::Ground;
                    let params = std::mem::take(&mut self.params);
                    self.csi(&params, b);
                }
                _ => {
                    self.error(format!("0x{:02x} inside a control sequence", b));
                    self.state = Parse::Ground;
                }
            },
        }
    }

    fn csi(&mut self, params: &[u8], fin: u8) {
        let text = String::from_utf8_lossy(params).into_owned();
        let nums: Vec<usize> = text.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        match (fin, text.as_str()) {
            (b'H', _) => {
                let r = nums.first().copied().filter(|&n| n > 0).unwrap_or(1);
                let c = nums.get(1).copied().filter(|&n| n > 0).unwrap_or(1);
                if r > self.rows || c > self.cols {
                    self.error(format!("the cursor sent to row {} column {}, off a {}x{} screen", r, c, self.cols, self.rows));
                }
                self.y = (r - 1).min(self.rows - 1);
                self.x = (c - 1).min(self.cols - 1);
                self.wrap_pending = false;
            }
            (b'K', "" | "0") => {
                if self.wrap_pending {
                    self.error(format!("an erase on row {} while the cursor waits to wrap: the last column goes", self.y + 1));
                }
                let (x, y) = (self.x, self.y);
                self.split_wide(x, y);
                for cell in self.grid[y][x..].iter_mut() {
                    *cell = Cell::blank();
                }
            }
            (b'J', "2") => {
                for row in self.grid.iter_mut() {
                    for cell in row.iter_mut() {
                        *cell = Cell::blank();
                    }
                }
            }
            (b'm', _) => {
                for n in &nums {
                    match n {
                        0 => self.attr = 0,
                        1 => self.attr |= BOLD,
                        4 => self.attr |= UNDERLINE,
                        7 => self.attr |= REVERSE,
                        _ => self.error(format!("SGR {}: not one the editor uses", n)),
                    }
                }
            }
            (b'h' | b'l', "?25") => self.cursor_visible = fin == b'h',
            (b'h' | b'l', "?1049") => self.alt_screen = fin == b'h',
            (b'h' | b'l', "?2004") => self.bracketed_paste = fin == b'h',
            _ => self.error(format!("CSI {} {}: not a sequence the editor sends", text, fin as char)),
        }
    }

    /* One character, at the cursor */
    fn put(&mut self, ch: &[u8]) {
        let (g, len) = glyph::at(ch, 0);
        let w = match g {
            Glyph::Char { width } if len == ch.len() => usize::from(width),
            _ => {
                self.error(format!("{:02x?} sent as it is: the editor shows a stand-in for it", ch));
                1
            }
        };
        if w == 0 {
            /* A mark that combines with the character before it */
            if self.x == 0 && !self.wrap_pending {
                self.error(format!("a combining mark {:02x?} at a row's start", ch));
                return;
            }
            let x = if self.wrap_pending { self.x } else { self.x - 1 };
            let x = if self.grid[self.y][x].tail { x.saturating_sub(1) } else { x };
            self.grid[self.y][x].text.push_str(&String::from_utf8_lossy(ch));
            return;
        }
        if self.wrap_pending {
            self.error(format!("text past the right edge of row {}: a terminal wraps it", self.y + 1));
            self.wrap_pending = false;
            self.x = 0;
            self.newline();
        }
        if w == 2 && self.x + 1 >= self.cols {
            self.error(format!("a wide character at the last column of row {}: a terminal wraps it", self.y + 1));
            return;
        }
        let (x, y) = (self.x, self.y);
        self.split_wide(x, y);
        if w == 2 {
            self.split_wide(x + 1, y);
        }
        self.grid[y][x] = Cell { text: String::from_utf8_lossy(ch).into_owned(), attr: self.attr, tail: false };
        if w == 2 {
            self.grid[y][x + 1] = Cell { text: String::new(), attr: self.attr, tail: true };
        }
        if x + w >= self.cols {
            self.x = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.x = x + w;
        }
    }

    /* Writing over half of a wide character blanks its other half */
    fn split_wide(&mut self, x: usize, y: usize) {
        let row = &mut self.grid[y];
        if x >= row.len() {
            return;
        }
        if row[x].tail && x > 0 {
            row[x - 1] = Cell::blank();
            row[x] = Cell::blank();
        } else if x + 1 < row.len() && row[x + 1].tail {
            row[x + 1] = Cell::blank();
        }
    }

    fn newline(&mut self) {
        if self.y + 1 < self.rows {
            self.y += 1;
        } else {
            self.error(String::from("the screen scrolled"));
            self.grid.remove(0);
            self.grid.push(vec![Cell::blank(); self.cols]);
        }
    }
}

fn glyph_width(text: &str) -> usize {
    match glyph::at(text.as_bytes(), 0).0 {
        Glyph::Char { width } => usize::from(width),
        _ => 1,
    }
}
