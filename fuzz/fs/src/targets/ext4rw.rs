//! ext4 mounted for writing on images the fuzzer's own mkfs made -- a
//! journal of transactions nobody replayed among them now and then, which
//! the mount replays -- and worked through the VFS as the kernel's callers
//! work it: names and data, files open, disks' images held. Every answer is
//! the model's, and the image a clean unmount leaves clean to the judge and
//! the model's tree.
//!
//! The power is cut at chosen requests -- now and then before every
//! request of one call -- and each picture judged as soon as its call is
//! done: replayed by the judge's own reader and clean to its check, its
//! tree what the call in flight may leave -- the tree before it or after
//! it; for a remove, some of what was under the path gone, deepest first;
//! for a write, every sector of the file what it held or what it was given,
//! nothing of any other file's -- and then mounted by the kernel on a disk
//! of its own beside the one under test, read-only and for writing, both
//! showing the judge's tree, the image that unmount leaves clean. Requests
//! fail now and then: then nothing may succeed the model does not have,
//! nothing be written in a mount after its first write failed, and the
//! image, replayed, must be what the call that met the failure may leave.

use std::collections::{BTreeMap, BTreeSet};

use crate::image::ext4 as img;
use crate::machine::disk;
use crate::model::{join, show, Data, Node, CHUNK};
use crate::targets::ext4::{agree, dump, geometry, one_in, style, too_big, tree, write_sparse, MKFS_TIME};
use crate::targets::fsops::{self, Fs, Limits, Model, Res};
use crate::{reached, Input};

/// Where the kernel mounts a picture: a path no name of the model's has.
const PICTURE: &str = "/\u{1}picture";

/// The requests a call is cut before, every one: more than a commit and a
/// checkpoint of the target's journals take.
const CUT_EVERY: u64 = 64;

type Path = Vec<Vec<u8>>;

/// What a call may leave of the tree if the power goes during it.
#[derive(Clone, Debug)]
enum Leave {
    /// The tree before it, or after.
    Either,
    /// Before, after, or before with some of what is at and under the path
    /// gone -- a remove goes a node at a time.
    Removing(Path),
    /// The file of these names written: every sector of it what it held or
    /// what it was given, its size between; `emptied`, a write_file's, cut
    /// to nothing first; `zeros`, a truncate's, which writes zeros over the
    /// new last block's tail.
    Data { names: Vec<Path>, emptied: bool, zeros: bool },
}

/// Every path of a tree, a file's content with it -- None for a directory.
fn flat(n: &Node) -> BTreeMap<Path, Option<&Data>> {
    fn walk<'a>(n: &'a Node, at: &mut Path, out: &mut BTreeMap<Path, Option<&'a Data>>) {
        for (name, child) in n.children().expect("a directory") {
            at.push(name.clone());
            match child {
                Node::Dir(_) => {
                    out.insert(at.clone(), None);
                    walk(child, at, out);
                }
                Node::File(d) => {
                    out.insert(at.clone(), Some(d));
                }
            }
            at.pop();
        }
    }
    let mut out = BTreeMap::new();
    walk(n, &mut Vec::new(), &mut out);
    out
}

/// `n` bytes of `d` from `at`, zeros past its end.
fn piece(d: &Data, at: u64, n: u64) -> Vec<u8> {
    let mut v = d.read(at, n);
    v.resize(n as usize, 0);
    v
}

/// Whether `got` is what a write may leave of a file that held `old` and
/// was given `new`: its size between the two, and every sector -- of
/// `sector` bytes, what a device puts down whole -- what was there, within
/// the old size, or what was given; with `zeros` past the new end, zeros.
fn data_may(got: &Data, old: &Data, new: &Data, zeros: bool, sector: u64) -> Result<(), String> {
    let (lo, hi) = (old.len().min(new.len()), old.len().max(new.len()));
    if got.len() < lo || got.len() > hi {
        return Err(format!("{} bytes, where it held {} and was given {}", got.len(), old.len(), new.len()));
    }
    let pages: BTreeSet<u64> = got.pages().chain(old.pages()).chain(new.pages()).collect();
    for page in pages {
        let mut at = page * CHUNK;
        while at < (page + 1) * CHUNK && at < got.len() {
            let n = sector.min(got.len() - at);
            let g = got.read(at, n);
            let ok = g == piece(new, at, n) || (at < old.len() && g == piece(old, at, n))
                || (zeros && at >= new.len() && g.iter().all(|&b| b == 0));
            if !ok {
                let differ = |want: Vec<u8>| {
                    let at: Vec<usize> = (0..g.len()).filter(|&k| g[k] != want[k]).collect();
                    format!("{} bytes from +{}", at.len(), at.first().copied().unwrap_or(0))
                };
                return Err(format!("the sector at {} holds neither what it held nor what it was given: it differs \
                                    from the first by {}, from the second by {}", at, differ(piece(old, at, n)),
                                   differ(piece(new, at, n))));
            }
            at += sector;
        }
    }
    Ok(())
}

