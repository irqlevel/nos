//! `hv forward`: a port of nos's, relayed to a port of a guest's -- how a
//! server in a guest is reached from outside, the guests having addresses
//! only on their switch (10.0.100.0/24, behind `hv0`).
//!
//! Each forward is a task that listens on nos's port -- on the device the
//! guests go out through by NAT, the default route's, which is the one the
//! world reaches them by; `eth0` while NAT is off -- and accepts; each
//! connection goes to a relay task of its own, which opens one to the
//! guest's port through `hv0` and carries what each side sends to the other
//! until either ends (`relay.rs`). The connect is the relay's: one to a
//! guest that does not answer -- booting, hung, dropping SYNs -- waits out
//! TCP's five seconds on that task alone, where the listener once waited it
//! out with every other client and its own stop waiting behind it. The relay
//! alternates between the two with short waits rather than blocking on one,
//! so it needs no second task and nothing shared between two -- and what one
//! side sent waits at the relay for room at the other a short wait at a time,
//! so a side that takes nothing neither holds the other direction back nor
//! keeps a stop waiting: after `RELAY_STALL_NS` of taking nothing it is given
//! up. A forward is to the address its guest had when it was made -- a guest
//! restarted keeps it; one stopped takes it away, and connections to it then
//! fail.
//!
//! What it does not do: a relay ends when either side's stream ends, closing
//! both -- there is no half-close, the kernel's TCP having no way for a
//! program to end its sending and go on reading -- so a client that shuts its
//! sending side and then waits for the answer (`nc -N`, a shell pipeline)
//! gets none; and an idle relay looks at each side every `RELAY_WAIT_MS`
//! rather than sleeping until either has something, the kernel's TCP having
//! no event a task can wait on for two connections at once.
//!
//! The kernel has 64 TCP connections in all and a forwarded one takes two,
//! so a forward relays at most `MAX_CONNS` at once, every forward together
//! at most `MAX_RELAYS` -- what is left of the 64 is nos's own: its sshd, its
//! API, a download -- and the rest are refused. A relay that ends closes its
//! two first, and each then waits out its close in the kernel for up to a
//! minute (TIME-WAIT), counted by no forward: the kernel takes such a slot
//! back for a new connection when it has none free (`net::tcp`'s
//! `reclaim`), so a client that opens and closes through a forward as fast
//! as it likes no longer holds every slot the machine has.
//! Taking a forward away closes its port, and waits out its relays.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kcore::consts::NS_PER_SEC;
use kcore::net::Nic;
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use kcore::tcp::{TcpListener, TcpStream, RECV_TIMEOUT};

use crate::net;
use crate::relay::{self, Ended, Read};

pub const MAX_FORWARDS: usize = 8;
const MAX_CONNS: usize = 8;
/// Relays of every forward together, at most: 24 of the kernel's 64
/// connections. Eight forwards of eight each were 128 once, and a client
/// that held idle connections open through four of them took every one.
const MAX_RELAYS: usize = 12;
/// Relays of every forward running now.
static RELAYS: AtomicUsize = AtomicUsize::new(0);

/// One of `MAX_RELAYS` taken, or false when every one is.
fn take_relay() -> bool {
    let mut n = RELAYS.load(Ordering::Acquire);
    while n < MAX_RELAYS {
        match RELAYS.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(now) => n = now,
        }
    }
    false
}

fn give_relay() {
    RELAYS.fetch_sub(1, Ordering::AcqRel);
}
/// Connections the kernel holds for a forward's port that its listener has
/// not taken yet, at most: it takes each within a wait, so a few are
/// plenty, and a flood of SYNs at the port costs the kernel's 64 connection
/// slots no more than this.
const LISTEN_BACKLOG: usize = 4;
/// How long the listener waits for a connection before it looks at its stop
/// flag; and a relay's wait on each side before it looks at the other.
const ACCEPT_WAIT_MS: u64 = 500;
const RELAY_WAIT_MS: u64 = 5;
/// A side that takes none of what it is sent for this long -- its TCP
/// window shut -- is not coming back for it: the connection is given up, as
/// sshd gives up such a client.
const RELAY_STALL_NS: u64 = 5 * 60 * NS_PER_SEC;
/// The device a forward listens through while NAT is off -- no way out for
/// the guests, no device of the default route's to name: nos's first.
const PUBLIC_IF: &str = "eth0";

/// Where a forward goes: the guest's address and port, the device it is
/// behind (`hv0`), and the device the guests go out through by NAT -- the
/// one it listens on, as the world reaches the guests through it -- when
/// NAT is on (`Switch::outer_nic`).
pub struct To {
    pub guest_ip: u32,
    pub guest_port: u16,
    pub host: Nic,
    pub outer: Option<Nic>,
}

