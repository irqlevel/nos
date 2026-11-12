//! A guest's disk that is a file of nos's -- an image, whose size is the
//! disk's, whole sectors of it -- or a device of nos's, a partition or a
//! whole disk (`crate::devices`), held while the guest runs, and read,
//! written and synced where the guest asks by a task of the disk's own, so
//! that the guest runs on while its disk works. A device's disk leaves the
//! filesystem out entirely: its requests go to the block layer's queue for
//! it (`kcore::block::DiskQueue`), as an image's go to the image's -- what
//! follows of an image is so of a device, but for its claim, which is the
//! block layer's.
//!
//! The image is held (`kcore::fs::Image`): nothing else may write the file,
//! cut it or move it while the disk is open, and so its reads and writes go
//! to the device off the VFS's lock -- one guest's disk does not wait
//! behind another's, nor behind nos's own files.
//!
//! The image is held with a queue of its own (`kcore::fs::ImageQueue`): a
//! buffer a request the device can have out (`IN_FLIGHT`), which are the
//! device's buffers too (`Backend::with_buffer`) -- a write's data copied
//! into one out of the guest's memory, a read's out of one into it -- and
//! which the image's disk reads and writes as they are, with no copy of its
//! own. The device hands a request over (`Backend::submit`) and the task
//! starts it the moment it finds it, so the disk has every request the
//! guest has out; each comes back when it is done -- not with the slowest
//! of a batch -- and what came back goes the other way, each lot waking the
//! vCPU (`Wake`) -- a halted one's task out of its wait, a running one out
//! of its guest -- to give it to the guest. A flush, a discard and a write
//! of zeros are served on their own, the image's I/O in flight done first.
//! The way there and the way back are queues under one lock, with room for
//! every request the device can have out taken when the disk is opened, so
//! nothing on the way allocates; the vCPU looks at a count first, every time
//! round its loop, and takes the lock only when something is there. The task
//! sleeps on its image's queue, which a request handed over wakes as well as
//! a command done -- spinning for it first while the guest keeps its disk
//! busy, for as long as its waits have been short (`POLL_MAX_NS`), so that
//! neither costs a sleep and a wake.
//!
//! A volume of NBS's (`nbs:<volume>`, plans/nbs.md, N8) is a disk the same
//! way: its media is the volume's session (nbsdrive's `Drive`), attached
//! as the disk opens -- for writing at a new generation, every cs fenced,
//! what its last writer left open reconciled -- whose requests the task
//! starts and whose answers it takes as an image's, its buffers the
//! session's own, its waits on the session's mailbox and the guest's
//! requests alike. The disk going lets the volume go: flushed, its chunks
//! closed, detached.
//!
//! The task is on a CPU of its own, not the vCPU's (`hv::place`): the disk
//! works while the guest runs rather than when it stops -- a thread of the
//! guest's own core where it has one free, since its cores are its own. It is the disk's for
//! as long as the disk is: dropping the disk tells it to stop -- after the
//! request it is serving, if any -- and waits for it, and what it had not
//! served goes with the queues.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use hv::disk::{self as blk, Op, Request};
use kcore::block::{DiskQueue, QueueError};
use kcore::events::SharedEvent;
use kcore::fs::{File, FileId, Image, ImageQueue, Lent, Started};
use kcore::sync::{Mutex, MutexGuard};
use kcore::task::TaskHandle;
use kcore::time::Duration;
use kcore::trace::DiskHow;

/// Whoever runs a guest's vCPU, told that one of its disks has served
/// something for it: from the disk's task, while the vCPU may be halted --
/// asleep on an event -- or in its guest, which a kick ends. From any
/// context.
pub trait Wake: Send + Sync {
    fn wake(&self);

