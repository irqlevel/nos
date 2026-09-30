//! A filesystem mounted at "/", worked through the VFS as the kernel's
//! callers work it -- the shell, a module's file ABI -- and the model of
//! what each call should do: the tree, the files open on it, and the rules
//! the VFS and the filesystem under it document. Every call's answer is
//! held to the model's, and so is the whole tree, read back through the
//! VFS, whenever a target asks.

use fs::vfs::{FileStat, Handle, Vfs, OPEN_APPEND, OPEN_CREATE, OPEN_READ, OPEN_TRUNCATE, OPEN_WRITE};
use fs::vnode::Kind;

use crate::model::{join, show, Data, Node};
use crate::{reached, Input};

/// A name is shorter than this, in the VFS and every filesystem under it.
pub const NAME_MAX: usize = 64;

/// What the filesystem under the VFS holds at most.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// A file's size: a write or a truncate past it is refused whole.
    pub max_file: u64,
    /// A directory's entries.
    pub max_entries: usize,
    /// How far into a file the input's offsets go: the limit, where there
    /// is one, and what memory is sure to hold where there is none.
    pub span: u64,
}

impl Limits {
    /// A filesystem's limits where it has them.
    pub fn fixed(max_file: u64, max_entries: usize) -> Limits {
        Limits { max_file, max_entries, span: max_file }
    }

    /// A filesystem in memory: none but memory's.
    pub fn memory() -> Limits {
        Limits { max_file: u64::MAX, max_entries: usize::MAX, span: 16 << 20 }
    }
}

/// A file open, as the model has it.
#[derive(Clone, Debug)]
pub struct Opened {
    pub path: Vec<Vec<u8>>,
    pub pos: u64,
    pub flags: usize,
}

/// What a path comes to, as the VFS resolves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Res {
    /// Nothing: no mount holds it, a name too long, a component missing or
    /// not a directory.
    Invalid,
    /// A node, by its path from the root.
    Node(Vec<Vec<u8>>),
    /// Nothing yet, where something could be made: the directory, and the
    /// name.
    Absent(Vec<Vec<u8>>, Vec<u8>),
}

pub struct Model {
    pub tree: Node,
    pub read_only: bool,
    pub limits: Limits,
    /// The kernel's handles, and the model's view of each.
    pub open: Vec<(Handle, Opened)>,
}

impl Model {
    pub fn new(tree: Node, read_only: bool, limits: Limits) -> Model {
        Model { tree, read_only, limits, open: Vec::new() }
    }

    pub fn resolve(&self, path: &[u8]) -> Res {
        let Some(rest) = path.strip_prefix(b"/") else { return Res::Invalid };
        let rest = rest.strip_prefix(b"/").unwrap_or(rest);
        let comps: Vec<&[u8]> = rest.split(|b| *b == b'/').filter(|c| !c.is_empty()).collect();
        let mut at: Vec<Vec<u8>> = Vec::new();
        for (k, c) in comps.iter().enumerate() {
            if c.len() >= NAME_MAX || c.contains(&0) {
                return Res::Invalid;
            }
            let last = k + 1 == comps.len();
            if *c == b"." || *c == b".." {
                if *c == b".." {
                    at.pop();
                }
                if last {
                    return Res::Node(at);
                }
                continue;
            }
            let mut child = at.clone();
            child.push(c.to_vec());
            match self.tree.get(&child) {
                Some(_) if last => return Res::Node(child),
                None if last => return Res::Absent(at, c.to_vec()),
                Some(n) if n.is_dir() => at = child,
                _ => return Res::Invalid,
            }
        }
        Res::Node(at)
    }

    pub fn node(&self, p: &[Vec<u8>]) -> Option<&Node> {
        self.tree.get(p)
    }

    pub fn size(&self, p: &[Vec<u8>]) -> u64 {
        match self.node(p) {
            Some(Node::File(d)) => d.len(),
            _ => 0,
        }
    }

    /// Whether anything at or under `p` is open.
    pub fn busy(&self, p: &[Vec<u8>]) -> bool {
        self.open.iter().any(|(_, o)| o.path.starts_with(p))
    }

    /// A node made in directory `dir`: whether the filesystem has room for
    /// the entry.
    pub fn make(&mut self, dir: &[Vec<u8>], name: &[u8], node: Node) -> bool {
        let limit = self.limits.max_entries;
        let Some(children) = self.tree.get_mut(dir).and_then(Node::children_mut) else { return false };
        if children.len() >= limit {
            return false;
        }
        children.insert(name.to_vec(), node);
        true
    }