/// What a forward's tasks share.
struct Shared {
    stop: AtomicBool,
    /// Connections being relayed, and all ever accepted, refused and failed,
    /// and given up for a side that took nothing.
    active: AtomicUsize,
    accepted: AtomicU64,
    refused: AtomicU64,
    failed: AtomicU64,
    stalled: AtomicU64,
    /// Where to: the guest's address and port, and the device it is behind.
    guest_ip: u32,
    guest_port: u16,
    host: Nic,
    /// The relays' tasks, joined when the forward goes -- and the finished
    /// ones as new ones start.
    relays: Mutex<Vec<(Arc<AtomicBool>, TaskHandle)>>,
}

/// A forward as the host API reads it (`Forwards::rows`).
pub struct Row {
    pub port: u16,
    pub vm: u32,
    pub guest_ip: u32,
    pub guest_port: u16,
    pub active: usize,
    pub accepted: u64,
    pub refused: u64,
    pub failed: u64,
    pub stalled: u64,
}

struct Forward {
    port: u16,
    vm: u32,
    shared: Arc<Shared>,
    task: Option<TaskHandle>,
}

/// Every forward.
pub struct Forwards {
    table: Mutex<Vec<Forward>>,
}

/// The listener task: accept, and hand the connection to a relay of its
/// own -- which connects to the guest -- within the bounds: nothing here
/// waits on a guest.
struct Listen {
    shared: Arc<Shared>,
    listener: TcpListener,
}

fn listen(start: Listen) {
    let Listen { shared, listener } = start;
    while !shared.stop.load(Ordering::Acquire) {
        let Some(client) = listener.accept(ACCEPT_WAIT_MS) else { continue };
        reap(&shared);
        if shared.active.load(Ordering::Acquire) >= MAX_CONNS || !take_relay() {
            shared.refused.fetch_add(1, Ordering::Relaxed);
            client.abort();
            continue;
        }
        let done = Arc::new(AtomicBool::new(false));
        shared.active.fetch_add(1, Ordering::AcqRel);
        let relay = Relay { shared: shared.clone(), done: done.clone(), a: client };
        match kcore::task::spawn_with("hv/relay", relay, relay_task) {
            /* The relay's hold on the client is gone with the spawn that
             * failed: it is closed. */
            None => {
                shared.active.fetch_sub(1, Ordering::AcqRel);
                give_relay();
                shared.failed.fetch_add(1, Ordering::Relaxed);
            }
            Some(task) => {
                let mut relays = shared.relays.lock();
                if relays.try_reserve(1).is_ok() {
                    relays.push((done, task));
                } else {
                    /* No room to keep it: joined now, which waits out the
                     * connection -- out of the lock. */
                    drop(relays);
                    drop(task);
                }
            }
        }
    }
    /* The listener closes with this task: its port answers no more. */
}

/// Join the relays that have finished, out of the lock.
fn reap(shared: &Shared) {
    let mut finished = Vec::new();
    {
        let mut relays = shared.relays.lock();
        let mut i = 0;
        while i < relays.len() {
            if relays[i].0.load(Ordering::Acquire) && finished.try_reserve(1).is_ok() {
                finished.push(relays.swap_remove(i));
            } else {
                i += 1;
            }
        }
    }
    drop(finished);
}

/// One connection's client side, accepted: the guest's is the relay's to
/// open.
struct Relay {
    shared: Arc<Shared>,
    done: Arc<AtomicBool>,
    a: TcpStream,
}

/// A connection as the relay drives it: its waits short, never forever.
impl relay::Side for TcpStream {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Read {
        /* 0 would be forever to the kernel */
        let n = TcpStream::recv(self, buf, wait_ms.max(1));
        if n > 0 {
            Read::Data(n as usize)
        } else if n == RECV_TIMEOUT {
            Read::Idle
        } else {
            Read::End
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        let n = TcpStream::send(self, buf, wait_ms.max(1));
        if n < 0 { None } else { Some(n as usize) }
    }
}

fn relay_task(relay: Relay) {
    let Relay { shared, done, mut a } = relay;
    /* The guest's side, opened here: a guest that does not answer is
     * waited out by this connection alone -- and by the forward's end,
     * which joins this task, for TCP's connect time at most. */
    match TcpStream::connect(&shared.host, shared.guest_ip, shared.guest_port) {
        Some(mut b) => {
            shared.accepted.fetch_add(1, Ordering::Relaxed);
            let stop = || shared.stop.load(Ordering::Acquire);
            /* An end, a failure or a stall on either side: both close. */
            if relay::relay(&mut a, &mut b, RELAY_WAIT_MS, RELAY_STALL_NS, &kcore::time::boot_time_ns, &stop)
                == Ended::Stalled
            {
                shared.stalled.fetch_add(1, Ordering::Relaxed);
                kcore::klog!(0, "hv: a forwarded connection given up -- a side took nothing for {} s",
                              RELAY_STALL_NS / NS_PER_SEC);
            }
            drop(a);
            drop(b);
        }
        None => {
            shared.failed.fetch_add(1, Ordering::Relaxed);
            a.abort();
        }
    }
    shared.active.fetch_sub(1, Ordering::AcqRel);
    give_relay();
    done.store(true, Ordering::Release);
}

impl Forwards {
    pub fn new() -> Option<Forwards> {
        let mut table = Vec::new();
        table.try_reserve_exact(MAX_FORWARDS).ok()?;
        Some(Forwards { table: Mutex::new(table)? })
    }