    /// Disk `disk` -- vda 0 and on -- has served requests, which wait in its
    /// pipe: given to the guest by whoever runs it -- by the disk's own task
    /// here, where the guest lets it (`hv::run::LinuxGuest::serve_disk`) --
    /// or the vCPU that does the devices' work woken for them.
    fn served(&self, disk: u8) {
        let _ = disk;
        self.wake();
    }
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
    /// What a device its disk is on is said to be held by, to whoever is
    /// refused it meanwhile: `vm web's disk`.
    pub owner: String,
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
    wake: Arc<dyn Wake>,
}

/// What the disk's task starts from: the pipe, and the image's queue -- its
/// calls the task's alone; the vCPU only lends its buffers and wakes it --
/// and the disk's number among its guest's, which its records say.
struct Serve {
    pipe: Arc<Pipe>,
    queue: Arc<dyn Media>,
    disk: u8,
    /// The CPU it is bound to, held to wake quickly while it serves.
    cpu: u32,
}

/// How long the task sleeps with nothing to do before it looks again: a
/// request handed over and a command done each wake it, so this is only
/// how stale a look may be.
const IDLE_SECS: u64 = 1;

/// How long the task spins for what it waits for before it sleeps for it,
/// at first and at most: grown after a wait that was short and dropped
/// after one that was long, as a vCPU's halt poll is (`hv::run`). A guest
/// keeping its disk busy has each request and each command done come
/// within microseconds -- the NVMe's 4k read in 38 us on the EX44 -- and a
/// sleep and a wake cost 5 us apiece there: two a request at QD1, 10 of
/// its 49 us. QEMU's iothread polls likewise. The task's CPU is its own
/// (`hv::place`), and an idle disk spins not at all.
const POLL_FIRST_NS: u64 = 10 * kcore::consts::NS_PER_US;
const POLL_MAX_NS: u64 = 100 * kcore::consts::NS_PER_US;

/// What a disk's task serves its requests with: an image's queue
/// (`kcore::fs::ImageQueue`) for a disk that is a file, a device's
/// (`kcore::block::DiskQueue`) for one that is a partition or a whole disk.
/// Their calls are the same; what is not a read or a write goes to the file
/// or the device by these too.
trait Media: Send + Sync {
    fn lend(&self, k: usize) -> Option<Buf<'_>>;
    fn start(&self, k: usize, write: bool, offset: u64, len: usize) -> kcore::error::Result<Started>;
    fn kick(&self);
    fn reap(&self, done: &mut dyn FnMut(usize, bool)) -> usize;
    fn poll(&self, d: Duration) -> bool;
    fn wait(&self, d: Duration) -> bool;
    fn wake(&self);
    fn flush(&self) -> bool;
    fn discard(&self, offset: u64, len: u64) -> bool;
    fn write_zeroes(&self, offset: u64, len: u64, unmap: bool) -> bool;
}

/// A buffer of a disk's lent to its device between requests: a queue's of
/// the kernel's (`Lent`), or one of a volume's session's own.
enum Buf<'a> {
    Kernel(Lent<'a>),
    Own(MutexGuard<'a, Vec<u8>>),
}

impl core::ops::Deref for Buf<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Buf::Kernel(l) => l,
            Buf::Own(v) => v.as_slice(),
        }
    }
}

impl core::ops::DerefMut for Buf<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            Buf::Kernel(l) => l,
            Buf::Own(v) => v.as_mut_slice(),
        }
    }
}

