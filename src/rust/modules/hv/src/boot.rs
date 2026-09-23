//! `hv boot`: load a Linux `bzImage` and run it on a vCPU, its early console
//! coming back as the command's output.
//!
//! The kernel and the initrd are read from a file and streamed straight into
//! the guest's physical memory a chunk at a time -- neither has to fit in one
//! allocation, which is why they are frames and read positionally. Then the
//! zero page, the command line, the memory map and the guest's page tables go
//! in (`hv::linux`), and the vCPU runs until it halts, faults on memory this
//! hypervisor does not emulate (a local APIC, next), or runs out of time.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use hv::linux::Header;
use hv::run::{LinuxGuest, Stop};
use hv::Machine;
use kcore::cmd::Output;
use kcore::sync::Mutex;

/// How much of the file is read at once when streaming it into guest memory.
const CHUNK: usize = 64 * 1024;
/// The default guest RAM, and the least it may be given.
const DEFAULT_MEM_MIB: u64 = 256;
const MIN_MEM_MIB: u64 = 64;
const MAX_MEM_MIB: u64 = 4096;
/// How long a guest may run before the host stops it, by default and at
/// most: a Linux early boot under TCG, itself under this hypervisor, is very
/// far from quick -- the decompressor alone is millions of instructions
/// twice emulated.
const DEFAULT_BUDGET_S: u64 = 60;
const MAX_BUDGET_S: u64 = 600;
/// The default command line: an early serial console, and nothing that wants
/// a timer or an APIC this hypervisor does not emulate yet.
const DEFAULT_CMDLINE: &str = "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check";

/// What `hv boot` was asked to do.
struct Request {
    kernel: String,
    initrd: Option<String>,
    mem_bytes: u64,
    budget_s: u64,
    cmdline: String,
    /// Bytes typed at the guest's console once it is up; `\n` in the word
    /// stands for a newline.
    input: Option<String>,
}

fn parse(args: &str) -> core::result::Result<Request, String> {
    let mut kernel = None;
    let mut initrd = None;
    let mut mem_mib = DEFAULT_MEM_MIB;
    let mut budget_s = DEFAULT_BUDGET_S;
    let mut cmdline: Option<String> = None;
    let mut input: Option<String> = None;

    for word in args.split_ascii_whitespace() {
        if let Some(mib) = word.strip_prefix("mem=") {
            mem_mib = mib.parse::<u64>().map_err(|_| String::from("mem= wants a number of MiB"))?;
        } else if let Some(path) = word.strip_prefix("initrd=") {
            initrd = Some(String::from(path));
        } else if let Some(secs) = word.strip_prefix("secs=") {
            budget_s = secs.parse::<u64>().map_err(|_| String::from("secs= wants a number of seconds"))?;
        } else if let Some(text) = word.strip_prefix("input=") {
            input = Some(String::from(text));
        } else if let Some(rest) = word.strip_prefix("cmdline=") {
            /* Everything after cmdline= to the end of the line is the
             * command line, spaces and all. */
            let at = args.find("cmdline=").expect("just matched") + "cmdline=".len();
            cmdline = Some(String::from(&args[at..]));
            let _ = rest;
            break;
        } else if kernel.is_none() {
            kernel = Some(String::from(word));
        }
    }

    let kernel = kernel.ok_or_else(|| String::from("hv boot <bzImage> [mem=MiB] [secs=N] [initrd=path] [input=...] [cmdline=...]"))?;
    if !(MIN_MEM_MIB..=MAX_MEM_MIB).contains(&mem_mib) {
        return Err(alloc::format!("mem= must be {}..{} MiB", MIN_MEM_MIB, MAX_MEM_MIB));
    }
    if budget_s == 0 || budget_s > MAX_BUDGET_S {
        return Err(alloc::format!("secs= must be 1..{}", MAX_BUDGET_S));
    }
    Ok(Request {
        kernel,
        initrd,
        mem_bytes: mem_mib * 1024 * 1024,
        budget_s,
        cmdline: cmdline.unwrap_or_else(|| String::from(DEFAULT_CMDLINE)),
        input,
    })
}