/// Whether `got` is what a call may leave, the power going during it: the
/// tree before it or after it, or what `leave` allows between the two.
fn may_leave(leave: &Leave, before: &Node, after: &Node, got: &Node, sector: u64) -> Result<(), String> {
    if got == before || got == after {
        return Ok(());
    }
    let (b, a, g) = (flat(before), flat(after), flat(got));
    match leave {
        Leave::Either => Err(format!("neither the tree before the call nor after it: {}",
                                     before.diff(got).unwrap_or_default())),
        Leave::Removing(p) => {
            for (k, v) in &g {
                if b.get(k) != Some(v) {
                    return Err(format!("{} is not as it was before the remove", show(&join(k))));
                }
            }
            match b.keys().find(|k| !g.contains_key(*k) && !k.starts_with(p)) {
                Some(k) => Err(format!("{} is gone, and the remove was of {}", show(&join(k)), show(&join(p)))),
                None => Ok(()),
            }
        }
        Leave::Data { names, emptied, zeros } => {
            let empty = Data::new();
            let keys: BTreeSet<&Path> = b.keys().chain(a.keys()).chain(g.keys()).collect();
            for k in keys {
                if !names.contains(k) {
                    if g.get(k) != b.get(k) {
                        return Err(format!("{}, which the call does not touch, changed", show(&join(k))));
                    }
                    continue;
                }
                match (g.get(k), b.get(k), a.get(k)) {
                    /* Made by the call, and not yet. */
                    (None, None, _) => {}
                    (Some(Some(got)), old, Some(Some(new))) => {
                        let old = match old {
                            Some(Some(d)) if !emptied => *d,
                            _ => &empty,
                        };
                        data_may(got, old, new, *zeros, sector).map_err(|e| format!("{}: {}", show(&join(k)), e))?;
                    }
                    _ => return Err(format!("{} is not the file it was, or was to be", show(&join(k)))),
                }
            }
            let contents: Vec<&Data> = names.iter().filter_map(|k| g.get(k).copied().flatten()).collect();
            if contents.windows(2).any(|w| w[0] != w[1]) {
                return Err(String::from("two names of one file hold different things"));
            }
            Ok(())
        }
    }
}

/// Every path of `map` at or under `at`, moved to under `to`.
fn move_under<V>(map: &mut BTreeMap<Path, V>, at: &[Vec<u8>], to: &[Vec<u8>]) {
    let moved: Vec<Path> = map.keys().filter(|k| k.starts_with(at)).cloned().collect();
    for k in moved {
        let v = map.remove(&k).expect("listed");
        let mut n = to.to_vec();
        n.extend_from_slice(&k[at.len()..]);
        map.insert(n, v);
    }
}

/// `call` on the model, kept only if `got`: what the call did, the model's
/// answer.
fn model_if(f: &mut Fs, got: bool, call: impl FnOnce(&mut Model) -> bool) -> bool {
    let saved = f.model.tree.clone();
    let links = f.model.links.clone();
    let want = call(&mut f.model);
    if !got {
        f.model.tree = saved;
        f.model.links = links;
    }
    want
}

struct Run {
    f: Fs,
    /// The entries the tree leaves out, by directory.
    hidden: BTreeMap<Path, Vec<Vec<u8>>>,
    strict: bool,
    /// The disk a picture is put on for the kernel to mount, by its handle.
    picture: Option<usize>,
    dev_bytes: u64,
    sector: u64,
    block: u64,
    /// With requests failing: the call that met one, the trees around it,
    /// and what it may leave -- the run ends there.
    uncertain: Option<(Node, Node, Leave)>,
    /// The blocks of each file mkfs allocated unwritten, as long as the
    /// file keeps its name: what says a write went into one.
    unwritten: BTreeMap<Path, BTreeSet<u64>>,
    /// Where in the disk's log each image's sync went: a flush it fails may
    /// be the image's own, off the VFS's lock (`Image::sync`), which the
    /// filesystem's commits -- each forcing its own -- do not count on.
    image_syncs: Vec<(usize, usize)>,
}

impl Run {
    fn blocked(&self, res: &Res) -> bool {
        matches!(res, Res::Absent(d, n) if self.hidden.get(d).is_some_and(|h| h.contains(n)))
    }

    /// Image `i` synced, where its requests went in the log kept.
    fn image_sync(&mut self, i: usize) {
        let start = disk::with(0, |d| d.log.len());
        self.f.image_sync(i);
        self.image_syncs.push((start, disk::with(0, |d| d.log.len())));
    }

