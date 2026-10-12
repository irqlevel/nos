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
//! The task is on a CPU of its own, not the vCPU's (`hv::place`): the disk
//! works while the guest runs rather than when it stops -- a thread of the
//! guest's own core where it has one free, since its cores are its own. It is the disk's for
//! as long as the disk is: dropping the disk tells it to stop -- after the
//! request it is serving, if any -- and waits for it, and what it had not
//! served goes with the queues.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use hv::disk::{self as blk, Op, Request};
use kcore::fs::{File, FileId};
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
/// what their tasks are called after, the CPU they are served on, the
/// claims every guest's disks hold on their images, and whose a claim of
/// theirs is.
#[derive(Clone)]
pub struct Runner {
    pub wake: Arc<dyn Wake>,
    /// A disk's task is `hv/<name>/vda`, ...
    pub name: String,
    pub cpu: u32,
    pub claims: Arc<Claims>,
    pub holder: Holder,
}

/// Whose guest a disk is: what a disk refused its image is told, so that
/// whoever reads it knows what to stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Holder {
    Vm(u32),
    Boot,
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Holder::Vm(id) => write!(f, "vm {}", id),
            Holder::Boot => f.write_str("an hv boot"),
        }
    }
}

/// Why a disk cannot have its image (`Claims::claim`).
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// `by`'s disk writes it -- or, with `writes` false, reads it, and
    /// this one would write it.
    Held { by: Holder, writes: bool },
    /// Every claim there is room for is held.
    Full,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Refused::Held { by, writes: true } => write!(f, "{}'s disk writes it", by),
            Refused::Held { by, writes: false } => write!(f, "{}'s disk reads it, and this one would write it", by),
            Refused::Full => f.write_str("every claim on an image is taken"),
        }
    }
}

/// The most claims held at once: every disk of every VM, and as many again
/// for `hv boot`s.
const CLAIMS_MAX: usize = 2 * crate::vms::MAX_VMS * hv::run::MAX_DISKS;

/// A claim held: the image, whether its disk writes it, and whose it is.
#[derive(Clone, Copy)]
struct Held {
    id: FileId,
    write: bool,
    by: Holder,
}

/// The images the guests' disks are open on, and how: a disk that writes its
/// image is the only one on it, and any number may read one nobody writes.
/// Two guests' filesystems on one set of blocks -- or one guest's on a
/// file another reads -- each wreck the other's with nothing to say so.
/// Keyed by the file, not the path: two names of one image are one image.
/// Among the VMs only: nos's own shell is not held to it.
pub struct Claims {
    held: Mutex<Vec<Held>>,
}

impl Claims {
    /// Room for every claim there can be, taken now: a claim allocates
    /// nothing.
    pub fn new() -> Option<Claims> {
        let mut held = Vec::new();
        held.try_reserve_exact(CLAIMS_MAX).ok()?;
        Some(Claims { held: Mutex::new(held)? })
    }

    /// Claim the image `id` for `by`'s disk, which writes it (`write`) or
    /// only reads it: held until the claim is dropped -- or why it cannot
    /// be.
    fn claim(self: &Arc<Claims>, id: FileId, write: bool, by: Holder) -> Result<Claim, Refused> {
        let mut held = self.held.lock();
        /* Readers share it; a writer has it alone. */
        if let Some(other) = held.iter().find(|h| h.id == id && (h.write || write)) {
            return Err(Refused::Held { by: other.by, writes: other.write });
        }
        if held.len() >= CLAIMS_MAX {
            return Err(Refused::Full);
        }
        /* Into the room taken at `new`. */
        held.push(Held { id, write, by });
        Ok(Claim { claims: self.clone(), id, by })
    }
}

/// A disk's claim on its image (`Claims`), given back when it goes.
struct Claim {
    claims: Arc<Claims>,
    id: FileId,
    by: Holder,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut held = self.claims.held.lock();
        /* Its own: two readers' claims on one image are two entries. */
        if let Some(i) = held.iter().position(|h| h.id == self.id && h.by == self.by) {
            held.swap_remove(i);
        }
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
    /// Its claim on its image (`Claims`): a field, and so given back only
    /// after `drop` has waited for the task, the file's last user.
    _claim: Claim,
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
        let id = file.identity().ok_or_else(|| alloc::format!("{}: the kernel does not know it", path))?;
        let claim = runner.claims.claim(id, !read_only, runner.holder)
            .map_err(|why| alloc::format!("{}: {}", path, why))?;

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
        Ok(FileDisk { pipe, size, read_only, task: Some(task), _claim: claim })
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
