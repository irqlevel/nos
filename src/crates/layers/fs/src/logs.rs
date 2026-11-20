//! The kernel's file logs: what a part of the kernel keeps for a person to
//! read later, each log in files of its own under `/logs` -- `net.log` is
//! the network's, TCP's connections and sshd's sessions -- and not in the
//! kernel log, whose ring, consoles and netconsole a port 22 knocked on all
//! day would fill with them (docs/logs.md).
//!
//! A LINE, `flog!(LOG, ..)`, is formatted where it is written, stamped with
//! the boot clock and put on one queue every log shares
//! (`kcore::static_ring::Mailbox`): nothing is locked, allocated or waited
//! for, so a line may be written from anywhere -- the receive path's soft
//! IRQ, under a spin lock, an interrupt handler. The writer, a task of its
//! own (`logs`), takes the lines off a burst at a time and appends each
//! log's to its file: a line is in its file a moment after it is written
//! (`logs sync` waits for that), and nobody who writes one ever waits for a
//! disk. A burst bigger than the queue loses lines, counted, and the file
//! says how many, about where they would have been.
//!
//! ROTATION. A log is held to two limits: the size of its file, and of all
//! its files together. A batch that would take `<name>.log` past the first
//! moves it to `<name>.log.1` first -- `.1` to `.2`, and so on -- and the
//! oldest go, as many as it takes for those left to fit the second with
//! room for a whole file beside them. A file someone has open cannot be
//! moved: the log goes on into it, to twice its limit at most, until it can.
//!
//! WHEN IT CANNOT be written -- the root read-only, the disk full, `/logs`
//! something else -- a log says so in the kernel log once, and its lines go
//! there instead until it can be again, which is tried every ten seconds: a
//! line kept out of the kernel log is never a line lost. So does a line
//! written once the writer has stopped.
//!
//! WHEN. The writer starts once the root is mounted (`rust_logs_start`); a
//! line written before then waits on the queue. It stops just before the
//! filesystems are unmounted at the halt (`rust_logs_stop`), what it holds
//! written first. A panic writes nothing: what is on the queue is lost with
//! the machine.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use ffi::logs::{LOG_CUT, LOG_FILE_DEFAULT, LOG_FILE_LIMIT, LOG_FILE_MIN, LOG_LINE_MAX, LOG_NAME_MAX, LOG_OLD_MAX,
                LOG_TOTAL_DEFAULT, LOG_TOTAL_LIMIT};
use kcore::cmd::Output;
use kcore::const_init::ConstInit;
use kcore::consts::{NS_PER_MS, NS_PER_SEC, NS_PER_US};
use kcore::cpu::PreemptOff;
use kcore::klog;
use kcore::logs::LineBuf;
use kcore::once::Once;
use kcore::static_ring::Mailbox;
use kcore::sync::{Event, IrqSpinLock, TryLock};
use kcore::task::{self, TaskHandle};
use kcore::time::{self, Duration};

use crate::paths::Path;
use crate::vfs::{Open, Vfs, OPEN_APPEND, OPEN_CREATE, OPEN_READ};
use crate::vnode::Kind;

/// Where every log's files are.
pub const DIR: &str = "/logs";
/// Logs at most: each a slot of the table, for good.
pub const LOGS_MAX: usize = 16;
/// Lines the queue holds for the writer, every log's together: a burst of
/// this many before it gets to them.
pub const QUEUE_LINES: usize = 512;
/// How long the writer lets a burst gather once its first line wakes it.
const GATHER_MS: u64 = 200;
/// How long a log that cannot be written waits before it is tried again,
/// its lines going to the kernel log meanwhile.
const RETRY_MS: u64 = 10_000;
/// How long the writer sleeps with nothing to wake it: a look round, in case
/// a wakeup was missed.
const IDLE_MS: u64 = 60_000;
/// How long `logs sync` waits for the writer, and how often it looks.
const SYNC_WAIT_MS: u64 = 5_000;
const SYNC_LOOK_MS: u64 = 10;
/// What a line's stamp takes at most -- a date of a year with twenty
/// digits -- and what else is formatted on the stack: a number, a name.
const STAMP_MAX: usize = 48;
/// What a log's own lines before a batch of its lines take at most: the
/// boot's and the lost ones'.
const HEAD_MAX: usize = 320;
/// What a line too long for the queue ends in, in its file.
const CUT_MARK: &[u8] = b" [cut]";
/// What ends a line, and what stands in for a control character in one: a
/// line is one line, whatever its writer put in it.
const NEWLINE: u8 = b'\n';
const CONTROL: u8 = b'?';
const DELETE: u8 = 0x7F;
/// What a log's staged lines keep of the memory they took, between rounds.
const KEEP_BYTES: usize = 16 * 1024;
/// A log's slot and a line's length, as a queued line carries them: a byte
/// each.
const _: () = assert!(LOGS_MAX <= u8::MAX as usize + 1 && LOG_LINE_MAX <= u8::MAX as usize);

/* ---- a log's name and limits ---- */

/// What a log is held to, in bytes: its file, and all its files together.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub file: u64,
    pub total: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits { file: LOG_FILE_DEFAULT, total: LOG_TOTAL_DEFAULT };

    /// Whether these are limits a log can be held to: a file of some dozens
    /// of lines at least, room for the file and one old one, no more than
    /// `LOG_OLD_MAX` old ones' worth.
    pub const fn valid(&self) -> bool {
        self.file >= LOG_FILE_MIN
            && self.file <= LOG_FILE_LIMIT
            && self.total >= 2 * self.file
            && self.total <= LOG_TOTAL_LIMIT
            && self.total <= self.file * (LOG_OLD_MAX + 1)
    }
}

/// Whether `name` names a log: a lowercase letter, then lowercase letters,
/// digits, '-' and '_', `LOG_NAME_MAX` bytes at most -- a file's name
/// before `.log` that is nothing else's.
pub const fn name_ok(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > LOG_NAME_MAX || !name[0].is_ascii_lowercase() {
        return false;
    }
    let mut at = 1;
    while at < name.len() {
        let b = name[at];
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            return false;
        }
        at += 1;
    }
    true
}