    /// One call: done, the model's trees around it kept, and every picture
    /// of a power cut taken during it judged against them.
    fn call(&mut self, r: &mut Input, op: impl FnOnce(&mut Run, &mut Input) -> Leave) {
        let before = self.f.model.tree.clone();
        let start = disk::with(0, |d| d.log.len());
        let leave = op(self, r);
        let after = self.f.model.tree.clone();
        let end = disk::with(0, |d| d.log.len());
        if !self.strict && self.uncertain.is_none() && disk::with(0, |d| d.log[start..end].iter().any(|io| !io.ok)) {
            self.uncertain = Some((before.clone(), after.clone(), leave.clone()));
        }
        let pictures = disk::with(0, |d| std::mem::take(&mut d.crashes));
        for (n, snap) in pictures {
            self.judge(n, snap, &before, &after, &leave);
        }
    }

    /// A power cut's picture judged: the judge's replay of it clean and its
    /// tree what the call may leave; then the kernel's read-only view of it
    /// and its replay both that tree, and what the replay's unmount leaves
    /// clean.
    fn judge(&mut self, n: u64, snap: disk::Media, before: &Node, after: &Node, leave: &Leave) {
        if let Some(dir) = std::env::var_os("FS_FUZZ_DUMP") {
            /* For e2fsck, which replays it with its own replay. */
            let base = format!("{}/ext4rw-crash-{}-{}", dir.to_string_lossy(), std::process::id(),
                               crate::machine::sched::now() + n);
            write_sparse(&format!("{}.img", base), &snap, self.dev_bytes);
        }
        let c = img::check(&snap, self.dev_bytes);
        invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the power going before request {} leaves a \
                   picture the judge's replay finds wrong: {:?} {:?}", n, c.corrupt, c.unclean);
        let Some(t) = c.tree else { panic!("invariant: the judge reads no tree out of a power cut's picture") };
        if let Err(e) = may_leave(leave, before, after, &t, self.sector) {
            panic!("invariant: the power going before request {} leaves a tree the call could not ({:?}): {}", n,
                   leave, e);
        }
        reached("ext4rw: a power cut's picture judged");
        let Some(handle) = self.picture else { return };
        disk::with(1, |d| {
            d.current = snap.clone();
            d.durable = snap;
            d.cache.clear();
            d.volatile = false;
            d.no_writes = Some("its ext4 is mounted read-only".into());
        });
        let vfs = self.f.vfs;
        let ro = fs::ext4::mount_at(PICTURE, handle, true);
        invariant!(ro == 1, "a power cut's picture does not mount read-only: {}", ro);
        let mut view = Fs::new(vfs, Model::new(t.clone(), true, Limits::memory()), true);
        if let Some(d) = view.differs_under(PICTURE.as_bytes()) {
            panic!("invariant: a power cut's picture, read through its journal, is not the judge's replay: {}", d);
        }
        invariant!(vfs.unmount(PICTURE.as_bytes()), "the read-only picture's unmount was refused");
        disk::with(1, |d| d.no_writes = None);
        let rw = fs::ext4::mount_at(PICTURE, handle, false);
        invariant!(rw == 0, "a power cut's picture does not mount for writing: {}", rw);
        if let Some(d) = view.differs_under(PICTURE.as_bytes()) {
            panic!("invariant: a power cut's picture, replayed by the kernel, is not the judge's replay: {}", d);
        }
        invariant!(vfs.unmount(PICTURE.as_bytes()), "the replayed picture's unmount was refused");
        let left = disk::with(1, |d| {
            d.no_writes = Some("its ext4 is unmounted".into());
            d.current.clone()
        });
        let k = img::check(&left, self.dev_bytes);
        invariant!(k.corrupt.is_empty() && k.unclean.is_empty() && !k.mounted && k.replayed == 0, "a power cut's \
                   picture, replayed by the kernel and unmounted, is not clean: {:?} {:?} mounted {} replayed {}",
                   k.corrupt, k.unclean, k.mounted, k.replayed);
        invariant!(k.tree.as_ref() == Some(&t), "the kernel's replay of a picture leaves another tree than the judge's: \
                   {:?}", k.tree.as_ref().and_then(|x| t.diff(x)));
        reached("ext4rw: a power cut's picture mounted by the kernel, read through and replayed");
    }
}

/* ---- the calls ---- */

fn create(run: &mut Run, r: &mut Input) -> Leave {
    let p = match run.hidden.iter().nth(r.below(run.hidden.len().max(1) as u64) as usize) {
        /* Now and then a name only the disk has, in its directory. */
        Some((d, h)) if r.chance(32) && !h.is_empty() => {
            let mut k = d.clone();
            k.push(h[r.below(h.len() as u64) as usize].clone());
            join(&k)
        }
        _ => fsops::path(r, &run.f.model),
    };
    let dir = r.bool();
    make(run, &p, dir);
    Leave::Either
}

