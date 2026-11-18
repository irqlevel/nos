//! `sftp`: the SFTP server (src/crates/common/net/sftp) on its own, fed
//! what a client sends -- requests one after another, cut into pieces
//! wherever the input says, a piece holding half a request or several --
//! over a model of the files the kernel gives it (`Model`: kcore::fs as
//! sshd's `KernelFiles` reaches it, which fails a call now and then, as a
//! disk does).
//!
//! The requests are every one SFTP version 3 has and the four extensions
//! the server says it has, at paths made of a few names -- `.`, `..`, `~`,
//! `a.new` beside `a`, a name as long as the files keep and one longer, a
//! path past the longest, one not UTF-8, one with a NUL -- with handles
//! open, closed, of the other kind and made up; now and then one cut
//! short, one with bytes after its end, one of a kind there is none of;
//! and, rarely, a stream there is no reading on from: an empty message,
//! one longer than the server takes, a second INIT.
//!
//! Every answer is held to what docs/sshd.md and the crate's documentation
//! say, request by request, worked out here from the files as they were
//! before it -- this judge's own reading, sharing nothing with the server:
//! one answer to each request, of its id and of a kind the request may
//! have, no longer than a message may be; a STATUS's code the one its case
//! has; a READ the bytes there, an ATTRS the size and the kind; a READDIR
//! every entry of a directory nobody changed once, and then its end; the
//! files changed by each request exactly as it says and by nothing else --
//! unchanged by a request refused, cut short, or failed by the model; at
//! most 32 handles, a closed one dead and never given out again; nothing
//! but well-formed paths handed to the files; every file the server opened
//! closed by its close or its drop; and its counters what was done.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use sftp::{Entry, Error, Files, Kind, Open, Server, Space, Stat};

use crate::common::runner::reached;
use crate::input::noise;
use crate::Input;

/* SFTP's numbers (draft-ietf-secsh-filexfer-02), the judge's own copy */
const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const LSTAT: u8 = 7;
const FSTAT: u8 = 8;
const SETSTAT: u8 = 9;
const FSETSTAT: u8 = 10;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const REALPATH: u8 = 16;
const STAT: u8 = 17;
const RENAME: u8 = 18;
const READLINK: u8 = 19;
const SYMLINK: u8 = 20;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;
const EXTENDED: u8 = 200;
const EXTENDED_REPLY: u8 = 201;

const F_READ: u32 = 0x01;
const F_WRITE: u32 = 0x02;
const F_APPEND: u32 = 0x04;
const F_CREAT: u32 = 0x08;
const F_TRUNC: u32 = 0x10;
const F_EXCL: u32 = 0x20;

const A_SIZE: u32 = 0x01;
const A_UIDGID: u32 = 0x02;
const A_PERM: u32 = 0x04;
const A_TIME: u32 = 0x08;
const A_EXT: u32 = 0x8000_0000;

const OK: u32 = 0;
const EOF: u32 = 1;
const NO_SUCH_FILE: u32 = 2;
const FAILURE: u32 = 4;
const BAD_MESSAGE: u32 = 5;
const UNSUPPORTED: u32 = 8;

const DIR_MODE: u32 = 0o040755;
const FILE_MODE: u32 = 0o100644;

/* What the server takes and gives, as docs/sshd.md has it */
const NAME_MAX: usize = 63;
const PATH_MAX: usize = 255;
const MAX_PACKET: usize = 64 * 1024;
const MAX_DATA: usize = MAX_PACKET - 1024;
const MAX_HANDLES: usize = 32;
const DIR_BATCH: usize = 100;
const EXTENSIONS: [(&[u8], &[u8]); 4] = [(b"limits@openssh.com", b"1"), (b"statvfs@openssh.com", b"2"),
                                          (b"fsync@openssh.com", b"1"), (b"expand-path@openssh.com", b"1")];

/* The model's disk: a file past this fails to grow, as a full disk does */
const FILE_MAX: u64 = 200 * 1024;
const TOTAL: u64 = 1 << 30;
/// Requests an input makes at most.
const REQUESTS: usize = 300;

/* ---- the files: a model of what kcore::fs gives sshd ---- */

#[derive(Clone, PartialEq, Debug)]
enum Node {
    Dir,
    File(Rc<Vec<u8>>),
}

/// Every path there is, absolute, `/` among them, to what is there.
type Tree = BTreeMap<String, Node>;

struct Disk {
    tree: Tree,
    /// The files open, by how many opens of each.
    open: BTreeMap<String, usize>,
    /// The call of the files that fails next, counted down, and how.
    fault: Option<(u64, Error)>,
    /// A call failed since this was last cleared.
    faulted: bool,
}

#[derive(Clone)]
struct Model(Rc<RefCell<Disk>>);

/// A file the server holds open.
struct MFile {
    disk: Rc<RefCell<Disk>>,
    path: String,
    write: bool,
}

impl Drop for MFile {
    fn drop(&mut self) {
        let mut d = self.disk.borrow_mut();
        let n = d.open.get_mut(&self.path).expect("an open file is counted");
        *n -= 1;
        if *n == 0 {
            d.open.remove(&self.path);
        }
    }
}

fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(cut) => &path[..cut],
    }
}

/// What is under `dir`, directly, in the tree's order.
fn children<'t>(tree: &'t Tree, dir: &str) -> impl Iterator<Item = (&'t String, &'t Node)> + 't {
    let dir = dir.to_string();
    tree.iter().filter(move |(p, _)| p.as_str() != "/" && parent(p) == dir)
}

fn under(path: &str, top: &str) -> bool {
    path == top || (top == "/" && path.starts_with('/')) || path.starts_with(&format!("{}/", top))
}

fn used(tree: &Tree) -> u64 {
    tree.values().map(|n| if let Node::File(c) = n { c.len() as u64 } else { 0 }).sum()
}

/// The server's promise about what it hands the files: a path absolute,
/// its `.` and `..` gone, no name empty, no NUL, none longer than the files
/// keep, nor the whole longer than they take.
fn promised(path: &str) {
    invariant!(path.starts_with('/'), "the server handed the files {:?}, a path not absolute", path);
    invariant!(path.len() <= PATH_MAX, "the server handed the files a path of {} bytes", path.len());
    invariant!(!path.contains('\0'), "the server handed the files {:?}, a NUL in it", path);
    if path != "/" {
        for name in path[1..].split('/') {
            invariant!(!name.is_empty() && name != "." && name != "..", "the server handed the files {:?}", path);
            invariant!(name.len() <= NAME_MAX, "the server handed the files a name of {} bytes", name.len());
        }
    }
}

impl Model {
    /// A call of the files: failed if it is the one the input says.
    fn call(&self) -> Result<std::cell::RefMut<'_, Disk>, Error> {
        let mut d = self.0.borrow_mut();
        if let Some((n, e)) = d.fault {
            if n == 0 {
                d.fault = None;
                d.faulted = true;
                return Err(e);
            }
            d.fault = Some((n - 1, e));
        }
        Ok(d)
    }
}

