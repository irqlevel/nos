//! GRUB's configuration, as much of it as says what GRUB would boot: the
//! menu entries -- in submenus too, at any depth -- each with its `linux`
//! and `initrd` lines and the `search` or `set root` that names the
//! filesystem they are on; the root set outside any entry, which every
//! entry starts from; the `default`; and grubenv's `next_entry` and
//! `saved_entry`, which a generated configuration defers to.
//!
//! It is not GRUB's script interpreter. It reads the script's words as GRUB
//! does -- quotes, escapes, comments, variables, and a brace a token of its
//! own wherever it is not quoted or escaped -- makes a command's arguments
//! of them as GRUB's script does, a variable's value outside quotes split
//! at blanks, and reads its blocks, but runs no condition: every branch of
//! an `if` is read, the last `search` and the last `linux` an entry has win
//! (a `linux` unloading the `initrd` before it, as GRUB's does), and a
//! variable has a value only where an assignment outside any condition
//! gives it one. An entry's body is read when it is chosen, its variables
//! as the whole script left them, as GRUB runs it at boot. What
//! grub-mkconfig writes -- Debian's and Ubuntu's grub.cfg -- is read as GRUB
//! reads it; what would take more -- another file read for the menu, a
//! variable with no value here -- is said, not guessed. scripts/hvboot-test.py
//! holds this to what GRUB 2.06 itself did with each of its cases.
//!
//! It holds no more than what it reads for: the text is lexed where it
//! lies, and each command read as it is parsed -- one the configuration is
//! not read for (`insmod`, `echo`, `load_video`) passed over, an entry kept
//! as its title, its id and where its body is; no word is ever held whole,
//! each read a piece at a time into what it is read for ([`Pieces`]); and
//! what variables add, the variables and the menu's items are held to
//! bounds, said when they are met. So a grub.cfg as long as the loader reads
//! (`plan::CONFIG_MAX`), of whatever shape, is read inside its heap -- the
//! harness holds it to that (fuzz/hvboot).

use alloc::string::String;
use alloc::vec::Vec;

/// What a word is read into as it is lexed, a piece at a time: its text as
/// written -- quotes and escapes taken off -- and its variables, inside
/// double quotes or not. An empty quoted string is a piece of text of its
/// own, empty: `""` is a word. Nothing holds a word whole: each is read into
/// what it is read for -- a command's name ([`Name`]), arguments
/// ([`Made`]), an assignment ([`AssignName`], [`Made`]) -- or into nothing
/// ([`Skip`]).
trait Pieces {
    fn text(&mut self, s: &str);
    fn var(&mut self, name: &str, quoted: bool);
}

/// A word passed over.
struct Skip;

impl Pieces for Skip {
    fn text(&mut self, _: &str) {}
    fn var(&mut self, _: &str, _: bool) {}
}

/// A command's first word, as much of it as says what the command is: its
/// text up to its first variable, and whether it has one.
#[derive(Default)]
struct Name {
    lead: String,
    var: bool,
}

impl Name {
    /// The word's text, when it has no variable in it.
    fn literal(&self) -> Option<&str> {
        (!self.var).then_some(self.lead.as_str())
    }
}

impl Pieces for Name {
    fn text(&mut self, s: &str) {
        if !self.var {
            self.lead.push_str(s);
        }
    }
    fn var(&mut self, _: &str, _: bool) {
        self.var = true;
    }
}

/// Where a command's words are in the text: from its first's first byte to
/// its last's last.
#[derive(Clone, Copy, Debug)]
struct Span {
    from: usize,
    to: usize,
}

#[derive(Debug)]
enum Tok {
    /// A word, read into the `Pieces` the lexer was given, and where it is.
    Word { from: usize, to: usize },
    /// A newline or `;`.
    Sep,
    Open,
    Close,
}

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// What ends a word outside quotes: a blank, a separator, a brace -- each
/// a token of its own, as GRUB's lexer has them.
fn ends_word(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b';' | b'{' | b'}')
}

/// What ends a run of characters taken as they are, outside quotes.
fn special(b: u8) -> bool {
    ends_word(b) || matches!(b, b'\\' | b'\'' | b'"' | b'$')
}

/// The script's tokens, read off its text one at a time.
struct Lexer<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    /// The words of `span` of `text`, to be read again.
    fn over(text: &'a str, span: Span) -> Lexer<'a> {
        Lexer { text: &text[..span.to], at: span.from }
    }

    /// The next token, a word's pieces into `into`.
    fn next(&mut self, into: &mut dyn Pieces) -> Option<Tok> {
        let b = self.text.as_bytes();
        loop {
            match *b.get(self.at)? {
                b' ' | b'\t' | b'\r' => self.at += 1,
                b'\\' if b.get(self.at + 1) == Some(&b'\n') => self.at += 2,
                b'\n' | b';' => {
                    self.at += 1;
                    return Some(Tok::Sep);
                }
                b'#' => {
                    while self.at < b.len() && b[self.at] != b'\n' {
                        self.at += 1;
                    }
                }
                b'{' => {
                    self.at += 1;
                    return Some(Tok::Open);
                }
                b'}' => {
                    self.at += 1;
                    return Some(Tok::Close);
                }
                _ => {
                    let from = self.at;
                    self.word(into);
                    return Some(Tok::Word { from, to: self.at });
                }
            }
        }
    }

    /// The next word, into `into`: false at the end of what is lexed.
    fn next_word(&mut self, into: &mut dyn Pieces) -> bool {
        loop {
            match self.next(into) {
                Some(Tok::Word { .. }) => return true,
                Some(_) => {}
                None => return false,
            }
        }
    }

    /// A word from `self.at`: up to a blank, a separator or a brace that is
    /// not quoted or escaped.
    fn word(&mut self, into: &mut dyn Pieces) {
        let b = self.text.as_bytes();
        while let Some(&c) = b.get(self.at) {
            match c {
                c if ends_word(c) => break,
                b'\\' => match self.text[self.at + 1..].chars().next() {
                    Some('\n') => self.at += 2,
                    Some(ch) => {
                        let mut buf = [0u8; 4];
                        into.text(ch.encode_utf8(&mut buf));
                        self.at += 1 + ch.len_utf8();
                    }
                    None => self.at += 1,
                },
                b'\'' => {
                    let from = self.at + 1;
                    let to = self.text[from..].find('\'').map_or(b.len(), |n| from + n);
                    into.text(&self.text[from..to]);
                    self.at = (to + 1).min(b.len());
                }
                b'"' => {
                    self.at += 1;
                    self.quoted(into);
                }
                b'$' => self.variable(into, false),
                _ => {
                    let from = self.at;
                    while self.at < b.len() && !special(b[self.at]) {
                        self.at += 1;
                    }
                    into.text(&self.text[from..self.at]);
                }
            }
        }
    }

    /// Inside double quotes, from after the one that opens them to after
    /// the one that closes them: `\\`, `\"` and `\$` the character, a
    /// backslash before a newline nothing, variables expanded, and anything
    /// else -- another backslash too -- as it is.
    fn quoted(&mut self, into: &mut dyn Pieces) {
        let b = self.text.as_bytes();
        into.text("");
        while let Some(&c) = b.get(self.at) {
            match c {
                b'"' => {
                    self.at += 1;
                    return;
                }
                b'\\' if matches!(b.get(self.at + 1), Some(b'\\' | b'"' | b'$')) => {
                    into.text(&self.text[self.at + 1..self.at + 2]);
                    self.at += 2;
                }
                b'\\' if b.get(self.at + 1) == Some(&b'\n') => self.at += 2,
                b'$' => self.variable(into, true),
                _ => {
                    let from = self.at;
                    self.at += 1;
                    while self.at < b.len() && !matches!(b[self.at], b'"' | b'\\' | b'$') {
                        self.at += 1;
                    }
                    into.text(&self.text[from..self.at]);
                }
            }
        }
    }

    /// A `$name`, `${name}`, `$1` or `$?` at `self.at` -- inside double
    /// quotes or not: a `$` that starts none is the character itself.
    fn variable(&mut self, into: &mut dyn Pieces, quoted: bool) {
        let rest = &self.text[self.at + 1..];
        if let Some(inner) = rest.strip_prefix('{') {
            if let Some(close) = inner.find('}') {
                into.var(&inner[..close], quoted);
                self.at += close + 3;
                return;
            }
        }
        let r = rest.as_bytes();
        let n = match r.first() {
            Some(c) if c.is_ascii_digit() => r.iter().take_while(|c| c.is_ascii_digit()).count(),
            Some(b'?' | b'#' | b'*' | b'@') => 1,
            _ => r.iter().take_while(|&&c| is_name(char::from(c))).count(),
        };
        if n == 0 {
            into.text("$");
            self.at += 1;
            return;
        }
        into.var(&rest[..n], quoted);
        self.at += 1 + n;
    }
}

