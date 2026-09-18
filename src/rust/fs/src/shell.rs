//! The shell's filesystem commands. They belong to this layer rather than to
//! `kernel/cmd.cpp`, which has no view of a file, a mount or a device to
//! reach through any more.
//!
//! Every message here is the one the C++ printed, because the tests match on
//! them -- `scripts/ext2-test.py` and `scripts/nanofs-test.py` drive the
//! filesystem entirely through these commands.

use core::fmt::Write;

use kcore::block::{self, Disk};
use kcore::cmd::Output;

use crate::files::{self, Buffer};
use crate::paths::{base_name, is_under, Path};
use crate::vfs::{FileStat, DirEntry, OPEN_APPEND, OPEN_CREATE, OPEN_READ, OPEN_TRUNCATE,
                 OPEN_WRITE};
use crate::vnode::{NAME_MAX, TYPE_DIR, TYPE_FILE};
use crate::{ext2, nanofs, ramfs, vfs_instance};

/// What `format`'s claim on its device says to whoever is refused it.
const FORMAT_HOLDER: &[u8] = b"format\0";

/// One file at a time through 64 KiB pieces: neither side has to fit in
/// memory.
const COPY_CHUNK: usize = 64 * 1024;

/// Deep enough for anything a rootfs holds; the paths cap it anyway.
const COPY_MAX_DEPTH: usize = 32;

fn args_of(args: &str) -> core::str::SplitWhitespace<'_> {
    args.split_whitespace()
}

fn stat_of(path: &str) -> Option<FileStat> {
    let vfs = vfs_instance()?;
    let mut st = FileStat { node_type: 0, size: 0, ino: 0 };
    if vfs.stat(path.as_bytes(), &mut st) { Some(st) } else { None }
}

fn entry_name(entry: &DirEntry) -> &str {
    let len = entry.name.iter().position(|b| *b == 0).unwrap_or(NAME_MAX);
    core::str::from_utf8(&entry.name[..len]).unwrap_or("?")
}

fn find_disk(name: &str, out: &mut Output) -> Option<Disk> {
    match Disk::open(name) {
        Some(disk) => Some(disk),
        None => {
            let _ = writeln!(out, "disk '{}' not found", name);
            None
        }
    }
}

/* ---- mounting ---- */

const MOUNT_USAGE_RAMFS: &str = "usage: mount ramfs <path>";
const MOUNT_USAGE_NANOFS: &str = "usage: mount nanofs <disk> <path>";
const MOUNT_USAGE_EXT2: &str = "usage: mount ext2 <disk> <path> [ro]";

pub fn mount(args: &str, out: &mut Output) {
    let mut tokens = args_of(args);
    let fs_name = match tokens.next() {
        Some(name) => name,
        None => {
            let _ = writeln!(out, "{}", MOUNT_USAGE_RAMFS);
            let _ = writeln!(out, "       mount nanofs <disk> <path>");
            let _ = writeln!(out, "       mount ext2 <disk> <path> [ro]");
            return;
        }
    };

    match fs_name {
        "ramfs" => {
            let path = match tokens.next() {
                Some(path) => path,
                None => { let _ = writeln!(out, "{}", MOUNT_USAGE_RAMFS); return; }
            };
            if ramfs::mount_at(path, false) {
                let _ = writeln!(out, "mounted ramfs on {}", path);
            } else {
                let _ = writeln!(out, "mount failed");
            }
        }
        "nanofs" => {
            let (disk, path) = match (tokens.next(), tokens.next()) {
                (Some(disk), Some(path)) => (disk, path),
                _ => { let _ = writeln!(out, "{}", MOUNT_USAGE_NANOFS); return; }
            };
            let dev = match find_disk(disk, out) { Some(dev) => dev, None => return };
            if nanofs::mount_at(path, dev.handle(), false) >= 0 {
                let _ = writeln!(out, "mounted nanofs on {}", path);
            } else {
                let _ = writeln!(out, "mount failed");
            }
        }
        "ext2" => {
            let (disk, path) = match (tokens.next(), tokens.next()) {
                (Some(disk), Some(path)) => (disk, path),
                _ => { let _ = writeln!(out, "{}", MOUNT_USAGE_EXT2); return; }
            };
            let read_only = match tokens.next() {
                None => false,
                Some("ro") => true,
                Some(_) => { let _ = writeln!(out, "{}", MOUNT_USAGE_EXT2); return; }
            };
            let dev = match find_disk(disk, out) { Some(dev) => dev, None => return };
            match ext2::mount_at(path, dev.handle(), read_only) {
                mounted if mounted < 0 => { let _ = writeln!(out, "mount failed"); }
                mounted => {
                    let _ = writeln!(out, "mounted ext2 on {} ({})", path,
                        if mounted == 1 { "ro" } else { "rw" });
                }
            }
        }
        other => { let _ = writeln!(out, "unknown filesystem '{}'", other); }
    }
}