fn content<'t>(tree: &'t Tree, path: &str) -> &'t Rc<Vec<u8>> {
    match tree.get(path) {
        Some(Node::File(c)) => c,
        _ => panic!("invariant: the path {:?} of an open file holds no file", path),
    }
}

impl Files for Model {
    type File = MFile;
    const NAME_MAX: usize = NAME_MAX;
    const PATH_MAX: usize = PATH_MAX;

    fn stat(&mut self, path: &str) -> Result<Stat, Error> {
        promised(path);
        let d = self.call()?;
        match d.tree.get(path) {
            Some(Node::Dir) => Ok(Stat { kind: Kind::Dir, size: 0 }),
            Some(Node::File(c)) => Ok(Stat { kind: Kind::File, size: c.len() as u64 }),
            None => Err(Error::NotFound),
        }
    }

    /* As kcore's: every refusal of an open is NotFound */
    fn open(&mut self, path: &str, how: Open) -> Result<MFile, Error> {
        promised(path);
        let mut d = self.call()?;
        match d.tree.get(path) {
            Some(Node::Dir) => return Err(Error::NotFound),
            Some(Node::File(_)) if how.exclusive => return Err(Error::NotFound),
            Some(Node::File(_)) => {
                if how.truncate {
                    d.tree.insert(path.to_string(), Node::File(Rc::new(Vec::new())));
                }
            }
            None => {
                if !how.create || d.tree.get(parent(path)) != Some(&Node::Dir) {
                    return Err(Error::NotFound);
                }
                d.tree.insert(path.to_string(), Node::File(Rc::new(Vec::new())));
            }
        }
        *d.open.entry(path.to_string()).or_default() += 1;
        Ok(MFile { disk: self.0.clone(), path: path.to_string(), write: how.write })
    }

    fn read(&mut self, f: &MFile, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let d = self.call()?;
        let c = content(&d.tree, &f.path);
        if offset >= c.len() as u64 {
            return Ok(0);
        }
        let at = offset as usize;
        let n = buf.len().min(c.len() - at);
        buf[..n].copy_from_slice(&c[at..at + n]);
        Ok(n)
    }

    fn write(&mut self, f: &MFile, offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut d = self.call()?;
        if !f.write {
            return Err(Error::Failed);
        }
        if data.is_empty() {
            return Ok(());
        }
        let Some(end) = offset.checked_add(data.len() as u64).filter(|&e| e <= FILE_MAX) else {
            return Err(Error::Failed);
        };
        let Some(Node::File(c)) = d.tree.get_mut(&f.path) else { panic!("invariant: an open file is gone") };
        let c = Rc::make_mut(c);
        if (c.len() as u64) < end {
            c.resize(end as usize, 0);
        }
        c[offset as usize..end as usize].copy_from_slice(data);
        Ok(())
    }

    fn size(&mut self, f: &MFile) -> Result<u64, Error> {
        let d = self.call()?;
        Ok(content(&d.tree, &f.path).len() as u64)
    }

    fn set_len(&mut self, f: &MFile, size: u64) -> Result<(), Error> {
        let mut d = self.call()?;
        if !f.write || size > FILE_MAX {
            return Err(Error::Failed);
        }
        let Some(Node::File(c)) = d.tree.get_mut(&f.path) else { panic!("invariant: an open file is gone") };
        Rc::make_mut(c).resize(size as usize, 0);
        Ok(())
    }

    fn sync(&mut self, _f: &MFile) -> Result<(), Error> {
        self.call()?;
        Ok(())
    }

    fn entry(&mut self, dir: &str, index: usize) -> Result<Option<Entry>, Error> {
        promised(dir);
        let d = self.call()?;
        if d.tree.get(dir) != Some(&Node::Dir) {
            return Err(Error::NotFound);
        }
        let entry = children(&d.tree, dir).nth(index).map(|(path, node)| Entry {
            name: path[parent(path).len() + usize::from(parent(path) != "/")..].as_bytes().to_vec(),
            kind: if *node == Node::Dir { Kind::Dir } else { Kind::File },
            size: if let Node::File(c) = node { c.len() as u64 } else { 0 },
        });
        Ok(entry)
    }

    fn make_dir(&mut self, path: &str) -> Result<(), Error> {
        promised(path);
        let mut d = self.call()?;
        if d.tree.contains_key(path) || d.tree.get(parent(path)) != Some(&Node::Dir) {
            return Err(Error::Failed);
        }
        d.tree.insert(path.to_string(), Node::Dir);
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), Error> {
        promised(path);
        let mut d = self.call()?;
        match d.tree.get(path) {
            Some(Node::File(_)) if !d.open.contains_key(path) => {
                d.tree.remove(path);
                Ok(())
            }
            _ => Err(Error::Failed),
        }
    }

    fn remove_dir(&mut self, path: &str) -> Result<(), Error> {
        promised(path);
        let mut d = self.call()?;
        if path == "/" || d.tree.get(path) != Some(&Node::Dir) || children(&d.tree, path).next().is_some() {
            return Err(Error::Failed);
        }
        d.tree.remove(path);
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        promised(from);
        promised(to);
        let mut d = self.call()?;
        if !may_rename(&d.tree, &d.open.keys().cloned().collect::<Vec<_>>(), from, to) {
            return Err(Error::Failed);
        }
        d.tree = renamed(&d.tree, from, to);
        Ok(())
    }

    fn space(&mut self, path: &str) -> Result<Space, Error> {
        promised(path);
        let d = self.call()?;
        Ok(Space { total: TOTAL, free: TOTAL - used(&d.tree) })
    }
}

/// Whether `from` can be renamed `to`: there, not the root, nothing at
/// `to`, a directory to put it in, not into itself, nothing under it open.
fn may_rename(tree: &Tree, open: &[String], from: &str, to: &str) -> bool {
    from != "/" && tree.contains_key(from) && !tree.contains_key(to) && tree.get(parent(to)) == Some(&Node::Dir)
        && !under(to, from) && !open.iter().any(|p| under(p, from))
}

/// The tree with everything at and under `from` moved to `to`.
fn renamed(tree: &Tree, from: &str, to: &str) -> Tree {
    tree.iter()
        .map(|(p, n)| if under(p, from) { (format!("{}{}", to, &p[from.len()..]), n.clone()) } else { (p.clone(), n.clone()) })
        .collect()
}

/* ---- the judge's reading of a client's path ---- */

/// The path the server makes of `raw` -- absolute against `/`, `.` and
/// empty names out, `..` taking the name before it -- with nothing checked
/// but that it is UTF-8 (REALPATH's).
fn normal(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut names: Vec<&str> = Vec::new();
    for name in text.split('/') {
        match name {
            "" | "." => {}
            ".." => {
                names.pop();
            }
            name => names.push(name),
        }
    }
    Some(format!("/{}", names.join("/")))
}

