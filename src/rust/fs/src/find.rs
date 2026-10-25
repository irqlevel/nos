//! `find`: the paths under a directory that an expression holds for, one a
//! line, the way find(1) prints them. The tests are find(1)'s that mean
//! something here -- a name or a whole path against a shell pattern, the
//! type, the size, emptiness -- joined by `!`, `-a` and `-o` and grouped by
//! parentheses, and the options that bound the walk: `-maxdepth`,
//! `-mindepth` and `-xdev`.
//!
//! Nothing here recurses deeper than a fixed bound, whatever it is given:
//! the walk keeps its place in each directory on a stack of its own, as
//! deep as a path can be long, and the expression is parsed into postfix --
//! at most `MAX_OPS` steps, nested at most `MAX_NESTING` deep -- which is
//! evaluated over a fixed array. Tests have no side effects, so evaluating
//! every one of them, with no short circuit, answers as find(1) does.
//!
//! The tree is read the way `ls` reads it, an entry at a time by its index
//! and with no lock held between two: what is made or removed in a
//! directory while it is walked may be seen or not. A directory the
//! filesystem cannot read is said, never taken for an empty one.

use core::fmt::Write;
use core::iter::Peekable;
use core::str::SplitWhitespace;

use alloc::vec::Vec;

use kcore::cmd::Output;

use crate::paths::Path;
use crate::vfs::{DirError, Vfs, MAX_PATH};
use crate::vnode::Kind;
use crate::vfs_instance;

const USAGE: &str = "usage: find [path...] [expression]";
const TESTS: &str = "  tests: -name -iname -path -ipath <pattern>, -type f|d, -size [+|-]N[c|K|M|G|T], -empty\n  \
                     options: -maxdepth N, -mindepth N, -xdev\n  \
                     operators: ! -not, -a -and (or nothing), -o -or, ( )";

/// The steps of an expression in postfix -- its tests, options and
/// operators: room for far more than a line of the shell spells out.
const MAX_OPS: usize = 64;

/// How deep parentheses and `!` may nest: what bounds the parser's
/// recursion.
const MAX_NESTING: usize = 16;

/// How deep a walk can go: every level adds a '/' and a name of a byte at
/// least to a path shorter than `MAX_PATH`.
const MAX_DEPTH: usize = MAX_PATH / 2;

#[derive(Clone, Copy)]
enum Cmp {
    Less,
    Equal,
    More,
}

#[derive(Clone, Copy)]
enum Test<'a> {
    /// The name -- the last component of the path -- against a pattern;
    /// true to match ASCII letters whatever their case
    Name(&'a str, bool),
    /// The whole path, as it is printed, against a pattern
    Path(&'a str, bool),
    Type(Kind),
    /// A file's size in bytes against a count; a directory has no size
    Size(Cmp, usize),
    /// A file of no bytes, or a directory of no entries
    Empty,
    /// An option in the expression, which holds for everything
    True,
}

#[derive(Clone, Copy)]
enum Op<'a> {
    Test(Test<'a>),
    Not,
    And,
    Or,
}

/// What bounds the walk, wherever in the expression it was given.
struct Options {
    min_depth: usize,
    max_depth: usize,
    /// Not into a directory on another mount than the walk began on
    xdev: bool,
}

/// What a path is asked.
struct Entry<'p> {
    path: &'p str,
    name: &'p str,
    kind: Kind,
    size: usize,
    empty: bool,
}

