//! What the CPUs of one guest reach each other by: a doorbell that has a
//! vCPU look at what is waiting for it, and a mailbox of what is.
//!
//! A guest CPU is a task of its own (`crate::run`), in its guest or halted
//! and asleep, on a host CPU of its own. Another of the guest's CPUs that
//! sends it an IPI -- or the host with something for it, a frame, a disk's
//! answer, a key typed -- cannot touch its state: that is its task's alone,
//! and may be in the CPU under a VMCB or a VMCS that very moment. So what is
//! sent is left in the target's mailbox, lock-free, and the target is rung:
//! its task woken if it waits, the CPU it runs its guest on interrupted if
//! it is in there (`Kick`), and either way it takes its mail before it next
//! enters. The ordering is `Kick`'s: the vCPU marks itself on its way in
//! before its last look at the mailbox, and a sender puts the mail in
//! before it rings -- so a message is found by that look, or rings a vCPU
//! that is in its guest or about to be, which then leaves it at once.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use hvarch::x86::svm::Kick;
use kcore::sync::Event;
use kcore::time::{self, Duration};
use kcore::trace::RingHow;

use crate::tp;

/// What wakes one vCPU: its task's event, for when it has nothing to run
/// and waits, and its kick, for when it is in its guest -- and a word that
/// says it was rung, for a halted one that spins a while before it waits
/// (`poll`), and one that says it is spinning so, which a ring then needs
/// not wake: the word is all a spinner looks at.
pub struct Doorbell {
    event: Event,
    kick: Kick,
    rung: AtomicBool,
    polling: AtomicBool,
    /// The CPU's number in its guest, which a record of a ring says.
    index: u32,
}

impl Doorbell {
    pub fn new(index: u32) -> Option<Doorbell> {
        Some(Doorbell { event: Event::new()?, kick: Kick::new(), rung: AtomicBool::new(false),
                        polling: AtomicBool::new(false), index })
    }

    /// Something waits for the vCPU, put where it looks before this: its
    /// task woken if it waits, and it leaves its guest if it is in there.
    /// From any context, interrupts off included.
    ///
    /// A vCPU spinning in `poll` sees the word, and its event is not
    /// signalled -- a schedule's worth of work for a task that is not asleep
    /// -- as KVM wakes no vCPU that polls. The word is stored, and only then
    /// the spinner looked for; `poll` stops looking, and only then looks at
    /// the word once more: of the two, one sees the other, so a ring that
    /// finds the spinner gone has the event signalled for the wait it goes
    /// to, and one that finds it spinning is seen by it.
    ///
    /// Its `hv_ring` is recorded before the vCPU is woken or kicked, so that
    /// a record of what it does about the ring comes after it.
    pub fn ring(&self) {
        self.rung.store(true, Ordering::SeqCst);
        let polling = self.polling.load(Ordering::SeqCst);
        tp::ring(self.index, if polling { RingHow::Polling } else { RingHow::Rung });
        if !polling {
            self.event.signal();
        }
        self.kick.kick();
    }

    /// Wake the vCPU's task if it waits, and leave a guest that runs to its
    /// next exit: for what can wait that long -- a key typed at its console.
    pub fn signal(&self) {
        self.rung.store(true, Ordering::Release);
        tp::ring(self.index, RingHow::Signalled);
        self.event.signal();
    }

    /// The vCPU's, after `Kick::prepare` and before its look at what waits
    /// for it: whether it was rung since the last look, the word cleared --
    /// what rang before is found by the look, and what rings after rings it
    /// again, and turns its entry back.
    pub fn take_rung(&self) -> bool {
        self.rung.swap(false, Ordering::SeqCst)
    }