/// The path the files are asked about, or None for one they cannot take:
/// not UTF-8, a NUL anywhere in it, a name or the whole too long.
fn valid(raw: &[u8]) -> Option<String> {
    if raw.contains(&0) {
        return None;
    }
    let p = normal(raw)?;
    (p.len() <= PATH_MAX && p.split('/').all(|n| n.len() <= NAME_MAX)).then_some(p)
}

/* ---- the requests ---- */

#[derive(Clone, Debug)]
enum Req {
    Open { path: Vec<u8>, pflags: u32 },
    Close(Vec<u8>),
    Read { h: Vec<u8>, off: u64, len: u32 },
    Write { h: Vec<u8>, off: u64, data: Vec<u8> },
    Stat(u8, Vec<u8>),
    Fstat(Vec<u8>),
    Setstat { path: Vec<u8>, flags: u32, size: u64 },
    Fsetstat { h: Vec<u8>, flags: u32, size: u64 },
    Opendir(Vec<u8>),
    Readdir(Vec<u8>),
    Remove(Vec<u8>),
    Mkdir(Vec<u8>),
    Rmdir(Vec<u8>),
    Realpath(Vec<u8>),
    Rename(Vec<u8>, Vec<u8>),
    Readlink(Vec<u8>),
    Symlink(Vec<u8>, Vec<u8>),
    Limits,
    Statvfs(Vec<u8>),
    Fsync(Vec<u8>),
    Expand(Vec<u8>),
    OtherExt(Vec<u8>),
    Unknown(u8),
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

/// ATTRS with `flags`, every field it says there: a size, the rest anything.
fn put_attrs(out: &mut Vec<u8>, flags: u32, size: u64, r: &mut Input) {
    put_u32(out, flags);
    if flags & A_SIZE != 0 {
        put_u64(out, size);
    }
    if flags & A_UIDGID != 0 {
        put_u32(out, r.u32());
        put_u32(out, r.u32());
    }
    if flags & A_PERM != 0 {
        put_u32(out, r.u32());
    }
    if flags & A_TIME != 0 {
        put_u32(out, r.u32());
        put_u32(out, r.u32());
    }
    if flags & A_EXT != 0 {
        let n = r.below(3) as u32;
        put_u32(out, n);
        for _ in 0..n {
            put_str(out, b"x@nos");
            put_str(out, b"y");
        }
    }
}

/// The request's message: its kind, its id, its fields.
fn encode(id: u32, req: &Req, r: &mut Input) -> Vec<u8> {
    let mut m = Vec::new();
    let kind = match req {
        Req::Open { .. } => OPEN,
        Req::Close(_) => CLOSE,
        Req::Read { .. } => READ,
        Req::Write { .. } => WRITE,
        Req::Stat(k, _) => *k,
        Req::Fstat(_) => FSTAT,
        Req::Setstat { .. } => SETSTAT,
        Req::Fsetstat { .. } => FSETSTAT,
        Req::Opendir(_) => OPENDIR,
        Req::Readdir(_) => READDIR,
        Req::Remove(_) => REMOVE,
        Req::Mkdir(_) => MKDIR,
        Req::Rmdir(_) => RMDIR,
        Req::Realpath(_) => REALPATH,
        Req::Rename(..) => RENAME,
        Req::Readlink(_) => READLINK,
        Req::Symlink(..) => SYMLINK,
        Req::Limits | Req::Statvfs(_) | Req::Fsync(_) | Req::Expand(_) | Req::OtherExt(_) => EXTENDED,
        Req::Unknown(k) => *k,
    };
    m.push(kind);
    put_u32(&mut m, id);
    match req {
        Req::Open { path, pflags } => {
            put_str(&mut m, path);
            put_u32(&mut m, *pflags);
            let flags = r.pick(&[0, A_PERM, A_PERM | A_TIME, A_SIZE | A_UIDGID | A_PERM | A_TIME | A_EXT]);
            put_attrs(&mut m, flags, r.u64(), r);
        }
        Req::Close(h) | Req::Fstat(h) | Req::Readdir(h) => put_str(&mut m, h),
        Req::Read { h, off, len } => {
            put_str(&mut m, h);
            put_u64(&mut m, *off);
            put_u32(&mut m, *len);
        }
        Req::Write { h, off, data } => {
            put_str(&mut m, h);
            put_u64(&mut m, *off);
            put_str(&mut m, data);
        }
        Req::Stat(_, p) | Req::Opendir(p) | Req::Remove(p) | Req::Rmdir(p) | Req::Realpath(p) | Req::Readlink(p) => {
            put_str(&mut m, p)
        }
        Req::Setstat { path, flags, size } => {
            put_str(&mut m, path);
            put_attrs(&mut m, *flags, *size, r);
        }
        Req::Fsetstat { h, flags, size } => {
            put_str(&mut m, h);
            put_attrs(&mut m, *flags, *size, r);
        }
        Req::Mkdir(p) => {
            put_str(&mut m, p);
            let flags = r.pick(&[0, A_PERM]);
            put_attrs(&mut m, flags, 0, r);
        }
        Req::Rename(a, b) | Req::Symlink(a, b) => {
            put_str(&mut m, a);
            put_str(&mut m, b);
        }
        Req::Limits => put_str(&mut m, EXTENSIONS[0].0),
        Req::Statvfs(p) => {
            put_str(&mut m, EXTENSIONS[1].0);
            put_str(&mut m, p);
        }
        Req::Fsync(h) => {
            put_str(&mut m, EXTENSIONS[2].0);
            put_str(&mut m, h);
        }
        Req::Expand(p) => {
            put_str(&mut m, EXTENSIONS[3].0);
            put_str(&mut m, p);
        }
        Req::OtherExt(name) => put_str(&mut m, name),
        Req::Unknown(_) => {}
    }
    m
}

/* ---- the client the judge is ---- */

#[derive(Clone, Debug)]
enum What {
    File { path: String, write: bool, append: bool },
    Dir { path: String },
}

struct Handle {
    raw: Vec<u8>,
    what: What,
    open: bool,
    /// A directory's: the names its READDIRs gave, the end reached, and
    /// whether the directory stayed as it was since it was opened -- else
    /// what came is not held to it.
    seen: BTreeSet<Vec<u8>>,
    done: bool,
    reliable: bool,
}

/// What a request is to be answered with.
#[derive(Debug)]
enum Want {
    Status(u32),
    /// Some STATUS, but OK: a request cut short, or one the files failed.
    AnyError,
    Handle(What),
    Data(Vec<u8>),
    Attrs(Kind, u64),
    Name(String),
    Listing(usize),
    Limits,
    Statvfs(u64, u64),
}

struct Sent {
    id: u32,
    req: Req,
    /// Cut short of its fields: any refusal will do, and nothing changes.
    cut: bool,
    /// Cut short of its id, too: answered as id 0.
    no_id: bool,
}

struct Client {
    handles: Vec<Handle>,
    /// Every handle ever given out: none is given out twice.
    given: BTreeSet<Vec<u8>>,
    next_id: u32,
    /// What the server has been sent and not yet taken, then sent but not
    /// yet answered.
    stream: Vec<u8>,
    sent: VecDeque<Sent>,
    /// What the client counts of what the server did.
    answered: u64,
    refused: u64,
    opened: u64,
    opened_to_write: u64,
    read: u64,
    written: u64,
}

impl Client {
    fn live(&self, raw: &[u8]) -> Option<usize> {
        self.handles.iter().position(|h| h.open && h.raw == raw)
    }