/// Why an expression could not be taken.
enum Bad<'a> {
    /// A word that is no test, option or operator
    Unknown(&'a str),
    /// A test or an option with no argument after it
    NoArgument(&'a str),
    /// A test or an option with an argument it does not take
    Argument(&'a str, &'a str),
    /// The expression goes on where a test was due -- at its end when
    /// `None`
    Expected(Option<&'a str>),
    Unclosed,
    Unopened,
    TooLong,
    TooDeep,
    NoMemory,
}

impl core::fmt::Display for Bad<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Bad::Unknown(word) if !word.starts_with('-') => {
                write!(f, "'{}' is no test: the paths come before the expression", word)
            }
            Bad::Unknown(word) => write!(f, "unknown test '{}'", word),
            Bad::NoArgument(test) => write!(f, "{} needs an argument", test),
            Bad::Argument(test, arg) => write!(f, "{} does not take '{}'", test, arg),
            Bad::Expected(Some(word)) => write!(f, "a test was expected before '{}'", word),
            Bad::Expected(None) => write!(f, "the expression ends where a test was expected"),
            Bad::Unclosed => write!(f, "a '(' is not closed"),
            Bad::Unopened => write!(f, "a ')' with no '(' before it"),
            Bad::TooLong => write!(f, "more than {} tests and operators", MAX_OPS),
            Bad::TooDeep => write!(f, "parentheses and '!' nested more than {} deep", MAX_NESTING),
            Bad::NoMemory => write!(f, "no memory for the expression"),
        }
    }
}

/* ---- the expression ---- */

struct Expr<'a> {
    /// In postfix, every operator after its operands
    ops: Vec<Op<'a>>,
    /// Whether a directory has to be read to be asked about: only `-empty`
    /// does
    needs_empty: bool,
}

impl Expr<'_> {
    /// Whether the expression holds for the entry: an empty one, for any.
    fn holds(&self, entry: &Entry) -> bool {
        let mut stack = [false; MAX_OPS];
        let mut depth = 0;
        for op in &self.ops {
            match *op {
                Op::Test(test) => {
                    stack[depth] = test.holds(entry);
                    depth += 1;
                }
                Op::Not => stack[depth - 1] = !stack[depth - 1],
                Op::And => {
                    depth -= 1;
                    stack[depth - 1] &= stack[depth];
                }
                Op::Or => {
                    depth -= 1;
                    stack[depth - 1] |= stack[depth];
                }
            }
        }
        depth == 0 || stack[0]
    }
}

impl Test<'_> {
    fn holds(self, entry: &Entry) -> bool {
        match self {
            Test::Name(pattern, fold) => matches(pattern, entry.name, fold),
            Test::Path(pattern, fold) => matches(pattern, entry.path, fold),
            Test::Type(kind) => entry.kind == kind,
            Test::Size(cmp, count) => entry.kind == Kind::File && match cmp {
                Cmp::Less => entry.size < count,
                Cmp::Equal => entry.size == count,
                Cmp::More => entry.size > count,
            },
            Test::Empty => entry.empty,
            Test::True => true,
        }
    }
}

/// The expression, by recursive descent over find(1)'s grammar, tightest
/// first: `( )`, then `!`, then `-a` -- or two tests side by side -- then
/// `-o`.
struct Parser<'a> {
    words: Peekable<SplitWhitespace<'a>>,
    ops: Vec<Op<'a>>,
    options: Options,
    needs_empty: bool,
    nesting: usize,
}

impl<'a> Parser<'a> {
    fn parse(words: SplitWhitespace<'a>) -> Result<(Expr<'a>, Options), Bad<'a>> {
        let mut ops = Vec::new();
        /* All of it now, so that nothing below allocates: a push past what
         * was reserved is refused before it is made. */
        if ops.try_reserve_exact(MAX_OPS).is_err() {
            return Err(Bad::NoMemory);
        }
        let mut parser = Parser {
            words: words.peekable(),
            ops,
            options: Options { min_depth: 0, max_depth: usize::MAX, xdev: false },
            needs_empty: false,
            nesting: 0,
        };

