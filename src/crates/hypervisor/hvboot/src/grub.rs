//! GRUB's configuration, as much of it as says what GRUB would boot: the
//! menu entries -- in submenus too -- each with its `linux` and `initrd`
//! lines and the `search` or `set root` that names the filesystem they are
//! on; the `default`; and grubenv's `next_entry` and `saved_entry`, which a
//! generated configuration defers to.
//!
//! It is not GRUB's script interpreter. It reads the script's words as GRUB
//! does -- quotes, escapes, comments, variables -- and its blocks, but runs
//! no condition: every branch of an `if` is read, the last `search` and the
//! last `linux` an entry has win, and a variable has a value only where a
//! `set` outside any condition gives it one. What grub-mkconfig writes --
//! Debian's and Ubuntu's grub.cfg -- is read as GRUB reads it; anything
//! that needs more is said, not guessed.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A piece of a word: as written, or a variable's value.
#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Lit(String),
    Var(String),
}

type Word = Vec<Piece>;

#[derive(Clone, Debug)]
enum Tok {
    Word(Word),
    /// A newline or `;`.
    Sep,
    Open,
    Close,
}

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn push_char(word: &mut Word, c: char) {
    if let Some(Piece::Lit(s)) = word.last_mut() {
        s.push(c);
    } else {
        word.push(Piece::Lit(c.to_string()));
    }
}