/// How a `search.<by>` command, or `search`'s own option, finds the
/// filesystem it names.
#[derive(Clone, Copy, Debug, PartialEq)]
enum By {
    Uuid,
    Label,
    File,
}

/// The commands the configuration is read for. Every other command is
/// dropped as it is parsed, and `top` and `entry_body` match on this, not on
/// a command's name: a command they read is one with a kind here first.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// `set NAME=VALUE`.
    Set,
    /// `NAME=VALUE` on its own: what GRUB makes of a first word that is no
    /// command and has a `=` in it.
    Assign,
    Linux,
    Initrd,
    /// `search`, with its options.
    Search,
    /// `search.fs_uuid`, `search.fs_label`, `search.file`: a name, then the
    /// variable to set, then hints.
    SearchBy(By),
    /// A command GRUB reads another file for its menu by -- `configfile`,
    /// `source`, `.`, `blscfg` -- which this does not: said.
    Elsewhere,
}

impl Kind {
    /// The kind of a command whose first word is `first`.
    fn of(first: &Name) -> Option<Kind> {
        let k = match first.literal() {
            Some("set") => Kind::Set,
            Some("linux" | "linux16" | "linuxefi") => Kind::Linux,
            Some("initrd" | "initrd16" | "initrdefi") => Kind::Initrd,
            Some("search") => Kind::Search,
            Some("search.fs_uuid") => Kind::SearchBy(By::Uuid),
            Some("search.fs_label") => Kind::SearchBy(By::Label),
            Some("search.file") => Kind::SearchBy(By::File),
            Some("configfile" | "source" | "." | "blscfg") => Kind::Elsewhere,
            _ => {
                let (name, _) = first.lead.split_once('=')?;
                return (!name.is_empty() && name.chars().all(is_name)).then_some(Kind::Assign);
            }
        };
        Some(k)
    }
}

/// A command as the parser has it: what its first word says, and where all
/// of its words are.
struct Cmd {
    first: Name,
    span: Span,
}

/// How a block ended: at a command whose name it was told to stop at, at a
/// closing brace -- where it was -- or at the end of what is parsed (or as
/// deep as blocks are read).
enum End {
    Stop(String),
    Close(usize),
    Done,
}

/// What a block's commands are read for as they are parsed: a level of the
/// menu -- the top, or a submenu's body -- the chosen entry's body, or
/// nothing: a condition, a function, a loop, an entry's body before it is
/// chosen.
enum Ctx<'x> {
    Menu(&'x mut Menu),
    Entry(&'x mut Boot),
    Skip,
}

struct Parser<'a> {
    text: &'a str,
    lexer: Lexer<'a>,
    /// A brace read past the end of a command, not yet taken.
    peeked: Option<Tok>,
    /// Commands read and given back, the next last: the words after a
    /// keyword that began a command -- `then echo` -- and what stood where a
    /// brace was looked for.
    pending: Vec<Cmd>,
    /// How deep the blocks go: a script that nests past this is cut there.
    depth: u32,
}

const MAX_DEPTH: u32 = 32;

enum Next {
    Cmd(Cmd),
    Open,
    Close,
    End,
}