    fn open_count(&self) -> usize {
        self.handles.iter().filter(|h| h.open).count()
    }

    /// The files the server should have open: the client's file handles.
    fn open_files(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for h in self.handles.iter().filter(|h| h.open) {
            if let What::File { path, .. } = &h.what {
                *m.entry(path.clone()).or_default() += 1;
            }
        }
        m
    }

    /// A handle for a request: one open, one closed, or made up.
    fn handle(&self, r: &mut Input, file: bool) -> Vec<u8> {
        let open: Vec<&Handle> = self.handles.iter()
            .filter(|h| h.open && matches!(h.what, What::File { .. }) == file).collect();
        match r.u8() % 8 {
            0..=4 if !open.is_empty() => open[r.below(open.len() as u64) as usize].raw.clone(),
            5 if !self.handles.is_empty() => self.handles[r.below(self.handles.len() as u64) as usize].raw.clone(),
            6 => {
                let n = r.pick(&[0usize, 1, 4, 8, 8, 9, 300]);
                r.bytes(n)
            }
            _ => match self.handles.first() {
                Some(h) => h.raw.clone(),
                None => vec![0; 8],
            },
        }
    }
}

/// A path for a request: a few names, among them the awkward ones.
fn path(r: &mut Input) -> Vec<u8> {
    const NAMES: [&[u8]; 12] = [b"a", b"b", b"d", b"e", b"a.new", b"f.txt", b".", b"..", b"~", b"", b"d", b"a"];
    let mut p = Vec::new();
    match r.u8() % 16 {
        0 => return Vec::new(),
        1 => return b"/".to_vec(),
        2 => {
            /* A name as long as the files keep, or one longer */
            let n = r.pick(&[NAME_MAX, NAME_MAX + 1]);
            p.extend_from_slice(b"/d/");
            p.extend(std::iter::repeat_n(b'n', n));
            return p;
        }
        3 => {
            /* A path past the longest, of names each short enough */
            for _ in 0..5 {
                p.push(b'/');
                p.extend(std::iter::repeat_n(b'p', NAME_MAX));
            }
            return p;
        }
        4 if r.chance(64) => return vec![b'/', 0xFF, 0xFE],
        5 if r.chance(64) => return b"/a\0b".to_vec(),
        _ => {}
    }
    if r.u8() % 4 != 0 {
        p.push(b'/');
    }
    for i in 0..r.range(1, 3) {
        if i > 0 {
            p.push(b'/');
        }
        p.extend_from_slice(r.pick(&NAMES));
    }
    if r.chance(16) {
        p.push(b'/');
    }
    p
}

/// What `req`, sent at `pre`, is to be answered with, and the files after
/// it -- the judge's own reading of the protocol and of docs/sshd.md.
fn expect(c: &Client, pre: &Tree, req: &Req) -> (Want, Tree) {
    let mut post = pre.clone();
    let st = |p: &str| pre.get(p).cloned();
    let want = match req {
        Req::Open { path, pflags } => {
            let create = pflags & F_CREAT != 0;
            let excl = create && pflags & F_EXCL != 0;
            let write = pflags & (F_WRITE | F_APPEND) != 0;
            let what = |p: &String| What::File { path: p.clone(), write, append: pflags & F_APPEND != 0 };
            match valid(path) {
                None => Want::Status(if create { FAILURE } else { NO_SUCH_FILE }),
                Some(_) if c.open_count() >= MAX_HANDLES => Want::Status(FAILURE),
                Some(p) => match st(&p) {
                    Some(Node::Dir) => Want::Status(FAILURE),
                    Some(Node::File(_)) if excl => Want::Status(FAILURE),
                    Some(Node::File(_)) => {
                        if pflags & F_TRUNC != 0 {
                            post.insert(p.clone(), Node::File(Rc::new(Vec::new())));
                        }
                        Want::Handle(what(&p))
                    }
                    None if !create => Want::Status(NO_SUCH_FILE),
                    None if pre.get(parent(&p)) == Some(&Node::Dir) => {
                        post.insert(p.clone(), Node::File(Rc::new(Vec::new())));
                        Want::Handle(what(&p))
                    }
                    None => Want::Status(NO_SUCH_FILE),
                },
            }
        }
        Req::Close(h) => Want::Status(if c.live(h).is_some() { OK } else { FAILURE }),
        Req::Read { h, off, len } => match c.live(h).map(|i| &c.handles[i].what) {
            Some(What::File { path, .. }) => {
                let data = content(pre, path);
                let n = (*len as usize).min(MAX_DATA);
                if n == 0 {
                    Want::Data(Vec::new())
                } else if *off >= data.len() as u64 {
                    Want::Status(EOF)
                } else {
                    let at = *off as usize;
                    Want::Data(data[at..data.len().min(at + n)].to_vec())
                }
            }
            _ => Want::Status(FAILURE),
        },
        Req::Write { h, off, data } => match c.live(h).map(|i| &c.handles[i].what) {
            Some(What::File { path, write: true, append }) => {
                let old = content(pre, path);
                let at = if *append { old.len() as u64 } else { *off };
                match at.checked_add(data.len() as u64) {
                    _ if data.is_empty() => Want::Status(OK),
                    Some(end) if end <= FILE_MAX => {
                        let mut new = (**old).clone();
                        if (new.len() as u64) < end {
                            new.resize(end as usize, 0);
                        }
                        new[at as usize..end as usize].copy_from_slice(data);
                        post.insert(path.clone(), Node::File(Rc::new(new)));
                        Want::Status(OK)
                    }
                    _ => Want::Status(FAILURE),
                }
            }
            _ => Want::Status(FAILURE),
        },
        Req::Stat(_, p) => match valid(p).and_then(|p| st(&p)) {
            Some(Node::Dir) => Want::Attrs(Kind::Dir, 0),
            Some(Node::File(d)) => Want::Attrs(Kind::File, d.len() as u64),
            None => Want::Status(NO_SUCH_FILE),
        },
        Req::Fstat(h) => match c.live(h).map(|i| &c.handles[i].what) {
            Some(What::File { path, .. }) => Want::Attrs(Kind::File, content(pre, path).len() as u64),
            Some(What::Dir { path }) => match st(path) {
                Some(Node::Dir) => Want::Attrs(Kind::Dir, 0),
                Some(Node::File(d)) => Want::Attrs(Kind::File, d.len() as u64),
                None => Want::Status(NO_SUCH_FILE),
            },
            None => Want::Status(FAILURE),
        },
        Req::Setstat { path, flags, size } => {
            if flags & !A_SIZE != 0 {
                Want::Status(UNSUPPORTED)
            } else {
                match valid(path).and_then(|p| st(&p).map(|n| (p, n))) {
                    None => Want::Status(NO_SUCH_FILE),
                    Some(_) if flags & A_SIZE == 0 => Want::Status(OK),
                    Some((_, Node::Dir)) => Want::Status(FAILURE),
                    Some(_) if *size > FILE_MAX => Want::Status(FAILURE),
                    Some((p, Node::File(d))) => {
                        let mut new = (*d).clone();
                        new.resize(*size as usize, 0);
                        post.insert(p, Node::File(Rc::new(new)));
                        Want::Status(OK)
                    }
                }
            }
        }
        Req::Fsetstat { h, flags, size } => match c.live(h).map(|i| &c.handles[i].what) {
            None => Want::Status(FAILURE),
            Some(_) if flags & !A_SIZE != 0 => Want::Status(UNSUPPORTED),
            Some(_) if flags & A_SIZE == 0 => Want::Status(OK),
            Some(What::File { path, write: true, .. }) if *size <= FILE_MAX => {
                let mut new = (**content(pre, path)).clone();
                new.resize(*size as usize, 0);
                post.insert(path.clone(), Node::File(Rc::new(new)));
                Want::Status(OK)
            }
            Some(_) => Want::Status(FAILURE),
        },
        Req::Opendir(p) => match valid(p).and_then(|p| st(&p).map(|n| (p, n))) {
            None => Want::Status(NO_SUCH_FILE),
            Some((_, Node::File(_))) => Want::Status(FAILURE),
            Some(_) if c.open_count() >= MAX_HANDLES => Want::Status(FAILURE),
            Some((p, Node::Dir)) => Want::Handle(What::Dir { path: p }),
        },
        Req::Readdir(h) => match c.live(h) {
            Some(i) => match &c.handles[i].what {
                What::Dir { path } if st(path) == Some(Node::Dir) => Want::Listing(i),
                What::Dir { .. } => Want::Status(NO_SUCH_FILE),
                What::File { .. } => Want::Status(FAILURE),
            },
            None => Want::Status(FAILURE),
        },
        Req::Remove(p) => match valid(p).and_then(|p| st(&p).map(|n| (p, n))) {
            None => Want::Status(NO_SUCH_FILE),
            Some((_, Node::Dir)) => Want::Status(FAILURE),
            Some((p, _)) if c.open_files().contains_key(&p) => Want::Status(FAILURE),
            Some((p, _)) => {
                post.remove(&p);
                Want::Status(OK)
            }
        },
        Req::Mkdir(p) => match valid(p) {
            None => Want::Status(FAILURE),
            Some(p) if pre.contains_key(&p) => Want::Status(FAILURE),
            Some(p) if pre.get(parent(&p)) != Some(&Node::Dir) => Want::Status(NO_SUCH_FILE),
            Some(p) => {
                post.insert(p, Node::Dir);
                Want::Status(OK)
            }
        },
        Req::Rmdir(p) => match valid(p).and_then(|p| st(&p).map(|n| (p, n))) {
            None => Want::Status(NO_SUCH_FILE),
            Some((_, Node::File(_))) => Want::Status(FAILURE),
            Some((p, _)) if p == "/" || children(pre, &p).next().is_some() => Want::Status(FAILURE),
            Some((p, _)) => {
                post.remove(&p);
                Want::Status(OK)
            }
        },
        Req::Realpath(p) => match normal(p) {
            Some(p) => Want::Name(p),
            None => Want::Status(NO_SUCH_FILE),
        },
        Req::Rename(from, to) => match (valid(from), valid(to)) {
            (None, _) => Want::Status(NO_SUCH_FILE),
            (_, None) => Want::Status(FAILURE),
            (Some(from), Some(to)) => {
                let open: Vec<String> = c.open_files().into_keys().collect();
                if may_rename(pre, &open, &from, &to) {
                    post = renamed(pre, &from, &to);
                    Want::Status(OK)
                } else if !pre.contains_key(&from) {
                    Want::Status(NO_SUCH_FILE)
                } else {
                    Want::Status(FAILURE)
                }
            }
        },
        Req::Readlink(p) => match valid(p).and_then(|p| st(&p)) {
            None => Want::Status(NO_SUCH_FILE),
            Some(_) => Want::Status(FAILURE),
        },
        Req::Symlink(..) => Want::Status(UNSUPPORTED),
        Req::Limits => Want::Limits,
        Req::Statvfs(p) => match valid(p) {
            Some(_) => Want::Statvfs(TOTAL, TOTAL - used(pre)),
            None => Want::Status(NO_SUCH_FILE),
        },
        Req::Fsync(h) => match c.live(h).map(|i| &c.handles[i].what) {
            Some(What::File { .. }) => Want::Status(OK),
            _ => Want::Status(FAILURE),
        },
        Req::Expand(p) => {
            let rest: &[u8] = if p == b"~" { b"" } else { p.strip_prefix(b"~/").unwrap_or(p) };
            match normal(rest) {
                Some(p) => Want::Name(p),
                None => Want::Status(NO_SUCH_FILE),
            }
        }
        Req::OtherExt(_) | Req::Unknown(_) => Want::Status(UNSUPPORTED),
    };
    (want, post)
}

/// The next request: of every kind, at paths and handles the client has.
fn request(c: &Client, r: &mut Input, pre: &Tree) -> Req {
    let size_of = |h: &[u8]| match c.live(h).map(|i| &c.handles[i].what) {
        Some(What::File { path, .. }) => match pre.get(path) {
            Some(Node::File(d)) => d.len() as u64,
            _ => 0,
        },
        _ => 0,
    };
    match r.u8() % 24 {
        0..=2 => {
            let pflags = match r.u8() % 6 {
                0 => F_READ,
                1 => F_WRITE | F_CREAT | F_TRUNC,
                2 => F_READ | F_WRITE,
                3 => F_WRITE | F_APPEND | F_CREAT,
                4 => F_WRITE | F_CREAT | F_EXCL,
                _ => r.u32() & 0x7F,
            };
            Req::Open { path: path(r), pflags }
        }
        3 => {
            let file = r.bool();
            Req::Close(c.handle(r, file))
        }
        4 | 5 => {
            let h = c.handle(r, true);
            let size = size_of(&h);
            let any = r.below(FILE_MAX);
            let off = r.pick(&[0, 1, size.saturating_sub(1), size, size + 1, size + 4096, any, u64::MAX]);
            let len = r.pick(&[0, 1, 1000, 4096, MAX_DATA as u32, MAX_DATA as u32 + 1, u32::MAX]);
            Req::Read { h, off, len }
        }
        6..=8 => {
            let h = c.handle(r, true);
            let size = size_of(&h);
            let any = r.below(FILE_MAX);
            let off = r.pick(&[0, size, size + 1, size + 5000, any, FILE_MAX - 10, 1 << 40, u64::MAX - 3]);
            let n = r.pick(&[0usize, 1, 17, 1000, 4096, 32768, MAX_DATA, MAX_DATA + 1]);
            Req::Write { h, off, data: noise(r.u32(), n) }
        }
        9 => Req::Stat(r.pick(&[STAT, LSTAT]), path(r)),
        10 => {
            let file = r.bool();
            Req::Fstat(c.handle(r, file))
        }
        11 => {
            let flags = r.pick(&[A_SIZE, A_SIZE, 0, A_PERM, A_TIME, A_UIDGID, A_SIZE | A_PERM, A_EXT]);
            let size = r.pick(&[0, 1, 5000, 70000, FILE_MAX + 1]);
            if r.bool() {
                Req::Setstat { path: path(r), flags, size }
            } else {
                Req::Fsetstat { h: c.handle(r, true), flags, size }
            }
        }
        12 => Req::Opendir(path(r)),
        13 | 14 => Req::Readdir(c.handle(r, false)),
        15 => Req::Remove(path(r)),
        16 => Req::Mkdir(path(r)),
        17 => Req::Rmdir(path(r)),
        18 => Req::Realpath(path(r)),
        19 => Req::Rename(path(r), path(r)),
        20 => match r.u8() % 3 {
            0 => Req::Readlink(path(r)),
            1 => Req::Symlink(path(r), path(r)),
            _ => Req::Unknown(r.pick(&[0, VERSION, 21, 99, STATUS, HANDLE, ATTRS, EXTENDED_REPLY, 255])),
        },
        21 => match r.u8() % 6 {
            0 => Req::Limits,
            1 | 2 => Req::Statvfs(path(r)),
            3 => Req::Expand(r.pick::<&[u8]>(&[b"~", b"~/a", b"~/../d", b"a/~"]).to_vec()),
            4 => Req::OtherExt(r.pick::<&[u8]>(&[b"posix-rename@openssh.com", b"copy-data", b""]).to_vec()),
            _ => {
                let file = r.bool();
                Req::Fsync(c.handle(r, file))
            }
        },
        _ => Req::Open { path: path(r), pflags: F_WRITE | F_CREAT | F_TRUNC },
    }
}

/* ---- the answers ---- */

struct Rd<'a> {
    b: &'a [u8],
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        invariant!(self.b.len() >= n, "an answer cut short: {} bytes wanted, {} there", n, self.b.len());
        let (a, rest) = self.b.split_at(n);
        self.b = rest;
        a
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().unwrap())
    }
    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().unwrap())
    }
    fn str(&mut self) -> &'a [u8] {
        let n = self.u32() as usize;
        self.take(n)
    }
    fn end(&self, what: &str) {
        invariant!(self.b.is_empty(), "{} bytes after the end of {}", self.b.len(), what);
    }
}

