//! `hv boot`: load a Linux `bzImage`, run it on a vCPU for a set time, and
//! print its console and how it ended -- the one-shot form of `hv start`,
//! which a gate can run from a script and read back whole.
//!
//! The guest is built from its files (`guest::build`) and run on a task of its
//! own, bound to a CPU the extension is on for, until it stops for good -- a
//! HLT with interrupts off, a triple fault, a touch of memory this hypervisor
//! does not emulate -- or its time runs out.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use hv::run::Host;
use hv::Machine;
use kcore::cmd::Output;
use kcore::consts::{MAX_CPUS, NS_PER_SEC};
use kcore::sync::Mutex;

use crate::guest::{self, LogLine, Ring, Spec};

const USAGE: &str = "hv boot <bzImage> [mem=MiB] [secs=N] [cpu=N] [initrd=path] [disk=path[:ro]]... [input=...] [cmdline=...]";
/// How long the guest runs by default, and at most: a Linux boot under TCG,
/// itself under this hypervisor, is very far from quick.
const DEFAULT_SECS: u64 = 60;
const MAX_SECS: u64 = 600;

/// The console of a `hv boot` guest: kept whole for the report, streamed to
/// the kernel log a line at a time -- so it is on nos's own console live, and
/// on a machine whose only console is the network that is where a guest's
/// boot is watched -- and typed at from `input=`.
struct BootHost {
    ring: Ring,
    line: LogLine,
    input: Vec<u8>,
    fed: usize,
    /// The module is going: stop now rather than at the end of `secs`, which
    /// the unload would otherwise wait out.
    unloading: Arc<AtomicBool>,
}

impl Host for BootHost {
    fn output(&mut self, byte: u8) {
        self.ring.push(byte);
        if self.line.push(byte) {
            kcore::trace!(0, "hvguest| {}", self.line.text());
            self.line.clear();
        }
    }

    fn input(&mut self, at_prompt: bool) -> Option<u8> {
        if !at_prompt {
            return None;
        }
        let byte = *self.input.get(self.fed)?;
        self.fed += 1;
        Some(byte)
    }

    fn stop_requested(&mut self) -> bool {
        self.unloading.load(Ordering::Acquire)
    }
}

/// The vCPU task's context: the guest to build and run, and where its verdict
/// goes.
struct Boot {
    machine: Arc<Machine>,
    spec: Spec,
    report: Mutex<String>,
    unloading: Arc<AtomicBool>,
}

impl Boot {
    fn run(self: Arc<Boot>) {
        let mut report = String::new();
        let mut guest = match guest::build(&self.machine, &self.spec) {
            Ok(guest) => guest,
            Err(why) => {
                let _ = writeln!(report, "hv: boot failed -- {}", why);
                *self.report.lock() = report;
                return;
            }
        };
        /* Everything the host needs is taken now, fallibly, so that the loop
         * allocates nothing as the guest writes. */
        let mut input = Vec::new();
        let reserved = input.try_reserve_exact(self.spec.input.len()).is_ok();
        let (Some(ring), Some(line), true) = (Ring::new(), LogLine::new(), reserved) else {
            let _ = writeln!(report, "hv: boot failed -- out of memory for its console");
            *self.report.lock() = report;
            return;
        };
        input.extend_from_slice(&self.spec.input);
        let mut host = BootHost { ring, line, input, fed: 0, unloading: self.unloading.clone() };

        let _ = writeln!(report, "hv: booting {} -- {} MiB, cmdline \"{}\"",
                         self.spec.kernel, self.spec.mem_bytes / (1024 * 1024), self.spec.cmdline);

        let secs = self.spec.secs.unwrap_or(DEFAULT_SECS);
        let start = kcore::time::boot_time_ns();
        let (stop, counts) = guest.run(&self.machine, secs * NS_PER_SEC, &mut host);
        let run_ns = kcore::time::boot_time_ns().saturating_sub(start);
        if !host.line.text().is_empty() {
            kcore::trace!(0, "hvguest| {}", host.line.text());
        }

        if !host.input.is_empty() {
            let _ = writeln!(report, "  input      {} of {} bytes taken by the guest (uart IER {:#04x})",
                             host.fed, host.input.len(), guest.uart_ier());
        }
        write_console(&mut report, &host.ring);
        guest::report(&mut report, &guest, &stop, &counts, run_ns);
        *self.report.lock() = report;
    }
}

/// The guest's console, made safe to print, between two markers a script can
/// find.
fn write_console(report: &mut String, ring: &Ring) {
    let mut raw = Vec::new();
    let mut text = Vec::new();
    if ring.total() == 0 {
        let _ = writeln!(report, "  the guest printed nothing to ttyS0");
        return;
    }
    if !ring.since(0, guest::CONSOLE_BYTES, &mut raw) || !guest::sanitize(&raw, &mut text) {
        let _ = writeln!(report, "  the guest's console could not be copied: out of memory");
        return;
    }
    let _ = writeln!(report, "  --- ttyS0 ---");
    let _ = writeln!(report, "{}", String::from_utf8_lossy(&text).trim_end());
    let _ = writeln!(report, "  --- end ttyS0 ---");
}

/// `hv boot ...`: build the job, run it on a vCPU task of its own, and print
/// what it reported.
pub fn boot(machine: &Arc<Machine>, args: &str, busy: &[u32; MAX_CPUS], unloading: &Arc<AtomicBool>,
            out: &mut Output) {
    if let Err(e) = hv::run::ensure_runnable(machine) {
        let _ = writeln!(out, "hv: no guest can run here -- {}", e);
        return;
    }
    let spec = match guest::parse(args, USAGE) {
        Ok(spec) if spec.log => {
            let _ = writeln!(out, "hv: log is hv start's -- hv boot sends the console to the kernel log always");
            return;
        }
        Ok(spec) if spec.restart => {
            let _ = writeln!(out, "hv: restart is hv start's -- a hv boot guest runs once");
            return;
        }
        Ok(spec) if spec.net => {
            let _ = writeln!(out, "hv: net is hv start's");
            return;
        }
        Ok(spec) if spec.secs.map_or(true, |s| (1..=MAX_SECS).contains(&s)) => spec,
        Ok(_) => {
            let _ = writeln!(out, "hv: secs= must be 1..{}", MAX_SECS);
            return;
        }
        Err(usage) => {
            let _ = writeln!(out, "{}", usage);
            return;
        }
    };
    let cpu = match guest::pick_cpu(machine, spec.cpu, busy) {
        Ok(cpu) => cpu,
        Err(why) => {
            let _ = writeln!(out, "hv: {}", why);
            return;
        }
    };

    let report = match Mutex::new(String::new()) {
        Some(report) => report,
        None => {
            let _ = writeln!(out, "hv: out of memory");
            return;
        }
    };
    let job = Arc::new(Boot { machine: machine.clone(), spec, report, unloading: unloading.clone() });
    /* On a task of its own, bound to that CPU, so a long boot does not sit
     * on the shell's stack; dropping the handle waits for it. */
    match kcore::task::spawn_on_with("hv/linux", 1u64 << cpu, job.clone(), Boot::run) {
        Some(task) => drop(task),
        None => {
            let _ = writeln!(out, "hv: no task for the guest");
            return;
        }
    }
    out.write_bytes(job.report.lock().as_bytes());
}