        if parser.peek().is_some() {
            parser.or()?;
            /* `or` stops at the end or at a ')' -- one that closes nothing */
            if parser.peek().is_some() {
                return Err(Bad::Unopened);
            }
        }
        Ok((Expr { ops: parser.ops, needs_empty: parser.needs_empty }, parser.options))
    }

    fn peek(&mut self) -> Option<&'a str> {
        self.words.peek().copied()
    }

    fn emit(&mut self, op: Op<'a>) -> Result<(), Bad<'a>> {
        if self.ops.len() >= MAX_OPS {
            return Err(Bad::TooLong);
        }
        self.ops.push(op);
        Ok(())
    }

    fn enter(&mut self) -> Result<(), Bad<'a>> {
        if self.nesting >= MAX_NESTING {
            return Err(Bad::TooDeep);
        }
        self.nesting += 1;
        Ok(())
    }

    /// and ( -o and )*
    fn or(&mut self) -> Result<(), Bad<'a>> {
        self.and()?;
        while matches!(self.peek(), Some("-o" | "-or")) {
            self.words.next();
            self.and()?;
            self.emit(Op::Or)?;
        }
        Ok(())
    }

    /// not ( [-a] not )*
    fn and(&mut self) -> Result<(), Bad<'a>> {
        self.not()?;
        loop {
            match self.peek() {
                None | Some(")" | "-o" | "-or") => return Ok(()),
                Some("-a" | "-and") => { self.words.next(); }
                Some(_) => {}
            }
            self.not()?;
            self.emit(Op::And)?;
        }
    }

    /// ! not | primary
    fn not(&mut self) -> Result<(), Bad<'a>> {
        if !matches!(self.peek(), Some("!" | "-not")) {
            return self.primary();
        }
        self.words.next();
        self.enter()?;
        self.not()?;
        self.nesting -= 1;
        self.emit(Op::Not)
    }

    /// ( or ) | a test | an option
    fn primary(&mut self) -> Result<(), Bad<'a>> {
        let word = match self.peek() {
            None => return Err(Bad::Expected(None)),
            Some(word @ (")" | "-o" | "-or" | "-a" | "-and")) => return Err(Bad::Expected(Some(word))),
            Some(word) => word,
        };
        self.words.next();

        if word == "(" {
            self.enter()?;
            self.or()?;
            if self.words.next() != Some(")") {
                return Err(Bad::Unclosed);
            }
            self.nesting -= 1;
            return Ok(());
        }

        let test = self.test(word)?;
        self.emit(Op::Test(test))
    }

    fn argument(&mut self, test: &'a str) -> Result<&'a str, Bad<'a>> {
        self.words.next().ok_or(Bad::NoArgument(test))
    }

    fn test(&mut self, word: &'a str) -> Result<Test<'a>, Bad<'a>> {
        Ok(match word {
            "-name" => Test::Name(self.argument(word)?, false),
            "-iname" => Test::Name(self.argument(word)?, true),
            "-path" => Test::Path(self.argument(word)?, false),
            "-ipath" => Test::Path(self.argument(word)?, true),
            "-type" => match self.argument(word)? {
                "f" => Test::Type(Kind::File),
                "d" => Test::Type(Kind::Dir),
                other => return Err(Bad::Argument(word, other)),
            },
            "-size" => {
                let arg = self.argument(word)?;
                let (cmp, count) = parse_size(arg).ok_or(Bad::Argument(word, arg))?;
                Test::Size(cmp, count)
            }
            "-empty" => {
                self.needs_empty = true;
                Test::Empty
            }
            "-maxdepth" | "-mindepth" => {
                let arg = self.argument(word)?;
                let depth = match arg.as_bytes().first() {
                    Some(digit) if digit.is_ascii_digit() => arg.parse().ok(),
                    _ => None,
                };
                let depth = depth.ok_or(Bad::Argument(word, arg))?;
                if word == "-maxdepth" {
                    self.options.max_depth = depth;
                } else {
                    self.options.min_depth = depth;
                }
                Test::True
            }
            "-xdev" | "-mount" => {
                self.options.xdev = true;
                Test::True
            }
            _ => return Err(Bad::Unknown(word)),
        })
    }
}