impl<'a> Parser<'a> {
    /// A parser of `span` of `text` -- all of it, or an entry's body.
    fn over(text: &'a str, span: Span) -> Parser<'a> {
        Parser { text, lexer: Lexer::over(text, span), peeked: None, pending: Vec::new(), depth: 0 }
    }

    /// The next command -- its first word read for its name, the rest
    /// passed over -- or a brace, or the end.
    fn next(&mut self) -> Next {
        if let Some(c) = self.pending.pop() {
            return Next::Cmd(c);
        }
        let mut name = Name::default();
        let mut first = None;
        let (mut to, mut second) = (0, None);
        loop {
            let t = match self.peeked.take() {
                Some(t) => Some(t),
                None if first.is_none() => self.lexer.next(&mut name),
                None => self.lexer.next(&mut Skip),
            };
            match t {
                None => break,
                Some(Tok::Word { from, to: end }) => {
                    match first {
                        None => first = Some(from),
                        Some(_) => {
                            second.get_or_insert(from);
                        }
                    }
                    to = end;
                }
                Some(Tok::Sep) if first.is_none() => {}
                Some(Tok::Sep) => break,
                Some(t @ (Tok::Open | Tok::Close)) if first.is_some() => {
                    self.peeked = Some(t);
                    break;
                }
                Some(Tok::Open) => return Next::Open,
                Some(Tok::Close) => return Next::Close,
            }
        }
        let Some(from) = first else { return Next::End };
        /* A keyword that begins a block's body may share its command with
         * the body's first: `then set x=1`, that command read next. */
        if let (Some("then" | "else" | "do"), Some(s)) = (name.literal(), second) {
            let mut rest = Lexer::over(self.text, Span { from: s, to });
            let mut next = Name::default();
            if rest.next_word(&mut next) {
                self.pending.push(Cmd { first: next, span: Span { from: s, to } });
            }
            return Next::Cmd(Cmd { first: name, span: Span { from, to: s } });
        }
        Next::Cmd(Cmd { first: name, span: Span { from, to } })
    }

    /// The body a brace opens, read for `ctx`, and where it is between its
    /// braces -- or, when no brace follows, none, and what came instead
    /// read as what comes next.
    fn body(&mut self, r: &mut Reading, ctx: &mut Ctx, conditional: bool) -> Span {
        match self.next() {
            Next::Open => {
                let from = self.lexer.at;
                let to = match self.block(r, ctx, conditional, &[]) {
                    End::Close(at) => at,
                    _ => self.lexer.at,
                };
                Span { from, to: to.max(from) }
            }
            Next::Cmd(c) => {
                self.pending.push(c);
                Span { from: 0, to: 0 }
            }
            Next::Close => {
                self.peeked = Some(Tok::Close);
                Span { from: 0, to: 0 }
            }
            Next::End => Span { from: 0, to: 0 },
        }
    }

    /// Commands until one whose first word is one of `stop` -- which is
    /// returned -- or a closing brace, or the end: each read for `ctx` as it
    /// is parsed.
    fn block(&mut self, r: &mut Reading, ctx: &mut Ctx, conditional: bool, stop: &[&str]) -> End {
        self.depth += 1;
        let end = loop {
            if self.depth > MAX_DEPTH {
                break End::Done;
            }
            let cmd = match self.next() {
                Next::Cmd(c) => c,
                Next::Open => {
                    /* A brace block of its own: read for the same. */
                    let _ = self.block(r, ctx, conditional, &[]);
                    continue;
                }
                Next::Close => break End::Close(self.lexer.at.saturating_sub(1)),
                Next::End => break End::Done,
            };
            let name = cmd.first.literal().unwrap_or_default();
            if stop.contains(&name) {
                break End::Stop(String::from(name));
            }
            match name {
                "menuentry" | "submenu" => match ctx {
                    Ctx::Menu(_) if r.items >= MAX_ITEMS => {
                        r.note(alloc::format!("menu items past the first {}, not read", MAX_ITEMS));
                        self.body(r, &mut Ctx::Skip, conditional);
                    }
                    Ctx::Menu(m) => {
                        r.items += 1;
                        let (title, id) = title_and_id(r, self.text, cmd.span);
                        if name == "menuentry" {
                            let body = self.body(r, &mut Ctx::Skip, conditional);
                            m.items.push(Item::Entry(Entry { title, id, body }));
                        } else {
                            let mut sub = Menu { nested: true, ..Menu::default() };
                            self.body(r, &mut Ctx::Menu(&mut sub), conditional);
                            sub.items.shrink_to_fit();
                            sub.roots.shrink_to_fit();
                            let s = Submenu { title, id, items: sub.items, roots: sub.roots };
                            m.items.push(Item::Submenu(alloc::boxed::Box::new(s)));
                        }
                    }
                    _ => {
                        self.body(r, &mut Ctx::Skip, conditional);
                    }
                },
                "function" => {
                    self.body(r, &mut Ctx::Skip, conditional);
                }
                "if" => loop {
                    /* The condition, not run, up to `then`; then the branch,
                     * read, up to `elif`, `else` or `fi`. */
                    if !matches!(self.block(r, &mut Ctx::Skip, conditional, &["then"]), End::Stop(_)) {
                        break;
                    }
                    match self.block(r, ctx, true, &["elif", "else", "fi"]) {
                        End::Stop(k) if k == "elif" => continue,
                        End::Stop(k) if k == "else" => {
                            let _ = self.block(r, ctx, true, &["fi"]);
                            break;
                        }
                        _ => break,
                    }
                },
                "for" | "while" | "until" => {
                    let _ = self.block(r, &mut Ctx::Skip, conditional, &["do"]);
                    let _ = self.block(r, &mut Ctx::Skip, conditional, &["done"]);
                }
                _ => {
                    if let Some(kind) = Kind::of(&cmd.first) {
                        match ctx {
                            Ctx::Menu(m) => menu_cmd(r, self.text, m, conditional, kind, cmd.span),
                            Ctx::Entry(b) => entry_cmd(r, self.text, b, kind, cmd.span),
                            Ctx::Skip => {}
                        }
                    }
                }
            }
        };
        self.depth -= 1;
        end
    }
}

/// Where an entry's files are: the filesystem `search` names, or a disk
/// and partition as GRUB names them -- `hd0,gpt16` -- or a device name this
/// does not know, which finds nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum Root {
    Device { disk: u32, part: Option<u32> },
    FsUuid(String),
    Label(String),
    File(String),
    Other(String),
}

/// A menu entry: its title, its id, and where its body is -- read when it
/// is chosen ([`read_entry`]), as GRUB runs it then.
#[derive(Clone, Debug)]
pub struct Entry {
    pub title: String,
    pub id: Option<String>,
    body: Span,
}

/// What an entry's body says: its kernel, the kernel's arguments, its
/// initrds, and the roots it sets.
#[derive(Clone, Debug, Default)]
pub struct Boot {
    pub kernel: Option<String>,
    /// The kernel's arguments, as GRUB's `linux` puts them on the command
    /// line after the kernel's path: each after a space, `quote`d.
    pub args: String,
    /// Its initrds' paths, each ended by a NUL, which no argument of GRUB's
    /// has in it: one string, however many there are (`initrds`).
    initrd_paths: String,
    /// Its own `search`es and `set root`s, in order: the last that finds its
    /// filesystem wins, as GRUB's root is whichever was set last.
    pub roots: Vec<Root>,
}

impl Boot {
    /// Its initrds' paths, in order.
    pub fn initrds(&self) -> impl Iterator<Item = &str> {
        self.initrd_paths.split_terminator('\0')
    }
}

