//! The hypervisor's tracepoints (docs/tracing.md, group `hv`, and a guest
//! disk's in groups `blk` and `disk`): each record built from what its site
//! has in hand -- an exit's from what the exit and the CPU's registers say
//! of it and from hvarch's stamps around the world switch (`Run`), the rest
//! from values the run loop (`run.rs`), the doorbells (`smp.rs`), the
//! virtio disk (`devices/blk.rs`) and the module's disk task already hold.
//!
//! Each asks `kcore::trace::on` first -- `exit`'s caller asks it, so as to
//! read the registers only then -- so that off, a record costs a load and a
//! branch, and nothing else here runs. Every value a record holds is the
//! guest's to choose -- a port, an MSR, an address, a register, a time it
//! made the host wait -- so nothing here may fail on one: no arithmetic that
//! can overflow, no conversion that can truncate unsaid. hv-fuzz runs it,
//! under its overflow checks, on every exit its scripts make.

use core::sync::atomic::{AtomicU64, Ordering};

use hvarch::x86::svm::Run;
use kcore::trace::{self, BlkComplete, BlkOp, BlkSubmit, DiskDone, DiskHow, DiskStart, ExitReason, HaltUntil, HvExit,
    HvHalt, HvInject, HvRing, HvTimer, HvWake, InjectKind, RingHow, TimerHow, WakeHow};

use crate::devices::blk::Op;
use crate::svm::Exit;

/// What a record of an exit reads of the CPU's registers, as the exit left
/// them: where the guest was, and RAX and RCX -- an MSR's number, a CPUID
/// leaf's, a hypercall's.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExitRegs {
    pub rip: u64,
    pub rax: u64,
    pub rcx: u64,
}

/// A port access's `info`: its size in the low byte, and these.
pub const IO_IN: u32 = 1 << 8;
pub const IO_STRING: u32 = 1 << 9;
pub const IO_REP: u32 = 1 << 10;
/// An MSR's or a control or debug register's `info`: a write.
pub const WRITE: u32 = 1;
/// A CR4 access's `detail` where the exit names no register, its
/// instruction to be decoded for it (`crate::cr`).
pub const NO_GPR: u64 = 0xFF;

/// What `hv_exit`'s `reason`, `detail` and `info` say of `exit`: the kind,
/// what it was about, and more of it (the fields' descriptions,
/// kernel/tracepoint.cpp, say which for each). In line in `exit`, its one
/// caller: a call there is an indirect one, through the module's GOT, and
/// its answer a trip through memory, right after the world switch.
#[inline(always)]
pub fn exit_fields(exit: &Exit, regs: &ExitRegs, host_vector: Option<u8>) -> (ExitReason, u64, u32) {
    let low = |r: u64| r & u64::from(u32::MAX);
    match *exit {
        Exit::Host => (ExitReason::Host, host_vector.map_or(0, u64::from), 0),
        Exit::Kicked => (ExitReason::Kicked, 0, 0),
        Exit::Io(io) => {
            let flags = if io.input { IO_IN } else { 0 } | if io.string { IO_STRING } else { 0 }
                | if io.rep { IO_REP } else { 0 };
            (ExitReason::Io, u64::from(io.port), u32::from(io.size) | flags)
        }
        Exit::Hlt => (ExitReason::Hlt, 0, 0),
        /* The leaf is EAX and the subleaf ECX, the registers' low halves */
        Exit::Cpuid => (ExitReason::Cpuid, low(regs.rax), low(regs.rcx) as u32),
        Exit::Msr { write } => (ExitReason::Msr, low(regs.rcx), if write { WRITE } else { 0 }),
        Exit::Hypercall => (ExitReason::Hypercall, regs.rax, 0),
        Exit::IrqWindow => (ExitReason::IrqWindow, 0, 0),
        Exit::NmiWindow => (ExitReason::NmiWindow, 0, 0),
        /* The error code's low half: every bit AMD-V and VT-x set there */
        Exit::NestedFault { gpa, error } => (ExitReason::NestedFault, gpa, error as u32),
        Exit::Exception { vector, error } => (ExitReason::Exception, u64::from(vector), error.unwrap_or(0)),
        Exit::MachineCheck => (ExitReason::MachineCheck, 0, 0),
        Exit::Shutdown => (ExitReason::Shutdown, 0, 0),
        Exit::Invalid => (ExitReason::Invalid, 0, 0),
        Exit::Cr8 { write, gpr } => (ExitReason::Cr8, u64::from(gpr), if write { WRITE } else { 0 }),
        Exit::Cr0Write { value } => (ExitReason::Cr0, value, WRITE),
        Exit::Cr4 { write, gpr } => (ExitReason::Cr4, gpr.map_or(NO_GPR, u64::from), if write { WRITE } else { 0 }),
        Exit::Dr { write, dr, .. } => (ExitReason::Dr, u64::from(dr), if write { WRITE } else { 0 }),
        Exit::Eoi { vector } => (ExitReason::Eoi, u64::from(vector), 0),
        Exit::Pause => (ExitReason::Pause, 0, 0),
        Exit::Undefined => (ExitReason::Undefined, 0, 0),
        Exit::Other(code) => (ExitReason::Other, code, 0),
    }
}

