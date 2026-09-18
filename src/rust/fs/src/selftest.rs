//! The filesystem self-test: what `fstest` runs from the shell, and what
//! `fstest=on` runs over the root filesystem at boot.
//!
//! It uses nothing but the file API, so it is the same test over ext2,
//! nanofs and ramfs alike -- and what it is really checking is that each of
//! them means the same thing by a write at an offset, a truncate that grows,
//! a rename across directories and a recursive remove.

use core::fmt::Write;

use kcore::cmd::Output;
use kcore::trace;

use crate::files::Buffer;
use crate::paths::Path;
use crate::vfs::{FileStat, DirEntry, OPEN_APPEND, OPEN_CREATE, OPEN_READ, OPEN_TRUNCATE,
                 OPEN_WRITE, File, Vfs};
use crate::vnode::{NAME_MAX, TYPE_DIR, TYPE_FILE};
use crate::vfs_instance;

/// The big file is written in chunks of one size and read back in chunks of
/// another, so no chunk boundary lines up with a block boundary twice.
const WRITE_CHUNK: usize = 64 * 1024;
const READ_CHUNK: usize = 12345;

/// Where a report goes: the log always, and the shell as well when the test
/// was asked for from there.
struct Reporter<'a> {
    out: Option<&'a mut Output>,
}

impl Reporter<'_> {
    fn say(&mut self, path: &str, what: &str) {
        trace!(0, "fstest: {}: {}", path, what);
        if let Some(out) = self.out.as_mut() {
            let _ = writeln!(out, "fstest: {}: {}", path, what);
        }
    }
}

/// A byte that depends on its offset alone, so any chunking reads it back.
fn pattern(offset: usize, salt: u8) -> u8 {
    ((offset * 7) ^ (offset >> 8) ^ (offset >> 16)) as u8 ^ salt
}

fn stat_of(vfs: &Vfs, path: &str) -> Option<FileStat> {
    let mut st = FileStat { node_type: 0, size: 0, ino: 0 };
    if vfs.stat(path.as_bytes(), &mut st) { Some(st) } else { None }
}

/// An open file that closes itself, so no path out of the test leaks one.
struct Handle<'a> {
    vfs: &'a Vfs,
    file: *mut File,
}

impl<'a> Handle<'a> {
    fn open(vfs: &'a Vfs, path: &str, flags: usize) -> Option<Self> {
        let file = vfs.open(path.as_bytes(), flags);
        if file.is_null() { None } else { Some(Self { vfs, file }) }
    }

    fn write(&self, data: &[u8]) -> bool {
        self.vfs.write(self.file, data.as_ptr(), data.len())
    }

    fn read(&self, buf: &mut Buffer, len: usize) -> Option<usize> {
        self.vfs.read(self.file, buf.as_mut_ptr(), len)
    }

    fn seek(&self, pos: usize) -> bool {
        self.vfs.seek(self.file, pos)
    }
}

impl Drop for Handle<'_> {
    fn drop(&mut self) {
        self.vfs.close(self.file);
    }
}

fn read_whole(vfs: &Vfs, path: &str, buf: &mut [u8]) -> Option<usize> {
    let file = Handle::open(vfs, path, OPEN_READ)?;
    let mut got = 0;
    while got < buf.len() {
        match vfs.read(file.file, unsafe { buf.as_mut_ptr().add(got) }, buf.len() - got) {
            Some(0) => break,
            Some(n) => got += n,
            None => return None,
        }
    }
    Some(got)
}

fn check_content(vfs: &Vfs, path: &str, expect: &[u8], report: &mut Reporter) -> bool {
    let mut buf = [0u8; 64];
    if expect.len() > buf.len() {
        return false;
    }

    match stat_of(vfs, path) {
        Some(st) if st.node_type == TYPE_FILE && st.size == expect.len() => {}
        _ => {
            report.say(path, "size is wrong");
            return false;
        }
    }

    match read_whole(vfs, path, &mut buf) {
        Some(got) if got == expect.len() && buf[..got] == *expect => true,
        _ => {
            report.say(path, "content is wrong");
            false
        }
    }
}

