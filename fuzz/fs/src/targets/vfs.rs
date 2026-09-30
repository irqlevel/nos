//! The VFS itself, over several filesystems at once: a ramfs at the root,
//! held to the model; procfs, ext2, nanofs and more ramfs mounted beside it
//! and taken down again at paths of every kind -- nested, doubled, without
//! a slash, too long; files open across it all; the C ABI the C++ half and
//! the modules reach it by (`kernel_vfs_*`, and the file ABI `kernel_file_*`
//! a module keeps its configuration through), fed words that are open
//! files, were, and never were; and tasks of their own at work on the
//! other mounts meanwhile. Held to what the VFS documents: a mount taken
//! exactly when its path is free and well-formed, its device nobody else's,
//! and there is room; an unmount refused while anything is open on it, and
//! a handle into it dead afterwards; a rename between two mounts refused;
//! every call of the file ABI what its composition of the VFS's calls
//! says; and the images of the filesystems taken down clean.

use fs::vfs::{Handle, MAX_MOUNTS, MAX_PATH, OPEN_READ, OPEN_WRITE};

use crate::image::{ext2 as ext2img, nanofs as nanoimg};
use crate::machine::{self, disk, sched};
use crate::model::{show, Data, Node};
use crate::targets::fsops::{self, Fs, Limits, Model, Opened};
use crate::{reached, Input};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ram,
    Proc,
    Ext2,
    Nano,
}

#[derive(Clone, Debug)]
struct Mount {
    path: Vec<u8>,
    kind: Kind,
    /// The block device it is on; 0 for none.
    dev: usize,
}

/// Where the other filesystems go: names the root's model never makes, so
/// that what is under them is never the model's.
const PLACES: &[&[u8]] = &[b"/M0", b"/M1", b"/M2", b"/M3", b"/M0/N", b"/M1/N/O"];

struct World {
    root: Fs,
    mounts: Vec<Mount>,
    ext2_dev: usize,
    nano_dev: usize,
    /// Handles into the other mounts, and which mount each is on.
    others: Vec<(Handle, Vec<u8>)>,
    tasks: Vec<usize>,
}

impl World {
    fn mount_path(&self, r: &mut Input) -> Vec<u8> {
        match r.u8() % 8 {
            0 => b"M9".to_vec(),
            1 => {
                let mut p = b"/".to_vec();
                p.extend(std::iter::repeat_n(b'L', MAX_PATH - 1 + r.below(2) as usize));
                p
            }
            2 => b"/".to_vec(),
            3 => b"/M0/".to_vec(),
            _ => r.pick(PLACES).to_vec(),
        }
    }

    /// A mount, taken exactly when the VFS's rules say.
    fn mount(&mut self, r: &mut Input) {
        let path = self.mount_path(r);
        let kind = r.pick(&[Kind::Ram, Kind::Proc, Kind::Ext2, Kind::Nano]);
        /* A device that carries the other kind now and then: not
         * recognised, and nothing mounted. */
        let crossed = r.chance(32);
        let dev = match (kind, crossed) {
            (Kind::Ext2, false) | (Kind::Nano, true) => self.ext2_dev,
            _ => self.nano_dev,
        };
        let text = String::from_utf8_lossy(&path).into_owned();
        let got = match kind {
            Kind::Ram => fs::ramfs::mount_at(&text, false),
            Kind::Proc => fs::procfs::mount_at(&text),
            Kind::Ext2 => fs::ext2::mount_at(&text, dev, false) >= 0,
            Kind::Nano => fs::nanofs::mount_at(&text, dev, false) >= 0,
        };
        let on_disk = matches!(kind, Kind::Ext2 | Kind::Nano);
        let device_busy = on_disk && self.mounts.iter().any(|m| m.dev == dev);
        let want = path.first() == Some(&b'/') && path.len() < MAX_PATH
            && !self.mounts.iter().any(|m| m.path == path) && self.mounts.len() < MAX_MOUNTS
            && !device_busy && !(on_disk && crossed) && path != b"/";
        invariant!(got == want, "a {:?} mount on {} {}, where the VFS's rules say {}", kind, show(&path),
                   if got { "was taken" } else { "was refused" }, if want { "it is" } else { "it is not" });
        if got {
            reached("a filesystem mounted beside the root");
            self.mounts.push(Mount { path, kind, dev: if on_disk { dev } else { 0 } });
        }
    }