/// An item of the menu: an entry, or a submenu -- out of line, so that an
/// item is an entry's size.
#[derive(Debug)]
pub enum Item {
    Entry(Entry),
    Submenu(alloc::boxed::Box<Submenu>),
}

/// A submenu, and the roots its body sets outside its entries -- where each
/// of them starts from, kept once here.
#[derive(Debug)]
pub struct Submenu {
    pub title: String,
    pub id: Option<String>,
    pub items: Vec<Item>,
    pub roots: Vec<Root>,
}

impl Item {
    fn title_and_id(&self) -> (&str, Option<&str>) {
        match self {
            Item::Entry(e) => (&e.title, e.id.as_deref()),
            Item::Submenu(s) => (&s.title, s.id.as_deref()),
        }
    }
}

/// A level of the menu as it is read -- the top, or a submenu's body
/// (`nested`): its items, the roots it sets outside them, and, the top's,
/// its default.
#[derive(Debug, Default)]
pub struct Menu {
    pub items: Vec<Item>,
    /// The `search`es and `set root`s outside every entry, in order: the
    /// root every entry of it starts from, as GRUB's environment is what an
    /// entry runs in.
    pub roots: Vec<Root>,
    /// The `default` an assignment gave a value, the last of them.
    default: Option<String>,
    /// Whether `default` is set from grubenv's `saved_entry`, or deferred to
    /// its `next_entry` -- what grub-mkconfig writes for GRUB_DEFAULT=saved,
    /// and for every configuration.
    saved: bool,
    next: bool,
    nested: bool,
}

/// The most variables said to have no value: a list, not a count.
const MAX_UNKNOWN: usize = 16;
/// The most variables kept: grub-mkconfig's script sets a few dozen.
pub const MAX_VARS: usize = 256;
/// The most menu items kept, submenus' included: grub-mkconfig writes some
/// three a kernel.
pub const MAX_ITEMS: usize = 2048;
/// The most bytes variables' values add to the commands read, all of them
/// together: a value used many times is text the file does not have, and
/// past this a variable is read as empty, said.
pub const EXPAND_MAX: usize = 256 * 1024;

/// What reading the script has gathered besides the menu: the variables,
/// and what is said of it.
#[derive(Debug, Default)]
pub struct Reading {
    /// Variables an assignment outside any condition gave a value.
    vars: Vec<(String, String)>,
    /// What variables' values have added so far.
    expanded: usize,
    /// The menu items kept so far.
    items: usize,
    /// Variables used that had no value here, and so are empty.
    pub unknown: Vec<String>,
    /// What GRUB would read that this does not, and what was cut: said,
    /// once each.
    pub notes: Vec<String>,
}

/// A grub.cfg, read: its menu, and what the reading gathered.
#[derive(Debug, Default)]
pub struct Config {
    pub menu: Menu,
    pub reading: Reading,
}

impl Reading {
    fn var(&self, name: &str) -> Option<&str> {
        self.vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn set(&mut self, name: String, value: String) {
        if let Some(v) = self.vars.iter_mut().find(|(n, _)| *n == name) {
            v.1 = value;
        } else if self.vars.len() < MAX_VARS {
            self.vars.push((name, value));
        } else {
            self.note(alloc::format!("variables past the {} set first, not kept", MAX_VARS));
        }
    }

    fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// A variable's value, if it has one here. In an entry, `$root` is the
    /// entry's root, which `plan::root_of` works out from its searches:
    /// empty here, so that `($root)/x` is a path on it (`plan::on`). A
    /// variable with no value is noted; one past the budget is empty.
    fn value(&mut self, name: &str, in_entry: bool) -> Option<String> {
        if in_entry && name == "root" {
            return Some(String::new());
        }
        let Some(len) = self.var(name).map(str::len) else {
            if !self.unknown.iter().any(|u| u == name) && self.unknown.len() < MAX_UNKNOWN {
                self.unknown.push(String::from(name));
            }
            return None;
        };
        if self.expanded.saturating_add(len) > EXPAND_MAX {
            self.note(alloc::format!("variables' values past {} KiB of them, read as empty", EXPAND_MAX >> 10));
            return None;
        }
        self.expanded += len;
        self.var(name).map(String::from)
    }
}

/// Arguments made of words as GRUB's script makes them
/// (`grub_script_arglist_to_argv`): a word's pieces into the argument being
/// made one after another -- text, and what was quoted, as it is; an
/// unquoted variable's value split at blanks, its first piece onto that
/// argument and each after it an argument of its own -- and an argument
/// nothing was put into, none: `$unset` is no argument, `""` an empty one.
struct Made<'c> {
    r: &'c mut Reading,
    in_entry: bool,
    /// `$menuentry_id_option` read as `--id`, as grub-mkconfig's script
    /// makes it wherever GRUB has ids.
    id_option: bool,
    /// The argument being made, if anything has been put into it.
    cur: Option<String>,
    out: Vec<String>,
}

impl<'c> Made<'c> {
    fn new(r: &'c mut Reading, in_entry: bool) -> Made<'c> {
        Made { r, in_entry, id_option: false, cur: None, out: Vec::new() }
    }

    /// A word's end: the argument being made, made.
    fn end(&mut self) {
        self.out.extend(self.cur.take());
    }
}

impl Pieces for Made<'_> {
    fn text(&mut self, s: &str) {
        self.cur.get_or_insert_with(String::new).push_str(s);
    }

    fn var(&mut self, name: &str, quoted: bool) {
        if self.id_option && name == "menuentry_id_option" {
            return self.text("--id");
        }
        let Some(value) = self.r.value(name, self.in_entry) else { return };
        if quoted {
            return self.text(&value);
        }
        for (i, part) in value.split([' ', '\t', '\n', '\r']).filter(|s| !s.is_empty()).enumerate() {
            if i != 0 {
                self.out.extend(self.cur.take());
            }
            self.text(part);
        }
    }
}

/// A command's arguments, made one at a time as they are read -- its name
/// first -- from its words where they are in the text: no more of them held
/// than one word makes.
struct Args<'t> {
    lexer: Lexer<'t>,
    /// The arguments the last word made, not yet taken: the next last.
    made: Vec<String>,
    in_entry: bool,
    id_option: bool,
}