/// Copy `len` bytes of the file at `path` into guest memory at `gpa`, a chunk
/// at a time. The whole file, from `offset` on, must be `len` bytes or more.
fn stream(guest: &mut LinuxGuest, path: &str, offset: u64, gpa: u64, len: u64) -> core::result::Result<(), String> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(CHUNK).map_err(|_| String::from("out of memory for a read buffer"))?;
    buf.resize(CHUNK, 0);

    let mut done = 0u64;
    while done < len {
        let want = ((len - done) as usize).min(CHUNK);
        let got = kcore::fs::read_at(path, offset + done, &mut buf[..want])
            .map_err(|e| alloc::format!("reading {}: {}", path, e))?;
        if got == 0 {
            return Err(alloc::format!("{} ended {} bytes short", path, len - done));
        }
        guest
            .memory_mut()
            .write(gpa + done, &buf[..got])
            .map_err(|_| alloc::format!("guest has no memory at {:#x}", gpa + done))?;
        done += got as u64;
    }
    Ok(())
}

/// Build the guest and load the kernel and initrd into it.
fn build(machine: &Machine, req: &Request) -> core::result::Result<LinuxGuest, String> {
    let mut guest = LinuxGuest::new(machine, req.mem_bytes).map_err(|e| alloc::format!("no guest: {}", e))?;

    /* The header is in the first page or two; read enough to parse it. */
    let mut first = Vec::new();
    first.try_reserve_exact(2 * kcore::consts::PAGE_SIZE).map_err(|_| String::from("out of memory"))?;
    first.resize(2 * kcore::consts::PAGE_SIZE, 0);
    let got = kcore::fs::read_at(&req.kernel, 0, &mut first)
        .map_err(|e| alloc::format!("reading {}: {}", req.kernel, e))?;
    first.truncate(got);
    let header = Header::parse(&first).map_err(|_| alloc::format!("{} is not a 64-bit bzImage", req.kernel))?;

    let kernel_size = kcore::fs::size(&req.kernel).map_err(|e| alloc::format!("{}: {}", req.kernel, e))?;
    let pm_offset = header.pm_offset();
    if pm_offset >= kernel_size {
        return Err(String::from("the image is shorter than its own setup"));
    }
    let kernel_len = kernel_size - pm_offset;

    let initrd_len = match &req.initrd {
        Some(path) => kcore::fs::size(path).map_err(|e| alloc::format!("{}: {}", path, e))?,
        None => 0,
    };

    let layout = hv::linux::plan(&header, req.mem_bytes, kernel_len, initrd_len)
        .map_err(|e| alloc::format!("the guest's memory does not fit its kernel: {}", e))?;

    /* The 64-bit kernel at its load address, then the initrd high. */
    stream(&mut guest, &req.kernel, pm_offset, layout.kernel_addr, kernel_len)?;
    if let Some(path) = &req.initrd {
        stream(&mut guest, path, 0, layout.initrd_addr, layout.initrd_len)?;
    }

    guest
        .load(&header, &first, layout, req.cmdline.as_bytes())
        .map_err(|e| alloc::format!("laying out the guest: {}", e))?;
    if let Some(text) = &req.input {
        /* `\n` in the word for a newline, so a whole command line fits one
         * token. */
        let bytes: alloc::vec::Vec<u8> = text.replace("\\n", "\n").into_bytes();
        guest.set_input(&bytes);
    }
    Ok(guest)
}

/// The vCPU task's context: the guest to build and run, and where its console
/// and verdict go.
pub struct Boot {
    machine: Arc<Machine>,
    req: Request,
    report: Mutex<String>,
}

