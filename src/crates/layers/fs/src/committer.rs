//! The task that commits what a filesystem staged once it has waited long
//! enough (`FileSystem::commit_due`): ext4's group of transactions, which a
//! sync commits at once and nothing else might commit for as long as nobody
//! syncs -- so that a call that returned is on the medium a few seconds
//! later whoever asks, as Linux's journal thread has it.
//!
//! One for every mount, started with the first mount whose filesystem
//! wants it (`Vfs::mount`), and stopped at the halt before the filesystems
//! are unmounted (`kernel_vfs_unmount_all`), each of which commits what it
//! holds itself. A filesystem asks `running` as it stages: with no task --
//! none could be had, or it has stopped -- it commits as it goes, each
//! transaction its own commit.

use core::sync::atomic::{AtomicBool, Ordering};

use kcore::klog;
use kcore::once::Once;
use kcore::sync::{Event, TryLock};
use kcore::task::{self, TaskHandle};
use kcore::time::{self, Duration};

/// How often the task looks at every mount.
const LOOK_MS: u64 = 1000;

/// Whether the task runs: set once it is spawned, cleared as it is told to
/// stop -- from then on a filesystem commits as it goes.
static RUNNING: AtomicBool = AtomicBool::new(false);
static STOPPING: AtomicBool = AtomicBool::new(false);
/// What wakes it early: the stop.
static WAKE: Once<Event> = Once::new();
/// The task, for the halt to wait for.
static TASK: TryLock<Option<TaskHandle>> = TryLock::new(None);

/// Whether a task commits in the background: what a filesystem that stages
/// asks before it leaves a commit for later.
pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// The task started, once: by the first mount that wants it. Said in the
/// log where it cannot be -- the filesystems then commit as they go.
pub fn start() {
    if RUNNING.load(Ordering::Acquire) || STOPPING.load(Ordering::Acquire) {
        return;
    }
    let Some(mut slot) = TASK.try_lock() else { return };
    if slot.is_some() {
        return;
    }
    if WAKE.get().is_none() {
        let Some(event) = Event::new() else {
            klog!(0, "fs: no event for the task that commits in the background: each change commits as it goes");
            return;
        };
        if WAKE.set(event).is_err() {
            return;
        }
    }
    match task::spawn("fs-commit", commit) {
        Some(handle) => {
            *slot = Some(handle);
            /* Only now: a transaction staged before this committed at once,
             * with no task yet to commit it later. */
            RUNNING.store(true, Ordering::Release);
        }
        None => klog!(0, "fs: no task to commit in the background: each change commits as it goes"),
    }
}

/// The task told to stop and waited for: at the halt, before the
/// filesystems are unmounted -- a transaction staged from here on commits
/// at once.
pub fn stop() {
    STOPPING.store(true, Ordering::Release);
    RUNNING.store(false, Ordering::Release);
    let handle = TASK.try_lock().and_then(|mut slot| slot.take());
    if let Some(wake) = WAKE.get() {
        wake.signal();
    }
    /* Dropping the handle waits for the task */
    drop(handle);
}

fn commit() {
    if let Some(wake) = WAKE.get() {
        while !STOPPING.load(Ordering::Acquire) {
            wake.wait_for(Duration::from_millis(LOOK_MS));
            if STOPPING.load(Ordering::Acquire) {
                break;
            }
            if let Some(vfs) = crate::vfs_instance() {
                vfs.commit_due(time::boot_time_ns());
            }
        }
    }
    /* No task from here on, however a start and the stop met: whatever
     * is staged now commits as it goes. */
    RUNNING.store(false, Ordering::Release);
}
