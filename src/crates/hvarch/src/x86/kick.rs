//! A vCPU's kick: how another CPU has a guest that runs leave it, and how
//! its entry is turned back -- for work that came on its way in, or an edge
//! of its timers -- and the host's timer armed for that edge as it is made.
//! Nothing here touches the CPU's virtualization: it is atomics and the
//! kernel's calls, and hv-fuzz runs it as it is.

use core::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// What another CPU does to have a guest that is running leave it -- the
/// guests' switch with a frame for it, a disk's task with what it asked --
/// and what [`super::svm::Guest::run`] looks at, with interrupts off, before it enters:
/// KVM's vCPU mode, as its kick uses it.
///
/// A guest's turn ends when the host takes an interrupt, and not before.
/// Work handed to a vCPU that is in its guest waits for that -- the host's
/// next tick, milliseconds, while a busy guest's inbox overflows -- unless
/// its CPU is interrupted now. The vCPU's side, in this order: `prepare`,
/// then its last look at what may be waiting for the guest, then
/// [`super::svm::Guest::run`] -- which enters only if no kick came meanwhile, and marks
/// the vCPU outside again on the way back -- or `cancel`, when it does not
/// enter at all. The other side: what it has for the guest made visible,
/// then `kick`. The vCPU's last look finds the work, or the kick finds the
/// vCPU in its guest or on its way in: then its CPU is interrupted, which
/// ends the guest's turn -- or, taken by the host before interrupts went
/// off there, leaves the entry to be refused. One interrupt at most per
/// entry, however many kick.
pub struct Kick {
    mode: AtomicU8,
    /// The CPU the vCPU last entered its guest on.
    cpu: AtomicU32,
    /// Interrupts sent: kicks that found the vCPU in its guest or on its
    /// way in.
    sent: AtomicU64,
    /// When the host's timer is to take the guest out, host nanoseconds on
    /// the boot clock -- its timers' next edge -- armed as its next entry is
    /// made (`arm_at`); 0 for none. The vCPU's own.
    edge: AtomicU64,
    /// Entries turned back because that edge had come by then.
    edges_passed: AtomicU64,
}

/* A vCPU's modes. */
const OUTSIDE_GUEST: u8 = 0;
const IN_GUEST: u8 = 1;
const EXITING_GUEST: u8 = 2;

impl Kick {
    pub const fn new() -> Kick {
        Kick {
            mode: AtomicU8::new(OUTSIDE_GUEST),
            cpu: AtomicU32::new(0),
            sent: AtomicU64::new(0),
            edge: AtomicU64::new(0),
            edges_passed: AtomicU64::new(0),
        }
    }

    /// The vCPU's, before it enters: the host's timer to take the guest out
    /// at `at` -- its timers' next edge -- armed as the entry is made, with
    /// interrupts off on the CPU it is made on and nothing left to run before
    /// it: an interrupt taken between an arming in the loop's round and the
    /// entry re-armed the timer for the host's next tick in its handler, and
    /// a guest computing past the edge was taken out only then, 10 ms late on
    /// the EX44. And the entry turned back if the edge has come by then, for
    /// the loop's round to take it (`edges_passed`). None for no edge.
    pub fn arm_at(&self, at: Option<u64>) {
        self.edge.store(at.unwrap_or(0), Ordering::Relaxed);
    }

    /// Entries turned back because the edge they were to be taken out at
    /// had come.
    pub fn edges_passed(&self) -> u64 {
        self.edges_passed.load(Ordering::Relaxed)
    }

    /// The edge the next entry arms the host's timer for, as `arm_at` last
    /// named it.
    pub fn edge(&self) -> Option<u64> {
        let edge = self.edge.load(Ordering::Relaxed);
        (edge != 0).then_some(edge)
    }

    /// How many kicks interrupted the vCPU's CPU.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// The vCPU's: from here a kick refuses its next entry. Before its last
    /// look at what may be waiting for the guest -- the fence orders the two,
    /// against `kick`'s.
    pub fn prepare(&self) {
        self.mode.store(IN_GUEST, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// The vCPU's: it will not enter after all -- it halts, or stops.
    pub fn cancel(&self) {
        self.mode.store(OUTSIDE_GUEST, Ordering::SeqCst);
    }

    /// Another CPU's, from any context: something is waiting for the guest,
    /// made visible before this.
    pub fn kick(&self) {
        fence(Ordering::SeqCst);
        if self.mode.compare_exchange(IN_GUEST, EXITING_GUEST, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            kcore::cpu::kick(self.cpu.load(Ordering::SeqCst));
            self.sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Another CPU's, from any context, once it has posted an interrupt for
    /// the vCPU (`vmx::PostedInterrupts::post`): the posted-interrupt
    /// notification to the CPU the vCPU is in its guest on, or on its way
    /// in to -- which takes what was posted there without an exit -- and
    /// true; false when it is neither, for the caller to wake it instead.
    /// A vCPU on its way in looks at what was posted after `prepare`, so it
    /// either finds the interrupt or is sent the notification; one that
    /// left meanwhile takes the notification as a host interrupt with
    /// nothing to do, and what was posted at its next look.
    pub fn notify(&self) -> bool {
        fence(Ordering::SeqCst);
        if self.mode.load(Ordering::SeqCst) != IN_GUEST {
            return false;
        }
        kcore::cpu::notify(self.cpu.load(Ordering::SeqCst));
        true
    }

    /// [`super::svm::Guest::run`]'s, with interrupts off on `cpu`: whether to enter.
    /// The CPU is stored before the mode is read, so that a kick that finds
    /// the vCPU in its guest after this interrupts the CPU it is on. The
    /// host's timer armed for the edge `arm_at` named, or the entry turned
    /// back if it has come: interrupts stay off from here until the guest
    /// runs, so nothing re-arms the timer meanwhile, and one due by then
    /// takes the guest out at once.
    pub(crate) fn entering(&self, cpu: u32) -> bool {
        self.cpu.store(cpu, Ordering::SeqCst);
        if self.mode.load(Ordering::SeqCst) != IN_GUEST {
            self.mode.store(OUTSIDE_GUEST, Ordering::SeqCst);
            return false;
        }
        let edge = self.edge.load(Ordering::Relaxed);
        if edge != 0 {
            /* Armed, and only then looked at: `timer_at` arms nothing for an
             * edge gone by, and one that went by between a look first and
             * the arming after it -- a few hundred nanoseconds -- left the
             * guest to compute past it until the host's next tick, 10 ms,
             * the EX44's latest. Looked at after, an edge still to come was
             * still to come when it was armed. */
            kcore::cpu::timer_at(edge);
            if edge <= kcore::time::boot_time_ns() {
                self.mode.store(OUTSIDE_GUEST, Ordering::SeqCst);
                self.edges_passed.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        }
        true
    }

    /// Whether a kick has come since the vCPU's `prepare` -- its entry to
    /// be turned back (`entering`): for a wait made with the vCPU in its
    /// guest as others see it (`super::fast::FastExit::hlt`), which ends
    /// for whatever a kick brings.
    pub fn refuses(&self) -> bool {
        self.mode.load(Ordering::SeqCst) != IN_GUEST
    }

    pub(crate) fn left(&self) {
        self.mode.store(OUTSIDE_GUEST, Ordering::SeqCst);
    }
}