pub fn umount(args: &str, out: &mut Output) {
    let path = match args_of(args).next() {
        Some(path) => path,
        None => { let _ = writeln!(out, "usage: umount <path>"); return; }
    };

    match vfs_instance() {
        Some(vfs) if vfs.unmount(path.as_bytes()) => {
            let _ = writeln!(out, "unmounted {}", path);
        }
        _ => { let _ = writeln!(out, "not mounted"); }
    }
}

pub fn mounts(_args: &str, out: &mut Output) {
    files::dump_mounts(out);
}

pub fn format(args: &str, out: &mut Output) {
    let mut tokens = args_of(args);
    let fs_name = match tokens.next() {
        Some(name) => name,
        None => { let _ = writeln!(out, "usage: format nanofs <disk>"); return; }
    };
    if fs_name != "nanofs" {
        let _ = writeln!(out, "unknown filesystem '{}'", fs_name);
        return;
    }

    let name = match tokens.next() {
        Some(name) => name,
        None => { let _ = writeln!(out, "usage: format nanofs <disk>"); return; }
    };
    let dev = match find_disk(name, out) { Some(dev) => dev, None => return };

    /* Not under a mounted filesystem, the disk log or a write test, nor over
       a disk one of those holds a partition of */
    let claim = match block::claim_as(dev.handle(), FORMAT_HOLDER.as_ptr()) {
        Ok(claim) => claim,
        Err(held_by) => {
            let _ = writeln!(out, "disk '{}' is in use by {}", name, held_by);
            return;
        }
    };

    let formatted = nanofs::format_device(dev.handle());
    block::release(claim);

    if formatted {
        let _ = writeln!(out, "formatted {} as nanofs", name);
    } else {
        let _ = writeln!(out, "format failed");
    }
}

/* ---- files and directories ---- */

pub fn ls(args: &str, out: &mut Output) {
    let path = args_of(args).next().unwrap_or("/");
    files::list_dir(path, out);
}

pub fn cat(args: &str, out: &mut Output) {
    match args_of(args).next() {
        Some(path) => { files::read_file(path, out); }
        None => { let _ = writeln!(out, "usage: cat <path>"); }
    }
}

/// The rest of the line after the first token, leading spaces dropped: what
/// `write` and `append` put in the file.
fn rest_after_first(args: &str) -> &str {
    let args = args.trim_start();
    match args.find(char::is_whitespace) {
        Some(at) => args[at..].trim_start_matches(' '),
        None => "",
    }
}

pub fn write(args: &str, out: &mut Output) {
    let path = match args_of(args).next() {
        Some(path) => path,
        None => { let _ = writeln!(out, "usage: write <path> <text>"); return; }
    };
    let content = rest_after_first(args);

    let vfs = match vfs_instance() { Some(vfs) => vfs, None => return };
    if vfs.write_file(path.as_bytes(), content.as_ptr(), content.len()) {
        let _ = writeln!(out, "wrote {} bytes", content.len());
    } else {
        let _ = writeln!(out, "write failed");
    }
}

