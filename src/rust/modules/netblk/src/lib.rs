#![no_std]

//! netblk: a disk -- NVMe, or a partition of an NVMe disk -- served over
//! UDP, with no copy of the data anywhere in the machine.
//!
//!     insmod /netblk.ko
//!     netblk start nvme0 7000
//!     netblk list
//!     netblk stop 7000
//!
//! A read is DMA'd by the disk straight into the frame the NIC transmits; a
//! write is DMA'd by the disk straight out of the frame the NIC received it
//! in, which then goes back out as the reply. The CPU writes the 72 bytes of
//! headers in front of the data and never touches the data itself.
//!
//! The path, and the kernel's lockless rings that join its pieces:
//!
//! - receive softirq: `on_frame` parses a request, takes a slot for it and
//!   queues it on `requests` -- no lock, no allocation, no copy;
//! - the worker, a task of the instance's pinned to one CPU: submits what is
//!   queued to the disk, each I/O's PRP pointing into its frame, and rings
//!   the disk's doorbell once for the batch;
//! - the disk's interrupt: `on_disk_done` queues the slot on `done`;
//! - the worker: writes each reply's headers and hands the batch to the NIC
//!   under one lock, with one doorbell.
//!
//! The worker polls a little while completions are due -- a wakeup is an
//! interrupt, an IPI and a context switch -- and sleeps otherwise.
//!
//! One datagram carries one I/O, whole sectors of it: with a 1500-byte MTU,
//! two 512-byte sectors. A read may ask for more, up to `READ_PIECES`
//! datagrams' worth, and is answered a datagram at a time. The protocol is in
//! docs/netblk.md; scripts/netblk.py speaks it.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::fmt::Write;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kcore::block::{BlockIo, Disk, DiskClaim, SubmitError, IO_FLUSH, IO_READ, IO_WRITE};
use kcore::cmd::{Command, Output};
use kcore::error::Error;
use kcore::net::{ListenError, NetFrame, Nic, UdpListener};
use kcore::ring::LocklessRing;
use kcore::sync::{Event, Mutex};
use kcore::task::TaskHandle;
use kcore::time::boot_time_ns;

const HELP: &str = "netblk start <disk> <port> [ro nic= mtu= cpu= poll=] | list | stop <port>|all - a disk over UDP";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: netblk start <disk> <port> [ro] [nic=eth0] [mtu=1500] [cpu=N] [poll=us]\n       netblk list\n       netblk stop <port>|all";

/* The protocol: every field big-endian */
const MAGIC: u32 = 0x4E42_4C4B; /* "NBLK" */
const VERSION: u8 = 1;

const OP_INFO: u8 = 1;
const OP_READ: u8 = 2;
const OP_WRITE: u8 = 3;
const OP_FLUSH: u8 = 4;
/* A reply's op is the request's with this bit set */
const OP_REPLY: u8 = 0x80;

const FLAG_FUA: u16 = 1;

const ST_OK: u16 = 0;
const ST_BADREQ: u16 = 1;
const ST_RANGE: u16 = 2;
const ST_IO: u16 = 3;
const ST_BUSY: u16 = 4;
const ST_ROFS: u16 = 5;

/* INFO's reply: size u64, sector size u32, max_io u32, max_read u32,
   flags u32, slots u32, and a u32 kept for later */
const INFO_LEN: usize = 32;
const INFO_READ_ONLY: u32 = 1;

const ETH_LEN: usize = 14;
const IP_LEN: usize = 20;
const UDP_LEN: usize = 8;
const HDR_LEN: usize = 30;
/* A reply's headers, never with IP options: where its data starts */
const REPLY_HDRS: usize = ETH_LEN + IP_LEN + UDP_LEN + HDR_LEN;

const ETHERTYPE_IP: u16 = 0x0800;
const IP_VERSION_IHL: u8 = 0x45;
const IPPROTO_UDP: u8 = 17;
/* More-fragments and the fragment offset: a fragment is never ours, there
   being no reassembly here */
const IP_FRAG_MASK: u16 = 0x3FFF;
const IP_DF: u16 = 0x4000;
const IP_TTL: u8 = 64;

const DEFAULT_NIC: &str = "eth0";
/* Ports below are the well-known ones -- DHCP's 68 among them, which the
   kernel takes for each attempt and gives back after */
const FIRST_PORT: u16 = 1024;
const DEFAULT_MTU: usize = 1500;
const MIN_MTU: usize = 576;
/* A frame from the kernel's pool holds 2 KiB, Ethernet header included */
const MAX_MTU: usize = 2048 - ETH_LEN;

/* A read asks for up to this many datagrams' worth */
const READ_PIECES: usize = 16;
/* Requests an instance holds at once, queued or at the disk: its slots, and
   the capacity of each ring, which therefore can never be full */
const SLOTS: usize = 256;
/* Completions, or submissions, the worker takes in one go */
const BATCH: usize = 64;
/* How long the worker polls for completions it is owed before it sleeps */
const POLL_NS: u64 = 50_000;
/* poll=: how long it may go on polling once it has nothing at all, for the
   next request -- a CPU spent on latency at a low queue depth */
const MAX_IDLE_POLL_US: u64 = 100_000;
/* A request stalled with nothing of ours at the disk: how long before it is
   tried again, no completion of ours being due to say there is room */
const STALL_RETRY_MS: u64 = 1;

/* The disk takes data dword aligned */
const DWORD_MASK: u64 = 3;

/* Service time -- from a request's arrival on the receive path to its reply
   going to the NIC, what this machine adds and the network does not -- is
   kept in units of 1024 ns, a shift of the clock rather than a division, in
   a u32 that wraps every 73 minutes: plenty for a duration. The histogram has
   2^SUB_BITS buckets to each power of two, so a percentile is within 1/16. */
const UNIT_SHIFT: u32 = 10;
const SUB_BITS: u32 = 4;
const SUB: usize = 1 << SUB_BITS;
const BUCKETS: usize = (u32::BITS as usize - SUB_BITS as usize + 1) * SUB;

const NS_PER_SEC: u64 = 1_000_000_000;
const NS_PER_US: u64 = 1_000;
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/* ------------------------------------------------------------------ */
/* The module and its command                                          */
/* ------------------------------------------------------------------ */

