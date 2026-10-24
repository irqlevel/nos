//! A VM with a folder keeps its console there: `console.log`, appended a
//! second at a time by a task of its own (`hv/vmN/log`) from the console's
//! ring as the guest prints -- never by the guest's CPUs, whose exits would
//! otherwise wait on the disk -- byte for byte as the guest wrote it, control
//! sequences and all (`hv console` shows it cleaned). Between the guest's
//! lines, in their places, what became of the VM: a line from hv for every
//! start, boot, stop and end (`Shared::note`), with the time. Past
//! `MAX_BYTES` the file moves to `console.log.1`, the one before it gone, and
//! a new one begins.
//!
//! What the ring no longer has when the task gets to it -- a guest printing
//! more than the ring keeps in a second -- is lost to the log, and the log
//! says how much. A write that fails -- the disk full, the root read-only --
//! is said once to the kernel log, and the task goes on trying.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::task::TaskHandle;

use crate::vms::Shared;

/// The log's file in the VM's folder, and what is put after its name for
/// the one before it.
pub const FILE: &str = "console.log";
const OLD_SUFFIX: &str = ".1";
/// The longest name of a file this writes in the folder.
pub const LONGEST_NAME: usize = FILE.len() + OLD_SUFFIX.len();
/// How big a log grows before it moves aside.
const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// How often the task writes, and how often it looks for its end between.
const WRITE_MS: u64 = 1000;
const LOOK_MS: u64 = 100;
/// The most it takes off the ring at once: all of it.
const CHUNK: usize = crate::guest::CONSOLE_BYTES;
const SECS_PER_DAY: u64 = 86_400;

/// A VM's log being written, until `finish`.
pub struct Log {
    job: Arc<Job>,
    task: Option<TaskHandle>,
}

struct Job {
    shared: Arc<Shared>,
    id: u32,
    path: String,
    old: String,
    end: AtomicBool,
}

impl Log {
    /// The writer started for VM `id`, whose folder is `folder`: None when
    /// it cannot be -- out of memory, or no task -- which the VM runs
    /// without, said in the kernel log.
    pub fn start(shared: &Arc<Shared>, id: u32, folder: &str) -> Option<Log> {
        let mut path = String::new();
        let mut old = String::new();
        let mut name = String::new();
        if path.try_reserve(folder.len() + FILE.len() + 1).is_err()
            || old.try_reserve(folder.len() + FILE.len() + OLD_SUFFIX.len() + 1).is_err()
            || name.try_reserve(24).is_err()
        {
            kcore::trace!(0, "hv: vm {} keeps no console.log -- out of memory", id);
            return None;
        }
        let _ = write!(path, "{}/{}", folder, FILE);
        let _ = write!(old, "{}{}", path, OLD_SUFFIX);
        let _ = write!(name, "hv/vm{}/log", id);
        let job = Arc::new(Job { shared: shared.clone(), id, path, old, end: AtomicBool::new(false) });
        match kcore::task::spawn_with(&name, job.clone(), write_log) {
            Some(task) => Some(Log { job, task: Some(task) }),
            None => {
                kcore::trace!(0, "hv: vm {} keeps no console.log -- no task for it", id);
                None
            }
        }
    }

    /// What is left written, and the writer gone: once the VM's own task has
    /// ended, so that how it ended is in the log.
    pub fn finish(mut self) {
        self.job.end.store(true, Ordering::Release);
        /* Dropping the handle waits for the task. */
        drop(self.task.take());
    }
}

/// The writer's task: the console and the notes into the file, a second at a
/// time, until it is told to end -- and then what is left, and the end.
fn write_log(job: Arc<Job>) {
    let mut w = Writer { job: &job, at: 0, buf: Vec::new(), failed: false };
    if w.buf.try_reserve_exact(CHUNK).is_err() {
        kcore::trace!(0, "hv: vm {} keeps no console.log -- out of memory", job.id);
        return;
    }
    loop {
        let ending = job.end.load(Ordering::Acquire);
        /* The notes first, each after the console up to where it was made;
         * then the console up to now. A note made after the take is the next
         * round's, and comes after what the console has by then -- which is
         * where it was made, or after. */
        for (at, wall, text) in job.shared.take_notes() {
            w.console(at);
            w.note(wall, &text);
        }
        w.console(u64::MAX);
        if ending {
            w.note(kcore::time::wall_clock_secs(), "off the list");
            return;
        }
        for _ in 0..WRITE_MS / LOOK_MS {
            if job.end.load(Ordering::Acquire) {
                break;
            }
            kcore::task::sleep_ms(LOOK_MS);
        }
    }
}

struct Writer<'a> {
    job: &'a Job,
    /// How far into the console the file has got: the next byte to write.
    at: u64,
    buf: Vec<u8>,
    /// A write has failed, and the kernel log was told.
    failed: bool,
}

impl Writer<'_> {
    /// The console from where the file has got up to `upto` -- and no
    /// further than it had got when this began: a guest printing faster
    /// than the file is written to does not keep the task here.
    fn console(&mut self, upto: u64) {
        let upto = upto.min(self.job.shared.console_total());
        loop {
            self.buf.clear();
            let Some(start) = self.job.shared.console_for_log(self.at, upto, CHUNK, &mut self.buf) else {
                return;
            };
            if start > self.at {
                let mut line = String::new();
                if line.try_reserve(96).is_ok() {
                    let _ = write!(line, "\n--- hv: {} bytes of the console printed too fast for this log ---\n",
                                   start - self.at);
                    self.write(line.as_bytes());
                }
                self.at = start;
            }
            if self.buf.is_empty() {
                return;
            }
            let buf = core::mem::take(&mut self.buf);
            self.write(&buf);
            self.at += buf.len() as u64;
            self.buf = buf;
        }
    }

    /// A line of hv's own, with the time.
    fn note(&mut self, wall: u64, text: &str) {
        let mut line = String::new();
        if line.try_reserve(text.len() + 64).is_err() {
            return;
        }
        let (days, secs) = (wall / SECS_PER_DAY, wall % SECS_PER_DAY);
        let (y, m, d) = hv::civil(days);
        let _ = write!(line, "\n--- hv: {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC vm {} {} ---\n", y, m, d,
                       secs / 3600, secs / 60 % 60, secs % 60, self.job.id, text);
        self.write(line.as_bytes());
    }

    /// Onto the end of the file, which moves aside first when this would
    /// take it past its most.
    fn write(&mut self, data: &[u8]) {
        let job = self.job;
        if let Ok(size) = kcore::fs::size(&job.path) {
            if size.saturating_add(data.len() as u64) > MAX_BYTES {
                /* The one before goes -- there may be none -- and this one
                 * takes its name. When it cannot, the file grows on: a log
                 * too big beats a log lost. */
                let _ = kcore::fs::remove(&job.old);
                let _ = kcore::fs::rename(&job.path, &job.old);
            }
        }
        match kcore::fs::append(&job.path, data) {
            Ok(()) => self.failed = false,
            Err(_) if !self.failed => {
                self.failed = true;
                kcore::trace!(0, "hv: vm {}: {} is not being written -- what the guest prints meanwhile is not \
                                  in it", job.id, job.path);
            }
            Err(_) => {}
        }
    }
}