impl Boot {
    fn run(self: Arc<Boot>) {
        let mut report = String::new();
        let mut guest = match build(&*self.machine, &self.req) {
            Ok(guest) => guest,
            Err(why) => {
                let _ = writeln!(report, "hv: boot failed -- {}", why);
                *self.report.lock() = report;
                return;
            }
        };

        let _ = writeln!(report, "hv: booting {} -- {} MiB, cmdline \"{}\"",
                         self.req.kernel, self.req.mem_bytes / (1024 * 1024), self.req.cmdline);

        /* Stream the guest's console to the kernel log a line at a time as
         * it comes, so it is on nos's own console live -- on a machine whose
         * only console is the network, that is where a guest's early boot is
         * watched -- and not only in the report at the end. */
        let mut line = String::new();
        let budget_ns = self.req.budget_s * kcore::consts::NS_PER_SEC;
        let (stop, counts) = guest.run(&*self.machine, budget_ns, |byte| {
            if byte == b'\n' {
                kcore::trace!(0, "hvguest| {}", line);
                line.clear();
            } else if byte != b'\r' && line.len() < 256 {
                line.push(byte as char);
            }
        });
        if !line.is_empty() {
            kcore::trace!(0, "hvguest| {}", line);
        }

        /* The guest's console, whatever came of it. */
        let (fed, total) = guest.input_progress();
        if total > 0 {
            let _ = writeln!(report, "  input      {} of {} bytes taken by the guest (uart IER {:#04x})", fed, total, guest.uart_ier());
        }
        let console = guest.output();
        if console.is_empty() {
            let _ = writeln!(report, "  the guest printed nothing to ttyS0");
        } else {
            let _ = writeln!(report, "  --- ttyS0 ---");
            let _ = writeln!(report, "{}", console.trim_end());
            let _ = writeln!(report, "  --- end ttyS0 ---");
        }

        let _ = write!(report, "  stopped    ");
        let _ = match stop {
            Stop::Halted { rip } => writeln!(report, "hlt at {:#x}", rip),
            Stop::Mmio { gpa, rip } => writeln!(report,
                "a read/write of guest physical {:#x} with no memory (an unemulated device?), rip {:#x}", gpa, rip),
            Stop::Shutdown { rip } => writeln!(report, "triple fault at {:#x}", rip),
            Stop::Exception { vector, rip } => writeln!(report, "exception {} at {:#x}", vector, rip),
            Stop::Refused(r) => writeln!(report, "not entered: {:?}", r),
            Stop::Invalid => writeln!(report, "VMEXIT_INVALID -- the CPU refused the VMCB"),
            Stop::Budget => writeln!(report, "by the host, after {} s", self.req.budget_s),
            Stop::Unexpected { exit, rip } => writeln!(report, "an exit with no handler: {:?} at {:#x}", exit, rip),
        };
        let _ = writeln!(report,
            "  exits      {} total: {} port in, {} port out, {} cpuid, {} rdmsr, {} wrmsr ({} #GP), {} irq, {} hlt, {} host",
            counts.exits, counts.port_in, counts.port_out, counts.cpuid,
            counts.msr_read, counts.msr_write, counts.msr_gp, counts.irq, counts.hlt, counts.host);
        let ((irr, isr, imr), (m0, r0, run0)) = guest.irq_debug();
        let _ = writeln!(report, "  irq        {} total ({} timer, {} serial), {} edges, {} blocked; PIC irr {:#04x} isr {:#04x} imr {:#04x}; PIT ch0 mode {} reload {} run {}",
            counts.irq, counts.irq0, counts.irq4, counts.edges0, counts.blocked, irr, isr, imr, m0, r0, run0);
        let hot = guest.hot_ports(6);
        if !hot.is_empty() {
            let _ = write!(report, "  busiest in ports");
            for (port, count) in hot {
                let _ = write!(report, " {:#x}:{}", port, count);
            }
            let _ = writeln!(report);
        }
        if !matches!(stop, Stop::Halted { .. } | Stop::Budget) {
            let _ = guest.dump(&mut report);
        }

        *self.report.lock() = report;
    }
}

/// `hv boot ...`: build the job, run it on a vCPU task of its own, and print
/// what it reported.
pub fn boot(machine: &Arc<Machine>, args: &str, out: &mut Output) {
    if let Err(e) = hv::run::ensure_runnable(machine) {
        let _ = writeln!(out, "hv: no guest can run here -- {}", e);
        return;
    }
    let req = match parse(args) {
        Ok(req) => req,
        Err(usage) => {
            let _ = writeln!(out, "{}", usage);
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
    /* The vCPU has to run where the extension is on: bind it to the lowest
     * such CPU, so it does not land on one `hv on` never touched. */
    let enabled = machine.enabled_mask();
    if enabled == 0 {
        let _ = writeln!(out, "hv: the extension is on for no CPU -- hv on first");
        return;
    }
    let cpu = enabled.trailing_zeros();

    let job = Arc::new(Boot { machine: machine.clone(), req, report });
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
