//! The world around the machine: the wire to its NIC, the hosts on it, and
//! the programs on the machine that use the network -- as the world task
//! runs them, one input at a time.
//!
//! The world is a task of the machine's (`sched::WORLD`), and the one its
//! soft IRQs run on: what it does between two waits happens with nothing
//! else running, as a CPU of its own would do it. It takes what the machine
//! sent off the wire, checks every frame of it (`check`), hands it to the
//! host it was for (a target's `Peers`), and puts what the hosts send back
//! on the wire when the link says it arrives -- late, twice, never, out of
//! order, as the input's link is. The machine's programs are tasks too
//! (`app`): each a call of a blocking API -- a connect, a resolve, a GET --
//! on a thread of its own.

pub mod check;
pub mod frames;
pub mod httpd;
pub mod lan;
pub mod link;
pub mod sshc;
pub mod tcpm;
pub mod tlsd;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::machine::{self, nic, sched};

pub use link::Link;

/// eth0's address, the gateway's and the DNS server's: QEMU's user
/// network, which is where the kernel's own tests run it.
pub const ETH0_IP: u32 = 0x0A00_020F;
pub const GW_IP: u32 = 0x0A00_0202;
pub const DNS_IP: u32 = 0x0A00_0203;
pub const MASK: u32 = 0xFFFF_FF00;
pub const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];

/* ---- each input ---- */

/// How much of an input is the world's own: the seed of its chaos -- which
/// task goes first, where one is preempted -- and of the entropy pool.
const HEADER: usize = 8;

/// The machine as each input finds it: the clock where it starts, the pool
/// seeded from the input, the chaos chosen.
pub fn begin(data: &[u8]) {
    let mut seed = [0u8; HEADER];
    for (i, b) in data.iter().take(HEADER).enumerate() {
        seed[i] = *b;
    }
    let word = u64::from_le_bytes(seed);
    /* Preemption at a lock let go of: never, for half the inputs, and for
     * the rest one time in 64, 16 or 4. */
    let preempt = match seed[0] {
        0..=127 => 0,
        128..=191 => 64,
        192..=239 => 16,
        _ => 4,
    };
    sched::begin_input(preempt, word);
    machine::seed_random(word);
}

/// The input past the world's header: what the target reads.
pub fn script(data: &[u8]) -> &[u8] {
    data.get(HEADER..).unwrap_or(&[])
}

/* ---- the machine's programs ---- */

/// A call a program on the machine makes, on a task of its own: its answer
/// once it has one.
pub struct App<T> {
    task: usize,
    result: Arc<Mutex<Option<T>>>,
}

impl<T: Send + 'static> App<T> {
    /// `f` on a task of its own, on CPU `cpu`.
    pub fn spawn(name: &str, cpu: u32, f: impl FnOnce() -> T + Send + 'static) -> App<T> {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let task = sched::spawn(name, sched::Kind::App, cpu, Box::new(move || {
            let r = sched::kernel(f);
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        }));
        App { task, result }
    }

    pub fn done(&self) -> bool {
        sched::done(self.task)
    }

    /// Its answer, once it has returned.
    pub fn take(&self) -> Option<T> {
        if !self.done() {
            return None;
        }
        self.result.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub fn task(&self) -> usize {
        self.task
    }
}

/* ---- the wire ---- */

/// What the hosts on the wire do with what the machine sends them: a
/// target's model of the network. `on_frame` is handed every frame the
/// machine sent, checked, and answers through `net`.
pub trait Peers {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]);
    /// The next time a host wants to act on its own -- a retransmit, a
    /// reply it sits on -- or `u64::MAX`.
    fn next_timer(&self) -> u64 {
        u64::MAX
    }
    /// That time has come.
    fn on_timer(&mut self, _net: &mut Net) {}
}

/// The wire between the world and the machine's NIC.
pub struct Net {
    /// Frames on their way to the machine: when each arrives.
    arriving: BTreeMap<(u64, u64), Vec<u8>>,
    order: u64,
    pub link: Link,
    /// Every frame the machine sent, in order, for a target to look back on.
    pub sent: Vec<Vec<u8>>,
    /// What the machine may send as, and to whom it may answer.
    pub expect: check::Expect,
    /// Frames the world has put on the wire for the machine, counted.
    pub delivered: u64,
}