    /// The vCPU's task, halted: spin for up to `ns` until rung -- on a CPU
    /// that is its guest's own, which nothing else waits for -- rather than
    /// sleep for a wake that comes that soon, and pay a wakeup and a
    /// schedule for it. True when rung. The event the ring signalled keeps
    /// the signal, so the next `wait` comes back at once: one look round the
    /// loop, which finds nothing. What was rung for is found by the loop's
    /// own look after either, as after `wait`: the word only stops the spin.
    pub fn poll(&self, ns: u64) -> bool {
        self.polling.store(true, Ordering::SeqCst);
        let end = time::boot_time_ns().saturating_add(ns);
        let rung = loop {
            if self.rung.swap(false, Ordering::Acquire) {
                break true;
            }
            if time::boot_time_ns() >= end {
                break false;
            }
            core::hint::spin_loop();
        };
        self.polling.store(false, Ordering::SeqCst);
        /* A ring between the last look and the store above found it spinning
         * and signalled nothing: looked for once more, now that a ring
         * signals the event again. */
        rung || self.rung.swap(false, Ordering::SeqCst)
    }

    /// The vCPU's task, with nothing to run: wait until rung, or until `ns`
    /// have passed. True when rung.
    pub fn wait(&self, ns: u64) -> bool {
        let rung = self.event.wait_for(Duration::from_nanos(ns));
        self.rung.store(false, Ordering::Relaxed);
        rung
    }

    /// The vCPU's task with no guest at all to run -- parked between one
    /// boot and the next: until rung.
    pub fn wait_forever(&self) {
        self.event.wait();
    }

    /// What its entries are marked by (`Kick`).
    pub fn kick(&self) -> &Kick {
        &self.kick
    }
}

/// The most CPUs a guest has: its x2APIC IDs are one byte in CPUID leaf 1,
/// its every CPU one bit of a `u64` here, and each is a task and a host CPU
/// of its own.
pub const MAX_VCPUS: usize = 16;

/// A guest's doorbells, one per CPU, made with the VM and kept for its life
/// -- across its reboots, since whoever rings them (the switch with a frame,
/// a command with a key) knows the VM and not the boot.
pub struct Doorbells {
    cpus: Vec<Doorbell>,
}

impl Doorbells {
    /// One for each of `cpus` CPUs, 1 to [`MAX_VCPUS`]; None when that is out
    /// of range or there is no memory.
    pub fn new(cpus: usize) -> Option<Doorbells> {
        if cpus == 0 || cpus > MAX_VCPUS {
            return None;
        }
        let mut v = Vec::new();
        v.try_reserve_exact(cpus).ok()?;
        for i in 0..cpus {
            v.push(Doorbell::new(u32::try_from(i).ok()?)?);
        }
        Some(Doorbells { cpus: v })
    }

    pub fn len(&self) -> usize {
        self.cpus.len()
    }

    /// CPU `i`'s. The boot CPU's is 0, and it is also the one the host's
    /// devices ring: the CPU that takes the 8259's interrupts, and whose
    /// task serves the guest's devices.
    pub fn get(&self, i: usize) -> Option<&Doorbell> {
        self.cpus.get(i)
    }

    /// Every CPU's: for a stop, which each has to see.
    pub fn ring_all(&self) {
        for d in &self.cpus {
            d.ring();
        }
    }

    /// Interrupts their kicks have sent, over every CPU.
    pub fn kicks(&self) -> u64 {
        self.cpus.iter().map(|d| d.kick.sent()).sum()
    }
}

/* An INIT and a start-up IPI waiting, as the mailbox keeps them, in one
 * word: the start-up IPI's vector, a bit so that vector 0 is a message too,
 * and the INIT's bit. One word, so that a take is one swap of it -- with
 * two, one of the two orders of a post against a take was always open: a
 * start-up IPI sent before an INIT could be taken after it. */
const SIPI_PENDING: u32 = 1 << 8;
const SIPI_VECTOR: u32 = 0xFF;
const INIT_PENDING: u32 = 1 << 9;

/// What other CPUs have sent one CPU and it has not yet taken: fixed
/// interrupts, a bit a vector, and which of them an IO-APIC's
/// level-triggered pin sent; an NMI; an INIT; a start-up IPI's vector.
/// Written by any of the guest's CPUs at once, read by its own, all
/// lock-free.
pub struct Mailbox {
    /// Edge-triggered fixed interrupts, a bit a vector.
    edge: [AtomicU64; 4],
    /// Level-triggered ones -- an IO-APIC's level pin -- in a bitmap of their
    /// own: each a `fetch_or` to post and a `swap` to take, with nothing
    /// held between the two words. Kept as a subset of one shared word, a
    /// level bit could go with an earlier edge post of its vector and the
    /// vector be handed on as edge-triggered.
    level: [AtomicU64; 4],
    nmi: AtomicBool,
    /// The INIT and the start-up IPI (`INIT_PENDING`, `SIPI_PENDING`).
    start: AtomicU32,
}

