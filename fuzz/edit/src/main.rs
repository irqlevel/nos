//! The editor (src/rust/edit) on the host, run the way the edit module runs
//! it -- bytes through the key decoder, the editor's actions answered, its
//! frames drawn on a terminal of the test's own (`vt`) -- and held to:
//!
//! - `cases`: sessions of keys, each with what it must leave in the file, at
//!   the cursor, in the clipboard and on the screen -- the Mac's keys, the
//!   selection, undo, finding, the questions before a quit or a write over a
//!   file that changed, pastes, and what is not text shown for what it is;
//! - `roundtrip`: files of random bytes read and saved untouched, which must
//!   come back byte for byte;
//! - `fuzz`: sessions of random keys, pastes, garbage, pauses and resizes on
//!   random files, after every one of which the editor must keep its
//!   promises (`Editor::check`) and the terminal must have been sent nothing
//!   it would wrap, scroll or not know. Now and then what was drawn a row at
//!   a time must be what a full redraw draws, and undoing everything must
//!   give back the file as it was read -- and redoing it, the file as it was.
//!
//! A failure says its seed and session, which make it again.
//!
//!     edit-test [all|cases|roundtrip|fuzz] [--seed N] [--sessions N] [--seconds S]

mod vt;

use std::time::{Duration, Instant};

use edit::{Action, Decoder, Editor, Key, Pos, Size};
use vt::Vt;

/* ---- the keys, as a terminal sends them ---- */

const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const RIGHT: &str = "\x1b[C";
const LEFT: &str = "\x1b[D";
const S_UP: &str = "\x1b[1;2A";
const S_DOWN: &str = "\x1b[1;2B";
const S_RIGHT: &str = "\x1b[1;2C";
const S_LEFT: &str = "\x1b[1;2D";
const OPT_LEFT: &str = "\x1bb";
const OPT_RIGHT: &str = "\x1bf";
const DEL: &str = "\x1b[3~";
const BS: &str = "\x7f";
const ENTER: &str = "\r";
const ESC: &str = "\x1b";
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

fn ctrl(c: char) -> String {
    char::from(c as u8 - b'a' + 1).to_string()
}

const SIZE: Size = Size { cols: 80, rows: 24 };
const NAME: &[u8] = b"/etc/test.conf";

/* ---- a session ---- */

struct Session {
    ed: Editor,
    dec: Decoder,
    vt: Vt,
    size: Size,
    /// The file as it is on the disk, and as the editor last read or wrote it.
    disk: Option<Vec<u8>>,
    base: Option<Vec<u8>>,
    /// What the editor had when it opened: what undoing everything gives.
    initial: Vec<u8>,
    quit: bool,
    log: Vec<String>,
}

impl Session {
    fn open(content: Option<&[u8]>, size: Size) -> Session {
        let ed = Editor::open(NAME, content).expect("open");
        let initial = ed.contents().expect("contents");
        let mut vt = Vt::new(size.cols.max(1), size.rows.max(1));
        vt.feed(edit::ENTER);
        Session {
            ed,
            dec: Decoder::new(),
            vt,
            size,
            disk: content.map(<[u8]>::to_vec),
            base: content.map(<[u8]>::to_vec),
            initial,
            quit: false,
            log: Vec::new(),
        }
    }

    fn text(content: &str) -> Session {
        Session::open(Some(content.as_bytes()), SIZE)
    }

    /// Bytes as the terminal sends them; an ESC left alone at the end is the
    /// key, as the pause after it says.
    fn feed(&mut self, bytes: &[u8]) {
        if self.log.len() < 4096 {
            self.log.push(format!("feed {:?}", String::from_utf8_lossy(bytes)));
        }
        for &b in bytes {
            self.dec.push(b);
            while let Some(k) = self.dec.next() {
                self.key(k);
            }
        }
        if self.dec.pending() && bytes.last() == Some(&0x1B) {
            self.pause();
        }
    }

    fn keys(&mut self, keys: &[&str]) {
        for k in keys {
            self.feed(k.as_bytes());
        }
    }

    fn pause(&mut self) {
        self.dec.pause();
        while let Some(k) = self.dec.next() {
            self.key(k);
        }
        self.ed.idle();
    }

    fn key(&mut self, k: Key) {
        if self.quit {
            return;
        }
        match self.ed.key(k) {
            Action::None => {}
            Action::Quit => self.quit = true,
            Action::Save { then_quit, force } => {
                if !force && self.disk != self.base {
                    self.ed.ask_overwrite(then_quit);
                    return;
                }
                let data = self.ed.contents().expect("contents");
                self.disk = Some(data.clone());
                self.base = Some(data);
                self.ed.saved();
                assert!(!self.ed.modified(), "modified right after a save");
                if then_quit {
                    self.quit = true;
                }
            }
        }
    }

