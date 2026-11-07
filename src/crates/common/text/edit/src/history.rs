//! Undo and redo: every change to the text as what it replaced and what with,
//! so that it can be taken back and made again.
//!
//! A change is recorded by the call that makes it, so there is no change
//! without its record and no record without its change: `History::change`
//! takes the memory for both first, and makes neither when it cannot. Keys
//! typed one after another are one step, as are a run of deletes and a
//! paste; anything else -- a move, a save, an undo -- ends the step.

use alloc::vec::Vec;

use crate::text::{self, Pos, Text};
use crate::Error;

/// What the history holds at most, in bytes of text, and in steps: past
/// either, the oldest go.
const BYTES_MAX: usize = 8 << 20;
const STEPS_MAX: usize = 10_000;

/// What made a change, for which ones join the one before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A character typed: joins the characters typed just before it.
    Typing,
    /// The character before the cursor deleted: joins the deletes before.
    DeleteBack,
    /// The character after the cursor deleted: joins the deletes before.
    DeleteForward,
    /// A piece of a paste: joins the rest of it.
    Paste,
    /// Anything else: a step of its own.
    Other,
}

/// Where the cursor and the selection's other end were.
pub type Place = (Pos, Option<Pos>);

struct Change {
    at: Pos,
    removed: Vec<u8>,
    inserted: Vec<u8>,
    before: Place,
    after: Place,
    kind: Kind,
}

impl Change {
    fn size(&self) -> usize {
        self.removed.len() + self.inserted.len()
    }
}

pub struct History {
    /// Done, then undone: `changes[..done]` are in the text, the rest can
    /// be made again.
    changes: Vec<Change>,
    done: usize,
    bytes: usize,
    /// `done` when the file was last saved; `None` once that state is gone
    /// from the history, or was never in it.
    saved: Option<usize>,
    /// The next change is a step of its own.
    sealed: bool,
    /// Steps let go of from the front, past what the history may hold.
    dropped: usize,
}

impl History {
    pub const fn new() -> History {
        History { changes: Vec::new(), done: 0, bytes: 0, saved: Some(0), sealed: true, dropped: 0 }
    }

    /// The next change starts a step of its own.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// The text is what was last saved: the state the history is in now.
    pub fn mark_saved(&mut self) {
        self.saved = Some(self.done);
        self.sealed = true;
    }

    /// Whether the text differs from what was last saved -- as far as the
    /// history knows: one that went past the saved state and came back by
    /// other edits still counts as different.
    pub fn modified(&self) -> bool {
        self.saved != Some(self.done)
    }

    /// Whether everything done since the file was read can still be undone.
    pub fn complete(&self) -> bool {
        self.dropped == 0
    }

    /// Replaces the text from `a` to `b` with `new`, and records it: where
    /// the new text ends, or why nothing was done.
    pub fn change(&mut self, text: &mut Text, a: Pos, b: Pos, new: &[u8], kind: Kind, before: Place) -> Result<Pos, Error> {
        let (a, b) = if b < a { (b, a) } else { (a, b) };
        if !self.sealed && self.done == self.changes.len() {
            if let Some(end) = self.join(text, a, b, new, kind)? {
                self.sealed = false;
                return Ok(end);
            }
        }
        let removed = text.slice(a, b)?;
        let inserted = text::copy(new)?;
        self.changes.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let end = text.replace(a, b, new)?;

        /* What was undone cannot be made again after this */
        if self.saved.is_some_and(|s| s > self.done) {
            self.saved = None;
        }
        for c in self.changes.drain(self.done..) {
            self.bytes -= c.size();
        }
        let change = Change { at: a, removed, inserted, before, after: (end, None), kind };
        self.bytes += change.size();
        self.changes.push(change);
        self.done += 1;
        self.sealed = matches!(kind, Kind::Other);
        self.trim();
        Ok(end)
    }

