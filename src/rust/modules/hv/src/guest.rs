//! What `hv boot` and `hv start` share: the guest a command line describes,
//! built from files; the report of how it ended; the ring its console is
//! kept in; and the console made safe to print.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use alloc::boxed::Box;

use alloc::sync::Arc;

use hv::disk as blk;
use hv::linux::Header;
use hv::run::{Counts, LinuxGuest, Stop, MAX_DISKS};
use hv::Machine;
use kcore::consts::{MAX_CPUS, NS_PER_MS};

/// How much of the file is read at once when streaming it into guest memory.
const CHUNK: usize = 64 * 1024;
/// The default guest RAM, and the range it may be given in.
const DEFAULT_MEM_MIB: u64 = 256;
const MIN_MEM_MIB: u64 = 64;
const MAX_MEM_MIB: u64 = 4096;
/// The default command line: the serial console, and no local APIC, which
/// this hypervisor does not emulate.
const DEFAULT_CMDLINE: &str = "console=ttyS0 nolapic";
/// How much of a guest's console is kept: its last this many bytes.
pub const CONSOLE_BYTES: usize = 64 * 1024;

/// A guest as a command line describes it.
pub struct Spec {
    pub kernel: String,
    pub initrd: Option<String>,
    /// `disk=`, as many as there are: files of nos's that are the guest's
    /// `vda`, `vdb`, ... in that order, and whether each is read-only
    /// (`disk=path:ro`).
    pub disks: Vec<(String, bool)>,
    pub mem_bytes: u64,
    pub cmdline: String,
    /// Bytes to type at its console once it is at a prompt.
    pub input: Vec<u8>,
    /// `secs=`, for `hv boot`.
    pub secs: Option<u64>,
    /// `cpu=`, for `hv start`.
    pub cpu: Option<u32>,
    /// `log`: its console to the kernel log too, a line at a time.
    pub log: bool,
    /// `restart`, for `hv start`: boot it again when it resets itself.
    pub restart: bool,
    /// `net`, for `hv start`: a NIC on the guests' switch. Which port is
    /// `nic`'s, once `hv start` has claimed one.
    pub net: bool,
    pub nic: Option<NicSpec>,
}

/// A guest's NIC: its port on the switch.
pub struct NicSpec {
    pub port: usize,
    pub switch: Arc<crate::net::Switch>,
}