struct NetBlk {
    _cmd: Command,
    _registry: Arc<Registry>,
}

impl kmod::Module for NetBlk {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let registry = Arc::new(Registry {
        lock: Mutex::new().ok_or(Error::NoMemory)?,
        instances: UnsafeCell::new(Vec::new()),
    });

    let reg = registry.clone();
    let cmd = Command::register("netblk", HELP, move |args, out| {
        if let Err(problem) = run(&reg, args, out) {
            let _ = writeln!(out, "netblk: {}", problem);
        }
    })?;

    /* The command goes first on rmmod, waiting out a call still running; the
       instances go with the last reference to the registry, right after. */
    Ok(Box::new(NetBlk { _cmd: cmd, _registry: registry }))
}

kmod::module!(name: "netblk", init: init);

/* The running instances, for the command's calls -- which may come from the
   console and the UDP shell at once -- to share */
struct Registry {
    lock: Mutex,
    instances: UnsafeCell<Vec<Instance>>,
}

/* instances is only reached under lock */
unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

impl Registry {
    fn with<R>(&self, f: impl FnOnce(&mut Vec<Instance>) -> R) -> R {
        let _guard = self.lock.lock();
        f(unsafe { &mut *self.instances.get() })
    }
}

fn run(reg: &Registry, args: &str, out: &mut Output) -> Result<(), String> {
    let mut words = args.split_whitespace();
    match words.next() {
        Some("start") => start(reg, words, out),
        Some("list") | None => {
            list(reg, out);
            Ok(())
        }
        Some("stop") => stop(reg, words.next(), out),
        Some(other) => Err(format!("what is '{}'?\n{}", other, USAGE)),
    }
}

fn start<'a>(reg: &Registry, mut words: impl Iterator<Item = &'a str>, out: &mut Output) -> Result<(), String> {
    let disk_name = words.next().ok_or_else(|| format!("which disk? `disks` lists them\n{}", USAGE))?;
    let port_text = words.next().ok_or_else(|| format!("which UDP port?\n{}", USAGE))?;
    let port = match port_text.parse::<u16>() {
        Ok(p) if p >= FIRST_PORT => p,
        Ok(_) => {
            return Err(format!(
                "ports below {} are the well-known ones -- DHCP's 68 among them, taken and given back each renewal",
                FIRST_PORT
            ))
        }
        _ => return Err(format!("{}: not a UDP port", port_text)),
    };
    if reg.with(|instances| instances.iter().any(|i| i.port == port)) {
        return Err(format!("port {} is served already -- one instance to a port, whatever the NIC", port));
    }

    let mut read_only = false;
    let mut nic_name = DEFAULT_NIC;
    let mut mtu = DEFAULT_MTU;
    let mut cpu = None;
    let mut poll_us = 0u64;
    for word in words {
        if word == "ro" {
            read_only = true;
            continue;
        }
        let number = |v: &str| v.parse::<u32>().map_err(|_| format!("{}: not a number", word));
        match word.split_once('=') {
            Some(("nic", v)) => nic_name = v,
            Some(("mtu", v)) => mtu = number(v)? as usize,
            Some(("cpu", v)) => cpu = Some(number(v)?),
            Some(("poll", v)) => poll_us = number(v)? as u64,
            _ => return Err(format!("what is '{}'?\n{}", word, USAGE)),
        }
    }
    if poll_us > MAX_IDLE_POLL_US {
        return Err(format!("poll from 0 to {} microseconds", MAX_IDLE_POLL_US));
    }

    let disk = Disk::open(disk_name).ok_or_else(|| format!("no block device {} -- `disks` lists them", disk_name))?;
    if !disk.can_submit() {
        return Err(format!(
            "{} has no asynchronous I/O -- netblk serves NVMe disks and their partitions",
            disk_name
        ));
    }
    let nic = Nic::find(nic_name).ok_or_else(|| format!("no network device {} -- `net` lists them", nic_name))?;

    if !(MIN_MTU..=MAX_MTU).contains(&mtu) {
        return Err(format!("mtu from {} to {}", MIN_MTU, MAX_MTU));
    }

    let sector_size = disk.sector_size();
    if sector_size == 0 || !sector_size.is_power_of_two() {
        return Err(format!("{} has {}-byte sectors?", disk_name, sector_size));
    }
    let max_io = ((mtu - IP_LEN - UDP_LEN - HDR_LEN) as u64 / sector_size) * sector_size;
    if max_io == 0 {
        return Err(format!(
            "a {}-byte sector of {} does not fit a {}-byte datagram -- no room to carry one without a copy",
            sector_size, disk_name, mtu
        ));
    }
    let size = disk.sectors() * sector_size;
    if size == 0 {
        return Err(format!("{} is empty", disk_name));
    }

    let online = kcore::cpu::online_mask();
    let cpu = match cpu {
        Some(c) if c < u64::BITS && online & (1u64 << c) != 0 => c,
        Some(c) => return Err(format!("cpu {} is not running", c)),
        None => pick_cpu(reg, online),
    };

    /* Held while it serves: no mount, disk log or writer can take the device
       -- or a disk or partition overlapping it -- from under its clients */
    let claim = if read_only {
        None
    } else {
        let claim = disk.claim().map_err(|who| {
            format!("{} is in use by {} -- serve it read-only with ro, or free it first", disk_name, who)
        })?;
        Some(claim)
    };

    let shared = Shared::new(disk, nic, port, read_only, sector_size, size, max_io, poll_us * NS_PER_US)
        .ok_or_else(|| "no memory for its rings and slots".to_string())?;
    let ctx = &*shared as *const Shared as *mut u8;

    let worker = kcore::task::spawn_on_with_ctx(1u64 << cpu, worker_main, ctx)
        .ok_or_else(|| "could not start its task".to_string())?;

    /* An instance from here on, so that a failure below is torn down by its
       drop like any other */
    let mut instance = Instance {
        port,
        disk_name: disk_name.to_string(),
        nic_name: nic_name.to_string(),
        cpu,
        mtu,
        started: boot_time_ns(),
        shared,
        listener: None,
        worker: Some(worker),
        _claim: claim,
    };

    /* Last: requests may arrive from this moment */
    instance.listener = Some(nic.listen_udp(port, on_frame, ctx).map_err(|e| match e {
        ListenError::PortTaken => format!("UDP port {} on {} is taken", port, nic_name),
        ListenError::TableFull => format!("{} listens on as many ports as it can already", nic_name),
        ListenError::Invalid => format!("{}: not a UDP port", port),
    })?);

    /* Checked again where two starts cannot both get past it */
    let taken = reg.with(|instances| {
        if instances.iter().any(|i| i.port == port) {
            Some(instance)
        } else {
            instances.push(instance);
            None
        }
    });
    if let Some(instance) = taken {
        drop(instance);
        return Err(format!("port {} is served already -- one instance to a port, whatever the NIC", port));
    }

    let _ = writeln!(
        out,
        "netblk: serving {} ({}, {}) on {} {}:{} -- {} bytes a datagram, worker on cpu {}{}",
        disk_name,
        Size(size),
        if read_only { "read-only" } else { "read-write" },
        nic_name,
        Ip(nic.ip()),
        port,
        max_io,
        cpu,
        if poll_us != 0 { format!(", polling {} us for the next request", poll_us) } else { String::new() }
    );
    Ok(())
}