/// Why a log was not made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    Name,
    Limits,
    /// Every slot of the table is a log's
    Full,
}

impl Refused {
    fn says(self) -> &'static str {
        match self {
            Refused::Name => "not a log's name",
            Refused::Limits => "limits no log is held to",
            Refused::Full => "no room for another log",
        }
    }
}

/// A file log of the image's, by its name -- `Log::new("net")` in a
/// `static` -- made the first time a line is written to it and named by its
/// slot from then on. Every `Log` of one name, the image's and any module's
/// (`kcore::logs::Log`), writes the same files.
pub struct Log {
    name: &'static str,
    limits: Limits,
    /// Its slot and one; 0 until it is made, `REFUSED` when it was not
    slot: AtomicUsize,
}

/// What a `Log` keeps for a log that was refused: said once, and its lines
/// to the kernel log.
const REFUSED: usize = usize::MAX;

impl Log {
    /// The log `name`, with the default limits if it is the first to ask
    /// for it. A name that is no log's does not build.
    pub const fn new(name: &'static str) -> Log {
        Log::with_limits(name, Limits::DEFAULT)
    }

    /// The log `name`, held to `limits` if it is the first to ask for it.
    pub const fn with_limits(name: &'static str, limits: Limits) -> Log {
        assert!(name_ok(name.as_bytes()), "a log's name: a lowercase letter, then letters, digits, '-' and '_'");
        assert!(limits.valid(), "a log's limits: a file of 4 KiB to 64 MiB, all its files twice it to a hundred times");
        Log { name, limits, slot: AtomicUsize::new(0) }
    }

    /// A line formatted: what `flog!` calls.
    pub fn line(&self, args: fmt::Arguments) {
        let mut buf = LineBuf::new();
        let _ = buf.write_fmt(args);
        self.write(buf.bytes(), buf.cut());
    }

    /// A line as it is, `LOG_LINE_MAX` bytes of it at most: onto the queue,
    /// from anywhere.
    pub fn write(&self, line: &[u8], cut: bool) {
        let mut slot = self.slot.load(Ordering::Relaxed);
        if slot == 0 {
            /* Two at once both make it, and are answered the same slot */
            slot = match open(self.name.as_bytes(), self.limits) {
                Ok(index) => index + 1,
                Err(_) => REFUSED,
            };
            self.slot.store(slot, Ordering::Relaxed);
        }
        if slot == REFUSED {
            spill_lost(line);
            return;
        }
        send(slot - 1, line, cut);
    }
}

/* ---- the table of logs ---- */

#[derive(Clone, Copy)]
struct Entry {
    name: [u8; LOG_NAME_MAX],
    name_len: usize,
    limits: Limits,
}

impl Entry {
    fn name(&self) -> &str {
        /* What `name_ok` let through: ASCII */
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }

    /// `/logs/<name>.log`
    fn path(&self) -> Option<Path> {
        let mut path = Path::join(DIR, self.name())?;
        path.push(".log")?;
        Some(path)
    }

    /// `/logs/<name>.log.<n>`
    fn old_path(&self, n: u64) -> Option<Path> {
        let mut path = self.path()?;
        let mut number = Words::new();
        write!(number, ".{}", n).ok()?;
        path.push(number.as_str())?;
        Some(path)
    }
}

struct Table {
    entries: [Option<Entry>; LOGS_MAX],
}

static TABLE: IrqSpinLock<Table> = IrqSpinLock::new(Table { entries: [None; LOGS_MAX] });

/// Logs made: each at the next slot, and none ever taken out -- so a slot
/// below this is a log's for good, which the writer and the C ABI look up
/// without the table's lock.
static MADE: AtomicUsize = AtomicUsize::new(0);

/// The log `name`, made with `limits` if it is new: its slot. A name or
/// limits it refuses, and a table with no room, are said in the kernel log.
/// From anywhere: the table's lock keeps interrupts off for a few dozen
/// compares, and nothing is allocated.
pub fn open(name: &[u8], limits: Limits) -> Result<usize, Refused> {
    let made = make(name, limits);
    if let Err(why) = made {
        klog!(0, "logs: a log called {:?} refused: {}", text_of(&name[..name.len().min(LOG_NAME_MAX + 1)]),
              why.says());
    }
    made
}

fn make(name: &[u8], limits: Limits) -> Result<usize, Refused> {
    if !name_ok(name) {
        return Err(Refused::Name);
    }
    if !limits.valid() {
        return Err(Refused::Limits);
    }
    let mut table = TABLE.lock();
    let made = MADE.load(Ordering::Relaxed);
    for (index, entry) in table.entries[..made].iter().enumerate() {
        if entry.as_ref().is_some_and(|e| &e.name[..e.name_len] == name) {
            return Ok(index);
        }
    }
    let Some(free) = table.entries.get_mut(made) else { return Err(Refused::Full) };
    let mut entry = Entry { name: [0; LOG_NAME_MAX], name_len: name.len(), limits };
    entry.name[..name.len()].copy_from_slice(name);
    *free = Some(entry);
    /* Published after the entry: a slot below MADE is a whole one */
    MADE.store(made + 1, Ordering::Release);
    Ok(made)
}

/// The log at `index`, as it was made.
fn entry(index: usize) -> Option<Entry> {
    if index >= MADE.load(Ordering::Acquire) {
        return None;
    }
    TABLE.lock().entries.get(index).copied().flatten()
}

/* ---- what is counted of each ---- */

struct Counts {
    /// Lines handed to it and queued
    queued: AtomicU64,
    /// Lines the queue had no room for, not yet said in its file
    lost: AtomicU64,
    /// Lines the queue had no room for, since the boot
    lost_all: AtomicU64,
    /// Lines that went to the kernel log, its file not to be written
    spilled: AtomicU64,
    /// Lines in its file, and the bytes put there
    written: AtomicU64,
    bytes: AtomicU64,
    /// Lines cut to what a line carries
    cut: AtomicU64,
    /// Times its file was moved along
    rotations: AtomicU64,
    /// Why it is not being written (`Why`), 0 while it is
    failing: AtomicU8,
}

