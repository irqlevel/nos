//! The filesystem self-test: what `fstest` runs from the shell, and what
//! `fstest=on` runs over the root filesystem at boot.
//!
//! It uses nothing but the file API, so it is the same test over ext2,
//! nanofs and ramfs alike -- and what it is really checking is that each of
//! them means the same thing by a write at an offset, a truncate that grows,
//! a rename across directories and a recursive remove. And one promise of
//! the VFS's own, over a filesystem of the test's: a call held up on one
//! mount holds up no call on another, and every other call on its own.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcore::cmd::Output;
use kcore::sync::Event;
use kcore::task::{self, TaskHandle};
use kcore::time::Duration;
use kcore::trace;

use crate::files::buffer;
use crate::image::Image;
use crate::paths::Path;
use crate::vfs::{FileStat, FileSystem, Open, Vfs, MAX_MOUNTS, OPEN_APPEND, OPEN_CREATE, OPEN_READ,
                 OPEN_TRUNCATE, OPEN_WRITE};
use crate::vnode::{Kind, NodeId, Tree};
use crate::vfs_instance;

/// The big file is written in chunks of one size and read back in chunks of
/// another, so no chunk boundary lines up with a block boundary twice.
const WRITE_CHUNK: usize = 64 * 1024;
const READ_CHUNK: usize = 12345;

/// The big file at its smallest: the patch in its middle and the cut past
/// it have to land inside it, as more than a block's worth.
pub const MIN_BIG_SIZE: usize = 8192;

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
    vfs.stat(path.as_bytes())
}

/// An open file that closes itself, so no path out of the test leaks one.
fn open<'a>(vfs: &'a Vfs, path: &str, flags: usize) -> Option<Open<'a>> {
    Open::new(vfs, path.as_bytes(), flags)
}