/* ---- the big file ---- */

const SALT: u8 = 0x5A;
const SALT2: u8 = 0xC3;

fn big_file(vfs: &Vfs, path: &str, size: usize, report: &mut Reporter) -> bool {
    let (mut wbuf, mut rbuf) = match (Buffer::new(WRITE_CHUNK), Buffer::new(READ_CHUNK)) {
        (Some(w), Some(r)) => (w, r),
        _ => {
            report.say(path, "alloc failed");
            return false;
        }
    };

    /* Sequential write in big chunks */
    {
        let file = match Handle::open(vfs, path, OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE) {
            Some(file) => file,
            None => { report.say(path, "create failed"); return false; }
        };
        let mut pos = 0;
        while pos < size {
            let chunk = (size - pos).min(WRITE_CHUNK);
            fill(&mut wbuf, chunk, pos, SALT);
            if !file.write(&wbuf.as_slice()[..chunk]) {
                report.say(path, "write failed");
                return false;
            }
            pos += chunk;
        }
    }

    match stat_of(vfs, path) {
        Some(st) if st.size == size => {}
        _ => { report.say(path, "size after write is wrong"); return false; }
    }

    /* Read back in odd-sized chunks */
    if !verify(vfs, path, &mut rbuf, size, None, report, "after write") {
        return false;
    }

    /* Overwrite a stretch in the middle, straddling block boundaries */
    let patch_off = size / 2 - 100;
    let patch_len = (4096 + 300).min(size - patch_off);
    {
        let file = match Handle::open(vfs, path, OPEN_WRITE) {
            Some(file) if file.seek(patch_off) => file,
            _ => { report.say(path, "open for patch failed"); return false; }
        };
        fill(&mut wbuf, patch_len, patch_off, SALT2);
        if !file.write(&wbuf.as_slice()[..patch_len]) {
            report.say(path, "patch write failed");
            return false;
        }
    }

    match stat_of(vfs, path) {
        Some(st) if st.size == size => {}
        _ => { report.say(path, "size after patch is wrong"); return false; }
    }

    let patched = Some((patch_off, patch_len));
    if !verify(vfs, path, &mut rbuf, size, patched, report, "after patch") {
        return false;
    }

    /* Cut the file short, then check what is left and that it ends there */
    let cut = size / 2 + 33;
    if !vfs.truncate(path.as_bytes(), cut) {
        report.say(path, "truncate failed");
        return false;
    }
    match stat_of(vfs, path) {
        Some(st) if st.size == cut => {}
        _ => { report.say(path, "size after truncate is wrong"); return false; }
    }
    if !verify(vfs, path, &mut rbuf, cut, patched, report, "after truncate") {
        return false;
    }

    /* Grow it back past the cut: the gap must read as zeros */
    {
        let file = match Handle::open(vfs, path, OPEN_WRITE | OPEN_APPEND) {
            Some(file) => file,
            None => { report.say(path, "append after truncate failed"); return false; }
        };
        if !file.write(b"END") {
            report.say(path, "append after truncate failed");
            return false;
        }
    }
    let tail = 3 + 5000;
    if !vfs.truncate(path.as_bytes(), cut + tail) {
        report.say(path, "truncate to grow failed");
        return false;
    }
    {
        let file = match Handle::open(vfs, path, OPEN_READ) {
            Some(file) if file.seek(cut) => file,
            _ => { report.say(path, "read of the grown tail failed"); return false; }
        };
        match file.read(&mut rbuf, tail) {
            Some(got) if got == tail => {}
            _ => { report.say(path, "read of the grown tail failed"); return false; }
        }
    }
    if &rbuf.as_slice()[..3] != b"END" {
        report.say(path, "appended bytes are wrong");
        return false;
    }
    if rbuf.as_slice()[3..tail].iter().any(|b| *b != 0) {
        report.say(path, "grown gap is not zero");
        return false;
    }

    true
}