pub fn append(args: &str, out: &mut Output) {
    let path = match args_of(args).next() {
        Some(path) => path,
        None => { let _ = writeln!(out, "usage: append <path> <text>"); return; }
    };
    let content = rest_after_first(args);

    let vfs = match vfs_instance() { Some(vfs) => vfs, None => return };
    let file = vfs.open(path.as_bytes(), OPEN_APPEND | OPEN_CREATE);
    if file.is_null() {
        let _ = writeln!(out, "open failed");
        return;
    }

    if vfs.write(file, content.as_ptr(), content.len()) {
        let _ = writeln!(out, "appended {} bytes", content.len());
    } else {
        let _ = writeln!(out, "write failed");
    }
    vfs.close(file);
}

pub fn mkdir(args: &str, out: &mut Output) {
    let path = match args_of(args).next() {
        Some(path) => path,
        None => { let _ = writeln!(out, "usage: mkdir <path>"); return; }
    };
    match vfs_instance() {
        Some(vfs) if vfs.create(path.as_bytes(), true) => {
            let _ = writeln!(out, "created {}", path);
        }
        _ => { let _ = writeln!(out, "mkdir failed"); }
    }
}

pub fn touch(args: &str, out: &mut Output) {
    let path = match args_of(args).next() {
        Some(path) => path,
        None => { let _ = writeln!(out, "usage: touch <path>"); return; }
    };
    match vfs_instance() {
        Some(vfs) if vfs.create(path.as_bytes(), false) => {
            let _ = writeln!(out, "created {}", path);
        }
        _ => { let _ = writeln!(out, "touch failed"); }
    }
}

pub fn del(args: &str, out: &mut Output) {
    let path = match args_of(args).next() {
        Some(path) => path,
        None => { let _ = writeln!(out, "usage: del <path>"); return; }
    };
    match vfs_instance() {
        Some(vfs) if vfs.remove(path.as_bytes()) => {
            let _ = writeln!(out, "removed {}", path);
        }
        _ => { let _ = writeln!(out, "del failed"); }
    }
}

pub fn mv(args: &str, out: &mut Output) {
    let mut tokens = args_of(args);
    let (from, to) = match (tokens.next(), tokens.next()) {
        (Some(from), Some(to)) => (from, to),
        _ => { let _ = writeln!(out, "usage: mv <old> <new>"); return; }
    };

    match vfs_instance() {
        Some(vfs) if vfs.rename(from.as_bytes(), to.as_bytes()) => {
            let _ = writeln!(out, "moved {} to {}", from, to);
        }
        _ => { let _ = writeln!(out, "mv failed"); }
    }
}

pub fn stat(args: &str, out: &mut Output) {
    let path = match args_of(args).next() {
        Some(path) => path,
        None => { let _ = writeln!(out, "usage: stat <path>"); return; }
    };

    match stat_of(path) {
        None => { let _ = writeln!(out, "not found"); }
        Some(st) if st.node_type == TYPE_DIR => {
            let _ = writeln!(out, "{}: directory, inode {}", path, st.ino);
        }
        Some(st) => {
            let _ = writeln!(out, "{}: file, {} bytes, inode {}", path, st.size, st.ino);
        }
    }
}

pub fn sync(_args: &str, out: &mut Output) {
    match vfs_instance() {
        Some(vfs) if vfs.sync() => { let _ = writeln!(out, "synced"); }
        _ => { let _ = writeln!(out, "sync failed"); }
    }
}

/* ---- cp ---- */

/// `dst`, or `dst/<basename of src>` when `dst` is an existing directory.
fn copy_target(src: &str, dst: &str) -> Option<Path> {
    match stat_of(dst) {
        Some(st) if st.node_type == TYPE_DIR => Path::join(dst, base_name(src)),
        _ => Path::from(dst),
    }
}