impl Counts {
    const fn new() -> Counts {
        Counts {
            queued: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            lost_all: AtomicU64::new(0),
            spilled: AtomicU64::new(0),
            written: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            cut: AtomicU64::new(0),
            rotations: AtomicU64::new(0),
            failing: AtomicU8::new(0),
        }
    }
}

static COUNTS: [Counts; LOGS_MAX] = [const { Counts::new() }; LOGS_MAX];

/// What is counted of a log since the boot: for `logs`, and for the gates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub queued: u64,
    pub lost: u64,
    pub spilled: u64,
    pub written: u64,
    pub bytes: u64,
    pub cut: u64,
    pub rotations: u64,
}

/// What is counted of the log called `name`; None for no such log.
pub fn stats(name: &str) -> Option<Stats> {
    let index = (0..MADE.load(Ordering::Acquire)).find(|&index| entry(index).is_some_and(|e| e.name() == name))?;
    let c = &COUNTS[index];
    Some(Stats {
        queued: c.queued.load(Ordering::Relaxed),
        lost: c.lost_all.load(Ordering::Relaxed),
        spilled: c.spilled.load(Ordering::Relaxed),
        written: c.written.load(Ordering::Relaxed),
        bytes: c.bytes.load(Ordering::Relaxed),
        cut: c.cut.load(Ordering::Relaxed),
        rotations: c.rotations.load(Ordering::Relaxed),
    })
}

/* ---- the queue ---- */

/// A line on its way to the writer.
#[derive(Clone, Copy)]
struct Queued {
    /// The boot clock as it was written, in ns
    at: u64,
    log: u8,
    len: u8,
    cut: bool,
    text: [u8; LOG_LINE_MAX],
}

impl ConstInit for Queued {
    const INIT: Queued = Queued { at: 0, log: 0, len: 0, cut: false, text: [0; LOG_LINE_MAX] };
}

static QUEUE: Mailbox<Queued, QUEUE_LINES> = Mailbox::new();

/* Where the writer is: */
/// lines wait on the queue for a writer to come;
const BEFORE: u8 = 0;
/// the writer takes them;
const RUNNING: u8 = 1;
/// the writer is told to finish, and writes what is queued;
const STOPPING: u8 = 2;
/// there is no writer, and a line goes to the kernel log.
const STOPPED: u8 = 3;
static STATE: AtomicU8 = AtomicU8::new(BEFORE);

/// What wakes the writer, and whether a line has asked it to since it last
/// looked: one signal a burst, not one a line.
static WAKE: Once<Event> = Once::new();
static PENDING: AtomicBool = AtomicBool::new(false);
/// The writer, for the halt to wait for.
static WRITER: TryLock<Option<TaskHandle>> = TryLock::new(None);
/// `logs sync`: asks numbered, and the last the writer has answered -- by a
/// round that began after it was asked.
static SYNC_ASKED: AtomicU64 = AtomicU64::new(0);
static SYNC_DONE: AtomicU64 = AtomicU64::new(0);

/// A line onto the queue of the log at slot `index`. Never waits.
fn send(index: usize, line: &[u8], cut: bool) {
    let Some(counts) = COUNTS.get(index) else { return };
    let len = line.len().min(LOG_LINE_MAX);
    let cut = cut || len < line.len();
    let line = &line[..len];
    if cut {
        counts.cut.fetch_add(1, Ordering::Relaxed);
    }
    if STATE.load(Ordering::Acquire) == STOPPED {
        spill(index, line, cut);
        return;
    }

    let at = time::boot_time_ns();
    /* Preemption off across the hand-over: the writer takes lines in order,
     * and would wait behind one whose sender was switched away half way */
    let held = PreemptOff::take();
    let queued = QUEUE.send(|q| {
        q.at = at;
        q.log = index as u8;
        q.len = len as u8;
        q.cut = cut;
        q.text[..len].copy_from_slice(line);
    });
    drop(held);
    if !queued {
        counts.lost.fetch_add(1, Ordering::Relaxed);
        counts.lost_all.fetch_add(1, Ordering::Relaxed);
        return;
    }
    counts.queued.fetch_add(1, Ordering::Relaxed);

    /* The writer may have stopped since the look above, and taken its last
     * look at the queue before this line was on it: then nobody else will.
     * The fence pairs with `finish`'s: one of the two sees the other. */
    fence(Ordering::SeqCst);
    if STATE.load(Ordering::Relaxed) == STOPPED {
        spill_queue();
        return;
    }
    if !PENDING.swap(true, Ordering::AcqRel) {
        if let Some(wake) = WAKE.get() {
            wake.signal();
        }
    }
}

/* ---- to the kernel log instead ---- */

/// A byte of a line as its file has it: a control character -- one of
/// ASCII's, a tab aside -- `?`, so that a line is one line and says nothing
/// to the terminal it is read on; anything else as it is.
fn controlled(b: u8) -> u8 {
    if (b < b' ' && b != b'\t') || b == DELETE { CONTROL } else { b }
}