    fn render(&mut self) {
        let mut out = Vec::new();
        self.ed.render(self.size, &mut out).expect("render");
        assert!(!out.contains(&b'\n'), "a frame sent a line feed");
        self.vt.feed(&out);
    }

    /// The window resized, as the session tells the module -- the size and
    /// that it changed, which the module answers with a whole redraw: a
    /// terminal's screen made smaller and then as large again, between two
    /// frames, has lost what it had, whatever the size says.
    fn resize(&mut self, size: Size) {
        self.size = size;
        self.vt.resize(size.cols.max(1), size.rows.max(1));
        self.ed.redraw();
    }

    fn contents(&self) -> Vec<u8> {
        self.ed.contents().expect("contents")
    }

    fn row(&self, y: usize) -> String {
        self.vt.row_trimmed(y)
    }

    /// What the editor promises, and what the terminal saw.
    fn check(&self) -> Result<(), String> {
        self.ed.check().map_err(String::from)?;
        if !self.vt.errors.is_empty() {
            return Err(format!("the terminal: {:?}", self.vt.errors));
        }
        Ok(())
    }

    /// What was drawn a row at a time is what a full redraw draws, the
    /// cursor where it puts it.
    fn consistent(&mut self) -> Result<(), String> {
        self.render();
        let cols = self.size.cols.clamp(1, 1000);
        let rows = self.size.rows.clamp(1, 500);
        if self.vt.cols != cols || self.vt.rows != rows {
            return Ok(());
        }
        self.ed.redraw();
        let mut out = Vec::new();
        self.ed.render(self.size, &mut out).expect("render");
        let mut fresh = Vt::new(cols, rows);
        fresh.feed(&out);
        for y in 0..rows {
            if self.vt.grid[y] != fresh.grid[y] {
                return Err(format!(
                    "row {} drawn a row at a time is {:?}, redrawn {:?} (the terminal said {:?})",
                    y + 1,
                    self.vt.row_text(y),
                    fresh.row_text(y),
                    self.vt.errors
                ));
            }
        }
        if (self.vt.x, self.vt.y) != (fresh.x, fresh.y) {
            return Err(format!("the cursor at {:?}, redrawn at {:?}", (self.vt.x, self.vt.y), (fresh.x, fresh.y)));
        }
        self.vt.feed(&out);
        Ok(())
    }
}

/* ---- cases ---- */

macro_rules! expect {
    ($cond:expr, $($what:tt)+) => {
        if !$cond {
            return Err(format!($($what)+));
        }
    };
}

fn eq(got: &[u8], want: &[u8], what: &str) -> Result<(), String> {
    expect!(got == want, "{}: {:?}, wanted {:?}", what, String::from_utf8_lossy(got), String::from_utf8_lossy(want));
    Ok(())
}

fn case_roundtrip_untouched() -> Result<(), String> {
    let files: &[&[u8]] = &[
        b"", b"\n", b"\n\n", b"a", b"a\n", b"a\r\nb\r\n", b"\xff\xfe\x00x", b"tab\there\n", "привет, мир\n".as_bytes(),
        b"no newline at the end", b"\x1b[31mred\x1b[0m\n",
    ];
    for f in files {
        let mut s = Session::open(Some(f), SIZE);
        s.feed(ctrl('s').as_bytes());
        expect!(s.disk.as_deref() == Some(*f), "{:?} saved as {:?}", f, s.disk);
        expect!(!s.ed.modified(), "{:?}: modified after a save", f);
    }
    Ok(())
}

fn case_typing() -> Result<(), String> {
    let mut s = Session::text("hello\n");
    s.feed(b"X");
    eq(&s.contents(), b"Xhello\n", "typed at the start")?;
    expect!(s.ed.modified(), "not modified after typing");
    s.feed(ctrl('e').as_bytes());
    s.feed(b"!");
    eq(&s.contents(), b"Xhello!\n", "^E then typed")?;
    s.feed(ctrl('a').as_bytes());
    s.feed(b">");
    eq(&s.contents(), b">Xhello!\n", "^A then typed")
}

fn case_smart_home() -> Result<(), String> {
    let mut s = Session::text("    foo\n");
    s.feed(ctrl('e').as_bytes());
    s.feed(ctrl('a').as_bytes());
    expect!(s.ed.cursor() == Pos::new(0, 4), "^A went to {:?}, not the indent", s.ed.cursor());
    s.feed(ctrl('a').as_bytes());
    expect!(s.ed.cursor() == Pos::new(0, 0), "^A again went to {:?}, not the start", s.ed.cursor());
    Ok(())
}