    /// `hv forward add <port> <vm> <guest-port>`: nos's `port` relayed to
    /// VM `vm`'s guest as `to` says -- said on `out` once it listens, with
    /// the address it listens on -- or why not. `listed` says whether the VM
    /// is on the VMs' list still: asked under this table's lock as the
    /// forward goes into it (`Vms::on_list`). It listens on the device the
    /// guests go out through: on a machine whose uplink is not its first
    /// NIC, a forward on `eth0` said it was made and was reached by nobody.
    pub fn add(&self, port: u16, vm: u32, to: To, listed: &dyn Fn() -> bool, out: &mut dyn Write)
               -> Result<(), String> {
        let To { guest_ip, guest_port, host, outer } = to;
        let public = match outer {
            Some(nic) => nic,
            None => Nic::find(PUBLIC_IF).ok_or_else(|| alloc::format!("no {} to listen through, and no way out for \
                                                                       the guests to name another", PUBLIC_IF))?,
        };
        /* Asked under the lock, said after it: nothing is printed with the
         * table held, a VM's stop taking its forwards away waits on it. */
        let (full, taken) = {
            let table = self.table.lock();
            (table.len() >= MAX_FORWARDS, table.iter().any(|f| f.port == port))
        };
        if full {
            return Err(alloc::format!("{} forwards already", MAX_FORWARDS));
        }
        if taken {
            return Err(alloc::format!("port {} is forwarded already", port));
        }
        /* Bulk: a forwarded port carries a guest's downloads and uploads to
         * the other end of a long round trip, which the 8 KiB of a small
         * connection held to 120 KiB/s at the EX44's 62 ms */
        let listener = TcpListener::bind_bulk(&public, port, LISTEN_BACKLOG)
            .map_err(|e| alloc::format!("port {} would not listen -- {}", port, e))?;
        let relays = Mutex::new(Vec::new()).ok_or_else(|| String::from("out of memory"))?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            accepted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            stalled: AtomicU64::new(0),
            guest_ip,
            guest_port,
            host,
            relays,
        });
        let task = kcore::task::spawn_with("hv/forward", Listen { shared: shared.clone(), listener }, listen)
            .ok_or_else(|| String::from("no task for the forward"))?;
        let forward = Forward { port, vm, shared, task: Some(task) };
        let refused = {
            let mut table = self.table.lock();
            if table.len() >= MAX_FORWARDS || table.iter().any(|f| f.port == port) {
                Some((forward, "was forwarded meanwhile"))
            } else if !listed() {
                /* Asked here, as it goes in, under this lock: a VM's stop
                 * takes it off the VMs' list and only then calls `drop_vm`,
                 * which takes this lock -- so a forward put in while the VM
                 * is listed is that `drop_vm`'s to take, and one asked for
                 * after it left is refused. Asked before, with nothing held
                 * between, one could land after the `drop_vm`: kept for a
                 * number no stop names again, relaying nos's port to
                 * whichever VM took that switch port next. */
                Some((forward, "is not forwarded: the vm was stopped meanwhile"))
            } else {
                /* Into the room taken at `new`. */
                table.push(forward);
                None
            }
        };
        if let Some((f, why)) = refused {
            end(f);
            return Err(alloc::format!("port {} {}", port, why));
        }
        let _ = writeln!(out, "hv: port {} forwarded to vm {}, {}:{} -- listened on at {}", port, vm,
                         net::dotted(guest_ip), guest_port, net::dotted(public.ip()));
        Ok(())
    }

    /// `hv forward del <port>`: the forward of nos's `port` taken away, its
    /// listener and relays ended -- the VM it was to's number, or why not.
    pub fn del(&self, port: u16) -> Result<u32, String> {
        let forward = {
            let mut table = self.table.lock();
            table.iter().position(|f| f.port == port).map(|i| table.swap_remove(i))
        };
        let forward = forward.ok_or_else(|| alloc::format!("port {} is not forwarded", port))?;
        let vm = forward.vm;
        end(forward);
        Ok(vm)
    }

    /// The VM nos's `port` is forwarded to, if it is.
    pub fn vm_of(&self, port: u16) -> Option<u32> {
        self.table.lock().iter().find(|f| f.port == port).map(|f| f.vm)
    }

    /// Every forward as the host API reads it, copied out under the lock --
    /// with nothing allocated there, the table holding MAX_FORWARDS at most
    /// -- its counters read after.
    pub fn rows(&self) -> Vec<Row> {
        let mut held: [Option<(u16, u32, Arc<Shared>)>; MAX_FORWARDS] = Default::default();
        {
            let table = self.table.lock();
            for (row, f) in held.iter_mut().zip(table.iter()) {
                *row = Some((f.port, f.vm, f.shared.clone()));
            }
        }
        let mut rows = Vec::new();
        if rows.try_reserve_exact(MAX_FORWARDS).is_err() {
            return rows;
        }
        for (port, vm, s) in held.iter().filter_map(Option::as_ref) {
            rows.push(Row {
                port: *port,
                vm: *vm,
                guest_ip: s.guest_ip,
                guest_port: s.guest_port,
                active: s.active.load(Ordering::Relaxed),
                accepted: s.accepted.load(Ordering::Relaxed),
                refused: s.refused.load(Ordering::Relaxed),
                failed: s.failed.load(Ordering::Relaxed),
                stalled: s.stalled.load(Ordering::Relaxed),
            });
        }
        rows
    }

    /// `hv forward`.
    pub fn list(&self, out: &mut dyn Write) {
        /* Copied out under the lock -- with nothing allocated, the table
         * holding MAX_FORWARDS at most -- and printed after it. */
        let mut rows: [Option<(u16, u32, Arc<Shared>)>; MAX_FORWARDS] = Default::default();
        {
            let table = self.table.lock();
            for (row, f) in rows.iter_mut().zip(table.iter()) {
                *row = Some((f.port, f.vm, f.shared.clone()));
            }
        }
        if rows[0].is_none() {
            let _ = writeln!(out, "hv: no forwards");
            return;
        }
        for (port, vm, s) in rows.iter().filter_map(Option::as_ref) {
            let _ = writeln!(out, "port {} -> vm {} {}:{}  {} active, {} accepted, {} refused, {} failed, \
                                   {} given up stalled",
                port, vm, net::dotted(s.guest_ip), s.guest_port, s.active.load(Ordering::Relaxed),
                s.accepted.load(Ordering::Relaxed), s.refused.load(Ordering::Relaxed),
                s.failed.load(Ordering::Relaxed), s.stalled.load(Ordering::Relaxed));
        }
    }

    /// Every forward to `vm` taken away, and how many there were: the VM is
    /// stopped and off the list, and its port of the switch -- its MAC and
    /// its address -- goes to the next VM started, which a forward kept by
    /// the address would relay to. Taken off the table under its lock with
    /// nothing allocated, and ended after, as `del` ends one.
    pub fn drop_vm(&self, vm: u32) -> usize {
        let mut gone: [Option<Forward>; MAX_FORWARDS] = Default::default();
        let mut n = 0;
        {
            let mut table = self.table.lock();
            let mut i = 0;
            /* The table holds MAX_FORWARDS at most, so `gone` holds them. */
            while i < table.len() && n < MAX_FORWARDS {
                if table[i].vm == vm {
                    gone[n] = Some(table.swap_remove(i));
                    n += 1;
                } else {
                    i += 1;
                }
            }
        }
        for f in gone.iter_mut().filter_map(Option::take) {
            end(f);
        }
        n
    }

    /// Every forward taken away: the module is going.
    pub fn close(&self) {
        loop {
            let forward = self.table.lock().pop();
            match forward {
                Some(f) => end(f),
                None => return,
            }
        }
    }
}

/// A forward taken off the table, ended: its listener and relays told to
/// stop, and waited for.
fn end(mut f: Forward) {
    f.shared.stop.store(true, Ordering::Release);
    drop(f.task.take());
    let relays = core::mem::take(&mut *f.shared.relays.lock());
    drop(relays);
}

/// `hv forward ...` as typed: the words after `forward`.
pub fn usage() -> String {
    String::from("hv forward [add <port> <vm> <guest-port> | del <port>]")
}