fn read_whole(vfs: &Vfs, path: &str, buf: &mut [u8]) -> Option<usize> {
    let file = open(vfs, path, OPEN_READ)?;
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
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
        Some(st) if st.kind == Kind::File && st.size == expect.len() => {}
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
    let (mut wbuf, mut rbuf) = match (buffer(WRITE_CHUNK), buffer(READ_CHUNK)) {
        (Some(w), Some(r)) => (w, r),
        _ => {
            report.say(path, "alloc failed");
            return false;
        }
    };

    /* Sequential write in big chunks */
    {
        let file = match open(vfs, path, OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE) {
            Some(file) => file,
            None => { report.say(path, "create failed"); return false; }
        };
        let mut pos = 0;
        while pos < size {
            let chunk = (size - pos).min(WRITE_CHUNK);
            fill(&mut wbuf, chunk, pos, SALT);
            if !file.write(&wbuf[..chunk]) {
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
        let file = match open(vfs, path, OPEN_WRITE) {
            Some(file) if file.seek(patch_off) => file,
            _ => { report.say(path, "open for patch failed"); return false; }
        };
        fill(&mut wbuf, patch_len, patch_off, SALT2);
        if !file.write(&wbuf[..patch_len]) {
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
        let file = match open(vfs, path, OPEN_WRITE | OPEN_APPEND) {
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
        let file = match open(vfs, path, OPEN_READ) {
            Some(file) if file.seek(cut) => file,
            _ => { report.say(path, "read of the grown tail failed"); return false; }
        };
        match file.read(&mut rbuf[..tail]) {
            Some(got) if got == tail => {}
            _ => { report.say(path, "read of the grown tail failed"); return false; }
        }
    }
    if &rbuf[..3] != b"END" {
        report.say(path, "appended bytes are wrong");
        return false;
    }
    if rbuf[3..tail].iter().any(|b| *b != 0) {
        report.say(path, "grown gap is not zero");
        return false;
    }

    true
}

fn fill(buf: &mut [u8], len: usize, base: usize, salt: u8) {
    for (i, byte) in buf[..len].iter_mut().enumerate() {
        *byte = pattern(base + i, salt);
    }
}

/// Read the whole file back in odd-sized chunks and check every byte against
/// the pattern -- the second salt over the patched stretch, when there is one.
fn verify(vfs: &Vfs, path: &str, rbuf: &mut [u8], expect_len: usize,
          patched: Option<(usize, usize)>, report: &mut Reporter, when: &str) -> bool {
    let file = match open(vfs, path, OPEN_READ) {
        Some(file) => file,
        None => {
            report.say(path, "open for read failed");
            return false;
        }
    };

    let mut pos = 0;
    loop {
        let got = match file.read(&mut rbuf[..READ_CHUNK]) {
            Some(0) => break,
            Some(got) => got,
            None => {
                report.say(path, "read failed");
                return false;
            }
        };

        for (i, byte) in rbuf[..got].iter().enumerate() {
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

/* ---- the far file ---- */

/// Where the far file has something written: across 2 and 4 GiB, where a
/// size takes a 32nd and a 33rd bit, and twice past them -- of 4 KiB ext2
/// blocks the last two under the triple indirect block, of 1 KiB blocks all
/// four. All inside what ext2 holds at any block size, 16 GiB at 1 KiB.
const FAR_SPOTS: [usize; 4] = [(1 << 31) - 150, (1 << 32) - 150, (1 << 32) + (8 << 20) + 3, (5 << 30) + 77];
/// Its size, past every spot.
const FAR_SIZE: usize = (5 << 30) + 4096;
/// What is written at each spot.
const FAR_PATCH: usize = 300;
/// What is read either side of a patch: holes, reading as zeros.
const FAR_MARGIN: usize = 64;
const FAR_SALT: u8 = 0x3C;

/// What the far file holds at `offset`, cut at `end` and grown back since:
/// a patch, or a hole.
fn far_byte(offset: usize, end: usize) -> u8 {
    let patched = FAR_SPOTS.iter().any(|spot| offset >= *spot && offset < spot + FAR_PATCH);
    if patched && offset < end { pattern(offset, FAR_SALT) } else { 0 }
}

/// Every patch and the holes either side of it, read back and held to
/// `far_byte`.
fn far_verify(vfs: &Vfs, path: &str, buf: &mut [u8], end: usize, report: &mut Reporter, when: &str) -> bool {
    let file = match open(vfs, path, OPEN_READ) {
        Some(file) => file,
        None => { report.say(path, "open of the far file failed"); return false; }
    };
    let len = FAR_PATCH + 2 * FAR_MARGIN;
    for spot in FAR_SPOTS {
        let from = spot - FAR_MARGIN;
        if !file.seek(from) {
            report.say(path, "seek in the far file failed");
            return false;
        }
        let mut got = 0;
        while got < len {
            match file.read(&mut buf[got..len]) {
                Some(0) | None => break,
                Some(n) => got += n,
            }
        }
        if got != len {
            report.say(path, "short read of the far file");
            return false;
        }
        if let Some(at) = (0..len).find(|i| buf[*i] != far_byte(from + i, end)) {
            report.say(path, "far file content mismatch");
            trace!(0, "fstest: {}: mismatch at {} {}", path, from + at, when);
            return false;
        }
    }
    true
}

/// A sparse file past 4 GiB, where the filesystem holds one at no cost but
/// what is written (`FileSystem::sparse_max`): holes up to its size, a
/// patch at each spot read back with the holes around it, the file cut in
/// the middle of one and grown back, what was cut reading as zeros. The
/// recursive remove at the end of the test then takes the file, tree and
/// all.
fn far_file(vfs: &Vfs, path: &str, report: &mut Reporter) -> bool {
    if vfs.sparse_max(path.as_bytes()) < FAR_SIZE {
        trace!(0, "fstest: {}: no sparse file that big here, the far file skipped", path);
        return true;
    }
    let mut buf = match buffer(FAR_PATCH + 2 * FAR_MARGIN) {
        Some(buf) => buf,
        None => { report.say(path, "alloc failed"); return false; }
    };

    if !vfs.write_file(path.as_bytes(), b"") || !vfs.truncate(path.as_bytes(), FAR_SIZE) {
        report.say(path, "the truncate past 4 GiB failed");
        return false;
    }
    {
        let file = match open(vfs, path, OPEN_WRITE) {
            Some(file) => file,
            None => { report.say(path, "open of the far file failed"); return false; }
        };
        for spot in FAR_SPOTS {
            fill(&mut buf, FAR_PATCH, spot, FAR_SALT);
            if !file.seek(spot) || !file.write(&buf[..FAR_PATCH]) {
                report.say(path, "a write past 2 GiB failed");
                return false;
            }
        }
    }
    match stat_of(vfs, path) {
        Some(st) if st.size == FAR_SIZE => {}
        _ => { report.say(path, "the far file's size is wrong"); return false; }
    }
    if !far_verify(vfs, path, &mut buf, FAR_SIZE, report, "after the writes") {
        return false;
    }

    /* Cut in the middle of the second patch: the two past it go, and the
     * end of it, and nothing reads past the cut */
    let cut = FAR_SPOTS[1] + FAR_PATCH / 2;
    if !vfs.truncate(path.as_bytes(), cut) {
        report.say(path, "the cut of the far file failed");
        return false;
    }
    let ended = open(vfs, path, OPEN_READ).is_some_and(|file| file.seek(cut) && file.read(&mut buf) == Some(0));
    match stat_of(vfs, path) {
        Some(st) if st.size == cut && ended => {}
        _ => { report.say(path, "the far file does not end at the cut"); return false; }
    }

    if !vfs.truncate(path.as_bytes(), FAR_SIZE) {
        report.say(path, "the far file would not grow back");
        return false;
    }
    if !far_verify(vfs, path, &mut buf, cut, report, "grown back after the cut") {
        return false;
    }

    report.say(path, "a sparse file past 4 GiB written, read back, cut and grown again");
    true
}

/* ---- a disk's image ---- */

/// The image's size: whole blocks of every size ext2 has, more than a
/// batch's worth of 1 KiB ones.
const IMAGE_SIZE: usize = 96 * 1024;

/// What a held image is written, in order, each read back whole after it:
/// part of a block that is there, a hole, a stretch across a hole and
/// blocks that are there, whole blocks that are there, and the last sector
/// -- at, length, salt.
const IMAGE_WRITES: [(usize, usize, u8); 5] =
    [(8192 + 100, 3, 2), (30000, 6000, 3), (4000, 20000, 4), (8192, 8192, 5), (IMAGE_SIZE - 512, 512, 6)];

/// Then zeros over it -- at, length, unmap -- across blocks and holes, given
/// back and written; and a discard of whole blocks of every size ext2 has.
const IMAGE_ZEROES: [(usize, usize, bool); 2] = [(10000, 20000, true), (40000, 1000, false)];
const IMAGE_DISCARD: (usize, usize) = (32768, 4096);

/// A disk's image (`image`): a file with holes held, and read and written
/// as a guest's disk is -- off the VFS's lock where its filesystem keeps
/// blocks on a device: holes reading as zeros, a write into one, a write
/// over part of a block and the rest of it kept, a write across blocks that
/// are there and a hole. While it is held everything else that would write
/// the file is refused; let go of, the file reads as the image left it; and
/// held to read, two at once may, while nothing writes it.
fn image_file(vfs: &'static Vfs, path: &str, report: &mut Reporter) -> bool {
    let (Some(mut expect), Some(mut got)) = (buffer(IMAGE_SIZE), buffer(IMAGE_SIZE)) else {
        report.say(path, "no memory for the image's buffers");
        return false;
    };

    /* A file of holes, with blocks in the middle written the usual way */
    if !vfs.write_file(path.as_bytes(), b"") || !vfs.truncate(path.as_bytes(), IMAGE_SIZE) {
        report.say(path, "the image's file could not be made");
        return false;
    }
    fill(&mut expect[8192..], 4096, 8192, 1);
    match open(vfs, path, OPEN_WRITE) {
        Some(file) if file.seek(8192) && file.write(&expect[8192..12288]) => {}
        _ => {
            report.say(path, "the image's middle could not be written");
            return false;
        }
    }

    let Some(mut image) = Image::open(vfs, path.as_bytes(), true) else {
        report.say(path, "the file could not be held as an image");
        return false;
    };

    let refused = open(vfs, path, OPEN_WRITE).is_none()
        && open(vfs, path, OPEN_APPEND).is_none()
        && open(vfs, path, OPEN_READ | OPEN_TRUNCATE).is_none()
        && !vfs.truncate(path.as_bytes(), 0)
        && !vfs.remove(path.as_bytes())
        && Image::open(vfs, path.as_bytes(), false).is_none();
    if !refused || open(vfs, path, OPEN_READ).is_none() {
        report.say(path, "a held image was written, cut, removed or held again -- or could not be read");
        return false;
    }

    if !image.read_at(0, &mut got) || got != expect {
        report.say(path, "a held image does not read as the file was written");
        return false;
    }
    for (at, len, salt) in IMAGE_WRITES {
        fill(&mut expect[at..], len, at, salt);
        if !image.write_at(at as u64, &expect[at..at + len]) {
            report.say(path, "a write to the image failed");
            return false;
        }
        if !image.read_at(0, &mut got) || got != expect {
            report.say(path, "the image reads back wrong after a write");
            return false;
        }
    }
    if image.write_at(IMAGE_SIZE as u64 - 1, &[0, 0]) || image.read_at(IMAGE_SIZE as u64, &mut got[..1]) {
        report.say(path, "the image took a write or a read past its end");
        return false;
    }

    /* What a guest gives back: a write of zeros, its whole blocks given back
     * or written; and a discard, whose whole blocks read as zeros after it
     * where the filesystem gives blocks back, and as they were where it
     * gives nothing back -- what it covers of a block in part as it was */
    for (at, len, unmap) in IMAGE_ZEROES {
        expect[at..at + len].fill(0);
        if !image.write_zeroes(at as u64, len as u64, unmap) || !image.read_at(0, &mut got) || got != expect {
            report.say(path, "a write of zeros to the image failed, or reads back wrong");
            return false;
        }
    }
    let (at, len) = IMAGE_DISCARD;
    if image.gives_back() {
        expect[at..at + len].fill(0);
    }
    if !image.discard(at as u64, len as u64) || !image.read_at(0, &mut got) || got != expect {
        report.say(path, "a discard of the image failed, or reads back wrong");
        return false;
    }
    if image.discard(IMAGE_SIZE as u64 - 512, 1024) || image.write_zeroes(IMAGE_SIZE as u64, 1, true) {
        report.say(path, "the image took a discard or a write of zeros past its end");
        return false;
    }

    if !image.sync() {
        report.say(path, "the image's sync failed");
        return false;
    }
    drop(image);

    /* Let go of: the file as the image left it, through the VFS */
    got.fill(0xAA);
    if read_whole(vfs, path, &mut got) != Some(IMAGE_SIZE) || got != expect {
        report.say(path, "the file does not read as its image left it");
        return false;
    }

    let (Some(mut first), Some(_second)) =
        (Image::open(vfs, path.as_bytes(), false), Image::open(vfs, path.as_bytes(), false))
    else {
        report.say(path, "two could not hold the file to read");
        return false;
    };
    if open(vfs, path, OPEN_WRITE).is_some() || first.write_at(0, &[1]) {
        report.say(path, "a file held to read was written");
        return false;
    }
    if !first.read_at(0, &mut got) || got != expect {
        report.say(path, "an image held to read reads wrong");
        return false;
    }
    true
}

/* ---- two mounts at once ---- */

/// How long the gate holds a call before letting it go of itself: what ends
/// a test gone wrong, rather than a boot that never does.
const GATE_HOLD_SECS: u64 = 30;
/// How long a call on another mount may take while one is held at the gate:
/// well inside the hold, so that a VFS that made it wait for the held call
/// is caught waiting, and not let through when the hold runs out.
const APART_SECS: u64 = 10;
/// How long a second call on the gate's own mount is watched: it must still
/// be waiting at the end.
const SAME_MS: u64 = 100;

const APART_TEXT: &[u8] = b"written while a call on another mount was held";

/// What the gate's filesystem, the test and its tasks share.
struct Apart {
    /// Signalled by each call as it comes into the gate's filesystem
    entered: Event,
    /// What lets the held call go
    release: Event,
    /// Whether a call that comes in is held there: the first one is
    hold: AtomicBool,
    /// The calls in the gate's filesystem now, and the most at once
    inside: AtomicUsize,
    most: AtomicUsize,
    /// The call on the other mount: through, and whether it did what it was
    /// to
    apart_done: Event,
    apart_ok: AtomicBool,
    /// The second call on the gate's mount: about to be made, and through
    same_started: Event,
    same_done: Event,
}

/// A filesystem of an empty root whose lookups are held at a gate: a call
/// on a mount whose device is slow, as the rest of the machine sees it.
struct GateFs {
    tree: Tree,
    root: Option<NodeId>,
    shared: Arc<Apart>,
}

impl FileSystem for GateFs {
    fn name(&self) -> &'static str {
        "gate"
    }

    fn mount(&mut self, _read_only: bool) -> Option<bool> {
        Some(true)
    }

    fn unmount(&mut self) {}

    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    fn root(&self) -> Option<NodeId> {
        self.root
    }

    fn lookup(&mut self, _dir: NodeId, _name: &[u8]) -> Option<NodeId> {
        let shared = &self.shared;
        let others = shared.inside.fetch_add(1, Ordering::SeqCst);
        shared.most.fetch_max(others + 1, Ordering::SeqCst);
        shared.entered.signal();
        /* Held only when alone in here: a second waiter on the one event
         * would be a bug of the test's own, on top of the one it found. */
        if others == 0 && shared.hold.load(Ordering::SeqCst) {
            shared.release.wait_for(Duration::from_secs(GATE_HOLD_SECS));
        }
        shared.inside.fetch_sub(1, Ordering::SeqCst);
        None
    }

    fn create_file(&mut self, _dir: NodeId, _name: &[u8]) -> Option<NodeId> {
        None
    }

    fn create_dir(&mut self, _dir: NodeId, _name: &[u8]) -> Option<NodeId> {
        None
    }

    fn read(&mut self, _file: NodeId, _buf: &mut [u8], _offset: usize) -> bool {
        false
    }

    fn write(&mut self, _file: NodeId, _data: &[u8], _offset: usize) -> bool {
        false
    }

    fn truncate(&mut self, _file: NodeId, _size: usize) -> bool {
        false
    }

    fn rename(&mut self, _node: NodeId, _dir: NodeId, _name: &[u8]) -> bool {
        false
    }

    fn remove(&mut self, _node: NodeId) -> bool {
        false
    }
}

/// What a task of the test is handed: the VFS, what it shares, and the path
/// it calls on.
struct Probe {
    vfs: &'static Vfs,
    shared: Arc<Apart>,
    path: Path,
}

/// The call held at the gate, its mount's lock with it.
fn held_call(probe: Probe) {
    let _ = probe.vfs.stat(probe.path.as_bytes());
}

/// A file written, read back and removed on the other mount.
fn apart_call(probe: Probe) {
    let (vfs, path) = (probe.vfs, probe.path.as_str());
    let mut back = [0u8; APART_TEXT.len()];
    let ok = vfs.write_file(path.as_bytes(), APART_TEXT)
        && read_whole(vfs, path, &mut back) == Some(APART_TEXT.len())
        && back == APART_TEXT
        && vfs.remove(path.as_bytes());
    probe.shared.apart_ok.store(ok, Ordering::SeqCst);
    probe.shared.apart_done.signal();
}

/// A second call on the gate's mount.
fn same_call(probe: Probe) {
    probe.shared.same_started.signal();
    let _ = probe.vfs.stat(probe.path.as_bytes());
    probe.shared.same_done.signal();
}

/// The calls made while one is held at the gate, each in a task of its own
/// in `tasks`: what went other than it should, or nothing.
fn watch(vfs: &'static Vfs, shared: &Arc<Apart>, paths: [Path; 3], tasks: &mut [Option<TaskHandle>; 3])
    -> Result<(), &'static str>
{
    let [held, apart, same] = paths;
    let probe = |path| Probe { vfs, shared: shared.clone(), path };

    tasks[0] = task::spawn_with("fstest-held", probe(held), held_call);
    if tasks[0].is_none() {
        return Err("no task for the call to hold");
    }
    if !shared.entered.wait_for(Duration::from_secs(APART_SECS)) {
        return Err("the call to hold never came into the gate");
    }

    tasks[1] = task::spawn_with("fstest-apart", probe(apart), apart_call);
    if tasks[1].is_none() {
        return Err("no task for the call on the other mount");
    }
    if !shared.apart_done.wait_for(Duration::from_secs(APART_SECS)) {
        return Err("a call waited behind one held on another mount");
    }
    if !shared.apart_ok.load(Ordering::SeqCst) {
        return Err("a file written, read and removed while a call on another mount was held came out wrong");
    }

    tasks[2] = task::spawn_with("fstest-same", probe(same), same_call);
    if tasks[2].is_none() {
        return Err("no task for a second call on the gate's mount");
    }
    if !shared.same_started.wait_for(Duration::from_secs(APART_SECS)) {
        return Err("the second call on the gate's mount never started");
    }
    if shared.same_done.wait_for(Duration::from_millis(SAME_MS)) {
        return Err("a second call on one mount went in beside the first");
    }
    Ok(())
}

/// Two mounts at once: a filesystem whose call is held, mounted under the
/// test's directory, and while the call is held a file written, read back
/// and removed in that directory -- on the filesystem under test -- and a
/// second call on the held one's mount, which must wait. Skipped, and said
/// so, where the mount table has no room.
fn two_mounts(vfs: &'static Vfs, base: &Path, report: &mut Reporter) -> bool {
    let at = base.as_str();
    /* The calls are made by tasks, and waited for in one: early in boot,
     * where test_vfs runs the test over a ramfs, there are none yet. */
    if task::current_id_or_none() == 0 {
        report.say(at, "not in a task: two mounts at once not tried");
        return true;
    }
    if vfs.mount_count() >= MAX_MOUNTS {
        report.say(at, "no room in the mount table for the gate: two mounts at once not tried");
        return true;
    }
    let paths = (Path::join(at, "gate"), Path::join(at, "gate/held"), Path::join(at, "apart.txt"),
                 Path::join(at, "gate/same"));
    let (Some(gate), Some(held), Some(apart), Some(same)) = paths else {
        report.say(at, "path too long");
        return false;
    };
    let events = (Event::new(), Event::new(), Event::new(), Event::new(), Event::new());
    let (Some(entered), Some(release), Some(apart_done), Some(same_started), Some(same_done)) = events else {
        report.say(at, "no memory for the gate's events");
        return false;
    };
    let shared = Arc::new(Apart {
        entered,
        release,
        hold: AtomicBool::new(true),
        inside: AtomicUsize::new(0),
        most: AtomicUsize::new(0),
        apart_done,
        apart_ok: AtomicBool::new(false),
        same_started,
        same_done,
    });

    let mut tree = Tree::new();
    let Some(root) = tree.alloc(b"/", Kind::Dir) else {
        report.say(at, "no memory for the gate's root");
        return false;
    };
    tree[root].dir_loaded = true;
    let fs = GateFs { tree, root: Some(root), shared: shared.clone() };
    if vfs.mount(gate.as_bytes(), Box::new(fs), true).is_none() {
        report.say(gate.as_str(), "the gate's filesystem would not mount");
        return false;
    }

    let mut tasks: [Option<TaskHandle>; 3] = [None, None, None];
    let verdict = watch(vfs, &shared, [held, apart, same], &mut tasks);
    /* Whatever came of it: the held call let go, and every task of the test
     * waited out -- each ends, the held one when the hold runs out at the
     * latest. */
    shared.hold.store(false, Ordering::SeqCst);
    shared.release.signal();
    drop(tasks);

    let unmounted = vfs.unmount(gate.as_bytes());
    if let Err(why) = verdict {
        report.say(gate.as_str(), why);
        return false;
    }
    if shared.most.load(Ordering::SeqCst) != 1 {
        report.say(gate.as_str(), "two calls were in one filesystem at once");
        return false;
    }
    if !unmounted {
        report.say(gate.as_str(), "the gate's filesystem would not unmount");
        return false;
    }
    report.say(gate.as_str(), "two mounts at once: a call on the other went through while one here was held, and a \
                               second one here waited for it");
    true
}

/* ---- the whole test ---- */

pub fn run(dir: &str, big_size: usize, out: Option<&mut Output>) -> bool {
    let mut report = Reporter { out };

    /* The size is whoever asked's -- the shell's, a module's -- and the
     * big file's patch and cut are worked out from it. */
    if big_size < MIN_BIG_SIZE {
        report.say(dir, "a big file under the smallest the test takes");
        return false;
    }

    let vfs = match vfs_instance() {
        Some(vfs) => vfs,
        None => { report.say(dir, "no filesystem layer"); return false; }
    };

    match stat_of(vfs, dir) {
        Some(st) if st.kind == Kind::Dir => {}
        _ => { report.say(dir, "not a directory"); return false; }
    }

    let base = match Path::join(dir, "fstest.tmp") { Some(p) => p, None => {
        report.say(dir, "path too long"); return false; } };
    let paths = [
        Path::join(base.as_str(), "a.txt"),
        Path::join(base.as_str(), "b.txt"),
        Path::join(base.as_str(), "sub"),
        Path::join(base.as_str(), "big.bin"),
        Path::join(base.as_str(), "far.bin"),
        Path::join(base.as_str(), "image.bin"),
    ];
    let (a, b, sub, big, far, image) = match &paths {
        [Some(a), Some(b), Some(sub), Some(big), Some(far), Some(image)] => (a, b, sub, big, far, image),
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

    let ok = body(vfs, &base, a, b, sub, &c, big, far, image, big_size, &mut report);
    if !ok {
        vfs.remove(base.as_bytes());
    }
    ok
}

#[allow(clippy::too_many_arguments)]
fn body(vfs: &'static Vfs, base: &Path, a: &Path, b: &Path, sub: &Path, c: &Path, big: &Path, far: &Path,
        image: &Path, big_size: usize, report: &mut Reporter) -> bool {
    if !vfs.write_file(a.as_bytes(), b"hello world")
        || !check_content(vfs, a.as_str(), b"hello world", report)
    {
        report.say(a.as_str(), "write and read back failed");
        return false;
    }

    /* Append */
    {
        let file = match open(vfs, a.as_str(), OPEN_APPEND) {
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
        let file = match open(vfs, a.as_str(), OPEN_WRITE) {
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

    let named_c = vfs.read_dir(sub.as_bytes(), 0)
        .is_some_and(|entry| entry.name() == b"c.txt" && entry.size == 8);
    if !named_c || vfs.read_dir(sub.as_bytes(), 1).is_some() {
        report.say(sub.as_str(), "readdir is wrong");
        return false;
    }

    /* Refusals: a missing file, a directory as a file, a duplicate name */
    let refused = open(vfs, b.as_str(), OPEN_READ).is_none()
        && open(vfs, sub.as_str(), OPEN_READ).is_none()
        && !vfs.create(sub.as_bytes(), true)
        && !vfs.create(c.as_bytes(), false);
    if !refused {
        report.say(sub.as_str(), "an operation that should fail succeeded");
        return false;
    }

    if big_size > 0 && !big_file(vfs, big.as_str(), big_size, report) {
        return false;
    }

    if !far_file(vfs, far.as_str(), report) {
        return false;
    }

    if !image_file(vfs, image.as_str(), report) {
        return false;
    }

    if !two_mounts(vfs, base, report) {
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