/// The longest whole-character start of `text`: what is shown of bytes that
/// may not all be text, where they are known to be but for a cut.
fn text_of(text: &[u8]) -> &str {
    match core::str::from_utf8(text) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&text[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// `line` as a line of the kernel log has it: a control character `?`, as
/// in a file (`controlled`) -- a newline in it would start a line of the
/// kernel log's that nobody wrote -- and a byte that is no UTF-8's `?` too.
fn shown(line: &[u8]) -> Text<{ LOG_LINE_MAX + STAMP_MAX + 8 }> {
    let mut out = Text::new();
    for chunk in line.utf8_chunks() {
        for c in chunk.valid().chars() {
            let c = if c.is_ascii() { controlled(c as u8) as char } else { c };
            if out.write_char(c).is_err() {
                return out;
            }
        }
        for _ in chunk.invalid() {
            if out.write_char(CONTROL as char).is_err() {
                return out;
            }
        }
    }
    out
}

/// A line of the log at `index` to the kernel log, its file not to be had.
fn spill(index: usize, line: &[u8], cut: bool) {
    let name = entry(index);
    klog!(0, "{}.log: {}{}", name.as_ref().map_or("?", Entry::name), shown(line).as_str(),
          if cut { " [cut]" } else { "" });
    if let Some(counts) = COUNTS.get(index) {
        counts.spilled.fetch_add(1, Ordering::Relaxed);
    }
}

/// A line for a log that was refused, to the kernel log.
fn spill_lost(line: &[u8]) {
    klog!(0, "logs: a line for a log that was refused: {}", shown(line).as_str());
}

/// Whatever is on the queue, to the kernel log: there is no writer to take
/// it any more. From anywhere, as `send` is.
fn spill_queue() {
    while let Some(q) = QUEUE.receive(|q| *q) {
        spill(usize::from(q.log), &q.text[..usize::from(q.len)], q.cut);
    }
}

/* ---- the writer: start and stop ---- */

/// The writer started: once, when the root is mounted. False -- and every
/// line from now on to the kernel log, those queued so far with them -- when
/// it cannot be.
pub fn start() -> bool {
    if STATE.load(Ordering::Acquire) != BEFORE {
        return false;
    }
    let Some(mut writer) = WRITER.try_lock() else { return false };
    let Some(event) = Event::new() else {
        klog!(0, "logs: no event for the file logs' writer -- their lines go to the kernel log");
        finish();
        return false;
    };
    if WAKE.set(event).is_err() {
        finish();
        return false;
    }
    STATE.store(RUNNING, Ordering::Release);
    match task::spawn("logs", write_logs) {
        Some(handle) => {
            *writer = Some(handle);
            true
        }
        None => {
            klog!(0, "logs: no task for the file logs' writer -- their lines go to the kernel log");
            finish();
            false
        }
    }
}

/// The writer told to finish and waited for: what is queued is written, and
/// every line from now on goes to the kernel log. At the halt, before the
/// filesystems go.
pub fn stop() {
    let handle = WRITER.try_lock().and_then(|mut writer| writer.take());
    match handle {
        Some(handle) => {
            STATE.store(STOPPING, Ordering::Release);
            if let Some(wake) = WAKE.get() {
                wake.signal();
            }
            /* Dropping the handle waits for the task */
            drop(handle);
        }
        /* Never started: what waits on the queue has nobody to write it */
        None => finish(),
    }
}

/// No writer from here on: the queue's lines, and every line after, to the
/// kernel log -- and the lines lost that no file said, said there.
fn finish() {
    STATE.store(STOPPED, Ordering::Relaxed);
    /* Pairs with `send`'s: a line queued after this saw STOPPED, or is on
     * the queue for the look below */
    fence(Ordering::SeqCst);
    spill_queue();
    for (index, counts) in COUNTS.iter().enumerate().take(MADE.load(Ordering::Acquire)) {
        let lost = counts.lost.swap(0, Ordering::AcqRel);
        if lost != 0 {
            let name = entry(index);
            klog!(0, "{}.log: {} lines lost: the queue was full", name.as_ref().map_or("?", Entry::name), lost);
        }
    }
}

/// Every line written so far in its file -- or in the kernel log -- by the
/// time this returns, unless the writer takes longer than `wait`: whether it
/// did. False at once with no writer running. Task context: it sleeps.
pub fn sync(wait: Duration) -> bool {
    if STATE.load(Ordering::Acquire) != RUNNING {
        return false;
    }
    let ask = SYNC_ASKED.fetch_add(1, Ordering::AcqRel) + 1;
    if let Some(wake) = WAKE.get() {
        wake.signal();
    }
    let deadline = time::boot_time_ns().saturating_add(wait.as_nanos());
    loop {
        if SYNC_DONE.load(Ordering::Acquire) >= ask {
            return true;
        }
        if time::boot_time_ns() >= deadline || STATE.load(Ordering::Acquire) != RUNNING {
            return SYNC_DONE.load(Ordering::Acquire) >= ask;
        }
        task::sleep_ms(SYNC_LOOK_MS);
    }
}

/* ---- the writer's task ---- */

fn write_logs() {
    let Some(wake) = WAKE.get() else {
        finish();
        return;
    };
    let mut w = Writer::new();
    loop {
        let state = STATE.load(Ordering::Acquire);
        let asked = SYNC_ASKED.load(Ordering::Acquire);
        /* Cleared before the queue is looked at: a line queued after this
         * wakes the writer again, and one queued before is on the queue */
        PENDING.swap(false, Ordering::AcqRel);
        w.round();
        SYNC_DONE.fetch_max(asked, Ordering::AcqRel);
        if state != RUNNING {
            break;
        }

        let idle = if w.failing() { RETRY_MS } else { IDLE_MS };
        let woken = wake.wait_for(Duration::from_millis(idle));
        /* A burst gathers before it is written -- unless someone waits */
        if woken && STATE.load(Ordering::Acquire) == RUNNING && SYNC_ASKED.load(Ordering::Acquire) == asked {
            task::sleep_ms(GATHER_MS);
        }
    }
    finish();
}

/// Why a log's file is not being written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Why {
    NoRoot = 1,
    ReadOnly,
    NotDir,
    NoDir,
    NotFile,
    Write,
    Rotate,
    Busy,
}

impl Why {
    const ALL: [Why; 8] =
        [Why::NoRoot, Why::ReadOnly, Why::NotDir, Why::NoDir, Why::NotFile, Why::Write, Why::Rotate, Why::Busy];

    fn says(self) -> &'static str {
        match self {
            Why::NoRoot => "no filesystem holds /logs",
            Why::ReadOnly => "its filesystem is read-only",
            Why::NotDir => "/logs is not a directory",
            Why::NoDir => "/logs could not be made",
            Why::NotFile => "something that is not a file has its name",
            Why::Write => "a write failed: the filesystem full, or the disk",
            Why::Rotate => "its files could not be moved along",
            Why::Busy => "it is twice its limit, and open, so not moved along",
        }
    }

    fn of(code: u8) -> Option<Why> {
        Why::ALL.into_iter().find(|why| *why as u8 == code)
    }
}