/// `-size`'s argument: `+N` more than N bytes, `-N` fewer, `N` exactly --
/// bytes as they are or with a `c` after them, or 1024 of them for each of
/// `K`, `M`, `G` and `T`. Not find(1)'s blocks of 512, rounded up: a size
/// here is what `ls` and `stat` print.
fn parse_size(text: &str) -> Option<(Cmp, usize)> {
    let (cmp, count) = match text.as_bytes().first()? {
        b'+' => (Cmp::More, &text[1..]),
        b'-' => (Cmp::Less, &text[1..]),
        _ => (Cmp::Equal, text),
    };
    if !count.as_bytes().first()?.is_ascii_digit() {
        return None;
    }
    let count = match count.strip_suffix('c') {
        Some(digits) if digits.bytes().all(|b| b.is_ascii_digit()) => digits.parse().ok()?,
        Some(_) => return None,
        None => crate::shell::parse_bytes(count)?,
    };
    Some((cmp, count))
}

/* ---- patterns ---- */

/// One element of a pattern, and where the pattern goes on after it.
enum Element<'a> {
    Star(usize),
    One(Set<'a>, usize),
}

/// What one character of the text has to be.
enum Set<'a> {
    Any,
    Char(char),
    /// Between a `[` and its `]`, and whether `!` or `^` negates it
    Class(&'a str, bool),
}

/// The element at `at` of the pattern; `None` at its end.
fn element(pattern: &str, at: usize) -> Option<Element<'_>> {
    let rest = &pattern[at..];
    let mut chars = rest.chars();
    Some(match chars.next()? {
        '*' => Element::Star(at + 1),
        '?' => Element::One(Set::Any, at + 1),
        '\\' => match chars.next() {
            Some(escaped) => Element::One(Set::Char(escaped), at + 1 + escaped.len_utf8()),
            None => Element::One(Set::Char('\\'), at + 1),
        },
        '[' => match class(rest) {
            Some((body, negated, len)) => Element::One(Set::Class(body, negated), at + len),
            None => Element::One(Set::Char('['), at + 1),
        },
        c => Element::One(Set::Char(c), at + c.len_utf8()),
    })
}

/// The set a `[` at the start of `rest` opens: what is between it and the
/// `]` that closes it, whether it is negated, and how long the whole of it
/// is -- `None` when nothing closes it. A `]` first is a member, and so is
/// a character after a `\`. ']' and '\' are ASCII, so no byte of another
/// character is taken for either.
fn class(rest: &str) -> Option<(&str, bool, usize)> {
    let bytes = rest.as_bytes();
    let negated = matches!(bytes.get(1), Some(b'!' | b'^'));
    let start = if negated { 2 } else { 1 };
    let mut at = start;
    if bytes.get(at) == Some(&b']') {
        at += 1;
    }
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b']' => return Some((&rest[start..at], negated, at + 1)),
            _ => at += 1,
        }
    }
    None
}

fn same(a: char, b: char, fold: bool) -> bool {
    a == b || (fold && a.eq_ignore_ascii_case(&b))
}

fn within(c: char, low: char, high: char, fold: bool) -> bool {
    let inside = |x: char| low <= x && x <= high;
    inside(c) || (fold && (inside(c.to_ascii_lowercase()) || inside(c.to_ascii_uppercase())))
}

/// Whether `c` is one of the class: characters, and ranges `a-z` -- a `-`
/// first or last is itself.
fn in_class(body: &str, c: char, fold: bool) -> bool {
    let mut chars = body.chars();
    while let Some(mut low) = chars.next() {
        if low == '\\' {
            low = chars.next().unwrap_or('\\');
        }
        let mut high = low;
        let mut ahead = chars.clone();
        if ahead.next() == Some('-') {
            if let Some(mut end) = ahead.next() {
                if end == '\\' {
                    end = ahead.next().unwrap_or('\\');
                }
                high = end;
                chars = ahead;
            }
        }
        if within(c, low, high, fold) {
            return true;
        }
    }
    false
}