    /// What `Vfs::open` should do, done to the model: the file open.
    pub fn open(&mut self, p: &[u8], flags: usize) -> Option<Opened> {
        let m = self;
        let mut flags = flags;
        if flags & OPEN_APPEND != 0 {
            flags |= OPEN_WRITE;
        }
        if flags & (OPEN_READ | OPEN_WRITE) == 0 {
            return None;
        }
        let writes = flags & (OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE) != 0;
        let res = m.resolve(p);
        if res == Res::Invalid || (writes && m.read_only) {
            return None;
        }
        let path = match res {
            Res::Node(n) => n,
            Res::Absent(dir, name) => {
                if flags & OPEN_CREATE == 0 || !m.make(&dir, &name, Node::File(Data::new())) {
                    return None;
                }
                let mut n = dir;
                n.push(name);
                n
            }
            Res::Invalid => return None,
        };
        let size = match m.node(&path) {
            Some(Node::File(d)) => d.len(),
            _ => return None,
        };
        if flags & OPEN_TRUNCATE != 0 && size != 0 {
            m.file_mut(&path).expect("a file").truncate(0);
        }
        let pos = if flags & OPEN_APPEND != 0 { m.size(&path) } else { 0 };
        Some(Opened { path, pos, flags })
    }

    /// What `Vfs::rename` should do, done to the model: whether it is.
    pub fn rename(&mut self, from: &[u8], to: &[u8]) -> bool {
        let m = self;
        if !from.starts_with(b"/") || !to.starts_with(b"/") {
            return false;
        }
        let Res::Node(src) = m.resolve(from) else { return false };
        if m.read_only || src.is_empty() || m.busy(&src) {
            return false;
        }
        let Res::Absent(dir, name) = m.resolve(to) else { return false };
        if dir.starts_with(&src) {
            return false;
        }
        let (sname, sdir) = src.split_last().expect("not the root");
        let full = m.tree.get(&dir).and_then(Node::children).is_some_and(|c| c.len() >= m.limits.max_entries)
            && sdir != dir.as_slice();
        if full {
            return false;
        }
        let node = m.tree.get_mut(sdir).and_then(Node::children_mut).and_then(|c| c.remove(sname));
        let Some(node) = node else { return false };
        m.tree.get_mut(&dir).and_then(Node::children_mut).expect("resolved").insert(name, node);
        true
    }

    /// What `Vfs::write_file` should do, done to the model.
    pub fn write_file(&mut self, p: &[u8], data: &[u8]) -> bool {
        let m = self;
        let res = m.resolve(p);
        if res == Res::Invalid || m.read_only {
            return false;
        }
        let path = match res {
            Res::Node(n) => n,
            Res::Absent(dir, name) => {
                if !m.make(&dir, &name, Node::File(Data::new())) {
                    return false;
                }
                let mut n = dir;
                n.push(name);
                n
            }
            Res::Invalid => return false,
        };
        let limit = m.limits.max_file;
        let Some(d) = m.file_mut(&path) else { return false };
        d.truncate(0);
        if data.len() as u64 > limit {
            return false;
        }
        d.write(0, data);
        true
    }


    /// What `Vfs::create` should do, done to the model.
    pub fn create(&mut self, p: &[u8], dir: bool) -> bool {
        match self.resolve(p) {
            Res::Absent(d, name) if !self.read_only => {
                self.make(&d, &name, if dir { Node::dir() } else { Node::File(Data::new()) })
            }
            _ => false,
        }
    }

    /// What `Vfs::remove` should do, done to the model.
    pub fn remove(&mut self, p: &[u8]) -> bool {
        match self.resolve(p) {
            Res::Node(n) if !n.is_empty() && !self.read_only && !self.busy(&n) => {
                let (name, dir) = n.split_last().expect("not the root");
                self.tree.get_mut(dir).and_then(Node::children_mut).is_some_and(|c| c.remove(name).is_some())
            }
            _ => false,
        }
    }

    /// What `Vfs::stat` should answer: a directory or not, and the size.
    pub fn stat(&self, p: &[u8]) -> Option<(bool, u64)> {
        match self.resolve(p) {
            Res::Node(n) => self.node(&n).map(|x| (x.is_dir(), self.size(&n))),
            _ => None,
        }
    }