/* From the top down, one CPU after another: CPU 0 has the most else to do */
fn pick_cpu(reg: &Registry, online: u64) -> u32 {
    let count = online.count_ones();
    if count == 0 {
        return 0;
    }
    let running = reg.with(|instances| instances.len()) as u32;
    let mut nth = count - 1 - running % count;
    for cpu in 0..u64::BITS {
        if online & (1u64 << cpu) != 0 {
            if nth == 0 {
                return cpu;
            }
            nth -= 1;
        }
    }
    0
}

fn stop(reg: &Registry, which: Option<&str>, out: &mut Output) -> Result<(), String> {
    let which = which.ok_or_else(|| format!("which port? `netblk list` shows them\n{}", USAGE))?;

    let stopping: Vec<Instance> = if which == "all" {
        reg.with(core::mem::take)
    } else {
        let port = which.parse::<u16>().map_err(|_| format!("{}: not a UDP port", which))?;
        reg.with(|instances| match instances.iter().position(|i| i.port == port) {
            Some(at) => alloc::vec![instances.remove(at)],
            None => Vec::new(),
        })
    };

    if stopping.is_empty() {
        if which != "all" {
            return Err(format!("nothing is served on port {}", which));
        }
        let _ = writeln!(out, "netblk: nothing served");
    }

    /* Outside the lock: an instance's drop waits for what its disk still has */
    for instance in stopping {
        let port = instance.port;
        let name = instance.disk_name.clone();
        drop(instance);
        let _ = writeln!(out, "netblk: stopped serving {} on port {}", name, port);
    }
    Ok(())
}

fn list(reg: &Registry, out: &mut Output) {
    reg.with(|instances| {
        if instances.is_empty() {
            let _ = writeln!(out, "netblk: nothing served");
            return;
        }
        let now = boot_time_ns();
        for i in instances.iter() {
            let sh = &*i.shared;
            let _ = writeln!(
                out,
                "port {}: {} ({}, {}) on {} {}, {} bytes a datagram (mtu {}), worker on cpu {}{}, up {} s",
                i.port,
                i.disk_name,
                Size(sh.size),
                if sh.read_only { "ro" } else { "rw" },
                i.nic_name,
                Ip(sh.nic.ip()),
                sh.max_io,
                i.mtu,
                i.cpu,
                if sh.idle_poll_ns != 0 {
                    format!(", poll {} us", sh.idle_poll_ns / NS_PER_US)
                } else {
                    String::new()
                },
                now.saturating_sub(i.started) / NS_PER_SEC
            );
            let _ = writeln!(
                out,
                "  requests {}, bad {}, refused {} (busy {}), in flight {} of {}: queued {}, at the disk {}{}, done {}",
                sh.rx.requests.get(),
                sh.rx.bad.get(),
                sh.rx.refused.get(),
                sh.rx.busy.get(),
                SLOTS.saturating_sub(sh.free.len()),
                SLOTS,
                sh.requests.len(),
                sh.worker.in_flight.get(),
                if sh.worker.stalled.get() != 0 { " (one waiting for room)" } else { "" },
                sh.done.len()
            );
            let _ = writeln!(
                out,
                "  reads {} ({}), writes {} ({}), flushes {}, I/O errors {}",
                sh.worker.reads.get(),
                Size(sh.worker.read_bytes.get()),
                sh.worker.writes.get(),
                Size(sh.worker.written_bytes.get()),
                sh.worker.flushes.get(),
                sh.worker.errors.get()
            );
            let _ = writeln!(
                out,
                "  disk full {} times, replies dropped {}, worker slept {} times",
                sh.worker.disk_busy.get(),
                sh.worker.tx_dropped.get(),
                sh.worker.sleeps.get()
            );
            if let (Some(p50), Some(p99), Some(p999)) = (
                sh.worker.service_percentile(P50),
                sh.worker.service_percentile(P99),
                sh.worker.service_percentile(P999),
            ) {
                let _ = writeln!(
                    out,
                    "  service time us (arrival to reply): p50 {}, p99 {}, p99.9 {}, max {}",
                    Micros(p50),
                    Micros(p99),
                    Micros(p999),
                    Micros(sh.worker.service_max.get() << UNIT_SHIFT)
                );
            }
        }
    });
}

/* A byte count the way a person would say it -- to a tenth, so that 1.9 GiB
   does not come out as 1 */
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (unit, name) = match self.0 {
            n if n >= GIB => (GIB, "GiB"),
            n if n >= MIB => (MIB, "MiB"),
            n if n >= KIB => (KIB, "KiB"),
            _ => (1, "bytes"),
        };
        let tenths = self.0 % unit * 10 / unit;
        if tenths == 0 {
            write!(f, "{} {}", self.0 / unit, name)
        } else {
            write!(f, "{}.{} {}", self.0 / unit, tenths, name)
        }
    }
}