fn case_lines() -> Result<(), String> {
    let mut s = Session::text("ab\n");
    s.keys(&[RIGHT, ENTER]);
    eq(&s.contents(), b"a\nb\n", "Enter")?;
    s.feed(BS.as_bytes());
    eq(&s.contents(), b"ab\n", "Backspace at a line's start")?;
    s.feed(DEL.as_bytes());
    eq(&s.contents(), b"a\n", "Delete")?;
    s.feed(ctrl('a').as_bytes());
    s.feed(ctrl('d').as_bytes());
    eq(&s.contents(), b"\n", "^D")
}

fn case_selection() -> Result<(), String> {
    let mut s = Session::text("hello world\n");
    s.keys(&[S_RIGHT, S_RIGHT, S_RIGHT, S_RIGHT, S_RIGHT]);
    expect!(s.ed.selection() == Some((Pos::new(0, 0), Pos::new(0, 5))), "selected {:?}", s.ed.selection());
    s.feed(ctrl('x').as_bytes());
    eq(&s.contents(), b" world\n", "^X of a selection")?;
    eq(s.ed.clipboard(), b"hello", "the clip")?;
    s.feed(ctrl('e').as_bytes());
    s.feed(ctrl('v').as_bytes());
    eq(&s.contents(), b" worldhello\n", "^V")?;
    s.feed(ctrl('z').as_bytes());
    eq(&s.contents(), b" world\n", "^Z of the paste")?;
    Ok(())
}

fn case_typing_over_selection() -> Result<(), String> {
    let mut s = Session::text("abc\n");
    s.keys(&[S_RIGHT, S_RIGHT, S_RIGHT, "q", "r"]);
    eq(&s.contents(), b"qr\n", "typed over the selection")?;
    s.feed(ctrl('z').as_bytes());
    eq(&s.contents(), b"abc\n", "one ^Z")?;
    expect!(s.ed.selection() == Some((Pos::new(0, 0), Pos::new(0, 3))), "the selection back as {:?}", s.ed.selection());
    Ok(())
}

fn case_cut_lines() -> Result<(), String> {
    let mut s = Session::text("1\n2\n3\n");
    s.feed(ctrl('x').as_bytes());
    s.feed(ctrl('x').as_bytes());
    eq(&s.contents(), b"3\n", "two ^X")?;
    eq(s.ed.clipboard(), b"1\n2\n", "the clip after two ^X")?;
    s.feed(ctrl('v').as_bytes());
    eq(&s.contents(), b"1\n2\n3\n", "^V of lines")?;
    expect!(s.ed.cursor() == Pos::new(2, 0), "the cursor at {:?} after ^V", s.ed.cursor());
    /* A cut that does not follow a cut starts the clip over */
    s.keys(&[UP, &ctrl('x')]);
    eq(s.ed.clipboard(), b"2\n", "the clip after a move and ^X")?;
    /* The last line, which has no newline of its own */
    let mut s = Session::text("a\nb");
    s.keys(&[DOWN, &ctrl('x')]);
    eq(&s.contents(), b"a\n", "^X of the last line")?;
    eq(s.ed.clipboard(), b"b\n", "the last line's clip")?;
    Ok(())
}

fn case_copy_line() -> Result<(), String> {
    let mut s = Session::text("a\nb\n");
    s.keys(&[DOWN, &ctrl('c'), &ctrl('v')]);
    eq(&s.contents(), b"a\nb\nb\n", "^C ^V of a line")?;
    expect!(s.ed.cursor() == Pos::new(2, 0), "the cursor at {:?}", s.ed.cursor());
    Ok(())
}

fn case_undo_redo() -> Result<(), String> {
    let mut s = Session::text("abc\n");
    s.feed(b"XYZ");
    eq(&s.contents(), b"XYZabc\n", "typed")?;
    s.feed(ctrl('z').as_bytes());
    eq(&s.contents(), b"abc\n", "^Z")?;
    expect!(!s.ed.modified(), "modified after undoing back to the file");
    s.feed(ctrl('y').as_bytes());
    eq(&s.contents(), b"XYZabc\n", "^Y")?;
    s.feed(ctrl('y').as_bytes());
    eq(s.ed.message(), b"nothing to redo", "the message")?;
    let mut s = Session::text("");
    s.keys(&["ab", ENTER, "cd"]);
    eq(&s.contents(), b"ab\ncd", "typed")?;
    for want in [&b"ab\n"[..], b"ab", b""] {
        s.feed(ctrl('z').as_bytes());
        eq(&s.contents(), want, "^Z")?;
    }
    Ok(())
}

