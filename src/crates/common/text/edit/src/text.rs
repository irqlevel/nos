//! The file being edited: its bytes, a line at a time.
//!
//! A file is its lines joined by newlines, and a newline after the last one
//! or not: that is all the structure there is, so any file comes back out
//! as it went in -- CR LF line ends, NUL bytes, invalid UTF-8, no newline at
//! the end. Every change is one call, `replace`, which takes the memory it
//! needs before it touches anything: when there is none to be had, the text
//! is as it was.

use alloc::vec::Vec;

use crate::glyph;
use crate::Error;

/// The most a file may hold: what is edited is held in kernel memory, and
/// with its undo history beside it.
pub const MAX_BYTES: usize = 16 << 20;

/// A place in the text: a line, and a byte in it -- where a glyph starts, or
/// the line's end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub const fn new(line: usize, col: usize) -> Pos {
        Pos { line, col }
    }
}

pub struct Text {
    /// Never empty, and no line has a newline in it.
    lines: Vec<Vec<u8>>,
    /// The file ends with a newline.
    final_newline: bool,
    /// What `to_bytes` makes: every line, the newlines between them and the
    /// last one.
    bytes: usize,
}

/// A copy of `data`, or the memory for one refused.
pub fn copy(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(data.len()).map_err(|_| Error::NoMemory)?;
    v.extend_from_slice(data);
    Ok(v)
}

/// Where text ends that starts at `at`: past its last newline, if it has
/// any.
pub fn end_of(at: Pos, text: &[u8]) -> Pos {
    match text.iter().rposition(|&b| b == b'\n') {
        None => Pos::new(at.line, at.col.saturating_add(text.len())),
        Some(last) => {
            let breaks = text.iter().filter(|&&b| b == b'\n').count();
            Pos::new(at.line.saturating_add(breaks), text.len() - last - 1)
        }
    }
}

impl Text {
    /// The file's content, as it was read.
    pub fn from_bytes(data: &[u8]) -> Result<Text, Error> {
        if data.len() > MAX_BYTES {
            return Err(Error::TooBig);
        }
        let (body, final_newline) = match data.split_last() {
            Some((b'\n', body)) => (body, true),
            _ => (data, false),
        };
        let count = body.iter().filter(|&&b| b == b'\n').count() + 1;
        let mut lines = Vec::new();
        lines.try_reserve_exact(count).map_err(|_| Error::NoMemory)?;
        for line in body.split(|&b| b == b'\n') {
            lines.push(copy(line)?);
        }
        Ok(Text { lines, final_newline, bytes: data.len() })
    }

    /// A file that does not exist yet: one empty line, and a newline after
    /// whatever is written in it.
    pub fn new_file() -> Result<Text, Error> {
        let mut lines = Vec::new();
        lines.try_reserve_exact(1).map_err(|_| Error::NoMemory)?;
        lines.push(Vec::new());
        Ok(Text { lines, final_newline: true, bytes: 1 })
    }