impl Media for ImageQueue {
    fn lend(&self, k: usize) -> Option<Buf<'_>> {
        ImageQueue::lend(self, k).map(Buf::Kernel)
    }
    fn start(&self, k: usize, write: bool, offset: u64, len: usize) -> kcore::error::Result<Started> {
        ImageQueue::start(self, k, write, offset, len)
    }
    fn kick(&self) {
        ImageQueue::kick(self)
    }
    fn reap(&self, done: &mut dyn FnMut(usize, bool)) -> usize {
        ImageQueue::reap(self, done)
    }
    fn poll(&self, d: Duration) -> bool {
        ImageQueue::poll(self, d)
    }
    fn wait(&self, d: Duration) -> bool {
        ImageQueue::wait(self, d)
    }
    fn wake(&self) {
        ImageQueue::wake(self)
    }
    fn flush(&self) -> bool {
        self.image().sync().is_ok()
    }
    /* Each range's whole chunks given back, as a discard asks */
    fn discard(&self, offset: u64, len: u64) -> bool {
        self.image().discard(offset, len).is_ok()
    }
    fn write_zeroes(&self, offset: u64, len: u64, unmap: bool) -> bool {
        self.image().write_zeroes(offset, len, unmap).is_ok()
    }
}

impl Media for DiskQueue {
    fn lend(&self, k: usize) -> Option<Buf<'_>> {
        DiskQueue::lend(self, k).map(Buf::Kernel)
    }
    fn start(&self, k: usize, write: bool, offset: u64, len: usize) -> kcore::error::Result<Started> {
        DiskQueue::start(self, k, write, offset, len)
    }
    fn kick(&self) {
        DiskQueue::kick(self)
    }
    fn reap(&self, done: &mut dyn FnMut(usize, bool)) -> usize {
        DiskQueue::reap(self, done)
    }
    fn poll(&self, d: Duration) -> bool {
        DiskQueue::poll(self, d)
    }
    fn wait(&self, d: Duration) -> bool {
        DiskQueue::wait(self, d)
    }
    fn wake(&self) {
        DiskQueue::wake(self)
    }
    fn flush(&self) -> bool {
        DiskQueue::flush(self).is_ok()
    }
    fn discard(&self, offset: u64, len: u64) -> bool {
        DiskQueue::discard(self, offset, len).is_ok()
    }
    /* Never asked: the device does not offer it (`Backend::writes_zeroes`). */
    fn write_zeroes(&self, _offset: u64, _len: u64, _unmap: bool) -> bool {
        false
    }
}

/// A volume of NBS's as a disk's media: its session -- the disk's task's
/// alone, but for the take at the end -- a buffer a request the device can
/// have out, each request's slot by the session's tag for it, and the
/// event the session's mailbox is made on, which a request handed over
/// signals too.
struct Volume {
    name: String,
    drive: Mutex<Option<nbsdrive::Drive>>,
    slots: Vec<Mutex<Vec<u8>>>,
    tags: Mutex<BTreeMap<u64, usize>>,
    event: SharedEvent,
}

impl Volume {
    /// Volume `name` attached on this host -- read-only, or for writing and
    /// with `force` taken from a writer elsewhere -- its session serving.
    fn open(name: &str, read_only: bool, force: bool) -> Result<Volume, String> {
        let me = nbsdrive::this_host()?;
        let event = SharedEvent::new().ok_or_else(|| String::from("no event for its session"))?;
        /* Everything the disk needs first, its 16 MiB of buffers above all,
         * and the attach last: once attached for writing, a new generation
         * taken and every chunk server fenced, only the drop of a `Volume`
         * holding the drive lets go of it -- a failure after the attach
         * left the volume attached here, refused to readers and to another
         * host without `force`. */
        let mut slots = Vec::new();
        slots.try_reserve_exact(blk::IN_FLIGHT).map_err(|_| String::from("out of memory for its buffers"))?;
        for _ in 0..blk::IN_FLIGHT {
            let mut buf = Vec::new();
            buf.try_reserve_exact(blk::REQUEST_BYTES).map_err(|_| String::from("out of memory for its buffers"))?;
            buf.resize(blk::REQUEST_BYTES, 0);
            slots.push(Mutex::new(buf).ok_or_else(|| String::from("out of memory for its buffers"))?);
        }
        let mut owned = String::new();
        owned.try_reserve_exact(name.len()).map_err(|_| String::from("out of memory"))?;
        owned.push_str(name);
        let volume = Volume {
            name: owned,
            drive: Mutex::new(None).ok_or_else(|| String::from("out of memory"))?,
            slots,
            tags: Mutex::new(BTreeMap::new()).ok_or_else(|| String::from("out of memory"))?,
            event,
        };
        let drive = nbsdrive::Drive::open_on(name, &me, !read_only, force, &volume.event)?;
        *volume.drive.lock() = Some(drive);
        Ok(volume)
    }

