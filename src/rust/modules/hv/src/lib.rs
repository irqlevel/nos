#![no_std]

//! The hypervisor, as a module that can be put in and taken out of a running
//! kernel.
//!
//! Why a module rather than part of the image: a machine that runs no guests
//! should carry none of this -- not the code, not the per-CPU pages, and
//! above all not the CPU left in a state it did not boot in. And a
//! hypervisor is the subsystem it is least pleasant to debug by rebooting:
//! `insmod`, try it, `rmmod`, change one field, `insmod` again is a loop of
//! seconds where a rebuild and a boot is a loop of minutes.
//!
//! What that costs is stated plainly in [`docs/hypervisor.md`]: a module
//! cannot register a block or a net driver, so a guest's virtual disks and
//! NICs will be served by this module over the kernel's own, never by
//! registering new ones.
//!
//!     insmod /hv.ko
//!     hv info          -- what this machine has
//!     hv on            -- turn the extension on
//!     hv run all       -- run the built-in guests, each in a VM of its own
//!     hv off
//!     rmmod hv
//!
//! The guests are a few bytes each (`hv::guests`); a Linux one is what
//! comes next (`plans/03-hypervisor.md`).

extern crate alloc;

#[cfg(target_arch = "x86_64")]
mod boot;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use hv::Machine;
use kcore::cmd::{Command, Output};
use kcore::consts::MAX_CPUS;
use kcore::sync::Mutex;

const HELP: &str = "hv [info|on|off [cpu|all]|run <guest|all> [cpu]|boot <bzImage> [mem=MiB] [secs=N] [initrd=path] [cmdline=...]] - the CPU's virtualization extension, and guests under it";

struct Hv {
    /// In an `Option` so that unregistering -- which waits out a call of the
    /// command still running on another CPU -- happens before the machine is
    /// let go of, and not in whatever order the fields happen to be in.
    cmd: Option<Command>,
    machine: Arc<Machine>,
}

impl kmod::Module for Hv {}

impl Drop for Hv {
    fn drop(&mut self) {
        /* The command goes first: `kernel_cmd_unregister` returns only once
         * any call of it still running has, so from here nothing can ask the
         * machine anything. */
        drop(self.cmd.take());

        /* Then the CPUs, here rather than in the machine's own drop, so that
         * what is left can be read back afterwards and said out loud. A
         * hypervisor module that goes and leaves the extension on is the one
         * failure this shape must not have: the code that would turn it off
         * has just been freed. */
        let was = self.machine.disable(u64::MAX);
        let left = self.machine.hardware_mask();
        if left != 0 {
            kcore::trace!(0, "hv: WARNING -- unloading with the extension still on for cpu mask 0x{:x}", left);
        }
        kcore::trace!(0, "hv: unloaded, extension off for cpu mask 0x{:x}", was);
    }
}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    /* A machine with no virtualization still gets a `Machine` and still
     * loads: `hv info` is a diagnostic, and the machine that most needs one
     * is the machine where a guest will not start. */
    let machine = Arc::new(Machine::new().map_err(kernel_error)?);

    let for_cmd = machine.clone();
    let cmd = Command::register("hv", HELP, move |args, out| command(&for_cmd, args, out))?;

    match machine.ext() {
        Ok(ext) => {
            /* Nothing else in this kernel turns the extension on, so a CPU
             * that has it on at load was left that way by a module that did
             * not clean up -- worth a line, since the pages it was using are
             * gone and the next `hv on` would hand it different ones. */
            let left_on = machine.hardware_mask();
            if left_on != 0 {
                kcore::trace!(0, "hv: WARNING -- already on for cpu mask 0x{:x} before this load", left_on);
            }
            kcore::trace!(0, "hv: loaded -- {}, ready", ext.vendor().name());
        }
        Err(e) => kcore::trace!(0, "hv: loaded -- no guest can run here: {}", e),
    }

    Ok(Box::new(Hv { cmd: Some(cmd), machine }))
}