/// A log as the writer has it: what it took off the queue for the file, and
/// how the file has gone.
#[derive(Default)]
struct Pending {
    /// Stamped lines, each with its newline
    text: Vec<u8>,
    lines: u64,
    /// The boot's line is in its file
    booted: bool,
    /// Its file's last line is to be looked at before the next is put
    /// after it: one a power cut or a failed write may have left without
    /// its end
    check_end: bool,
    /// Not being written, and why; tried again from `retry_at`
    failed: Option<Why>,
    retry_at: u64,
    /// Lines that went to the kernel log since it failed
    spilled: u64,
}

struct Writer {
    vfs: Option<&'static Vfs>,
    logs: [Pending; LOGS_MAX],
    /// The wall clock's second at the boot clock's zero -- 0 with no wall
    /// clock -- once read
    epoch: Option<u64>,
}

impl Writer {
    fn new() -> Writer {
        Writer {
            vfs: crate::vfs_instance(),
            logs: core::array::from_fn(|_| Pending { check_end: true, ..Pending::default() }),
            epoch: None,
        }
    }

    fn failing(&self) -> bool {
        self.logs.iter().any(|p| p.failed.is_some())
    }

    /// The queue taken -- as much as it held as this began, no more, so that
    /// a flood cannot keep the writer here -- and every log's lines written.
    fn round(&mut self) {
        for _ in 0..QUEUE_LINES {
            let Some(q) = QUEUE.receive(|q| *q) else { break };
            self.stage(&q);
        }
        let now = time::boot_time_ns();
        for index in 0..MADE.load(Ordering::Acquire) {
            if let Some(entry) = entry(index) {
                self.flush(index, &entry, now);
            }
        }
    }

    fn epoch(&mut self) -> u64 {
        *self.epoch.get_or_insert_with(boot_epoch)
    }

    /// A line put with its log's others: its stamp, its text -- a control
    /// character in it made `?` -- and its end.
    fn stage(&mut self, q: &Queued) {
        let index = usize::from(q.log);
        let epoch = self.epoch();
        let (Some(p), Some(counts)) = (self.logs.get_mut(index), COUNTS.get(index)) else { return };
        let text = &q.text[..usize::from(q.len)];
        if p.text.try_reserve(STAMP_MAX + 1 + text.len() + CUT_MARK.len() + 1).is_err() {
            counts.lost.fetch_add(1, Ordering::Relaxed);
            counts.lost_all.fetch_add(1, Ordering::Relaxed);
            return;
        }
        /* Within what was reserved: nothing below allocates */
        p.text.extend_from_slice(stamp(epoch, q.at).bytes());
        p.text.push(b' ');
        p.text.extend(text.iter().map(|&b| controlled(b)));
        if q.cut {
            p.text.extend_from_slice(CUT_MARK);
        }
        p.text.push(NEWLINE);
        p.lines += 1;
    }

    /// A log's lines into its file -- or, while it cannot be written, to the
    /// kernel log.
    fn flush(&mut self, index: usize, entry: &Entry, now: u64) {
        let epoch = self.epoch();
        let vfs = self.vfs;
        let counts = &COUNTS[index];
        let lost = counts.lost.swap(0, Ordering::AcqRel);
        let p = &mut self.logs[index];
        if p.text.is_empty() && lost == 0 {
            return;
        }

        /* What the file says before the lines: the boot, the first time
         * anything of this boot's goes in, and the lines lost */
        let mut head = Head::new();
        if !p.booted {
            let mut version = [0u8; 64];
            let len = kcore::procinfo::version(&mut version);
            let _ = write!(head, "{} --- a boot: {} ---\n", stamp(epoch, 0).as_str(), text_of(&version[..len]));
        }
        if lost != 0 {
            let _ = write!(head, "{} --- {} lines lost here: the queue was full ---\n", stamp(epoch, now).as_str(), lost);
        }

        let text = core::mem::take(&mut p.text);
        let held = p.failed.is_some() && now < p.retry_at;
        let result = match (held, vfs) {
            (true, _) => Err((p.failed.unwrap_or(Why::Write), Part::Head(0))),
            (false, None) => Err((Why::NoRoot, Part::Head(0))),
            (false, Some(vfs)) => match append(vfs, index, entry, &mut p.check_end, head.bytes()) {
                Ok(()) => append(vfs, index, entry, &mut p.check_end, &text).map_err(|(why, done)| (why, Part::Text(done))),
                Err((why, done)) => Err((why, Part::Head(done))),
            },
        };

        match result {
            Ok(()) => {
                p.booted = true;
                if p.failed.take().is_some() {
                    klog!(0, "logs: /logs/{}.log written again -- {} of its lines went to the kernel log meanwhile",
                          entry.name(), p.spilled);
                    p.spilled = 0;
                }
                counts.failing.store(0, Ordering::Relaxed);
                counts.written.fetch_add(p.lines, Ordering::Relaxed);
                counts.bytes.fetch_add((head.bytes().len() + text.len()) as u64, Ordering::Relaxed);
            }
            Err((why, part)) => {
                if !held {
                    if p.failed != Some(why) {
                        klog!(0, "logs: /logs/{}.log cannot be written -- {}: its lines go to the kernel log until \
                                  it can", entry.name(), why.says());
                    }
                    p.failed = Some(why);
                    p.retry_at = now.saturating_add(RETRY_MS * NS_PER_MS);
                    p.check_end = true;
                    counts.failing.store(why as u8, Ordering::Relaxed);
                }
                /* What did not get into the file, to the kernel log: the
                 * lines past what was written, the lost ones said */
                let (head_done, text_done) = match part {
                    Part::Head(done) => (done, 0),
                    Part::Text(done) => (head.bytes().len(), done),
                };
                if head_done < head.bytes().len() {
                    p.booted = p.booted || head_done != 0;
                    if lost != 0 {
                        klog!(0, "{}.log: {} lines lost: the queue was full", entry.name(), lost);
                    }
                } else {
                    p.booted = true;
                }
                let mut spilled = 0;
                for line in text[text_done..].split(|&b| b == NEWLINE).filter(|line| !line.is_empty()) {
                    klog!(0, "{}.log: {}", entry.name(), shown(line).as_str());
                    spilled += 1;
                }
                let written = text[..text_done].iter().filter(|&&b| b == NEWLINE).count() as u64;
                counts.written.fetch_add(written, Ordering::Relaxed);
                counts.bytes.fetch_add((head_done + text_done) as u64, Ordering::Relaxed);
                counts.spilled.fetch_add(spilled, Ordering::Relaxed);
                p.spilled += spilled;
            }
        }

        /* The memory kept for the next round, the lines gone */
        p.text = text;
        p.text.clear();
        p.lines = 0;
        if p.text.capacity() > KEEP_BYTES {
            p.text.shrink_to(KEEP_BYTES);
        }
    }
}

