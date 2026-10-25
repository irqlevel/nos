//! `find`, run as the shell runs it over the filesystem a target works, and
//! held to a model of its own: an expression of the input's -- now and then
//! words that are none -- and what the command prints held to what the
//! model's tree gives, worked out here from the command's documentation
//! alone. Its parser builds a tree where the kernel's emits postfix, its
//! patterns are matched by dynamic programming where the kernel's go back
//! to the last `*`, and its walk recurses over the model where the kernel's
//! keeps a stack of directory indices over the VFS.

use std::collections::HashMap;

use crate::machine::cmd;
use crate::model::Node;
use crate::targets::fsops::{self, say, Fs, Res};
use crate::{reached, Input};

/// What the kernel's command documents of its bounds.
const MAX_PATH: usize = 256;
const MAX_OPS: usize = 64;
const MAX_NESTING: usize = 16;
const USAGE: &str = "usage: find [path...] [expression]";

/* ---- what the input says ---- */

const PATTERNS: &[&str] = &[
    "*", "a", "a*", "*a*", "?", "??*", "[ab]*", "[!a]*", "[^a]*", "[a-c]", "*.txt", "x.t?t", "\\*", "[", "[]a]*",
    "*[", "A*", "N*", "*/d*", "/*", "*b", "d*/*", "lost+found", "*n*n*n*", "[z-a]", "[-a]*", "[a-]", "\\", "[\\]]",
    "[!]", "[]", "D", "?*?", "*X*", "f[0-9]*", "SUB", "*/*/*", "*.TXT", "[A-C]*", "[!A-Z]", "*\\.txt", "fil?",
];

fn pattern(r: &mut Input) -> String {
    if r.chance(64) {
        const ALPHABET: &[char] = &['a', 'b', '*', '?', '[', ']', '!', '^', '-', '\\', '.', 'x', 'd', 'n', '/', 'A', 'f'];
        (0..r.range(1, 8)).map(|_| r.pick(ALPHABET)).collect()
    } else {
        r.pick(PATTERNS).to_string()
    }
}

fn size(r: &mut Input) -> String {
    let sign = r.pick(&["", "+", "-", "", "+", "-", "++", "+-"]);
    let count = r.pick(&["0", "1", "5", "9", "63", "100", "4095", "4096", "9000", "65536", "99999999999999999999",
                         "18446744073709551615", "", "x"]);
    let unit = r.pick(&["", "", "", "c", "k", "K", "M", "g", "T", "Kc", "cc", "b"]);
    format!("{}{}{}", sign, count, unit)
}

fn primary(r: &mut Input, depth: u32, words: &mut Vec<String>) {
    match r.u8() % 16 {
        0 | 1 => {
            words.push(r.pick(&["-name", "-iname"]).into());
            words.push(pattern(r));
        }
        2 => {
            words.push(r.pick(&["-path", "-ipath"]).into());
            words.push(pattern(r));
        }
        3 | 4 => {
            words.push("-type".into());
            words.push(r.pick(&["f", "d", "f", "d", "x", "fd"]).into());
        }
        5 => {
            words.push("-size".into());
            words.push(size(r));
        }
        6 => words.push("-empty".into()),
        7 => {
            words.push("-maxdepth".into());
            words.push(r.pick(&["0", "1", "2", "3", "5", "x", "-1", "+1"]).into());
        }
        8 => {
            words.push("-mindepth".into());
            words.push(r.pick(&["0", "1", "2", "3"]).into());
        }
        9 => words.push(r.pick(&["-xdev", "-mount"]).into()),
        10 | 11 if depth < 4 => {
            words.push("(".into());
            expression(r, depth + 1, words);
            words.push(")".into());
        }
        12 if depth < 4 => {
            words.push(r.pick(&["!", "-not"]).into());
            primary(r, depth + 1, words);
        }
        _ => {
            words.push("-name".into());
            words.push(pattern(r));
        }
    }
}

