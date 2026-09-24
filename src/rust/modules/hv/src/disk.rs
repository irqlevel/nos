//! A guest's disk that is a file of nos's -- an image, whose size is the
//! disk's, whole sectors of it -- held open while the guest runs, and read,
//! written and synced where the guest asks by a task of the disk's own, so
//! that the guest runs on while its disk works.
//!
//! The device hands a request over (`Backend::submit`) and the task serves
//! it through `kcore::fs::File`, one at a time in the order they came; what
//! it has served goes back the other way, and each one back wakes the vCPU
//! (`Wake`) -- a halted one's task out of its wait, a running one out of its
//! guest -- to give it to the guest. Both ways are queues under one lock,
//! with room for every request the device can have out (`IN_FLIGHT`) taken
//! when the disk is opened, so nothing on the way allocates; the vCPU looks
//! at a count first, every time round its loop, and takes the lock only
//! when something is there.
//!
//! The task is on a CPU of its own, not the vCPU's (`disk_cpu`): the disk
//! works while the guest runs rather than when it stops. It is the disk's for
//! as long as the disk is: dropping the disk tells it to stop -- after the
//! request it is serving, if any -- and waits for it, and what it had not
//! served goes with the queues.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use hv::disk::{self as blk, Op, Request};
use kcore::consts::MAX_CPUS;
use kcore::fs::File;
use kcore::sync::{Event, Mutex};
use kcore::task::TaskHandle;

/// Whoever runs a guest's vCPU, told that one of its disks has served
/// something for it: from the disk's task, while the vCPU may be halted --
/// asleep on an event -- or in its guest, which a kick ends. From any
/// context.
pub trait Wake: Send + Sync {
    fn wake(&self);
}

/// What a guest's disks are to know of whoever runs it: whom they wake,
/// what their tasks are called after, and the CPU they are served on.
#[derive(Clone)]
pub struct Runner {
    pub wake: Arc<dyn Wake>,
    /// A disk's task is `hv/<name>/vda`, ...
    pub name: String,
    pub cpu: u32,
}

/// The CPU a guest's disks are served on: an online one other than its
/// vCPU's -- the vCPU's own only on a machine of one CPU -- with the fewest
/// vCPUs (`load` counts them, by CPU), the higher of any two that tie, CPU 0
/// last: where the boot CPU's own work runs.
pub fn disk_cpu(vcpu: u32, load: &[u32; MAX_CPUS]) -> u32 {
    let online = kcore::cpu::online_mask();
    let mut best: Option<u32> = None;
    for cpu in 1..MAX_CPUS as u32 {
        if cpu == vcpu || online & (1u64 << cpu) == 0 {
            continue;
        }
        if best.map_or(true, |b| load[cpu as usize] <= load[b as usize]) {
            best = Some(cpu);
        }
    }
    match best {
        Some(cpu) => cpu,
        None if vcpu != 0 => 0,
        None => vcpu,
    }
}

/// The requests on their way: to the task, and back.
struct Requests {
    todo: VecDeque<Request>,
    done: VecDeque<Request>,
    /// The disk is going: the task serves nothing more.
    stop: bool,
}

/// What the disk's task and its vCPU share.
struct Pipe {
    requests: Mutex<Requests>,
    /// How many wait in `done`: what the vCPU looks at before the lock.
    done: AtomicUsize,
    /// What the task waits on: a request handed over, or the stop.
    work: Event,
    wake: Arc<dyn Wake>,
}

/// What the disk's task starts from: the pipe, and the file, which is the
/// task's alone and closed when it ends.
struct Serve {
    pipe: Arc<Pipe>,
    file: File,
}

pub struct FileDisk {
    pipe: Arc<Pipe>,
    size: u64,
    read_only: bool,
    /// The disk's task: told to stop, and waited for, when the disk goes.
    task: Option<TaskHandle>,
}

impl FileDisk {
    /// The image at `path` -- read-only to the guest with `read_only` --
    /// served for `runner`'s guest by a task called after it and the disk's
    /// `letter`.
    pub fn open(path: &str, read_only: bool, runner: &Runner, letter: char) -> Result<FileDisk, String> {
        let file = File::open(path, !read_only).map_err(|e| alloc::format!("{}: {}", path, e))?;
        let size = file.size().map_err(|e| alloc::format!("{}: {}", path, e))?;
        let size = size - size % blk::SECTOR;
        if size == 0 {
            return Err(alloc::format!("{}: not a sector long", path));
        }

        let mut todo = VecDeque::new();
        let mut done = VecDeque::new();
        let mut name = String::new();
        if todo.try_reserve_exact(blk::IN_FLIGHT).is_err() || done.try_reserve_exact(blk::IN_FLIGHT).is_err()
            || name.try_reserve(runner.name.len() + 8).is_err()
        {
            return Err(alloc::format!("{}: out of memory for its queues", path));
        }
        let (Some(requests), Some(work)) = (Mutex::new(Requests { todo, done, stop: false }), Event::new()) else {
            return Err(alloc::format!("{}: out of memory for its queues", path));
        };
        let pipe = Arc::new(Pipe { requests, done: AtomicUsize::new(0), work, wake: runner.wake.clone() });

        let _ = write!(name, "hv/{}/vd{}", runner.name, letter);
        let task = kcore::task::spawn_on_with(&name, 1u64 << runner.cpu, Serve { pipe: pipe.clone(), file }, serve)
            .ok_or_else(|| alloc::format!("{}: no task to serve it", path))?;
        Ok(FileDisk { pipe, size, read_only, task: Some(task) })
    }
}

impl blk::Backend for FileDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn submit(&mut self, req: Request) {
        /* Into the room taken at `open`: the device never has more out. */
        self.pipe.requests.lock().todo.push_back(req);
        self.pipe.work.signal();
    }

    fn take(&mut self) -> Option<Request> {
        if self.pipe.done.load(Ordering::Acquire) == 0 {
            return None;
        }
        let mut requests = self.pipe.requests.lock();
        let req = requests.done.pop_front();
        self.pipe.done.store(requests.done.len(), Ordering::Release);
        req
    }
}

impl Drop for FileDisk {
    fn drop(&mut self) {
        self.pipe.requests.lock().stop = true;
        self.pipe.work.signal();
        /* Waits for the task -- after the request it is serving -- and so for
         * the last wake it makes. */
        drop(self.task.take());
    }
}

/// The disk's task: every request in the order it came, until the disk goes.
/// A read that comes back short, or any call that fails, is an error the
/// guest is told of.
fn serve(start: Serve) {
    let Serve { pipe, file } = start;
    loop {
        let next = {
            let mut requests = pipe.requests.lock();
            if requests.stop {
                break;
            }
            requests.todo.pop_front()
        };
        let Some(mut req) = next else {
            /* A request or the stop signals it; one signalled since the look
             * is not lost -- the event remembers it. */
            pipe.work.wait();
            continue;
        };

        let offset = req.offset();
        let ok = match req.op() {
            Op::Read => {
                let buf = req.data_mut();
                matches!(file.read_at(offset, buf), Ok(n) if n == buf.len())
            }
            Op::Write => file.write_at(offset, req.data()).is_ok(),
            Op::Flush => file.sync().is_ok(),
        };
        req.done(ok);

        {
            let mut requests = pipe.requests.lock();
            /* Into the room taken at `open`, as `submit`'s. */
            requests.done.push_back(req);
            pipe.done.store(requests.done.len(), Ordering::Release);
        }
        pipe.wake.wake();
    }
}