    /// The session's size, its volume's number and generation.
    fn shape(&self) -> (u64, u32, u64) {
        self.drive.lock().as_ref().map_or((0, 0, 0), |d| {
            let s = d.session();
            (s.size(), s.volume(), s.generation())
        })
    }

    /// What the session has done, each back in its slot -- a read's bytes
    /// copied into it, a write's buffer put back -- and said by `done`.
    fn take(&self, d: &mut nbsdrive::Drive, done: &mut dyn FnMut(usize, bool)) -> usize {
        let mut n = 0;
        while let Some(t) = d.take_done() {
            let Some(k) = self.tags.lock().remove(&t.tag) else { continue };
            if let Some(slot) = self.slots.get(k) {
                let mut buf = slot.lock();
                if buf.is_empty() {
                    /* A write's: its own buffer, whole again */
                    *buf = t.data;
                    buf.resize(blk::REQUEST_BYTES, 0);
                } else if let Some(to) = buf.get_mut(..t.data.len()) {
                    to.copy_from_slice(&t.data);
                }
            }
            done(k, t.ok);
            n += 1;
        }
        n
    }
}

impl Media for Volume {
    fn lend(&self, k: usize) -> Option<Buf<'_>> {
        self.slots.get(k).map(|s| Buf::Own(s.lock()))
    }

    fn start(&self, k: usize, write: bool, offset: u64, len: usize) -> kcore::error::Result<Started> {
        use kcore::error::Error;
        let slot = self.slots.get(k).ok_or(Error::InvalidValue)?;
        let mut d = self.drive.lock();
        let d = d.as_mut().ok_or(Error::IoError)?;
        let tag = if write {
            /* The slot's buffer goes with the write, and comes back with it */
            let mut data = core::mem::take(&mut *slot.lock());
            data.truncate(len);
            match d.submit(nbsdrive::Op::Write { offset }, data) {
                Ok(tag) => tag,
                Err(_) => {
                    slot.lock().resize(blk::REQUEST_BYTES, 0);
                    return Err(Error::NoMemory);
                }
            }
        } else {
            let len = u32::try_from(len).map_err(|_| Error::InvalidValue)?;
            d.submit(nbsdrive::Op::Read { offset, len }, Vec::new()).map_err(|_| Error::NoMemory)?
        };
        self.tags.lock().insert(tag, k);
        Ok(Started::InFlight)
    }

    fn kick(&self) {}

    fn reap(&self, done: &mut dyn FnMut(usize, bool)) -> usize {
        let mut d = self.drive.lock();
        let Some(d) = d.as_mut() else { return 0 };
        d.turn(0);
        self.take(d, done)
    }

    fn poll(&self, wait: Duration) -> bool {
        self.wait(wait)
    }

    fn wait(&self, wait: Duration) -> bool {
        let mut d = self.drive.lock();
        let Some(d) = d.as_mut() else { return false };
        d.turn(wait.as_nanos());
        d.has_done()
    }

    fn wake(&self) {
        self.event.signal();
    }

    fn flush(&self) -> bool {
        self.drive.lock().as_mut().is_some_and(|d| d.flush().is_ok())
    }

    /* Neither offered: the device does not (`Backend::discards`). */
    fn discard(&self, _offset: u64, _len: u64) -> bool {
        false
    }

    fn write_zeroes(&self, _offset: u64, _len: u64, _unmap: bool) -> bool {
        false
    }
}