fn expression(r: &mut Input, depth: u32, words: &mut Vec<String>) {
    primary(r, depth, words);
    for _ in 0..r.below(4) {
        match r.u8() % 6 {
            0 => words.push("-o".into()),
            1 => words.push("-or".into()),
            2 => words.push("-a".into()),
            3 => words.push("-and".into()),
            _ => {}
        }
        primary(r, depth, words);
    }
}

/// What follows the paths: an expression, or words that are none -- one
/// out of place, too deep, too long.
fn words(r: &mut Input) -> Vec<String> {
    let mut words = Vec::new();
    match r.u8() % 16 {
        0 => {}
        1 => {
            let deep = r.range(14, 18) as usize;
            for _ in 0..deep {
                words.push(r.pick(&["!", "("]).into());
            }
            words.push("-empty".into());
            let closing = words.iter().filter(|w| *w == "(").count();
            for _ in 0..closing {
                words.push(")".into());
            }
        }
        2 => {
            for _ in 0..r.range(28, 36) {
                words.push("-empty".into());
                words.push("-o".into());
            }
            words.push("-type".into());
            words.push("d".into());
        }
        _ => expression(r, 0, &mut words),
    }
    if r.chance(40) {
        let odd = r.pick(&["(", ")", "!", "-o", "-a", "-bogus", "/x", "-name", "-type", "-size", "-maxdepth", "-help"]);
        let at = r.below(words.len() as u64 + 1) as usize;
        words.insert(at, odd.into());
    }
    if r.chance(20) && !words.is_empty() {
        let at = r.below(words.len() as u64) as usize;
        words.remove(at);
    }
    words
}

/// A path to start at: a node of the tree's when its path can be typed --
/// text, and no space in it -- or one that is no node; `/` otherwise.
fn start(r: &mut Input, f: &Fs) -> String {
    let path = fsops::path(r, &f.model);
    match String::from_utf8(path) {
        Ok(p) if !p.is_empty() && !p.contains(|c: char| c.is_whitespace() || c.is_control()) && !p.starts_with('-')
                 && p != "!" && p != "(" => p,
        _ => "/".into(),
    }
}

/* ---- the model ---- */

#[derive(Debug)]
enum Expr {
    Name(Vec<Glob>, bool),
    Path(Vec<Glob>, bool),
    Type(bool),
    Size(std::cmp::Ordering, u64),
    Empty,
    True,
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

struct Options {
    min_depth: usize,
    max_depth: usize,
}

/// A pattern's element.
#[derive(Debug)]
enum Glob {
    Star,
    Any,
    Char(char),
    Class(bool, Vec<(char, char)>),
}

fn compile(pattern: &str) -> Vec<Glob> {
    let c: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '*' => out.push(Glob::Star),
            '?' => out.push(Glob::Any),
            '\\' if i + 1 < c.len() => {
                i += 1;
                out.push(Glob::Char(c[i]));
            }
            '[' => {
                if let Some((class, end)) = class(&c, i) {
                    out.push(class);
                    i = end;
                    continue;
                }
                out.push(Glob::Char('['));
            }
            ch => out.push(Glob::Char(ch)),
        }
        i += 1;
    }
    out
}