fn fill(buf: &mut Buffer, len: usize, base: usize, salt: u8) {
    let slice = buf.as_mut_slice();
    for (i, byte) in slice[..len].iter_mut().enumerate() {
        *byte = pattern(base + i, salt);
    }
}

/// Read the whole file back in odd-sized chunks and check every byte against
/// the pattern -- the second salt over the patched stretch, when there is one.
fn verify(vfs: &Vfs, path: &str, rbuf: &mut Buffer, expect_len: usize,
          patched: Option<(usize, usize)>, report: &mut Reporter, when: &str) -> bool {
    let file = match Handle::open(vfs, path, OPEN_READ) {
        Some(file) => file,
        None => {
            report.say(path, "open for read failed");
            return false;
        }
    };

    let mut pos = 0;
    loop {
        let got = match file.read(rbuf, READ_CHUNK) {
            Some(0) => break,
            Some(got) => got,
            None => {
                report.say(path, "read failed");
                return false;
            }
        };

        for (i, byte) in rbuf.as_slice()[..got].iter().enumerate() {
            let off = pos + i;
            let want = match patched {
                Some((at, len)) if off >= at && off < at + len => pattern(off, SALT2),
                _ => pattern(off, SALT),
            };
            if *byte != want {
                report.say(path, "content mismatch");
                trace!(0, "fstest: {}: mismatch at {} {}", path, off, when);
                return false;
            }
        }
        pos += got;
    }

    if pos != expect_len {
        report.say(path, "short read");
        return false;
    }
    true
}

/* ---- the whole test ---- */

pub fn run(dir: &str, big_size: usize, out: Option<&mut Output>) -> bool {
    let mut report = Reporter { out };

    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => { report.say(dir, "no filesystem layer"); return false; }
    };

    match stat_of(vfs, dir) {
        Some(st) if st.node_type == TYPE_DIR => {}
        _ => { report.say(dir, "not a directory"); return false; }
    }

    let base = match Path::join(dir, "fstest.tmp") { Some(p) => p, None => {
        report.say(dir, "path too long"); return false; } };
    let paths = [
        Path::join(base.as_str(), "a.txt"),
        Path::join(base.as_str(), "b.txt"),
        Path::join(base.as_str(), "sub"),
        Path::join(base.as_str(), "big.bin"),
    ];
    let (a, b, sub, big) = match &paths {
        [Some(a), Some(b), Some(sub), Some(big)] => (a, b, sub, big),
        _ => { report.say(dir, "path too long"); return false; }
    };
    let c = match Path::join(sub.as_str(), "c.txt") { Some(p) => p, None => {
        report.say(dir, "path too long"); return false; } };

    /* Leftovers of an interrupted run */
    if stat_of(vfs, base.as_str()).is_some() && !vfs.remove(base.as_bytes()) {
        report.say(base.as_str(), "cannot remove leftovers");
        return false;
    }

    if !vfs.create(base.as_bytes(), true) {
        report.say(base.as_str(), "mkdir failed");
        return false;
    }

    let ok = body(vfs, &base, a, b, sub, &c, big, big_size, &mut report);
    if !ok {
        vfs.remove(base.as_bytes());
    }
    ok
}