/// An `hv::Error` as the kernel's, for the one place the two meet: what
/// `insmod` gets back when the module will not start. The hypervisor's own
/// reasons are richer than the kernel's eight codes and are what the report
/// prints; this is only the word `insmod` prints.
fn kernel_error(e: hv::Error) -> kcore::error::Error {
    match e {
        hv::Error::NoMemory => kcore::error::Error::NoMemory,
        hv::Error::NoSuchCpu => kcore::error::Error::InvalidValue,
        _ => kcore::error::Error::DeviceError,
    }
}

fn command(machine: &Arc<Machine>, args: &str, out: &mut Output) {
    let mut words = args.split_ascii_whitespace();
    match words.next() {
        None => status(machine, out),
        Some("info") => {
            let _ = machine.caps().report(out);
            status(machine, out);
        }
        Some("on") => switch(machine, words.next(), true, out),
        Some("off") => switch(machine, words.next(), false, out),
        Some("run") => run(machine, words.next(), words.next(), out),
        #[cfg(target_arch = "x86_64")]
        Some("boot") => boot::boot(machine, args.strip_prefix("boot").unwrap_or("").trim_start(), out),
        #[cfg(not(target_arch = "x86_64"))]
        Some("boot") => {
            let _ = writeln!(out, "hv: no Linux guest on this architecture yet");
        }
        Some(other) => {
            let _ = writeln!(out, "hv: no such thing as \"{}\"", other);
            let _ = writeln!(out, "{}", HELP);
        }
    }
}

fn status(machine: &Machine, out: &mut Output) {
    match machine.ext() {
        Ok(ext) => {
            /* What the CPUs say, not what this module remembers telling
             * them: the two can only differ through a bug here, and the
             * whole point of asking is to see it. */
            let hardware = machine.hardware_mask();
            let _ = write!(out, "hv: {} on for cpu ", ext.vendor().name());
            write_mask(out, hardware);
            let _ = write!(out, " of ");
            write_mask(out, kcore::cpu::online_mask());
            let _ = writeln!(out);

            /* Each way round says something different, so each is said
             * on its own, naming the CPUs. */
            let ours = machine.enabled_mask();
            let stray = hardware & !ours;
            if stray != 0 {
                let _ = write!(out, "hv: WARNING -- on for cpu ");
                write_mask(out, stray);
                let _ = writeln!(out, " without a page from this module: left on by an earlier load? hv off clears it");
            }
            let lost = ours & !hardware;
            if lost != 0 {
                let _ = write!(out, "hv: WARNING -- off for cpu ");
                write_mask(out, lost);
                let _ = writeln!(out, " though this module turned it on: something turned it off behind its back");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "hv: no guest can run here -- {}", e);
        }
    }
}

/// Guests to run, and where what they did is written: what a vCPU's task
/// shares with the command that is waiting for it.
struct Job {
    machine: Arc<Machine>,
    guests: Vec<&'static str>,
    report: Mutex<String>,
}

impl Job {
    /// The vCPU's task: every guest of the job in turn, on whichever CPU
    /// the task was bound to.
    fn run(job: Arc<Job>) {
        let mut report = String::new();
        let mut ok = 0usize;
        for name in &job.guests {
            if hv::guests::run_one(&job.machine, name, &mut report) == Some(true) {
                ok += 1;
            }
        }
        if job.guests.len() > 1 {
            let _ = writeln!(report, "hv: {} of {} guests ok", ok, job.guests.len());
        }
        *job.report.lock() = report;
    }
}