/// Split the script into words, separators and braces.
fn lex(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut toks = Vec::new();
    let mut word: Word = Vec::new();
    let mut started = false;
    let mut quoted = false;
    let mut i = 0;
    let end_word = |toks: &mut Vec<Tok>, word: &mut Word, started: &mut bool, quoted: &mut bool| {
        if *started {
            let w = core::mem::take(word);
            let brace = if *quoted { None } else { match w.as_slice() {
                [Piece::Lit(s)] if s == "{" => Some(Tok::Open),
                [Piece::Lit(s)] if s == "}" => Some(Tok::Close),
                _ => None,
            } };
            toks.push(brace.unwrap_or(Tok::Word(w)));
        }
        *started = false;
        *quoted = false;
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                end_word(&mut toks, &mut word, &mut started, &mut quoted);
                i += 1;
            }
            '\n' | ';' => {
                end_word(&mut toks, &mut word, &mut started, &mut quoted);
                toks.push(Tok::Sep);
                i += 1;
            }
            '#' if !started => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\\' => {
                if chars.get(i + 1) == Some(&'\n') {
                    i += 2;
                } else if let Some(&n) = chars.get(i + 1) {
                    started = true;
                    push_char(&mut word, n);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '\'' => {
                started = true;
                quoted = true;
                if !matches!(word.last(), Some(Piece::Lit(_))) {
                    word.push(Piece::Lit(String::new()));
                }
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    push_char(&mut word, chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                started = true;
                quoted = true;
                if !matches!(word.last(), Some(Piece::Lit(_))) {
                    word.push(Piece::Lit(String::new()));
                }
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if matches!(chars.get(i + 1), Some('\\' | '"' | '$')) => {
                            push_char(&mut word, chars[i + 1]);
                            i += 2;
                        }
                        '\\' if chars.get(i + 1) == Some(&'\n') => i += 2,
                        '$' => i = variable(&chars, i, &mut word),
                        c => {
                            push_char(&mut word, c);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            '$' => {
                started = true;
                i = variable(&chars, i, &mut word);
            }
            c => {
                started = true;
                push_char(&mut word, c);
                i += 1;
            }
        }
    }
    end_word(&mut toks, &mut word, &mut started, &mut quoted);
    toks
}

/// A `$name` or `${name}` at `chars[at]`, into `word`: where the scan goes
/// on. A `$` that starts no name is the character itself.
fn variable(chars: &[char], at: usize, word: &mut Word) -> usize {
    if chars.get(at + 1) == Some(&'{') {
        if let Some(close) = chars[at + 2..].iter().position(|&c| c == '}') {
            let name: String = chars[at + 2..at + 2 + close].iter().collect();
            word.push(Piece::Var(name));
            return at + 3 + close;
        }
    }
    let n = chars[at + 1..].iter().take_while(|&&c| is_name(c)).count();
    if n == 0 {
        push_char(word, '$');
        return at + 1;
    }
    word.push(Piece::Var(chars[at + 1..at + 1 + n].iter().collect()));
    at + 1 + n
}

/// A statement of the script.
#[derive(Debug)]
enum Stmt {
    Cmd(Vec<Word>),
    /// Every branch: conditions are not run.
    Branches(Vec<Vec<Stmt>>),
    Menu { args: Vec<Word>, body: Vec<Stmt> },
    Submenu { args: Vec<Word>, body: Vec<Stmt> },
}

/// The literal text of a word, when it has no variable in it.
fn literal(w: &Word) -> Option<String> {
    let mut s = String::new();
    for p in w {
        match p {
            Piece::Lit(l) => s.push_str(l),
            Piece::Var(_) => return None,
        }
    }
    Some(s)
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    /// The words of a command after a keyword that began it -- `then echo`
    /// -- read as the next command.
    pending: Vec<Word>,
    /// How deep the blocks go: a script that nests past this is cut there.
    depth: u32,
}

const MAX_DEPTH: u32 = 32;

enum Next {
    Words(Vec<Word>),
    Open,
    Close,
    End,
}

impl Parser {
    /// The next command's words, or a brace, or the end.
    fn next(&mut self) -> Next {
        if !self.pending.is_empty() {
            return Next::Words(core::mem::take(&mut self.pending));
        }
        let mut words = Vec::new();
        while let Some(t) = self.toks.get(self.at) {
            match t {
                Tok::Word(w) => {
                    words.push(w.clone());
                    self.at += 1;
                }
                Tok::Sep => {
                    self.at += 1;
                    if !words.is_empty() {
                        break;
                    }
                }
                Tok::Open | Tok::Close => {
                    if !words.is_empty() {
                        break;
                    }
                    self.at += 1;
                    return if matches!(t, Tok::Open) { Next::Open } else { Next::Close };
                }
            }
        }
        if words.is_empty() {
            return Next::End;
        }
        /* A keyword that begins a block's body may share its command with
         * the body's first: `then set x=1`. */
        if let Some(k) = literal(&words[0]) {
            if matches!(k.as_str(), "then" | "else" | "do") && words.len() > 1 {
                self.pending = words.split_off(1);
            }
        }
        Next::Words(words)
    }

    /// Statements until a command whose first word is one of `stop` --
    /// which is returned -- or a closing brace, or the end.
    fn block(&mut self, stop: &[&str]) -> (Vec<Stmt>, Option<String>) {
        let mut out = Vec::new();
        self.depth += 1;
        loop {
            if self.depth > MAX_DEPTH {
                break;
            }
            let words = match self.next() {
                Next::Words(w) => w,
                Next::Open => {
                    /* A brace block of its own: read, and kept flat. */
                    let (inner, _) = self.block(&[]);
                    out.extend(inner);
                    continue;
                }
                Next::Close | Next::End => break,
            };
            let first = literal(&words[0]).unwrap_or_default();
            if stop.contains(&first.as_str()) {
                self.depth -= 1;
                return (out, Some(first));
            }
            match first.as_str() {
                "menuentry" | "submenu" => {
                    let body = if matches!(self.next(), Next::Open) { self.block(&[]).0 } else { Vec::new() };
                    let args = words[1..].to_vec();
                    out.push(if first == "menuentry" { Stmt::Menu { args, body } } else { Stmt::Submenu { args, body } });
                }
                "function" => {
                    if matches!(self.next(), Next::Open) {
                        let _ = self.block(&[]);
                    }
                }
                "if" => {
                    let mut branches = Vec::new();
                    loop {
                        /* The condition, up to `then`; then the branch, up to
                         * `elif`, `else` or `fi`. */
                        let (_, k) = self.block(&["then"]);
                        if k.is_none() {
                            break;
                        }
                        let (body, k) = self.block(&["elif", "else", "fi"]);
                        branches.push(body);
                        match k.as_deref() {
                            Some("elif") => continue,
                            Some("else") => {
                                let (body, _) = self.block(&["fi"]);
                                branches.push(body);
                                break;
                            }
                            _ => break,
                        }
                    }
                    out.push(Stmt::Branches(branches));
                }
                "for" | "while" | "until" => {
                    let _ = self.block(&["do"]);
                    let _ = self.block(&["done"]);
                }
                _ => out.push(Stmt::Cmd(words)),
            }
        }
        self.depth -= 1;
        (out, None)
    }
}

/// Where an entry's files are: the filesystem `search` names, or a disk
/// and partition as GRUB names them -- `hd0,gpt16` -- or the one the
/// configuration was read from.
#[derive(Clone, Debug, PartialEq)]
pub enum Root {
    Device { disk: u32, part: Option<u32> },
    FsUuid(String),
    Label(String),
    File(String),
}

#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub title: String,
    pub id: Option<String>,
    pub kernel: Option<String>,
    pub args: Vec<String>,
    pub initrds: Vec<String>,
    /// The `search`es and `set root`s, in order: the last that finds its
    /// filesystem wins, as GRUB's root is whichever was set last.
    pub roots: Vec<Root>,
}

#[derive(Debug)]
pub enum Item {
    Entry(Entry),
    Submenu { title: String, id: Option<String>, entries: Vec<Entry> },
}

#[derive(Debug, Default)]
pub struct Config {
    pub items: Vec<Item>,
    /// The `default` a `set` gave a literal value, the last of them.
    default: Option<String>,
    /// Whether `default` is set from grubenv's `saved_entry`, or deferred to
    /// its `next_entry` -- what grub-mkconfig writes for GRUB_DEFAULT=saved,
    /// and for every configuration.
    saved: bool,
    next: bool,
    /// Variables a `set` outside any condition gave a value.
    vars: Vec<(String, String)>,
    /// Variables used that had no value here, and so are empty.
    pub unknown: Vec<String>,
}

impl Config {
    fn var(&self, name: &str) -> Option<&str> {
        self.vars.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn expand(&mut self, w: &Word) -> String {
        let mut s = String::new();
        for p in w {
            match p {
                Piece::Lit(l) => s.push_str(l),
                Piece::Var(v) => {
                    let value = self.var(v).map(String::from);
                    match value {
                        Some(value) => s.push_str(&value),
                        None if !self.unknown.contains(v) => self.unknown.push(v.clone()),
                        None => {}
                    }
                }
            }
        }
        s
    }
}

/// `set NAME=VALUE`: the name, and the value's word.
fn assignment(words: &[Word]) -> Option<(String, Word)> {
    let w = words.get(1)?;
    let Some(Piece::Lit(first)) = w.first() else { return None };
    let (name, rest) = first.split_once('=')?;
    let mut value: Word = Vec::new();
    if !rest.is_empty() {
        value.push(Piece::Lit(String::from(rest)));
    }
    value.extend(w[1..].iter().cloned());
    Some((String::from(name), value))
}

fn uses(w: &Word, name: &str) -> bool {
    w.iter().any(|p| matches!(p, Piece::Var(v) if v == name))
}

/// The title and the id of a `menuentry` or `submenu`: its first word, and
/// `--id X`, `--id=X` or `$menuentry_id_option X` among the rest.
fn title_and_id(cfg: &mut Config, args: &[Word]) -> (String, Option<String>) {
    let title = args.first().map(|w| cfg.expand(w)).unwrap_or_default();
    let mut id = None;
    let mut i = 1;
    while i < args.len() {
        let w = &args[i];
        if matches!(w.as_slice(), [Piece::Var(v)] if v == "menuentry_id_option") || literal(w).as_deref() == Some("--id") {
            if let Some(next) = args.get(i + 1) {
                id = Some(cfg.expand(next));
                i += 1;
            }
        } else if let Some(v) = literal(w).as_deref().and_then(|l| l.strip_prefix("--id=")) {
            id = Some(String::from(v));
        }
        i += 1;
    }
    (title, id)
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

/// What a `search` sets root to: by UUID, label or a file on it.
fn search(cfg: &mut Config, words: &[Word]) -> Option<Root> {
    let cmd = literal(&words[0]).unwrap_or_default();
    let mut kind = match cmd.as_str() {
        "search.fs_uuid" => "uuid",
        "search.fs_label" => "label",
        "search.file" => "file",
        _ => "file",
    };
    let mut var = String::from("root");
    let mut names = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let w = cfg.expand(&words[i]);
        match w.as_str() {
            "--fs-uuid" | "-u" => kind = "uuid",
            "--label" | "-l" => kind = "label",
            "--file" | "-f" => kind = "file",
            "--no-floppy" | "-n" | "--efidisk-only" => {}
            "--set" | "-s" => var = String::from("root"),
            "-h" => i += 1,
            w if w.starts_with("--set=") => var = String::from(&w[6..]),
            w if w.starts_with("--hint") => {}
            w if w.starts_with('-') => {}
            w => names.push(String::from(w)),
        }
        i += 1;
    }
    /* `search.fs_uuid UUID VAR`: the variable after the name. */
    if cmd.starts_with("search.") && names.len() == 2 {
        var = names.pop().unwrap_or_default();
    }
    if var != "root" {
        return None;
    }
    let name = names.into_iter().next()?;
    Some(match kind {
        "uuid" => Root::FsUuid(name),
        "label" => Root::Label(name),
        _ => Root::File(name),
    })
}

/// An entry's body: its `linux`, `initrd`, `search` and `set root`, every
/// branch of its conditions read in turn.
fn entry_body(cfg: &mut Config, body: &[Stmt], e: &mut Entry) {
    for s in body {
        match s {
            Stmt::Cmd(words) => {
                let cmd = literal(&words[0]).unwrap_or_default();
                match cmd.as_str() {
                    "linux" | "linux16" | "linuxefi" => {
                        e.kernel = words.get(1).map(|w| cfg.expand(w));
                        e.args = words[2.min(words.len())..].iter().map(|w| cfg.expand(w))
                            .filter(|a| !a.is_empty()).collect();
                    }
                    "initrd" | "initrd16" | "initrdefi" => {
                        e.initrds = words[1..].iter().map(|w| cfg.expand(w)).filter(|a| !a.is_empty()).collect();
                    }
                    "search" | "search.fs_uuid" | "search.fs_label" | "search.file" => {
                        if let Some(r) = search(cfg, words) {
                            e.roots.push(r);
                        }
                    }
                    "set" => {
                        if let Some((name, value)) = assignment(words) {
                            if name == "root" {
                                let v = cfg.expand(&value);
                                if let Some(r) = device(&v) {
                                    e.roots.push(r);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Stmt::Branches(branches) => {
                for b in branches {
                    entry_body(cfg, b, e);
                }
            }
            Stmt::Menu { .. } | Stmt::Submenu { .. } => {}
        }
    }
}

/// The top level: variables, `default`, and the menu.
fn top(cfg: &mut Config, body: &[Stmt], conditional: bool, items: &mut Vec<Item>) {
    for s in body {
        match s {
            Stmt::Cmd(words) => {
                if literal(&words[0]).as_deref() != Some("set") {
                    continue;
                }
                let Some((name, value)) = assignment(words) else { continue };
                if name == "default" {
                    if uses(&value, "saved_entry") {
                        cfg.saved = true;
                    } else if uses(&value, "next_entry") {
                        cfg.next = true;
                    } else if let Some(l) = literal(&value) {
                        cfg.default = Some(l);
                    }
                } else if !conditional {
                    let v = cfg.expand(&value);
                    cfg.vars.push((name, v));
                }
            }
            Stmt::Branches(branches) => {
                for b in branches {
                    top(cfg, b, true, items);
                }
            }
            Stmt::Menu { args, body } => {
                let (title, id) = title_and_id(cfg, args);
                let mut e = Entry { title, id, ..Entry::default() };
                entry_body(cfg, body, &mut e);
                items.push(Item::Entry(e));
            }
            Stmt::Submenu { args, body } => {
                let (title, id) = title_and_id(cfg, args);
                let mut inner = Vec::new();
                top(cfg, body, conditional, &mut inner);
                let entries = inner.into_iter().filter_map(|i| match i { Item::Entry(e) => Some(e), _ => None }).collect();
                items.push(Item::Submenu { title, id, entries });
            }
        }
    }
}

/// Read a grub.cfg.
pub fn parse(text: &str) -> Config {
    let mut p = Parser { toks: lex(text), at: 0, pending: Vec::new(), depth: 0 };
    let (body, _) = p.block(&[]);
    let mut cfg = Config::default();
    let mut items = Vec::new();
    top(&mut cfg, &body, false, &mut items);
    cfg.items = items;
    cfg
}

/// grubenv: `name=value` lines, backslash escapes undone; the rest padding.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else { continue };
        let mut v = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => v.push('\n'),
                    Some(o) => v.push(o),
                    None => {}
                }
            } else {
                v.push(c);
            }
        }
        vars.push((String::from(name), v));
    }
    vars
}

fn env_get<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

/// The entry `spec` names, as GRUB's `default` takes one: a number, a
/// title or an id, and `A>B` for an entry in a submenu.
fn find<'a>(items: &'a [Item], spec: &str) -> Option<&'a Entry> {
    let (head, rest) = match spec.split_once('>') {
        Some((h, r)) => (h, Some(r)),
        None => (spec, None),
    };
    let pick = |title: &str, id: &Option<String>, i: usize| {
        head.parse::<usize>().ok() == Some(i) || title == head || id.as_deref() == Some(head)
    };
    for (i, item) in items.iter().enumerate() {
        match item {
            Item::Entry(e) if rest.is_none() && pick(&e.title, &e.id, i) => return Some(e),
            Item::Submenu { title, id, entries } if pick(title, id, i) => {
                let inner = rest.unwrap_or("0");
                return entries.iter().enumerate()
                    .find(|(j, e)| inner.parse::<usize>().ok() == Some(*j) || e.title == inner
                          || e.id.as_deref() == Some(inner))
                    .map(|(_, e)| e);
            }
            _ => {}
        }
    }
    None
}

/// The entry GRUB would boot -- or the one `wanted` names, from
/// `hvboot.entry=` -- and why that one.
pub fn choose<'a>(cfg: &'a Config, env: &[(String, String)], wanted: Option<&str>)
    -> Result<(&'a Entry, String), String>
{
    if let Some(w) = wanted {
        return match find(&cfg.items, w) {
            Some(e) => Ok((e, alloc::format!("hvboot.entry={}", w))),
            None => Err(alloc::format!("hvboot.entry={}: no such entry in grub.cfg", w)),
        };
    }
    let mut tried = Vec::new();
    if cfg.next {
        if let Some(n) = env_get(env, "next_entry") {
            if let Some(e) = find(&cfg.items, n) {
                return Ok((e, alloc::format!("grubenv's next_entry, {}", n)));
            }
            tried.push(alloc::format!("next_entry {}", n));
        }
    }
    if cfg.saved {
        if let Some(n) = env_get(env, "saved_entry") {
            if let Some(e) = find(&cfg.items, n) {
                return Ok((e, alloc::format!("grubenv's saved_entry, {}", n)));
            }
            tried.push(alloc::format!("saved_entry {}", n));
        }
    }
    if let Some(d) = &cfg.default {
        if let Some(e) = find(&cfg.items, d) {
            return Ok((e, alloc::format!("the default, {}", d)));
        }
        tried.push(alloc::format!("default {}", d));
    }
    match find(&cfg.items, "0") {
        Some(e) if tried.is_empty() => Ok((e, String::from("the first entry"))),
        Some(e) => Ok((e, alloc::format!("the first entry, no entry being {}", tried.join(" or ")))),
        None => Err(String::from("grub.cfg has no menu entry")),
    }
}