/// How far a failed flush got: into the head -- the boot's and the lost
/// lines' lines -- or into the text past it.
enum Part {
    Head(usize),
    Text(usize),
}

/// The wall clock's second at the boot clock's zero: the wall clock less the
/// boot clock, read where both are in one second. 0 with no wall clock.
fn boot_epoch() -> u64 {
    const TRIES: usize = 4;
    let mut wall = 0;
    let mut booted = 0;
    for _ in 0..TRIES {
        let before = time::boot_time_ns() / NS_PER_SEC;
        wall = time::wall_clock_secs();
        booted = time::boot_time_ns() / NS_PER_SEC;
        if wall == 0 || before == booted {
            break;
        }
    }
    if wall == 0 { 0 } else { wall.saturating_sub(booted) }
}

/// A line's stamp: the wall clock at boot clock `at_ns`, as UTC to the
/// microsecond -- `2026-11-20T09:13:20.214790Z` -- or, with no wall clock,
/// the boot clock's seconds: `+4000.214790`.
fn stamp(epoch: u64, at_ns: u64) -> Words {
    let (secs, us) = (at_ns / NS_PER_SEC, at_ns % NS_PER_SEC / NS_PER_US);
    let mut text = Words::new();
    let _ = if epoch == 0 {
        write!(text, "+{}.{:06}", secs, us)
    } else {
        let t = calendar::Utc::of(epoch.saturating_add(secs));
        write!(text, "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z", t.year, t.month, t.day, t.hour, t.minute, t.second,
               us)
    };
    text
}