/// A file or directory made at `p`, the model held to: refused where the
/// disk has the name and the tree does not show it.
fn make(run: &mut Run, p: &[u8], dir: bool) {
    let res = run.f.model.resolve(p);
    if run.blocked(&res) {
        let got = run.f.vfs.create(p, dir);
        invariant!(!got, "create {} succeeded over a name the disk has", show(p));
        reached("ext4rw: a name the disk has and the tree does not, refused");
        return;
    }
    run.f.create(p, dir);
}

fn remove(run: &mut Run, r: &mut Input) -> Leave {
    let p = fsops::path(r, &run.f.model);
    let res = run.f.model.resolve(&p);
    let leave = match &res {
        Res::Node(n) if !n.is_empty() => Leave::Removing(n.clone()),
        _ => Leave::Either,
    };
    let held = matches!(&res, Res::Node(n) if run.hidden.keys().any(|k| k.starts_with(n)));
    if held {
        /* A directory holding what the tree does not show is refused whole. */
        let got = run.f.vfs.remove(&p);
        invariant!(!got, "remove {} succeeded over entries the tree does not show", show(&p));
        reached("ext4rw: a directory holding what the tree does not show, kept");
        return leave;
    }
    run.f.remove(&p);
    leave
}

fn rename(run: &mut Run, r: &mut Input) -> Leave {
    let a = fsops::path(r, &run.f.model);
    let b = fsops::path(r, &run.f.model);
    let (src, dst) = (run.f.model.resolve(&a), run.f.model.resolve(&b));
    if run.blocked(&dst) {
        let got = run.f.vfs.rename(&a, &b);
        invariant!(!got, "rename onto {} succeeded over a name the disk has", show(&b));
        return Leave::Either;
    }
    let mark = crate::machine::trace_mark();
    let got = run.f.vfs.rename(&a, &b);
    let want = model_if(&mut run.f, got, |m| m.rename(&a, &b));
    if run.strict && !(got != want && !got && fsops::out_of_room(mark)) {
        invariant!(got == want, "rename {} to {} {}, where the model {}", show(&a), show(&b),
                   if got { "succeeded" } else { "failed" }, if want { "does it" } else { "does not" });
    }
    invariant!(!got || want, "rename {} to {} succeeded, and the model does not have it", show(&a), show(&b));
    if got && want {
        if let (Res::Node(s), Res::Absent(d, n)) = (src, dst) {
            let mut to = d;
            to.push(n);
            move_under(&mut run.hidden, &s, &to);
        }
        reached("ext4rw: something renamed");
    }
    Leave::Either
}

/// The file a data call touches, by every name it has.
fn data(run: &Run, p: &[Vec<u8>], emptied: bool, zeros: bool) -> Leave {
    Leave::Data { names: run.f.model.names_of(p), emptied, zeros }
}

/// Where a write into the file at `p` goes: now and then into a block
/// mkfs left unwritten, else where the input says.
fn aim(run: &Run, r: &mut Input, p: &[Vec<u8>], size: u64) -> u64 {
    if let Some(set) = run.unwritten.get(p) {
        if !set.is_empty() && r.chance(96) {
            let l = *set.iter().nth(r.below(set.len() as u64) as usize).expect("in range");
            return l * run.block + r.below(run.block);
        }
    }
    fsops::offset(r, size, run.block, run.f.model.limits.span)
}

/// A write of `len` bytes at `at` to the file at `p`: whether it went into
/// blocks mkfs left unwritten -- which must read as zeros until written,
/// and as what was written after.
fn into_unwritten(run: &mut Run, p: &[Vec<u8>], at: u64, len: u64) {
    if len == 0 {
        return;
    }
    let (first, last) = (at / run.block, (at + len - 1) / run.block);
    if let Some(set) = run.unwritten.get_mut(p) {
        let hit: Vec<u64> = set.range(first..=last).copied().collect();
        if !hit.is_empty() {
            reached("ext4rw: a write into an unwritten extent");
            for l in hit {
                set.remove(&l);
            }
        }
    }
}

fn open(run: &mut Run, r: &mut Input) -> Leave {
    use fs::vfs::{OPEN_APPEND, OPEN_CREATE, OPEN_READ, OPEN_TRUNCATE, OPEN_WRITE};
    if run.f.model.open.len() >= 12 {
        return Leave::Either;
    }
    let p = fsops::path(r, &run.f.model);
    let flags = match r.u8() % 6 {
        0 => OPEN_READ,
        1 => OPEN_WRITE | OPEN_CREATE,
        2 => OPEN_READ | OPEN_WRITE,
        3 => OPEN_APPEND | OPEN_CREATE,
        4 => OPEN_WRITE | OPEN_TRUNCATE,
        _ => r.u8() as usize & 31,
    };
    let res = run.f.model.resolve(&p);
    if run.blocked(&res) && flags & OPEN_CREATE != 0 {
        return Leave::Either;
    }
    run.f.open(&p, flags);
    let path = match res {
        Res::Node(n) => n,
        Res::Absent(mut d, n) => {
            d.push(n);
            d
        }
        Res::Invalid => return Leave::Either,
    };
    run.f.model.propagate(&path);
    data(run, &path, flags & OPEN_TRUNCATE != 0, false)
}