/// The class a `[` at `open` makes, and where the pattern goes on after
/// it; `None` when nothing closes it.
fn class(c: &[char], open: usize) -> Option<(Glob, usize)> {
    let mut i = open + 1;
    let negated = matches!(c.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let first = i;
    let mut close = i;
    if c.get(close) == Some(&']') {
        close += 1;
    }
    loop {
        match c.get(close)? {
            '\\' => close += 2,
            ']' => break,
            _ => close += 1,
        }
    }
    let body = &c[first..close];
    let take = |k: &mut usize| {
        let ch = body[*k];
        *k += 1;
        if ch == '\\' && *k < body.len() {
            *k += 1;
            return body[*k - 1];
        }
        ch
    };
    let mut ranges = Vec::new();
    let mut k = 0;
    while k < body.len() {
        let low = take(&mut k);
        if k + 1 < body.len() && body[k] == '-' {
            k += 1;
            let high = take(&mut k);
            ranges.push((low, high));
        } else {
            ranges.push((low, low));
        }
    }
    Some((Glob::Class(negated, ranges), close + 1))
}

fn one(g: &Glob, c: char, fold: bool) -> bool {
    match g {
        Glob::Star | Glob::Any => true,
        Glob::Char(want) => *want == c || (fold && want.eq_ignore_ascii_case(&c)),
        Glob::Class(negated, ranges) => {
            let hit = ranges.iter().any(|&(low, high)| {
                let inside = |x: char| low <= x && x <= high;
                inside(c) || (fold && (inside(c.to_ascii_lowercase()) || inside(c.to_ascii_uppercase())))
            });
            hit != *negated
        }
    }
}

/// Whether the pattern matches the whole text: whether each suffix of the
/// pattern matches each suffix of the text, from the ends back.
fn glob(p: &[Glob], text: &str, fold: bool) -> bool {
    let t: Vec<char> = text.chars().collect();
    let (n, m) = (p.len(), t.len());
    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[n][m] = true;
    for i in (0..n).rev() {
        for j in (0..=m).rev() {
            dp[i][j] = match &p[i] {
                Glob::Star => dp[i + 1][j] || (j < m && dp[i][j + 1]),
                g => j < m && one(g, t[j], fold) && dp[i + 1][j + 1],
            };
        }
    }
    dp[0][0]
}

/// find's grammar, as its documentation has it -- `( )`, then `!`, then
/// `-a` or nothing, then `-o` -- with the command's bounds: the tests and
/// operators counted, and `(` and `!` nested no deeper than it takes.
struct Parser<'w> {
    words: &'w [&'w str],
    at: usize,
    ops: usize,
    nesting: usize,
    options: Options,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.words.get(self.at).copied()
    }

    fn take(&mut self) -> Option<&str> {
        self.at += 1;
        self.words.get(self.at - 1).copied()
    }

    fn op(&mut self, e: Expr) -> Option<Expr> {
        self.ops += 1;
        (self.ops <= MAX_OPS).then_some(e)
    }

    fn deeper(&mut self) -> Option<()> {
        self.nesting += 1;
        (self.nesting <= MAX_NESTING).then_some(())
    }

    fn or(&mut self) -> Option<Expr> {
        let mut e = self.and()?;
        while matches!(self.peek(), Some("-o" | "-or")) {
            self.at += 1;
            let right = self.and()?;
            e = self.op(Expr::Or(Box::new(e), Box::new(right)))?;
        }
        Some(e)
    }

    fn and(&mut self) -> Option<Expr> {
        let mut e = self.not()?;
        loop {
            match self.peek() {
                None | Some(")" | "-o" | "-or") => return Some(e),
                Some("-a" | "-and") => self.at += 1,
                Some(_) => {}
            }
            let right = self.not()?;
            e = self.op(Expr::And(Box::new(e), Box::new(right)))?;
        }
    }

    fn not(&mut self) -> Option<Expr> {
        if matches!(self.peek(), Some("!" | "-not")) {
            self.at += 1;
            self.deeper()?;
            let e = self.not()?;
            self.nesting -= 1;
            return self.op(Expr::Not(Box::new(e)));
        }
        match self.take()? {
            ")" | "-o" | "-or" | "-a" | "-and" => None,
            "(" => {
                self.deeper()?;
                let e = self.or()?;
                if self.take()? != ")" {
                    return None;
                }
                self.nesting -= 1;
                Some(e)
            }
            word => {
                let word = word.to_string();
                let e = self.test(&word)?;
                self.op(e)
            }
        }
    }

    fn test(&mut self, word: &str) -> Option<Expr> {
        Some(match word {
            "-name" | "-iname" => Expr::Name(compile(self.take()?), word == "-iname"),
            "-path" | "-ipath" => Expr::Path(compile(self.take()?), word == "-ipath"),
            "-type" => match self.take()? {
                "f" => Expr::Type(false),
                "d" => Expr::Type(true),
                _ => return None,
            },
            "-size" => {
                let (cmp, count) = size_arg(self.take()?)?;
                Expr::Size(cmp, count)
            }
            "-empty" => Expr::Empty,
            "-maxdepth" | "-mindepth" => {
                let arg = self.take()?;
                if arg.is_empty() || !arg.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let depth: usize = arg.parse().ok()?;
                if word == "-maxdepth" {
                    self.options.max_depth = depth;
                } else {
                    self.options.min_depth = depth;
                }
                Expr::True
            }
            "-xdev" | "-mount" => Expr::True,
            _ => return None,
        })
    }
}