/* Percentiles `list` gives the service time at, in ten-thousandths */
const P50: u64 = 5_000;
const P99: u64 = 9_900;
const P999: u64 = 9_990;

/* Nanoseconds, shown in microseconds to a tenth */
struct Micros(u64);

impl fmt::Display for Micros {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0 / NS_PER_US, (self.0 % NS_PER_US) / 100)
    }
}

/* An IPv4 address in host byte order, dotted */
struct Ip(u32);

impl fmt::Display for Ip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}

/* ------------------------------------------------------------------ */
/* An instance                                                         */
/* ------------------------------------------------------------------ */

struct Instance {
    port: u16,
    disk_name: String,
    nic_name: String,
    cpu: u32,
    mtu: usize,
    started: u64,
    shared: Box<Shared>,
    listener: Option<UdpListener>,
    worker: Option<TaskHandle>,
    /* Last to go */
    _claim: Option<DiskClaim>,
}

/* Everything behind `shared` is reached from other contexts only through the
   rings, the event and its atomics */
unsafe impl Send for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        /* No new requests: the listener goes once no receive callback of it
           is still running. */
        drop(self.listener.take());

        /* The worker drops what is queued, waits for what the disk has, and
           returns. */
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.event.signal();
        if let Some(worker) = self.worker.as_ref() {
            worker.wait();
        }

        /* The disk's completion callback runs in its interrupt handler, and
           the last one may still be on its way out -- signalling the event --
           after the worker has seen its completion. Every handler running
           anywhere has returned once this does; then the rings, the event and
           the slots can go. */
        kcore::cpu::synchronize();
        drop(self.worker.take());
    }
}

/* What the receive softirq, the worker and the disk's interrupt handler all
   reach, through a pointer: the configuration, the rings, the slots */
struct Shared {
    disk: Disk,
    nic: Nic,
    mac: [u8; 6],
    port: u16,
    read_only: bool,
    sector_size: u64,
    sector_shift: u32,
    size: u64,
    max_io: u64,
    max_read: u64,
    /* poll=, in nanoseconds; 0 sleeps as soon as there is nothing to do */
    idle_poll_ns: u64,

    /* free slots; requests, from the receive path to the worker; and
       completions, from the disk's interrupt to the worker */
    free: LocklessRing,
    requests: LocklessRing,
    done: LocklessRing,
    /* the worker's wakeup, from either producer */
    event: Event,
    /* SLOTS of them, reached only through the pointers the rings carry */
    slots: *mut Slot,
    stopping: AtomicBool,

    rx: RxStats,
    worker: WorkerStats,
}

impl Shared {
    fn new(
        disk: Disk,
        nic: Nic,
        port: u16,
        read_only: bool,
        sector_size: u64,
        size: u64,
        max_io: u64,
        idle_poll_ns: u64,
    ) -> Option<Box<Self>> {
        let free = LocklessRing::new(SLOTS)?;
        let requests = LocklessRing::new(SLOTS)?;
        let done = LocklessRing::new(SLOTS)?;
        let event = Event::new()?;

        let mut slots = Vec::with_capacity(SLOTS);
        for _ in 0..SLOTS {
            slots.push(Slot::empty());
        }
        let slots = Box::into_raw(slots.into_boxed_slice()) as *mut Slot;

        let shared = Box::new(Self {
            disk,
            nic,
            mac: nic.mac(),
            port,
            read_only,
            sector_size,
            sector_shift: sector_size.trailing_zeros(),
            size,
            max_io,
            max_read: max_io * READ_PIECES as u64,
            idle_poll_ns,
            free,
            requests,
            done,
            event,
            slots,
            stopping: AtomicBool::new(false),
            rx: RxStats::new(),
            worker: WorkerStats::new(),
        });

        let base = &*shared as *const Shared;
        for i in 0..SLOTS {
            let slot = unsafe { shared.slots.add(i) };
            unsafe { (*slot).shared = base };
            shared.free.push(slot as usize);
        }
        Some(shared)
    }

    #[inline]
    fn aligned(&self, offset: u64, length: u64) -> bool {
        (offset | length) & (self.sector_size - 1) == 0
    }

    #[inline]
    fn in_range(&self, offset: u64, length: u64) -> bool {
        offset <= self.size && length <= self.size - offset
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(self.slots, SLOTS)) });
    }
}

/* A request, from the moment the receive path takes it to the moment the
   worker sends its reply: a cache line each, so that two CPUs working on
   neighbours never share one */
#[repr(C, align(64))]
struct Slot {
    shared: *const Shared,
    /* A read's frame to transmit, allocated when it is submitted; a write's
       or a flush's frame as received, kept to become the reply */
    frame: usize,
    cookie: u64,
    offset: u64,
    length: u32,
    /* when the request arrived, in service-time units */
    arrived: u32,
    status: u16,
    /* where a write's data starts in its frame */
    data_off: u16,
    op: u8,
    fua: bool,
    peer: Peer,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            shared: core::ptr::null(),
            frame: 0,
            cookie: 0,
            offset: 0,
            length: 0,
            arrived: 0,
            status: ST_OK,
            data_off: 0,
            op: 0,
            fua: false,
            peer: Peer { mac: [0; 6], ip: 0, port: 0, local_ip: 0 },
        }
    }
}

const _: () = assert!(core::mem::size_of::<Slot>() == 64, "a slot is one cache line");

/* The clock, in service-time units */
#[inline]
fn now_units() -> u32 {
    (boot_time_ns() >> UNIT_SHIFT) as u32
}

fn bucket(units: u32) -> usize {
    if units < SUB as u32 {
        return units as usize;
    }
    let msb = u32::BITS - 1 - units.leading_zeros();
    let shift = msb - SUB_BITS;
    let sub = ((units >> shift) as usize) & (SUB - 1);
    (shift as usize + 1) * SUB + sub
}

/* The middle of a bucket, in nanoseconds */
fn bucket_ns(b: usize) -> u64 {
    let (low, high) = if b < SUB {
        (b as u64, b as u64 + 1)
    } else {
        let shift = (b / SUB - 1) as u32;
        let sub = (b % SUB) as u64;
        ((SUB as u64 + sub) << shift, (SUB as u64 + sub + 1) << shift)
    };
    ((low + high) << UNIT_SHIFT) / 2
}