    /// Takes the last change back: where the cursor and selection were
    /// before it, or `None` with nothing to undo.
    pub fn undo(&mut self, text: &mut Text) -> Result<Option<Place>, Error> {
        let Some(c) = self.done.checked_sub(1).and_then(|i| self.changes.get(i)) else {
            return Ok(None);
        };
        let end = text::end_of(c.at, &c.inserted);
        text.replace(c.at, end, &c.removed)?;
        let before = c.before;
        self.done -= 1;
        self.sealed = true;
        Ok(Some(before))
    }

    /// Makes the last change undone again: where the cursor and selection
    /// were after it, or `None` with nothing to redo.
    pub fn redo(&mut self, text: &mut Text) -> Result<Option<Place>, Error> {
        let Some(c) = self.changes.get(self.done) else {
            return Ok(None);
        };
        let end = text::end_of(c.at, &c.removed);
        text.replace(c.at, end, &c.inserted)?;
        let after = c.after;
        self.done += 1;
        self.sealed = true;
        Ok(Some(after))
    }

    /// Checks what the history promises, for the tests.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.done > self.changes.len() {
            return Err("history: more done than there are changes");
        }
        if self.bytes != self.changes.iter().map(Change::size).sum::<usize>() {
            return Err("history: the size it keeps is not its size");
        }
        if self.saved.is_some_and(|s| s > self.changes.len()) {
            return Err("history: saved at a step it does not have");
        }
        Ok(())
    }

    /* This change made part of the last, when it continues it: typing on,
       deleting on, pasting on. `None` when it does not. */
    fn join(&mut self, text: &mut Text, a: Pos, b: Pos, new: &[u8], kind: Kind) -> Result<Option<Pos>, Error> {
        let Some(last) = self.done.checked_sub(1).and_then(|i| self.changes.get_mut(i)) else {
            return Ok(None);
        };
        if last.kind != kind {
            return Ok(None);
        }
        let last_end = text::end_of(last.at, &last.inserted);
        let end = match kind {
            Kind::Typing | Kind::Paste if a == b && a == last_end && !last.inserted.is_empty() => {
                last.inserted.try_reserve(new.len()).map_err(|_| Error::NoMemory)?;
                let end = text.replace(a, b, new)?;
                last.inserted.extend_from_slice(new);
                last.after = (end, None);
                self.bytes += new.len();
                end
            }
            Kind::DeleteBack if new.is_empty() && b == last.at && last.inserted.is_empty() => {
                let gone = text.slice(a, b)?;
                last.removed.try_reserve(gone.len()).map_err(|_| Error::NoMemory)?;
                let end = text.replace(a, b, &[])?;
                /* In front of what was deleted before it */
                let n = gone.len();
                last.removed.extend_from_slice(&gone);
                last.removed.rotate_right(n);
                last.at = a;
                last.after = (end, None);
                self.bytes += n;
                end
            }
            Kind::DeleteForward if new.is_empty() && a == last.at && last.inserted.is_empty() => {
                let gone = text.slice(a, b)?;
                last.removed.try_reserve(gone.len()).map_err(|_| Error::NoMemory)?;
                let end = text.replace(a, b, &[])?;
                last.removed.extend_from_slice(&gone);
                last.after = (end, None);
                self.bytes += gone.len();
                end
            }
            _ => return Ok(None),
        };
        /* The step the file was saved at is not that step any more */
        if self.saved == Some(self.done) {
            self.saved = None;
        }
        Ok(Some(end))
    }

    /* The oldest steps let go of, past what the history may hold */
    fn trim(&mut self) {
        let mut drop = 0;
        let mut bytes = self.bytes;
        while drop < self.done && (bytes > BYTES_MAX || self.changes.len() - drop > STEPS_MAX) {
            bytes -= self.changes.get(drop).map_or(0, Change::size);
            drop += 1;
        }
        if drop == 0 {
            return;
        }
        self.changes.drain(..drop);
        self.bytes = bytes;
        self.done -= drop;
        self.saved = self.saved.and_then(|s| s.checked_sub(drop));
        self.dropped += drop;
    }
}