/// `-size`'s argument: a sign, digits, and a unit -- none or `c` for bytes,
/// K, M, G, T (either case) for powers of 1024.
fn size_arg(arg: &str) -> Option<(std::cmp::Ordering, u64)> {
    use std::cmp::Ordering::*;
    let (cmp, rest) = match arg.as_bytes().first()? {
        b'+' => (Greater, &arg[1..]),
        b'-' => (Less, &arg[1..]),
        _ => (Equal, arg),
    };
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let (digits, unit) = rest.split_at(end);
    if digits.is_empty() {
        return None;
    }
    let count: u64 = digits.parse().ok()?;
    let shift = match unit {
        "" | "c" => 0,
        "K" | "k" => 10,
        "M" | "m" => 20,
        "G" | "g" => 30,
        "T" | "t" => 40,
        _ => return None,
    };
    Some((cmp, count.checked_mul(1 << shift)?))
}

/// What the expression is asked of a path.
struct Entry<'p> {
    path: &'p str,
    name: &'p str,
    node: &'p Node,
}

fn holds(e: &Expr, at: &Entry) -> bool {
    match e {
        Expr::Name(p, fold) => glob(p, at.name, *fold),
        Expr::Path(p, fold) => glob(p, at.path, *fold),
        Expr::Type(dir) => at.node.is_dir() == *dir,
        Expr::Size(cmp, count) => match at.node {
            Node::File(d) => d.len().cmp(count) == *cmp,
            Node::Dir(_) => false,
        },
        Expr::Empty => match at.node {
            Node::File(d) => d.len() == 0,
            Node::Dir(c) => c.is_empty(),
        },
        Expr::True => true,
        Expr::Not(a) => !holds(a, at),
        Expr::And(a, b) => holds(a, at) && holds(b, at),
        Expr::Or(a, b) => holds(a, at) || holds(b, at),
    }
}

struct Walk<'m> {
    expr: Option<&'m Expr>,
    options: &'m Options,
    lines: Vec<String>,
}

impl Walk<'_> {
    fn visit(&mut self, path: &str, name: &str, node: &Node, depth: usize) {
        if depth >= self.options.min_depth && self.expr.map_or(true, |e| holds(e, &Entry { path, name, node })) {
            self.lines.push(path.to_string());
        }
    }

    fn under(&mut self, dir: &str, children: &std::collections::BTreeMap<Vec<u8>, Node>, depth: usize) {
        for (name, child) in children {
            let name = match std::str::from_utf8(name) {
                Ok(name) => name,
                Err(_) => {
                    self.lines.push(format!("find: {}: an entry whose name is not UTF-8", dir));
                    reached("find: an entry whose name is not text");
                    continue;
                }
            };
            let path = if dir.ends_with('/') { format!("{}{}", dir, name) } else { format!("{}/{}", dir, name) };
            if path.len() >= MAX_PATH {
                self.lines.push(format!("find: {}: path too long", path));
                reached("find: a path too long");
                continue;
            }
            self.visit(&path, name, child, depth);
            if let Node::Dir(c) = child {
                if depth < self.options.max_depth {
                    self.under(&path, c, depth + 1);
                }
            }
        }
    }

    fn walk(&mut self, start: &str, f: &Fs) {
        if start.len() >= MAX_PATH {
            self.lines.push(format!("find: {}: path too long", start));
            return;
        }
        let node = match f.model.resolve(start.as_bytes()) {
            Res::Node(n) => f.model.node(&n),
            _ => None,
        };
        let Some(node) = node else {
            self.lines.push(format!("find: {}: not found", start));
            return;
        };
        let trimmed = start.trim_end_matches('/');
        let name = if trimmed.is_empty() { "/" } else { trimmed.rsplit('/').next().unwrap_or(trimmed) };
        self.visit(start, name, node, 0);
        if let Node::Dir(c) = node {
            if self.options.max_depth > 0 {
                self.under(start, c, 1);
            }
        }
    }
}

