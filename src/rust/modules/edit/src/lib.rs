#![no_std]

//! `edit <file>`: a text editor, for the person at an SSH session with a
//! terminal -- the editor itself is src/rust/edit, which has no kernel in
//! it; this is its seat in the kernel: the command, the file read and
//! written, and the session's terminal, its keys in and its screen out.
//!
//!     insmod /lib/modules/edit.ko        (the image's /etc/rc does)
//!     edit /etc/rc                       (ssh -t <host> edit /etc/rc)
//!
//! A save is `kcore::fs::write`, which writes the new content whole and
//! syncs it before it takes the old one's place: a full disk or a crash in
//! the middle leaves the old file, never half of the new one. A file that
//! changed on disk since it was read -- `sshd allow` adding a key while the
//! keys are edited -- is asked about before it is written over. A session
//! that ends with changes not saved -- the connection gone -- leaves them in
//! `<file>.save`, never over a file that is there.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::fmt::Write;

use edit::{Action, Decoder, Editor, Key, Size};
use kcore::cmd::{Command, Output, Terminal};
use kcore::error::Error;
use kcore::time::Duration;

const HELP: &str = "edit <file> - a text editor, in an ssh session with a terminal (ssh -t); ^S saves, ^Q quits";

/// How long input may pause inside a key's sequence before what came is
/// taken for what it is: an ESC with nothing after it is the Esc key.
const ESC_WAIT_MS: u64 = 30;
/// How often the terminal is looked at -- a resize -- while nothing is typed.
const POLL_MS: u64 = 200;
/// The size of a terminal whose client did not say (`ssh -tt` from a script).
const DEFAULT_SIZE: Size = Size { cols: 80, rows: 24 };
/// Typing taken at a time.
const KEYS: usize = 256;
/// Where unsaved changes go when the session ends under them: `<file>.save`,
/// or the first of `.save.1` .. `.save.9` not taken.
const SAVE_NAMES: usize = 10;

struct Edit {
    _cmd: Command,
}

impl kmod::Module for Edit {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("edit", HELP, run)?;
    Ok(Box::new(Edit { _cmd: cmd }))
}

kmod::module!(name: "edit", init: init);

/// One file being edited: the editor, and what the file was when it was
/// read or last written -- what a save holds the disk to.
struct Seat<'p> {
    path: &'p str,
    base: Option<Vec<u8>>,
    editor: Editor,
    decoder: Decoder,
}

/// How an editing session ended.
enum End {
    Quit,
    /// The session is over -- its connection gone -- with the editor open.
    Gone,
}

fn run(args: &str, out: &mut Output) {
    let path = args.trim();
    if path.is_empty() || path.contains(char::is_whitespace) {
        let _ = writeln!(out, "usage: edit <file>");
        return;
    }
    if !path.starts_with('/') {
        let _ = writeln!(out, "edit: the file's whole path, from /: edit /{}", path);
        return;
    }
    let Some(term) = out.terminal() else {
        let _ = writeln!(out, "edit: needs a terminal to draw on -- from an ssh session with one: ssh -t <host> edit {}", path);
        return;
    };
    if kcore::fs::list(path).is_ok() {
        let _ = writeln!(out, "edit: {} is a directory", path);
        return;
    }
    let base = match kcore::fs::read(path, edit::MAX_BYTES) {
        Ok(data) => Some(data),
        Err(Error::NotFound) => {
            let dir = &path[..path.rfind('/').unwrap_or(0).max(1)];
            if kcore::fs::list(dir).is_err() {
                let _ = writeln!(out, "edit: no directory {} to make {} in", dir, path);
                return;
            }
            None
        }
        Err(Error::InvalidValue) => {
            let _ = writeln!(out, "edit: {} is over {} MiB, more than edit holds", path, edit::MAX_BYTES >> 20);
            return;
        }
        Err(e) => {
            let _ = writeln!(out, "edit: cannot read {}: {}", path, e);
            return;
        }
    };
    let editor = match Editor::open(path.as_bytes(), base.as_deref()) {
        Ok(e) => e,
        Err(_) => {
            let _ = writeln!(out, "edit: no memory for {}", path);
            return;
        }
    };
    let mut seat = Seat { path, base, editor, decoder: Decoder::new() };

    out.write_bytes(edit::ENTER);
    let end = seat.session(out, term);
    out.write_bytes(edit::LEAVE);
    if let End::Gone = end {
        seat.keep_unsaved();
    }
}