impl<'t> Args<'t> {
    fn new(text: &'t str, span: Span, in_entry: bool) -> Args<'t> {
        Args { lexer: Lexer::over(text, span), made: Vec::new(), in_entry, id_option: false }
    }

    fn next(&mut self, r: &mut Reading) -> Option<String> {
        loop {
            if let Some(a) = self.made.pop() {
                return Some(a);
            }
            let mut m = Made::new(r, self.in_entry);
            m.id_option = self.id_option;
            if !self.lexer.next_word(&mut m) {
                return None;
            }
            m.end();
            self.made = m.out;
            self.made.reverse();
        }
    }
}

/// An assignment's word, as far as its name: the text before its `=`, and
/// whether its value uses grubenv's `saved_entry` or `next_entry` -- what
/// `default` defers to, and is not made a value of. Not an assignment when a
/// variable comes before the `=`.
#[derive(Default)]
struct AssignName {
    lead: String,
    name: Option<String>,
    var_first: bool,
    saved: bool,
    next: bool,
}

impl Pieces for AssignName {
    fn text(&mut self, s: &str) {
        if self.name.is_none() && !self.var_first {
            self.lead.push_str(s);
            if let Some(i) = self.lead.find('=') {
                self.lead.truncate(i);
                self.name = Some(core::mem::take(&mut self.lead));
            }
        }
    }

    fn var(&mut self, name: &str, _: bool) {
        if self.name.is_none() {
            self.var_first = true;
        }
        self.saved |= name == "saved_entry";
        self.next |= name == "next_entry";
    }
}

/// An assignment's value: what follows its `=`, made the arguments it
/// makes.
struct AssignValue<'c> {
    made: Made<'c>,
    past: bool,
}

impl Pieces for AssignValue<'_> {
    fn text(&mut self, s: &str) {
        if self.past {
            return self.made.text(s);
        }
        if let Some(i) = s.find('=') {
            self.past = true;
            self.made.text(&s[i + 1..]);
        }
    }

    fn var(&mut self, name: &str, quoted: bool) {
        if self.past {
            self.made.var(name, quoted);
        }
    }
}

/// The assignment at `span` -- `set NAME=VALUE` or `NAME=VALUE` -- read as
/// far as its name.
fn assignment(text: &str, kind: Kind, span: Span) -> Option<AssignName> {
    let mut words = Lexer::over(text, span);
    if kind == Kind::Set {
        words.next_word(&mut Skip);
    }
    let mut a = AssignName::default();
    words.next_word(&mut a).then_some(())?;
    a.name.is_some().then_some(a)
}

/// The value of the assignment at `span`, as GRUB's `set` takes it: the
/// first argument what follows the `=` makes, whole, and no more.
fn assigned(r: &mut Reading, text: &str, kind: Kind, span: Span, in_entry: bool) -> String {
    let mut words = Lexer::over(text, span);
    if kind == Kind::Set {
        words.next_word(&mut Skip);
    }
    let mut v = AssignValue { made: Made::new(r, in_entry), past: false };
    words.next_word(&mut v);
    v.made.end();
    v.made.out.into_iter().next().unwrap_or_default()
}

/// What an option of a GRUB command takes after it.
#[derive(Clone, Copy, PartialEq)]
enum Takes {
    Nothing,
    /// A value, or none: `--set` alone, `--set=VAR`.
    Maybe,
    Value,
}

struct Opt {
    long: &'static str,
    short: Option<u8>,
    takes: Takes,
}

const fn opt(long: &'static str, short: Option<u8>, takes: Takes) -> Opt {
    Opt { long, short, takes }
}

/// `search`'s options (GRUB's commands/search_wrap.c).
const SEARCH_OPTIONS: &[Opt] = &[
    opt("file", Some(b'f'), Takes::Nothing),
    opt("label", Some(b'l'), Takes::Nothing),
    opt("fs-uuid", Some(b'u'), Takes::Nothing),
    opt("set", Some(b's'), Takes::Maybe),
    opt("no-floppy", Some(b'n'), Takes::Nothing),
    opt("efidisk-only", None, Takes::Nothing),
    opt("hint", Some(b'h'), Takes::Value),
    opt("hint-ieee1275", None, Takes::Value),
    opt("hint-bios", None, Takes::Value),
    opt("hint-baremetal", None, Takes::Value),
    opt("hint-efi", None, Takes::Value),
    opt("hint-arc", None, Takes::Value),
];

/// `menuentry`'s and `submenu`'s options (GRUB's commands/menuentry.c).
const MENU_OPTIONS: &[Opt] = &[
    opt("class", None, Takes::Value),
    opt("users", None, Takes::Value),
    opt("hotkey", None, Takes::Value),
    opt("source", None, Takes::Value),
    opt("id", None, Takes::Value),
    opt("unrestricted", None, Takes::Nothing),
];