impl Net {
    pub fn new(link: Link) -> Net {
        Net { arriving: BTreeMap::new(), order: 0, link, sent: Vec::new(), expect: check::Expect::default(),
              delivered: 0 }
    }

    /// `frame`, to the machine: when the link says, if it says at all.
    pub fn send(&mut self, frame: Vec<u8>) {
        let now = sched::now();
        for at in self.link.fate(now, frame.len()) {
            self.order += 1;
            self.arriving.insert((at, self.order), frame.clone());
        }
    }

    /// `frame`, to the machine, now and certainly: the attacker's own, or
    /// the one frame a test is about.
    pub fn inject(&mut self, frame: Vec<u8>) {
        self.order += 1;
        self.arriving.insert((sched::now(), self.order), frame);
    }

    /// `frame`, to the machine at `at`.
    pub fn inject_at(&mut self, at: u64, frame: Vec<u8>) {
        self.order += 1;
        self.arriving.insert((at, self.order), frame);
    }

    fn next_arrival(&self) -> u64 {
        self.arriving.keys().next().map_or(u64::MAX, |&(at, _)| at)
    }

    /// What arrives by `now`, onto the NIC; true when anything did.
    fn arrive(&mut self, now: u64) -> bool {
        let mut any = false;
        while let Some((&(at, order), _)) = self.arriving.iter().next() {
            if at > now {
                break;
            }
            let frame = self.arriving.remove(&(at, order)).expect("the first");
            if machine::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("[{:>12}] >> {}", now / 1000, check::describe(&frame));
            }
            nic::wire().inbound[0].push_back(frame);
            self.delivered += 1;
            any = true;
        }
        if any {
            /* The NIC's interrupt: the receive pass is asked for. */
            sched::softirq_raise(kcore::softirq::TYPE_NET_RX);
        }
        any
    }

    pub fn in_flight(&self) -> usize {
        self.arriving.len()
    }
}

/// The world: the wire, and the hosts on it.
pub struct World<P: Peers> {
    pub net: Net,
    pub peers: P,
}

impl<P: Peers> World<P> {
    pub fn new(link: Link, peers: P) -> World<P> {
        World { net: Net::new(link), peers }
    }

    /// Everything that happens now, until nothing more does: the soft IRQs
    /// run, what the machine sent taken, checked and handed to its hosts,
    /// what arrives by now put on the NIC.
    pub fn pump(&mut self) {
        loop {
            sched::run_softirqs();
            let sent: Vec<Vec<u8>> = nic::wire().out[0].drain(..).collect();
            for frame in &sent {
                if machine::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("[{:>12}] << {}", sched::now() / 1000, check::describe(frame));
                }
                check::tx(&self.net.expect, frame);
                self.net.sent.push(frame.clone());
                self.peers.on_frame(&mut self.net, frame);
            }
            let now = sched::now();
            if self.peers.next_timer() <= now {
                self.peers.on_timer(&mut self.net);
            }
            let arrived = self.net.arrive(now);
            if sent.is_empty() && !arrived && !sched::softirq_pending_any() {
                return;
            }
        }
    }

    /// The world lets the machine run for `ns`, doing its part as things
    /// happen.
    pub fn run_for(&mut self, ns: u64) {
        let end = sched::now().saturating_add(ns).min(sched::END);
        self.run_until(end, || false);
    }

    /// The world lets the machine run until `done` says so or `end` comes:
    /// whether `done` did.
    pub fn run_until(&mut self, end: u64, mut done: impl FnMut() -> bool) -> bool {
        loop {
            self.pump();
            if done() {
                return true;
            }
            let now = sched::now();
            if now >= end {
                return false;
            }
            let next = self.net.next_arrival().min(self.peers.next_timer()).min(end);
            sched::world_wait(next.max(now));
        }
    }

    /// Until `app` returns, or `ns` has passed: whether it did.
    pub fn wait_app<T: Send + 'static>(&mut self, app: &App<T>, ns: u64) -> bool {
        let end = sched::now().saturating_add(ns).min(sched::END);
        self.run_until(end, || app.done())
    }
}