fn handle_path(run: &Run, i: usize) -> Option<Path> {
    run.f.model.open.get(i).map(|(_, o)| o.path.clone())
}

fn write(run: &mut Run, r: &mut Input) -> Leave {
    let i = r.below(run.f.model.open.len().max(1) as u64) as usize;
    let Some(path) = handle_path(run, i) else { return Leave::Either };
    let bs = run.block;
    let size = run.f.model.size(&path);
    match r.u8() % 4 {
        0 | 1 => {
            let n = fsops::length(r);
            let data = crate::input::noise(r.u32(), n);
            let at = run.f.model.open.get(i).map_or(0, |(_, o)| o.pos);
            run.f.write(i, &data);
            into_unwritten(run, &path, at, n as u64);
        }
        2 => {
            let at = aim(run, r, &path, size);
            let n = fsops::length(r);
            let data = crate::input::noise(r.u32(), n);
            run.f.write_within(i, at, &data);
            into_unwritten(run, &path, at, n as u64);
        }
        _ => {
            let at = fsops::offset(r, size, bs, run.f.model.limits.span);
            run.f.set_length(i, at);
            run.f.model.propagate(&path);
            return data(run, &path, false, true);
        }
    }
    run.f.model.propagate(&path);
    data(run, &path, false, false)
}

fn read(run: &mut Run, r: &mut Input) -> Leave {
    let i = r.below(run.f.model.open.len().max(1) as u64) as usize;
    let Some(path) = handle_path(run, i) else { return Leave::Either };
    let size = run.f.model.size(&path);
    match r.u8() % 3 {
        0 => {
            let n = fsops::length(r);
            run.f.read(i, n);
        }
        1 => {
            let at = fsops::offset(r, size, run.block, run.f.model.limits.span);
            let n = fsops::length(r);
            run.f.read_at(i, at, n);
        }
        _ => {
            let at = fsops::offset(r, size, run.block, run.f.model.limits.span);
            run.f.seek(i, at);
        }
    }
    Leave::Either
}

fn truncate(run: &mut Run, r: &mut Input) -> Leave {
    let p = fsops::path(r, &run.f.model);
    let Res::Node(path) = run.f.model.resolve(&p) else {
        run.f.truncate(&p, 0);
        return Leave::Either;
    };
    let size = run.f.model.size(&path);
    let to = fsops::offset(r, size, run.block, run.f.model.limits.span);
    run.f.truncate(&p, to);
    run.f.model.propagate(&path);
    data(run, &path, false, true)
}

fn write_file(run: &mut Run, r: &mut Input) -> Leave {
    let p = fsops::path(r, &run.f.model);
    let res = run.f.model.resolve(&p);
    if run.blocked(&res) {
        return Leave::Either;
    }
    let n = fsops::length(r);
    let data_in = crate::input::noise(r.u32(), n);
    run.f.write_file(&p, &data_in);
    let path = match res {
        Res::Node(n) => n,
        Res::Absent(mut d, n) => {
            d.push(n);
            d
        }
        Res::Invalid => return Leave::Either,
    };
    run.f.model.propagate(&path);
    data(run, &path, true, false)
}