impl Drop for Volume {
    /// The volume let go of: what is out done, flushed, its chunks closed,
    /// detached -- or said, where the session cannot.
    fn drop(&mut self) {
        let d = self.drive.lock().take();
        if let Some(d) = d {
            match d.stop() {
                Ok(()) => kcore::klog!(1, "hv: nbs:{} let go of", self.name),
                Err(e) => kcore::klog!(0, "hv: nbs:{} not let go of cleanly: {}", self.name, e),
            }
        }
    }
}

/// A guest's disk served by a task of its own: a file of nos's -- an image
/// -- a device of nos's, a partition or a whole disk (`crate::devices`), or
/// a volume of NBS's.
pub struct QueuedDisk {
    pipe: Arc<Pipe>,
    /// The image's or the device's queue and its buffers, which the device
    /// fills and empties -- before `_claim`, so that the image is let go of
    /// before its claim is. A device's queue holds the device's claim itself.
    queue: Arc<dyn Media>,
    size: u64,
    read_only: bool,
    /// What the device tells the guest to align its discards to
    /// (`Backend::discard_sectors`): the image's chunk.
    discard_sectors: u32,
    /// What the virtio device offers the guest of a discard and a write of
    /// zeros, and the block every request is whole ones of.
    discards: bool,
    writes_zeroes: bool,
    block_size: u32,
    /// The disk's task: told to stop, and waited for, when the disk goes.
    task: Option<TaskHandle>,
    /// Its claim on its image (`Claims`): a field, and so given back only
    /// after `drop` has waited for the task, the image's last user. None for
    /// a device, whose queue holds the block layer's claim on it.
    _claim: Option<Claim>,
    /// What the guest's GET_ID says, where it is not the disk's letter: a
    /// volume's `nbs-<number>`.
    serial: Option<String>,
}

/// What a disk is served from, and how: the guest's view of it.
struct Served {
    queue: Arc<dyn Media>,
    size: u64,
    discard_sectors: u32,
    discards: bool,
    writes_zeroes: bool,
    block_size: u32,
    claim: Option<Claim>,
    serial: Option<String>,
}

impl QueuedDisk {
    /// The disk `word` names -- a file's path, a device's id (`part:...`,
    /// `disk:...`) or name (`dev:nvme14`), or a volume of NBS's
    /// (`nbs:<volume>`, taken from a writer elsewhere with `force`) --
    /// read-only to the guest with `read_only`, served for `runner`'s guest
    /// as its disk `index` (vda 0), by a task called after it and the
    /// disk's letter.
    pub fn open(word: &str, read_only: bool, force: bool, runner: &Runner, index: u8)
        -> Result<QueuedDisk, String> {
        let served = match crate::devices::source(word) {
            crate::devices::Source::File(path) => open_file(path, read_only, runner)?,
            crate::devices::Source::Volume(name) => open_volume(name, read_only, force)?,
            _ => open_device(word, read_only, runner)?,
        };

        let mut todo = VecDeque::new();
        let mut done = VecDeque::new();
        let mut name = String::new();
        if todo.try_reserve_exact(blk::IN_FLIGHT).is_err() || done.try_reserve_exact(blk::IN_FLIGHT).is_err()
            || name.try_reserve(runner.name.len() + 8).is_err()
        {
            return Err(alloc::format!("{}: out of memory for its queues", word));
        }
        let Some(requests) = Mutex::new(Requests { todo, done, stop: false }) else {
            return Err(alloc::format!("{}: out of memory for its queues", word));
        };
        let pipe = Arc::new(Pipe { requests, done: AtomicUsize::new(0), wake: runner.wake.clone() });

        let letter = char::from(b'a'.saturating_add(index));
        let _ = write!(name, "hv/{}/vd{}", runner.name, letter);
        let queue = served.queue.clone();
        let task = kcore::task::spawn_on_with(&name, 1u64 << runner.cpu,
                                              Serve { pipe: pipe.clone(), queue: queue.clone(), disk: index,
                                                      cpu: runner.cpu }, serve)
            .ok_or_else(|| alloc::format!("{}: no task to serve it", word))?;
        Ok(QueuedDisk { pipe, queue, size: served.size, read_only, discard_sectors: served.discard_sectors,
                        discards: served.discards, writes_zeroes: served.writes_zeroes,
                        block_size: served.block_size, task: Some(task), _claim: served.claim,
                        serial: served.serial })
    }