fn case_words() -> Result<(), String> {
    let mut s = Session::text("foo bar baz\n");
    s.feed(OPT_RIGHT.as_bytes());
    expect!(s.ed.cursor() == Pos::new(0, 3), "Option-Right to {:?}", s.ed.cursor());
    s.feed(OPT_RIGHT.as_bytes());
    expect!(s.ed.cursor() == Pos::new(0, 7), "Option-Right to {:?}", s.ed.cursor());
    s.feed(OPT_LEFT.as_bytes());
    expect!(s.ed.cursor() == Pos::new(0, 4), "Option-Left to {:?}", s.ed.cursor());
    s.feed(ctrl('w').as_bytes());
    eq(&s.contents(), b"bar baz\n", "^W")?;
    s.keys(&[&ctrl('e'), OPT_LEFT, &ctrl('u')]);
    eq(&s.contents(), b"baz\n", "^U")?;
    /* Option-Delete, as a terminal that sends Option as Meta sends it */
    s.keys(&[&ctrl('e'), "\x1b\x7f"]);
    eq(&s.contents(), b"\n", "Option-Delete")
}

fn case_find() -> Result<(), String> {
    let mut s = Session::text("one two three two\n");
    s.keys(&[&ctrl('f'), "two"]);
    expect!(s.ed.selection() == Some((Pos::new(0, 4), Pos::new(0, 7))), "found {:?}", s.ed.selection());
    s.feed(ENTER.as_bytes());
    expect!(s.ed.selection() == Some((Pos::new(0, 14), Pos::new(0, 17))), "next {:?}", s.ed.selection());
    s.feed(ENTER.as_bytes());
    expect!(s.ed.selection() == Some((Pos::new(0, 4), Pos::new(0, 7))), "round to {:?}", s.ed.selection());
    eq(s.ed.message(), b"search wrapped", "the message")?;
    s.feed(UP.as_bytes());
    expect!(s.ed.selection() == Some((Pos::new(0, 14), Pos::new(0, 17))), "back to {:?}", s.ed.selection());
    s.feed(ESC.as_bytes());
    expect!(!s.ed.prompting(), "still finding after Esc");
    s.feed(ctrl('x').as_bytes());
    eq(&s.contents(), b"one two three \n", "^X of what was found")?;
    /* No capital: either case; a capital: that case */
    let mut s = Session::text("Two two\n");
    s.keys(&[&ctrl('f'), "two"]);
    expect!(s.ed.selection() == Some((Pos::new(0, 0), Pos::new(0, 3))), "found {:?}", s.ed.selection());
    s.keys(&[&ctrl('u'), "two", &ctrl('u'), "Two", ENTER]);
    expect!(s.ed.selection() == Some((Pos::new(0, 0), Pos::new(0, 3))), "Two found again {:?}", s.ed.selection());
    /* Not found: the cursor where it was */
    let mut s = Session::text("abc\n");
    s.keys(&[RIGHT, &ctrl('f'), "zz"]);
    expect!(s.ed.cursor() == Pos::new(0, 1) && s.ed.selection().is_none(), "moved to {:?}", s.ed.cursor());
    s.render();
    expect!(s.row(23).contains("[not found]"), "the prompt is {:?}", s.row(23));
    Ok(())
}

fn case_goto_line() -> Result<(), String> {
    let text: String = (1..=10).map(|i| format!("line {}\n", i)).collect();
    let mut s = Session::text(&text);
    s.keys(&[&ctrl('g'), "7", ENTER]);
    expect!(s.ed.cursor() == Pos::new(6, 0), "went to {:?}", s.ed.cursor());
    s.keys(&[&ctrl('g'), "999", ENTER]);
    expect!(s.ed.cursor() == Pos::new(9, 0), "went to {:?}", s.ed.cursor());
    Ok(())
}

fn case_quit() -> Result<(), String> {
    let mut s = Session::text("a\n");
    s.feed(ctrl('q').as_bytes());
    expect!(s.quit, "^Q with nothing changed did not quit");

    let mut s = Session::text("a\n");
    s.keys(&["x", &ctrl('q')]);
    expect!(!s.quit && s.ed.prompting(), "^Q with changes did not ask");
    s.feed(b"n");
    expect!(s.quit, "n did not quit");
    eq(s.disk.as_deref().unwrap_or(b""), b"a\n", "the file after n")?;

    let mut s = Session::text("a\n");
    s.keys(&["x", &ctrl('q'), "y"]);
    expect!(s.quit, "y did not quit");
    eq(s.disk.as_deref().unwrap_or(b""), b"xa\n", "the file after y")?;

    let mut s = Session::text("a\n");
    s.keys(&["x", &ctrl('q'), ESC]);
    expect!(!s.quit && !s.ed.prompting() && s.ed.modified(), "Esc did not go back");
    /* A pasted y is not an answer */
    s.keys(&[&ctrl('q'), PASTE_START, "y", PASTE_END]);
    expect!(!s.quit && s.ed.prompting(), "a pasted y answered");
    Ok(())
}

