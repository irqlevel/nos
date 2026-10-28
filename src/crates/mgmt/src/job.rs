//! Jobs: a method that takes long, called on a task of the bus's own
//! (`mgmt/job<N>`), so that whoever started it gets its number at once and
//! asks after it: its state, its progress, the lines it logs, its result.
//! The record outlives the job, and its provider -- a module unloaded --
//! and whoever started it.
//!
//! A job is told to stop (`cancelled`) by a cancel, by its provider's
//! unregister, which then waits for it, and by the halt; it is its method's
//! to look and stop. A job whose provider went before it started ends Gone.
//!
//! Locks: the table's, then a job's -- never the other way round, and never
//! the bus's while either is held.

use crate::{begin, run, CallState, Status};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};
use kcore::consts::{NS_PER_MS, NS_PER_SEC};
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use kcore::time::boot_time_ns;
use kcore::trace;

/// Jobs not done, at most: a fifth is refused (Busy).
pub const MAX_RUNNING: usize = 4;
/// Records kept, done ones the first to go.
pub const MAX_RECORDS: usize = 32;
/// A job's result, its parameters, a line of its log, its lines kept.
pub const RESULT_MAX: usize = 16 * 1024;
pub const PARAMS_MAX: usize = 4 * 1024;
pub const NOTE_MAX: usize = 200;
pub const NOTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
}

impl JobState {
    pub fn name(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
        }
    }
}

/// A job as asked after.
pub struct JobInfo {
    pub id: u64,
    pub provider: String,
    pub method: String,
    pub state: JobState,
    pub status: Status,
    pub done: u64,
    pub total: u64,
    pub queued_ms: u64,
    pub started_ms: u64,
    pub ended_ms: u64,
    /// The number the next line of its log will have.
    pub notes_next: u64,
}

pub(crate) struct Job {
    id: u64,
    provider: String,
    method: String,
    /// The registration it was started on: a provider of that name
    /// registered again is not it.
    handle: u64,
    params: Vec<u8>,
    cancel: AtomicBool,
    finished: AtomicBool,
    inner: Mutex<Inner>,
}

struct Inner {
    state: JobState,
    status: Status,
    done: u64,
    total: u64,
    notes: VecDeque<(u64, String)>,
    next_note: u64,
    result: Vec<u8>,
    queued_ms: u64,
    started_ms: u64,
    ended_ms: u64,
}

impl Job {
    pub(crate) fn progress(&self, done: u64, total: u64) {
        let mut i = self.inner.lock();
        i.done = done;
        i.total = total;
    }