    /// What the guest's GET_ID says of it, where not the disk's letter.
    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
}

/// Volume `name` of NBS's as a disk: its session started -- attached, the
/// cs's fenced, what its last writer left open reconciled -- its blocks
/// the volume's 4 KiB, which every request is whole ones of.
fn open_volume(name: &str, read_only: bool, force: bool) -> Result<Served, String> {
    let v = Volume::open(name, read_only, force).map_err(|e| alloc::format!("nbs:{}: {}", name, e))?;
    let (size, number, generation) = v.shape();
    if size == 0 {
        return Err(alloc::format!("nbs:{}: no blocks", name));
    }
    kcore::klog!(1, "hv: nbs:{} (volume {}) attached, generation {}", name, number, generation);
    Ok(Served { queue: Arc::new(v), size, discard_sectors: blk::DISCARD_ALIGNMENT, discards: false,
                writes_zeroes: false, block_size: nbsdrive::BLOCK as u32, claim: None,
                serial: Some(alloc::format!("nbs-{}", number)) })
}

/// The image at `path`, held as a disk -- for writing unless `read_only` --
/// claimed among the guests' disks for `runner`'s.
fn open_file(path: &str, read_only: bool, runner: &Runner) -> Result<Served, String> {
    /* Which file it is, first: the claim says whose guest's disk has it,
     * where the hold below would only refuse. */
    let id = File::open(path, false).map_err(|e| alloc::format!("{}: {}", path, e))?
        .identity().ok_or_else(|| alloc::format!("{}: the kernel does not know it", path))?;
    let claim = runner.claims.claim(id, !read_only, runner.holder)
        .map_err(|why| alloc::format!("{}: {}", path, why))?;
    let image = Image::open(path, !read_only).map_err(|_| {
        alloc::format!("{}: cannot be held as a disk -- nos has it open for writing, or cannot write it here", path)
    })?;
    let queue = image.into_queue(blk::IN_FLIGHT, blk::REQUEST_BYTES)
        .map_err(|_| alloc::format!("{}: out of memory for its buffers", path))?;
    let image = queue.image();
    /* The file the claim is on, still: not one put in its place between. */
    if image.identity() != Some(id) {
        return Err(alloc::format!("{}: replaced while it was being opened", path));
    }
    let size = image.size().map_err(|e| alloc::format!("{}: {}", path, e))?;
    let size = size - size % blk::SECTOR;
    if size == 0 {
        return Err(alloc::format!("{}: not a sector long", path));
    }
    /* What the image gives back of a discard: whole chunks, and nothing
     * of less -- what the guest is told to align its discards to, so
     * that its long ones are split on a chunk's edge. A block where it
     * gives back nothing at all. */
    let discard_sectors = u32::try_from(image.discard_unit() / blk::SECTOR).ok().filter(|&s| s != 0)
        .unwrap_or(blk::DISCARD_ALIGNMENT);
    Ok(Served { queue: Arc::new(queue), size, discard_sectors, discards: true, writes_zeroes: true,
                block_size: blk::SECTOR as u32, claim: Some(claim), serial: None })
}