impl Seat<'_> {
    /// Keys in and frames out until the editor quits or the session ends.
    fn session(&mut self, out: &mut Output, mut term: Terminal) -> End {
        let mut keys = [0u8; KEYS];
        let mut frame = Vec::new();
        let mut draw = true;
        loop {
            /* A resize -- any, even back to the size the screen was drawn
               at, which a terminal does not keep the screen through -- is a
               screen drawn again from nothing */
            if let Some(now) = out.terminal() {
                if now.changes != term.changes {
                    self.editor.redraw();
                    draw = true;
                }
                term = now;
            }
            if draw {
                frame.clear();
                if self.editor.render(size_of(term), &mut frame).is_ok() {
                    out.write_bytes(&frame);
                } else {
                    /* No memory for the frame: what was sent of it is not
                       known, so the next is drawn whole */
                    self.editor.redraw();
                }
                draw = false;
            }

            let wait = if self.decoder.pending() { ESC_WAIT_MS } else { POLL_MS };
            match out.read_input(&mut keys, Duration::from_millis(wait)) {
                None => return End::Gone,
                Some(0) => {
                    if self.decoder.pending() || self.decoder.in_paste() {
                        self.decoder.pause();
                        if self.drain() {
                            return End::Quit;
                        }
                        self.editor.idle();
                        draw = true;
                    }
                }
                Some(n) => {
                    for &b in keys.get(..n).unwrap_or(&[]) {
                        self.decoder.push(b);
                        if self.drain() {
                            return End::Quit;
                        }
                    }
                    draw = true;
                }
            }
        }
    }

    /// The keys the decoder has, each done: true once the editor quits.
    fn drain(&mut self) -> bool {
        while let Some(k) = self.decoder.next() {
            if self.key(k) {
                return true;
            }
        }
        false
    }

    fn key(&mut self, k: Key) -> bool {
        match self.editor.key(k) {
            Action::None => false,
            Action::Quit => true,
            Action::Save { then_quit, force } => self.save(then_quit, force),
        }
    }

    /// The file written: true when it was, and the editor is to quit after.
    fn save(&mut self, then_quit: bool, force: bool) -> bool {
        if !force && self.changed_on_disk() {
            self.editor.ask_overwrite(then_quit);
            return false;
        }
        let data = match self.editor.contents() {
            Ok(d) => d,
            Err(_) => {
                self.editor.say(b"no memory to save in: not saved");
                return false;
            }
        };
        match kcore::fs::write(self.path, &data) {
            Ok(()) => {
                self.editor.saved();
                self.base = Some(data);
                then_quit
            }
            Err(_) => {
                self.editor.say(b"the file could not be written: not saved -- a full disk, or one mounted read-only?");
                false
            }
        }
    }

    /// Whether the file on disk is not what was read, or last written: made,
    /// removed or changed since -- or not to be read now, which is asked
    /// about too rather than written over blind.
    fn changed_on_disk(&self) -> bool {
        match (kcore::fs::read(self.path, edit::MAX_BYTES), &self.base) {
            (Ok(now), Some(base)) => now != *base,
            (Err(Error::NotFound), None) => false,
            _ => true,
        }
    }

    /// The session ended under changes not saved: they go to `<file>.save`,
    /// or the first `.save.N` not taken -- never over a file that is there.
    fn keep_unsaved(&self) {
        if !self.editor.modified() {
            return;
        }
        let Ok(data) = self.editor.contents() else {
            kcore::trace!(0, "edit: the session ended with {} changed, and no memory to keep the changes in", self.path);
            return;
        };
        for n in 0..SAVE_NAMES {
            let name = if n == 0 { format!("{}.save", self.path) } else { format!("{}.save.{}", self.path, n) };
            match kcore::fs::create(&name, &data) {
                Ok(()) => {
                    kcore::trace!(0, "edit: the session ended with {} changed: the changes are in {}", self.path, name);
                    return;
                }
                Err(Error::Busy) => continue,
                Err(e) => {
                    kcore::trace!(0, "edit: the session ended with {} changed, and {} could not be written: {}", self.path, name, e);
                    return;
                }
            }
        }
        kcore::trace!(0, "edit: the session ended with {} changed, and {}.save to .save.9 are all taken", self.path, self.path);
    }
}

/// The terminal's size as the editor takes it: a client that did not say
/// gets the size terminals have always had.
fn size_of(t: Terminal) -> Size {
    if t.cols == 0 || t.rows == 0 {
        return DEFAULT_SIZE;
    }
    Size {
        cols: usize::try_from(t.cols).unwrap_or(usize::MAX),
        rows: usize::try_from(t.rows).unwrap_or(usize::MAX),
    }
}
