//! The file logs (`fs::logs`, docs/logs.md): lines written to named logs
//! from the world, from tasks of their own and from interrupt handlers --
//! through the image's `fs::logs::Log`, through the C ABI a module writes by
//! and through a module's `kcore::logs::Log` -- before the writer starts,
//! while it runs and after it stops, in bursts more than its queue holds; to
//! logs held to small limits, over a /logs that already has old files of
//! theirs, files of other names, things in the way, a file of theirs held
//! open -- or that is a file itself -- on a root that is writable, read-only
//! or not there. And what comes of it held to what was written:
//!
//! - each line in exactly one place: its log's files, the kernel log, or
//!   counted lost -- and what is counted of the log adds up to what was
//!   written to it;
//! - in a file in the order written, stamped with the wall clock -- read
//!   back by the judge's own calendar -- its text as it was handed over, a
//!   control character `?`, a line too long cut where a character begins
//!   when it was formatted and said to be cut;
//! - a boot said once, before its lines; lines lost said;
//! - a log's file within its limit after a line went into it, and all its
//!   files within theirs once it has moved along -- a file's over each while
//!   one was held open; its old files numbered from 1 on, the oldest the
//!   ones gone, the rest whole and in order; anything not a log's file as it
//!   was;
//! - a log that cannot be written said so once for each reason in a row,
//!   and a line for a log that was refused in the kernel log;
//! - `tail` the file's last lines, `logs sync` answered while the writer
//!   runs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use fs::logs::{self, Limits, Log};
use fs::vfs::{Handle, Vfs, OPEN_READ};
use fs::vnode::Kind;
use kcore::time::Duration;

use crate::machine::{self, cmd, sched};
use crate::{reached, Input};

const NAMES: [&str; 4] = ["net", "a", "b-1", "x_y"];
/// The image's logs and a module's, by those names.
static IMAGE: [Log; 4] = [Log::new("net"), Log::new("a"), Log::new("b-1"), Log::new("x_y")];
static MODULE: [kcore::logs::Log; 4] = [kcore::logs::Log::new("net"), kcore::logs::Log::new("a"),
                                        kcore::logs::Log::new("b-1"), kcore::logs::Log::new("x_y")];
/// Names no log has, as a module may ask for them.
const BAD: [&str; 7] = ["Net", "1x", "", "a/b", "abcdefghijklmnop", "a.b", "net log"];
static MODULE_BAD: kcore::logs::Log = kcore::logs::Log::new("Bad");

const LINE_MAX: usize = ffi::logs::LOG_LINE_MAX;
const CUT: &[u8] = b" [cut]";
const DIR: &str = "/logs";
/// Lines an input writes at most, its floods among them.
const LINES: u64 = 1500;
/// What the fuzz machine's boot clock says as an input starts, in seconds.
const BOOTED_SECS: u64 = sched::START / 1_000_000_000;
const BOOT_LINE: &[u8] = b"--- a boot: nos fuzz (fs-fuzz) ---";

/* The writer's timing as docs/logs.md says it: lines gathered a second once
 * the first wakes it, cut short once half the queue's 512 wait -- though to
 * no less than 200 ms -- and at once by `logs sync`. Held on the machine's
 * clock, with room either side of each edge. */
/// A sync answered within this, the gather cut short.
const SYNC_PROMPT_MS: u64 = 100;
/// Half the queue: a burst of this many and more cuts the gather short.
const HALF_QUEUE: u64 = 256;
/// A burst's most here: the queue's room, empty, for every line of it.
const BURST_MAX: u64 = 500;
/// A burst not written yet this long after it came, and written by then.
const BURST_NOT_YET_MS: u64 = 100;
const BURST_BY_MS: u64 = 400;
/// A trickle -- fewer lines than half the queue -- the same.
const TRICKLE_NOT_YET_MS: u64 = 500;
const TRICKLE_BY_MS: u64 = 1_500;