/// The device `word` names -- by its id, or for `hv boot` and a start with
/// a kernel its name -- held as a disk: claimed in the block layer for
/// `runner`'s guest, whether it writes it or not, so that nothing else does
/// meanwhile -- no mount, disk log or other guest, nor anything overlapping
/// it -- and refused outright where the machine's firmware boots from it.
fn open_device(word: &str, read_only: bool, runner: &Runner) -> Result<Served, String> {
    let disk = crate::devices::find(word)?;
    if let Some(what) = disk.kind().boots_the_machine() {
        return Err(alloc::format!("{} ({}) is {}: the machine boots from it, and no guest is given it", word,
                                  disk.name(), what));
    }
    let ss = disk.sector_size();
    if ss != blk::SECTOR && ss != 4096 {
        return Err(alloc::format!("{} ({}) has sectors of {} bytes: a guest's disk is given 512 or 4096", word,
                                  disk.name(), ss));
    }
    let name = disk.name();
    let discards = !read_only && disk.can_discard();
    let queue = disk.into_queue(!read_only, &runner.owner, blk::IN_FLIGHT, blk::REQUEST_BYTES).map_err(|e| match e {
        QueueError::Held(by) => alloc::format!("{} ({}) is in use by {}", word, name, by),
        QueueError::Refused => alloc::format!("{} ({}): no memory for its queue, or every queue is taken", word, name),
    })?;
    let size = queue.size();
    if size == 0 {
        return Err(alloc::format!("{} ({}): not a sector long", word, name));
    }
    Ok(Served { queue: Arc::new(queue), size, discard_sectors: blk::DISCARD_ALIGNMENT.max((ss / blk::SECTOR) as u32),
                discards, writes_zeroes: false, block_size: ss as u32, claim: None, serial: None })
}