/// A few words formatted on the stack -- a stamp, a name, a marker's line --
/// with nothing allocated: what does not fit is refused whole.
struct Text<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    fn new() -> Text<N> {
        Text { buf: [0; N], len: 0 }
    }

    fn as_str(&self) -> &str {
        /* Only whole strs go in */
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> Write for Text<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.len() > N - self.len {
            return Err(fmt::Error);
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

/// A stamp, a number, a name.
type Words = Text<STAMP_MAX>;
/// A log's own lines before a batch.
type Head = Text<HEAD_MAX>;

/* ---- the files ---- */

/// `data` -- whole lines -- onto the end of the log's file, which moves
/// along first whenever the next line would take it past its limit: Ok, or
/// why not and how much of `data` is in the file.
fn append(vfs: &Vfs, index: usize, entry: &Entry, check_end: &mut bool, data: &[u8]) -> Result<(), (Why, usize)> {
    if data.is_empty() {
        return Ok(());
    }
    ready(vfs).map_err(|why| (why, 0))?;
    let path = entry.path().ok_or((Why::NotFile, 0))?;
    let mut size = file_size(vfs, &path).map_err(|why| (why, 0))?;
    let file_max = entry.limits.file;

    if *check_end {
        /* A line a write or a power cut left without its end: ended, so the
         * next one starts a line of its own */
        if size != 0 && last_byte(vfs, &path) != Some(NEWLINE) {
            put(vfs, &path, &[NEWLINE]).map_err(|why| (why, 0))?;
            size += 1;
        }
        *check_end = false;
    }

    let mut done = 0;
    while done < data.len() {
        let rest = &data[done..];
        let mut take = whole_lines(rest, file_max.saturating_sub(size));
        if take == 0 && size == 0 {
            /* A line longer than a whole file: there is none (LOG_FILE_MIN) */
            take = first_line(rest);
        }
        if take == 0 {
            match rotate(vfs, index, entry, &path, size) {
                Ok(()) => {
                    size = 0;
                    continue;
                }
                /* A file held open goes on growing, to twice its limit */
                Err(Why::Busy) => {
                    take = whole_lines(rest, file_max.saturating_mul(2).saturating_sub(size));
                    if take == 0 {
                        return Err((Why::Busy, done));
                    }
                }
                Err(why) => return Err((why, done)),
            }
        }
        if let Err(why) = put(vfs, &path, &rest[..take]) {
            *check_end = true;
            return Err((why, done));
        }
        size += take as u64;
        done += take;
    }
    Ok(())
}

/// The longest start of `data` that is whole lines and no more than `room`
/// bytes.
fn whole_lines(data: &[u8], room: u64) -> usize {
    let room = usize::try_from(room).unwrap_or(usize::MAX);
    if data.len() <= room {
        return data.len();
    }
    data[..room].iter().rposition(|&b| b == NEWLINE).map_or(0, |at| at + 1)
}

/// `data`'s first line, its end with it.
fn first_line(data: &[u8]) -> usize {
    data.iter().position(|&b| b == NEWLINE).map_or(data.len(), |at| at + 1)
}

/// `/logs` there to write in: on a filesystem that is writable, a directory
/// -- made, the first time, if it is not there.
fn ready(vfs: &Vfs) -> Result<(), Why> {
    let Some(mount) = vfs.mount_info_of(DIR.as_bytes()) else { return Err(Why::NoRoot) };
    if mount.read_only {
        return Err(Why::ReadOnly);
    }
    match vfs.stat(DIR.as_bytes()) {
        Some(found) if found.kind == Kind::Dir => Ok(()),
        Some(_) => Err(Why::NotDir),
        None if vfs.create(DIR.as_bytes(), true) => Ok(()),
        None => Err(Why::NoDir),
    }
}

/// The file's size: 0 when it is not there yet.
fn file_size(vfs: &Vfs, path: &Path) -> Result<u64, Why> {
    match vfs.stat(path.as_bytes()) {
        None => Ok(0),
        Some(found) if found.kind == Kind::File => Ok(found.size as u64),
        Some(_) => Err(Why::NotFile),
    }
}

fn last_byte(vfs: &Vfs, path: &Path) -> Option<u8> {
    let file = Open::new(vfs, path.as_bytes(), OPEN_READ)?;
    let size = file.size();
    if size == 0 || !file.seek(size - 1) {
        return None;
    }
    let mut byte = [0u8; 1];
    match file.read(&mut byte) {
        Some(1) => Some(byte[0]),
        _ => None,
    }
}

/// `data` onto the end of the file, which is made if it is not there.
fn put(vfs: &Vfs, path: &Path, data: &[u8]) -> Result<(), Why> {
    let file = Open::new(vfs, path.as_bytes(), OPEN_APPEND | OPEN_CREATE).ok_or(Why::Write)?;
    if file.write(data) { Ok(()) } else { Err(Why::Write) }
}

/// The log's file moved along: `<name>.log` to `.1`, each old one to the
/// next number, and the oldest gone -- as many as it takes for the old ones
/// to fit its total less a whole file, and to be no more than
/// `LOG_OLD_MAX`. Busy, with nothing moved, when one of the files to move
/// or to go is open.
fn rotate(vfs: &Vfs, index: usize, entry: &Entry, path: &Path, current: u64) -> Result<(), Why> {
    let old = old_files(vfs, entry)?;

    /* The newest stay, as many as fit beside the file going old */
    let budget = entry.limits.total.saturating_sub(entry.limits.file);
    let mut used = current;
    let mut keep = 0;
    for &(_, size) in &old {
        if keep + 2 > LOG_OLD_MAX as usize || used.saturating_add(size) > budget {
            break;
        }
        used += size;
        keep += 1;
    }

    /* Every file this moves or removes, closed: or nothing moves */
    let open = |p: &Path| vfs.is_open(p.as_bytes()) == Some(true);
    if open(path) {
        return Err(Why::Busy);
    }
    for &(n, _) in &old {
        if entry.old_path(n).as_ref().is_some_and(open) {
            return Err(Why::Busy);
        }
    }

    /* Counted as it begins: one stopped part way has moved files too. What
     * it leaves is in order all the same -- the oldest gone, the rest each
     * where it was or at its new number -- and the next one numbers them
     * again from 1 */
    COUNTS[index].rotations.fetch_add(1, Ordering::Relaxed);
    for &(n, _) in &old[keep..] {
        let gone = entry.old_path(n).ok_or(Why::Rotate)?;
        if !vfs.remove_one(gone.as_bytes(), Kind::File) {
            return Err(refused(vfs, &gone));
        }
    }
    /* Each kept one to its number -- the newest `.2`, the next `.3`, and so
     * on -- in an order that has nothing in the way: those going down past
     * a gap first, the newest first, each to where a newer one has gone
     * from; then those going up one, the oldest first, each to where an
     * older one has gone from. (The oldest first throughout put `.98` onto
     * a `.3` that had not moved yet: the fuzzer's.) */
    for (k, &(n, _)) in old[..keep].iter().enumerate() {
        if n > k as u64 + 2 {
            move_old(vfs, entry, n, k as u64 + 2)?;
        }
    }
    for (k, &(n, _)) in old[..keep].iter().enumerate().rev() {
        if n < k as u64 + 2 {
            move_old(vfs, entry, n, k as u64 + 2)?;
        }
    }
    let first = entry.old_path(1).ok_or(Why::Rotate)?;
    if !vfs.rename(path.as_bytes(), first.as_bytes()) {
        return Err(refused(vfs, path));
    }
    Ok(())
}

/// The log's old file `.from` renamed `.to`.
fn move_old(vfs: &Vfs, entry: &Entry, from: u64, to: u64) -> Result<(), Why> {
    let (from, to) = (entry.old_path(from).ok_or(Why::Rotate)?, entry.old_path(to).ok_or(Why::Rotate)?);
    if vfs.rename(from.as_bytes(), to.as_bytes()) { Ok(()) } else { Err(refused(vfs, &from)) }
}

/// Why a file of the log's was not moved or removed: somebody opened it
/// after it was looked at -- a reader on another CPU, between the look and
/// the rename -- which is waited out as a file open at the look is (Busy);
/// or anything else.
fn refused(vfs: &Vfs, path: &Path) -> Why {
    if vfs.is_open(path.as_bytes()) == Some(true) { Why::Busy } else { Why::Rotate }
}

/// The log's old files there are -- `<name>.log.<n>`, `n` a number from 1
/// written as numbers are -- with their sizes, the newest first. Something
/// else by such a name is refused: it would be in the way.
fn old_files(vfs: &Vfs, entry: &Entry) -> Result<Vec<(u64, u64)>, Why> {
    let mut prefix = Words::new();
    write!(prefix, "{}.log.", entry.name()).map_err(|_| Why::NotFile)?;

    let mut old = Vec::new();
    for at in 0.. {
        let found = match vfs.dir_entry(DIR.as_bytes(), at) {
            Ok(Some(found)) => found,
            Ok(None) => break,
            Err(_) => return Err(Why::Rotate),
        };
        let Some(n) = found.name().strip_prefix(prefix.bytes()).and_then(number) else { continue };
        if found.kind != Kind::File {
            return Err(Why::NotFile);
        }
        if old.try_reserve(1).is_err() {
            return Err(Why::Rotate);
        }
        old.push((n, found.size as u64));
    }
    old.sort_unstable_by_key(|&(n, _)| n);
    Ok(old)
}

/// A number from 1, as a number is written: digits, no 0 before them.
fn number(text: &[u8]) -> Option<u64> {
    if text.is_empty() || text[0] == b'0' || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    text.iter().try_fold(0u64, |n, &d| n.checked_mul(10)?.checked_add(u64::from(d - b'0')))
}

/* ---- the C ABI a module writes by (ffi::logs, kcore::logs) ---- */

/// The log `name`, made with these limits -- 0 and 0 the defaults -- if it
/// is new: its slot and one, or 0, said.
///
/// # Safety
/// `name` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_log_open(name: *const u8, len: usize, file_max: u64, total_max: u64) -> u64 {
    if name.is_null() || len == 0 || len > LOG_NAME_MAX {
        klog!(0, "logs: a log refused: a name of {} bytes", len);
        return 0;
    }
    // SAFETY: the caller's, `len` bytes.
    let name = unsafe { core::slice::from_raw_parts(name, len) };
    let limits = match (file_max, total_max) {
        (0, 0) => Limits::DEFAULT,
        (file, total) => Limits { file, total },
    };
    open(name, limits).map_or(0, |index| index as u64 + 1)
}

/// A line onto the log's queue, `len` bytes of it at most `LOG_LINE_MAX`;
/// `cut` `LOG_CUT` when it was longer. A word that is no log's has its line
/// said in the kernel log.
///
/// # Safety
/// `line` points at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_log_write(log: u64, line: *const u8, len: usize, cut: u32) {
    let taken = len.min(LOG_LINE_MAX);
    let text = if line.is_null() || taken == 0 {
        &[][..]
    } else {
        // SAFETY: the caller's, `len` bytes, and no more than that is read.
        unsafe { core::slice::from_raw_parts(line, taken) }
    };
    let cut = cut & LOG_CUT != 0 || taken < len;
    let index = usize::try_from(log).ok().and_then(|log| log.checked_sub(1));
    match index.filter(|&index| index < MADE.load(Ordering::Acquire)) {
        Some(index) => send(index, text, cut),
        None => spill_lost(text),
    }
}