impl Set<'_> {
    fn holds(&self, c: char, fold: bool) -> bool {
        match *self {
            Set::Any => true,
            Set::Char(want) => same(want, c, fold),
            Set::Class(body, negated) => in_class(body, c, fold) != negated,
        }
    }
}

/// Whether the shell pattern `pattern` matches the whole of `text`, as
/// fnmatch(3) does with no flags: `*` any run of characters, '/' among
/// them; `?` any one; `[...]` one of a set -- `a-z` a range, `!` or `^`
/// first for one not in it -- and `\` the next character as itself. A `[`
/// that nothing closes is itself. `fold` matches ASCII letters whatever
/// their case.
///
/// The walk with one place to go back to: a `*` remembers where it stood,
/// and a mismatch after it starts over from there with one more character
/// taken by the `*`. Every other element takes exactly one character,
/// which is what makes the last `*` the only one worth going back to --
/// and the whole match at most the product of the two lengths.
fn matches(pattern: &str, text: &str, fold: bool) -> bool {
    let (mut p, mut t) = (0, 0);
    /* Where the pattern goes on after the last `*`, and where in the text
     * that was last tried */
    let mut star: Option<(usize, usize)> = None;

    loop {
        match element(pattern, p) {
            Some(Element::Star(next)) => {
                star = Some((next, t));
                p = next;
                continue;
            }
            Some(Element::One(set, next)) => {
                if let Some(c) = text[t..].chars().next() {
                    if set.holds(c, fold) {
                        p = next;
                        t += c.len_utf8();
                        continue;
                    }
                }
            }
            None if t == text.len() => return true,
            None => {}
        }

        match star {
            Some((after, tried)) => match text[tried..].chars().next() {
                Some(c) => {
                    star = Some((after, tried + c.len_utf8()));
                    p = after;
                    t = tried + c.len_utf8();
                }
                None => return false,
            },
            None => return false,
        }
    }
}

/* ---- the walk ---- */

/// Where the walk is in one directory: how long the directory's path is,
/// and which of its entries comes next.
struct Level {
    len: usize,
    next: usize,
}

struct Finder<'e, 'a> {
    vfs: &'static Vfs,
    expr: &'e Expr<'a>,
    options: &'e Options,
    /// Where the walk is, a directory a level
    levels: Vec<Level>,
}

/// The name a starting path is asked by: its last component, or `/` for
/// the root.
fn start_name(start: &str) -> &str {
    let trimmed = start.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    match trimmed.rfind('/') {
        Some(at) => &trimmed[at + 1..],
        None => trimmed,
    }
}

fn dir_error(why: DirError) -> &'static str {
    match why {
        DirError::NotFound => "no longer there",
        DirError::NotDir => "no longer a directory",
        DirError::Unreadable => "the filesystem cannot read this directory",
    }
}

impl Finder<'_, '_> {
    /// Print the path if the expression holds for it, and it is deep enough
    /// to be asked.
    fn visit(&self, path: &str, name: &str, kind: Kind, size: usize, depth: usize, out: &mut Output) {
        if depth < self.options.min_depth {
            return;
        }

        let empty = match kind {
            Kind::File => size == 0,
            Kind::Dir if !self.expr.needs_empty => false,
            Kind::Dir => match self.vfs.dir_entry(path.as_bytes(), 0) {
                Ok(first) => first.is_none(),
                Err(why) => {
                    let _ = writeln!(out, "find: {}: {}", path, dir_error(why));
                    return;
                }
            },
        };

        if self.expr.holds(&Entry { path, name, kind, size, empty }) {
            let _ = writeln!(out, "{}", path);
        }
    }