/// `\n` in a word for a newline, so that a line to type fits one word.
pub fn unescape(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(text.len()).map_err(|_| String::from("out of memory"))?;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Read the words of `args`: a kernel, then `key=value` options, and last
/// `cmdline=` with the rest of the line. `usage` is what to say for none.
pub fn parse(args: &str, usage: &str) -> Result<Spec, String> {
    let mut spec = Spec {
        kernel: String::new(),
        initrd: None,
        disks: Vec::new(),
        mem_bytes: DEFAULT_MEM_MIB * 1024 * 1024,
        cmdline: String::from(DEFAULT_CMDLINE),
        input: Vec::new(),
        secs: None,
        cpu: None,
        log: false,
        restart: false,
        net: false,
        nic: None,
    };
    let mut mem_mib = DEFAULT_MEM_MIB;

    let mut rest = args.trim_start();
    while !rest.is_empty() {
        /* cmdline= takes everything after it, spaces and all. */
        if let Some(line) = rest.strip_prefix("cmdline=") {
            spec.cmdline = String::from(line.trim_end());
            break;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        rest = rest[end..].trim_start();

        if let Some(v) = word.strip_prefix("mem=") {
            mem_mib = v.parse().map_err(|_| String::from("mem= wants a number of MiB"))?;
        } else if let Some(v) = word.strip_prefix("initrd=") {
            spec.initrd = Some(String::from(v));
        } else if let Some(v) = word.strip_prefix("disk=") {
            if spec.disks.len() >= MAX_DISKS {
                return Err(alloc::format!("at most {} disks", MAX_DISKS));
            }
            let (path, ro) = match v.strip_suffix(":ro") {
                Some(path) => (path, true),
                None => (v, false),
            };
            spec.disks.push((String::from(path), ro));
        } else if let Some(v) = word.strip_prefix("secs=") {
            spec.secs = Some(v.parse().map_err(|_| String::from("secs= wants a number of seconds"))?);
        } else if let Some(v) = word.strip_prefix("cpu=") {
            spec.cpu = Some(v.parse().map_err(|_| String::from("cpu= wants a CPU number"))?);
        } else if let Some(v) = word.strip_prefix("input=") {
            spec.input = unescape(v)?;
        } else if word == "log" {
            spec.log = true;
        } else if word == "restart" {
            spec.restart = true;
        } else if word == "net" {
            spec.net = true;
        } else if spec.kernel.is_empty() && !word.contains('=') {
            spec.kernel = String::from(word);
        } else {
            return Err(alloc::format!("hv: \"{}\" is not an option here\n{}", word, usage));
        }
    }

    if spec.kernel.is_empty() {
        return Err(String::from(usage));
    }
    if !(MIN_MEM_MIB..=MAX_MEM_MIB).contains(&mem_mib) {
        return Err(alloc::format!("mem= must be {}..{} MiB", MIN_MEM_MIB, MAX_MEM_MIB));
    }
    spec.mem_bytes = mem_mib * 1024 * 1024;
    Ok(spec)
}

/// Copy `len` bytes of the file at `path` into guest memory at `gpa`, a chunk
/// at a time. The file, from `offset` on, must be `len` bytes or more.
fn stream(guest: &mut LinuxGuest, path: &str, offset: u64, gpa: u64, len: u64) -> Result<(), String> {
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

/// Build the guest: its memory, the kernel and the initrd streamed into it,
/// and the rest of what the boot protocol wants laid out beside them.
pub fn build(machine: &Machine, spec: &Spec) -> Result<LinuxGuest, String> {
    let mut guest = LinuxGuest::new(machine, spec.mem_bytes).map_err(|e| alloc::format!("no guest: {}", e))?;

    /* The header is in the first page or two; read enough to parse it. */
    let mut first = Vec::new();
    first.try_reserve_exact(2 * kcore::consts::PAGE_SIZE).map_err(|_| String::from("out of memory"))?;
    first.resize(2 * kcore::consts::PAGE_SIZE, 0);
    let got = kcore::fs::read_at(&spec.kernel, 0, &mut first)
        .map_err(|e| alloc::format!("reading {}: {}", spec.kernel, e))?;
    first.truncate(got);
    let header = Header::parse(&first).map_err(|_| alloc::format!("{} is not a 64-bit bzImage", spec.kernel))?;

    let kernel_size = kcore::fs::size(&spec.kernel).map_err(|e| alloc::format!("{}: {}", spec.kernel, e))?;
    let pm_offset = header.pm_offset();
    if pm_offset >= kernel_size {
        return Err(String::from("the image is shorter than its own setup"));
    }
    let kernel_len = kernel_size - pm_offset;

    let initrd_len = match &spec.initrd {
        Some(path) => kcore::fs::size(path).map_err(|e| alloc::format!("{}: {}", path, e))?,
        None => 0,
    };

    let layout = hv::linux::plan(&header, spec.mem_bytes, kernel_len, initrd_len)
        .map_err(|e| alloc::format!("the guest's memory does not fit its kernel: {}", e))?;

    /* The 64-bit kernel at its load address, then the initrd high. */
    stream(&mut guest, &spec.kernel, pm_offset, layout.kernel_addr, kernel_len)?;
    if let Some(path) = &spec.initrd {
        stream(&mut guest, path, 0, layout.initrd_addr, layout.initrd_len)?;
    }

    guest
        .load(&header, &first, layout, spec.cmdline.as_bytes())
        .map_err(|e| alloc::format!("laying out the guest: {}", e))?;

    if let Some(nic) = &spec.nic {
        guest.add_nic(Box::new(nic.switch.backend(nic.port)), crate::net::port_mac(nic.port))
            .map_err(|e| alloc::format!("the NIC: {}", e))?;
    }
    for (i, (path, ro)) in spec.disks.iter().enumerate() {
        let disk = FileDisk::open(path, *ro)?;
        let mut id = String::new();
        let _ = write!(id, "nos-vd{}", (b'a' + i as u8) as char);
        guest.add_disk(Box::new(disk), &id).map_err(|e| alloc::format!("{}: {}", path, e))?;
    }
    Ok(guest)
}

/// A guest's disk that is a file of nos's -- an image, whose size is the
/// disk's, whole sectors of it -- read and written where the guest asks,
/// and synced by its flush.
struct FileDisk {
    path: String,
    size: u64,
    read_only: bool,
}

impl FileDisk {
    fn open(path: &str, read_only: bool) -> Result<FileDisk, String> {
        let size = kcore::fs::size(path).map_err(|e| alloc::format!("{}: {}", path, e))?;
        let size = size - size % blk::SECTOR;
        if size == 0 {
            return Err(alloc::format!("{}: not a sector long", path));
        }
        Ok(FileDisk { path: String::from(path), size, read_only })
    }
}

impl blk::Backend for FileDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> bool {
        matches!(kcore::fs::read_at(&self.path, offset, buf), Ok(n) if n == buf.len())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> bool {
        /* The device refuses a write to a read-only disk before it gets
         * here; this is the second no. */
        !self.read_only && kcore::fs::write_at(&self.path, offset, data).is_ok()
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn flush(&mut self) -> bool {
        kcore::fs::sync().is_ok()
    }
}

/// The CPU a vCPU is to run on: `wanted` if the extension is on there, else
/// the CPU the extension is on for that has fewest vCPUs already (`load`
/// counts them, by CPU), the higher of any two that tie -- CPU 0 is where
/// the boot CPU's own work runs, and the last to be given a guest.
pub fn pick_cpu(machine: &Machine, wanted: Option<u32>, load: &[u32; MAX_CPUS]) -> Result<u32, String> {
    let enabled = machine.enabled_mask();
    if enabled == 0 {
        return Err(String::from("the extension is on for no CPU -- hv on first"));
    }
    if let Some(cpu) = wanted {
        if (cpu as usize) < MAX_CPUS && enabled & (1u64 << cpu) != 0 {
            return Ok(cpu);
        }
        return Err(alloc::format!("the extension is not on for cpu {}", cpu));
    }
    let mut best: Option<u32> = None;
    for cpu in 0..MAX_CPUS as u32 {
        if enabled & (1u64 << cpu) == 0 {
            continue;
        }
        if best.map_or(true, |b| load[cpu as usize] <= load[b as usize]) {
            best = Some(cpu);
        }
    }
    best.ok_or_else(|| String::from("the extension is on for no CPU -- hv on first"))
}

/// Why the guest stopped, in a line.
pub fn describe(stop: &Stop, out: &mut dyn Write) -> core::fmt::Result {
    match *stop {
        Stop::Halted { rip } => write!(out, "hlt with interrupts off at {:#x}", rip),
        Stop::Mmio { gpa, rip } => write!(out,
            "a touch of guest physical {:#x}, no memory and no device there, rip {:#x}", gpa, rip),
        Stop::Shutdown { rip } => write!(out, "triple fault at {:#x}", rip),
        Stop::Reset { port, value, rip } => write!(out, "the guest asked for a reset, {:#04x} to port {:#x} ({}), at {:#x}",
            value, port, hv::run::reset_source(port), rip),
        Stop::Exception { vector, rip } => write!(out, "exception {} at {:#x}", vector, rip),
        Stop::Refused(r) => write!(out, "not entered: {:?}", r),
        Stop::Invalid => write!(out, "VMEXIT_INVALID -- the CPU refused the VMCB"),
        Stop::Budget => write!(out, "by the host, its time up"),
        Stop::Requested => write!(out, "on request"),
        Stop::Unexpected { exit, rip } => write!(out, "an exit with no handler: {:?} at {:#x}", exit, rip),
    }
}

/// How the run ended and what it counted: what `hv boot` and `hv stop` say.
pub fn report(out: &mut dyn Write, guest: &LinuxGuest, stop: &Stop, counts: &Counts, run_ns: u64) {
    let run_ns = run_ns.max(1);
    let _ = write!(out, "  stopped    ");
    let _ = describe(stop, out);
    let _ = writeln!(out, ", after {} ms", run_ns / NS_PER_MS);
    let _ = writeln!(out,
        "  exits      {} total: {} port in, {} port out, {} cpuid, {} rdmsr, {} wrmsr ({} #GP), {} irq, {} hlt, {} host",
        counts.exits, counts.port_in, counts.port_out, counts.cpuid,
        counts.msr_read, counts.msr_write, counts.msr_gp, counts.irq, counts.hlt, counts.host);
    if counts.ud != 0 || counts.wbinvd != 0 {
        let _ = writeln!(out, "  answered   {} #UD for instructions CPUID did not offer, {} WBINVD stepped past",
            counts.ud, counts.wbinvd);
    }
    let absent = guest.absent_pages();
    if !absent.is_empty() {
        let _ = write!(out, "  absent     reads of no device answered with all ones at");
        for gpa in absent {
            let _ = write!(out, " {:#x}", gpa);
        }
        let _ = writeln!(out);
    }
    for (msr, value, write) in guest.msr_faults() {
        let _ = if *write {
            writeln!(out, "  #GP        wrmsr {:#x} <- {:#x}", msr, value)
        } else {
            writeln!(out, "  #GP        rdmsr {:#x}", msr)
        };
    }
    /* How much of the run the vCPU's task spent asleep with the guest halted:
     * the host CPU an idle guest gives back. */
    let _ = writeln!(out, "  halted     slept {} ms in {} sleeps, {}% of the run",
        counts.slept_ns / NS_PER_MS, counts.sleeps, counts.slept_ns * 100 / run_ns);
    let ((irr, isr, imr), (m0, r0, run0)) = guest.irq_debug();
    let _ = writeln!(out, "  irq        {} total ({} timer, {} serial), {} edges, {} blocked; PIC irr {:#04x} isr {:#04x} imr {:#04x}; PIT ch0 mode {} reload {} run {}",
        counts.irq, counts.irq0, counts.irq4, counts.edges0, counts.blocked, irr, isr, imr, m0, r0, run0);
    for (i, (sectors, s, broken)) in guest.disk_stats().enumerate() {
        let _ = writeln!(out, "  vd{}        {} MiB: {} reads ({} KiB), {} writes ({} KiB), {} flushes, {} errors{}",
            (b'a' + i as u8) as char, sectors * blk::SECTOR / (1024 * 1024), s.reads, s.read_bytes / 1024,
            s.writes, s.written_bytes / 1024, s.flushes, s.errors,
            if broken { "; stopped over a ring the driver broke" } else { "" });
    }
    for (i, (s, broken)) in guest.nic_stats().enumerate() {
        let _ = writeln!(out, "  eth{}       {} frames sent, {} received, {} dropped{}",
            i, s.sent, s.received, s.dropped,
            if broken { "; stopped over a ring the driver broke" } else { "" });
    }
    let hot = guest.hot_ports(6);
    if !hot.is_empty() {
        let _ = write!(out, "  busiest in ports");
        for (port, count) in hot {
            let _ = write!(out, " {:#x}:{}", port, count);
        }
        let _ = writeln!(out);
    }
    if !matches!(stop, Stop::Halted { .. } | Stop::Budget | Stop::Requested | Stop::Reset { .. }) {
        let _ = guest.dump(out);
    }
}

/// The last `CONSOLE_BYTES` a guest wrote to its console, and how many it
/// has written in all -- so a reader can ask for what came after a point it
/// saw, and learn it lost some if the ring went round in between.
pub struct Ring {
    buf: Vec<u8>,
    /// Where the oldest kept byte is.
    head: usize,
    len: usize,
    total: u64,
}

impl Ring {
    /// None when the memory is not there; the whole ring is taken now, so
    /// that `push` never allocates.
    pub fn new() -> Option<Ring> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(CONSOLE_BYTES).ok()?;
        buf.resize(CONSOLE_BYTES, 0);
        Some(Ring { buf, head: 0, len: 0, total: 0 })
    }

    pub fn push(&mut self, byte: u8) {
        let cap = self.buf.len();
        if self.len < cap {
            self.buf[(self.head + self.len) % cap] = byte;
            self.len += 1;
        } else {
            self.buf[self.head] = byte;
            self.head = (self.head + 1) % cap;
        }
        self.total += 1;
    }

    /// Bytes written in all.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Where the oldest byte still kept is: anything before it is gone.
    pub fn oldest(&self) -> u64 {
        self.total - self.len as u64
    }

    /// The kept bytes from absolute position `from` on (from the oldest kept
    /// if it is older), onto `out`, at most `max`. False if memory ran out.
    pub fn since(&self, from: u64, max: usize, out: &mut Vec<u8>) -> bool {
        let oldest = self.oldest();
        let from = from.max(oldest).min(self.total);
        let skip = (from - oldest) as usize;
        let n = (self.len - skip).min(max);
        if out.try_reserve(n).is_err() {
            return false;
        }
        let cap = self.buf.len();
        /* The last `n` of what is after `from`, when `max` cuts it short. */
        let start = skip + (self.len - skip - n);
        for i in 0..n {
            out.push(self.buf[(self.head + start + i) % cap]);
        }
        true
    }

    /// Whether `needle` is among the kept bytes from absolute position
    /// `from` on.
    pub fn contains_since(&self, from: u64, needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        let oldest = self.oldest();
        let skip = (from.max(oldest).min(self.total) - oldest) as usize;
        let len = self.len - skip;
        if needle.len() > len {
            return false;
        }
        let cap = self.buf.len();
        let at = |i: usize| self.buf[(self.head + skip + i) % cap];
        (0..=len - needle.len()).any(|s| needle.iter().enumerate().all(|(k, b)| at(s + k) == *b))
    }
}

/// The longest line of a guest's console the kernel log takes.
const LOG_LINE: usize = 256;
/// The longest control sequence `TermFilter` holds while it decides; a
/// longer one is let through as it is.
const CSI_MAX: usize = 32;

/// Where a byte of a guest's console is in an escape sequence: what
/// [`sanitize`] and [`LogLine`] leave out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    Text,
    /// ESC: a sequence, one more byte long unless that byte is `[`.
    Start,
    /// ESC [: a CSI sequence, to its final byte.
    Csi,
}

impl Esc {
    /// The state after `byte`, and whether `byte` is text.
    fn step(self, byte: u8) -> (Esc, bool) {
        match (self, byte) {
            (_, 0x1B) => (Esc::Start, false),
            (Esc::Start, b'[') => (Esc::Csi, false),
            (Esc::Start, _) => (Esc::Text, false),
            (Esc::Csi, 0x40..=0x7E) => (Esc::Text, false),
            (Esc::Csi, _) => (Esc::Csi, false),
            (Esc::Text, _) => (Esc::Text, true),
        }
    }
}

/// A guest's console for the kernel log, a line at a time: printable ASCII
/// only, escape sequences left out whole, the line cut at `LOG_LINE`. Its
/// room is taken when it is made, so a byte never allocates.
pub struct LogLine {
    text: String,
    esc: Esc,
}

impl LogLine {
    pub fn new() -> Option<LogLine> {
        let mut text = String::new();
        text.try_reserve_exact(LOG_LINE).ok()?;
        Some(LogLine { text, esc: Esc::Text })
    }

    /// Take a byte; true when it ended a line, which `text` then holds until
    /// `clear`.
    pub fn push(&mut self, byte: u8) -> bool {
        let (esc, text) = self.esc.step(byte);
        self.esc = esc;
        if !text {
            return false;
        }
        if byte == b'\n' {
            return true;
        }
        if (0x20..0x7F).contains(&byte) && self.text.len() < LOG_LINE {
            self.text.push(byte as char);
        }
        false
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }
}

/// A guest's console on its way to a person's terminal, for `hv attach`: as
/// it is -- colours, cursor movement, a line editor's redraws -- but for what
/// a terminal would answer or act on. A query (a device status report,
/// `ESC [ 6 n`; device attributes, `ESC [ c`; `ESC Z`) is taken out: the
/// terminal's answer would be typed into the guest after the UART's own. The
/// string sequences -- an operating-system command, `ESC ]`, and `ESC P`,
/// `ESC _`, `ESC ^`, `ESC X` -- are taken out whole, to their end: they set a
/// terminal's title, its clipboard, its palette, nothing a guest should reach
/// on the person's machine. And NULs, which the kernel's printers end a
/// string at. It keeps its place across calls: a sequence may be cut
/// anywhere between two reads of the console.
pub struct TermFilter {
    state: Term,
    csi: [u8; CSI_MAX],
    csi_len: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Term {
    Text,
    /// ESC.
    Esc,
    /// ESC [ and what has come of it, held in `csi`.
    Csi,
    /// A CSI too long to hold, let through to its final byte.
    CsiLong,
    /// A string sequence, dropped to its BEL or ST.
    Str,
    /// An ESC inside one: `\` ends it.
    StrEsc,
}

const BEL: u8 = 0x07;
const ESC: u8 = 0x1B;
const NUL: u8 = 0x00;

impl TermFilter {
    pub fn new() -> TermFilter {
        TermFilter { state: Term::Text, csi: [0; CSI_MAX], csi_len: 0 }
    }

    /// `bytes`, filtered, onto `out`. Pushes at most `bytes.len() + CSI_MAX
    /// + 2` bytes: room taken for that allocates nothing here.
    pub fn filter(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &b in bytes {
            self.state = match self.state {
                Term::Text => match b {
                    ESC => Term::Esc,
                    NUL => Term::Text,
                    _ => {
                        out.push(b);
                        Term::Text
                    }
                },
                Term::Esc => match b {
                    b'[' => {
                        self.csi_len = 0;
                        Term::Csi
                    }
                    b']' | b'P' | b'_' | b'^' | b'X' => Term::Str,
                    /* DECID: identify yourself -- a query too */
                    b'Z' => Term::Text,
                    ESC => {
                        out.push(ESC);
                        Term::Esc
                    }
                    NUL => Term::Esc,
                    _ => {
                        out.push(ESC);
                        out.push(b);
                        Term::Text
                    }
                },
                Term::Csi => match b {
                    0x20..=0x3F => {
                        if self.csi_len < CSI_MAX {
                            self.csi[self.csi_len] = b;
                            self.csi_len += 1;
                            Term::Csi
                        } else {
                            self.flush_csi(out);
                            out.push(b);
                            Term::CsiLong
                        }
                    }
                    /* The final byte: a query goes, anything else is let
                     * through whole. */
                    0x40..=0x7E => {
                        if b != b'n' && b != b'c' {
                            self.flush_csi(out);
                            out.push(b);
                        }
                        Term::Text
                    }
                    /* A control byte inside it: what came so far as it was,
                     * and the byte as text would have it. */
                    ESC => {
                        self.flush_csi(out);
                        Term::Esc
                    }
                    _ => {
                        self.flush_csi(out);
                        if b != NUL {
                            out.push(b);
                        }
                        Term::Text
                    }
                },
                Term::CsiLong => {
                    if b != NUL {
                        out.push(b);
                    }
                    if (0x40..=0x7E).contains(&b) { Term::Text } else { Term::CsiLong }
                }
                Term::Str => match b {
                    BEL => Term::Text,
                    ESC => Term::StrEsc,
                    _ => Term::Str,
                },
                Term::StrEsc => match b {
                    b'\\' => Term::Text,
                    ESC => Term::StrEsc,
                    _ => Term::Str,
                },
            };
        }
    }

    fn flush_csi(&mut self, out: &mut Vec<u8>) {
        out.push(ESC);
        out.push(b'[');
        out.extend_from_slice(&self.csi[..self.csi_len]);
        self.csi_len = 0;
    }
}

/// A guest's console as it is safe to print to whoever asked: printable
/// ASCII, newlines and tabs; escape sequences taken out whole -- a shell's
/// `ESC [ 6 n` printed to a terminal makes the terminal type its answer into
/// whatever reads it next -- carriage returns dropped, other control bytes
/// dropped, and anything above ASCII shown as `?`.
pub fn sanitize(bytes: &[u8], out: &mut Vec<u8>) -> bool {
    if out.try_reserve(bytes.len()).is_err() {
        return false;
    }
    let mut esc = Esc::Text;
    for &b in bytes {
        let (next, text) = esc.step(b);
        esc = next;
        if !text {
            continue;
        }
        match b {
            b'\n' | b'\t' | 0x20..=0x7E => out.push(b),
            0x80..=0xFF => out.push(b'?'),
            _ => {}
        }
    }
    true
}