/// A command's arguments, after its name, as GRUB's parser reads them
/// (lib/arg.c), one at a time: the options given -- each the last value it
/// was given, if any -- and the command's first arguments that are none, as
/// many as `keep`. An option with a value takes the next argument for it --
/// a long one when that does not begin with `-`, a short one at the end of
/// its group always, or, when the value may be left out, when that argument
/// is no option. Err for an option the command does not have, or one that
/// wants a value and has none: GRUB refuses the command.
#[allow(clippy::type_complexity)]
fn grub_args(r: &mut Reading, args: &mut Args, opts: &'static [Opt], keep: usize)
    -> Result<(Vec<(&'static str, Option<String>)>, Vec<String>), String>
{
    let mut given: Vec<(&'static str, Option<String>)> = Vec::new();
    let mut rest = Vec::new();
    let mut ahead: Option<String> = None;
    let mut give = |name: &'static str, value: Option<String>| match given.iter_mut().find(|(n, _)| *n == name) {
        Some(g) => g.1 = value,
        None => given.push((name, value)),
    };
    while let Some(a) = ahead.take().or_else(|| args.next(r)) {
        if !a.starts_with('-') || a.len() == 1 {
            if rest.len() < keep {
                rest.push(a);
            }
            continue;
        }
        if a == "--" {
            while let Some(a) = args.next(r) {
                if rest.len() < keep {
                    rest.push(a);
                }
            }
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, mut value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(String::from(v))),
                None => (long, None),
            };
            let Some(o) = opts.iter().find(|o| o.long == name) else {
                return Err(alloc::format!("unknown argument {}", a));
            };
            if value.is_none() && o.takes != Takes::Nothing {
                match args.next(r) {
                    Some(n) if !n.starts_with('-') => value = Some(n),
                    other => ahead = other,
                }
            }
            if o.takes == Takes::Value && value.is_none() {
                return Err(alloc::format!("{} wants a value", a));
            }
            give(o.long, value);
            continue;
        }
        let group = a.as_bytes()[1..].to_vec();
        for (k, &c) in group.iter().enumerate() {
            let Some(o) = opts.iter().find(|o| o.short == Some(c)) else {
                return Err(alloc::format!("unknown argument -{}", char::from(c)));
            };
            let mut value = None;
            if k + 1 == group.len() && o.takes != Takes::Nothing {
                match args.next(r) {
                    Some(n) if o.takes == Takes::Value || n.len() < 2 || !n.starts_with('-') => value = Some(n),
                    other => ahead = other,
                }
                if o.takes == Takes::Value && value.is_none() {
                    return Err(alloc::format!("-{} wants a value", char::from(c)));
                }
            }
            give(o.long, value);
        }
    }
    Ok((given, rest))
}

/// The title and the id of a `menuentry` or `submenu` at `span`: its first
/// argument that is no option, and `--id`'s value; `$menuentry_id_option`
/// is `--id`.
fn title_and_id(r: &mut Reading, text: &str, span: Span) -> (String, Option<String>) {
    let mut args = Args::new(text, span, false);
    args.id_option = true;
    let _ = args.next(r);
    match grub_args(r, &mut args, MENU_OPTIONS, 1) {
        Ok((given, rest)) => {
            let id = given.into_iter().find(|(n, _)| *n == "id").and_then(|(_, v)| v);
            (rest.into_iter().next().unwrap_or_default(), id)
        }
        /* An option GRUB would refuse the entry for: its first argument
         * taken for the title, as it was before options were read. */
        Err(_) => {
            let mut args = Args::new(text, span, false);
            let _ = args.next(r);
            (args.next(r).unwrap_or_default(), None)
        }
    }
}

/// `hd0,gpt16`, `(hd1,msdos2)`, `hd0`: the disk and the partition.
pub fn device(spec: &str) -> Option<Root> {
    let s = spec.trim_matches(|c| c == '(' || c == ')');
    let rest = s.strip_prefix("hd")?;
    let (disk, part) = match rest.split_once(',') {
        Some((d, p)) => (d, Some(p.trim_start_matches("gpt").trim_start_matches("msdos"))),
        None => (rest, None),
    };
    let disk = disk.parse().ok()?;
    let part = match part {
        Some(p) => Some(p.parse().ok()?),
        None => None,
    };
    Some(Root::Device { disk, part })
}

/// What a root set to `value` names: a device as GRUB names one, or, not
/// one this knows, itself -- which finds nothing.
fn root_named(value: &str) -> Option<Root> {
    if value.is_empty() {
        return None;
    }
    Some(device(value).unwrap_or_else(|| Root::Other(String::from(value))))
}

fn by_root(by: By, name: String) -> Root {
    match by {
        By::Uuid => Root::FsUuid(name),
        By::Label => Root::Label(name),
        By::File => Root::File(name),
    }
}

/// The root a search sets, if it sets root. `search.<by> NAME VAR HINT...`
/// sets VAR when there is one; `search`, its options read as GRUB reads
/// them, sets the variable `--set` names -- root when it names none -- by
/// label, UUID or file, in that order of precedence, its first name the one
/// looked for; and with no name left after its options, it looks for the
/// one `--set` took and sets root, as GRUB 2.06 does (`search -f -s FILE`).
/// A search without `--set` only says where it found the name, and one GRUB
/// refuses -- an option it does not have, no kind of search -- sets nothing.
fn search(r: &mut Reading, text: &str, kind: Kind, span: Span, in_entry: bool) -> Option<Root> {
    let mut args = Args::new(text, span, in_entry);
    let _ = args.next(r);
    match kind {
        Kind::SearchBy(by) => {
            let name = args.next(r)?;
            (args.next(r)? == "root").then(|| by_root(by, name))
        }
        Kind::Search => {
            let (given, names) = grub_args(r, &mut args, SEARCH_OPTIONS, 1).ok()?;
            let set = given.iter().find(|(n, _)| *n == "set")?.1.clone();
            let (name, var) = match (names.into_iter().next(), set) {
                (Some(name), var) => (name, var.unwrap_or_else(|| String::from("root"))),
                (None, Some(name)) => (name, String::from("root")),
                (None, None) => return None,
            };
            if var != "root" {
                return None;
            }
            let has = |name: &str| given.iter().any(|(n, _)| *n == name);
            let by = if has("label") {
                By::Label
            } else if has("fs-uuid") {
                By::Uuid
            } else if has("file") {
                By::File
            } else {
                return None;
            };
            Some(by_root(by, name))
        }
        _ => None,
    }
}

/// The note a command that reads another file for the menu gets.
fn elsewhere(r: &mut Reading, text: &str, span: Span) {
    let mut name = Name::default();
    Lexer::over(text, span).next_word(&mut name);
    let cmd = name.literal().unwrap_or_default();
    r.note(if cmd == "blscfg" {
        String::from("`blscfg`: GRUB's menu from /loader/entries, which hvboot does not read")
    } else {
        alloc::format!("`{}`: another file GRUB reads for its menu, which hvboot does not", cmd)
    });
}

/// A command of the chosen entry's body: its `linux`, `initrd`, `search`
/// and `set root`.
fn entry_cmd(r: &mut Reading, text: &str, b: &mut Boot, kind: Kind, span: Span) {
    match kind {
        Kind::Linux => {
            let mut args = Args::new(text, span, true);
            let _ = args.next(r);
            b.kernel = args.next(r);
            b.args.clear();
            while let Some(a) = args.next(r) {
                b.args.push(' ');
                quote(&a, &mut b.args);
            }
            /* GRUB's `linux` unloads what was loaded: an initrd before it
             * is not this kernel's. */
            b.initrd_paths.clear();
        }
        Kind::Initrd => {
            let mut args = Args::new(text, span, true);
            let _ = args.next(r);
            b.initrd_paths.clear();
            while let Some(a) = args.next(r) {
                /* A NUL would end the argument in GRUB, a C string. */
                b.initrd_paths.push_str(a.split('\0').next().unwrap_or_default());
                b.initrd_paths.push('\0');
            }
        }
        Kind::Search | Kind::SearchBy(_) => {
            if let Some(root) = search(r, text, kind, span, true) {
                b.roots.push(root);
            }
        }
        Kind::Set | Kind::Assign => {
            if assignment(text, kind, span).is_some_and(|a| a.name.as_deref() == Some("root")) {
                let v = assigned(r, text, kind, span, true);
                b.roots.extend(root_named(&v));
            }
        }
        Kind::Elsewhere => elsewhere(r, text, span),
    }
}

/// A command of a level of the menu: its assignments, the default among
/// them, and the roots it sets.
fn menu_cmd(r: &mut Reading, text: &str, m: &mut Menu, conditional: bool, kind: Kind, span: Span) {
    match kind {
        Kind::Set | Kind::Assign => {
            let Some(a) = assignment(text, kind, span) else { return };
            match a.name.as_deref().unwrap_or_default() {
                /* The menu's default is the top level's: a submenu's own is
                 * what GRUB gives it from the top's, after its `>`. */
                "default" if m.nested => {}
                "default" if a.saved => m.saved = true,
                "default" if a.next => m.next = true,
                "default" => m.default = Some(assigned(r, text, kind, span, false)),
                "root" => {
                    let v = assigned(r, text, kind, span, false);
                    m.roots.extend(root_named(&v));
                }
                name if !conditional => {
                    let name = String::from(name);
                    let v = assigned(r, text, kind, span, false);
                    r.set(name, v);
                }
                _ => {}
            }
        }
        Kind::Search | Kind::SearchBy(_) => {
            if let Some(root) = search(r, text, kind, span, false) {
                m.roots.push(root);
            }
        }
        Kind::Elsewhere => elsewhere(r, text, span),
        Kind::Linux | Kind::Initrd => {}
    }
}

/// Read a grub.cfg: its menu, and its variables as the whole of it leaves
/// them. A `}` that closes no `{` -- which GRUB refuses the file for -- is
/// said, and the rest read on.
pub fn parse(text: &str) -> Config {
    let mut p = Parser::over(text, Span { from: 0, to: text.len() });
    let mut cfg = Config::default();
    while let End::Close(_) = p.block(&mut cfg.reading, &mut Ctx::Menu(&mut cfg.menu), false, &[]) {
        cfg.reading.note(String::from("a `}` that closes no `{`, which GRUB refuses the file for: read on past"));
    }
    cfg
}

/// The chosen entry's body, read -- now, its variables as the whole script
/// left them, as GRUB runs an entry's body after its script: its `linux`,
/// `initrd`, `search` and `set root`, every branch of its conditions in
/// turn.
pub fn read_entry(r: &mut Reading, text: &str, e: &Entry) -> Boot {
    let mut p = Parser::over(text, e.body);
    let mut b = Boot::default();
    while let End::Close(_) = p.block(r, &mut Ctx::Entry(&mut b), false, &[]) {}
    b
}

/// The signature GRUB's environment block begins with.
const ENV_SIGNATURE: &[u8] = b"# GRUB Environment Block\n";

/// Where the value that starts at `at` ends: at a newline not escaped --
/// GRUB writes a backslash before a backslash and before a newline in a
/// value -- or None when the block ends first.
fn value_end(b: &[u8], mut at: usize) -> Option<usize> {
    while at < b.len() {
        match b[at] {
            b'\n' => return Some(at),
            b'\\' => at += 2,
            _ => at += 1,
        }
    }
    None
}

/// grubenv, as GRUB reads it (`grub_envblk_iterate`): its signature first --
/// GRUB's `load_env` takes nothing from a file without it, and neither does
/// this (None) -- then `name=value` lines, a value running to a newline not
/// escaped, each `\X` in it read as X; `#` lines and the padding passed over.
/// A line with no `=`, or a value the block ends inside, ends the reading,
/// as GRUB's does.
pub fn parse_env(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let body = bytes.strip_prefix(ENV_SIGNATURE)?;
    let mut vars = Vec::new();
    let mut at = 0;
    while at < body.len() {
        if body[at] == b'#' {
            while at < body.len() && body[at] != b'\n' {
                at += 1;
            }
            at += 1;
            continue;
        }
        let Some(eq) = body[at..].iter().position(|&c| c == b'=').map(|n| at + n) else { break };
        let Some(end) = value_end(body, eq + 1) else { break };
        let mut value = Vec::with_capacity(end - eq - 1);
        let mut i = eq + 1;
        while i < end {
            if body[i] == b'\\' {
                i += 1;
            }
            if i < end {
                value.push(body[i]);
            }
            i += 1;
        }
        vars.push((String::from_utf8_lossy(&body[at..eq]).into_owned(), String::from_utf8_lossy(&value).into_owned()));
        at = end + 1;
    }
    Some(vars)
}

/// GRUB's environment block -- `text`, the file as it was read -- as its
/// `save_env` leaves it with `name` set empty: the same length, the
/// variable's line made `name=`, every other line as it was -- a comment,
/// such as the warning grub-editenv writes under the signature, too -- the
/// block padded with `#` to its length again. None for what is not a block
/// GRUB wrote, or has no `name` in it. A line ends at a newline that is not
/// escaped: GRUB writes a backslash before a newline in a value.
pub fn env_without(text: &[u8], name: &str) -> Option<Vec<u8>> {
    let body = text.strip_prefix(ENV_SIGNATURE)?;
    let mut out = Vec::with_capacity(text.len());
    out.extend_from_slice(ENV_SIGNATURE);
    let mut found = false;
    let mut at = 0usize;
    while at < body.len() {
        /* The line from `at`: to its unescaped newline, or the end */
        let end = value_end(body, at).map_or(body.len(), |e| e + 1).min(body.len());
        let line = &body[at..end];
        at = end;
        if line.starts_with(b"#") && !line.ends_with(b"\n") {
            /* The padding, the block's last: made again below */
            continue;
        }
        if line.strip_prefix(name.as_bytes()).is_some_and(|rest| rest.starts_with(b"=")) {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b"=\n");
            found = true;
        } else {
            out.extend_from_slice(line);
        }
    }
    if !found || out.len() > text.len() {
        return None;
    }
    out.resize(text.len(), b'#');
    /* Every other variable as it was, and this one empty: what is written
     * is held to that, not trusted to the scan above. */
    let mut was = parse_env(text)?;
    let mut now = parse_env(&out)?;
    was.retain(|(n, _)| n != name);
    if now.iter().any(|(n, v)| n == name && !v.is_empty()) {
        return None;
    }
    now.retain(|(n, _)| n != name);
    (was == now).then_some(out)
}