/// `hv run <guest|all> [cpu]`: each guest in a VM of its own, on a task of
/// its own -- a guest's CPU is a task, here as it will be for a Linux one --
/// bound to `cpu` when one is named, and waited for.
fn run(machine: &Arc<Machine>, which: Option<&str>, cpu: Option<&str>, out: &mut Output) {
    /* First, so that a machine with nothing to run a guest under says that,
     * and not that it has no such guest. */
    if let Err(e) = machine.ext() {
        let _ = writeln!(out, "hv: no guest can run here -- {}", e);
        return;
    }
    let guests: Vec<&'static str> = match which {
        Some("all") => hv::guests::names().collect(),
        Some(name) => hv::guests::names().filter(|g| *g == name).collect(),
        None => Vec::new(),
    };
    if guests.is_empty() {
        let _ = write!(out, "hv: run which guest? all");
        for name in hv::guests::names() {
            let _ = write!(out, ", {}", name);
        }
        let _ = writeln!(out);
        return;
    }

    let affinity = match cpu {
        None => None,
        Some(word) => match word.parse::<usize>() {
            Ok(cpu) if cpu < MAX_CPUS && kcore::cpu::online_mask() & (1u64 << cpu) != 0 => Some(1u64 << cpu),
            _ => {
                let _ = writeln!(out, "hv: \"{}\" is not a running CPU", word);
                return;
            }
        },
    };

    let report = match Mutex::new(String::new()) {
        Some(report) => report,
        None => {
            let _ = writeln!(out, "hv: out of memory");
            return;
        }
    };
    let job = Arc::new(Job { machine: machine.clone(), guests, report });
    let task = match affinity {
        Some(mask) => kcore::task::spawn_on_with("hv/vcpu", mask, job.clone(), Job::run),
        None => kcore::task::spawn_with("hv/vcpu", job.clone(), Job::run),
    };
    match task {
        /* Dropping the handle waits for the task. */
        Some(task) => drop(task),
        None => {
            let _ = writeln!(out, "hv: no task for the guest");
            return;
        }
    }
    out.write_bytes(job.report.lock().as_bytes());
}

/// `hv on [cpu|all]` and `hv off [cpu|all]`, which differ in one word and
/// in nothing else worth writing twice.
fn switch(machine: &Machine, which: Option<&str>, on: bool, out: &mut Output) {
    let mask = match which {
        None | Some("all") => kcore::cpu::online_mask(),
        Some(word) => match word.parse::<usize>() {
            Ok(cpu) if cpu < MAX_CPUS => 1u64 << cpu,
            _ => {
                let _ = writeln!(out, "hv: \"{}\" is not a CPU -- a number under {}, or all",
                                 word, MAX_CPUS);
                return;
            }
        },
    };

    let changed = if on {
        match machine.enable(mask) {
            Ok(changed) => changed,
            Err(refused) => {
                let _ = match refused.cpu {
                    Some(cpu) => writeln!(out, "hv: cpu {} would not take it -- {}", cpu, refused.error),
                    None => writeln!(out, "hv: cannot turn it on -- {}", refused.error),
                };
                /* Whatever did come on before the CPU that refused stays on,
                 * and the status line below says which. */
                status(machine, out);
                return;
            }
        }
    } else {
        machine.disable(mask)
    };

    let _ = write!(out, "hv: turned {} for cpu ", if on { "on" } else { "off" });
    write_mask(out, changed);
    let _ = writeln!(out);
    status(machine, out);
}

/// A CPU mask as people read one: `0-3`, `0,2-5`, or `none`.
fn write_mask(out: &mut Output, mask: u64) {
    if mask == 0 {
        let _ = write!(out, "none");
        return;
    }

    let mut first = true;
    let mut i = 0usize;
    while i < u64::BITS as usize {
        if mask & (1u64 << i) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < u64::BITS as usize && mask & (1u64 << i) != 0 {
            i += 1;
        }
        let _ = write!(out, "{}", if first { "" } else { "," });
        first = false;
        if start == i - 1 {
            let _ = write!(out, "{}", start);
        } else {
            let _ = write!(out, "{}-{}", start, i - 1);
        }
    }
}

kmod::module!(name: "hv", init: init);