    /// An unmount, refused exactly when something is open on the mount.
    fn unmount(&mut self, r: &mut Input) {
        if self.mounts.is_empty() {
            return;
        }
        let i = r.below(self.mounts.len() as u64) as usize;
        let path = self.mounts[i].path.clone();
        let busy = self.others.iter().any(|(_, m)| *m == path);
        /* Not while a task may be at work in it: what it holds open is its
         * own business. */
        self.tasks.retain(|t| !sched::done(*t));
        if !self.tasks.is_empty() {
            return;
        }
        let got = self.root.vfs.unmount(&path);
        fsops::say(format_args!("unmount {} -> {}", show(&path), got));
        invariant!(got != busy, "the unmount of {} {} with {} files open on it", show(&path),
                   if got { "went through" } else { "was refused" },
                   self.others.iter().filter(|(_, m)| *m == path).count());
        if got {
            self.mounts.remove(i);
            reached("a filesystem unmounted from beside the root");
        }
    }

    /// A path on one of the other mounts, or where one was.
    fn other_path(&self, r: &mut Input) -> Vec<u8> {
        let base: Vec<u8> = if !self.mounts.is_empty() && r.u8() % 4 != 0 {
            self.mounts[r.below(self.mounts.len() as u64) as usize].path.clone()
        } else {
            r.pick(PLACES).to_vec()
        };
        let mut p = base;
        let rest: Vec<u8> = match r.u8() % 8 {
            0 => Vec::new(),
            1 => b"/..".to_vec(),
            2 => b"/../a".to_vec(),
            3 => r.pick(&[&b"/version"[..], b"/cmdline", b"/interrupts", b"/lost+found"]).to_vec(),
            _ => {
                let mut v = b"/".to_vec();
                v.extend_from_slice(&fsops::name(r));
                v
            }
        };
        p.extend_from_slice(&rest);
        p
    }

    /// An operation on one of the other mounts: nothing held to a model,
    /// everything to not failing badly.
    fn other(&mut self, r: &mut Input) {
        let vfs = self.root.vfs;
        let p = self.other_path(r);
        /* Where nothing is mounted any more, the path is the root's -- and
         * the root is the model's. */
        if self.owner_of(&p) == b"/" {
            return;
        }
        match r.u8() % 8 {
            0 => {
                let _ = vfs.stat(&p);
            }
            1 => {
                let mut i = 0;
                while vfs.read_dir(&p, i).is_some() && i < 10_000 {
                    i += 1;
                }
            }
            2 => {
                let flags = r.pick(&[OPEN_READ, OPEN_WRITE | fs::vfs::OPEN_CREATE, OPEN_READ | OPEN_WRITE]);
                if let Some(h) = vfs.open(&p, flags) {
                    let owner = self.owner_of(&p);
                    fsops::say(format_args!("other open {} -> {:#x} on {}", show(&p), Handle::into_raw(Some(h)), show(&owner)));
                    self.others.push((h, owner));
                }
            }
            3 => {
                if !self.others.is_empty() {
                    let (h, _) = self.others.remove(r.below(self.others.len() as u64) as usize);
                    fsops::say(format_args!("other close {:#x}", Handle::into_raw(Some(h))));
                    vfs.close(h);
                }
            }
            4 => {
                if let Some(&(h, _)) = self.others.get(r.below(self.others.len().max(1) as u64) as usize) {
                    let data = crate::input::noise(r.u32(), fsops::length(r));
                    let _ = vfs.write(h, &data);
                    let mut buf = vec![0u8; fsops::length(r)];
                    let _ = vfs.read(h, &mut buf);
                    let _ = vfs.size(h);
                }
            }
            5 => {
                let dir = r.bool();
                let _ = vfs.create(&p, dir);
            }
            6 => {
                let q = self.other_path(r);
                if self.owner_of(&q) != b"/" {
                    let _ = vfs.rename(&p, &q);
                }
            }
            _ => {
                let _ = vfs.remove(&p);
            }
        }
    }

    /// Which mount path a path is under -- the longest that is a prefix at
    /// a slash, as the VFS picks it.
    fn owner_of(&self, p: &[u8]) -> Vec<u8> {
        let mut best: &[u8] = b"/";
        for m in &self.mounts {
            let mp = &m.path[..];
            if p.starts_with(mp) && (p.len() == mp.len() || p[mp.len()] == b'/') && mp.len() > best.len() {
                best = mp;
            }
        }
        best.to_vec()
    }