impl blk::Backend for QueuedDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn discard_sectors(&self) -> u32 {
        self.discard_sectors
    }

    fn discards(&self) -> bool {
        self.discards
    }

    fn writes_zeroes(&self) -> bool {
        self.writes_zeroes
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn with_buffer(&mut self, slot: usize, use_it: &mut dyn FnMut(&mut [u8]) -> bool) -> bool {
        /* None while the image has the slot's request: the device reaches a
         * buffer only between requests. */
        match self.queue.lend(slot) {
            Some(mut lent) => use_it(&mut lent),
            None => false,
        }
    }

    fn submit(&mut self, req: Request) {
        /* Into the room taken at `open`: the device never has more out. */
        self.pipe.requests.lock().todo.push_back(req);
        self.queue.wake();
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

impl Drop for QueuedDisk {
    fn drop(&mut self) {
        self.pipe.requests.lock().stop = true;
        self.queue.wake();
        /* Waits for the task -- after the request it is serving -- and so for
         * the last wake it makes. The image waits for what it has in flight
         * as it is let go of, with the last of the queue. */
        drop(self.task.take());
    }
}

/// The disk's task, until the disk goes: every request started as it comes
/// -- a read's or a write's with the image's queue (`ImageQueue::start`), a
/// flush, a discard or a write of zeros served then and there -- and each
/// given back when it is done, every lot of them with a wake of the vCPU.
/// Any call that fails is an error the guest is told of. Each request's
/// `disk_start` is recorded as it is picked up, and its `disk_done` as it is
/// done -- before it goes back the other way.
fn serve(start: Serve) {
    let Serve { pipe, queue, disk, cpu } = start;
    /* The CPU it serves from held to wake quickly while it does: its idles
     * between the guest's requests out of the C-state between C1 and the
     * deepest, whose wake each request after one would wait for. */
    let _latency = kcore::cpu::Latency::hold(cpu);
    let (mut new, mut finished, mut flying) = (Vec::new(), Vec::new(), Vec::new());
    if new.try_reserve_exact(blk::IN_FLIGHT).is_err() || finished.try_reserve_exact(blk::IN_FLIGHT).is_err()
        || flying.try_reserve_exact(blk::IN_FLIGHT).is_err()
    {
        /* Nothing served: the guest's requests stay with the device, and
         * its disk goes quiet -- said, for whoever looks. */
        kcore::klog!(0, "hv: a disk's task has no memory for its requests");
        return;
    }
    flying.resize_with(blk::IN_FLIGHT, || None::<Request>);
    /* How long the next wait spins first (POLL_FIRST_NS) */
    let mut poll_ns = 0u64;
    loop {
        {
            let mut requests = pipe.requests.lock();
            if requests.stop {
                break;
            }
            /* Into the room taken above: the device never has more out. */
            new.extend(requests.todo.drain(..));
        }
        let mut owed = false;
        for mut req in new.drain(..) {
            let (slot, op) = (req.slot(), req.op());
            hv::tp::disk_start(disk, slot, op, req.offset(), req.len());
            match op {
                Op::Read | Op::Write => {
                    match queue.start(slot, op == Op::Write, req.offset(), req.len()) {
                        Ok(Started::InFlight) if flying.get(slot).is_some_and(Option::is_none) => {
                            owed = true;
                            flying[slot] = Some(req);
                        }
                        Ok(Started::InFlight) => {
                            /* A slot the device has two requests of: never. */
                            kcore::klog!(0, "hv: a disk's slot {} started twice", slot);
                        }
                        Ok(Started::Done) => {
                            req.done(true);
                            hv::tp::disk_done(disk, slot, op, DiskHow::AtOnce, true);
                            finished.push(req);
                        }
                        Err(_) => {
                            req.done(false);
                            hv::tp::disk_done(disk, slot, op, DiskHow::AtOnce, false);
                            finished.push(req);
                        }
                    }
                }
                _ => {
                    /* What was started goes to the disk first: this one waits
                     * for it. */
                    if core::mem::take(&mut owed) {
                        queue.kick();
                    }
                    let ok = serve_one(&*queue, &mut req);
                    hv::tp::disk_done(disk, slot, op, DiskHow::Command, ok);
                    finished.push(req);
                }
            }
        }
        if owed {
            queue.kick();
        }
        queue.reap(&mut |slot, ok| match flying.get_mut(slot).and_then(Option::take) {
            Some(mut req) => {
                req.done(ok);
                hv::tp::disk_done(disk, slot, req.op(), DiskHow::Async, ok);
                finished.push(req);
            }
            None => kcore::klog!(0, "hv: a disk's image gave back slot {}, which it was not given", slot),
        });
        if finished.is_empty() {
            /* A request handed over wakes it, and so does a command done --
             * one that came since the look is not lost: the queue remembers
             * it. Spun for first, for as long as the waits before say it is
             * likely to come (POLL_MAX_NS). */
            let looked = kcore::time::boot_time_ns();
            if poll_ns != 0 && queue.poll(Duration::from_nanos(poll_ns)) {
                continue;
            }
            queue.wait(Duration::from_secs(IDLE_SECS));
            let waited = kcore::time::boot_time_ns().saturating_sub(looked);
            poll_ns = if waited > POLL_MAX_NS {
                0
            } else if waited > poll_ns {
                poll_ns.saturating_mul(2).clamp(POLL_FIRST_NS, POLL_MAX_NS)
            } else {
                poll_ns
            };
            continue;
        }
        {
            let mut requests = pipe.requests.lock();
            /* Into the room taken at `open`, as `submit`'s. */
            requests.done.extend(finished.drain(..));
            pipe.done.store(requests.done.len(), Ordering::Release);
        }
        pipe.wake.served(disk);
    }
}

/// A request that is not a read or a write, through the file or the device:
/// whether it went well.
fn serve_one(media: &dyn Media, req: &mut Request) -> bool {
    let ok = match req.op() {
        /* Never here: the queue's. */
        Op::Read | Op::Write => false,
        Op::Flush => media.flush(),
        Op::Discard => req.ranges().all(|r| media.discard(r.offset, r.len)),
        Op::WriteZeroes => req.ranges().all(|r| media.write_zeroes(r.offset, r.len, r.unmap)),
    };
    req.done(ok);
    ok
}
