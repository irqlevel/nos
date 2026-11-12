#![no_std]

//! nbs-cs: NBS's chunk server on this host (plans/nbs.md, N7).
//!
//!     insmod /lib/modules/nbs-cs.ko      after nbs-mds.ko, whose metadata it serves
//!     nbscs                              the cs: what it is doing, its devices
//!     nbscs chunks                       the chunks it holds: each one's state, volume, generation
//!     nbscs fences                       the volumes it fences: each one's generation
//!
//! The devices are the metadata's -- `nbs devices add` on this host puts one
//! there -- and the cs takes each in once its host's mds says so: held with
//! a queue of its own (`disk.rs`), claimed as `nbs-cs`, and run by nbscs's
//! engine. A blank one is formatted; one that is NBS's but another cluster's
//! or cs's, or holds something else, is left as it is, said in `nbscs`, and
//! tried again every few seconds.
//! Then the cs serves `nbs.cs` (`serve.rs`): NBS's clients' fences, writes,
//! reads, flushes and closes, checked with this host's mds where the engine
//! has not checked them yet.
//!
//! What a cs keeps is on its devices -- its fences, the table of its slots
//! -- and a cs started again takes it all back from them.

extern crate alloc;

mod disk;
mod serve;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::cmd::{Command as ShellCommand, Output};
use kcore::error::Error;
use kcore::events::SharedEvent;
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use nbscs::{Holding, SlotState};

const HELP: &str = "nbscs [chunks | fences] - NBS's chunk server on this host: its devices, chunks, fences";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");
/// A slot's MiB, as the shell shows a device's room.
const SLOT_MIB: u64 = nbscs::SLOT >> 20;
/// The chunks `nbscs chunks` shows at most.
pub const CHUNKS_SHOWN: usize = 256;

/// A device as the shell shows it.
#[derive(Clone, Default)]
pub struct DeviceShown {
    pub device: u32,
    pub ident: String,
    pub slots: u32,
    pub free: u32,
    pub lost: u32,
    pub failed: bool,
    pub active: bool,
    /// Why it was not taken in.
    pub trouble: Option<String>,
}

/// What the shell shows of the cs, as its task last made it.
#[derive(Default)]
pub struct Shown {
    pub cs: Option<u32>,
    pub devices: Vec<DeviceShown>,
    pub held: u64,
    pub open: u64,
    /// The first CHUNKS_SHOWN of them.
    pub chunks: Vec<Holding>,
    pub fences: Vec<(u32, u64)>,
    pub served: u64,
    pub refused: u64,
    /// Writes passed on along their chains (N13), and those that could not
    /// be.
    pub passed_on: u64,
    pub not_passed: u64,
    /// Answers the channel would not take.
    pub answers_lost: u64,
}

#[derive(Default)]
struct Status {
    state: String,
    shown: Shown,
}

/// Between the cs's task and the shell.
pub struct Shared {
    stop: AtomicBool,
    /// The task's one event: its mailbox's and its queues'.
    pub event: SharedEvent,
    status: Mutex<Status>,
}

impl Shared {
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// What the cs is doing, as the shell says it.
    pub fn say(&self, what: &str) {
        let mut s = self.status.lock();
        if s.state != what {
            s.state.clear();
            s.state.push_str(what);
        }
    }

    /// What the shell shows, made again; the one before dropped off the
    /// lock.
    pub fn show(&self, shown: Shown) {
        let old = core::mem::replace(&mut self.status.lock().shown, shown);
        drop(old);
    }
}

/* Taken down in this order: the command, whose calls the loader has waited
   out; then the task told to stop, woken and waited for -- it flushes the
   devices on its way out, and their queues go with its store */
struct Cs {
    cmd: Option<ShellCommand>,
    shared: Arc<Shared>,
    task: Option<TaskHandle>,
}

impl kmod::Module for Cs {
    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.event.signal();
    }
}

impl Drop for Cs {
    fn drop(&mut self) {
        drop(self.cmd.take());
        self.shared.stop.store(true, Ordering::Release);
        self.shared.event.signal();
        drop(self.task.take());
    }
}

fn init(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    if let Some((key, _)) = params.words().next() {
        let _ = writeln!(out, "nbscs: \"{}\" is not a parameter of nbs-cs's -- it takes none", key);
        return Err(Error::InvalidValue);
    }
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        event: SharedEvent::new().ok_or(Error::NoMemory)?,
        status: Mutex::new(Status { state: String::from("starting"), ..Status::default() }).ok_or(Error::NoMemory)?,
    });
    let task = kcore::task::spawn_with("nbs/cs", shared.clone(), serve::run).ok_or(Error::NoMemory)?;
    let sh = shared.clone();
    let cmd = match ShellCommand::register("nbscs", HELP, move |args, out| show(&sh, args, out)) {
        Ok(c) => c,
        Err(e) => {
            shared.stop.store(true, Ordering::Release);
            shared.event.signal();
            drop(task);
            return Err(e);
        }
    };
    Ok(Box::new(Cs { cmd: Some(cmd), shared, task: Some(task) }))
}

kmod::module!(name: "nbs-cs", init_with_params: init);

fn state_name(s: SlotState) -> &'static str {
    match s {
        SlotState::Free => "free",
        SlotState::Open => "open",
        SlotState::Written => "closed",
        SlotState::Lost => "lost",
    }
}

fn show(shared: &Shared, args: &str, out: &mut Output) {
    let s = shared.status.lock();
    let v = &s.shown;
    match args.trim() {
        "" => {}
        "chunks" => {
            for h in &v.chunks {
                let _ = writeln!(out, "  chunk {}.{} {} volume {} generation {} device {}", h.chunk, h.incarnation,
                                 state_name(h.state), h.volume, h.generation, h.device);
            }
            if v.held > v.chunks.len() as u64 {
                let _ = writeln!(out, "  ... and {} more", v.held - v.chunks.len() as u64);
            }
            return;
        }
        "fences" => {
            for (volume, generation) in &v.fences {
                let _ = writeln!(out, "  volume {} fenced at generation {}", volume, generation);
            }
            return;
        }
        _ => {
            let _ = writeln!(out, "usage: nbscs [chunks | fences]");
            return;
        }
    }
    match v.cs {
        Some(cs) => {
            let _ = writeln!(out, "nbscs: cs {} -- {}: {} chunks held, {} open, {} volumes fenced, {} requests \
                                   served, {} refused, {} writes passed on along chains, {} not, {} answers \
                                   lost", cs, s.state, v.held, v.open, v.fences.len(), v.served, v.refused,
                             v.passed_on, v.not_passed, v.answers_lost);
        }
        None => {
            let _ = writeln!(out, "nbscs: {}", s.state);
        }
    }
    for d in &v.devices {
        match &d.trouble {
            Some(why) => {
                let _ = writeln!(out, "  device {:<4} {:<40} not taken: {}", d.device, d.ident, why);
            }
            None => {
                let what = if d.failed { "failed" } else if d.active { "active" } else { "draining" };
                let _ = writeln!(out, "  device {:<4} {:<40} {} MiB, {} MiB free, {} slots lost, {}", d.device, d.ident,
                                 u64::from(d.slots) * SLOT_MIB, u64::from(d.free) * SLOT_MIB, d.lost, what);
            }
        }
    }
}