    /// A rename from the root to another mount, or back: refused.
    fn across(&mut self, r: &mut Input) {
        if self.mounts.is_empty() {
            return;
        }
        let a = fsops::path(r, &self.root.model);
        let b = self.other_path(r);
        let (from, to) = if r.bool() { (a, b) } else { (b, a) };
        if self.owner_of(&from) == self.owner_of(&to) {
            return;
        }
        let got = self.root.vfs.rename(&from, &to);
        invariant!(!got, "a rename from {} to {}, across two mounts, went through", show(&from), show(&to));
        reached("a rename across mounts refused");
    }

    /// What `mounts` says is what is mounted.
    fn listing(&mut self) {
        let vfs = self.root.vfs;
        let n = vfs.mount_count();
        invariant!(n == 1 + self.mounts.len(), "{} mounts, where {} were taken", n, 1 + self.mounts.len());
        let mut paths: Vec<(Vec<u8>, &str)> = (0..n).filter_map(|i| vfs.mount_info(i))
            .map(|m| (m.path().to_vec(), m.fs_name)).collect();
        let mut want: Vec<(Vec<u8>, &str)> = self.mounts.iter().map(|m| (m.path.clone(), match m.kind {
            Kind::Ram => "ramfs",
            Kind::Proc => "procfs",
            Kind::Ext2 => "ext2",
            Kind::Nano => "nanofs",
        })).collect();
        want.push((b"/".to_vec(), "ramfs"));
        paths.sort();
        want.sort();
        invariant!(paths == want, "the mounts are {:?}, and were taken as {:?}",
                   paths.iter().map(|(p, k)| format!("{} {}", show(p), k)).collect::<Vec<_>>(),
                   want.iter().map(|(p, k)| format!("{} {}", show(p), k)).collect::<Vec<_>>());
        let _ = crate::machine::cmd::run("mounts");
    }

    /// A task of its own at work on the other mounts, as another program
    /// on the machine is: through the VFS, a few calls, and done.
    fn task(&mut self, r: &mut Input) {
        if self.mounts.is_empty() || self.tasks.len() >= 3 {
            return;
        }
        let paths: Vec<Vec<u8>> = (0..8).map(|_| self.other_path(r)).filter(|p| self.owner_of(p) != b"/").collect();
        let seed = r.u32();
        let vfs = self.root.vfs;
        let id = sched::spawn("fsuser", sched::Kind::Kernel, machine::next_cpu(), Box::new(move || {
            let mut k = seed;
            for p in &paths {
                k = k.wrapping_mul(1_103_515_245).wrapping_add(12345);
                match k % 5 {
                    0 => {
                        let _ = vfs.create(p, k & 1 == 0);
                    }
                    1 => {
                        if let Some(h) = vfs.open(p, OPEN_READ | OPEN_WRITE | fs::vfs::OPEN_CREATE) {
                            let data = [k as u8; 700];
                            let _ = vfs.write(h, &data);
                            kcore::task::yield_to_runnable();
                            let mut buf = [0u8; 300];
                            let _ = vfs.read(h, &mut buf);
                            vfs.close(h);
                        }
                    }
                    2 => {
                        let _ = vfs.remove(p);
                    }
                    3 => {
                        let _ = vfs.write_file(p, &[1, 2, 3]);
                    }
                    _ => {
                        let _ = vfs.sync();
                    }
                }
            }
        }));
        self.tasks.push(id);
        reached("tasks at work on the other mounts");
    }
}

/* ---- the C ABI ---- */

extern "C" {
    fn kernel_vfs_open(path: *const u8, len: usize, flags: usize) -> *mut core::ffi::c_void;
    fn kernel_vfs_close(file: *mut core::ffi::c_void);
    fn kernel_vfs_read(file: *mut core::ffi::c_void, buf: *mut u8, len: usize, out: *mut usize) -> i32;
    fn kernel_vfs_write(file: *mut core::ffi::c_void, data: *const u8, len: usize) -> i32;
    fn kernel_vfs_size(file: *mut core::ffi::c_void) -> usize;
    fn kernel_vfs_remove(path: *const u8, len: usize) -> i32;
    fn kernel_vfs_sync() -> i32;
}

/// What a word handed to the C ABI is.
enum Word {
    /// One of the root's open files: the index of it.
    Root(usize),
    /// Somebody else's: a file open on another mount, or a task's, or
    /// maybe one -- nothing to hold it to.
    Other,
    /// No open file's.
    Nothing,
}

