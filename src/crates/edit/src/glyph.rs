//! What a line's bytes look like on a terminal: a character and how many
//! columns it takes, or something that is not text, shown for what it is
//! rather than sent to the terminal as it is.

/// How far apart tab stops are, in columns: where a terminal has them.
pub const TAB: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    /// A character, and its width in columns: 1, 2 for a wide one (CJK,
    /// most emoji), 0 for one that combines with the character before it.
    Char { width: u8 },
    /// A tab: as far as the next tab stop.
    Tab,
    /// A control character, shown as `^X`.
    Control(u8),
    /// A byte that is not part of a UTF-8 character, shown in hex.
    Invalid(u8),
    /// A character that has no look of its own, or would change how the rest
    /// of the line is shown: C1 controls, zero-width and direction marks, a
    /// byte order mark. Shown as `?`.
    Hidden,
}

impl Glyph {
    /// What is shown in place of one that is not plain text, `None` for a
    /// character, which is shown as itself, and a tab, as spaces.
    pub fn stand_in(self) -> Option<[u8; 2]> {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        match self {
            Glyph::Char { .. } | Glyph::Tab => None,
            Glyph::Control(c) => Some([b'^', if c == 0x7F { b'?' } else { (c & 0x1F) | 0x40 }]),
            Glyph::Invalid(b) => Some([HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xF)]]),
            Glyph::Hidden => Some([b'?', 0]),
        }
    }
}

/// The glyph at byte `i` of `line`, and how many bytes it takes: at least
/// one, so that a loop over a line always moves on. Past the end of the line
/// it is a `Hidden` of one byte.
pub fn at(line: &[u8], i: usize) -> (Glyph, usize) {
    let Some(&b) = line.get(i) else {
        return (Glyph::Hidden, 1);
    };
    let len = match b {
        b'\t' => return (Glyph::Tab, 1),
        0x00..=0x1F | 0x7F => return (Glyph::Control(b), 1),
        0x20..=0x7E => return (Glyph::Char { width: 1 }, 1),
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return (Glyph::Invalid(b), 1),
    };
    /* The standard library's check: no overlong form, no surrogate, nothing
       past U+10FFFF */
    let seq = line.get(i..i.saturating_add(len));
    match seq.map(core::str::from_utf8) {
        Some(Ok(s)) => match s.chars().next() {
            Some(c) => (classify(u32::from(c)), len),
            None => (Glyph::Invalid(b), 1),
        },
        _ => (Glyph::Invalid(b), 1),
    }
}

/// Columns the glyph takes when it starts at column `col`.
pub fn width(g: Glyph, col: usize) -> usize {
    match g {
        Glyph::Char { width } => usize::from(width),
        Glyph::Tab => TAB - col % TAB,
        Glyph::Control(_) | Glyph::Invalid(_) => 2,
        Glyph::Hidden => 1,
    }
}

/// Where the glyph that ends at byte `col` of `line` starts: the glyphs are
/// read from the start of the line, so the one that ends here is the longest
/// that does -- a byte that is the last of a character is never a glyph of
/// its own. `col` is a glyph boundary and above 0.
pub fn start_before(line: &[u8], col: usize) -> usize {
    let col = col.min(line.len());
    for back in (1..=4).rev() {
        let Some(start) = col.checked_sub(back) else { continue };
        if at(line, start).1 == back {
            return start;
        }
    }
    col.saturating_sub(1)
}

/// Whether byte `col` of `line` is where a glyph starts, or the end of the
/// line: where a cursor may be. A glyph of more than one byte is a lead byte
/// and continuation bytes, so one starts at every byte that is not a
/// continuation byte; and a continuation byte is inside one only when a
/// character that covers it starts at most three bytes before.
pub fn is_boundary(line: &[u8], col: usize) -> bool {
    match line.get(col) {
        None => col == line.len(),
        Some(&b) if b & 0xC0 != 0x80 => true,
        Some(_) => (1..=3).all(|back| match col.checked_sub(back) {
            Some(start) => {
                let (g, len) = at(line, start);
                matches!(g, Glyph::Invalid(_)) || len <= back
            }
            None => true,
        }),
    }
}

fn classify(cp: u32) -> Glyph {
    if within(cp, HIDDEN) {
        Glyph::Hidden
    } else if within(cp, COMBINING) {
        Glyph::Char { width: 0 }
    } else if within(cp, WIDE) {
        Glyph::Char { width: 2 }
    } else {
        Glyph::Char { width: 1 }
    }
}

fn within(cp: u32, ranges: &[(u32, u32)]) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                core::cmp::Ordering::Less
            } else if lo > cp {
                core::cmp::Ordering::Greater
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/* C1 controls, zero-width and direction marks, separators that end a line
   on a terminal of their own accord, the byte order mark, interlinear
   annotation, tags */
const HIDDEN: &[(u32, u32)] = &[
    (0x0080, 0x009F),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0xE0000, 0xE007F),
];

/* Marks that combine with the character before them: the common blocks,
   not all of Unicode -- a terminal has its own idea of the rest */
const COMBINING: &[(u32, u32)] = &[
    (0x0300, 0x036F),
    (0x0483, 0x0489),
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    (0x0610, 0x061A),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DC),
    (0x06DF, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06EA, 0x06ED),
    (0x0900, 0x0902),
    (0x093C, 0x093C),
    (0x0941, 0x0948),
    (0x094D, 0x094D),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E47, 0x0E4E),
    (0x1AB0, 0x1AFF),
    (0x1DC0, 0x1DFF),
    (0x20D0, 0x20FF),
    (0x302A, 0x302D),
    (0x3099, 0x309A),
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
    (0xE0100, 0xE01EF),
];

/* East Asian wide and fullwidth, and the emoji shown wide by default */
const WIDE: &[(u32, u32)] = &[
    (0x1100, 0x115F),
    (0x231A, 0x231B),
    (0x2329, 0x232A),
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x2E80, 0x303E),
    (0x3041, 0x33FF),
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xA000, 0xA4CF),
    (0xA960, 0xA97F),
    (0xAC00, 0xD7A3),
    (0xF900, 0xFAFF),
    (0xFE10, 0xFE19),
    (0xFE30, 0xFE6F),
    (0xFF00, 0xFF60),
    (0xFFE0, 0xFFE6),
    (0x16FE0, 0x16FE4),
    (0x17000, 0x18CFF),
    (0x1B000, 0x1B2FF),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F200, 0x1F202),
    (0x1F210, 0x1F23B),
    (0x1F240, 0x1F248),
    (0x1F250, 0x1F251),
    (0x1F260, 0x1F265),
    (0x1F300, 0x1F64F),
    (0x1F680, 0x1F6FF),
    (0x1F7E0, 0x1F7EB),
    (0x1F90C, 0x1F9FF),
    (0x1FA70, 0x1FAFF),
    (0x20000, 0x2FFFD),
    (0x30000, 0x3FFFD),
];