    pub(crate) fn note(&self, line: &str) {
        let mut n = line.len().min(NOTE_MAX);
        while !line.is_char_boundary(n) {
            n -= 1;
        }
        let mut text = String::new();
        if text.try_reserve_exact(n).is_err() {
            return;
        }
        text.push_str(&line[..n]);
        let mut i = self.inner.lock();
        if i.notes.len() == NOTES {
            i.notes.pop_front();
        }
        if i.notes.try_reserve(1).is_ok() {
            let seq = i.next_note;
            i.notes.push_back((seq, text));
            i.next_note += 1;
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    fn started(&self) {
        let mut i = self.inner.lock();
        i.state = JobState::Running;
        i.started_ms = now_ms();
    }

    fn finish(&self, status: Status, result: Vec<u8>) {
        {
            let mut i = self.inner.lock();
            i.state = JobState::Done;
            i.status = status;
            i.result = result;
            i.ended_ms = now_ms();
        }
        self.finished.store(true, Ordering::Release);
    }

    fn info(&self) -> Option<JobInfo> {
        let provider = crate::copy(&self.provider).ok()?;
        let method = crate::copy(&self.method).ok()?;
        let i = self.inner.lock();
        Some(JobInfo {
            id: self.id,
            provider,
            method,
            state: i.state,
            status: i.status,
            done: i.done,
            total: i.total,
            queued_ms: i.queued_ms,
            started_ms: i.started_ms,
            ended_ms: i.ended_ms,
            notes_next: i.next_note,
        })
    }
}

fn now_ms() -> u64 {
    boot_time_ns() / NS_PER_MS
}

pub(crate) struct Jobs {
    records: Vec<Arc<Job>>,
    /// Each job's task, with its number: joined once it is done.
    tasks: Vec<(u64, TaskHandle)>,
    next: u64,
    /// The halt has taken the tasks: no job starts.
    stopped: bool,
}

static JOBS: OnceBox<Mutex<Jobs>> = OnceBox::new();

pub(crate) fn jobs() -> Option<&'static Mutex<Jobs>> {
    JOBS.get_or_try_init(|| {
        let mut records = Vec::new();
        records.try_reserve_exact(MAX_RECORDS).ok()?;
        /* A task for every record, so that keeping one never allocates */
        let mut tasks = Vec::new();
        tasks.try_reserve_exact(MAX_RECORDS).ok()?;
        Some(alloc::boxed::Box::new(Mutex::new(Jobs { records, tasks, next: 0, stopped: false })?))
    })
}

/// The job of `provider`'s `method` started with `params`: its number, or
/// why not -- no such method of the kind (NoProvider, NoMethod, Kind), too
/// many jobs not done (Busy), the halt begun (Gone).
pub fn job_start(provider: &str, method: &str, params: &[u8]) -> Result<u64, Status> {
    if params.len() > PARAMS_MAX {
        return Err(Status::TooLarge);
    }
    let handle = crate::job_target(provider, method)?;
    let table = jobs().ok_or(Status::Busy)?;
    let provider = crate::copy(provider)?;
    let method = crate::copy(method)?;
    let mut p = Vec::new();
    p.try_reserve_exact(params.len()).map_err(|_| Status::Busy)?;
    p.extend_from_slice(params);
    let inner = Mutex::new(Inner {
        state: JobState::Queued,
        status: Status::Ok,
        done: 0,
        total: 0,
        notes: VecDeque::new(),
        next_note: 0,
        result: Vec::new(),
        queued_ms: now_ms(),
        started_ms: 0,
        ended_ms: 0,
    })
    .ok_or(Status::Busy)?;

    /* What goes -- records evicted, tasks of jobs done -- is dropped out of
     * the lock: dropping a task's handle joins it */
    let mut evicted: Vec<Arc<Job>> = Vec::new();
    let mut reaped: Vec<(u64, TaskHandle)> = Vec::new();
    evicted.try_reserve_exact(MAX_RECORDS).map_err(|_| Status::Busy)?;
    reaped.try_reserve_exact(MAX_RECORDS).map_err(|_| Status::Busy)?;
    let job = {
        let mut j = table.lock();
        if j.stopped {
            return Err(Status::Gone);
        }
        if j.records.iter().filter(|r| !r.finished.load(Ordering::Acquire)).count() >= MAX_RUNNING {
            return Err(Status::Busy);
        }
        while j.records.len() >= MAX_RECORDS {
            let Some(old) = j.records.iter().position(|r| r.finished.load(Ordering::Acquire)) else {
                return Err(Status::Busy);
            };
            evicted.push(j.records.remove(old));
        }
        let mut i = 0;
        while i < j.tasks.len() {
            let id = j.tasks[i].0;
            let done = j.records.iter().chain(evicted.iter()).find(|r| r.id == id).is_none_or(|r| r.finished.load(Ordering::Acquire));
            if done {
                reaped.push(j.tasks.swap_remove(i));
            } else {
                i += 1;
            }
        }
        j.next += 1;
        let job = Arc::new(Job {
            id: j.next,
            provider,
            method,
            handle,
            params: p,
            cancel: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            inner,
        });
        /* Room taken at init: MAX_RECORDS, and one went if it was full */
        j.records.push(job.clone());
        job
    };
    drop(evicted);
    drop(reaped);

    let mut name = String::new();
    let _ = name.try_reserve(24);
    let _ = write!(name, "mgmt/job{}", job.id);
    match kcore::task::spawn_with(&name, job.clone(), run_job) {
        Some(task) => {
            let mut j = table.lock();
            if j.tasks.len() < j.tasks.capacity() {
                j.tasks.push((job.id, task));
            } else {
                /* Never: there are no more tasks than records. Joined, out
                 * of the lock, rather than lost. */
                drop(j);
                drop(task);
            }
        }
        None => job.finish(Status::Busy, Vec::new()),
    }
    Ok(job.id)
}

/// A job's task: the method called, the result kept.
fn run_job(job: Arc<Job>) {
    let Some(state) = CallState::new(RESULT_MAX, Some(job.clone())) else {
        job.finish(Status::Busy, Vec::new());
        return;
    };
    let status = match begin(&job.provider, &job.method, &state, true, Some(job.handle)) {
        /* Its provider went between the start and now */
        Err(Status::NoProvider) => Status::Gone,
        Err(s) => s,
        Ok((handler, begun)) => {
            job.started();
            let s = run(handler, &begun, &job.method, &job.params, &state);
            drop(begun);
            s
        }
    };
    let (result, whole) = state.take();
    let status = if !whole && status == Status::Ok { Status::TooLarge } else { status };
    job.finish(status, result);
}

/// Every job of the registration `handle` not done told to stop: its
/// provider is going.
pub(crate) fn cancel_provider(handle: u64) {
    let Some(table) = jobs() else { return };
    let j = table.lock();
    for r in j.records.iter().filter(|r| r.handle == handle) {
        r.cancel.store(true, Ordering::Release);
    }
}

/// Told to stop; false for a job there is no record of, or one done.
pub fn job_cancel(id: u64) -> bool {
    let Some(table) = jobs() else { return false };
    let j = table.lock();
    match j.records.iter().find(|r| r.id == id) {
        Some(r) if !r.finished.load(Ordering::Acquire) => {
            r.cancel.store(true, Ordering::Release);
            true
        }
        _ => false,
    }
}

fn find(id: u64) -> Option<Arc<Job>> {
    let j = jobs()?.lock();
    j.records.iter().find(|r| r.id == id).cloned()
}

pub fn job_info(id: u64) -> Option<JobInfo> {
    find(id)?.info()
}

/// Every record, oldest first.
pub fn job_list() -> Vec<JobInfo> {
    let mut out = Vec::new();
    let Some(table) = jobs() else { return out };
    let records: Vec<Arc<Job>> = {
        let j = table.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(j.records.len()).is_err() {
            return out;
        }
        v.extend(j.records.iter().cloned());
        v
    };
    if out.try_reserve_exact(records.len()).is_ok() {
        out.extend(records.iter().filter_map(|r| r.info()));
    }
    out
}

/// The lines of a job's log from number `since` on, each handed to `line`:
/// the number the next will have, or None for no such job.
pub fn job_notes(id: u64, since: u64, line: &mut dyn FnMut(u64, &str)) -> Option<u64> {
    let job = find(id)?;
    /* Copied under the job's lock, handed on after it */
    let mut lines: Vec<(u64, String)> = Vec::new();
    let next = {
        let i = job.inner.lock();
        if lines.try_reserve_exact(i.notes.len()).is_err() {
            return Some(since);
        }
        for (seq, text) in i.notes.iter().filter(|(s, _)| *s >= since) {
            let Ok(t) = crate::copy(text) else { break };
            lines.push((*seq, t));
        }
        i.next_note
    };
    for (seq, text) in &lines {
        line(*seq, text);
    }
    Some(next)
}

/// A job's status and result, once it is done.
pub fn job_result(id: u64, sink: &mut dyn FnMut(&[u8]) -> bool) -> Option<Status> {
    let job = find(id)?;
    if !job.finished.load(Ordering::Acquire) {
        return None;
    }
    let (status, result) = {
        let i = job.inner.lock();
        let mut r = Vec::new();
        if r.try_reserve_exact(i.result.len()).is_err() {
            return Some(Status::Busy);
        }
        r.extend_from_slice(&i.result);
        (i.status, r)
    };
    if !result.is_empty() && !sink(&result) {
        return Some(Status::TooLarge);
    }
    Some(status)
}

/// The halt's part: every job told to stop and waited for -- saying every
/// 10 s which still runs -- and its task joined.
pub(crate) fn stop_all() {
    let Some(table) = jobs() else { return };
    let (records, tasks) = {
        let mut j = table.lock();
        j.stopped = true;
        for r in &j.records {
            r.cancel.store(true, Ordering::Release);
        }
        let mut records = Vec::new();
        if records.try_reserve_exact(j.records.len()).is_ok() {
            records.extend(j.records.iter().cloned());
        }
        (records, core::mem::take(&mut j.tasks))
    };
    let start = boot_time_ns();
    let mut said = start;
    while let Some(r) = records.iter().find(|r| !r.finished.load(Ordering::Acquire)) {
        let now = boot_time_ns();
        if now.saturating_sub(said) >= 10 * NS_PER_SEC {
            said = now;
            trace!(0, "mgmt: the halt has waited {} s for job {} ({}.{}) to stop",
                   now.saturating_sub(start) / NS_PER_SEC, r.id, r.provider, r.method);
        }
        kcore::task::sleep_ms(10);
    }
    drop(tasks);
}