fn image(run: &mut Run, r: &mut Input) -> Leave {
    let held = run.f.images.len();
    if held == 0 || r.u8() % 4 == 0 {
        if held < 4 {
            let p = fsops::path(r, &run.f.model);
            let write = !r.chance(4);
            run.f.image_open(&p, write);
            /* Another name of what is held, opened for writing: the hold is
             * the file's, by whatever name. */
            if let (Res::Node(n), true) = (run.f.model.resolve(&p), r.bool()) {
                if let Some(other) = run.f.model.names_of(&n).into_iter().find(|q| *q != n) {
                    run.f.open(&join(&other), fs::vfs::OPEN_WRITE);
                }
            }
        }
        return Leave::Either;
    }
    let i = r.below(held as u64) as usize;
    let Some((path, _)) = run.f.model.holds.get(i).cloned() else { return Leave::Either };
    let size = run.f.model.size(&path);
    let span = run.f.model.limits.span;
    match r.u8() % 8 {
        0 => {
            run.f.image_close(i);
            Leave::Either
        }
        1 => {
            run.image_sync(i);
            Leave::Either
        }
        2 | 3 => {
            let at = fsops::offset(r, size, run.block, span);
            let n = fsops::length(r);
            run.f.image_read(i, at, n);
            Leave::Either
        }
        k => {
            /* Synced as part of it: a write off the VFS's lock is the
             * device's until a flush, and the model has it at once. */
            let at = if k <= 5 { aim(run, r, &path, size) } else { fsops::offset(r, size, run.block, span) };
            if k <= 5 {
                let n = fsops::length(r);
                let data_in = crate::input::noise(r.u32(), n);
                run.f.image_write(i, at, &data_in);
                into_unwritten(run, &path, at, n as u64);
            } else {
                let len = (fsops::length(r) as u64).saturating_mul(1 + r.below(4));
                let zeroes = if r.bool() { None } else { Some(r.bool()) };
                run.f.image_discard(i, at, len, zeroes, false);
            }
            if i < run.f.images.len() {
                run.image_sync(i);
            }
            run.f.model.propagate(&path);
            data(run, &path, false, false)
        }
    }
}

fn look(run: &mut Run, r: &mut Input) -> Leave {
    let p = fsops::path(r, &run.f.model);
    match r.u8() % 4 {
        0 | 1 => run.f.stat(&p),
        2 if run.strict => {
            run.f.list(&p);
        }
        _ if run.strict => run.f.compare(),
        _ => {}
    }
    Leave::Either
}

fn close(run: &mut Run, r: &mut Input) -> Leave {
    let i = r.below(run.f.model.open.len().max(1) as u64) as usize;
    run.f.close(i);
    Leave::Either
}

fn sync(run: &mut Run, _r: &mut Input) -> Leave {
    let ok = run.f.vfs.sync();
    invariant!(ok || !run.strict, "sync failed");
    Leave::Either
}

/// Second names for some of the files, each in its own directory: the
/// tree given them, and the pairs for mkfs.
fn add_links(r: &mut Input, tree: &mut Node) -> Vec<(Path, Path)> {
    fn walk(r: &mut Input, n: &mut Node, at: &mut Path, out: &mut Vec<(Path, Path)>) {
        let Some(children) = n.children_mut() else { return };
        let files: Vec<Vec<u8>> = children.iter().filter(|(_, c)| !c.is_dir()).map(|(k, _)| k.clone()).collect();
        for name in files {
            if r.u8() % 4 != 1 {
                continue;
            }
            let mut link = name.clone();
            link.extend_from_slice(b"-ln");
            if link.len() >= fsops::NAME_MAX || children.contains_key(&link) {
                continue;
            }
            let Some(content) = children.get(&name).cloned() else { continue };
            children.insert(link.clone(), content);
            let mut l = at.clone();
            l.push(link);
            let mut t = at.clone();
            t.push(name);
            out.push((l, t));
        }
        let dirs: Vec<Vec<u8>> = children.iter().filter(|(_, c)| c.is_dir()).map(|(k, _)| k.clone()).collect();
        for d in dirs {
            at.push(d.clone());
            if let Some(c) = children.get_mut(&d) {
                walk(r, c, at, out);
            }
            at.pop();
        }
    }
    let mut out = Vec::new();
    walk(r, tree, &mut Vec::new(), &mut out);
    out
}