/// What is counted of the log called `name`: lines queued, and lines done
/// with -- written, sent to the kernel log, or lost.
fn counted(name: &str) -> (u64, u64) {
    let s = logs::stats(name).expect("a log made at the start");
    (s.queued, s.written + s.spilled + s.lost)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Root {
    Writable,
    ReadOnly,
    None,
}

/// What a line was made from: bytes, handed over as they are, or text,
/// formatted -- of characters of more than a byte, which a cut does not
/// split.
#[derive(Clone)]
enum Body {
    Bytes(Vec<u8>),
    Text(String),
}

/// How a line is handed over.
#[derive(Clone, Copy, Debug)]
enum Via {
    Image,
    Module,
    Abi,
}

struct Sent {
    log: usize,
    /// What its file says after the stamp
    text: Vec<u8>,
    /// Written once the writer had stopped: the kernel log's at once
    after_stop: bool,
}

#[derive(Default)]
struct Model {
    next: u64,
    sent: BTreeMap<u64, Sent>,
    /// Lines handed to a log that was refused
    refused: BTreeSet<u64>,
    stopped: bool,
}

type Shared = Arc<Mutex<Model>>;

fn model(m: &Shared) -> MutexGuard<'_, Model> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The text a line's file has after its stamp: as much of it as a line
/// carries -- whole characters of one formatted -- each control character
/// `?`, and its cut said.
fn expected(raw: &[u8], formatted: bool) -> Vec<u8> {
    let mut n = raw.len().min(LINE_MAX);
    if formatted {
        let s = std::str::from_utf8(raw).expect("a formatted line is text");
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
    }
    let mut text: Vec<u8> = raw[..n].iter().map(|&b| if (b < b' ' && b != b'\t') || b == 0x7F { b'?' } else { b })
        .collect();
    if n < raw.len() {
        text.extend_from_slice(CUT);
    }
    text
}

fn body(r: &mut Input) -> Body {
    let n = match r.u8() % 10 {
        0 => r.range(LINE_MAX as u64 - 30, LINE_MAX as u64 + 40) as usize,
        1 => 0,
        _ => r.below(60) as usize,
    };
    if r.chance(48) {
        let chars = ['z', 'é', '日', 'ж', '\u{1F600}'];
        Body::Text((0..n).map(|_| chars[(r.u8() % 5) as usize]).collect())
    } else {
        let mut bytes: Vec<u8> = (0..n).map(|_| b'a' + r.u8() % 26).collect();
        if !bytes.is_empty() && r.chance(64) {
            let at = r.below(bytes.len() as u64) as usize;
            bytes[at] = r.pick(&[b'\n', b'\r', 0, 0x1B, 0x7F, b'\t', 0xFF, 0xC3, 0x80]);
        }
        Body::Bytes(bytes)
    }
}

/// A line's number taken and what its file should say written down -- the
/// two with nothing between them and the line's hand-over that could let
/// another task run, so that the numbers are the queue's order. Whether it
/// is formatted -- text, handed over by a `Log`'s `line`, which cuts it
/// where a character begins -- or handed over as bytes, cut where they are.
fn prepare(m: &Shared, log: Option<usize>, body: &Body, via: Via) -> (Vec<u8>, bool) {
    let mut g = model(m);
    let seq = g.next;
    g.next += 1;
    let mut raw = format!("#{}:", seq).into_bytes();
    let formatted = match body {
        Body::Bytes(b) => {
            raw.extend_from_slice(b);
            false
        }
        Body::Text(s) => {
            raw.extend_from_slice(s.as_bytes());
            !matches!(via, Via::Abi)
        }
    };
    match log {
        Some(log) => {
            let after_stop = g.stopped;
            g.sent.insert(seq, Sent { log, text: expected(&raw, formatted), after_stop });
        }
        None => {
            g.refused.insert(seq);
        }
    }
    (raw, formatted)
}

/// The line handed over: nothing allocated, so from an interrupt handler
/// too.
fn hand(via: Via, log: usize, abi: u64, raw: &[u8], formatted: bool) {
    let text = if formatted { std::str::from_utf8(raw).ok() } else { None };
    match (via, text) {
        (Via::Image, Some(t)) => IMAGE[log].line(format_args!("{}", t)),
        (Via::Image, None) => IMAGE[log].write(raw, false),
        (Via::Module, Some(t)) => MODULE[log].line(format_args!("{}", t)),
        (Via::Module, None) => MODULE[log].write(raw, false),
        // SAFETY: `raw` is `raw.len()` readable bytes.
        (Via::Abi, _) => unsafe { logs::kernel_log_write(abi, raw.as_ptr(), raw.len(), 0) },
    }
}

/// A line written where the input says, from the world -- or from an
/// interrupt handler -- by whichever way it says.
fn write(r: &mut Input, m: &Shared, abi: &[u64; 4], irq: bool) {
    let log = r.below(4) as usize;
    let via = [Via::Image, Via::Module, Via::Abi][r.below(3) as usize];
    let b = body(r);
    let (raw, formatted) = prepare(m, Some(log), &b, via);
    if irq {
        sched::interrupt(|| hand(via, log, abi[log], &raw, formatted));
    } else {
        hand(via, log, abi[log], &raw, formatted);
    }
}

/// `n` lines a file had before the boot, each `width` long: the judge's
/// own memory.
fn lines_of(tag: &str, name: &str, n: u64, width: usize) -> Vec<u8> {
    sched::harness(|| {
        let mut out = Vec::new();
        for k in 0..n {
            let mut line = format!("{} {} {}", tag, name, k);
            while line.len() < width {
                line.push('.');
            }
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out
    })
}

/// What a file had before the boot is at most: up to twice a small log's
/// limit, a little of a big one's.
const BEFORE_MAX: u64 = 64 * 1024;

/// What /logs held before the writer: the files put there, by name, and
/// the directories.
#[derive(Default)]
struct Before {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    /// /logs itself is a file
    not_dir: bool,
}

fn put(vfs: &Vfs, name: &str, data: &[u8], before: &mut Before) {
    let path = format!("{}/{}", DIR, name);
    assert!(vfs.write_file(path.as_bytes(), data), "the judge's own {} could not be written", path);
    before.files.insert(name.to_string(), data.to_vec());
}

fn prepare_dir(r: &mut Input, vfs: &Vfs, limits: &[Limits; 4], before: &mut Before) {
    match r.u8() % 8 {
        0 => {
            assert!(vfs.write_file(DIR.as_bytes(), b"not a directory\n"));
            before.not_dir = true;
            return;
        }
        1..=3 => return,
        _ => {}
    }
    assert!(vfs.create(DIR.as_bytes(), true));
    for (log, name) in NAMES.iter().enumerate() {
        let file = limits[log].file.min(BEFORE_MAX / 2);
        let width = r.range(10, 120) as usize;
        if r.bool() {
            /* What the last boot left in its file: up to twice the file's
             * limit, its last line now and then without its end */
            let n = r.below((2 * file / (width as u64 + 1)).max(1));
            let mut data = lines_of("prior", name, n, width);
            if !data.is_empty() && r.chance(32) {
                data.pop();
            }
            put(vfs, &format!("{}.log", name), &data, before);
        }
        for _ in 0..r.below(4) {
            let n = r.pick(&[1, 2, 3, 4, 7, 98, 99, 100, 150]);
            let lines = r.below((2 * file / (width as u64 + 1)).max(1));
            put(vfs, &format!("{}.log.{}", name, n), &lines_of(&format!("old{}", n), name, lines, width), before);
        }
        if r.chance(24) {
            /* Names a log's old files do not have, and must keep */
            let junk = r.pick(&["log.0", "log.01", "log.1a", "logs", "log.-1", "log.", "log.1.bak"]);
            put(vfs, &format!("{}.{}", name, junk), b"not a log's\n", before);
        }
        if r.chance(8) {
            /* In the way: a directory by an old file's name, or the file's */
            let dir = if r.bool() { format!("{}.log.{}", name, r.pick(&[1, 2, 5])) } else { format!("{}.log", name) };
            if !before.files.contains_key(&dir) && vfs.create(format!("{}/{}", DIR, dir).as_bytes(), true) {
                before.dirs.insert(dir);
            }
        }
    }
    if r.chance(32) {
        put(vfs, "other.txt", b"somebody else's\n", before);
    }
}

/// Limits a log is held to in an input: small, so that it moves along.
fn limits(r: &mut Input) -> Limits {
    if r.chance(24) {
        return Limits::DEFAULT;
    }
    let file = r.pick(&[4096, 4096, 5000, 8192, 16384]);
    Limits { file, total: file * r.range(2, 6) }
}

pub fn logs(r: &mut Input) {
    machine::keep_logged();
    let vfs = fs::vfs_instance().expect("the VFS");
    let no_wall = r.chance(16);
    if no_wall {
        sched::set_wall_base(0);
    }

    let root = match r.u8() % 24 {
        0 => Root::None,
        1 => Root::ReadOnly,
        _ => Root::Writable,
    };
    match root {
        Root::Writable => assert!(fs::ramfs::mount_at("/", false)),
        Root::ReadOnly => assert!(fs::ramfs::mount_at("/", true)),
        Root::None => {}
    }

    let mut lim = [Limits::DEFAULT; 4];
    for l in lim.iter_mut() {
        *l = limits(r);
    }
    let mut before = Before::default();
    if root == Root::Writable {
        prepare_dir(r, vfs, &lim, &mut before);
    }

    /* The logs made, each with its limits -- by the image's call or a
     * module's -- after a refusal or two of what no log may be */
    for (log, name) in NAMES.iter().enumerate() {
        if r.chance(32) {
            // SAFETY: the name is its length's readable bytes.
            let h = unsafe { logs::kernel_log_open(name.as_ptr(), name.len(), 4096, 4096) };
            assert!(h == 0, "a log made with limits that hold no old file");
        }
        let made = if r.bool() {
            logs::open(name.as_bytes(), lim[log]).is_ok()
        } else {
            // SAFETY: as above.
            unsafe { logs::kernel_log_open(name.as_ptr(), name.len(), lim[log].file, lim[log].total) != 0 }
        };
        assert!(made, "log {} with limits {:?} refused", name, lim[log]);
    }
    let mut abi = [0u64; 4];
    for (log, name) in NAMES.iter().enumerate() {
        // SAFETY: as above.
        abi[log] = unsafe { logs::kernel_log_open(name.as_ptr(), name.len(), 0, 0) };
        assert!(abi[log] == log as u64 + 1, "log {} opened again as {}", name, abi[log]);
    }
    for bad in BAD {
        // SAFETY: as above.
        let h = unsafe { logs::kernel_log_open(bad.as_ptr(), bad.len(), 0, 0) };
        assert!(h == 0, "a log called {:?} made", bad);
    }
    assert!(logs::open(b"net", Limits { file: 4096, total: 1 << 40 }).is_err(), "limits past the most taken");

    let m: Shared = Arc::new(Mutex::new(Model::default()));

    /* Each way a line goes, once, before anything else runs: the image's
     * and a module's handle are made at the first line, and a lock taken
     * there lets another task run -- after this, the numbers are the
     * queue's order */
    for log in 0..4 {
        for via in [Via::Image, Via::Module] {
            let (raw, formatted) = prepare(&m, Some(log), &Body::Text("warm".to_string()), via);
            hand(via, log, abi[log], &raw, formatted);
        }
    }
    {
        let (raw, _) = prepare(&m, None, &Body::Bytes(b"to a log refused".to_vec()), Via::Module);
        MODULE_BAD.write(&raw, false);
        let (raw, _) = prepare(&m, None, &Body::Bytes(b"to no log".to_vec()), Via::Abi);
        // SAFETY: as above.
        unsafe { logs::kernel_log_write(99, raw.as_ptr(), raw.len(), 0) };
    }

    /* The boot: lines before the writer, then the writer -- now and then
     * none, and every line to the kernel log */
    for _ in 0..r.below(8) {
        let irq = r.chance(64);
        write(r, &m, &abi, irq);
    }
    let started = !r.chance(12) && logs::start();
    if started {
        reached("the writer started");
    }

    let mut tasks = Vec::new();
    let mut held: Vec<Handle> = Vec::new();
    let mut held_ever = [false; 4];
    while let Some(op) = r.op(13) {
        if model(&m).next >= LINES {
            break;
        }
        match op {
            0..=3 => write(r, &m, &abi, false),
            4 => write(r, &m, &abi, true),
            5 => {
                /* A task of its own writing a burst, yielding now and then */
                let count = r.range(1, 40);
                let log = r.below(4) as usize;
                let via = [Via::Image, Via::Module, Via::Abi][r.below(3) as usize];
                let bodies: Vec<Body> = (0..count).map(|_| body(r)).collect();
                let (m2, h) = (m.clone(), abi[log]);
                let id = sched::spawn("logger", sched::Kind::Kernel, machine::next_cpu(), Box::new(move || {
                    for (k, b) in bodies.iter().enumerate() {
                        let (raw, formatted) = prepare(&m2, Some(log), b, via);
                        hand(via, log, h, &raw, formatted);
                        if k % 3 == 0 {
                            kcore::task::yield_to_runnable();
                        }
                    }
                }));
                tasks.push(id);
            }
            6 if r.chance(64) => {
                /* More than the queue holds, all at once */
                let log = r.below(4) as usize;
                for _ in 0..r.range(500, 620) {
                    let (raw, _) = prepare(&m, Some(log), &Body::Bytes(b"flood".to_vec()), Via::Image);
                    IMAGE[log].write(&raw, false);
                }
                reached("a flood");
            }
            7 => {
                let asked = kcore::time::boot_time_ns();
                let synced = logs::sync(Duration::from_millis(5_000));
                let took_ms = (kcore::time::boot_time_ns() - asked) / 1_000_000;
                assert!(synced == started, "logs sync {} with the writer {}", if synced { "answered" } else {
                    "not answered" }, if started { "running" } else { "never started" });
                assert!(!started || took_ms < SYNC_PROMPT_MS, "logs sync answered {} ms on: the writer's gather \
                        not cut short", took_ms);
            }
            8 => kcore::task::sleep_ms(r.pick(&[0, 1, 50, 250, 1_000, 12_000, 70_000])),
            9 => {
                /* A log's file held open -- what a reader does -- or let go */
                if held.len() < 2 && r.bool() {
                    let log = r.below(4) as usize;
                    let path = if r.bool() { format!("{}/{}.log", DIR, NAMES[log]) } else {
                        format!("{}/{}.log.1", DIR, NAMES[log]) };
                    if let Some(h) = vfs.open(path.as_bytes(), OPEN_READ) {
                        held.push(h);
                        held_ever[log] = true;
                        reached("a log's file held open");
                    }
                } else if !held.is_empty() {
                    vfs.close(held.remove(0));
                }
            }
            12 if started => {
                /* The gather's timing, on a quiet machine -- every task of
                 * the input's done, everything before written: a trickle is
                 * written a second after its first line, not before; a burst
                 * of half a queue and more 200 ms after it came, not before,
                 * and long before the second */
                for t in tasks.drain(..) {
                    sched::join(t);
                }
                assert!(logs::sync(Duration::from_millis(5_000)), "logs sync not answered on a quiet machine");
                let log = r.below(4) as usize;
                let burst = r.bool();
                let n = if burst { r.range(HALF_QUEUE, BURST_MAX) } else { r.range(1, 10) };
                let (queued_before, _) = counted(NAMES[log]);
                for _ in 0..n {
                    let (raw, _) = prepare(&m, Some(log), &Body::Bytes(b"gathered".to_vec()), Via::Image);
                    IMAGE[log].write(&raw, false);
                }
                let (queued, done) = counted(NAMES[log]);
                assert!(queued - queued_before == n, "{} of {} lines queued, the queue empty", queued - queued_before,
                        n);
                let (not_yet, by) = if burst { (BURST_NOT_YET_MS, BURST_BY_MS) } else {
                    (TRICKLE_NOT_YET_MS, TRICKLE_BY_MS) };
                kcore::task::sleep_ms(not_yet);
                let (_, done_early) = counted(NAMES[log]);
                assert!(done_early == done, "{} of {} lines written {} ms after they came: the gather {}",
                        done_early - done, n, not_yet, if burst { "of a burst not held to its 200 ms" } else {
                        "of a trickle shorter than its second" });
                kcore::task::sleep_ms(by - not_yet);
                let (_, done_late) = counted(NAMES[log]);
                assert!(done_late - done >= n, "{} of {} lines not written {} ms after they came: the gather {}",
                        n - (done_late - done).min(n), n, by, if burst { "of a burst not cut short" } else {
                        "longer than its second" });
                reached(if burst { "a burst written once 200 ms passed" } else { "a trickle written at the second" });
            }
            10 => {
                let out = cmd::run("logs");
                assert!(out.starts_with("log ") && out.contains(" state"), "logs said {:?}", out);
                for name in NAMES {
                    assert!(out.lines().any(|l| l.split_whitespace().next() == Some(name)), "logs left out {}: {:?}",
                            name, out);
                }
            }
            _ => {
                let out = cmd::run("logs sync");
                let want = if started { "logs: written" } else { "logs: no writer is running" };
                assert!(out.trim_end() == want, "logs sync said {:?}, where the writer {}", out,
                        if started { "runs" } else { "never started" });
            }
        }
    }
    for t in tasks {
        sched::join(t);
    }
    for h in held.drain(..) {
        vfs.close(h);
    }

    /* The halt: the writer told to finish, then lines with nobody to write
     * them -- the kernel log's, at once */
    logs::stop();
    model(&m).stopped = true;
    for _ in 0..r.below(4) {
        let irq = r.chance(64);
        write(r, &m, &abi, irq);
    }
    assert!(cmd::run("logs sync").trim_end() == "logs: no writer is running", "logs sync answered at the halt");

    judge(vfs, root, &lim, &before, &held_ever, &model(&m), no_wall);
}

/* ---- the judge ---- */

/// A stamp's time and its length: `YYYY-MM-DDTHH:MM:SS.uuuuuuZ`, read by a
/// calendar of the judge's own into seconds since 1970 -- or, with no wall
/// clock, `+secs.uuuuuu`, the boot clock's.
fn stamp(line: &[u8]) -> Option<(u64, u64, usize)> {
    let num = |s: &[u8]| -> Option<u64> {
        if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(s).ok()?.parse().ok()
    };
    if line.first() == Some(&b'+') {
        let end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
        let dot = line[..end].iter().position(|&b| b == b'.')?;
        if end - dot != 7 {
            return None;
        }
        return Some((num(&line[1..dot])?, num(&line[dot + 1..end])?, end));
    }
    if line.len() < 27 || line[4] != b'-' || line[7] != b'-' || line[10] != b'T' || line[13] != b':'
        || line[16] != b':' || line[19] != b'.' || line[26] != b'Z' {
        return None;
    }
    let (y, mo, d) = (num(&line[0..4])?, num(&line[5..7])?, num(&line[8..10])?);
    let (h, mi, s, us) = (num(&line[11..13])?, num(&line[14..16])?, num(&line[17..19])?, num(&line[20..26])?);
    let leap = |y: u64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    const DAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&mo) || d == 0 || d > DAYS[mo as usize - 1] + u64::from(mo == 2 && leap(y)) || h > 23
        || mi > 59 || s > 59 || y < 1970 {
        return None;
    }
    let mut days = (1970..y).map(|y| if leap(y) { 366 } else { 365 }).sum::<u64>();
    days += (1..mo).map(|m| DAYS[m as usize - 1] + u64::from(m == 2 && leap(y))).sum::<u64>();
    days += d - 1;
    Some((days * 86_400 + h * 3_600 + mi * 60 + s, us, 27))
}

/// A line of a log's files, as the judge reads it.
#[derive(Debug, PartialEq)]
enum Line {
    /// One the files had before: a `prior` or an `old` one
    Before(Vec<u8>),
    Boot,
    Lost(u64),
    /// One of this boot's: its number and its text after the stamp
    Sent(u64, Vec<u8>),
}

fn parse(name: &str, line: &[u8], no_wall: bool, end_secs: u64) -> Line {
    let shown = String::from_utf8_lossy(line);
    if line.starts_with(format!("prior {} ", name).as_bytes()) || line.starts_with(b"old") {
        return Line::Before(line.to_vec());
    }
    let Some((secs, us, len)) = stamp(line) else { panic!("invariant: a line of {}'s with no stamp: {:?}", name, shown) };
    assert!(line.get(len) == Some(&b' '), "invariant: a stamp not followed by its line: {:?}", shown);
    let rest = &line[len + 1..];
    let (lo, hi) = if no_wall { (BOOTED_SECS, end_secs + BOOTED_SECS) } else {
        (machine::WALL_BASE, machine::WALL_BASE + end_secs) };
    if rest == BOOT_LINE {
        /* The boot clock's zero, on the wall clock -- what every stamp is
         * reckoned from */
        let zero = if no_wall { 0 } else { machine::WALL_BASE - BOOTED_SECS };
        assert!(secs == zero && us == 0, "invariant: {}'s boot stamped {}.{:06}, where the boot was at {}", name, secs,
                us, zero);
        return Line::Boot;
    }
    assert!(us < 1_000_000 && secs >= lo && secs <= hi + 1, "invariant: {:?} of {}'s stamped {}.{:06}, outside the \
            run's {}..{}", shown, name, secs, us, lo, hi);
    let text = String::from_utf8_lossy(rest);
    if let Some(n) = text.strip_prefix("--- ").and_then(|t| t.strip_suffix(" lines lost here: the queue was full ---")) {
        return Line::Lost(n.parse().unwrap_or_else(|_| panic!("invariant: a count of lost lines {:?}", n)));
    }
    let Some(seq) = rest.strip_prefix(b"#").and_then(|t| {
        let colon = t.iter().position(|&b| b == b':')?;
        std::str::from_utf8(&t[..colon]).ok()?.parse::<u64>().ok()
    }) else {
        panic!("invariant: a line in {}'s files nobody wrote: {:?}", name, shown)
    };
    Line::Sent(seq, rest.to_vec())
}

/// A number written as the logs write an old file's: digits, from 1, no 0
/// before them.
fn old_number(text: &str) -> Option<u64> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn read(vfs: &Vfs, path: &str) -> Vec<u8> {
    let file = fs::vfs::Open::new(vfs, path.as_bytes(), OPEN_READ)
        .unwrap_or_else(|| panic!("invariant: {} is listed and cannot be opened", path));
    let mut out = Vec::new();
    let mut buf = vec![0u8; 16384];
    loop {
        match file.read(&mut buf) {
            Some(0) => break,
            Some(n) => sched::harness(|| out.extend_from_slice(&buf[..n])),
            None => panic!("invariant: {} cannot be read", path),
        }
    }
    out
}

/// What the kernel log says of the logs: the lines that went there instead
/// of a file -- by log, their number and what was said -- the lost ones
/// said, the refused ones', and each log's failures and recoveries in order.
#[derive(Default)]
struct Said {
    spilled: [BTreeMap<u64, Vec<u8>>; 4],
    lost: [u64; 4],
    refused: Vec<u64>,
    /// A failure's reason, or None for a recovery
    turns: [Vec<Option<String>>; 4],
}

fn said() -> Said {
    let mut s = Said::default();
    for (_, line) in machine::logged() {
        let text = String::from_utf8_lossy(&line).into_owned();
        let Some(at) = text.find("logs.rs,") else { continue };
        let Some(colon) = text[at..].find(": ") else { continue };
        let msg = text[at + colon + 2..].trim_end_matches('\n');
        if let Some(rest) = msg.strip_prefix("logs: a line for a log that was refused: #") {
            s.refused.push(rest.split(':').next().and_then(|n| n.parse().ok()).expect("a refused line's number"));
            continue;
        }
        for (log, name) in NAMES.iter().enumerate() {
            if let Some(rest) = msg.strip_prefix(&format!("logs: /logs/{}.log cannot be written -- ", name)) {
                s.turns[log].push(Some(rest.split(':').next().unwrap_or("").to_string()));
            } else if msg.starts_with(&format!("logs: /logs/{}.log written again", name)) {
                s.turns[log].push(None);
            } else if let Some(rest) = msg.strip_prefix(&format!("{}.log: ", name)) {
                if let Some(n) = rest.strip_suffix(" lines lost: the queue was full").and_then(|n| n.parse::<u64>().ok()) {
                    s.lost[log] += n;
                    continue;
                }
                /* A line staged for the file has its stamp; one that never
                 * got that far has none */
                let body = match stamp(rest.as_bytes()) {
                    Some((_, _, len)) => &rest[len + 1..],
                    None => rest,
                };
                let seq = body.strip_prefix('#').and_then(|b| b.split(':').next()).and_then(|n| n.parse().ok())
                    .unwrap_or_else(|| panic!("invariant: a line of {}'s in the kernel log nobody wrote: {:?}", name,
                                              msg));
                let dup = s.spilled[log].insert(seq, body.as_bytes().to_vec());
                assert!(dup.is_none(), "invariant: line {} of {}'s in the kernel log twice", seq, name);
            }
        }
    }
    s
}

/// A line's text as the kernel log shows it: the file's, a byte that is no
/// UTF-8's `?` -- as far as the kernel log's line holds it.
fn shown(text: &[u8]) -> String {
    let mut out = String::new();
    for chunk in text.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push('?');
        }
    }
    out
}