    /// A file's content, if `p` is one.
    pub fn content(&self, p: &[u8]) -> Option<Data> {
        match self.resolve(p) {
            Res::Node(n) => match self.node(&n) {
                Some(Node::File(d)) => Some(d.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn file_mut(&mut self, p: &[Vec<u8>]) -> Option<&mut Data> {
        match self.tree.get_mut(p) {
            Some(Node::File(d)) => Some(d),
            _ => None,
        }
    }
}

/// The filesystem as a target drives it: the VFS, the model, and whether
/// the model is to be held to (a filesystem whose image is damaged has
/// none).
pub struct Fs {
    pub vfs: &'static Vfs,
    pub model: Model,
    /// Every answer held to the model's.
    pub strict: bool,
}

/* ---- what the input chooses ---- */

const NAMES: &[&[u8]] = &[b"a", b"b", b"c", b"dir", b"file", b"x.txt", b"lost+found", b"sub", b"\xff\xfe", b"a b"];

pub fn name(r: &mut Input) -> Vec<u8> {
    match r.u8() % 16 {
        0 => vec![b'n'; 63],
        1 => vec![b'n'; 64],
        2 => r.pick(&[&b"."[..], b"..", b"a\0b", b"\0"]).to_vec(),
        3 => (0..r.range(1, 12)).map(|_| b'a' + (r.u8() % 26)).collect(),
        _ => r.pick(NAMES).to_vec(),
    }
}

/// A path: most often one the tree has, or one beside it; now and then one
/// with dots in it, or doubled slashes, or none at the start.
pub fn path(r: &mut Input, m: &Model) -> Vec<u8> {
    /* A node of the tree's: down from the root, a child at a time, as
     * far as the input says. */
    let mut comps: Vec<Vec<u8>> = Vec::new();
    let mut at = &m.tree;
    while let Some(children) = at.children() {
        if children.is_empty() || r.u8() % 4 == 3 {
            break;
        }
        let k = r.below(children.len() as u64) as usize;
        let (name, child) = children.iter().nth(k).expect("in range");
        comps.push(name.clone());
        at = child;
    }
    match r.u8() % 8 {
        0 | 1 => comps.push(name(r)),
        2 => {
            if !comps.is_empty() {
                comps.pop();
            }
        }
        _ => {}
    }
    let mut p = join(&comps);
    match r.u8() % 16 {
        0 => p.extend_from_slice(b"/"),
        1 => p.extend_from_slice(b"/."),
        2 => p.extend_from_slice(b"/.."),
        3 => p.insert(0, b'/'),
        4 => {
            p.remove(0);
        }
        5 => {
            let mut q = b"/./".to_vec();
            q.extend_from_slice(&p);
            p = q;
        }
        6 => p = r.pick(&[&b""[..], b"/", b"//", b"/..", b"/../..", b"x"]).to_vec(),
        _ => {}
    }
    p
}

/// How much a write writes.
pub fn length(r: &mut Input) -> usize {
    match r.u8() % 8 {
        0 => 0,
        1 => 1,
        2 => r.pick(&[511, 512, 513, 1023, 1024, 1025, 4095, 4096, 4097]),
        3 => r.below(65536) as usize,
        _ => r.below(3000) as usize,
    }
}

/// Where in a file something happens: its start, its end, a block's edge
/// near either, or somewhere far past it.
pub fn offset(r: &mut Input, size: u64, block: u64, max: u64) -> u64 {
    let edge = |r: &mut Input, at: u64| (at + r.below(3)).saturating_sub(1);
    let at = match r.u8() % 10 {
        0 => 0,
        1 => size,
        2 => size.saturating_sub(r.below(3)),
        3 => {
            let k = size / block + r.below(3);
            edge(r, k * block)
        }
        4 => edge(r, 12 * block),
        5 => edge(r, (12 + block / 4) * block),
        6 => max.saturating_sub(r.below(8192)),
        7 => r.below(1 << 20),
        _ => r.below(size + 8192),
    };
    at.min(max.saturating_add(4096))
}

/* ---- the calls, each held to the model ---- */

/// Whether the filesystem ran out of room -- blocks, inodes -- in what was
/// traced since `mark`: what makes a call fail that the model, which does
/// not count them, has succeed.
fn out_of_room(mark: u64) -> bool {
    crate::machine::traced_since(mark).iter().any(|l| {
        let l = String::from_utf8_lossy(l);
        l.contains("no free blocks") || l.contains("no free inodes") || l.contains("no free data blocks")
    })
}

/// Whether `got` is what a write of `data` at `at` that stopped part way
/// may leave of `old`: its size, and within it the first bytes of `data`
/// and then what was there.
fn partial_write(old: &Data, got: &Data, at: u64, data: &[u8]) -> bool {
    if got.len() != old.len() {
        return false;
    }
    let end = (at + data.len() as u64).min(old.len()).max(at.min(old.len()));
    let start = at.min(old.len());
    if got.read(0, start) != old.read(0, start) || got.read(end, u64::MAX) != old.read(end, u64::MAX) {
        return false;
    }
    let mid = got.read(start, end - start);
    let k = mid.iter().zip(data).position(|(g, d)| g != d).unwrap_or(mid.len());
    mid[k..] == old.read(start + k as u64, end - start - k as u64)[..]
}

impl Fs {
    fn check(&self, ok: bool, what: std::fmt::Arguments) {
        if self.strict && !ok {
            panic!("invariant: {}", what);
        }
    }

    /// A call the kernel refused and the model did not, for want of room:
    /// the model as it was before, `saved`, and true -- or false, when it
    /// was no such thing.
    fn refused_for_room(&mut self, got: bool, want: bool, mark: u64, saved: &Node) -> bool {
        if got || !want || !out_of_room(mark) {
            return false;
        }
        self.model.tree = saved.clone();
        reached("a call refused for want of room");
        true
    }

    /// A write to `path` refused for want of room: the file must be as a
    /// write that stopped part way leaves it, and the model takes what it
    /// is.
    fn resync_partial(&mut self, path: &[Vec<u8>], old: &Data, at: u64, data: &[u8]) {
        /* What the kernel has of the file: the stretch the write was at,
         * and the rest of it where it is not zeros, or should not be. */
        let p = join(path);
        let Some(file) = fs::vfs::Open::new(self.vfs, &p, OPEN_READ) else {
            self.check(false, format_args!("{} does not open after a write to it ran out of room", show(&p)));
            return;
        };
        let size = file.size() as u64;
        let start = at.min(size);
        let end = (at + data.len() as u64).min(size);
        let mut got = old.clone();
        got.truncate(size);
        let mut ranges = vec![(start, end - start)];
        for b in old.nonzero_blocks(4096) {
            ranges.push((b * 4096, 4096));
        }
        for (off, n) in ranges {
            let n = n.min(size.saturating_sub(off));
            let mut buf = vec![0u8; n as usize];
            let mut k = 0;
            if file.seek(off as usize) {
                while k < buf.len() {
                    match file.read(&mut buf[k..]) {
                        Some(0) | None => break,
                        Some(r) => k += r,
                    }
                }
            }
            got.write(off, &buf[..k]);
        }
        drop(file);
        self.check(partial_write(old, &got, at, data), format_args!("a write of {} bytes at {} to {} ({} bytes) that \
                   ran out of room left {} bytes, not the old ones with a first part of the new", data.len(), at,
                   show(&p), old.len(), got.len()));
        if let Some(d) = self.model.file_mut(path) {
            *d = got;
        }
    }

    pub fn stat(&mut self, p: &[u8]) {
        let got = self.vfs.stat(p);
        let want = match self.model.resolve(p) {
            Res::Node(n) => self.model.node(&n).map(|x| (x.is_dir(), self.model.size(&n))),
            _ => None,
        };
        let got2 = got.as_ref().map(|s: &FileStat| (s.kind == Kind::Dir, s.size as u64));
        self.check(got2 == want, format_args!("stat {} is {:?}, and the model has {:?}", show(p), got2, want));
    }

    /// Every entry of the directory, as `read_dir` gives them.
    pub fn list(&mut self, p: &[u8]) -> Vec<(Vec<u8>, bool, u64)> {
        let mut got = Vec::new();
        let mut index = 0;
        while let Some(e) = self.vfs.read_dir(p, index) {
            got.push((e.name().to_vec(), e.kind == Kind::Dir, e.size as u64));
            index += 1;
            if index > 100_000 {
                panic!("invariant: read_dir of {} goes on past {} entries", show(p), index);
            }
        }
        got.sort();
        if self.strict {
            let want: Vec<(Vec<u8>, bool, u64)> = match self.model.resolve(p) {
                Res::Node(n) => match self.model.node(&n) {
                    Some(Node::Dir(c)) => c.iter().map(|(k, v)| {
                        (k.clone(), v.is_dir(), match v { Node::File(d) => d.len(), _ => 0 })
                    }).collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            self.check(got == want, format_args!("the directory {} lists {:?}, and the model has {:?}", show(p),
                                                 got.iter().map(|e| show(&e.0)).collect::<Vec<_>>(),
                                                 want.iter().map(|e| show(&e.0)).collect::<Vec<_>>()));
        }
        got
    }

    pub fn open(&mut self, p: &[u8], flags: usize) {
        say(format_args!("open {} {:#x}", show(p), flags));
        let (mark, saved) = (crate::machine::trace_mark(), self.model.tree.clone());
        let got = self.vfs.open(p, flags);
        let want = self.model.open(p, flags);
        if self.refused_for_room(got.is_some(), want.is_some(), mark, &saved) {
            return;
        }
        self.check(got.is_some() == want.is_some(), format_args!("open {} with flags {:#x} {}, where the model {}", show(p),
                   flags, if got.is_some() { "succeeded" } else { "failed" }, if want.is_some() { "opens it" } else { "does not" }));
        say(format_args!("  -> {:#x}", Handle::into_raw(got)));
        match (got, want) {
            (Some(h), Some(o)) => {
                reached("a file opened");
                self.model.open.push((h, o));
            }
            (Some(h), None) => self.vfs.close(h),
            _ => {}
        }
    }

    pub fn close(&mut self, i: usize) {
        if i < self.model.open.len() {
            let (h, _) = self.model.open.remove(i);
            say(format_args!("close {:#x}", Handle::into_raw(Some(h))));
            self.vfs.close(h);
        }
    }

    pub fn close_all(&mut self) {
        while !self.model.open.is_empty() {
            self.close(0);
        }
    }

    pub fn read(&mut self, i: usize, n: usize) {
        let Some((h, o)) = self.model.open.get(i).cloned() else { return };
        let mut buf = vec![0u8; n];
        let got = self.vfs.read(h, &mut buf);
        let want = if o.flags & OPEN_READ == 0 {
            None
        } else {
            let d = match self.model.node(&o.path) {
                Some(Node::File(d)) => d.clone(),
                _ => Data::new(),
            };
            Some(d.read(o.pos, n as u64))
        };
        self.check(got == want.as_ref().map(Vec::len), format_args!("a read of {} bytes of {} at {} gave {:?}, and \
                   the model {:?}", n, show(&join(&o.path)), o.pos, got, want.as_ref().map(Vec::len)));
        if let (Some(k), Some(w)) = (got, want) {
            self.check(buf[..k] == w[..], format_args!("a read of {} at {} gave other bytes than were written",
                                                       show(&join(&o.path)), o.pos));
            self.model.open[i].1.pos += k as u64;
            if k != 0 {
                reached("a file read back");
            }
        }
    }

    pub fn write(&mut self, i: usize, data: &[u8]) {
        let Some((h, o)) = self.model.open.get(i).cloned() else { return };
        say(format_args!("write {} bytes to {} at {}", data.len(), show(&join(&o.path)), o.pos));
        let (mark, saved) = (crate::machine::trace_mark(), self.model.tree.clone());
        let got = self.vfs.write(h, data);
        let want = self.model_write(i, data);
        if self.refused_for_room(got, want, mark, &saved) {
            self.model.open[i].1.pos = o.pos;
            let at = if o.flags & OPEN_APPEND != 0 { self.model.size(&o.path) } else { o.pos };
            let old = match self.model.node(&o.path) {
                Some(Node::File(d)) => d.clone(),
                _ => Data::new(),
            };
            self.resync_partial(&o.path, &old, at, data);
            return;
        }
        self.check(got == want, format_args!("a write of {} bytes to {} at {} {}, where the model {}", data.len(),
                   show(&join(&o.path)), o.pos, if got { "succeeded" } else { "failed" }, if want { "writes it" } else { "does not" }));
    }

    pub fn model_write(&mut self, i: usize, data: &[u8]) -> bool {
        let limit = self.model.limits.max_file;
        let o = self.model.open[i].1.clone();
        if o.flags & (OPEN_WRITE | OPEN_APPEND) == 0 {
            return false;
        }
        if data.is_empty() {
            return true;
        }
        let at = if o.flags & OPEN_APPEND != 0 { self.model.size(&o.path) } else { o.pos };
        let Some(end) = at.checked_add(data.len() as u64) else { return false };
        if end > limit {
            return false;
        }
        match self.model.file_mut(&o.path) {
            Some(d) => d.write(at, data),
            None => return false,
        }
        self.model.open[i].1.pos = end;
        if at > 12 * 4096 {
            reached("a write past the direct blocks");
        }
        true
    }

    pub fn seek(&mut self, i: usize, pos: u64) {
        let Some((h, _)) = self.model.open.get(i).cloned() else { return };
        let Ok(p) = usize::try_from(pos) else { return };
        let ok = self.vfs.seek(h, p);
        self.check(ok, format_args!("a seek of an open file failed"));
        self.model.open[i].1.pos = pos;
        let size = self.vfs.size(h) as u64;
        let want = self.model.size(&self.model.open[i].1.path);
        self.check(size == want, format_args!("an open file's size is {}, and the model's {}", size, want));
    }

    pub fn read_at(&mut self, i: usize, at: u64, n: usize) {
        let Some((h, o)) = self.model.open.get(i).cloned() else { return };
        let Ok(off) = usize::try_from(at) else { return };
        let mut buf = vec![0u8; n];
        let got = self.vfs.read_at(h, off, &mut buf);
        let want = if o.flags & OPEN_READ == 0 {
            None
        } else {
            match self.model.node(&o.path) {
                Some(Node::File(d)) => Some(d.read(at, n as u64)),
                _ => Some(Vec::new()),
            }
        };
        self.check(got == want.as_ref().map(Vec::len), format_args!("read_at {} of {} bytes of {} gave {:?}, and the \
                   model {:?}", at, n, show(&join(&o.path)), got, want.as_ref().map(Vec::len)));
        if let (Some(k), Some(w)) = (got, want) {
            self.check(buf[..k] == w[..], format_args!("read_at {} of {} gave other bytes than were written", at,
                                                       show(&join(&o.path))));
        }
    }

    pub fn write_within(&mut self, i: usize, at: u64, data: &[u8]) {
        let Some((h, o)) = self.model.open.get(i).cloned() else { return };
        let Ok(off) = usize::try_from(at) else { return };
        say(format_args!("write_within {} bytes to {} at {}", data.len(), show(&join(&o.path)), at));
        let (mark, saved) = (crate::machine::trace_mark(), self.model.tree.clone());
        let got = self.vfs.write_within(h, off, data);
        let size = self.model.size(&o.path);
        let want = o.flags & OPEN_WRITE != 0
            && at.checked_add(data.len() as u64).is_some_and(|end| end <= size);
        if self.refused_for_room(got, want, mark, &saved) {
            let old = match self.model.node(&o.path) {
                Some(Node::File(d)) => d.clone(),
                _ => Data::new(),
            };
            self.resync_partial(&o.path, &old, at, data);
            return;
        }
        self.check(got == want, format_args!("write_within {} of {} bytes to {} ({} bytes) {}", at, data.len(),
                   show(&join(&o.path)), size, if got { "succeeded" } else { "failed" }));
        if got && want && !data.is_empty() {
            if let Some(d) = self.model.file_mut(&o.path) {
                d.write(at, data);
            }
        }
    }

    pub fn create(&mut self, p: &[u8], dir: bool) {
        say(format_args!("create {} dir {}", show(p), dir));
        let (mark, saved) = (crate::machine::trace_mark(), self.model.tree.clone());
        let got = self.vfs.create(p, dir);
        let want = self.model.create(p, dir);
        if self.refused_for_room(got, want, mark, &saved) {
            return;
        }
        self.check(got == want, format_args!("create {} ({}) {}, where the model {}", show(p),
                   if dir { "a directory" } else { "a file" }, if got { "succeeded" } else { "failed" },
                   if want { "makes it" } else { "does not" }));
        if got && dir {
            reached("a directory made");
        }
    }

    pub fn remove(&mut self, p: &[u8]) {
        say(format_args!("remove {}", show(p)));
        let got = self.vfs.remove(p);
        let want = self.model.remove(p);
        self.check(got == want, format_args!("remove {} {}, where the model {}", show(p),
                   if got { "succeeded" } else { "failed" }, if want { "removes it" } else { "does not" }));
        if got {
            reached("something removed");
        }
    }

    pub fn truncate(&mut self, p: &[u8], size: u64) {
        let Ok(s) = usize::try_from(size) else { return };
        say(format_args!("truncate {} to {}", show(p), size));
        let got = self.vfs.truncate(p, s);
        let limit = self.model.limits.max_file;
        let want = match self.model.resolve(p) {
            Res::Node(n) if !self.model.read_only && size <= limit => match self.model.file_mut(&n) {
                Some(d) => {
                    d.truncate(size);
                    true
                }
                None => false,
            },
            _ => false,
        };
        self.check(got == want, format_args!("truncate {} to {} {}, where the model {}", show(p), size,
                   if got { "succeeded" } else { "failed" }, if want { "does it" } else { "does not" }));
    }

    pub fn rename(&mut self, from: &[u8], to: &[u8]) {
        say(format_args!("rename {} to {}", show(from), show(to)));
        let (mark, saved) = (crate::machine::trace_mark(), self.model.tree.clone());
        let got = self.vfs.rename(from, to);
        let want = self.model.rename(from, to);
        if self.refused_for_room(got, want, mark, &saved) {
            return;
        }
        self.check(got == want, format_args!("rename {} to {} {}, where the model {}", show(from), show(to),
                   if got { "succeeded" } else { "failed" }, if want { "does it" } else { "does not" }));
        if got {
            reached("something renamed");
        }
    }

    pub fn write_file(&mut self, p: &[u8], data: &[u8]) {
        say(format_args!("write_file {} of {} bytes", show(p), data.len()));
        let (mark, saved) = (crate::machine::trace_mark(), self.model.tree.clone());
        let got = self.vfs.write_file(p, data);
        let want = self.model.write_file(p, data);
        if self.refused_for_room(got, want, mark, &saved) {
            /* Made, or not for want of an inode; emptied, and then the
             * write of the new content stopped part way. */
            if let (Res::Node(n), Some(st)) = (self.model.resolve(p), self.vfs.stat(p)) {
                let _ = st;
                self.model.file_mut(&n).map(|d| d.truncate(0));
                self.resync_partial(&n, &Data::new(), 0, data);
            } else if let (Res::Absent(dir, name), Some(_)) = (self.model.resolve(p), self.vfs.stat(p)) {
                self.model.make(&dir, &name, Node::File(Data::new()));
                let mut n = dir;
                n.push(name);
                self.resync_partial(&n, &Data::new(), 0, data);
            }
            return;
        }
        self.check(got == want, format_args!("write_file {} of {} bytes {}, where the model {}", show(p), data.len(),
                   if got { "succeeded" } else { "failed" }, if want { "writes it" } else { "does not" }));
    }

    pub fn sync(&mut self) {
        let ok = self.vfs.sync();
        self.check(ok, format_args!("sync failed"));
    }

    /// The whole tree, read back through the VFS, held to the model's: every
    /// directory's entries, every file's size, and its content -- all of
    /// it for a file of a megabyte or less; for a bigger one, every piece
    /// the model has written in it, its end, and holes here and there.
    pub fn compare(&mut self) {
        if !self.strict {
            self.read_tree(b"/", 0);
            return;
        }
        let model = self.model.tree.clone();
        if let Some(d) = self.compare_at(b"/", &model, 0) {
            panic!("invariant: the tree read back is not the model's: {}", d);
        }
    }

    fn compare_at(&mut self, p: &[u8], model: &Node, depth: usize) -> Option<String> {
        let at = |name: &[u8]| {
            let mut c = p.to_vec();
            if !c.ends_with(b"/") {
                c.push(b'/');
            }
            c.extend_from_slice(name);
            c
        };
        match model {
            Node::Dir(children) => {
                let listed = self.list_raw(p);
                let want: Vec<(Vec<u8>, bool)> = children.iter().map(|(k, v)| (k.clone(), v.is_dir())).collect();
                let got: Vec<(Vec<u8>, bool)> = listed.iter().map(|e| (e.0.clone(), e.1)).collect();
                if got != want {
                    return Some(format!("{} lists {:?}, and the model {:?}", show(p),
                                        got.iter().map(|e| show(&e.0)).collect::<Vec<_>>(),
                                        want.iter().map(|e| show(&e.0)).collect::<Vec<_>>()));
                }
                if depth > 64 {
                    return None;
                }
                for (name, child) in children {
                    if let Some(d) = self.compare_at(&at(name), child, depth + 1) {
                        return Some(d);
                    }
                }
                None
            }
            Node::File(data) => {
                let file = match fs::vfs::Open::new(self.vfs, p, OPEN_READ) {
                    Some(file) => file,
                    None => return Some(format!("{} does not open", show(p))),
                };
                if file.size() as u64 != data.len() {
                    return Some(format!("{} is {} bytes, and the model's {}", show(p), file.size(), data.len()));
                }
                let mut ranges: Vec<(u64, u64)> = Vec::new();
                if data.len() <= 1 << 20 {
                    ranges.push((0, data.len()));
                } else {
                    for b in data.nonzero_blocks(4096) {
                        ranges.push((b * 4096, 4096));
                    }
                    ranges.push((data.len().saturating_sub(4096), 4096));
                    for k in 1..8u64 {
                        ranges.push((data.len() / 8 * k, 4096));
                    }
                }
                for (off, n) in ranges {
                    let n = n.min(data.len().saturating_sub(off));
                    let mut buf = vec![0u8; n as usize];
                    let mut got = 0usize;
                    if !file.seek(off as usize) {
                        return Some(format!("{} does not seek to {}", show(p), off));
                    }
                    while got < buf.len() {
                        match file.read(&mut buf[got..]) {
                            Some(0) | None => break,
                            Some(k) => got += k,
                        }
                    }
                    if got != buf.len() || buf != data.read(off, n) {
                        return Some(format!("{}: {} bytes at {} read back as {} other bytes", show(p), n, off, got));
                    }
                }
                None
            }
        }
    }

    /// The directory's entries, sorted, with nothing held to the model.
    fn list_raw(&mut self, p: &[u8]) -> Vec<(Vec<u8>, bool, u64)> {
        let mut got = Vec::new();
        let mut index = 0;
        while let Some(e) = self.vfs.read_dir(p, index) {
            got.push((e.name().to_vec(), e.kind == Kind::Dir, e.size as u64));
            index += 1;
            if index > 100_000 {
                panic!("invariant: read_dir of {} goes on past {} entries", show(p), index);
            }
        }
        got.sort();
        got
    }

    /// The tree under `p`, as the VFS shows it.
    pub fn read_tree(&mut self, p: &[u8], depth: usize) -> Node {
        let mut children = std::collections::BTreeMap::new();
        if depth > 64 {
            return Node::Dir(children);
        }
        let mut index = 0;
        while let Some(e) = self.vfs.read_dir(p, index) {
            index += 1;
            if index > 100_000 {
                break;
            }
            let name = e.name().to_vec();
            if name.contains(&b'/') || name == b"." || name == b".." || name.is_empty() {
                continue;
            }
            let mut child = p.to_vec();
            if !child.ends_with(b"/") {
                child.push(b'/');
            }
            child.extend_from_slice(&name);
            let node = if e.kind == Kind::Dir {
                self.read_tree(&child, depth + 1)
            } else {
                Node::File(self.read_file_upto(&child, 1 << 20).unwrap_or_default())
            };
            children.insert(name, node);
        }
        Node::Dir(children)
    }

    /// The first `limit` bytes of a file, at most, through the VFS.
    pub fn read_file_upto(&mut self, p: &[u8], limit: u64) -> Option<Data> {
        let file = fs::vfs::Open::new(self.vfs, p, OPEN_READ)?;
        let size = (file.size() as u64).min(limit);
        let mut d = Data::new();
        let mut buf = vec![0u8; 65536];
        let mut at = 0u64;
        while at < size {
            let got = match file.read(&mut buf) {
                Some(0) | None => break,
                Some(k) => k,
            };
            d.write(at, &buf[..got]);
            at += got as u64;
        }
        d.truncate(at);
        Some(d)
    }
}

/// Says what the target does, in a replay's `--trace`.
pub fn say(what: std::fmt::Arguments) {
    if crate::machine::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[{:>12}] -- {}", crate::machine::sched::now() / 1000, what);
    }
}

/// One operation of the input's on the filesystem.
pub fn step(f: &mut Fs, r: &mut Input, block: u64) {
    let open = f.model.open.len() as u64;
    let pick_open = |r: &mut Input| r.below(open.max(1)) as usize;
    match r.u8() % 20 {
        0 => {
            let p = path(r, &f.model);
            f.stat(&p);
        }
        1 => {
            let p = path(r, &f.model);
            f.list(&p);
        }
        2 | 3 => {
            let p = path(r, &f.model);
            let flags = match r.u8() % 6 {
                0 => OPEN_READ,
                1 => OPEN_WRITE | OPEN_CREATE,
                2 => OPEN_READ | OPEN_WRITE,
                3 => OPEN_APPEND | OPEN_CREATE,
                4 => OPEN_WRITE | OPEN_TRUNCATE,
                _ => r.u8() as usize & 31,
            };
            if f.model.open.len() < 16 {
                f.open(&p, flags);
            }
        }
        4 => {
            let i = pick_open(r);
            f.close(i);
        }
        5 | 6 => {
            let i = pick_open(r);
            let n = length(r);
            f.read(i, n);
        }
        7 | 8 => {
            let i = pick_open(r);
            let n = length(r);
            let data = crate::input::noise(r.u32(), n);
            f.write(i, &data);
        }
        9 => {
            let i = pick_open(r);
            let size = f.model.open.get(i).map_or(0, |(_, o)| f.model.size(&o.path));
            let at = offset(r, size, block, f.model.limits.span);
            f.seek(i, at);
        }
        10 => {
            let i = pick_open(r);
            let size = f.model.open.get(i).map_or(0, |(_, o)| f.model.size(&o.path));
            let at = offset(r, size, block, f.model.limits.span);
            let n = length(r);
            if r.bool() {
                f.read_at(i, at, n);
            } else {
                let data = crate::input::noise(r.u32(), n);
                f.write_within(i, at, &data);
            }
        }
        11 | 12 => {
            let p = path(r, &f.model);
            let dir = r.bool();
            f.create(&p, dir);
        }
        13 => {
            let p = path(r, &f.model);
            f.remove(&p);
        }
        14 => {
            let p = path(r, &f.model);
            let size = match f.model.resolve(&p) {
                Res::Node(n) => f.model.size(&n),
                _ => 0,
            };
            let to = offset(r, size, block, f.model.limits.span);
            f.truncate(&p, to);
        }
        15 => {
            let a = path(r, &f.model);
            let b = path(r, &f.model);
            f.rename(&a, &b);
        }
        16 => {
            let p = path(r, &f.model);
            let n = length(r);
            let data = crate::input::noise(r.u32(), n);
            f.write_file(&p, &data);
        }
        17 => f.sync(),
        18 => {
            /* A chain of directories, deep: what a tree a remount must read
             * back looks like at its deepest. */
            let mut p = b"/".to_vec();
            for k in 0..r.range(1, 40) {
                if k > 0 {
                    p.push(b'/');
                }
                p.push(b'd');
                f.create(&p, true);
            }
        }
        _ => f.compare(),
    }
}