#[allow(clippy::too_many_arguments)]
fn body(vfs: &Vfs, base: &Path, a: &Path, b: &Path, sub: &Path, c: &Path, big: &Path,
        big_size: usize, report: &mut Reporter) -> bool {
    if !vfs.write_file(a.as_bytes(), b"hello world".as_ptr(), 11)
        || !check_content(vfs, a.as_str(), b"hello world", report)
    {
        report.say(a.as_str(), "write and read back failed");
        return false;
    }

    /* Append */
    {
        let file = match Handle::open(vfs, a.as_str(), OPEN_APPEND) {
            Some(file) if file.write(b" again") => file,
            _ => { report.say(a.as_str(), "append failed"); return false; }
        };
        drop(file);
    }
    if !check_content(vfs, a.as_str(), b"hello world again", report) {
        return false;
    }

    /* Write at an offset */
    {
        let file = match Handle::open(vfs, a.as_str(), OPEN_WRITE) {
            Some(file) if file.seek(6) && file.write(b"WORLD") => file,
            _ => { report.say(a.as_str(), "write at offset failed"); return false; }
        };
        drop(file);
    }
    if !check_content(vfs, a.as_str(), b"hello WORLD again", report) {
        return false;
    }

    /* Shrink, then grow: the gap reads as zeros */
    if !vfs.truncate(a.as_bytes(), 5) || !check_content(vfs, a.as_str(), b"hello", report) {
        report.say(a.as_str(), "truncate failed");
        return false;
    }
    if !vfs.truncate(a.as_bytes(), 8)
        || !check_content(vfs, a.as_str(), b"hello\0\0\0", report)
    {
        report.say(a.as_str(), "truncate to grow failed");
        return false;
    }

    /* Rename in place, then move into a subdirectory */
    if !vfs.rename(a.as_bytes(), b.as_bytes())
        || stat_of(vfs, a.as_str()).is_some()
        || !check_content(vfs, b.as_str(), b"hello\0\0\0", report)
    {
        report.say(a.as_str(), "rename failed");
        return false;
    }
    if !vfs.create(sub.as_bytes(), true)
        || !vfs.rename(b.as_bytes(), c.as_bytes())
        || stat_of(vfs, b.as_str()).is_some()
        || !check_content(vfs, c.as_str(), b"hello\0\0\0", report)
    {
        report.say(b.as_str(), "move failed");
        return false;
    }

    let mut entry = DirEntry { name: [0; NAME_MAX], node_type: 0, size: 0 };
    let named_c = vfs.read_dir(sub.as_bytes(), 0, &mut entry)
        && entry_is(&entry, "c.txt") && entry.size == 8;
    if !named_c || vfs.read_dir(sub.as_bytes(), 1, &mut entry) {
        report.say(sub.as_str(), "readdir is wrong");
        return false;
    }

    /* Refusals: a missing file, a directory as a file, a duplicate name */
    let refused = vfs.open(b.as_bytes(), OPEN_READ).is_null()
        && vfs.open(sub.as_bytes(), OPEN_READ).is_null()
        && !vfs.create(sub.as_bytes(), true)
        && !vfs.create(c.as_bytes(), false);
    if !refused {
        report.say(sub.as_str(), "an operation that should fail succeeded");
        return false;
    }

    if big_size > 0 && !big_file(vfs, big.as_str(), big_size, report) {
        return false;
    }

    /* Everything goes with the directory */
    if !vfs.remove(base.as_bytes())
        || stat_of(vfs, base.as_str()).is_some()
        || stat_of(vfs, c.as_str()).is_some()
    {
        report.say(base.as_str(), "recursive remove failed");
        return false;
    }

    if !vfs.sync() {
        report.say(base.as_str(), "sync failed");
        return false;
    }

    true
}

fn entry_is(entry: &DirEntry, name: &str) -> bool {
    let len = entry.name.iter().position(|b| *b == 0).unwrap_or(NAME_MAX);
    &entry.name[..len] == name.as_bytes()
}

/* ---- what the kernel calls ---- */

/// The self-test over `dir` with a big file of `size` bytes: 0 passed.
///
/// # Safety
/// `dir` points at `dir_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_fs_selftest(dir: *const u8, dir_len: usize, size: usize) -> i32 {
    let dir = match unsafe { crate::path(dir, dir_len) } {
        Some(dir) => core::str::from_utf8(dir).unwrap_or(""),
        None => return -1,
    };
    if run(dir, size, None) { 0 } else { -1 }
}