/* Where a reply goes, and where it comes from */
#[derive(Clone, Copy)]
struct Peer {
    mac: [u8; 6],
    ip: u32,
    port: u16,
    /* the address the request came to */
    local_ip: u32,
}

/* A counter with one writer -- the receive softirq, which runs on one CPU at
   a time, or the worker -- so a load and a store, not a locked instruction
   per request. Readers only report it. */
struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    #[inline]
    fn add(&self, n: u64) {
        self.0.store(self.0.load(Ordering::Relaxed).wrapping_add(n), Ordering::Relaxed);
    }

    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/* The receive path's counters, on a line of their own */
#[repr(align(64))]
struct RxStats {
    requests: Counter,
    bad: Counter,
    refused: Counter,
    busy: Counter,
}

impl RxStats {
    const fn new() -> Self {
        Self { requests: Counter::new(), bad: Counter::new(), refused: Counter::new(), busy: Counter::new() }
    }
}

/* The worker's */
#[repr(align(64))]
struct WorkerStats {
    reads: Counter,
    writes: Counter,
    flushes: Counter,
    read_bytes: Counter,
    written_bytes: Counter,
    errors: Counter,
    disk_busy: Counter,
    tx_dropped: Counter,
    sleeps: Counter,
    /* what it has at the disk, and whether a request waits for room there:
       stored as they change, for `list` */
    in_flight: Counter,
    stalled: Counter,
    /* service times, and the longest */
    service: [Counter; BUCKETS],
    service_max: Counter,
}

impl WorkerStats {
    const fn new() -> Self {
        const ZERO: Counter = Counter::new();
        Self {
            reads: Counter::new(),
            writes: Counter::new(),
            flushes: Counter::new(),
            read_bytes: Counter::new(),
            written_bytes: Counter::new(),
            errors: Counter::new(),
            disk_busy: Counter::new(),
            tx_dropped: Counter::new(),
            sleeps: Counter::new(),
            in_flight: Counter::new(),
            stalled: Counter::new(),
            service: [ZERO; BUCKETS],
            service_max: Counter::new(),
        }
    }

    #[inline]
    fn record_service(&self, units: u32) {
        self.service[bucket(units)].add(1);
        if units as u64 > self.service_max.get() {
            self.service_max.0.store(units as u64, Ordering::Relaxed);
        }
    }

    /* The service time per10k ten-thousandths of the replies took at most,
       in nanoseconds; None before the first */
    fn service_percentile(&self, per10k: u64) -> Option<u64> {
        let total: u64 = self.service.iter().map(Counter::get).sum();
        if total == 0 {
            return None;
        }
        let rank = ((total * per10k + 9_999) / 10_000).max(1);
        let mut seen = 0;
        for (b, n) in self.service.iter().enumerate() {
            seen += n.get();
            if seen >= rank {
                return Some(bucket_ns(b));
            }
        }
        Some(self.service_max.get() << UNIT_SHIFT)
    }
}

/* ------------------------------------------------------------------ */
/* The receive path: the NIC's softirq                                 */
/* ------------------------------------------------------------------ */

struct Request {
    peer: Peer,
    op: u8,
    flags: u16,
    cookie: u64,
    offset: u64,
    length: u32,
    data_off: usize,
    data_len: usize,
}

#[inline]
fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

#[inline]
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[inline]
fn be64(b: &[u8], at: usize) -> u64 {
    ((be32(b, at) as u64) << 32) | be32(b, at + 4) as u64
}

/* An Ethernet frame holding a netblk request, or None. The dispatcher
   matched the UDP port and nothing else. */
fn parse(b: &[u8]) -> Option<Request> {
    if b.len() < ETH_LEN + IP_LEN + UDP_LEN + HDR_LEN || be16(b, 12) != ETHERTYPE_IP {
        return None;
    }

    let ip = ETH_LEN;
    let ihl = (b[ip] & 0x0F) as usize * 4;
    if b[ip] >> 4 != 4 || ihl < IP_LEN || be16(b, ip + 6) & IP_FRAG_MASK != 0 {
        return None;
    }

    let udp = ip + ihl;
    if b.len() < udp + UDP_LEN + HDR_LEN {
        return None;
    }
    /* The UDP length, not the frame's: a short frame carries Ethernet padding */
    let udp_len = be16(b, udp + 4) as usize;
    if udp_len < UDP_LEN + HDR_LEN || udp + udp_len > b.len() {
        return None;
    }

    /* A reply is never answered, whatever it carries: two servers would
       otherwise bounce one datagram between them for good, each refusal an
       answer to the last */
    let hdr = udp + UDP_LEN;
    if be32(b, hdr) != MAGIC || b[hdr + 4] != VERSION || b[hdr + 5] & OP_REPLY != 0 {
        return None;
    }
    let local_ip = be32(b, ip + 16);

    Some(Request {
        peer: Peer {
            mac: [b[6], b[7], b[8], b[9], b[10], b[11]],
            ip: be32(b, ip + 12),
            port: be16(b, udp),
            local_ip,
        },
        op: b[hdr + 5],
        flags: be16(b, hdr + 6),
        cookie: be64(b, hdr + 8),
        offset: be64(b, hdr + 16),
        length: be32(b, hdr + 24),
        data_off: hdr + HDR_LEN,
        data_len: udp_len - UDP_LEN - HDR_LEN,
    })
}

/* The listener: every datagram to the port, lent for the call. */
extern "C" fn on_frame(ctx: *mut u8, frame: usize) {
    let sh = unsafe { &*(ctx as *const Shared) };

    /* To the NIC's own address and no other -- not a subnet broadcast, not an
       address it was never given: the reply comes from it */
    let Some(req) = parse(unsafe { NetFrame::lent(frame) }).filter(|r| r.peer.local_ip == sh.nic.ip()) else {
        sh.rx.bad.add(1);
        return;
    };
    sh.rx.requests.add(1);

    match req.op {
        OP_INFO => answer_info(sh, &req),
        OP_READ => queue_read(sh, frame, &req, now_units()),
        OP_WRITE => queue_write(sh, frame, &req, now_units()),
        OP_FLUSH => queue_flush(sh, frame, &req, now_units()),
        _ => refuse(sh, unsafe { NetFrame::retain(frame) }, &req, ST_BADREQ),
    }
}

