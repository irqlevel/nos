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
//!     hv boot /bzImage initrd=/initrd secs=60   -- a Linux guest, for a while
//!     hv start /bzImage initrd=/initrd          -- one that runs until stopped
//!     hv exec 0 uname -a                        -- a line typed at its shell
//!     hv stop 0
//!     hv off
//!     rmmod hv         -- stops every guest still running first
//!
//! The built-in guests are a few bytes each (`hv::guests`); the Linux ones
//! are x86-64 only, so far (`plans/03-hypervisor.md`).

extern crate alloc;

#[cfg(target_arch = "x86_64")]
mod boot;
#[cfg(target_arch = "x86_64")]
mod guest;
#[cfg(target_arch = "x86_64")]
mod vms;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use hv::Machine;
use kcore::cmd::{Command, Output};
use kcore::consts::MAX_CPUS;
use kcore::sync::Mutex;

const HELP: &str = "hv [info|on|off|run|boot|start|list|console|attach|send|exec|wait|restart|stop|help] - the CPU's virtualization extension, and guests under it";

/// What `hv help` prints: every subcommand, a line each.
const USAGE: &str = "\
hv [info]                          what the CPU has, and which CPUs the extension is on for
hv on|off [cpu|all]                turn the extension on or off
hv run <guest|all> [cpu]           run the built-in guests
hv boot <bzImage> [mem=MiB] [secs=N] [cpu=N] [initrd=path] [disk=path]... [input=...] [cmdline=...]
                                   a Linux guest for secs, then its console and how it ended
hv start <bzImage> [mem=MiB] [cpu=N] [initrd=path] [disk=path]... [input=...] [log] [restart] [cmdline=...]
                                   a Linux guest that runs until hv stop; restart boots it
                                   again when it resets itself
hv list                            the started guests
hv console <id> [bytes=N]          the end of one's console
hv attach <id>                     its console, live, typed at -- ^] detaches (ssh -t)
hv send <id> <text>                type at it (\\n for a newline)
hv exec <id> [secs=N] <line>       type a line and print the answer, up to the next prompt
hv wait <id> [secs=N] <text>       until its console shows text, or it stops
hv restart <id>                    boot it again from its files, running or stopped
hv stop <id|all>                   stop it, say how it ended, take it off the list
";

/// What the command works on: the machine, and the guests started on it.
struct State {
    machine: Arc<Machine>,
    #[cfg(target_arch = "x86_64")]
    vms: vms::Vms,
    /// Set as the module starts to go: an `hv boot` running then stops at
    /// once instead of at the end of its time.
    #[cfg(target_arch = "x86_64")]
    unloading: Arc<core::sync::atomic::AtomicBool>,
}

struct Hv {
    /// In an `Option` so that unregistering -- which waits out a call of the
    /// command still running on another CPU -- happens before the machine is
    /// let go of, and not in whatever order the fields happen to be in.
    cmd: Option<Command>,
    state: Arc<State>,
}

impl kmod::Module for Hv {}

impl Drop for Hv {
    fn drop(&mut self) {
        /* The guests first: an `hv boot` running, and every `hv start`ed one
         * -- so that an `hv wait` or `hv exec` waiting on one returns, and it
         * is those calls the unregister below waits for. */
        #[cfg(target_arch = "x86_64")]
        {
            self.state.unloading.store(true, core::sync::atomic::Ordering::Release);
            self.state.vms.close();
        }

        /* Then the command: `kernel_cmd_unregister` returns only once any
         * call of it still running has, so from here nothing can ask the
         * machine anything -- nor start a guest on it, since `close` turned
         * every later `hv start` away. */
        drop(self.cmd.take());

        /* Then the CPUs, here rather than in the machine's own drop, so that
         * what is left can be read back afterwards and said out loud. A
         * hypervisor module that goes and leaves the extension on is the one
         * failure this shape must not have: the code that would turn it off
         * has just been freed. */
        let machine = &self.state.machine;
        let was = machine.disable(u64::MAX);
        let left = machine.hardware_mask();
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
    let state = Arc::new(State {
        machine: machine.clone(),
        #[cfg(target_arch = "x86_64")]
        vms: vms::Vms::new().ok_or(kcore::error::Error::NoMemory)?,
        #[cfg(target_arch = "x86_64")]
        unloading: Arc::new(core::sync::atomic::AtomicBool::new(false)),
    });

    let for_cmd = state.clone();
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

    Ok(Box::new(Hv { cmd: Some(cmd), state }))
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

fn command(state: &State, args: &str, out: &mut Output) {
    let machine = &state.machine;
    /* The subcommand, and what follows it spaces and all: a command line, the
     * text to type at a guest. Split where `char` says, so that both halves
     * start on a character. */
    let args = args.trim_start();
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    let (first, rest) = (&args[..end], args[end..].trim_start());
    let mut words = rest.split_ascii_whitespace();
    match first {
        "" => status(machine, out),
        "info" => {
            let _ = machine.caps().report(out);
            status(machine, out);
        }
        "help" => {
            let _ = write!(out, "{}", USAGE);
        }
        "on" => switch(state, words.next(), true, out),
        "off" => switch(state, words.next(), false, out),
        "run" => run(machine, words.next(), words.next(), out),
        #[cfg(target_arch = "x86_64")]
        "boot" => boot::boot(machine, rest, &state.vms.load(), &state.unloading, out),
        #[cfg(target_arch = "x86_64")]
        "start" => state.vms.start(machine, rest, out),
        #[cfg(target_arch = "x86_64")]
        "list" => state.vms.list(out),
        #[cfg(target_arch = "x86_64")]
        "console" => state.vms.console(rest, out),
        #[cfg(target_arch = "x86_64")]
        "send" => state.vms.send(rest, out),
        #[cfg(target_arch = "x86_64")]
        "exec" => state.vms.exec(rest, out),
        #[cfg(target_arch = "x86_64")]
        "wait" => state.vms.wait(rest, out),
        #[cfg(target_arch = "x86_64")]
        "attach" => state.vms.attach(rest, out),
        #[cfg(target_arch = "x86_64")]
        "restart" => state.vms.restart(rest, out),
        #[cfg(target_arch = "x86_64")]
        "stop" => state.vms.stop(rest, out),
        #[cfg(not(target_arch = "x86_64"))]
        "boot" | "start" | "list" | "console" | "attach" | "send" | "exec" | "wait" | "restart" | "stop" => {
            let _ = writeln!(out, "hv: no Linux guest on this architecture yet");
        }
        other => {
            let _ = writeln!(out, "hv: no such thing as \"{}\"", other);
            let _ = write!(out, "{}", USAGE);
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
/// in nothing else worth writing twice -- but for a guest `hv start` left
/// running, which `hv off` will not turn the extension off under.
fn switch(state: &State, which: Option<&str>, on: bool, out: &mut Output) {
    let machine = &*state.machine;
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

    /* A courtesy, not what keeps the CPU safe: a guest started between this
     * look and the switch finds the extension off at its next entry, which
     * checks with interrupts off, and stops there -- "not entered" in its
     * `hv list` line. */
    #[cfg(target_arch = "x86_64")]
    if !on {
        if let Some((id, cpu)) = state.vms.running_on(mask) {
            let _ = writeln!(out, "hv: vm {} is running on cpu {} -- hv stop it first", id, cpu);
            return;
        }
    }

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