fn classify(w: &mut World, word: usize) -> Word {
    if let Some(i) = w.root.model.open.iter().position(|(h, _)| Handle::into_raw(Some(*h)) == word) {
        return Word::Root(i);
    }
    w.tasks.retain(|t| !sched::done(*t));
    if !w.tasks.is_empty() || w.others.iter().any(|(h, _)| Handle::into_raw(Some(*h)) == word) {
        return Word::Other;
    }
    Word::Nothing
}

/// A word the C ABI is handed for an open file: one of the open files', one
/// that was, or one that never was.
fn word(r: &mut Input, w: &World) -> usize {
    let live: Vec<usize> = w.root.model.open.iter().map(|(h, _)| Handle::into_raw(Some(*h))).collect();
    match r.u8() % 6 {
        0 | 1 if !live.is_empty() => live[r.below(live.len() as u64) as usize],
        2 if !live.is_empty() => live[r.below(live.len() as u64) as usize] ^ (1 << 32),
        _ => {
            let any = r.u64() as usize;
            r.pick(&[0usize, 1, 2, 3, 0xFFFF_FFFF, 1 << 32, usize::MAX, any])
        }
    }
}

impl World {
    fn abi(&mut self, r: &mut Input) {
        match r.u8() % 6 {
            0 => {
                /* An open by pointer and length: none, too long, or one of
                 * the model's. */
                let p = match r.u8() % 8 {
                    0 => Vec::new(),
                    1 => vec![b'/'; MAX_PATH],
                    _ => fsops::path(r, &self.root.model),
                };
                let flags = r.pick(&[OPEN_READ, OPEN_WRITE | fs::vfs::OPEN_CREATE, OPEN_READ | OPEN_WRITE]);
                // SAFETY: `p` is `p.len()` bytes.
                let raw = unsafe { kernel_vfs_open(p.as_ptr(), p.len(), flags) } as usize;
                fsops::say(format_args!("kernel_vfs_open {} {:#x} -> {:#x}", show(&p), flags, raw));
                let want = if p.is_empty() || p.len() >= MAX_PATH { None } else { self.root.model.open(&p, flags) };
                invariant!((raw != 0) == want.is_some(), "kernel_vfs_open of {} gave {:#x}, where the model {}",
                           show(&p), raw, if want.is_some() { "opens it" } else { "does not" });
                if let (Some(h), Some(o)) = (Handle::from_raw(raw), want) {
                    self.root.model.open.push((h, o));
                }
            }
            1 => {
                let w = word(r, self);
                let n = fsops::length(r);
                let mut buf = vec![0u8; n];
                let mut out = usize::MAX;
                /* Whose the word is, before the call: a task may finish, and
                 * close what it had, before the answer is looked at. */
                let known = classify(self, w);
                // SAFETY: the buffer's own length, and a word to write.
                let got = unsafe { kernel_vfs_read(w as *mut _, buf.as_mut_ptr(), n, &mut out) };
                match known {
                    Word::Other => {}
                    Word::Root(i) => {
                        let o: Opened = self.root.model.open[i].1.clone();
                        let want = (o.flags & OPEN_READ != 0).then(|| {
                            self.root.model.content(&crate::model::join(&o.path)).unwrap_or_default().read(o.pos, n as u64)
                        });
                        invariant!((got == 0) == want.is_some() && want.as_ref().is_none_or(|v| v.len() == out && buf[..out] == v[..]),
                                   "kernel_vfs_read of an open file gave {} ({} bytes), where the model {:?}", got, out,
                                   want.as_ref().map(Vec::len));
                        if let Some(v) = want {
                            self.root.model.open[i].1.pos += v.len() as u64;
                        }
                    }
                    Word::Nothing => invariant!(got == -1, "kernel_vfs_read of {:#x}, no open file, gave {}", w, got),
                }
            }
            2 => {
                let w = word(r, self);
                let known = classify(self, w);
                let data = crate::input::noise(r.u32(), fsops::length(r));
                if let Word::Other = known {
                    // SAFETY: the data's own length.
                    let _ = unsafe { kernel_vfs_write(w as *mut _, data.as_ptr(), data.len()) };
                } else if let Word::Root(i) = known {
                    /* As `write` on the handle, whose model the root's is. */
                    // SAFETY: the data's own length.
                    let got = unsafe { kernel_vfs_write(w as *mut _, data.as_ptr(), data.len()) };
                    let want = self.root.model_write(i, &data);
                    invariant!((got == 0) == want, "kernel_vfs_write to an open file gave {}, where the model {}", got,
                               if want { "writes it" } else { "does not" });
                } else {
                    // SAFETY: the data's own length.
                    let got = unsafe { kernel_vfs_write(w as *mut _, data.as_ptr(), data.len()) };
                    invariant!(got == -1, "kernel_vfs_write to {:#x}, no open file, gave {}", w, got);
                }
            }
            3 => {
                let w = word(r, self);
                let known = classify(self, w);
                // SAFETY: any word will do.
                let got = unsafe { kernel_vfs_size(w as *mut _) };
                let want = match known {
                    Word::Root(i) => Some(self.root.model.size(&self.root.model.open[i].1.path) as usize),
                    Word::Other => None,
                    Word::Nothing => Some(0),
                };
                invariant!(want.is_none_or(|x| x == got), "kernel_vfs_size of {:#x} is {}, and the model's {:?}", w,
                           got, want);
            }
            4 => {
                let w = word(r, self);
                fsops::say(format_args!("kernel_vfs_close {:#x}", w));
                // SAFETY: any word will do.
                unsafe { kernel_vfs_close(w as *mut _) };
                self.root.model.open.retain(|(h, _)| Handle::into_raw(Some(*h)) != w);
                self.others.retain(|(h, _)| Handle::into_raw(Some(*h)) != w);
            }
            _ => {
                let p = fsops::path(r, &self.root.model);
                if p.is_empty() || p.len() >= MAX_PATH {
                    return;
                }
                // SAFETY: `p` is its own length.
                let got = unsafe { kernel_vfs_remove(p.as_ptr(), p.len()) };
                let want = self.root.model.remove(&p);
                invariant!((got == 0) == want, "kernel_vfs_remove of {} gave {}, where the model {}", show(&p), got,
                           if want { "removes it" } else { "does not" });
                // SAFETY: nothing taken.
                invariant!(unsafe { kernel_vfs_sync() } == 0, "kernel_vfs_sync failed");
            }
        }
    }