    /// Walk the tree from `start`, printing every path the expression holds
    /// for, and saying where it was whatever could not be walked or asked.
    fn walk(&mut self, start: &str, out: &mut Output) {
        let mut path = match Path::from(start) {
            Some(path) => path,
            None => {
                let _ = writeln!(out, "find: {}: path too long", start);
                return;
            }
        };
        let st = match self.vfs.stat(start.as_bytes()) {
            Some(st) => st,
            None => {
                let _ = writeln!(out, "find: {}: not found", start);
                return;
            }
        };
        let mount = self.vfs.mount_id(start.as_bytes());

        self.visit(start, start_name(start), st.kind, st.size, 0, out);

        self.levels.clear();
        if st.kind == Kind::Dir && self.options.max_depth > 0 {
            self.levels.push(Level { len: path.len(), next: 0 });
        }

        while let Some(level) = self.levels.last_mut() {
            path.truncate(level.len);
            let entry = match self.vfs.dir_entry(path.as_bytes(), level.next) {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    self.levels.pop();
                    continue;
                }
                Err(why) => {
                    let _ = writeln!(out, "find: {}: {}", path.as_str(), dir_error(why));
                    self.levels.pop();
                    continue;
                }
            };
            level.next += 1;
            let depth = self.levels.len();

            /* A name the VFS would resolve as something else -- or not at
             * all -- is said, and left alone: walked, it would be another
             * path than the entry's. No filesystem here makes one; a
             * damaged one might. */
            let name = match core::str::from_utf8(entry.name()) {
                Ok(name) if !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0']) => name,
                Ok(name) => {
                    let _ = writeln!(out, "find: {}: an entry named '{}' cannot be walked", path.as_str(), name);
                    continue;
                }
                Err(_) => {
                    let _ = writeln!(out, "find: {}: an entry whose name is not UTF-8", path.as_str());
                    continue;
                }
            };

            let dir_len = path.len();
            let slash = !path.as_str().ends_with('/');
            if (slash && path.push("/").is_none()) || path.push(name).is_none() {
                path.truncate(dir_len);
                let _ = writeln!(out, "find: {}{}{}: path too long", path.as_str(), if slash { "/" } else { "" },
                                 name);
                continue;
            }

            self.visit(path.as_str(), name, entry.kind, entry.size, depth, out);

            if entry.kind != Kind::Dir || depth >= self.options.max_depth {
                continue;
            }
            if self.options.xdev && self.vfs.mount_id(path.as_bytes()) != mount {
                continue;
            }
            /* Never short of room: each level is a '/' and a name longer */
            if self.levels.len() >= MAX_DEPTH {
                let _ = writeln!(out, "find: {}: too deep", path.as_str());
                continue;
            }
            self.levels.push(Level { len: path.len(), next: 0 });
        }
    }
}

/// `find [path...] [expression]`: the paths are the words before the first
/// that begins with `-`, or is `!` or `(`; none is `/`.
pub fn find(args: &str, out: &mut Output) {
    let is_path = |word: &&str| !word.starts_with('-') && *word != "!" && *word != "(";
    let paths = args.split_whitespace().take_while(is_path).count();

    let mut words = args.split_whitespace();
    for _ in 0..paths {
        words.next();
    }
    if matches!(words.clone().next(), Some("-help" | "--help")) {
        let _ = writeln!(out, "{}\n{}", USAGE, TESTS);
        return;
    }

    let (expr, options) = match Parser::parse(words) {
        Ok(parsed) => parsed,
        Err(bad) => {
            let _ = writeln!(out, "find: {}", bad);
            let _ = writeln!(out, "{}", USAGE);
            return;
        }
    };

    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => return,
    };
    let mut levels = Vec::new();
    if levels.try_reserve_exact(MAX_DEPTH).is_err() {
        let _ = writeln!(out, "find: no memory for the walk");
        return;
    }

    let mut finder = Finder { vfs, expr: &expr, options: &options, levels };
    if paths == 0 {
        finder.walk("/", out);
    }
    for start in args.split_whitespace().take(paths) {
        finder.walk(start, out);
    }
}