/// The writer started: once the root is mounted.
#[no_mangle]
pub extern "C" fn rust_logs_start() {
    start();
}

/// The writer finished and gone: at the halt, before the filesystems go.
#[no_mangle]
pub extern "C" fn rust_logs_stop() {
    stop();
}

/* ---- the shell: `logs` ---- */

pub fn command(args: &str, out: &mut Output) {
    let mut words = args.split_whitespace();
    match (words.next(), words.next()) {
        (None, _) => list(out),
        (Some("sync"), None) => {
            if STATE.load(Ordering::Acquire) != RUNNING {
                let _ = writeln!(out, "logs: no writer is running");
            } else if sync(Duration::from_millis(SYNC_WAIT_MS)) {
                let _ = writeln!(out, "logs: written");
            } else {
                let _ = writeln!(out, "logs: the writer has not answered in {} s", SYNC_WAIT_MS / 1000);
            }
        }
        _ => {
            let _ = writeln!(out, "usage: logs [sync]");
        }
    }
}

const LIST_COLUMNS: usize = 10;
const LIST_HEADER: [&str; LIST_COLUMNS] =
    ["log", "file", "size", "old", "all", "limits", "lines", "lost", "to dmesg", "state"];

/// Every log: its file and how big, its old files and all of them together,
/// its limits, the lines it has written, lost and sent to the kernel log
/// since the boot, and whether it is being written.
fn list(out: &mut Output) {
    let vfs = crate::vfs_instance();
    let mut rows: Vec<[String; LIST_COLUMNS]> = Vec::new();
    for (index, c) in COUNTS.iter().enumerate().take(MADE.load(Ordering::Acquire)) {
        let Some(entry) = entry(index) else { continue };
        let path = entry.path();
        let size = match (vfs, path.as_ref()) {
            (Some(vfs), Some(path)) => file_size(vfs, path).unwrap_or(0),
            _ => 0,
        };
        let old = vfs.and_then(|vfs| old_files(vfs, &entry).ok()).unwrap_or_default();
        let old_bytes: u64 = old.iter().map(|&(_, size)| size).sum();
        let state = match (STATE.load(Ordering::Acquire), Why::of(c.failing.load(Ordering::Relaxed))) {
            (BEFORE, _) => "waiting for the root",
            (STOPPING | STOPPED, _) => "stopped: to dmesg",
            (_, Some(why)) => why.says(),
            (_, None) => "written",
        };
        let human = crate::shell::human;
        rows.push([
            String::from(entry.name()),
            String::from(path.as_ref().map_or("?", Path::as_str)),
            human(size),
            alloc::format!("{}", old.len()),
            human(size.saturating_add(old_bytes)),
            alloc::format!("{}/{}", human(entry.limits.file), human(entry.limits.total)),
            alloc::format!("{}", c.written.load(Ordering::Relaxed)),
            alloc::format!("{}", c.lost_all.load(Ordering::Relaxed)),
            alloc::format!("{}", c.spilled.load(Ordering::Relaxed)),
            String::from(state),
        ]);
    }
    if rows.is_empty() {
        let _ = writeln!(out, "logs: none yet");
        return;
    }

    /* The name and the file to the left, the numbers to the right, each
       column as wide as its widest; the state as it is */
    let mut width = LIST_HEADER.map(str::len);
    for row in &rows {
        for (w, cell) in width.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.len());
        }
    }
    let header = LIST_HEADER.map(String::from);
    for row in core::iter::once(&header).chain(rows.iter()) {
        let mut line = String::new();
        for (i, (cell, w)) in row.iter().zip(width).enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let _ = if i < 2 || i == LIST_COLUMNS - 1 {
                write!(line, "{:<w$}", cell, w = w)
            } else {
                write!(line, "{:>w$}", cell, w = w)
            };
        }
        let _ = writeln!(out, "{}", line.trim_end());
    }
}