    /// The file ABI a module keeps its configuration through: each call
    /// what its composition of the VFS's calls does to the model.
    fn file_abi(&mut self, r: &mut Input) {
        let p = fsops::path(r, &self.root.model);
        let Ok(text) = std::str::from_utf8(&p) else { return };
        if p.is_empty() || p.len() >= MAX_PATH {
            return;
        }
        let m = &mut self.root.model;
        let located = |m: &Model| -> Option<Vec<u8>> {
            if m.stat(&p).is_some() {
                return Some(p.clone());
            }
            let mut next = p.clone();
            next.extend_from_slice(b".new");
            (next.len() < MAX_PATH && m.stat(&next).is_some()).then_some(next)
        };
        match r.u8() % 5 {
            0 => {
                // SAFETY: `text` is its own length.
                let got = unsafe { fs::files::kernel_file_size(text.as_ptr(), text.len()) };
                let want = match located(m).and_then(|at| m.stat(&at)) {
                    Some((false, size)) => size as isize,
                    _ => -1,
                };
                invariant!(got == want, "kernel_file_size of {} is {}, and the model's {}", text, got, want);
            }
            1 => {
                let data = crate::input::noise(r.u32(), fsops::length(r));
                // SAFETY: both are their own lengths.
                let got = unsafe { fs::files::kernel_file_write(text.as_ptr(), text.len(), data.as_ptr(), data.len()) };
                /* replace_file: the new content in <path>.new, synced; the
                 * old file removed; the new one renamed over it. */
                let mut next = p.clone();
                next.extend_from_slice(b".new");
                let want = if next.len() >= MAX_PATH || m.stat(&p).is_some_and(|(dir, _)| dir) {
                    false
                } else if !m.write_file(&next, &data) {
                    m.remove(&next);
                    false
                } else if m.stat(&p).is_some() && !m.remove(&p) {
                    false
                } else {
                    m.rename(&next, &p)
                };
                invariant!((got == 0) == want, "kernel_file_write of {} gave {}, where the model {}", text, got,
                           if want { "writes it" } else { "does not" });
                if want {
                    reached("a configuration file replaced");
                }
            }
            2 => {
                let data = crate::input::noise(r.u32(), fsops::length(r));
                // SAFETY: both are their own lengths.
                let got = unsafe { fs::files::kernel_file_create(text.as_ptr(), text.len(), data.as_ptr(), data.len()) };
                let want = if located(m).is_some() || !m.create(&p, false) {
                    1
                } else if m.write_file(&p, &data) {
                    0
                } else {
                    -1
                };
                invariant!(got == want, "kernel_file_create of {} gave {}, and the model {}", text, got, want);
            }
            3 => {
                // SAFETY: `text` is its own length.
                let got = unsafe { fs::files::kernel_file_remove(text.as_ptr(), text.len()) };
                let mut removed = false;
                let mut want = 0;
                while let Some(at) = located(m) {
                    if !m.remove(&at) {
                        want = -1;
                        break;
                    }
                    removed = true;
                }
                if want == 0 && !removed {
                    want = -1;
                }
                invariant!(got == want, "kernel_file_remove of {} gave {}, and the model {}", text, got, want);
            }
            _ => {
                let cap = fsops::length(r);
                let mut buf = vec![0u8; cap];
                // SAFETY: the buffer's own length.
                let got = unsafe { fs::files::kernel_file_read(text.as_ptr(), text.len(), buf.as_mut_ptr(), cap) };
                let want: isize = match located(m).and_then(|at| m.content(&at)) {
                    Some(d) => d.read(0, cap as u64).len() as isize,
                    None => -1,
                };
                invariant!(got == want, "kernel_file_read of {} gave {}, and the model {}", text, got, want);
            }
        }
    }
}