/// Nanoseconds a cycle of the counter is, in 32.32 fixed point: worked out
/// once, the rate being the kernel's for good once known. 0 until then.
static NS_PER_CYCLE: AtomicU64 = AtomicU64::new(0);

const FRACTION_BITS: u32 = 32;

#[cold]
#[inline(never)]
fn ns_per_cycle_asked() -> u64 {
    let Some(hz) = kcore::time::cycle_counter_hz() else { return 0 };
    /* A rate of 1 Hz is still a factor that fits: 10^9 << 32 is under 2^62. */
    let factor = (kcore::consts::NS_PER_SEC << FRACTION_BITS) / hz;
    NS_PER_CYCLE.store(factor, Ordering::Relaxed);
    factor
}

/// `cycles` of the counter in nanoseconds, as a record's field holds them:
/// `u32::MAX` for that or more, and 0 while the counter's rate is unknown.
#[inline]
pub fn ns(cycles: u64) -> u32 {
    let mut factor = NS_PER_CYCLE.load(Ordering::Relaxed);
    if factor == 0 {
        factor = ns_per_cycle_asked();
    }
    let ns = (u128::from(cycles) * u128::from(factor)) >> FRACTION_BITS;
    u32::try_from(ns).unwrap_or(u32::MAX)
}

/// What `hv_exit`'s times say of `run`, the CPU's last entry as hvarch
/// timed it: in the guest, out of it before, and since the exit -- the last
/// as the counter's low half at the exit, which the kernel reads back as
/// the nanoseconds from it to the record's own stamp (`Kind::Since`), so
/// that no counter is read here for it; 1 where that half is 0, which says
/// none. All 0 for an entry not made -- a kick turned it back -- or not
/// timed; and a count that went backwards -- another CPU's, not quite in
/// step -- a time of 0, not a wrap.
#[inline(always)]
pub fn times(exit: &Exit, run: Run) -> (u32, u32, u32) {
    if matches!(exit, Exit::Kicked) || run.exited == 0 {
        return (0, 0, 0);
    }
    let guest = ns(run.exited.saturating_sub(run.entered));
    let out = if run.before == 0 { 0 } else { ns(run.entered.saturating_sub(run.before)) };
    /* The low half: it always fits, the high one masked off */
    let since = u32::try_from(run.exited & u64::from(u32::MAX)).unwrap_or(1);
    (guest, out, since.max(1))
}

/// The `hv_exit` record of `exit`, CPU `vcpu`'s -- run by the task of pid
/// `pid` (`vm::Cpu::pid`) -- its registers `regs` as the exit left them and
/// its world switch `run` as hvarch timed it. The caller has asked `on` --
/// every entry's call site is `vm::Cpu::enter`. hvarch's stamps are the
/// TSC, the counter the kernel stamps the record with.
pub fn exit(vcpu: u32, pid: u32, exit: &Exit, regs: ExitRegs, run: Run, host_vector: Option<u8>) {
    let (reason, detail, info) = exit_fields(exit, &regs, host_vector);
    let (guest_ns, out_ns, since_ns) = times(exit, run);
    trace::emit(&HvExit {
        detail,
        rip: regs.rip,
        pid,
        guest_ns,
        out_ns,
        since_ns,
        info,
        vcpu: vcpu_field(vcpu),
        reason: reason as u8,
        _pad: 0,
    });
}

/// A guest CPU's number as a record's field holds it.
fn vcpu_field(vcpu: u32) -> u16 {
    u16::try_from(vcpu).unwrap_or(u16::MAX)
}

/// Nanoseconds as a record's field holds them: `u32::MAX` for that or more.
pub fn ns_field(ns: u64) -> u32 {
    u32::try_from(ns).unwrap_or(u32::MAX)
}

/// `hv_inject`: `kind`, of `vector`, into CPU `vcpu` at its entry, by the
/// task of pid `pid` that runs it -- as `hv_halt`, `hv_wake` and
/// `hv_timer` are, each of a CPU's own loop (`vm::Cpu::pid`).
#[inline]
pub fn inject(vcpu: u32, pid: u32, kind: InjectKind, vector: u8) {
    if trace::on::<HvInject>() {
        trace::emit(&HvInject { pid, vcpu: vcpu_field(vcpu), vector, kind: kind as u8 });
    }
}

/// What a halted CPU's loop looked at for the time it sleeps to: its APIC
/// timer's next edge, the PIT's and the devices' next round -- the first
/// CPU's -- and the end of its run.
pub struct HaltFor {
    pub timer: Option<u64>,
    pub pit: Option<u64>,
    pub devices: Option<u64>,
    pub deadline: u64,
}

impl HaltFor {
    /// Which of them `until` is, the soonest: the APIC timer's first, where
    /// two are the same time; none of them, the longest a halt waits.
    pub fn until(&self, until: u64) -> HaltUntil {
        if self.timer == Some(until) {
            HaltUntil::Timer
        } else if self.pit == Some(until) {
            HaltUntil::Pit
        } else if self.devices == Some(until) {
            HaltUntil::Device
        } else if self.deadline == until {
            HaltUntil::Deadline
        } else {
            HaltUntil::Nothing
        }
    }
}