/* Answered on the spot, in the frame the request came in */
fn refuse(sh: &Shared, mut frame: NetFrame, req: &Request, status: u16) {
    sh.rx.refused.add(1);
    let len = write_reply(frame.data_raw_mut(REPLY_HDRS), sh, &req.peer, req.op, req.cookie, req.offset, req.length, status, 0);
    frame.set_len(len);
    sh.nic.transmit(frame);
}

fn answer_info(sh: &Shared, req: &Request) {
    let Some(mut reply) = NetFrame::alloc_tx(REPLY_HDRS + INFO_LEN) else {
        return;
    };

    let buf = reply.data_raw_mut(REPLY_HDRS + INFO_LEN);
    let len = write_reply(buf, sh, &req.peer, OP_INFO, req.cookie, 0, INFO_LEN as u32, ST_OK, INFO_LEN);

    let info = &mut buf[REPLY_HDRS..];
    info[0..8].copy_from_slice(&sh.size.to_be_bytes());
    info[8..12].copy_from_slice(&(sh.sector_size as u32).to_be_bytes());
    info[12..16].copy_from_slice(&(sh.max_io as u32).to_be_bytes());
    info[16..20].copy_from_slice(&(sh.max_read as u32).to_be_bytes());
    info[20..24].copy_from_slice(&(if sh.read_only { INFO_READ_ONLY } else { 0 }).to_be_bytes());
    info[24..28].copy_from_slice(&(SLOTS as u32).to_be_bytes());
    info[28..32].copy_from_slice(&0u32.to_be_bytes());

    reply.set_len(len);
    sh.nic.transmit(reply);
}

/* A read takes a slot for every datagram of its answer; the frame it came in
   is not kept */
fn queue_read(sh: &Shared, frame: usize, req: &Request, arrived: u32) {
    let length = req.length as u64;
    if length == 0 || length > sh.max_read || !sh.aligned(req.offset, length) {
        return refuse(sh, unsafe { NetFrame::retain(frame) }, req, ST_BADREQ);
    }
    if !sh.in_range(req.offset, length) {
        return refuse(sh, unsafe { NetFrame::retain(frame) }, req, ST_RANGE);
    }

    let pieces = ((length + sh.max_io - 1) / sh.max_io) as usize;
    let mut slots = [0usize; READ_PIECES];
    for i in 0..pieces {
        match sh.free.pop() {
            Some(slot) => slots[i] = slot,
            None => {
                for &slot in &slots[..i] {
                    sh.free.push(slot);
                }
                sh.rx.busy.add(1);
                return refuse(sh, unsafe { NetFrame::retain(frame) }, req, ST_BUSY);
            }
        }
    }

    for (i, &slot) in slots[..pieces].iter().enumerate() {
        let at = i as u64 * sh.max_io;
        let s = unsafe { &mut *(slot as *mut Slot) };
        s.frame = 0;
        s.op = OP_READ;
        s.fua = false;
        s.cookie = req.cookie;
        s.offset = req.offset + at;
        s.length = core::cmp::min(sh.max_io, length - at) as u32;
        s.arrived = arrived;
        s.status = ST_OK;
        s.data_off = 0;
        s.peer = req.peer;
        sh.requests.push(slot);
    }
    sh.event.signal();
}

/* A write keeps its frame: the disk takes the data out of it where it lies,
   and it goes back out as the reply */
fn queue_write(sh: &Shared, frame: usize, req: &Request, arrived: u32) {
    let frame = unsafe { NetFrame::retain(frame) };
    if sh.read_only {
        return refuse(sh, frame, req, ST_ROFS);
    }

    let length = req.length as u64;
    if length == 0 || length > sh.max_io || length != req.data_len as u64 || !sh.aligned(req.offset, length) {
        return refuse(sh, frame, req, ST_BADREQ);
    }
    if !sh.in_range(req.offset, length) {
        return refuse(sh, frame, req, ST_RANGE);
    }

    /* Every NIC here puts a frame in a pool frame, 8-byte aligned, and every
       header before the data is whole dwords -- so this holds; were it ever
       not to, the request is refused rather than copied. */
    if (frame.data_phys() + req.data_off as u64) & DWORD_MASK != 0 {
        return refuse(sh, frame, req, ST_BADREQ);
    }

    let Some(slot) = sh.free.pop() else {
        sh.rx.busy.add(1);
        return refuse(sh, frame, req, ST_BUSY);
    };

    let s = unsafe { &mut *(slot as *mut Slot) };
    s.frame = frame.into_raw();
    s.op = OP_WRITE;
    s.fua = req.flags & FLAG_FUA != 0;
    s.cookie = req.cookie;
    s.offset = req.offset;
    s.length = req.length;
    s.arrived = arrived;
    s.status = ST_OK;
    s.data_off = req.data_off as u16;
    s.peer = req.peer;
    sh.requests.push(slot);
    sh.event.signal();
}

fn queue_flush(sh: &Shared, frame: usize, req: &Request, arrived: u32) {
    let frame = unsafe { NetFrame::retain(frame) };
    if sh.read_only {
        return refuse(sh, frame, req, ST_ROFS);
    }

    let Some(slot) = sh.free.pop() else {
        sh.rx.busy.add(1);
        return refuse(sh, frame, req, ST_BUSY);
    };

    let s = unsafe { &mut *(slot as *mut Slot) };
    s.frame = frame.into_raw();
    s.op = OP_FLUSH;
    s.fua = false;
    s.cookie = req.cookie;
    s.offset = req.offset;
    s.length = req.length;
    s.arrived = arrived;
    s.status = ST_OK;
    s.data_off = 0;
    s.peer = req.peer;
    sh.requests.push(slot);
    sh.event.signal();
}

/* ------------------------------------------------------------------ */
/* Replies                                                             */
/* ------------------------------------------------------------------ */

/* The headers in front of a reply's data -- Ethernet, IPv4 without options,
   UDP and netblk's own -- into buf, which holds at least REPLY_HDRS; the
   frame's length back. The UDP checksum is left 0, "not computed", which
   IPv4 allows: computing it would read every byte of the data the disk just
   wrote, the one thing this path never does. */