/// An ATTRS as the server says it of what the files keep: a size and a mode.
fn attrs_of(a: &mut Rd) -> (Kind, u64) {
    let flags = a.u32();
    invariant!(flags == A_SIZE | A_PERM, "an ATTRS with flags {:#x}", flags);
    let size = a.u64();
    let mode = a.u32();
    invariant!(mode == DIR_MODE || mode == FILE_MODE, "an ATTRS with mode {:o}", mode);
    (if mode == DIR_MODE { Kind::Dir } else { Kind::File }, size)
}

/// The one answer the server made, held to what was wanted of it.
fn judge(c: &mut Client, sent: &Sent, want: Want, faulted: bool, answer: &[u8], pre: &Tree) {
    let mut a = Rd { b: answer };
    let len = a.u32() as usize;
    invariant!(len == answer.len() - 4, "an answer of {} bytes says it has {}", answer.len() - 4, len);
    invariant!(len <= MAX_PACKET + 1024, "an answer of {} bytes", len);
    let kind = a.u8();
    let id = a.u32();
    let want_id = if sent.no_id { 0 } else { sent.id };
    invariant!(id == want_id, "{:?} (id {}) answered as id {}", sent.req, want_id, id);
    c.answered += 1;

    let want = if sent.cut || faulted { Want::AnyError } else { want };
    if faulted {
        if let Req::Readdir(h) = &sent.req {
            if let Some(i) = c.live(h) {
                c.handles[i].reliable = false;
            }
        }
    }

    if kind == STATUS {
        let code = a.u32();
        let why = a.str();
        a.str();
        a.end("a STATUS");
        invariant!(std::str::from_utf8(why).is_ok_and(|w| !w.is_empty()), "a STATUS whose message is {:?}", why);
        if code != OK && code != EOF {
            c.refused += 1;
        }
        let fits = match &want {
            Want::Status(w) => code == *w,
            Want::AnyError => code != OK,
            Want::Listing(_) => code == EOF,
            _ => false,
        };
        invariant!(fits, "{:?}, wanted {:?}, was answered with STATUS {} {:?}", sent.req, want, code,
                   String::from_utf8_lossy(why));
        let used = match &sent.req {
            Req::Close(h) | Req::Fstat(h) | Req::Readdir(h) | Req::Fsync(h) => Some(h),
            Req::Read { h, .. } | Req::Write { h, .. } | Req::Fsetstat { h, .. } => Some(h),
            _ => None,
        };
        if used.is_some_and(|h| c.live(h).is_none() && c.handles.iter().any(|k| &k.raw == h)) {
            reached("sftp: a closed handle refused");
        }
        if let (Want::Listing(i), EOF) = (&want, code) {
            let h = &mut c.handles[*i];
            if h.reliable {
                let What::Dir { path } = &h.what else { unreachable!() };
                let all: BTreeSet<Vec<u8>> = children(pre, path)
                    .map(|(p, _)| p[parent(p).len() + usize::from(parent(p) != "/")..].as_bytes().to_vec())
                    .collect();
                invariant!(h.seen == all, "the listing of {} ended having given {:?}, the directory holding {:?}",
                           path, h.seen.iter().map(|n| String::from_utf8_lossy(n)).collect::<Vec<_>>(),
                           all.iter().map(|n| String::from_utf8_lossy(n)).collect::<Vec<_>>());
                if !all.is_empty() {
                    reached("sftp: a directory listed whole");
                }
            }
            h.done = true;
        }
        if code == OK {
            match &sent.req {
                Req::Close(raw) => {
                    let i = c.live(raw).expect("a close answered OK closes an open handle");
                    c.handles[i].open = false;
                }
                Req::Write { data, .. } => c.written += data.len() as u64,
                _ => {}
            }
        }
        return;
    }

    match (kind, &want) {
        (HANDLE, Want::Handle(what)) => {
            let raw = a.str().to_vec();
            a.end("a HANDLE");
            invariant!(!raw.is_empty() && raw.len() <= 256, "a handle of {} bytes", raw.len());
            invariant!(c.given.insert(raw.clone()), "a handle given out twice: {:?}", raw);
            if let What::File { write, .. } = what {
                c.opened += 1;
                if *write {
                    c.opened_to_write += 1;
                }
            }
            c.handles.push(Handle { raw, what: what.clone(), open: true, seen: BTreeSet::new(), done: false,
                                    reliable: true });
        }
        (DATA, Want::Data(bytes)) => {
            let got = a.str();
            a.end("a DATA");
            invariant!(got == bytes.as_slice(), "{:?} gave {} bytes, the file has {} there", sent.req, got.len(),
                       bytes.len());
            c.read += got.len() as u64;
            if !got.is_empty() {
                reached("sftp: a file read");
            }
        }
        (ATTRS, Want::Attrs(k, size)) => {
            let (got_k, got_size) = attrs_of(&mut a);
            a.end("an ATTRS");
            invariant!(got_k == *k && got_size == *size, "{:?} said {:?} of {} bytes, the files {:?} of {}",
                       sent.req, got_k, got_size, k, size);
        }
        (NAME, Want::Name(p)) => {
            invariant!(a.u32() == 1, "a REALPATH answered with a NAME of more than one");
            let name = a.str();
            let long = a.str();
            invariant!(a.u32() == 0, "a REALPATH's name with attributes");
            a.end("a REALPATH's NAME");
            invariant!(name == p.as_bytes() && long == p.as_bytes(), "{:?} made {:?}, the judge {:?}", sent.req,
                       String::from_utf8_lossy(name), p);
        }
        (NAME, Want::Listing(i)) => {
            let count = a.u32() as usize;
            invariant!((1..=DIR_BATCH).contains(&count), "a READDIR of {} entries", count);
            let h = &mut c.handles[*i];
            let What::Dir { path } = &h.what else { unreachable!() };
            for _ in 0..count {
                let name = a.str().to_vec();
                let long = a.str();
                let (k, size) = attrs_of(&mut a);
                let mode: &[u8] = if k == Kind::Dir { b"drwxr-xr-x " } else { b"-rw-r--r-- " };
                invariant!(long.starts_with(mode) && long.ends_with(&name) && long.len() > mode.len() + name.len(),
                           "the long name {:?} of {:?}", String::from_utf8_lossy(long), String::from_utf8_lossy(&name));
                invariant!(!h.done || !h.reliable, "an entry {:?} after the listing's end",
                           String::from_utf8_lossy(&name));
                if h.reliable {
                    let full = if path == "/" {
                        format!("/{}", String::from_utf8_lossy(&name))
                    } else {
                        format!("{}/{}", path, String::from_utf8_lossy(&name))
                    };
                    let (want_k, want_size) = match pre.get(&full) {
                        Some(Node::Dir) => (Kind::Dir, 0),
                        Some(Node::File(d)) => (Kind::File, d.len() as u64),
                        None => panic!("invariant: the listing of {} gave {}, which is not there", path, full),
                    };
                    invariant!(k == want_k && size == want_size, "the listing gave {} as {:?} of {}, the files have {:?} of {}",
                               full, k, size, want_k, want_size);
                    invariant!(h.seen.insert(name.clone()), "the listing of {} gave {} twice", path, full);
                }
            }
            a.end("a READDIR's NAME");
        }
        (EXTENDED_REPLY, Want::Limits) => {
            let got = [a.u64(), a.u64(), a.u64(), a.u64()];
            a.end("limits@openssh.com's answer");
            invariant!(got == [MAX_PACKET as u64, MAX_DATA as u64, MAX_DATA as u64, MAX_HANDLES as u64],
                       "limits@openssh.com says {:?}", got);
        }
        (EXTENDED_REPLY, Want::Statvfs(total, free)) => {
            let got: Vec<u64> = (0..11).map(|_| a.u64()).collect();
            a.end("statvfs@openssh.com's answer");
            invariant!(got[0] == 4096 && got[1] == 4096 && got[2] == total / 4096 && got[3] == free / 4096
                       && got[4] == free / 4096 && got[10] == NAME_MAX as u64, "statvfs@openssh.com says {:?}", got);
        }
        _ => panic!("invariant: {:?}, wanted {:?}, was answered with a message of kind {}", sent.req, want, kind),
    }
}