/// `hv_halt`: CPU `vcpu` halted with nothing to take at `now`, to spin for
/// `poll_ns` -- 0 for no spin -- and sleep to `until`, the soonest of `of`.
#[inline]
pub fn halt(vcpu: u32, pid: u32, now: u64, until: u64, of: &HaltFor, poll_ns: u64) {
    if trace::on::<HvHalt>() {
        trace::emit(&HvHalt {
            pid,
            wait_ns: ns_field(until.saturating_sub(now)),
            poll_ns: ns_field(poll_ns),
            vcpu: vcpu_field(vcpu),
            until: of.until(until) as u8,
            _pad: 0,
        });
    }
}

/// `hv_wake`: that halt of CPU `vcpu` over, `how`, `halted_ns` after its
/// look and `late_ns` past the time it slept to.
#[inline]
pub fn wake(vcpu: u32, pid: u32, how: WakeHow, halted_ns: u64, late_ns: u64) {
    if trace::on::<HvWake>() {
        trace::emit(&HvWake {
            pid,
            halted_ns: ns_field(halted_ns),
            late_ns: ns_field(late_ns),
            vcpu: vcpu_field(vcpu),
            how: how as u8,
            _pad: 0,
        });
    }
}

/// `hv_timer`: CPU `vcpu`'s APIC timer interrupt asked for, `late_ns` past
/// its edge, after `how`.
#[inline]
pub fn timer(vcpu: u32, pid: u32, how: TimerHow, late_ns: u64) {
    if trace::on::<HvTimer>() {
        trace::emit(&HvTimer {
            pid,
            late_ns: ns_field(late_ns),
            vcpu: vcpu_field(vcpu),
            how: how as u8,
            _pad: 0,
        });
    }
}

/// `hv_ring`: CPU `vcpu` rung, or notified, `how`, by the running task.
#[inline]
pub fn ring(vcpu: u32, how: RingHow) {
    if trace::on::<HvRing>() {
        trace::emit(&HvRing { pid: trace::pid(), vcpu: vcpu_field(vcpu), how: how as u8, _pad: 0 });
    }
}

/* ---- a guest disk's requests (groups `blk` and `disk`) ---- */

/// A request's kind, as the trace names it.
pub fn blk_op(op: Op) -> BlkOp {
    match op {
        Op::Read => BlkOp::Read,
        Op::Write => BlkOp::Write,
        Op::Flush => BlkOp::Flush,
        Op::Discard => BlkOp::Discard,
        Op::WriteZeroes => BlkOp::WriteZeroes,
    }
}

/// A slot's number as a record's field holds it.
fn slot_field(slot: usize) -> u16 {
    u16::try_from(slot).unwrap_or(u16::MAX)
}

/// A request's bytes as a record's field holds them: `u32::MAX` for that or
/// more -- never, a request's being `REQUEST_BYTES` at most.
fn len_field(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// `blk_submit`: disk `disk`'s request in `slot` -- `op`, `len` bytes at
/// `offset` -- handed to its backend, by the running task.
#[inline]
pub fn blk_submit(disk: u8, slot: usize, op: Op, offset: u64, len: usize) {
    if trace::on::<BlkSubmit>() {
        trace::emit(&BlkSubmit {
            offset,
            len: len_field(len),
            pid: trace::pid(),
            slot: slot_field(slot),
            disk,
            op: blk_op(op) as u8,
            _pad: [0; 4],
        });
    }
}

/// `blk_complete`: that request back from the backend, `ok` or not.
#[inline]
pub fn blk_complete(disk: u8, slot: usize, op: Op, offset: u64, len: usize, ok: bool) {
    if trace::on::<BlkComplete>() {
        trace::emit(&BlkComplete {
            offset,
            len: len_field(len),
            pid: trace::pid(),
            slot: slot_field(slot),
            disk,
            op: blk_op(op) as u8,
            ok: u8::from(ok),
            _pad: [0; 3],
        });
    }
}

/// `disk_start`: the request picked up by the disk's task, the running one.
#[inline]
pub fn disk_start(disk: u8, slot: usize, op: Op, offset: u64, len: usize) {
    if trace::on::<DiskStart>() {
        trace::emit(&DiskStart {
            offset,
            len: len_field(len),
            pid: trace::pid(),
            slot: slot_field(slot),
            disk,
            op: blk_op(op) as u8,
            _pad: [0; 4],
        });
    }
}

/// `disk_done`: the request done in the disk's task, `how`, `ok` or not.
#[inline]
pub fn disk_done(disk: u8, slot: usize, op: Op, how: DiskHow, ok: bool) {
    if trace::on::<DiskDone>() {
        trace::emit(&DiskDone {
            pid: trace::pid(),
            slot: slot_field(slot),
            disk,
            op: blk_op(op) as u8,
            how: how as u8,
            ok: u8::from(ok),
            _pad: [0; 2],
        });
    }
}