fn case_changed_on_disk() -> Result<(), String> {
    let mut s = Session::text("a\n");
    s.feed(b"x");
    s.disk = Some(b"other\n".to_vec());
    s.feed(ctrl('s').as_bytes());
    expect!(s.ed.prompting(), "a save over a changed file did not ask");
    s.feed(b"n");
    eq(s.disk.as_deref().unwrap_or(b""), b"other\n", "the file after n")?;
    s.keys(&[&ctrl('s'), "y"]);
    eq(s.disk.as_deref().unwrap_or(b""), b"xa\n", "the file after y")?;
    Ok(())
}

fn case_paste() -> Result<(), String> {
    let mut s = Session::text("\n");
    s.keys(&[PASTE_START, "a\r\nb\x13c\x1b[31md\tz", PASTE_END]);
    eq(&s.contents(), b"a\nbc[31md\tz\n", "pasted")?;
    expect!(s.disk.as_deref() == Some(b"\n"), "a ^S in a paste saved");
    s.feed(ctrl('z').as_bytes());
    eq(&s.contents(), b"\n", "one ^Z takes a paste")?;
    let mut s = Session::text("hello\n");
    s.keys(&[S_RIGHT, S_RIGHT, S_RIGHT, S_RIGHT, S_RIGHT, PASTE_START, "bye", PASTE_END]);
    eq(&s.contents(), b"bye\n", "a paste over a selection")?;
    /* A paste of more than a piece goes in whole */
    let big: String = "0123456789abcdef\n".repeat(10_000);
    let mut s = Session::text("");
    s.keys(&[PASTE_START, &big, PASTE_END]);
    eq(&s.contents(), big.as_bytes(), "a big paste")?;
    Ok(())
}

fn case_moves() -> Result<(), String> {
    let mut s = Session::text("abcdef\nab\nabcdef\n");
    s.feed(ctrl('e').as_bytes());
    s.feed(DOWN.as_bytes());
    expect!(s.ed.cursor() == Pos::new(1, 2), "Down to {:?}", s.ed.cursor());
    s.feed(DOWN.as_bytes());
    expect!(s.ed.cursor() == Pos::new(2, 6), "Down again to {:?}: the column not kept", s.ed.cursor());
    let mut s = Session::text("ab\ncd\n");
    s.feed(S_DOWN.as_bytes());
    expect!(s.ed.selection() == Some((Pos::new(0, 0), Pos::new(1, 0))), "Shift-Down selected {:?}", s.ed.selection());
    s.feed(LEFT.as_bytes());
    expect!(s.ed.cursor() == Pos::new(0, 0) && s.ed.selection().is_none(), "Left left {:?}", s.ed.cursor());
    s.keys(&[S_RIGHT, ESC]);
    expect!(s.ed.selection().is_none(), "Esc left a selection");
    /* Every way a terminal has of sending them */
    let mut s = Session::text("abc def\n");
    for (seq, want) in [
        ("\x1bOC", 1), ("\x1b[4~", 7), ("\x1b[1~", 0), ("\x1bOF", 7), ("\x1bOH", 0),
        ("\x1b[1;5C", 3), ("\x1b[1;3C", 7), ("\x1b[1;3D", 4), ("\x1b[F", 7), ("\x1b[H", 0),
    ] {
        s.feed(seq.as_bytes());
        expect!(s.ed.cursor() == Pos::new(0, want), "{:?} went to {:?}, not column {}", seq, s.ed.cursor(), want);
    }
    s.keys(&[S_LEFT, S_UP]);
    Ok(())
}