/// Every path printed after any it was found under: the walk goes down a
/// directory before it goes on past it, and prints a path before what is
/// under it.
fn preorder(lines: &[&str]) -> bool {
    let mut at: HashMap<&str, usize> = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        at.entry(line).or_insert(i);
    }
    lines.iter().enumerate().filter(|(_, line)| !line.starts_with("find: ")).all(|(i, line)| {
        let mut up = *line;
        while let Some(cut) = up.trim_end_matches('/').rfind('/') {
            up = if cut == 0 { "/" } else { &up[..cut] };
            if at.get(up).is_some_and(|&j| j > i) {
                return false;
            }
            if up == "/" {
                break;
            }
        }
        true
    })
}

pub fn find(f: &mut Fs, r: &mut Input) {
    let mut starts = Vec::new();
    for _ in 0..r.below(3) {
        starts.push(start(r, f));
    }
    let words = words(r);
    let line = format!("find {} {}", starts.join(" "), words.join(" "));
    let got = cmd::run(&line);
    say(format_args!("{} -> {} lines", line, got.lines().count()));
    if !f.strict {
        return;
    }

    /* What the command sees: the words of the line, the paths first. */
    let all: Vec<&str> = line.split_whitespace().skip(1).collect();
    let paths = all.iter().take_while(|w| !w.starts_with('-') && **w != "!" && **w != "(").count();
    let (paths, rest) = all.split_at(paths);

    if matches!(rest.first(), Some(&("-help" | "--help"))) {
        invariant!(got.starts_with(USAGE), "{:?} printed {:?}, and no usage", line, got);
        return;
    }

    let mut parser = Parser { words: rest, at: 0, ops: 0, nesting: 0,
                              options: Options { min_depth: 0, max_depth: usize::MAX } };
    let expr = if rest.is_empty() {
        Some(None)
    } else {
        parser.or().filter(|_| parser.at == rest.len()).map(Some)
    };
    let Some(expr) = expr else {
        let lines: Vec<&str> = got.lines().collect();
        invariant!(lines.len() == 2 && lines[0].starts_with("find: ") && lines[1] == USAGE,
                   "{:?} is no expression, and printed {:?}", line, got);
        reached("find: an expression refused");
        return;
    };

    let mut walk = Walk { expr: expr.as_ref(), options: &parser.options, lines: Vec::new() };
    if paths.is_empty() {
        walk.walk("/", f);
    }
    for start in paths {
        walk.walk(start, f);
    }

    let printed: Vec<&str> = got.lines().collect();
    let mut have = printed.clone();
    have.sort_unstable();
    let mut want: Vec<&str> = walk.lines.iter().map(String::as_str).collect();
    want.sort_unstable();
    invariant!(have == want, "{:?} printed {:?}, and the model has {:?}", line, have, want);
    invariant!(paths.len() > 1 || preorder(&printed), "{:?} printed a path before one it is under: {:?}", line, printed);

    if !want.is_empty() {
        reached("find: paths printed, as the model has them");
    }
    if want.len() > 3 {
        reached("find: more than three paths printed");
    }
    if paths.len() > 1 {
        reached("find: two paths walked");
    }
}