fn env_get<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

/// A number as GRUB reads `default` first (`grub_strtoul(s, 0, 0)`):
/// leading blanks passed over, `0x` hex, a `0` before another digit octal,
/// else decimal -- as many digits as there are, whatever follows them. None
/// for no digits at all, when GRUB looks for a title or an id instead.
fn grub_number(s: &str) -> Option<u64> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let b = s.as_bytes();
    let (base, digits) = match (b.first(), b.get(1)) {
        (Some(b'0'), Some(b'x')) => (16, &b[2..]),
        (Some(b'0'), Some(b'0'..=b'7')) => (8, b),
        _ => (10, b),
    };
    let mut n: u64 = 0;
    let mut found = false;
    for &c in digits {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            b'A'..=b'Z' => c - b'A' + 10,
            _ => break,
        };
        if u64::from(d) >= base {
            break;
        }
        found = true;
        /* GRUB stops on an overflow with an error: no entry by number. */
        n = n.saturating_mul(base).saturating_add(u64::from(d));
    }
    found.then_some(n)
}

/// Whether `spec` names `name` -- an entry's title or id -- at its first
/// level, as GRUB's `menuentry_eq` has it: the spec up to a `>` that is not
/// doubled, `>>` standing for a `>` of the name's own.
fn names(name: &str, spec: &str) -> bool {
    let (n, s) = (name.as_bytes(), spec.as_bytes());
    let (mut i, mut j) = (0, 0);
    loop {
        if s.get(j) == Some(&b'>') {
            if s.get(j + 1) != Some(&b'>') {
                return i == n.len();
            }
            j += 1;
        }
        match (n.get(i), s.get(j)) {
            (None, None) => return true,
            (a, b) if a != b => return false,
            _ => {}
        }
        i += 1;
        j += 1;
    }
}