fn judge(vfs: &Vfs, root: Root, lim: &[Limits; 4], before: &Before, held_ever: &[bool; 4], m: &Model, no_wall: bool) {
    let end_secs = (sched::now() - sched::START) / 1_000_000_000 + 1;
    let said = said();

    /* What is in /logs now */
    let mut listed: BTreeMap<String, (Kind, u64)> = BTreeMap::new();
    if root == Root::Writable && !before.not_dir {
        let mut at = 0;
        while let Ok(Some(e)) = vfs.dir_entry(DIR.as_bytes(), at) {
            listed.insert(String::from_utf8_lossy(e.name()).into_owned(), (e.kind, e.size as u64));
            at += 1;
        }
    } else {
        let stat = vfs.stat(DIR.as_bytes());
        assert!(stat.is_none() || before.not_dir, "invariant: /logs made where it could not be written");
    }

    /* Anything that is no log's file as it was */
    for (name, data) in &before.files {
        let ours = NAMES.iter().any(|log| {
            name == &format!("{}.log", log) || name.strip_prefix(&format!("{}.log.", log)).and_then(old_number).is_some()
        });
        if !ours {
            assert!(read(vfs, &format!("{}/{}", DIR, name)) == *data, "invariant: {} changed, which is no log's", name);
        }
    }
    for dir in &before.dirs {
        assert!(matches!(listed.get(dir), Some((Kind::Dir, _))), "invariant: the directory {} gone", dir);
    }

    for (log, name) in NAMES.iter().enumerate() {
        let stats = logs::stats(name).expect("a log made");
        let limits = lim[log];
        let held = held_ever[log];

        /* Its files, the oldest first */
        let current = format!("{}.log", name);
        let mut old: BTreeMap<u64, String> = BTreeMap::new();
        for (file, (kind, _)) in &listed {
            if let Some(n) = file.strip_prefix(&format!("{}.log.", name)).and_then(old_number) {
                if *kind == Kind::File {
                    old.insert(n, file.clone());
                }
            }
        }
        let mut lines: Vec<Line> = Vec::new();
        let mut total = 0;
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for (_, file) in old.iter().rev() {
            files.push((file.clone(), read(vfs, &format!("{}/{}", DIR, file))));
        }
        if matches!(listed.get(&current), Some((Kind::File, _))) {
            let data = read(vfs, &format!("{}/{}", DIR, current));
            files.push((current.clone(), data));
        }
        /* The file a line of this boot's went into last: within its limit
         * -- twice it, while it was held open */
        let mut newest_written: Option<(String, u64)> = None;
        /* The judge's own memory, as much of it as the files are long */
        sched::harness(|| {
            for (file, data) in &files {
                total += data.len() as u64;
                for l in data.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
                    let line = parse(name, l, no_wall, end_secs);
                    if matches!(line, Line::Sent(..)) {
                        newest_written = Some((file.clone(), data.len() as u64));
                    }
                    lines.push(line);
                }
            }
        });
        if let Some((file, size)) = &newest_written {
            let most = limits.file * if held { 2 } else { 1 };
            assert!(*size <= most, "invariant: {} is {} bytes, held to {}", file, size, most);
        }
        /* A file past its limit -- held open, or left by a boot that held it
         * to more (or at it, and given the end of its last line) -- goes old
         * whole, and may take them all past theirs */
        let oversized = held || before.files.get(&current).is_some_and(|data| data.len() as u64 >= limits.file);

        /* Before the boot's lines, what the files had; after, never */
        let first_new = lines.iter().position(|l| !matches!(l, Line::Before(_))).unwrap_or(lines.len());
        assert!(lines[first_new..].iter().all(|l| !matches!(l, Line::Before(_))), "invariant: {}'s old lines after \
                this boot's", name);
        let boots = lines.iter().filter(|l| **l == Line::Boot).count();
        assert!(boots <= 1, "invariant: {}'s files say the boot {} times", name, boots);
        if boots == 1 {
            assert!(lines[first_new] == Line::Boot, "invariant: {}'s lines before the boot is said", name);
        }
        /* Said before the first of them, unless that went old and away */
        if stats.rotations == 0 && lines.iter().any(|l| matches!(l, Line::Sent(..))) {
            assert!(boots == 1, "invariant: {}'s lines of this boot in its file, the boot not said", name);
        }

        /* This boot's lines: in order, as written, each once */
        let mut in_files: BTreeSet<u64> = BTreeSet::new();
        let mut last = None;
        let mut lost_said = said.lost[log];
        for l in &lines {
            match l {
                Line::Sent(seq, text) => {
                    let sent = m.sent.get(seq).unwrap_or_else(|| panic!("invariant: line {} in {}'s files was never \
                                                                          written", seq, name));
                    assert!(sent.log == log && !sent.after_stop, "invariant: line {} in {}'s files: written to {} {}",
                            seq, name, NAMES[sent.log], if sent.after_stop { "after the halt" } else { "" });
                    assert!(*text == sent.text, "invariant: line {} in {}'s file says {:?}, written as {:?}", seq,
                            name, String::from_utf8_lossy(text), String::from_utf8_lossy(&sent.text));
                    assert!(last.is_none_or(|l| l < *seq), "invariant: line {} in {}'s files after line {:?}", seq,
                            name, last);
                    last = Some(*seq);
                    in_files.insert(*seq);
                }
                Line::Lost(n) => lost_said += n,
                _ => {}
            }
        }

        /* And every line somewhere: its files, the kernel log, or lost */
        let spilled = &said.spilled[log];
        for (seq, text) in spilled {
            let sent = m.sent.get(seq).unwrap_or_else(|| panic!("invariant: line {} of {}'s in the kernel log was \
                                                                  never written", seq, name));
            assert!(sent.log == log && !in_files.contains(seq), "invariant: line {} of {}'s in the kernel log and in \
                    a file", seq, name);
            /* As far as the kernel log's line holds it: a line it cut ends
             * in "...", after what may be half a character */
            let want = shown(&sent.text);
            let got = String::from_utf8_lossy(text);
            let got = if got == want { &got[..] } else { got.trim_end_matches("...").trim_end_matches('\u{FFFD}') };
            assert!(want.starts_with(got), "invariant: line {} of {}'s said in the kernel log as {:?}, written as {:?}",
                    seq, name, got, want);
        }
        let mine: Vec<(&u64, &Sent)> = m.sent.iter().filter(|(_, s)| s.log == log).collect();
        for (seq, s) in &mine {
            if s.after_stop {
                assert!(spilled.contains_key(seq), "invariant: line {} of {}'s, written after the halt, not in the \
                        kernel log", seq, name);
            }
        }
        let missing: Vec<u64> = mine.iter().filter(|(seq, _)| !in_files.contains(seq) && !spilled.contains_key(seq))
            .map(|(seq, _)| **seq).collect();
        assert!(stats.written + stats.spilled + stats.lost == mine.len() as u64, "invariant: {}: {} lines written, \
                {} to the kernel log and {} lost, of {}", name, stats.written, stats.spilled, stats.lost, mine.len());
        assert!(stats.spilled == spilled.len() as u64, "invariant: {}: {} lines counted to the kernel log, {} there",
                name, stats.spilled, spilled.len());
        assert!(missing.len() as u64 >= stats.lost, "invariant: {}: {} lines lost, {} missing", name, stats.lost,
                missing.len());
        if stats.rotations == 0 {
            assert!(missing.len() as u64 == stats.lost, "invariant: {}: never moved along, {} lines missing and {} \
                    lost: {:?}", name, missing.len(), stats.lost, &missing[..missing.len().min(8)]);
            assert!(stats.written == in_files.len() as u64, "invariant: {}: {} lines counted written, {} in its file",
                    name, stats.written, in_files.len());
            assert!(lost_said == stats.lost, "invariant: {}: {} lines lost, {} said", name, stats.lost, lost_said);
        } else {
            reached("a log moved along");
            let first = in_files.first().copied().unwrap_or(u64::MAX);
            let newer = missing.iter().filter(|&&seq| seq > first).count() as u64;
            assert!(newer <= stats.lost, "invariant: {}: {} lines newer than its oldest in a file missing, {} lost",
                    name, newer, stats.lost);
            /* Numbered from 1 -- unless a move along was stopped part way by
             * a file opened under it, which leaves them in order */
            assert!(held || old.keys().copied().eq(1..=old.len() as u64), "invariant: {}'s old files numbered {:?}",
                    name, old.keys().collect::<Vec<_>>());
            assert!(old.len() as u64 <= ffi::logs::LOG_OLD_MAX, "invariant: {} old files of {}'s", old.len(), name);
            /* As the last move along left them: the file that went old,
             * and as many older ones as fit beside it in all but a file's
             * room -- none, when it was past that itself */
            let sizes: Vec<u64> = old.values().map(|f| listed.get(f).map_or(0, |&(_, size)| size)).collect();
            let first = sizes.first().copied().unwrap_or(0);
            let old_total: u64 = sizes.iter().sum();
            let budget = limits.total - limits.file;
            assert!(old_total <= first.max(budget), "invariant: {}'s old files take {} bytes, past the {} left beside a \
                    file, and the {} of the one that went old last", name, old_total, budget, first);
            if !oversized {
                assert!(first <= limits.file && total <= limits.total, "invariant: {}'s files take {} bytes, held to \
                        {}; the one that went old last {}", name, total, limits.total, first);
            }
        }

        /* What the files had before: the newest of it kept, in its order */
        let (had, kept) = sched::harness(|| {
            let mut had: Vec<Vec<u8>> = Vec::new();
            let mut old_before: Vec<(u64, &Vec<u8>)> = before.files.iter()
                .filter_map(|(f, d)| f.strip_prefix(&format!("{}.log.", name)).and_then(old_number).map(|n| (n, d)))
                .collect();
            old_before.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
            for (_, data) in old_before {
                had.extend(data.split(|&b| b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec));
            }
            if let Some(data) = before.files.get(&current) {
                had.extend(data.split(|&b| b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec));
            }
            let kept: Vec<Vec<u8>> = lines[..first_new].iter().map(|l| match l {
                Line::Before(b) => b.clone(),
                _ => unreachable!(),
            }).collect();
            (had, kept)
        });
        if !had.ends_with(&kept) {
            let skip = had.len().saturating_sub(kept.len());
            let at = kept.iter().zip(&had[skip..]).position(|(k, h)| k != h).unwrap_or(0);
            panic!("invariant: {}'s files kept {} of the {} lines they had, not the newest: line {} of those kept is \
                    {:?}, where it was {:?}", name, kept.len(), had.len(), at,
                   kept.get(at).map(|l| String::from_utf8_lossy(l).into_owned()),
                   had.get(skip + at).map(|l| String::from_utf8_lossy(l).into_owned()));
        }
        if stats.rotations == 0 {
            assert!(kept == had, "invariant: {} never moved along, and kept {} of the {} lines its files had", name,
                    kept.len(), had.len());
        }

        /* A failure said once for each reason in a row */
        for pair in said.turns[log].windows(2) {
            assert!(pair[0] != pair[1] || pair[0].is_none(), "invariant: {}'s failure said twice in a row: {:?}", name,
                    pair[0]);
        }
        if !mine.iter().all(|(_, s)| s.after_stop) && stats.written == 0 && stats.spilled > 0 {
            let why = match (root, before.not_dir) {
                (Root::None, _) => Some("no filesystem holds /logs"),
                (Root::ReadOnly, _) => Some("its filesystem is read-only"),
                (_, true) => Some("/logs is not a directory"),
                _ => None,
            };
            if let (Some(why), Some(Some(first))) = (why, said.turns[log].first()) {
                assert!(first == why, "invariant: {} could not be written because {}, and said {:?}", name, why, first);
            }
        }

        /* `tail` is the file's last lines */
        if files.last().is_some_and(|(f, _)| *f == current) {
            let data = &files.last().expect("the current file").1;
            let want: Vec<&[u8]> = data.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
            let n = want.len().min(3);
            let mut expect = want[want.len() - n..].join(&b'\n');
            if n != 0 {
                expect.push(b'\n');
            }
            let out = cmd::run(&format!("tail {}/{} 3", DIR, current));
            assert!(out == String::from_utf8_lossy(&expect), "invariant: tail of {} said {:?}, its last lines {:?}",
                    current, out, String::from_utf8_lossy(&expect));
        }
    }

    /* A line for no log: in the kernel log, once */
    let mut refused = said.refused.clone();
    refused.sort_unstable();
    assert!(refused.iter().copied().eq(m.refused.iter().copied()), "invariant: the lines for no log said {:?}, \
            written {:?}", refused, m.refused);
}
