//! The terminal's screen as it was last drawn, so that a frame sends only
//! the rows that changed: a key typed costs a row, not the screen -- which
//! over an SSH connection across the world is the difference between an
//! editor and a wait.
//!
//! A row is kept as the bytes that drew it, escape sequences and all: two
//! rows the same bytes are the same on the screen. A row is drawn from its
//! first column, and what it does not fill is erased -- unless it fills the
//! last column, where the cursor waits to wrap and an erase there would take
//! the last character with it.

use alloc::vec::Vec;

use crate::Error;

/// How text is shown: a set of these.
pub const PLAIN: u8 = 0;
pub const REVERSE: u8 = 1;
pub const UNDERLINE: u8 = 2;
pub const BOLD: u8 = 4;

pub struct Screen {
    rows: Vec<Vec<u8>>,
    scratch: Vec<u8>,
    cols: usize,
    height: usize,
    /// What the terminal shows is known: false, and the next frame clears
    /// it and draws every row.
    valid: bool,
}

/// A row being drawn: what it says and how much of the width it fills.
pub struct Row<'a> {
    buf: &'a mut Vec<u8>,
    width: usize,
    attr: u8,
}

pub fn push(buf: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    buf.try_reserve(bytes.len()).map_err(|_| Error::NoMemory)?;
    buf.extend_from_slice(bytes);
    Ok(())
}

/// A number in decimal, as escape sequences take them.
pub fn push_num(buf: &mut Vec<u8>, mut n: usize) -> Result<(), Error> {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 || i == 0 {
            break;
        }
    }
    push(buf, &digits[i..])
}

impl Row<'_> {
    /// Columns filled.
    pub fn width(&self) -> usize {
        self.width
    }

    /// How what comes next would be shown: what `Screen::commit` resets.
    pub fn attr(&self) -> u8 {
        self.attr
    }

    /// Text that takes `cols` columns, shown as `attr` says.
    pub fn text(&mut self, bytes: &[u8], cols: usize, attr: u8) -> Result<(), Error> {
        self.set(attr)?;
        push(self.buf, bytes)?;
        self.width += cols;
        Ok(())
    }

    /// `n` spaces.
    pub fn blank(&mut self, n: usize, attr: u8) -> Result<(), Error> {
        self.set(attr)?;
        self.buf.try_reserve(n).map_err(|_| Error::NoMemory)?;
        self.buf.extend(core::iter::repeat_n(b' ', n));
        self.width += n;
        Ok(())
    }

    fn set(&mut self, attr: u8) -> Result<(), Error> {
        if attr == self.attr {
            return Ok(());
        }
        self.attr = attr;
        push(self.buf, b"\x1b[0")?;
        if attr & REVERSE != 0 {
            push(self.buf, b";7")?;
        }
        if attr & UNDERLINE != 0 {
            push(self.buf, b";4")?;
        }
        if attr & BOLD != 0 {
            push(self.buf, b";1")?;
        }
        push(self.buf, b"m")
    }
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub const fn new() -> Screen {
        Screen { rows: Vec::new(), scratch: Vec::new(), cols: 0, height: 0, valid: false }
    }

    /// The next frame draws everything: what the terminal shows is not
    /// known any more.
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    /// Starts a frame of `rows` rows of `cols` columns: a size other than
    /// the last one's, or a screen not known, is cleared and drawn whole.
    pub fn begin(&mut self, cols: usize, rows: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        /* The cursor hidden while it moves about the screen */
        push(out, b"\x1b[?25l")?;
        if self.valid && cols == self.cols && rows == self.height {
            return Ok(());
        }
        if self.rows.len() < rows {
            self.rows.try_reserve(rows - self.rows.len()).map_err(|_| Error::NoMemory)?;
            self.rows.resize_with(rows, Vec::new);
        }
        self.rows.truncate(rows);
        for r in self.rows.iter_mut() {
            r.clear();
        }
        self.cols = cols;
        self.height = rows;
        push(out, b"\x1b[0m\x1b[H\x1b[2J")?;
        self.valid = true;
        Ok(())
    }

    /// A row to draw, empty.
    pub fn row(&mut self) -> Row<'_> {
        self.scratch.clear();
        Row { buf: &mut self.scratch, width: 0, attr: PLAIN }
    }

    /// The row drawn into `row()` is row `r` (from 0): sent when it is not
    /// what the screen has there. Its width is `width`, as the row said.
    pub fn commit(&mut self, r: usize, width: usize, attr: u8, out: &mut Vec<u8>) -> Result<(), Error> {
        if attr != PLAIN {
            push(&mut self.scratch, b"\x1b[0m")?;
        }
        if width < self.cols {
            push(&mut self.scratch, b"\x1b[K")?;
        }
        let Some(old) = self.rows.get_mut(r) else {
            return Ok(());
        };
        if *old == self.scratch {
            return Ok(());
        }
        push(out, b"\x1b[")?;
        push_num(out, r + 1)?;
        push(out, b";1H")?;
        push(out, &self.scratch)?;
        core::mem::swap(old, &mut self.scratch);
        Ok(())
    }

    /// Ends the frame with the cursor at column `x`, row `y` (from 0).
    pub fn end(&mut self, x: usize, y: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        push(out, b"\x1b[")?;
        push_num(out, y + 1)?;
        push(out, b";")?;
        push_num(out, x + 1)?;
        push(out, b"H\x1b[?25h")
    }
}