fn case_screen() -> Result<(), String> {
    let mut s = Session::open(Some(b"hello\nworld\n"), Size { cols: 40, rows: 10 });
    s.render();
    expect!(s.row(0) == "hello" && s.row(1) == "world", "rows {:?} {:?}", s.row(0), s.row(1));
    expect!(s.row(8).contains("/etc/test.conf") && s.row(8).contains("line 1/2, col 1"), "status {:?}", s.row(8));
    expect!(s.row(9).starts_with("^S Save"), "help {:?}", s.row(9));
    s.feed(b"x");
    s.render();
    expect!(s.row(8).contains("[modified]"), "status {:?}", s.row(8));
    expect!((s.vt.x, s.vt.y) == (1, 0), "the cursor at {:?}", (s.vt.x, s.vt.y));

    let mut s = Session::open(Some(b"a\x01b\xffc\td\n"), Size { cols: 40, rows: 10 });
    s.render();
    expect!(s.row(0) == "a^AbFFc d", "row {:?}", s.row(0));
    let reverse: Vec<bool> = s.vt.grid[0][..9].iter().map(|c| c.attr & vt::REVERSE != 0).collect();
    expect!(reverse == [false, true, true, false, true, true, false, false, false], "reverse {:?}", reverse);

    let mut s = Session::open(Some("日本\n".as_bytes()), Size { cols: 40, rows: 10 });
    s.feed(RIGHT.as_bytes());
    s.render();
    expect!(s.row(0) == "日本", "row {:?}", s.row(0));
    expect!((s.vt.x, s.vt.y) == (2, 0), "the cursor at {:?}", (s.vt.x, s.vt.y));

    let line = "x".repeat(50) + "\n";
    let mut s = Session::open(Some(line.as_bytes()), Size { cols: 30, rows: 6 });
    s.render();
    expect!(s.row(0) == "x".repeat(29) + ">", "a long line {:?}", s.row(0));
    s.feed(ctrl('e').as_bytes());
    s.render();
    expect!(s.row(0) == "x".repeat(28), "scrolled {:?}", s.row(0));

    /* A line as wide as the terminal fits, the last column and all: no erase
       after it, which would take that column's character */
    let line = "y".repeat(24) + "\n";
    let mut s = Session::open(Some(line.as_bytes()), Size { cols: 24, rows: 6 });
    s.render();
    expect!(s.row(0) == "y".repeat(24), "a full row {:?}", s.row(0));
    s.check()?;
    let line = "z".repeat(23) + "\n";
    let mut s = Session::open(Some(line.as_bytes()), Size { cols: 24, rows: 6 });
    s.render();
    expect!(s.row(0) == "z".repeat(23), "a row one short {:?}", s.row(0));

    let mut s = Session::open(Some(b"abc\n"), Size { cols: 10, rows: 2 });
    s.render();
    expect!(s.row(0).starts_with("edit: the"), "a small terminal {:?}", s.row(0));
    s.resize(Size { cols: 80, rows: 24 });
    s.render();
    expect!(s.row(0) == "abc", "after a resize {:?}", s.row(0));
    s.check()
}

fn case_new_file() -> Result<(), String> {
    let mut s = Session::open(None, SIZE);
    s.feed(ctrl('q').as_bytes());
    expect!(s.quit && s.disk.is_none(), "an untouched new file was written, or not left");
    let mut s = Session::open(None, SIZE);
    s.keys(&["abc", &ctrl('s')]);
    eq(s.disk.as_deref().unwrap_or(b"-"), b"abc\n", "a new file saved")?;
    Ok(())
}

fn case_decoder() -> Result<(), String> {
    let mut d = Decoder::new();
    let mut got = Vec::new();
    for &b in "\x1b[1;2A\x1bOB\x1b[5;5~é\x1b[99;99;99;99;99;99;99;99X\x01".as_bytes() {
        d.push(b);
        while let Some(k) = d.next() {
            got.push(k);
        }
    }
    use edit::Mods;
    let shift = Mods { shift: true, alt: false, ctrl: false };
    let ctl = Mods { shift: false, alt: false, ctrl: true };
    expect!(got.len() == 5, "keys {:?}", got);
    expect!(got[0] == Key::Up(shift) && got[1] == Key::Down(Mods::NONE) && got[2] == Key::PageUp(ctl), "keys {:?}", got);
    expect!(matches!(got[3], Key::Char(c) if c.as_bytes() == "é".as_bytes()), "keys {:?}", got);
    expect!(got[4] == Key::Ctrl(b'a'), "keys {:?}", got);
    /* A character cut short is its bytes */
    let mut d = Decoder::new();
    for &b in b"\xd0a" {
        d.push(b);
    }
    let k1 = d.next();
    let k2 = d.next();
    expect!(matches!(k1, Some(Key::Char(c)) if c.as_bytes() == b"\xd0") && matches!(k2, Some(Key::Char(c)) if c.as_bytes() == b"a"), "{:?} {:?}", k1, k2);
    Ok(())
}

const CASES: &[(&str, fn() -> Result<(), String>)] = &[
    ("an untouched file is saved as it was read", case_roundtrip_untouched),
    ("typing, ^A and ^E", case_typing),
    ("^A goes to the indent, then the line's start", case_smart_home),
    ("Enter, Backspace, Delete and ^D", case_lines),
    ("a selection cut, pasted and undone", case_selection),
    ("typing over a selection is one undo step", case_typing_over_selection),
    ("lines cut in a run and pasted back", case_cut_lines),
    ("^C and ^V of a line", case_copy_line),
    ("undo and redo", case_undo_redo),
    ("the word keys, ^W and ^U", case_words),
    ("find: next, back, round, either case, not found", case_find),
    ("go to a line", case_goto_line),
    ("the questions before a quit", case_quit),
    ("a save over a file that changed on disk", case_changed_on_disk),
    ("a paste is text", case_paste),
    ("moves, the kept column, and every way of sending a key", case_moves),
    ("what the screen shows", case_screen),
    ("a new file", case_new_file),
    ("the key decoder", case_decoder),
];