/// What a CPU took out of its mailbox.
pub struct Mail {
    pub fixed: [u64; 4],
    /// Which of `fixed` are level-triggered.
    pub level: [u64; 4],
    pub nmi: bool,
    pub init: bool,
    /// A start-up IPI's vector.
    pub sipi: Option<u8>,
}

impl Mail {
    pub fn is_empty(&self) -> bool {
        self.fixed == [0; 4] && !self.nmi && !self.init && self.sipi.is_none()
    }
}

impl Mailbox {
    pub const fn new() -> Mailbox {
        Mailbox {
            edge: [const { AtomicU64::new(0) }; 4],
            level: [const { AtomicU64::new(0) }; 4],
            nmi: AtomicBool::new(false),
            start: AtomicU32::new(0),
        }
    }

    pub fn post_fixed(&self, vector: u8) {
        self.edge[usize::from(vector / 64)].fetch_or(1 << (vector % 64), Ordering::AcqRel);
    }

    /// A fixed interrupt from a level-triggered pin.
    pub fn post_level(&self, vector: u8) {
        self.level[usize::from(vector / 64)].fetch_or(1 << (vector % 64), Ordering::AcqRel);
    }

    pub fn post_nmi(&self) {
        self.nmi.store(true, Ordering::Release);
    }

    /// An INIT: it forgets any start-up IPI sent before it, which a CPU
    /// that was not waiting for one when it came ignored -- one store of
    /// the word, the INIT's bit alone.
    pub fn post_init(&self) {
        self.start.store(INIT_PENDING, Ordering::Release);
    }

    /// A start-up IPI, in place of any sent before it and beside an INIT
    /// that waits: the INIT's bit kept.
    pub fn post_sipi(&self, vector: u8) {
        let sipi = SIPI_PENDING | u32::from(vector);
        let mut old = self.start.load(Ordering::Acquire);
        loop {
            match self.start.compare_exchange_weak(old, (old & INIT_PENDING) | sipi, Ordering::AcqRel,
                                                   Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => old = now,
            }
        }
    }

    /// Whether anything waits in it, taken out or not: what a CPU going to
    /// sleep looks at after it has said so (`crate::run`).
    pub fn has_mail(&self) -> bool {
        self.start.load(Ordering::Acquire) != 0
            || self.nmi.load(Ordering::Acquire)
            || self.edge.iter().any(|w| w.load(Ordering::Acquire) != 0)
            || self.level.iter().any(|w| w.load(Ordering::Acquire) != 0)
    }

    /// Everything posted so far, taken out. What a CPU does with it is in
    /// the order a real one would: the INIT first -- it resets the CPU --
    /// and then the start-up IPI it waits for; a start-up IPI posted before
    /// the INIT was forgotten by `post_init`, and one posted after it is
    /// taken with it, to be honoured after it. Each word one swap, nothing
    /// to order between two.
    pub fn take(&self) -> Mail {
        let start = self.start.swap(0, Ordering::AcqRel);
        let mut fixed = [0u64; 4];
        let mut level = [0u64; 4];
        for w in 0..4 {
            /* A look before each swap: most of the time there is nothing,
             * and a load leaves the line shared where a swap would take it. */
            let edge = if self.edge[w].load(Ordering::Acquire) != 0 { self.edge[w].swap(0, Ordering::AcqRel) } else { 0 };
            level[w] = if self.level[w].load(Ordering::Acquire) != 0 { self.level[w].swap(0, Ordering::AcqRel) } else { 0 };
            /* A vector posted both ways before this take is taken once, as
             * the level one: what needs its end of interrupt said */
            fixed[w] = edge | level[w];
        }
        Mail {
            fixed,
            level,
            nmi: self.nmi.swap(false, Ordering::AcqRel),
            init: start & INIT_PENDING != 0,
            sipi: (start & SIPI_PENDING != 0).then_some((start & SIPI_VECTOR) as u8),
        }
    }
}