/// One file, through the file API in pieces: neither side has to fit in
/// memory. Fails with the target left as it was written so far.
fn copy_file(src: &str, dst: &str, out: &mut Output) -> Option<usize> {
    let vfs = vfs_instance()?;

    let input = vfs.open(src.as_bytes(), OPEN_READ);
    if input.is_null() {
        let _ = writeln!(out, "cp: cannot open {}", src);
        return None;
    }

    let output = vfs.open(dst.as_bytes(), OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE);
    if output.is_null() {
        let _ = writeln!(out, "cp: cannot create {}", dst);
        vfs.close(input);
        return None;
    }

    let mut buf = match Buffer::new(COPY_CHUNK) {
        Some(buf) => buf,
        None => {
            let _ = writeln!(out, "cp: alloc failed");
            vfs.close(output);
            vfs.close(input);
            return None;
        }
    };

    let mut copied = 0;
    let mut ok = true;
    loop {
        let got = match vfs.read(input, buf.as_mut_ptr(), COPY_CHUNK) {
            Some(0) => break,
            Some(got) => got,
            None => {
                let _ = writeln!(out, "cp: read from {} failed", src);
                ok = false;
                break;
            }
        };
        if !vfs.write(output, buf.as_slice().as_ptr(), got) {
            let _ = writeln!(out, "cp: write to {} failed", dst);
            ok = false;
            break;
        }
        copied += got;
    }

    vfs.close(output);
    vfs.close(input);
    if ok { Some(copied) } else { None }
}

fn copy_tree(src: &str, dst: &str, depth: usize, out: &mut Output,
             files_done: &mut usize, bytes: &mut usize) -> bool {
    let vfs = match vfs_instance() { Some(vfs) => vfs, None => return false };

    if depth >= COPY_MAX_DEPTH {
        let _ = writeln!(out, "cp: {}: too deep", src);
        return false;
    }

    match stat_of(dst) {
        None => {
            if !vfs.create(dst.as_bytes(), true) {
                let _ = writeln!(out, "cp: cannot create directory {}", dst);
                return false;
            }
        }
        Some(st) if st.node_type != TYPE_DIR => {
            let _ = writeln!(out, "cp: {} exists and is not a directory", dst);
            return false;
        }
        Some(_) => {}
    }

    let mut index = 0;
    loop {
        let mut entry = DirEntry { name: [0; NAME_MAX], node_type: 0, size: 0 };
        if !vfs.read_dir(src.as_bytes(), index, &mut entry) {
            break;
        }
        index += 1;

        let name = entry_name(&entry);
        let (from, to) = match (Path::join(src, name), Path::join(dst, name)) {
            (Some(from), Some(to)) => (from, to),
            _ => {
                let _ = writeln!(out, "cp: path too long under {}", src);
                return false;
            }
        };

        if entry.node_type == TYPE_DIR {
            if !copy_tree(from.as_str(), to.as_str(), depth + 1, out, files_done, bytes) {
                return false;
            }
        } else {
            match copy_file(from.as_str(), to.as_str(), out) {
                Some(copied) => { *files_done += 1; *bytes += copied; }
                None => return false,
            }
        }
    }

    true
}

pub fn cp(args: &str, out: &mut Output) {
    let mut tokens = args_of(args);
    let mut first = tokens.next();
    let recursive = first == Some("-r");
    if recursive {
        first = tokens.next();
    }

    let (src, dst_arg) = match (first, tokens.next()) {
        (Some(src), Some(dst)) => (src, dst),
        _ => { let _ = writeln!(out, "usage: cp [-r] <src> <dst>"); return; }
    };

    let st = match stat_of(src) {
        Some(st) => st,
        None => { let _ = writeln!(out, "cp: {} not found", src); return; }
    };

    let dst = match copy_target(src, dst_arg) {
        Some(dst) => dst,
        None => { let _ = writeln!(out, "cp: path too long"); return; }
    };

    if src == dst.as_str() {
        let _ = writeln!(out, "cp: {} and {} are the same file", src, dst.as_str());
        return;
    }

    if st.node_type == TYPE_DIR {
        if !recursive {
            let _ = writeln!(out, "cp: {} is a directory (use -r)", src);
            return;
        }
        if is_under(src, dst.as_str()) {
            let _ = writeln!(out, "cp: cannot copy {} into itself", src);
            return;
        }
        let (mut done, mut bytes) = (0, 0);
        if copy_tree(src, dst.as_str(), 0, out, &mut done, &mut bytes) {
            let _ = writeln!(out, "copied {} files, {} bytes to {}", done, bytes, dst.as_str());
        }
        return;
    }

    if let Some(copied) = copy_file(src, dst.as_str(), out) {
        let _ = writeln!(out, "copied {} bytes to {}", copied, dst.as_str());
    }
}