pub fn ext4rw(r: &mut Input) {
    let mut g = geometry(r);
    if g.journal_blocks == 0 {
        g.journal_blocks = 1024;
        if !g.valid() {
            return;
        }
    }
    let mut st = style(r);
    if one_in(r, 3) {
        st.journaled = r.range(1, 12) as u8;
    }
    let bs = g.block_size();
    let mut budget = g.blocks / 3;
    let mut inodes = g.inodes() / 3;
    let mut tree = tree(r, bs, &mut budget, &mut inodes);
    if r.chance(128) {
        st.links = add_links(r, &mut tree);
    }
    let Ok(made) = img::mkfs(&g, &tree, &st, MKFS_TIME, r.u64()) else { return };
    agree(&g, &st, &made, &tree);
    let sizes: Vec<u64> = [512, 1024, 2048, 4096].into_iter().filter(|s| *s <= bs).collect();
    let ss = r.pick(&sizes);
    let sectors = g.blocks * bs / ss + r.below(8);
    let dev_bytes = sectors * ss;
    let mut d = disk::Disk::new("vda", ss, sectors, made.media.clone());
    d.seed_chaos(r.u64());
    d.volatile = r.u8() % 4 != 0;
    d.latency = r.chance(64);
    let faults = r.chance(24);
    let cuts = !faults && d.volatile;
    if faults {
        for _ in 0..r.range(1, 3) {
            d.fail.insert(r.below(3000));
        }
        if r.chance(64) {
            d.dead_from = Some(r.below(3000));
        }
    } else if cuts {
        for _ in 0..r.range(1, 4) {
            d.crash_at.insert(r.below(3000));
        }
    }
    disk::insert(0, d);
    let Some(dev) = disk::register(0) else { return };
    let handle = dev.handle();
    let picture = if cuts {
        disk::insert(1, disk::Disk::new("vdb", ss, sectors, disk::Media::new()));
        disk::register(1).map(|p| p.handle())
    } else {
        None
    };
    let mounted = fs::ext4::mount_at("/", handle, false);
    if faults && mounted != 0 {
        reached("ext4rw: a mount that met a failing request, refused");
        return;
    }
    invariant!(mounted == 0, "ext4 did not mount for writing an image of the fuzzer's mkfs ({:?}, {:?}): {}", g, st,
               mounted);
    reached(if made.journaled.is_empty() { "ext4rw: mounted for writing" } else { "ext4rw: mounted, its journal replayed" });
    let mut want = tree;
    want.children_mut().expect("a directory").insert(b"lost+found".to_vec(), Node::dir());
    let vfs = fs::vfs_instance().expect("the VFS");
    let max_file = ((1u64 << 32) - 1) * bs;
    let mut model = Model::new(want, false, Limits::fixed(max_file, usize::MAX));
    model.punch_block = bs;
    let mut groups: BTreeMap<Path, Vec<Path>> = BTreeMap::new();
    for (link, target) in &st.links {
        groups.entry(target.clone()).or_insert_with(|| vec![target.clone()]).push(link.clone());
    }
    model.links = groups.into_values().collect();
    if !model.links.is_empty() {
        reached("ext4rw: files with two names");
    }
    let mut run = Run {
        f: Fs::new(vfs, model, !faults),
        hidden: made.hidden_at.clone(),
        strict: !faults,
        picture,
        dev_bytes,
        sector: ss,
        block: bs,
        uncertain: None,
        unwritten: made.unwritten.iter().map(|(p, v)| (p.clone(), v.iter().copied().collect())).collect(),
        image_syncs: Vec::new(),
    };
    /* Pictures the mount's own writes left: the tree as it was. */
    run.call(r, |_, _| Leave::Either);

    /* Where each mount begins among the disk's requests: a failed write
     * stops what the rest of its mount writes. */
    let mut sessions: Vec<u64> = vec![0];
    let mut ops = 0;
    let mut live = true;
    let mut cut_every = false;
    while r.more() && ops < 200 && live && run.uncertain.is_none() {
        ops += 1;
        if r.chance(10) {
            /* Unmounted and mounted again: refused while a file is open. */
            let busy = run.f.busy();
            let mut done = false;
            run.call(r, |run, _| {
                done = run.f.vfs.unmount(b"/");
                if done {
                    sessions.push(disk::with(0, |d| d.log.len() as u64));
                    let again = fs::ext4::mount_at("/", handle, false);
                    if !run.strict && again != 0 {
                        live = false;
                    } else {
                        invariant!(again == 0, "ext4 mounted again as {}", again);
                        reached("ext4rw: unmounted and mounted again");
                    }
                }
                Leave::Either
            });
            invariant!(done != busy, "an unmount with {} files and {} images open {}", run.f.model.open.len(),
                       run.f.images.len(), if done { "went through" } else { "was refused" });
            continue;
        }
        if cuts && !cut_every && r.chance(12) {
            /* The power cut before every request of the next call: every
             * point of its commit judged. */
            disk::with(0, |d| {
                let n = d.log.len() as u64;
                d.crash_at.extend(n..n + CUT_EVERY);
            });
            cut_every = true;
            reached("ext4rw: the power cut before every request of a call");
        }
        match r.u8() % 24 {
            0..=3 => run.call(r, create),
            4 | 5 => run.call(r, remove),
            6 | 7 => run.call(r, rename),
            8 | 9 => run.call(r, open),
            10 => run.call(r, close),
            11..=13 => run.call(r, write),
            14 => run.call(r, read),
            15 => run.call(r, truncate),
            16 | 17 => run.call(r, write_file),
            18..=20 => run.call(r, image),
            21 => run.call(r, sync),
            22 => {
                /* A directory of many entries, each its own call: it grows
                 * a block at a time, its extents among the others'. */
                let p = fsops::path(r, &run.f.model);
                if let Res::Node(d) = run.f.model.resolve(&p) {
                    if matches!(run.f.model.node(&d), Some(Node::Dir(_))) {
                        let tag = r.u16();
                        for k in 0..r.range(8, 80) {
                            if run.uncertain.is_some() {
                                /* A call met a failed request: the run ends
                                 * there, judged by that call's trees. */
                                break;
                            }
                            let mut c = d.clone();
                            c.push(format!("entry-{:04x}-{:03}-with-a-longer-name", tag, k).into_bytes());
                            let path = join(&c);
                            run.call(r, |run, _| {
                                make(run, &path, k % 7 == 0);
                                Leave::Either
                            });
                        }
                        reached("ext4rw: a directory grown by many entries");
                    }
                }
            }
            _ => run.call(r, look),
        }
    }
    if live {
        if !faults && run.uncertain.is_none() {
            run.f.compare();
        }
        run.f.close_all();
        run.call(r, |run, _| {
            invariant!(run.f.vfs.unmount(b"/"), "the unmount at the end was refused");
            Leave::Either
        });
    }
    if faults {
        /* What the failures left, replayed by the judge -- and the kernel
         * still mounting it: what the call that met them may leave. */
        let media = disk::with(0, |d| d.durable.clone());
        let c = img::check(&media, dev_bytes);
        /* In each mount, nothing written after its first failed write of
         * anything but a file's data -- the journal, a block's home, the
         * superblock, a flush. A file's data block that does not write
         * fails its call, its transaction undone, and leaves the filesystem
         * writable: one file's bad sector -- a guest disk's -- is not every
         * file's. Nor is an image's own flush the filesystem's. */
        let log: Vec<disk::Io> = disk::with(0, |d| d.log.clone());
        sessions.push(log.len() as u64);
        let writes = |io: &disk::Io| io.op != disk::Op::Read;
        let syncs = std::mem::take(&mut run.image_syncs);
        let others = |k: usize, io: &disk::Io| {
            if io.op == disk::Op::Flush {
                return syncs.iter().any(|&(s, e)| s <= k && k < e);
            }
            io.count != 0
                && (io.sector * ss / bs..=((io.sector + io.count) * ss - 1) / bs)
                    .all(|b| c.no_metadata.get(b as usize) == Some(&true))
        };
        for w in sessions.windows(2) {
            let (from, to) = (w[0] as usize, (w[1] as usize).min(log.len()));
            if (from..to).any(|k| writes(&log[k]) && !log[k].ok && others(k, &log[k])) {
                reached("ext4rw: a file's data, or an image's flush, failed, and the filesystem went on");
            }
            if let Some(k) = (from..to).find(|&k| writes(&log[k]) && !log[k].ok && !others(k, &log[k])) {
                let after = (k + 1..to).find(|&j| writes(&log[j]) && log[j].ok);
                invariant!(after.is_none(), "request {} was written after request {}, a write of metadata or a \
                           flush, failed", after.unwrap_or(0), k);
                reached("ext4rw: a write failed, and nothing written after it");
            }
        }
        invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the image after requests failed, replayed, is not \
                   clean: {:?} {:?}", c.corrupt, c.unclean);
        let got = c.tree.expect("a tree, from a clean image");
        let model = run.f.model.tree.clone();
        let (before, after, leave) = run.uncertain.clone().unwrap_or((model.clone(), model, Leave::Either));
        if let Err(e) = may_leave(&leave, &before, &after, &got, ss) {
            panic!("invariant: the image after requests failed is not what the call that met them may leave ({:?}): \
                    {}", leave, e);
        }
        disk::with(0, |d| {
            d.current = d.durable.clone();
            d.cache.clear();
            d.fail.clear();
            d.dead_from = None;
        });
        invariant!(fs::ext4::mount_at("/", handle, false) == 0, "the image after requests failed does not mount");
        invariant!(vfs.unmount(b"/"), "the image after requests failed does not unmount");
        reached("ext4rw: an image judged after requests failed");
        return;
    }
    /* The image a clean unmount leaves: clean, nothing to replay, the
     * model's tree. */
    disk::with(0, |d| d.no_writes = Some("its ext4 is unmounted".into()));
    let media = disk::with(0, |d| d.current.clone());
    let c = img::check(&media, dev_bytes);
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the image after a clean unmount is not clean: {:?} {:?}",
               c.corrupt, c.unclean);
    invariant!(!c.mounted && c.replayed == 0, "the image after a clean unmount is left mounted ({}) or with {} \
               transactions to replay", c.mounted, c.replayed);
    if let Some(t) = &c.tree {
        if let Some(diff) = run.f.model.tree.diff(t) {
            panic!("invariant: the image after a clean unmount holds another tree than the model's: {}", diff);
        }
    }
    if !too_big(&run.f.model.tree) {
        /* For e2fsck, and for debugfs to read every file out of whole. */
        let written = img::Made { media, inodes: BTreeMap::new(), hidden: made.hidden, hidden_at: BTreeMap::new(),
                                  journaled: Vec::new(), unwritten: BTreeMap::new() };
        dump("ext4rw", &written, &run.f.model.tree, g.blocks * bs);
    }
}
