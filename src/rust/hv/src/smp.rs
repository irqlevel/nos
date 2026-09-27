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
use kcore::time::Duration;

/// What wakes one vCPU: its task's event, for when it has nothing to run
/// and waits, and its kick, for when it is in its guest.
pub struct Doorbell {
    event: Event,
    kick: Kick,
}

impl Doorbell {
    pub fn new() -> Option<Doorbell> {
        Some(Doorbell { event: Event::new()?, kick: Kick::new() })
    }

    /// Something waits for the vCPU, put where it looks before this: its
    /// task woken if it waits, and it leaves its guest if it is in there.
    /// From any context, interrupts off included.
    pub fn ring(&self) {
        self.event.signal();
        self.kick.kick();
    }

    /// Wake the vCPU's task if it waits, and leave a guest that runs to its
    /// next exit: for what can wait that long -- a key typed at its console.
    pub fn signal(&self) {
        self.event.signal();
    }

    /// The vCPU's task, with nothing to run: wait until rung, or until `ns`
    /// have passed. True when rung.
    pub fn wait(&self, ns: u64) -> bool {
        self.event.wait_for(Duration::from_nanos(ns))
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
        for _ in 0..cpus {
            v.push(Doorbell::new()?);
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

/* A start-up IPI waiting, as the mailbox keeps it: the vector, and a bit so
 * that vector 0 is a message too. */
const SIPI_PENDING: u32 = 1 << 8;
const SIPI_VECTOR: u32 = 0xFF;

/// What other CPUs have sent one CPU and it has not yet taken: fixed
/// interrupts, a bit a vector; an NMI; an INIT; a start-up IPI's vector.
/// Written by any of the guest's CPUs at once, read by its own, all
/// lock-free.
pub struct Mailbox {
    fixed: [AtomicU64; 4],
    nmi: AtomicBool,
    init: AtomicBool,
    sipi: AtomicU32,
}

/// What a CPU took out of its mailbox.
pub struct Mail {
    pub fixed: [u64; 4],
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
            fixed: [const { AtomicU64::new(0) }; 4],
            nmi: AtomicBool::new(false),
            init: AtomicBool::new(false),
            sipi: AtomicU32::new(0),
        }
    }

    pub fn post_fixed(&self, vector: u8) {
        self.fixed[usize::from(vector / 64)].fetch_or(1 << (vector % 64), Ordering::AcqRel);
    }

    pub fn post_nmi(&self) {
        self.nmi.store(true, Ordering::Release);
    }

    /// An INIT: it forgets any start-up IPI sent before it, which a CPU
    /// that was not waiting for one when it came ignored.
    pub fn post_init(&self) {
        self.sipi.store(0, Ordering::Release);
        self.init.store(true, Ordering::Release);
    }

    pub fn post_sipi(&self, vector: u8) {
        self.sipi.store(SIPI_PENDING | u32::from(vector), Ordering::Release);
    }

    /// Whether anything waits in it, taken out or not: what a CPU going to
    /// sleep looks at after it has said so (`crate::run`).
    pub fn has_mail(&self) -> bool {
        self.sipi.load(Ordering::Acquire) != 0
            || self.init.load(Ordering::Acquire)
            || self.nmi.load(Ordering::Acquire)
            || self.fixed.iter().any(|w| w.load(Ordering::Acquire) != 0)
    }

    /// Everything posted so far, taken out. What a CPU does with it is in
    /// the order a real one would: the INIT first -- it resets the CPU --
    /// and then the start-up IPI it waits for; a start-up IPI posted before
    /// the INIT was forgotten by `post_init`.
    ///
    /// The start-up IPI is taken before the INIT, the reverse of the order
    /// they are posted in: an INIT and a start-up IPI posted between the two
    /// swaps then leave the start-up IPI for the next take, after the INIT
    /// it follows -- where the other order would take the start-up IPI now,
    /// to a CPU not yet waiting for one, and lose it.
    pub fn take(&self) -> Mail {
        let sipi = self.sipi.swap(0, Ordering::AcqRel);
        let init = self.init.swap(false, Ordering::AcqRel);
        let mut fixed = [0u64; 4];
        for (w, word) in self.fixed.iter().enumerate() {
            /* A look before the swap: most of the time there is nothing,
             * and a load leaves the line shared where a swap would take it. */
            if word.load(Ordering::Acquire) != 0 {
                fixed[w] = word.swap(0, Ordering::AcqRel);
            }
        }
        Mail {
            fixed,
            nmi: self.nmi.swap(false, Ordering::AcqRel),
            init,
            sipi: (sipi & SIPI_PENDING != 0).then_some((sipi & SIPI_VECTOR) as u8),
        }
    }
}