/* ---- fstest ---- */

const DEFAULT_BIG_SIZE: usize = 300 * 1024;
const MAX_BIG_SIZE: usize = 1024 * 1024 * 1024;

/// A count with an optional K or M suffix, as the shell has always taken it.
fn parse_size(text: &str) -> Option<usize> {
    let (digits, mult) = match text.as_bytes().last() {
        Some(b'K') | Some(b'k') => (&text[..text.len() - 1], 1024),
        Some(b'M') | Some(b'm') => (&text[..text.len() - 1], 1024 * 1024),
        _ => (text, 1),
    };
    let size: usize = digits.parse().ok()?;
    let size = size.checked_mul(mult)?;
    if size > MAX_BIG_SIZE { None } else { Some(size) }
}

pub fn fstest(args: &str, out: &mut Output) {
    let mut tokens = args_of(args);
    let dir = tokens.next().unwrap_or("/");
    let big_size = match tokens.next() {
        None => DEFAULT_BIG_SIZE,
        Some(text) => match parse_size(text) {
            Some(size) => size,
            None => { let _ = writeln!(out, "usage: fstest [dir] [size[K|M]]"); return; }
        },
    };

    if crate::selftest::run(dir, big_size, Some(out)) {
        let _ = writeln!(out, "fstest: passed ({}, {} byte file)", dir, big_size);
    } else {
        let _ = writeln!(out, "fstest: FAILED");
    }
}

/* ---- registration ---- */

pub fn register_all() {
    let commands: &[(&str, &str, fn(&str, &mut Output))] = &[
        ("format", "format nanofs <disk> - format disk", format),
        ("mount", "mount <ramfs|nanofs|ext2> ... - mount filesystem", mount),
        ("umount", "umount <path> - unmount filesystem", umount),
        ("mounts", "mounts - list mount points", mounts),
        ("ls", "ls [path] - list directory (default /)", ls),
        ("cat", "cat <path> - show file content", cat),
        ("write", "write <path> <text> - write to file", write),
        ("mkdir", "mkdir <path> - create directory", mkdir),
        ("touch", "touch <path> - create empty file", touch),
        ("cp", "cp [-r] <src> <dst> - copy a file, or a directory tree with -r", cp),
        ("rm", "rm <path> - remove file or directory (recursively)", del),
        /* `del` was a hidden alias of `rm`; a registered command cannot be
         * hidden from `help`, so it says what it is instead. */
        ("del", "del <path> - the same as rm", del),
        ("append", "append <path> <text> - append text to file", append),
        ("mv", "mv <old> <new> - rename or move a file or directory", mv),
        ("stat", "stat <path> - show type, size and inode", stat),
        ("sync", "sync - flush filesystems to disk", sync),
        ("fstest", "fstest [dir] [size] - filesystem self-test", fstest),
    ];

    for (name, help, handler) in commands {
        let handler = *handler;
        match kcore::cmd::Command::register(name, help, move |args, out| handler(args, out)) {
            /* The command is the kernel's own and stays for good. */
            Ok(cmd) => core::mem::forget(cmd),
            Err(_) => kcore::trace!(0, "fs: cannot register the {} command", name),
        }
    }
}