/* ---- the run ---- */

/// Takes what the server has been sent, a message at a time, holding each
/// answer to its request: Err once the server says the stream cannot be read
/// on from -- which the caller says whether it should have.
fn pump(server: &mut Server<Model>, model: &Model, c: &mut Client, buf: &mut Vec<u8>, r: &mut Input)
    -> Result<(), &'static str> {
    loop {
        if r.chance(8) {
            let e = r.pick(&[Error::Failed, Error::NoMemory, Error::NotFound]);
            model.0.borrow_mut().fault = Some((r.below(3), e));
        }
        model.0.borrow_mut().faulted = false;
        let pre = model.0.borrow().tree.clone();
        let mut out = Vec::new();
        let taken = server.input(buf, &mut out)?;
        if taken == 0 {
            invariant!(out.is_empty(), "{} bytes answered to no whole message", out.len());
            return Ok(());
        }
        invariant!(taken <= buf.len(), "the server took {} bytes of {}", taken, buf.len());
        buf.drain(..taken);
        let sent = c.sent.pop_front().expect("a message the client sent");
        let faulted = std::mem::take(&mut model.0.borrow_mut().faulted);
        if faulted {
            reached("sftp: a request the files failed");
        }
        /* The files after it: as the request says, or untouched where it
           was refused */
        let (want, after) = expect(c, &pre, &sent.req);
        let ok_status = out.len() >= 13 && out[4] == STATUS && out[9..13] == [0, 0, 0, 0];
        let success = out.len() > 4 && (out[4] != STATUS || ok_status);
        let post = model.0.borrow().tree.clone();
        /* Directories whose listing was going on and changed */
        for h in c.handles.iter_mut().filter(|h| h.open) {
            if let What::Dir { path } = &h.what {
                let before: Vec<_> = children(&pre, path).collect();
                let now: Vec<_> = children(&post, path).collect();
                if before != now || pre.get(path) != post.get(path) {
                    h.reliable = false;
                }
            }
        }
        judge(c, &sent, want, faulted, &out, &pre);
        if success && !sent.cut && !faulted {
            invariant!(post == after, "{:?} left the files {:?}, the judge {:?}", sent.req, post.keys().collect::<Vec<_>>(),
                       after.keys().collect::<Vec<_>>());
            if matches!(sent.req, Req::Write { ref data, .. } if !data.is_empty()) {
                reached("sftp: a file written");
            }
        } else {
            invariant!(post == pre, "{:?}, refused, changed the files", sent.req);
        }
        let open = model.0.borrow().open.clone();
        invariant!(open == c.open_files(), "the server holds open {:?}, the client {:?}", open, c.open_files());
    }
}