/// A small tree for an image beside the root.
fn small_tree(r: &mut Input) -> Node {
    let mut c = std::collections::BTreeMap::new();
    for k in 0..r.below(4) {
        c.insert(format!("f{}", k).into_bytes(), Node::File(Data::from(&crate::input::noise(r.u32(), r.below(5000) as usize))));
    }
    Node::Dir(c)
}

pub fn vfs(r: &mut Input) {
    let mut p = machine::params();
    p.cmdline = b"root=auto dhcp=off".to_vec();
    p.interrupts = vec![(b"timer".to_vec(), 12345), (b"nvme0-q1".to_vec(), 7), (b"a-name-longer-than-its-column".to_vec(), i64::MAX)];
    machine::set_params(p);

    /* The disks the other filesystems are on. */
    let g = crate::targets::ext2::geometry(r);
    let Ok(made) = ext2img::mkfs(&g, &small_tree(r), &Default::default(), 1_749_000_000, r.u64()) else { return };
    let bs = g.block_size() as u64;
    disk::insert(0, disk::Disk::new("vda", 512, g.blocks as u64 * bs / 512, made.media));
    let Ok(nano) = nanoimg::mkfs(&small_tree(r), [3u8; 16], 0) else { return };
    disk::insert(1, disk::Disk::new("vdb", 4096, nanoimg::DEVICE_BYTES / 4096, nano.media));
    let (Some(e), Some(n)) = (disk::register(0), disk::register(1)) else { return };

    invariant!(fs::ramfs::mount_at("/", false), "a ramfs would not mount at the root");
    let vfs = fs::vfs_instance().expect("the VFS");
    let mut w = World {
        root: Fs { vfs, model: Model::new(Node::dir(), false, Limits::memory()), strict: true },
        mounts: Vec::new(),
        ext2_dev: e.handle(),
        nano_dev: n.handle(),
        others: Vec::new(),
        tasks: Vec::new(),
    };
    let mut ops = 0;
    while r.more() && ops < 200 {
        ops += 1;
        match r.u8() % 14 {
            0..=4 => fsops::step(&mut w.root, r, 4096),
            5 => w.mount(r),
            6 => w.unmount(r),
            7 | 8 => w.other(r),
            9 => w.abi(r),
            10 => w.file_abi(r),
            11 => w.across(r),
            12 => w.task(r),
            _ => w.listing(),
        }
    }
    for t in std::mem::take(&mut w.tasks) {
        sched::join(t);
    }
    w.root.compare();
    w.root.close_all();
    for (h, _) in std::mem::take(&mut w.others) {
        vfs.close(h);
    }
    w.listing();

    /* The shutdown path's unmount of everything; and then the images of the
     * filesystems on disks are clean. */
    vfs.unmount_all();
    invariant!(vfs.mount_count() == 0, "{} mounts after unmounting everything", vfs.mount_count());
    let e2 = disk::with(0, |d| d.current.clone());
    let c = ext2img::check(&e2, g.blocks as u64 * bs);
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the ext2 beside the root is left {:?} {:?}", c.corrupt,
               c.unclean);
    let nm = disk::with(1, |d| d.current.clone());
    let c = nanoimg::check(&nm);
    invariant!(c.corrupt.is_empty() && c.unclean.is_empty(), "the nanofs beside the root is left {:?} {:?}", c.corrupt,
               c.unclean);
}