fn run_cases() -> bool {
    let mut ok = true;
    for (name, case) in CASES {
        let result = std::panic::catch_unwind(case).unwrap_or_else(|p| Err(panic_text(&p)));
        match result {
            Ok(()) => println!("PASS: {}", name),
            Err(why) => {
                println!("FAIL: {} -- {}", name, why);
                ok = false;
            }
        }
    }
    ok
}

/* ---- random ---- */

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const WORDS: &[&str] = &[
    "foo", "bar", "nos", "=", " ", "  ", "\t", "привет", "日本語", "😀", "e\u{301}", "\u{200b}", "#", "[section]",
    "key = \"value\"", "x", "-", "_",
];

fn random_text(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    let lines = rng.below(40);
    for _ in 0..lines {
        let parts = rng.below(12);
        for _ in 0..parts {
            match rng.below(20) {
                0 => out.push(rng.below(256) as u8),
                1 => out.push(rng.below(32) as u8),
                2 => out.extend(std::iter::repeat(b'w').take(rng.below(300))),
                _ => out.extend_from_slice(rng.pick(WORDS).as_bytes()),
            }
        }
        out.extend_from_slice(if rng.chance(10) { b"\r\n" } else { b"\n" });
    }
    if rng.chance(30) {
        out.pop();
    }
    out
}

fn random_size(rng: &mut Rng) -> Size {
    *rng.pick(&[
        Size { cols: 80, rows: 24 },
        Size { cols: 24, rows: 3 },
        Size { cols: 23, rows: 5 },
        Size { cols: 10, rows: 2 },
        Size { cols: 1, rows: 1 },
        Size { cols: 200, rows: 60 },
        Size { cols: 41, rows: 7 },
        Size { cols: 30, rows: 4 },
    ])
}

const SPECIAL: &[&str] = &[
    UP, DOWN, LEFT, RIGHT, S_UP, S_DOWN, S_LEFT, S_RIGHT, OPT_LEFT, OPT_RIGHT, DEL, BS, ENTER, "\t", "\x1b[H", "\x1b[F",
    "\x1b[5~", "\x1b[6~", "\x1b[1;2H", "\x1b[1;2F", "\x1b[5;2~", "\x1b[6;2~", "\x1b[1;5H", "\x1b[1;5F", "\x1b[1;3A",
    "\x1b[1;3B", "\x1b[1;6C", "\x1b[1;4D", "\x1bOA", "\x1bOB", "\x1bOC", "\x1bOD", "\x1bOH", "\x1bOF", "\x1b[3;3~",
    "\x1b\x7f", "\x1b[2~", "\x1b[Z", "\x1b[15~", "\x1b[?1;2c", "\x1b[201~", "\x1b\x1b", "\x1bx", "\x1b[",
];

/// One step of a random session.
fn step(rng: &mut Rng, s: &mut Session) -> Result<(), String> {
    match rng.below(100) {
        0..=29 => {
            let text = if rng.chance(75) {
                char::from(b' ' + rng.below(95) as u8).to_string()
            } else {
                rng.pick(WORDS).to_string()
            };
            s.feed(text.as_bytes());
        }
        30..=49 => s.feed(rng.pick(SPECIAL).as_bytes()),
        50..=59 => {
            if s.ed.prompting() && rng.chance(60) {
                s.feed(rng.pick(&["y", "n", "Y", "N", ESC, "1", "2", "7"]).as_bytes());
            } else {
                let c = char::from(b'a' + rng.below(26) as u8);
                s.feed(ctrl(c).as_bytes());
            }
        }
        60..=63 => {
            let mut paste = PASTE_START.as_bytes().to_vec();
            paste.extend(random_text(rng).into_iter().take(rng.below(3000)));
            if rng.chance(80) {
                paste.extend_from_slice(PASTE_END.as_bytes());
            }
            let cut = rng.below(paste.len() + 1);
            s.feed(&paste[..cut]);
            if rng.chance(50) {
                s.pause();
            }
            s.feed(&paste[cut..]);
        }
        64..=66 => {
            let garbage: Vec<u8> = (0..1 + rng.below(8)).map(|_| rng.below(256) as u8).collect();
            s.feed(&garbage);
        }
        67..=71 => s.pause(),
        72..=73 => {
            let size = random_size(rng);
            s.resize(size);
        }
        74..=93 => s.render(),
        94..=95 => s.consistent()?,
        96 => {
            /* Someone else writes the file */
            s.disk = Some(random_text(rng));
        }
        _ => undo_everything(s)?,
    }
    s.check()
}