pub fn sftp(r: &mut Input) {
    let mut tree = Tree::new();
    tree.insert("/".into(), Node::Dir);
    tree.insert("/d".into(), Node::Dir);
    tree.insert("/a".into(), Node::File(Rc::new(noise(r.u32(), r.below(80_000) as usize))));
    tree.insert("/d/b".into(), Node::File(Rc::new(noise(r.u32(), 10))));
    let model = Model(Rc::new(RefCell::new(Disk { tree, open: BTreeMap::new(), fault: None, faulted: false })));
    let mut server = Server::new(model.clone());
    let mut c = Client { handles: Vec::new(), given: BTreeSet::new(), next_id: 1, stream: Vec::new(),
                         sent: VecDeque::new(), answered: 0, refused: 0, opened: 0, opened_to_write: 0, read: 0,
                         written: 0 };
    let mut buf: Vec<u8> = Vec::new();

    /* The INIT, and the VERSION it is answered with -- or, rarely, a
       request before it, which ends the stream */
    let mut init = vec![INIT];
    init.extend_from_slice(&r.pick(&[3u32, 3, 6, 0]).to_be_bytes());
    if r.chance(32) {
        put_str(&mut init, b"posix-rename@openssh.com");
        put_str(&mut init, b"1");
    }
    if r.rare() {
        init[0] = OPEN;
        let mut m = (init.len() as u32).to_be_bytes().to_vec();
        m.extend_from_slice(&init);
        let mut out = Vec::new();
        invariant!(server.input(&m, &mut out).is_err(), "a request before the INIT was taken");
        reached("sftp: a request before the INIT refused");
        return;
    }
    let mut m = (init.len() as u32).to_be_bytes().to_vec();
    m.extend_from_slice(&init);
    let mut out = Vec::new();
    let taken = server.input(&m, &mut out).expect("the INIT");
    invariant!(taken == m.len(), "the INIT taken as {} bytes of {}", taken, m.len());
    {
        let mut a = Rd { b: &out };
        invariant!(a.u32() as usize == out.len() - 4, "a VERSION of the wrong length");
        invariant!(a.u8() == VERSION && a.u32() == 3, "the INIT answered with something but VERSION 3");
        let mut exts = BTreeSet::new();
        while !a.b.is_empty() {
            exts.insert((a.str().to_vec(), a.str().to_vec()));
        }
        let want: BTreeSet<(Vec<u8>, Vec<u8>)> = EXTENSIONS.iter().map(|(n, d)| (n.to_vec(), d.to_vec())).collect();
        invariant!(exts == want, "the VERSION names the extensions {:?}", exts);
    }

    let mut requests = 0;
    let mut burst = 0;
    while r.more() && requests < REQUESTS {
        requests += 1;
        let pre = model.0.borrow().tree.clone();
        /* Now and then opens of a file that is there, one after another --
           as many as the handles a server has, and more */
        if burst == 0 && r.chance(6) {
            burst = r.range(8, MAX_HANDLES as u64 + 8);
        }
        let req = if burst > 0 {
            burst -= 1;
            Req::Open { path: b"/a".to_vec(), pflags: F_READ }
        } else {
            request(&c, r, &pre)
        };
        let id = c.next_id;
        c.next_id = c.next_id.wrapping_add(if r.chance(8) { r.u32() } else { 1 });
        let mut msg = encode(id, &req, r);
        let mut sent = Sent { id, req, cut: false, no_id: false };
        if r.chance(6) {
            /* Cut short of its fields -- or of its id */
            let keep = r.range(1, (msg.len() - 1) as u64) as usize;
            msg.truncate(keep);
            sent.cut = true;
            sent.no_id = keep < 5;
            reached("sftp: a request cut short");
        } else if r.chance(6) {
            /* Bytes after its end, which the server reads past */
            let n = r.range(1, 9) as usize;
            msg.extend_from_slice(&r.bytes(n));
        }
        let stream_error = if r.rare() {
            /* A stream there is no reading on from: an empty message, one
               past the longest, a second INIT */
            match r.u8() % 3 {
                0 => msg.clear(),
                1 => msg.resize(MAX_PACKET + 1 + r.below(100) as usize, 0),
                _ => msg = vec![INIT, 0, 0, 0, 3],
            }
            true
        } else {
            false
        };
        c.stream.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        c.stream.extend_from_slice(&msg);
        c.sent.push_back(sent);

        /* Sent on now, a piece at a time or all of it -- and the server
           takes what it can */
        let deliver = if stream_error || !r.more() || r.chance(96) {
            c.stream.len()
        } else {
            r.below(c.stream.len() as u64 + 1) as usize
        };
        let piece: Vec<u8> = c.stream.drain(..deliver).collect();
        buf.extend_from_slice(&piece);
        match pump(&mut server, &model, &mut c, &mut buf, r) {
            Ok(()) => invariant!(!stream_error, "a stream that cannot be read on from was taken"),
            Err(why) => {
                invariant!(stream_error, "the server gave up on the stream: {}", why);
                reached("sftp: a stream the server could not read on from");
                drop(server);
                invariant!(model.0.borrow().open.is_empty(), "files still open after the server was dropped");
                return;
            }
        }
        if c.open_count() >= MAX_HANDLES {
            reached("sftp: every handle open");
        }
        if c.handles.len() > MAX_HANDLES && c.open_count() < MAX_HANDLES / 2 {
            reached("sftp: handles opened and closed past the most at once");
        }
    }
    /* What is left, sent whole */
    let rest: Vec<u8> = std::mem::take(&mut c.stream);
    buf.extend_from_slice(&rest);
    if let Err(why) = pump(&mut server, &model, &mut c, &mut buf, r) {
        panic!("invariant: the server gave up on the stream: {}", why);
    }
    invariant!(c.sent.is_empty() && buf.is_empty(), "{} requests sent and never answered", c.sent.len());

    let s = server.stats();
    let want = (c.answered, c.refused, c.opened, c.opened_to_write, c.read, c.written);
    let got = (s.requests, s.refused, s.opened, s.opened_to_write, s.read, s.written);
    invariant!(got == want, "the server counts (requests, refused, opened, to write, read, written) {:?}, the client {:?}",
               got, want);
    drop(server);
    invariant!(model.0.borrow().open.is_empty(), "files still open after the server was dropped");
}