fn write_reply(
    buf: &mut [u8],
    sh: &Shared,
    peer: &Peer,
    op: u8,
    cookie: u64,
    offset: u64,
    length: u32,
    status: u16,
    payload: usize,
) -> usize {
    let h = &mut buf[..REPLY_HDRS];

    h[0..6].copy_from_slice(&peer.mac);
    h[6..12].copy_from_slice(&sh.mac);
    h[12..14].copy_from_slice(&ETHERTYPE_IP.to_be_bytes());

    let ip = &mut h[ETH_LEN..ETH_LEN + IP_LEN];
    ip[0] = IP_VERSION_IHL;
    ip[1] = 0;
    ip[2..4].copy_from_slice(&((IP_LEN + UDP_LEN + HDR_LEN + payload) as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&0u16.to_be_bytes());
    ip[6..8].copy_from_slice(&IP_DF.to_be_bytes());
    ip[8] = IP_TTL;
    ip[9] = IPPROTO_UDP;
    ip[10..12].copy_from_slice(&0u16.to_be_bytes());
    ip[12..16].copy_from_slice(&peer.local_ip.to_be_bytes());
    ip[16..20].copy_from_slice(&peer.ip.to_be_bytes());
    let checksum = ip_checksum(ip);
    ip[10..12].copy_from_slice(&checksum.to_be_bytes());

    let udp = &mut h[ETH_LEN + IP_LEN..ETH_LEN + IP_LEN + UDP_LEN];
    udp[0..2].copy_from_slice(&sh.port.to_be_bytes());
    udp[2..4].copy_from_slice(&peer.port.to_be_bytes());
    udp[4..6].copy_from_slice(&((UDP_LEN + HDR_LEN + payload) as u16).to_be_bytes());
    udp[6..8].copy_from_slice(&0u16.to_be_bytes());

    let nb = &mut h[ETH_LEN + IP_LEN + UDP_LEN..];
    nb[0..4].copy_from_slice(&MAGIC.to_be_bytes());
    nb[4] = VERSION;
    nb[5] = op | OP_REPLY;
    nb[6..8].copy_from_slice(&0u16.to_be_bytes());
    nb[8..16].copy_from_slice(&cookie.to_be_bytes());
    nb[16..24].copy_from_slice(&offset.to_be_bytes());
    nb[24..28].copy_from_slice(&length.to_be_bytes());
    nb[28..30].copy_from_slice(&status.to_be_bytes());

    REPLY_HDRS + payload
}

#[inline]
fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in header.chunks_exact(2) {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/* ------------------------------------------------------------------ */
/* The disk's interrupt                                                */
/* ------------------------------------------------------------------ */

/* No sleeping, allocating or freeing here: the status into the slot, the
   slot onto `done`, and the worker told. */
extern "C" fn on_disk_done(ctx: *mut u8, status: i32) {
    let slot = ctx as *mut Slot;
    unsafe {
        (*slot).status = if status == 0 { ST_OK } else { ST_IO };
        /* Read before the push: the slot is the worker's the moment it is on
           the ring */
        let sh = &*(*slot).shared;
        /* Cannot fail: the ring has room for every slot */
        sh.done.push(slot as usize);
        sh.event.signal();
    }
}

/* ------------------------------------------------------------------ */
/* The worker                                                          */
/* ------------------------------------------------------------------ */

extern "C" fn worker_main(ctx: *mut u8) {
    let sh = unsafe { &*(ctx as *const Shared) };
    let mut worker = Worker { sh, in_flight: 0, stalled: 0, tx: [0; BATCH], ntx: 0, now: 0 };
    worker.run();
}

enum Issued {
    Submitted,
    /* the disk has no room: try again after a completion */
    Busy,
    /* answer it with this status */
    Failed(u16),
    /* no frame to answer with: dropped, for the client to ask again */
    Dropped,
}

struct Worker<'a> {
    sh: &'a Shared,
    /* at the disk */
    in_flight: usize,
    /* a request the disk had no room for, first in line; 0 for none */
    stalled: usize,
    /* replies to transmit as one batch */
    tx: [usize; BATCH],
    ntx: usize,
    /* the clock, once a pass, for the service times of what it finishes */
    now: u32,
}

/* The physical address of a frame the slot holds, the frame kept */
#[inline]
fn frame_phys(frame: usize) -> u64 {
    ManuallyDrop::new(unsafe { NetFrame::from_raw(frame) }).data_phys()
}

impl Worker<'_> {
    fn run(&mut self) {
        let mut idle_since = 0u64;
        loop {
            self.now = now_units();
            let work = self.reap() + self.submit();
            self.flush_tx();
            self.sh.worker.in_flight.0.store(self.in_flight as u64, Ordering::Relaxed);
            self.sh.worker.stalled.0.store((self.stalled != 0) as u64, Ordering::Relaxed);
            if work != 0 {
                idle_since = 0;
                continue;
            }

            if self.sh.stopping.load(Ordering::Acquire) {
                self.discard();
                if self.in_flight == 0 {
                    return;
                }
            }

            /* A request stalled and nothing of ours at the disk: its queue is
               full of someone else's commands, and no completion of ours will
               say when there is room. Ask again after a pause -- the CPU may
               go idle meanwhile -- rather than hammer the disk's lock. */
            if self.stalled != 0 && self.in_flight == 0 {
                kcore::task::sleep_ms(STALL_RETRY_MS);
                continue;
            }

            /* Owed completions: poll for them a while before sleeping -- a
               wakeup costs an interrupt, an IPI and a context switch -- and,
               with poll=, for the next request too. Polling gives the CPU to
               any other task runnable here, and never to its idle task. Not
               a plain yield: that goes to idle on a CPU with nothing else to
               run, which halts the CPU until the next interrupt that CPU
               takes -- the tick, as often as not, the disk's and the NIC's
               landing on others -- while a completion or a request finding
               this task runnable, not blocked, sends it no IPI. Nor a plain
               spin: the softirq tasks share this CPU, and one the tick
               preempted halfway through the transmit path would wait for the
               next tick to get it back, every reply queued behind the NIC's
               ring waiting with it. */
            let limit = if self.in_flight != 0 {
                POLL_NS.max(self.sh.idle_poll_ns)
            } else {
                self.sh.idle_poll_ns
            };
            if limit != 0 {
                let now = boot_time_ns();
                if idle_since == 0 {
                    idle_since = now;
                }
                if now - idle_since < limit {
                    kcore::task::yield_to_runnable();
                    continue;
                }
            }

            idle_since = 0;
            self.sh.worker.sleeps.add(1);
            self.sh.event.wait();
        }
    }

    /* Completions: each reply written and queued to transmit */
    fn reap(&mut self) -> usize {
        let mut n = 0;
        while n < BATCH {
            let Some(slot) = self.sh.done.pop() else {
                break;
            };
            n += 1;
            self.in_flight -= 1;
            self.finish(slot as *mut Slot);
        }
        n
    }

    /* Requests: to the disk, one doorbell for the lot */
    fn submit(&mut self) -> usize {
        let mut work = 0;
        let mut issued = 0;
        while issued < BATCH {
            let retry = self.stalled != 0;
            let slot = if retry {
                core::mem::replace(&mut self.stalled, 0)
            } else {
                match self.sh.requests.pop() {
                    Some(slot) => slot,
                    None => break,
                }
            };

            match self.issue(slot as *mut Slot) {
                Issued::Submitted => {
                    self.in_flight += 1;
                    issued += 1;
                    work += 1;
                }
                Issued::Busy => {
                    self.stalled = slot;
                    if !retry {
                        self.sh.worker.disk_busy.add(1);
                    }
                    break;
                }
                Issued::Failed(status) => {
                    unsafe { (*(slot as *mut Slot)).status = status };
                    self.finish(slot as *mut Slot);
                    work += 1;
                }
                Issued::Dropped => {
                    self.release(slot as *mut Slot);
                    work += 1;
                }
            }
        }

        if issued != 0 {
            self.sh.disk.kick();
        }
        work
    }

    fn issue(&mut self, slot: *mut Slot) -> Issued {
        let sh = self.sh;
        let s = unsafe { &mut *slot };

        let (op, phys, count) = match s.op {
            OP_READ => {
                if s.frame == 0 {
                    match NetFrame::alloc_tx(REPLY_HDRS + s.length as usize) {
                        Some(frame) => s.frame = frame.into_raw(),
                        None => return Issued::Dropped,
                    }
                }
                /* Straight into the frame, behind the room for the headers */
                (IO_READ, frame_phys(s.frame) + REPLY_HDRS as u64, s.length >> sh.sector_shift)
            }
            /* Straight out of the frame, where the NIC put the data */
            OP_WRITE => (IO_WRITE, frame_phys(s.frame) + s.data_off as u64, s.length >> sh.sector_shift),
            _ => (IO_FLUSH, 0, 0),
        };

        let io = BlockIo {
            op,
            fua: (op == IO_WRITE && s.fua) as u8,
            reserved: 0,
            count,
            sector: s.offset >> sh.sector_shift,
            phys,
            done: on_disk_done,
            ctx: slot as *mut u8,
        };

        match unsafe { sh.disk.submit(&io, false) } {
            Ok(()) => Issued::Submitted,
            Err(SubmitError::Busy) => Issued::Busy,
            Err(_) => Issued::Failed(ST_IO),
        }
    }

    /* The reply, in the slot's own frame, queued; the slot back to the free */
    fn finish(&mut self, slot: *mut Slot) {
        let sh = self.sh;
        let s = unsafe { &mut *slot };

        let ok = s.status == ST_OK;
        let mut payload = 0;
        match s.op {
            OP_READ if ok => {
                payload = s.length as usize;
                sh.worker.reads.add(1);
                sh.worker.read_bytes.add(s.length as u64);
            }
            OP_WRITE if ok => {
                sh.worker.writes.add(1);
                sh.worker.written_bytes.add(s.length as u64);
            }
            OP_FLUSH if ok => sh.worker.flushes.add(1),
            _ => {}
        }
        if s.status == ST_IO {
            sh.worker.errors.add(1);
        }
        /* The clock was read at the top of the pass: a request that arrived
           since and failed at once comes out a hair negative, which is 0 */
        let took = self.now.wrapping_sub(s.arrived);
        sh.worker.record_service(if took > u32::MAX / 2 { 0 } else { took });

        let mut frame = unsafe { NetFrame::from_raw(s.frame) };
        s.frame = 0;
        let len = write_reply(frame.data_raw_mut(REPLY_HDRS), sh, &s.peer, s.op, s.cookie, s.offset, s.length, s.status, payload);
        frame.set_len(len);

        /* Nothing of the slot is read after this */
        sh.free.push(slot as usize);
        self.queue_tx(frame.into_raw());
    }

    /* The slot back to the free, its frame released unanswered */
    fn release(&mut self, slot: *mut Slot) {
        let s = unsafe { &mut *slot };
        if s.frame != 0 {
            drop(unsafe { NetFrame::from_raw(s.frame) });
            s.frame = 0;
        }
        self.sh.free.push(slot as usize);
    }

    /* Stopping: what has not reached the disk is dropped unanswered */
    fn discard(&mut self) {
        if self.stalled != 0 {
            let slot = core::mem::replace(&mut self.stalled, 0);
            self.release(slot as *mut Slot);
        }
        while let Some(slot) = self.sh.requests.pop() {
            self.release(slot as *mut Slot);
        }
    }

    #[inline]
    fn queue_tx(&mut self, frame: usize) {
        self.tx[self.ntx] = frame;
        self.ntx += 1;
        if self.ntx == BATCH {
            self.flush_tx();
        }
    }

    /* The replies so far, to the NIC as one batch: one lock, one doorbell */
    fn flush_tx(&mut self) {
        if self.ntx == 0 {
            return;
        }
        let queued = unsafe { self.sh.nic.transmit_raw(&self.tx[..self.ntx]) };
        if queued < self.ntx {
            self.sh.worker.tx_dropped.add((self.ntx - queued) as u64);
        }
        self.ntx = 0;
    }
}