    /// The file's bytes, as they would be written.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        out.try_reserve_exact(self.bytes).map_err(|_| Error::NoMemory)?;
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push(b'\n');
            }
            out.extend_from_slice(line);
        }
        if self.final_newline {
            out.push(b'\n');
        }
        Ok(out)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The line's bytes: none past the last line.
    pub fn line(&self, i: usize) -> &[u8] {
        self.lines.get(i).map_or(&[], |l| l.as_slice())
    }

    pub fn last_line(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    /// The end of the last line.
    pub fn end(&self) -> Pos {
        let last = self.last_line();
        Pos::new(last, self.line(last).len())
    }

    /// The place nearest `p` that is one: in the text, and where a glyph
    /// starts, or before the glyph `p` is inside of.
    pub fn clamp(&self, p: Pos) -> Pos {
        if p.line > self.last_line() {
            return self.end();
        }
        let line = self.line(p.line);
        let mut col = p.col.min(line.len());
        while col > 0 && !glyph::is_boundary(line, col) {
            col -= 1;
        }
        Pos::new(p.line, col)
    }

    /// The glyph after `p`, or the start of the next line: `p` itself at
    /// the end of the text.
    pub fn next(&self, p: Pos) -> Pos {
        let line = self.line(p.line);
        if p.col < line.len() {
            Pos::new(p.line, p.col + glyph::at(line, p.col).1)
        } else if p.line < self.last_line() {
            Pos::new(p.line + 1, 0)
        } else {
            p
        }
    }

    /// The glyph before `p`, or the end of the line before: `p` itself at
    /// the start of the text.
    pub fn prev(&self, p: Pos) -> Pos {
        if p.col > 0 {
            Pos::new(p.line, glyph::start_before(self.line(p.line), p.col))
        } else if p.line > 0 {
            Pos::new(p.line - 1, self.line(p.line - 1).len())
        } else {
            p
        }
    }

    /// The bytes from `a` to `b`, a newline between each two lines; `a` is
    /// not after `b`.
    pub fn slice(&self, a: Pos, b: Pos) -> Result<Vec<u8>, Error> {
        let (a, b) = (self.fit(a), self.fit(b));
        let mut out = Vec::new();
        out.try_reserve_exact(self.span(a, b)).map_err(|_| Error::NoMemory)?;
        if a.line == b.line {
            out.extend_from_slice(self.line(a.line).get(a.col..b.col).unwrap_or(&[]));
            return Ok(out);
        }
        out.extend_from_slice(self.line(a.line).get(a.col..).unwrap_or(&[]));
        for l in a.line + 1..b.line {
            out.push(b'\n');
            out.extend_from_slice(self.line(l));
        }
        out.push(b'\n');
        out.extend_from_slice(self.line(b.line).get(..b.col).unwrap_or(&[]));
        Ok(out)
    }

    /// How many bytes are between `a` and `b`, newlines included.
    pub fn span(&self, a: Pos, b: Pos) -> usize {
        let (a, b) = (self.fit(a), self.fit(b));
        if b <= a {
            return 0;
        }
        if a.line == b.line {
            return b.col - a.col;
        }
        let mut n = self.line(a.line).len() - a.col + 1;
        for l in a.line + 1..b.line {
            n += self.line(l).len() + 1;
        }
        n + b.col
    }

    /// Puts `new` where the text from `a` to `b` was, and says where it
    /// ends. Refused, with nothing changed, when the file would pass
    /// `MAX_BYTES` or the memory for the change cannot be had: everything it
    /// needs is taken first, and only then is anything moved.
    pub fn replace(&mut self, a: Pos, b: Pos, new: &[u8]) -> Result<Pos, Error> {
        let (a, b) = (self.fit(a), self.fit(b));
        let (a, b) = if b < a { (b, a) } else { (a, b) };
        let gone = self.span(a, b);
        let bytes = self.bytes - gone + new.len();
        if bytes > MAX_BYTES {
            return Err(Error::TooBig);
        }
        let end = end_of(a, new);

        let Some(nl) = new.iter().position(|&c| c == b'\n') else {
            /* One line comes of it: the start of a's, the new text, the rest
               of b's */
            if a.line == b.line {
                splice(&mut self.lines[a.line], a.col, b.col, new)?;
            } else {
                let tail_len = self.line(b.line).len() - b.col;
                let first = &mut self.lines[a.line];
                let want = (a.col + new.len() + tail_len).saturating_sub(first.len());
                first.try_reserve(want).map_err(|_| Error::NoMemory)?;
                let (head, rest) = self.lines.split_at_mut(b.line);
                let first = &mut head[a.line];
                first.truncate(a.col);
                first.extend_from_slice(new);
                first.extend_from_slice(rest[0].get(b.col..).unwrap_or(&[]));
                self.lines.drain(a.line + 1..=b.line);
            }
            self.bytes = bytes;
            return Ok(end);
        };

        /* Lines of their own: the new text's after its first newline, the
           last of them taking the rest of b's line */
        let first_piece = &new[..nl];
        let mut added: Vec<Vec<u8>> = Vec::new();
        let pieces = new[nl + 1..].split(|&c| c == b'\n');
        added.try_reserve_exact(pieces.clone().count()).map_err(|_| Error::NoMemory)?;
        let tail = self.line(b.line).get(b.col..).unwrap_or(&[]);
        let mut pieces = pieces.peekable();
        while let Some(piece) = pieces.next() {
            if pieces.peek().is_some() {
                added.push(copy(piece)?);
            } else {
                let mut last = Vec::new();
                last.try_reserve_exact(piece.len() + tail.len()).map_err(|_| Error::NoMemory)?;
                last.extend_from_slice(piece);
                last.extend_from_slice(tail);
                added.push(last);
            }
        }
        self.lines.try_reserve(added.len()).map_err(|_| Error::NoMemory)?;
        let first = &mut self.lines[a.line];
        let want = (a.col + first_piece.len()).saturating_sub(first.len());
        first.try_reserve(want).map_err(|_| Error::NoMemory)?;

        /* Nothing below allocates */
        first.truncate(a.col);
        first.extend_from_slice(first_piece);
        self.lines.drain(a.line + 1..=b.line);
        let at = a.line + 1;
        let count = added.len();
        self.lines.resize_with(self.lines.len() + count, Vec::new);
        self.lines[at..].rotate_right(count);
        for (slot, line) in self.lines[at..at + count].iter_mut().zip(added) {
            *slot = line;
        }
        self.bytes = bytes;
        Ok(end)
    }

    /// Checks what the text promises: a line at least, no newline in any,
    /// the size it says. For the tests.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.lines.is_empty() {
            return Err("text: no line");
        }
        if self.lines.iter().any(|l| l.contains(&b'\n')) {
            return Err("text: a newline inside a line");
        }
        let n: usize = self.lines.iter().map(Vec::len).sum::<usize>()
            + self.lines.len() - 1
            + usize::from(self.final_newline);
        if n != self.bytes {
            return Err("text: the size it keeps is not its size");
        }
        Ok(())
    }

    /* `p` brought into the text, as bytes: no further than its line's end */
    fn fit(&self, p: Pos) -> Pos {
        if p.line > self.last_line() {
            return self.end();
        }
        Pos::new(p.line, p.col.min(self.line(p.line).len()))
    }
}

/* The bytes of `line` from `from` to `to` made `new`, without allocating
   but for the room, which is taken first */
fn splice(line: &mut Vec<u8>, from: usize, to: usize, new: &[u8]) -> Result<(), Error> {
    let gone = to - from;
    if new.len() > gone {
        let grow = new.len() - gone;
        line.try_reserve(grow).map_err(|_| Error::NoMemory)?;
        line.resize(line.len() + grow, 0);
        line[from..].rotate_right(grow);
    } else {
        line.drain(from + new.len()..to);
    }
    line[from..from + new.len()].copy_from_slice(new);
    Ok(())
}