/// Undoing every step gives back the file as it was read, and redoing as
/// many, the file as it was -- steps undone before are left undone.
fn undo_everything(s: &mut Session) -> Result<(), String> {
    if s.ed.prompting() || s.dec.in_paste() || s.dec.pending() || !s.ed.history_complete() {
        return Ok(());
    }
    s.pause();
    if s.ed.prompting() || s.dec.in_paste() || s.quit {
        return Ok(());
    }
    let now = s.contents();
    let mut undone = 0;
    loop {
        s.key(Key::Ctrl(b'z'));
        if s.ed.message() == b"nothing to undo" || undone > 100_000 {
            break;
        }
        undone += 1;
    }
    if s.contents() != s.initial {
        return Err(format!(
            "undoing everything left {:?}, the file was {:?}",
            String::from_utf8_lossy(&s.contents()),
            String::from_utf8_lossy(&s.initial)
        ));
    }
    for _ in 0..undone {
        s.key(Key::Ctrl(b'y'));
        if s.ed.message() == b"nothing to redo" {
            return Err(format!("{} steps undone, and fewer to redo", undone));
        }
    }
    if s.contents() != now {
        return Err(format!(
            "redoing everything gave {:?}, it was {:?}",
            String::from_utf8_lossy(&s.contents()),
            String::from_utf8_lossy(&now)
        ));
    }
    Ok(())
}

fn fuzz_session(seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let content = if rng.chance(10) { None } else { Some(random_text(&mut rng)) };
    let mut s = Session::open(content.as_deref(), random_size(&mut rng));
    let steps = 50 + rng.below(500);
    for i in 0..steps {
        if s.quit {
            let disk = s.disk.clone();
            let size = s.size;
            s = Session::open(disk.as_deref(), size);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(&mut rng, &mut s)))
            .unwrap_or_else(|p| Err(panic_text(&p)));
        if let Err(why) = result {
            let tail: Vec<&String> = s.log.iter().rev().take(12).collect();
            return Err(format!("step {}: {}\n  last fed, newest first: {:?}", i, why, tail));
        }
    }
    s.consistent()
}

fn run_fuzz(seed: u64, sessions: u64, seconds: Option<u64>) -> bool {
    let start = Instant::now();
    let mut done = 0u64;
    loop {
        if let Some(limit) = seconds {
            if start.elapsed() >= Duration::from_secs(limit) {
                break;
            }
        } else if done >= sessions {
            break;
        }
        let session_seed = seed.wrapping_mul(1_000_003).wrapping_add(done);
        if let Err(why) = fuzz_session(session_seed) {
            println!("FAIL: fuzz session {} (seed {}, session seed {}): {}", done, seed, session_seed, why);
            return false;
        }
        done += 1;
    }
    println!("PASS: fuzz, {} sessions from seed {} in {:.1} s", done, seed, start.elapsed().as_secs_f64());
    true
}

fn run_roundtrip(seed: u64, count: u64) -> bool {
    let mut rng = Rng(seed);
    for i in 0..count {
        let most = if rng.chance(5) { 100_000 } else { 200 };
        let len = rng.below(most);
        let data: Vec<u8> = (0..len)
            .map(|_| match rng.below(4) {
                0 => b'\n',
                1 => b'\r',
                _ => rng.below(256) as u8,
            })
            .collect();
        let ed = Editor::open(NAME, Some(&data)).expect("open");
        if ed.contents().expect("contents") != data {
            println!("FAIL: roundtrip {} (seed {}): {:?} came back otherwise", i, seed, data);
            return false;
        }
    }
    println!("PASS: {} random files read and written back byte for byte", count);
    true
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    let text = p
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    format!("panicked: {}", text)
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut what = "all".to_string();
    let mut seed = 1u64;
    let mut sessions = 3000u64;
    let mut seconds = None;
    let mut i = 0;
    while i < args.len() {
        let value = || args.get(i + 1).and_then(|v| v.parse::<u64>().ok());
        match args[i].as_str() {
            "--seed" => {
                seed = value().expect("--seed N");
                i += 1;
            }
            "--sessions" => {
                sessions = value().expect("--sessions N");
                i += 1;
            }
            "--seconds" => {
                seconds = Some(value().expect("--seconds S"));
                i += 1;
            }
            /* One session again, by the seed a failure said */
            "--session" => {
                let session = value().expect("--session SEED");
                match fuzz_session(session) {
                    Ok(()) => println!("PASS: session {}", session),
                    Err(why) => println!("FAIL: session {}: {}", session, why),
                }
                return;
            }
            w @ ("all" | "cases" | "roundtrip" | "fuzz") => what = w.to_string(),
            other => {
                eprintln!("edit-test: what is {:?}? (all|cases|roundtrip|fuzz, --seed, --sessions, --seconds)", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let mut ok = true;
    if what == "all" || what == "cases" {
        ok &= run_cases();
    }
    if what == "all" || what == "roundtrip" {
        ok &= run_roundtrip(seed, 2000);
    }
    if what == "all" || what == "fuzz" {
        ok &= run_fuzz(seed, sessions, seconds);
    }
    std::process::exit(if ok { 0 } else { 1 });
}
