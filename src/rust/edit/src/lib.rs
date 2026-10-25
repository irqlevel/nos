//! A text editor for a terminal: what `edit <file>` runs (the edit module,
//! src/rust/modules/edit), with nothing of the kernel in it -- keys in as a
//! terminal sends them, the screen out as escape sequences, and the file's
//! bytes in and out through whoever runs it.
//!
//! - [`keys`] turns what a terminal sends into keys: xterm's sequences, as
//!   Terminal.app, iTerm2 and Linux's terminals send them, pastes bracketed;
//! - [`Editor`] is the file and everything done to it: the cursor and the
//!   selection, the undo history, the clipboard, finding and going to a
//!   line, and what to ask before quitting -- and the frame that shows it,
//!   a row sent only when it changed.
//!
//! The text is the file's bytes, line by line, and comes out as it went in:
//! a file read and written back untouched is the same file, whatever is in
//! it -- invalid UTF-8, control characters, CR LF line ends, no newline at
//! the end. What is not text is shown for what it is (`^X`, a byte in hex)
//! rather than sent to the terminal.
//!
//! It runs in the kernel, so nothing here allocates without being able to
//! fail: an edit takes the memory it needs first, and one that cannot have
//! it is not made -- the text, the cursor and the history stay as they were
//! -- rather than panic the machine.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod editor;
pub mod glyph;
mod history;
pub mod keys;
mod screen;
mod text;

pub use editor::{Action, Editor, Size};
pub use keys::{Decoder, Key, Mods};
pub use text::{Pos, MAX_BYTES};

/// Why something was not done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NoMemory,
    /// The file would pass `MAX_BYTES`.
    TooBig,
}

/// What a terminal is told as the editor starts: to show a screen of its
/// own -- what was on the terminal comes back when the editor leaves -- and
/// to bracket what is pasted, so that a paste is text and not keys.
pub const ENTER: &[u8] = b"\x1b[?1049h\x1b[?2004h";
/// What puts the terminal back as it was.
pub const LEAVE: &[u8] = b"\x1b[0m\x1b[?2004l\x1b[?25h\x1b[?1049l";