/// What follows the first `>` of `spec` that is not doubled: the default a
/// submenu's own menu is given (GRUB's `grub_menu_execute_entry`) -- None,
/// GRUB's unset, when nothing does.
fn rest_of(spec: &str) -> Option<&str> {
    let b = spec.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'>' {
            if b.get(i + 1) == Some(&b'>') {
                i += 2;
                continue;
            }
            return Some(&spec[i + 1..]).filter(|r| !r.is_empty());
        }
        i += 1;
    }
    None
}

/// The item a level's `spec` names: by number, if it starts with one, as
/// GRUB reads it first -- else by title or id.
fn item_at<'a>(items: &'a [Item], spec: &str) -> Option<&'a Item> {
    if let Some(n) = grub_number(spec) {
        return usize::try_from(n).ok().and_then(|n| items.get(n));
    }
    items.iter().find(|it| {
        let (title, id) = it.title_and_id();
        names(title, spec) || id.is_some_and(|id| names(id, spec))
    })
}

/// The entry GRUB would boot, and what goes with it.
pub struct Choice<'a> {
    pub entry: &'a Entry,
    /// What named it.
    pub why: String,
    /// Whether it is grubenv's `next_entry`, which the script grub-mkconfig
    /// writes empties before it boots it (`set next_entry=` and `save_env
    /// next_entry`): the caller's to clear (`env_without`), or
    /// `grub-reboot`'s entry is booted for good.
    pub once: bool,
    /// The roots the submenus it is in set, outermost first: what it starts
    /// from after the top level's, before its own.
    pub roots: Vec<&'a [Root]>,
}

/// The entry `spec` names, as GRUB's `default` names one, if its first
/// level names an item: an entry there, or a submenu, in which what follows
/// the spec's first `>` is looked for the same way -- and, naming nothing,
/// the submenu's first item is taken, as GRUB takes it. Each such fall to a
/// first item said in `why`, after `how`.
fn find<'a>(items: &'a [Item], spec: &str, how: String) -> Option<Choice<'a>> {
    let mut item = item_at(items, spec)?;
    let mut spec = Some(spec);
    let mut fell: Vec<String> = Vec::new();
    let mut roots = Vec::new();
    loop {
        match item {
            Item::Entry(entry) => {
                let why = if fell.is_empty() { how } else { alloc::format!("{} ({})", how, fell.join("; ")) };
                return Some(Choice { entry, why, once: false, roots });
            }
            Item::Submenu(s) => {
                let (title, inner) = (&s.title, &s.items);
                roots.push(s.roots.as_slice());
                spec = spec.and_then(rest_of);
                item = match spec.and_then(|s| item_at(inner, s)) {
                    Some(it) => it,
                    None => {
                        if let Some(s) = spec {
                            fell.push(alloc::format!("nothing in \"{}\" being {}: its first", title, s));
                        }
                        inner.first()?
                    }
                };
            }
        }
    }
}

/// The entry GRUB would boot off `menu` -- or the one `wanted` names, from
/// `hvboot.entry=` -- and why that one.
pub fn choose<'a>(menu: &'a Menu, env: &[(String, String)], wanted: Option<&str>) -> Result<Choice<'a>, String> {
    if let Some(w) = wanted {
        return find(&menu.items, w, alloc::format!("hvboot.entry={}", w))
            .ok_or_else(|| alloc::format!("hvboot.entry={}: no such entry in grub.cfg", w));
    }
    let mut tried = Vec::new();
    if menu.next {
        if let Some(n) = env_get(env, "next_entry") {
            if let Some(c) = find(&menu.items, n, alloc::format!("grubenv's next_entry, {}", n)) {
                return Ok(Choice { once: true, ..c });
            }
            tried.push(alloc::format!("next_entry {}", n));
        }
    }
    if menu.saved {
        if let Some(n) = env_get(env, "saved_entry") {
            if let Some(c) = find(&menu.items, n, alloc::format!("grubenv's saved_entry, {}", n)) {
                return Ok(c);
            }
            tried.push(alloc::format!("saved_entry {}", n));
        }
    }
    if let Some(d) = &menu.default {
        if let Some(c) = find(&menu.items, d, alloc::format!("the default, {}", d)) {
            return Ok(c);
        }
        tried.push(alloc::format!("default {}", d));
    }
    let how = if tried.is_empty() {
        String::from("the first entry")
    } else {
        alloc::format!("the first entry, no entry being {}", tried.join(" or "))
    };
    find(&menu.items, "0", how).ok_or_else(|| String::from("grub.cfg has no menu entry"))
}

/// An argument onto `line` as GRUB's `linux` gives it to the kernel
/// (`grub_create_loader_cmdline`): in double quotes when it has a space in
/// it, and a backslash, a single and a double quote each with a backslash
/// before it.
pub fn quote(arg: &str, line: &mut String) {
    let space = arg.contains(' ');
    if space {
        line.push('"');
    }
    for c in arg.chars() {
        if matches!(c, '\\' | '\'' | '"') {
            line.push('\\');
        }
        line.push(c);
    }
    if space {
        line.push('"');
    }
}
