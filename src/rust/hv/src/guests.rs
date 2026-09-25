//! Guests of a few bytes that the hypervisor carries to show itself working.
//!
//! Each is a handful of instructions -- assembled once with NASM and kept
//! here as bytes beside the source they came from -- put in a VM of its own,
//! run until it stops, and checked against what it was told to do. They are
//! the first two rungs of `plans/03-hypervisor.md`, a guest that exits where
//! it was told to and a guest in long mode under nested paging, and three
//! more that show the host is still the host when a guest goes wrong on
//! purpose:
//!
//!   exits      port I/O both ways and CPUID, answered by the host and read
//!              back out of the guest's memory
//!   hypercall  memory above 4 GiB through its own page table, then a
//!              hypercall with every register across it both ways
//!   fault      a write to memory its page table maps and the nested one
//!              does not: stopped at the nested table, the address named
//!   absent     a read of a device the platform does not have -- AMD's FCH,
//!              which Linux on a Zen CPU reads at a fixed address -- answered
//!              with all ones, and a write to it stopped
//!   triple     a triple fault stops the guest, not the CPU
//!   refused    a VMCB that breaks one of `vmrun`'s rules is never handed
//!              to the CPU, and the refusal names the rule -- where the
//!              CPU's own answer would be VMEXIT_INVALID and nothing more
//!   spin       `cli; jmp $` does not keep the host's interrupts out, and
//!              the host stops it when its time is up
//!   tpr        CR8, the task priority register, written to 15 and read
//!              back: the guest's own shadow, not the host CPU's, which
//!              would otherwise keep every interrupt off that CPU
//!   asid       three VMs on one CPU, each reading its own page at one
//!              address: two taking turns never read each other's, and the
//!              third, given an ASID one of them had, reads its own too --
//!              what a TLB entry left under a reused ASID would get wrong
//!
//! Every one starts in long mode with paging on, as a 64-bit Linux kernel
//! is started. Not only because that is where a guest of this hypervisor is
//! going: QEMU's TCG before 9.2 does not put the accesses of a guest with
//! paging off through the nested table at all -- it takes a guest physical
//! address for a host one -- so a real-mode guest there runs out of the
//! host's memory, and a gate built on one would be testing the emulator.

use alloc::string::String;
use core::fmt::Write;

use hvarch::Result;
use kcore::time;

use crate::devices::Uart;
use crate::machine::Machine;
use crate::svm::{Exit, LongMode};
use crate::vm::{Refusal, Vm};

/// COM1's port base: the serial console a guest's `console=ttyS0` writes to.
const COM1: u16 = 0x3F8;

/// The port a guest writes its text to, as Bochs and QEMU's debugcon have
/// it. Reading it answers its own number, which is how a guest tells that it
/// is there.
const DEBUG_PORT: u16 = 0xE9;
/// A CPUID leaf only this hypervisor answers, and what it answers: EAX, EBX,
/// ECX and EDX, four bytes each.
const CPUID_LEAF: u64 = 0x4E4F_5300;
const CPUID_ANSWER: &[u8; 16] = b"nos hypervisor\0\0";
/// The hypercall the `hypercall` guest makes.
const HYPERCALL: u64 = 0x4E4F_5301;
/// The most of what a guest says that is kept: a guest that talks for its
/// whole budget is not a reason for the host to run out of memory.
const SAID_MAX: usize = 256;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// Every vector: for a guest whose every fault is a finding.
const ALL_EXCEPTIONS: u32 = u32::MAX;

/// What stopped a guest for good.
#[derive(Clone, Copy, Debug)]
enum Stop {
    Halted { rip: u64 },
    Fault { gpa: u64, error: u64, rip: u64 },
    Shutdown { rip: u64 },
    Exception { vector: u8, error: Option<u32>, rip: u64 },
    MachineCheck { rip: u64 },
    /// It ran for as long as it was let, and the host stopped it.
    Budget,
    Refused(Refusal),
    /// `vmrun` refused the VMCB, which the checks before it did not catch.
    Invalid,
    /// An exit this harness has no answer for.
    Unexpected { exit: Exit, rip: u64 },
}

/// What happened while a guest ran.
struct Run {
    cpus: u64,
    said: String,
    port_in: u32,
    port_out: u32,
    cpuid: u32,
    hypercall: u32,
    /// The host's own interrupts that took the CPU back from the guest.
    host: u32,
    /// Reads of an absent device answered with a page of all ones.
    absent: u32,
    /// `mov`s to and from CR8 stopped at and answered from the shadow TPR:
    /// under VT-x, which stops the guest there; AMD-V keeps the shadow
    /// itself and never stops.
    cr8: u32,
    /// The guest's registers at its hypercall: RAX, RBX, RCX, RDX, RSI, RDI,
    /// RBP, R8-R15.
    at_hypercall: Option<[u64; 15]>,
    /// The emulated serial console, and what the guest sent it.
    uart: Uart,
    serial: String,
    stop: Stop,
    ns: u64,
}

/// What the host answers the hypercall with, register by register in the
/// order above: a value of each register's own, so that one landing in
/// another's place shows.
fn answer(register: usize) -> u64 {
    0x1111_1111_1111_1111u64.wrapping_mul(register as u64 + 1)
}

fn cpuid_words() -> [u64; 4] {
    let mut w = [0u64; 4];
    for (i, word) in w.iter_mut().enumerate() {
        let b = &CPUID_ANSWER[i * 4..i * 4 + 4];
        *word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64;
    }
    w
}

/// Run `vm` until it stops, answering what a guest of this harness asks.
fn run(vm: &mut Vm, machine: &Machine, budget_ms: u64) -> Run {
    let mut run = Run {
        cpus: 0,
        said: String::new(),
        port_in: 0,
        port_out: 0,
        cpuid: 0,
        hypercall: 0,
        host: 0,
        absent: 0,
        cr8: 0,
        at_hypercall: None,
        uart: Uart::new(),
        serial: String::new(),
        stop: Stop::Budget,
        ns: 0,
    };
    let start = time::boot_time_ns();
    let budget = budget_ms * kcore::consts::NS_PER_MS;

    run.stop = loop {
        if time::boot_time_ns().saturating_sub(start) >= budget {
            break Stop::Budget;
        }
        let (exit, cpu) = match vm.enter(machine, None) {
            Ok(entered) => entered,
            Err(refusal) => break Stop::Refused(refusal),
        };
        run.cpus |= 1u64 << (cpu as u64 % u64::BITS as u64);
        let rip = vm.vcpu().save().rip;

        match exit {
            /* Taken by the host on the way out, with the guest's state
             * safely in the VMCB: straight back in. (Nothing kicks these
             * guests: none is entered with a `Kick`.) */
            Exit::Host | Exit::Kicked => run.host += 1,
            Exit::Io(io) if io.size == 1 && !io.string && Uart::owns(COM1, io.port) => {
                let v = vm.vcpu_mut();
                let offset = io.port - COM1;
                if io.input {
                    let byte = run.uart.read(offset);
                    let s = v.save_mut();
                    s.rax = (s.rax & !0xFF) | byte as u64;
                    run.port_in += 1;
                } else {
                    if let Some(byte) = run.uart.write(offset, v.save().rax as u8) {
                        /* Kept up to the same cap as the debug port's, and
                         * without an allocation that could fail by panicking. */
                        if run.serial.len() < SAID_MAX && run.serial.try_reserve(1).is_ok() {
                            run.serial.push(byte as char);
                        }
                    }
                    run.port_out += 1;
                }
                v.skip_io(&io);
            }
            Exit::Io(io) if io.port == DEBUG_PORT && io.size == 1 && !io.string => {
                let v = vm.vcpu_mut();
                if io.input {
                    let s = v.save_mut();
                    s.rax = (s.rax & !0xFF) | DEBUG_PORT as u64;
                    run.port_in += 1;
                } else {
                    let byte = v.save().rax as u8;
                    if run.said.len() < SAID_MAX {
                        run.said.push(if byte.is_ascii_graphic() || byte == b' ' { byte as char } else { '.' });
                    }
                    run.port_out += 1;
                }
                v.skip_io(&io);
            }
            Exit::Cpuid => {
                let v = vm.vcpu_mut();
                let words = if v.save().rax & 0xFFFF_FFFF == CPUID_LEAF { cpuid_words() } else { [0; 4] };
                v.save_mut().rax = words[0];
                let r = v.regs_mut();
                r.rbx = words[1];
                r.rcx = words[2];
                r.rdx = words[3];
                v.skip_cpuid();
                run.cpuid += 1;
            }
            Exit::Hypercall => {
                let v = vm.vcpu_mut();
                let r = *v.regs();
                run.at_hypercall = Some([
                    v.save().rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp,
                    r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15,
                ]);
                v.save_mut().rax = answer(0);
                let r = v.regs_mut();
                for (i, reg) in [
                    &mut r.rbx, &mut r.rcx, &mut r.rdx, &mut r.rsi, &mut r.rdi, &mut r.rbp,
                    &mut r.r8, &mut r.r9, &mut r.r10, &mut r.r11, &mut r.r12, &mut r.r13,
                    &mut r.r14, &mut r.r15,
                ].into_iter().enumerate() {
                    *reg = answer(i + 1);
                }
                v.skip_vmmcall();
                run.hypercall += 1;
            }
            Exit::Hlt => break Stop::Halted { rip },
            Exit::NestedFault { gpa, error } => {
                /* A read of the MMIO window with no device behind it is
                 * answered with all ones, as the Linux guest's loop answers
                 * one (`GuestMemory::map_absent`); anything else stops. */
                use hvarch::x86::svm::vmcb::npf;
                let plain_read = error & (npf::PRESENT | npf::WRITE | npf::FETCH) == 0
                    && error & npf::FINAL != 0;
                if plain_read && vm.memory_mut().map_absent(gpa).is_ok() {
                    run.absent += 1;
                    continue;
                }
                break Stop::Fault { gpa, error, rip };
            }
            Exit::Shutdown => break Stop::Shutdown { rip },
            Exit::Exception { vector, error } => break Stop::Exception { vector, error, rip },
            Exit::MachineCheck => break Stop::MachineCheck { rip },
            Exit::Invalid => break Stop::Invalid,
            Exit::Cr8 { write, gpr } => {
                vm.vcpu_mut().cr8_access(write, gpr);
                run.cr8 += 1;
            }
            other => break Stop::Unexpected { exit: other, rip },
        }
    };
    run.ns = time::boot_time_ns().saturating_sub(start);
    run
}

/// A built-in guest: how to make it, how long it may run, and what it has
/// to have done.
struct Spec {
    name: &'static str,
    about: &'static str,
    exceptions: u32,
    budget_ms: u64,
    build: fn(&mut Vm) -> Result<()>,
    /// What was checked, or what was wrong.
    check: fn(&Vm, &Run) -> core::result::Result<String, String>,
}

const GUESTS: &[Spec] = &[
    Spec {
        name: "exits",
        about: "port I/O both ways, and CPUID answered by the host",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_exits,
        check: check_exits,
    },
    Spec {
        name: "uart",
        about: "an 8250 brought up and written to over port I/O",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_uart,
        check: check_uart,
    },
    Spec {
        name: "hypercall",
        about: "memory above 4 GiB through its own page table, then every register across a hypercall",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_hypercall,
        check: check_hypercall,
    },
    Spec {
        name: "fault",
        about: "a write to memory its page table maps and the nested one does not",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_fault,
        check: check_fault,
    },
    Spec {
        name: "absent",
        about: "a read of a device that is not there answered with all ones, and a write to it stopped",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_absent,
        check: check_absent,
    },
    Spec {
        name: "triple",
        about: "an interrupt with no IDT to take it: a triple fault",
        exceptions: 0,
        budget_ms: 2000,
        build: build_triple,
        check: check_triple,
    },
    Spec {
        name: "refused",
        about: "a VMCB that breaks a rule vmrun checks: CR0.NW without CR0.CD",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_refused,
        check: check_refused,
    },
    Spec {
        name: "spin",
        about: "cli; jmp $ -- for as long as the host lets it",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 300,
        build: build_spin,
        check: check_spin,
    },
    Spec {
        name: "tpr",
        about: "CR8 -- the host's task priority register -- written to 15 and read back: the guest's own, not the CPU's",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_tpr,
        check: check_tpr,
    },
];

/* N rounds of: a read of each of K pages, 4 KiB apart from 0x10000 up,
 * then a CPUID the host answers. With K 0 it is exits back to back and
 * nothing else -- what a round trip through the host costs, and nothing a
 * device model adds; with K pages, each exit is followed by K translations,
 * which a TLB flushed on the way in has to walk again. N goes in at
 * `BENCH_ROUNDS_AT`, K at `BENCH_PAGES_AT`. */
const BENCH_CODE: [u8; 42] = [
    0xBE, 0x00, 0x00, 0x00, 0x00,                   // mov esi, N
    0xBF, 0x00, 0x00, 0x01, 0x00,                   // .round: mov edi, 0x10000
    0xB9, 0x00, 0x00, 0x00, 0x00,                   // mov ecx, K
    0x67, 0xE3, 0x0C,                               // jecxz .exit
    0x8B, 0x07,                                     // .touch: mov eax, [rdi]
    0x81, 0xC7, 0x00, 0x10, 0x00, 0x00,             // add edi, 0x1000
    0xFF, 0xC9,                                     // dec ecx
    0x75, 0xF4,                                     // jnz .touch
    0xB8, 0x00, 0x53, 0x4F, 0x4E,                   // .exit: mov eax, 0x4e4f5300
    0x0F, 0xA2,                                     // cpuid
    0xFF, 0xCE,                                     // dec esi
    0x75, 0xDC,                                     // jnz .round
    0xF4,                                           // hlt
];
const BENCH_ROUNDS_AT: usize = 1;
const BENCH_PAGES_AT: usize = 11;
const BENCH_HLT: u64 = ENTRY + 0x29;
/// Where the pages it reads start, and the most there are: up to the end
/// of its memory.
const BENCH_PAGES_BASE: u64 = 0x10000;
pub const BENCH_MAX_PAGES: u32 = ((MEMORY - BENCH_PAGES_BASE) / kcore::consts::PAGE_SIZE as u64) as u32;
/// Long enough for a million exits under TCG, twice emulated.
const BENCH_BUDGET_MS: u64 = 120_000;

/// What `bench` measured.
pub struct Bench {
    pub exits: u64,
    pub ns: u64,
    /// Time-stamp counter ticks over the same stretch as `ns`.
    pub ticks: u64,
    /// The host's interrupts among the exits.
    pub host: u32,
    pub profile: Option<hvarch::x86::svm::Profile>,
}

/// What a VM exit costs on the CPU this runs on: `exits` CPUIDs, each after
/// a read of each of `pages` pages -- with a flush of the whole TLB on every
/// entry when `flush` says so, for what that costs, and each entry timed in
/// its parts when `profile` does. Timed from the first entry to the halt --
/// or why it did not run to its halt.
pub fn bench(machine: &Machine, exits: u32, pages: u32, flush: bool, profile: bool)
    -> core::result::Result<Bench, String>
{
    if pages > BENCH_MAX_PAGES {
        return Err(alloc::format!("{} pages is more than its {}", pages, BENCH_MAX_PAGES));
    }
    let mut code = BENCH_CODE;
    code[BENCH_ROUNDS_AT..BENCH_ROUNDS_AT + 4].copy_from_slice(&exits.to_le_bytes());
    code[BENCH_PAGES_AT..BENCH_PAGES_AT + 4].copy_from_slice(&pages.to_le_bytes());
    let mut vm = Vm::new(machine, ALL_EXCEPTIONS).map_err(|e| alloc::format!("no VM: {}", e))?;
    board(&mut vm, &code).map_err(|e| alloc::format!("no guest: {}", e))?;
    vm.vcpu_mut().set_flush_always(flush);
    vm.vcpu_mut().set_profile(profile);
    let t0 = hvarch::x86::cpu::rdtsc();
    let r = run(&mut vm, machine, BENCH_BUDGET_MS);
    let ticks = hvarch::x86::cpu::rdtsc().saturating_sub(t0);
    halted_at(&r, BENCH_HLT)?;
    if r.cpuid != exits {
        return Err(alloc::format!("{} CPUID exits of {}", r.cpuid, exits));
    }
    Ok(Bench { exits: u64::from(r.cpuid), ns: r.ns, ticks, host: r.host, profile: vm.vcpu().profile() })
}

/* ---- asid: what the TLB keeps between one guest's entries and another's --
 *
 * The same program in three VMs, each with a marker of its own at 0x10000:
 * it reads the marker, then N times makes an exit and reads it again, and
 * leaves how many reads differed from the first, and the first, for the
 * host. A and B take turns on one CPU, an exit each; C is made once both
 * have gone, with the ASIDs a generation limited to two, so that the one it
 * is given is one A or B had, after the flush that ended their generation.
 * A reused ASID whose flush was missed reads another guest's page -- a page
 * gone back to the host, for C. Under TCG every entry flushes whatever the
 * VMCB asks, so this can only fail on a CPU that keeps translations. */
const ASID_CODE: [u8; 59] = [
    0xBE, 0x00, 0x00, 0x00, 0x00,                   // mov esi, N
    0x44, 0x8B, 0x2C, 0x25, 0x00, 0x00, 0x01, 0x00, // mov r13d, [0x10000]
    0x45, 0x31, 0xE4,                               // xor r12d, r12d
    0xB8, 0x00, 0x53, 0x4F, 0x4E,                   // .round: mov eax, 0x4e4f5300
    0x0F, 0xA2,                                     // cpuid
    0x8B, 0x04, 0x25, 0x00, 0x00, 0x01, 0x00,       // mov eax, [0x10000]
    0x44, 0x39, 0xE8,                               // cmp eax, r13d
    0x74, 0x03,                                     // je .same
    0x41, 0xFF, 0xC4,                               // inc r12d
    0xFF, 0xCE,                                     // .same: dec esi
    0x75, 0xE6,                                     // jnz .round
    0x44, 0x89, 0x24, 0x25, 0x00, 0x70, 0x00, 0x00, // mov [0x7000], r12d
    0x44, 0x89, 0x2C, 0x25, 0x04, 0x70, 0x00, 0x00, // mov [0x7004], r13d
    0xF4,                                           // hlt
];
const ASID_ROUNDS_AT: usize = 1;
const ASID_ROUNDS: u32 = 8;
const ASID_HLT: u64 = ENTRY + 0x3A;
const ASID_MARKER_AT: u64 = 0x10000;
const ASID_MARKERS: [u32; 3] = [0x4141_4141, 0x4242_4242, 0x4343_4343];
const ASID_NAMES: [char; 3] = ['A', 'B', 'C'];
/// ASIDs a generation while it runs: two, so that the third VM ends one.
const ASID_LIMIT: u32 = 2;
const ASID_BUDGET_MS: u64 = 5000;

/// What the several VMs of a check did between them.
#[derive(Default)]
struct Tally {
    cpus: u64,
    cpuid: u32,
    host: u32,
    /// The CPU the extension was not on for, when nothing ran at all.
    not_on: Option<u32>,
}

fn asid_vm(machine: &Machine, which: usize) -> core::result::Result<Vm, String> {
    let mut code = ASID_CODE;
    code[ASID_ROUNDS_AT..ASID_ROUNDS_AT + 4].copy_from_slice(&ASID_ROUNDS.to_le_bytes());
    let mut vm = Vm::new(machine, ALL_EXCEPTIONS).map_err(|e| alloc::format!("no VM: {}", e))?;
    board(&mut vm, &code).map_err(|e| alloc::format!("no guest: {}", e))?;
    vm.memory_mut().write_obj(ASID_MARKER_AT, &ASID_MARKERS[which])
        .map_err(|e| alloc::format!("no marker: {}", e))?;
    Ok(vm)
}

/// Into `vm` until its next CPUID, which is answered, or its halt: true
/// once it has halted.
fn asid_step(vm: &mut Vm, machine: &Machine, tally: &mut Tally, deadline: u64)
    -> core::result::Result<bool, String>
{
    loop {
        if time::boot_time_ns() >= deadline {
            return Err(String::from("it ran out of time"));
        }
        let (exit, cpu) = match vm.enter(machine, None) {
            Ok(entered) => entered,
            Err(Refusal::NotOn(cpu)) if tally.cpus == 0 => {
                tally.not_on = Some(cpu);
                return Err(String::from("not run"));
            }
            Err(refusal) => return Err(alloc::format!("not entered: {:?}", refusal)),
        };
        tally.cpus |= 1u64 << (cpu as u64 % u64::BITS as u64);
        match exit {
            Exit::Host => tally.host += 1,
            Exit::Cpuid => {
                vm.vcpu_mut().skip_cpuid();
                tally.cpuid += 1;
                return Ok(false);
            }
            Exit::Hlt if vm.vcpu().save().rip == ASID_HLT => return Ok(true),
            other => {
                return Err(alloc::format!("an exit with no answer here: {:?} at {:#x}", other, vm.vcpu().save().rip));
            }
        }
    }
}

/// What VM `which` found at its marker, every time.
fn asid_found(vm: &Vm, which: usize) -> core::result::Result<(), String> {
    let differed: u32 = read(vm, RESULTS)?;
    let first: u32 = read(vm, RESULTS + 4)?;
    if first != ASID_MARKERS[which] {
        return Err(alloc::format!("vm {} read {:#x} at its marker, not its own {:#x}: another guest's page, through a translation its ASID should not have had",
                                  ASID_NAMES[which], first, ASID_MARKERS[which]));
    }
    if differed != 0 {
        return Err(alloc::format!("vm {} read something else at its marker {} times of {}", ASID_NAMES[which], differed, ASID_ROUNDS));
    }
    Ok(())
}

fn asid_check(machine: &Machine, tally: &mut Tally) -> core::result::Result<String, String> {
    let deadline = time::boot_time_ns() + ASID_BUDGET_MS * kcore::consts::NS_PER_MS;
    let _limit = hvarch::x86::svm::AsidLimit::new(ASID_LIMIT);
    let (cpu, before) = hvarch::x86::svm::asid_generation().ok_or_else(|| String::from("no ASIDs on this CPU"))?;

    let mut a = asid_vm(machine, 0)?;
    let mut b = asid_vm(machine, 1)?;
    let (mut a_done, mut b_done) = (false, false);
    while !(a_done && b_done) {
        if !a_done {
            a_done = asid_step(&mut a, machine, tally, deadline)?;
        }
        if !b_done {
            b_done = asid_step(&mut b, machine, tally, deadline)?;
        }
    }
    asid_found(&a, 0)?;
    asid_found(&b, 1)?;
    let had = [a.vcpu().asid(), b.vcpu().asid()];
    drop(a);
    drop(b);

    let mut c = asid_vm(machine, 2)?;
    while !asid_step(&mut c, machine, tally, deadline)? {}
    asid_found(&c, 2)?;
    let given = c.vcpu().asid();

    /* VT-x's ASID is the VPID, held for a guest's life and given back at its
     * end, the lowest free one taken next: C's is A's or B's, and what the
     * first entry's INVVPID is for is exactly what a reused ASID's flush is
     * for on the AMD side. The generation machinery below is AMD's. */
    if machine.ext() == Ok(hvarch::Ext::Vmx) {
        return match (had, given) {
            (_, None) => Ok(String::from("each of three VMs read its own page through its own EPT; this CPU has no VPIDs, so every transition flushes")),
            ([Some(a), Some(b)], Some(c)) if c == a || c == b => Ok(alloc::format!(
                "vm C was given VPID {}, which vm {} had, and read its own too", c, if c == a { "A" } else { "B" })),
            (had, Some(c)) => Err(alloc::format!("vm C was given VPID {}, which neither A ({:?}) nor B ({:?}) had", c, had[0], had[1])),
        };
    }

    let (cpu_after, after) = hvarch::x86::svm::asid_generation().ok_or_else(|| String::from("no ASIDs on this CPU"))?;
    if tally.cpus != 1u64 << cpu || cpu_after != cpu {
        return Ok(String::from("each read its own page; on more than one CPU, so the end of a generation was not what C was given"));
    }
    if after == before {
        return Err(alloc::format!("with {} ASIDs a generation, three VMs ended none", ASID_LIMIT));
    }
    let whose = match given {
        Some(n) if Some(n) == had[0] => "vm A",
        Some(n) if Some(n) == had[1] => "vm B",
        _ => return Err(alloc::format!("vm C was given ASID {:?}, which neither A ({:?}) nor B ({:?}) had", given, had[0], had[1])),
    };
    Ok(alloc::format!("A and B each read their own page taking turns; vm C was given ASID {}, which {} had, after {} generation(s) ended, and read its own too",
                      given.unwrap_or(0), whose, after - before))
}

fn run_asid(machine: &Machine) -> Several {
    let start = time::boot_time_ns();
    let mut tally = Tally::default();
    let verdict = asid_check(machine, &mut tally);
    Several { tally, ns: time::boot_time_ns().saturating_sub(start), verdict }
}

/// A check that takes more than one VM: made, run and judged by `run`.
struct SeveralSpec {
    name: &'static str,
    about: &'static str,
    run: fn(&Machine) -> Several,
}

/// What such a check did.
struct Several {
    tally: Tally,
    ns: u64,
    verdict: core::result::Result<String, String>,
}

const SEVERAL: &[SeveralSpec] = &[
    SeveralSpec {
        name: "asid",
        about: "three VMs on one CPU, and none reads another's page through the TLB",
        run: run_asid,
    },
];

fn run_several(machine: &Machine, spec: &SeveralSpec, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", spec.name, spec.about);
    let s = (spec.run)(machine);
    if let Some(cpu) = s.tally.not_on {
        let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first",
                         spec.name, cpu);
        return false;
    }
    let _ = write!(out, "  ran on     cpu");
    for cpu in 0..u64::BITS {
        if s.tally.cpus & (1u64 << cpu) != 0 {
            let _ = write!(out, " {}", cpu);
        }
    }
    let _ = writeln!(out, "{}, {} us", if s.tally.cpus == 0 { " none" } else { "" }, s.ns / kcore::consts::NS_PER_US);
    let _ = writeln!(out, "  exits      {} cpuid, {} host interrupt", s.tally.cpuid, s.tally.host);
    match s.verdict {
        Ok(checked) => {
            let _ = writeln!(out, "  checked    {}", checked);
            let _ = writeln!(out, "hv: guest {} ok", spec.name);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", spec.name, why);
            false
        }
    }
}

/// The names of the built-in guests, for a command's help.
pub fn names() -> impl Iterator<Item = &'static str> {
    GUESTS.iter().map(|g| g.name).chain(SEVERAL.iter().map(|g| g.name))
}

/// Run the built-in guest `name` on the CPU this is called on and say what
/// it did. `None` when there is no such guest; otherwise whether it did
/// what it was told.
pub fn run_one(machine: &Machine, name: &str, out: &mut dyn Write) -> Option<bool> {
    if let Some(spec) = SEVERAL.iter().find(|g| g.name == name) {
        return Some(run_several(machine, spec, out));
    }
    let spec = GUESTS.iter().find(|g| g.name == name)?;
    Some(run_spec(machine, spec, out))
}

fn run_spec(machine: &Machine, spec: &Spec, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", spec.name, spec.about);
    let mut vm = match Vm::new(machine, spec.exceptions).and_then(|mut vm| (spec.build)(&mut vm).map(|_| vm)) {
        Ok(vm) => vm,
        Err(e) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- could not be made: {}", spec.name, e);
            return false;
        }
    };

    let r = run(&mut vm, machine, spec.budget_ms);
    if let (Stop::Refused(Refusal::NotOn(cpu)), 0) = (r.stop, r.cpus) {
        /* Not a finding about the guest: nothing ran. */
        let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first",
                         spec.name, cpu);
        return false;
    }
    let _ = write!(out, "  ran on     cpu");
    for cpu in 0..u64::BITS {
        if r.cpus & (1u64 << cpu) != 0 {
            let _ = write!(out, " {}", cpu);
        }
    }
    let _ = writeln!(out, "{}, {} us", if r.cpus == 0 { " none" } else { "" }, r.ns / kcore::consts::NS_PER_US);
    if !r.said.is_empty() {
        let _ = writeln!(out, "  said       \"{}\"", r.said);
    }
    let _ = writeln!(out, "  exits      {} port in, {} port out, {} cpuid, {} hypercall, {} host interrupt",
                     r.port_in, r.port_out, r.cpuid, r.hypercall, r.host);
    let _ = write!(out, "  stopped    ");
    let _ = match r.stop {
        Stop::Halted { rip } => writeln!(out, "hlt at {:#x}", rip),
        Stop::Fault { gpa, error, rip } => writeln!(out, "nested page fault at gpa {:#x}, error {:#x}, rip {:#x}", gpa, error, rip),
        Stop::Shutdown { rip } => writeln!(out, "shutdown (triple fault) at {:#x}", rip),
        Stop::Exception { vector, error, rip } => match error {
            Some(code) => writeln!(out, "exception {} error {:#x} at {:#x}", vector, code, rip),
            None => writeln!(out, "exception {} at {:#x}", vector, rip),
        },
        Stop::MachineCheck { rip } => writeln!(out, "MACHINE CHECK while the guest ran, at {:#x}", rip),
        Stop::Budget => writeln!(out, "by the host, after {} ms", spec.budget_ms),
        Stop::Refused(Refusal::Vmcb(rule)) => writeln!(out, "not entered -- the VMCB breaks a rule: {}", rule),
        Stop::Refused(Refusal::NotOn(cpu)) => writeln!(out, "not entered -- the extension went off for cpu {}", cpu),
        Stop::Refused(Refusal::FiveLevelPaging(cpu)) => writeln!(
            out, "not entered -- cpu {} translates with five levels, and a nested table of four would be walked as five", cpu),
        Stop::Refused(Refusal::Flush(cpu)) => writeln!(
            out, "not entered -- cpu {} would not drop what it had cached through the nested table (INVEPT)", cpu),
        Stop::Invalid => writeln!(out, "the CPU refused the entry (VMEXIT_INVALID on AMD-V, a VM-entry failure on VT-x)"),
        Stop::Unexpected { exit, rip } => writeln!(out, "an exit with no answer here: {:?} at {:#x}", exit, rip),
    };

    match (spec.check)(&vm, &r) {
        Ok(checked) => {
            if !checked.is_empty() {
                let _ = writeln!(out, "  checked    {}", checked);
            }
            let _ = writeln!(out, "hv: guest {} ok", spec.name);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", spec.name, why);
            let _ = vm.vcpu().dump(out);
            false
        }
    }
}

/* The guests' machine: 1 MiB of memory with a GDT, a TSS and a page table
 * in it, and code at 0x8000. The page table maps the first 2 MiB to
 * themselves -- the second of them has no memory behind it, which is what
 * the `fault` guest finds -- and 1 GiB to 1 GiB + 2 MiB to guest physical
 * 4 GiB, where the `hypercall` guest is given a page. */

const MEMORY: u64 = 1 * MIB;
const GDT: u64 = 0x1000;
const TSS: u64 = 0x1100;
const PML4: u64 = 0x2000;
const PDPT: u64 = 0x3000;
const PD_LOW: u64 = 0x4000;
const PD_HIGH: u64 = 0x5000;
/// Where a guest leaves what it found, for the check to read back.
const RESULTS: u64 = 0x7000;
const ENTRY: u64 = 0x8000;
const STACK: u64 = 0xF000;
/// Guest physical 4 GiB: past where a 32-bit address reaches.
const HIGH: u64 = 4 * GIB;
/// The guest-virtual address the page table maps there.
const HIGH_VIRTUAL: u64 = 1 * GIB;

/* Guest page-table entries: present and writable, and in a page directory
 * a 2 MiB page. */
const PTE_P_W: u64 = 0x3;
const PTE_LARGE: u64 = 0x80;

/* The GDT: null, 64-bit code at 0x08, data at 0x10, and the TSS at 0x18,
 * which a 64-bit TSS descriptor takes two entries for. */
const GDT_CODE64: u64 = 0x00AF_9B00_0000_FFFF;
const GDT_DATA: u64 = 0x00CF_9300_0000_FFFF;
const GDT_ENTRIES: u64 = 5;
const TSS_LIMIT: u64 = 0x67;
const TSS_BUSY_PRESENT: u64 = 0x8B;
/// An IDT with no entries: the first interrupt or exception the guest does
/// not have intercepted is a triple fault.
const NO_IDT: u16 = 0;

fn board(vm: &mut Vm, code: &[u8]) -> Result<()> {
    let m = vm.memory_mut();
    m.add(0, MEMORY)?;

    let tss_low = TSS_LIMIT | (TSS & 0xFF_FFFF) << 16 | TSS_BUSY_PRESENT << 40 | (TSS >> 24 & 0xFF) << 56;
    let tss_high = TSS >> 32;
    for (i, entry) in [0, GDT_CODE64, GDT_DATA, tss_low, tss_high].iter().enumerate() {
        m.write_obj(GDT + i as u64 * 8, entry)?;
    }
    m.write_obj(PML4, &(PDPT | PTE_P_W))?;
    m.write_obj(PDPT, &(PD_LOW | PTE_P_W))?;
    m.write_obj(PDPT + (HIGH_VIRTUAL >> 30) * 8, &(PD_HIGH | PTE_P_W))?;
    m.write_obj(PD_LOW, &(PTE_P_W | PTE_LARGE))?;
    m.write_obj(PD_HIGH, &(HIGH | PTE_P_W | PTE_LARGE))?;
    m.write(ENTRY, code)?;

    vm.vcpu_mut().long_mode(&LongMode {
        entry: ENTRY,
        stack: STACK,
        cr3: PML4,
        gdt: GDT,
        gdt_limit: (GDT_ENTRIES * 8 - 1) as u16,
        idt_limit: NO_IDT,
        code_selector: 0x08,
        data_selector: 0x10,
        tss_selector: 0x18,
        tss: TSS,
    });
    Ok(())
}

fn halted_at(r: &Run, rip: u64) -> core::result::Result<(), String> {
    match r.stop {
        Stop::Halted { rip: at } if at == rip => Ok(()),
        _ => Err(String::from("it did not stop at its hlt")),
    }
}

fn said(r: &Run, expected: &str) -> core::result::Result<(), String> {
    if r.said != expected {
        return Err(alloc::format!("it said \"{}\", not \"{}\"", r.said, expected));
    }
    Ok(())
}

fn read<T: kcore::pod::Pod>(vm: &Vm, gpa: u64) -> core::result::Result<T, String> {
    vm.memory().read_obj(gpa).map_err(|e| alloc::format!("guest memory at {:#x}: {}", gpa, e))
}

/* The guests. Each program is the output of `nasm -f bin` for `bits 64` and
 * `org 0x8000`, the source line beside the bytes it became. */

const EXITS_CODE: &[u8] = &[
    0xBA, 0xE9, 0x00, 0x00, 0x00,                   // mov edx, 0xe9       ; the debug port
    0xEC,                                           // in al, dx           ; which answers 0xe9
    0x88, 0x04, 0x25, 0x10, 0x70, 0x00, 0x00,       // mov [0x7010], al
    0x48, 0x8D, 0x35, 0x2C, 0x00, 0x00, 0x00,       // lea rsi, [rel msg]
    0xAC,                                           // .next: lodsb
    0x84, 0xC0,                                     // test al, al
    0x74, 0x03,                                     // jz .cpuid
    0xEE,                                           // out dx, al
    0xEB, 0xF8,                                     // jmp .next
    0xB8, 0x00, 0x53, 0x4F, 0x4E,                   // .cpuid: mov eax, 0x4e4f5300
    0x0F, 0xA2,                                     // cpuid
    0x89, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,       // mov [0x7000], eax
    0x89, 0x1C, 0x25, 0x04, 0x70, 0x00, 0x00,       // mov [0x7004], ebx
    0x89, 0x0C, 0x25, 0x08, 0x70, 0x00, 0x00,       // mov [0x7008], ecx
    0x89, 0x14, 0x25, 0x0C, 0x70, 0x00, 0x00,       // mov [0x700c], edx
    0xF4,                                           // hlt
    b'n', b'o', b's', b':', b' ', b'p', b'o', b'r', b't', b's', b' ', b'a', b'n', b'd', b' ',
    b'c', b'p', b'u', b'i', b'd', 0,                // msg
];
const EXITS_SAYS: &str = "nos: ports and cpuid";
const EXITS_HLT: u64 = ENTRY + 0x3F;
const EXITS_CPUID_AT: u64 = RESULTS;
const EXITS_IN_AT: u64 = RESULTS + 0x10;

fn build_exits(vm: &mut Vm) -> Result<()> {
    board(vm, EXITS_CODE)
}

fn check_exits(vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    halted_at(r, EXITS_HLT)?;
    said(r, EXITS_SAYS)?;
    if r.port_in != 1 || r.cpuid != 1 {
        return Err(String::from("not one port read and one cpuid"));
    }
    let mut found = [0u8; 16];
    vm.memory().read(EXITS_CPUID_AT, &mut found).map_err(|e| alloc::format!("guest memory: {}", e))?;
    if &found != CPUID_ANSWER {
        return Err(alloc::format!("cpuid came back as {:02x?}", found));
    }
    let port: u8 = read(vm, EXITS_IN_AT)?;
    if port != DEBUG_PORT as u8 {
        return Err(alloc::format!("the port read came back as {:#x}", port));
    }
    Ok(String::from("the port answered 0xe9 and cpuid \"nos hypervisor\", and both came back out of guest memory"))
}


/* An 8250 brought up the way a driver does -- disable interrupts, set the
 * divisor behind DLAB, 8N1, the FIFO on, DTR/RTS/OUT2 -- then a line polled
 * out of LSR.THRE a byte at a time. It writes 0xA5 to the scratch register
 * and reads it back first, a presence test the emulated UART has to pass. */
const UART_CODE: &[u8] = &[
    0x66, 0xBA, 0xF9, 0x03, 0x30, 0xC0, 0xEE, 0x66, 0xBA, 0xFB, 0x03, 0xB0, 0x80, 0xEE, 0x66, 0xBA, 0xF8, 0x03, 0xB0, 0x01, 0xEE, 0x66, 0xBA, 0xF9, 0x03, 0x30, 0xC0, 0xEE, 0x66, 0xBA, 0xFB, 0x03, 0xB0, 0x03, 0xEE, 0x66, 0xBA, 0xFA, 0x03, 0xB0, 0xC7, 0xEE, 0x66, 0xBA, 0xFC, 0x03, 0xB0, 0x0B, 0xEE, 0x66, 0xBA, 0xFF, 0x03, 0xB0, 0xA5, 0xEE, 0xEC, 0x41, 0x88, 0xC0, 0x48, 0x8D, 0x35, 0x18, 0x00, 0x00, 0x00, 0xAC, 0x84, 0xC0, 0x74, 0x12, 0x66, 0xBA, 0xFD, 0x03, 0x50, 0xEC, 0xA8, 0x20, 0x58, 0x74, 0xF5, 0x66, 0xBA, 0xF8, 0x03, 0xEE, 0xEB, 0xE9, 0xF4,
    b'n', b'o', b's', b':', b' ', b'h', b'e', b'l', b'l', b'o', b' ', b'f', b'r', b'o', b'm',
    b' ', b'a', b' ', b'g', b'u', b'e', b's', b't', b' ', b'o', b'v', b'e', b'r', b' ',
    b't', b't', b'y', b'S', b'0', 10, 0,
];
const UART_HLT: u64 = ENTRY + 0x5A;
const UART_SAYS: &str = "nos: hello from a guest over ttyS0\n";

fn build_uart(vm: &mut Vm) -> Result<()> {
    board(vm, UART_CODE)
}

fn check_uart(_vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    halted_at(r, UART_HLT)?;
    if r.serial != UART_SAYS {
        return Err(alloc::format!("the UART received {:?}", r.serial));
    }
    Ok(alloc::format!("the guest brought up the 8250 and sent {} bytes through it", r.uart.written()))
}

const HYPERCALL_CODE: &[u8] = &[
    0x48, 0x8D, 0x35, 0x25, 0x01, 0x00, 0x00,       // lea rsi, [rel msg]
    0xBA, 0xE9, 0x00, 0x00, 0x00,                   // mov edx, 0xe9
    0xAC,                                           // .next: lodsb
    0x84, 0xC0,                                     // test al, al
    0x74, 0x03,                                     // jz .paged
    0xEE,                                           // out dx, al
    0xEB, 0xF8,                                     // jmp .next
    0xBF, 0x00, 0x00, 0x00, 0x40,                   // .paged: mov rdi, 0x40000000
    0x48, 0xB8, 0x68, 0x67, 0x69, 0x68, 0x20, 0x73, 0x6F, 0x6E, // mov rax, 0x6e6f732068696768
    0x48, 0x89, 0x07,                               // mov [rdi], rax
    0xB8, 0x01, 0x53, 0x4F, 0x4E,                   // mov eax, 0x4e4f5301 ; the hypercall
    0x48, 0x8B, 0x1F,                               // mov rbx, [rdi]      ; what 4 GiB gave back
    0x48, 0xB9, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, // mov rcx, 0x0101010101010101
    0x48, 0xBA, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, // mov rdx, 0x0202...
    0x48, 0xBE, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, // mov rsi, 0x0303...
    0x48, 0xBF, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, // mov rdi, 0x0404...
    0x48, 0xBD, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, // mov rbp, 0x0505...
    0x49, 0xB8, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, // mov r8, 0x0808...
    0x49, 0xB9, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, // mov r9, 0x0909...
    0x49, 0xBA, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, // mov r10, 0x0a0a...
    0x49, 0xBB, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B, // mov r11, 0x0b0b...
    0x49, 0xBC, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, // mov r12, 0x0c0c...
    0x49, 0xBD, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, // mov r13, 0x0d0d...
    0x49, 0xBE, 0x0E, 0x0E, 0x0E, 0x0E, 0x0E, 0x0E, 0x0E, 0x0E, // mov r14, 0x0e0e...
    0x49, 0xBF, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, 0x0F, // mov r15, 0x0f0f...
    0x0F, 0x01, 0xD9,                               // vmmcall
    0x48, 0x89, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00, // mov [0x7000], rax
    0x48, 0x89, 0x1C, 0x25, 0x08, 0x70, 0x00, 0x00, // mov [0x7008], rbx
    0x48, 0x89, 0x0C, 0x25, 0x10, 0x70, 0x00, 0x00, // mov [0x7010], rcx
    0x48, 0x89, 0x14, 0x25, 0x18, 0x70, 0x00, 0x00, // mov [0x7018], rdx
    0x48, 0x89, 0x34, 0x25, 0x20, 0x70, 0x00, 0x00, // mov [0x7020], rsi
    0x48, 0x89, 0x3C, 0x25, 0x28, 0x70, 0x00, 0x00, // mov [0x7028], rdi
    0x48, 0x89, 0x2C, 0x25, 0x30, 0x70, 0x00, 0x00, // mov [0x7030], rbp
    0x4C, 0x89, 0x04, 0x25, 0x38, 0x70, 0x00, 0x00, // mov [0x7038], r8
    0x4C, 0x89, 0x0C, 0x25, 0x40, 0x70, 0x00, 0x00, // mov [0x7040], r9
    0x4C, 0x89, 0x14, 0x25, 0x48, 0x70, 0x00, 0x00, // mov [0x7048], r10
    0x4C, 0x89, 0x1C, 0x25, 0x50, 0x70, 0x00, 0x00, // mov [0x7050], r11
    0x4C, 0x89, 0x24, 0x25, 0x58, 0x70, 0x00, 0x00, // mov [0x7058], r12
    0x4C, 0x89, 0x2C, 0x25, 0x60, 0x70, 0x00, 0x00, // mov [0x7060], r13
    0x4C, 0x89, 0x34, 0x25, 0x68, 0x70, 0x00, 0x00, // mov [0x7068], r14
    0x4C, 0x89, 0x3C, 0x25, 0x70, 0x70, 0x00, 0x00, // mov [0x7070], r15
    0xF4,                                           // hlt
    b'n', b'o', b's', b':', b' ', b'l', b'o', b'n', b'g', b' ', b'm', b'o', b'd', b'e', 0, // msg
];
const HYPERCALL_SAYS: &str = "nos: long mode";
const HYPERCALL_HLT: u64 = ENTRY + 0x12B;
/// What the guest writes at 1 GiB, which its page table sends to 4 GiB.
const HIGH_VALUE: u64 = 0x6E6F_7320_6869_6768;

/// AMD's hypercall opcode, `vmmcall`, as it sits in `HYPERCALL_CODE`; on
/// Intel the CPU has `vmcall` instead, and the AMD byte is an invalid opcode.
const VMMCALL: [u8; 3] = [0x0F, 0x01, 0xD9];
const VMCALL: [u8; 3] = [0x0F, 0x01, 0xC1];

fn build_hypercall(vm: &mut Vm) -> Result<()> {
    /* The one instruction whose encoding is the vendor's: patch the AMD
     * hypercall to Intel's where the guest runs under VT-x. Both are three
     * bytes, so nothing after it moves. */
    let mut code = alloc::vec::Vec::new();
    code.try_reserve_exact(HYPERCALL_CODE.len()).map_err(|_| hvarch::Error::NoMemory)?;
    code.extend_from_slice(HYPERCALL_CODE);
    if vm.is_vmx() {
        if let Some(at) = code.windows(3).position(|w| w == VMMCALL) {
            code[at..at + 3].copy_from_slice(&VMCALL);
        }
    }
    board(vm, &code)?;
    vm.memory_mut().add(HIGH, kcore::consts::PAGE_SIZE as u64)
}

fn check_hypercall(vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    halted_at(r, HYPERCALL_HLT)?;
    said(r, HYPERCALL_SAYS)?;
    let high: u64 = read(vm, HIGH)?;
    if high != HIGH_VALUE {
        return Err(alloc::format!("guest physical 4 GiB holds {:#x}", high));
    }

    let sent = r.at_hypercall.ok_or_else(|| String::from("it made no hypercall"))?;
    let mut expected = [0u64; 15];
    expected[0] = HYPERCALL;
    expected[1] = HIGH_VALUE;
    /* RCX to RBP carry 0x01.. to 0x05.., R8 to R15 0x08.. to 0x0f.. */
    for (i, byte) in [1u64, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15].iter().enumerate() {
        expected[i + 2] = byte * 0x0101_0101_0101_0101;
    }
    if sent != expected {
        return Err(alloc::format!("the registers at the hypercall were {:x?}", sent));
    }

    for i in 0..expected.len() {
        let got: u64 = read(vm, RESULTS + i as u64 * 8)?;
        if got != answer(i) {
            return Err(alloc::format!("register {} came back as {:#x}, not {:#x}", i, got, answer(i)));
        }
    }
    Ok(String::from("guest physical 4 GiB holds what it wrote; 15 registers went out at the hypercall and 15 answers came back"))
}

const FAULT_CODE: &[u8] = &[
    0xBF, 0x00, 0xF0, 0x1F, 0x00,                   // mov edi, 0x1ff000 ; mapped by its page table, past its memory
    0xC6, 0x07, 0x5A,                               // mov byte [rdi], 0x5a
    0xF4,                                           // hlt
];
const FAULT_GPA: u64 = 0x1F_F000;
const FAULT_RIP: u64 = ENTRY + 5;

fn build_fault(vm: &mut Vm) -> Result<()> {
    board(vm, FAULT_CODE)
}

fn check_fault(_vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    use hvarch::x86::svm::vmcb::npf;
    match r.stop {
        Stop::Fault { gpa, error, rip } if gpa == FAULT_GPA && rip == FAULT_RIP => {
            if error & npf::WRITE == 0 || error & npf::PRESENT != 0 || error & npf::FINAL == 0 {
                return Err(alloc::format!("the fault was not a write to nothing: error {:#x}", error));
            }
            Ok(String::from("stopped at the nested table, at the address and the instruction it was told to"))
        }
        _ => Err(String::from("it did not fault where it was told to")),
    }
}

/* AMD's FCH reset-status register, which Linux reads at this fixed address
 * on every Zen CPU (`print_s5_reset_status_mmio`) and takes all ones from as
 * "no such device". The guest maps the MMIO window's top 2 MiB to itself to
 * reach it. */
const ABSENT_CODE: &[u8] = &[
    0xBE, 0xC0, 0x03, 0xD8, 0xFE,                   // mov esi, 0xfed803c0
    0x8B, 0x06,                                     // mov eax, [rsi]        ; all ones
    0x89, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,       // mov [0x7000], eax
    0x8B, 0x86, 0x40, 0xFC, 0xFF, 0xFF,             // mov eax, [rsi - 0x3c0] ; the same page
    0x89, 0x04, 0x25, 0x04, 0x70, 0x00, 0x00,       // mov [0x7004], eax
    0xC6, 0x06, 0x00,                               // mov byte [rsi], 0     ; a write: stops here
    0xF4,                                           // hlt
];
const ABSENT_GPA: u64 = 0xFED8_03C0;
const ABSENT_WRITE_RIP: u64 = ENTRY + 0x1B;
/// The page directory for the guest's fourth gigabyte, and which of its 2 MiB
/// entries covers the register: (0xfed80000 - 3 GiB) / 2 MiB.
const PD_MMIO: u64 = 0x6000;
const MMIO_GIB: u64 = 3;
const ABSENT_PD_INDEX: u64 = (ABSENT_GPA - MMIO_GIB * GIB) >> 21;

fn build_absent(vm: &mut Vm) -> Result<()> {
    board(vm, ABSENT_CODE)?;
    let m = vm.memory_mut();
    m.write_obj(PDPT + MMIO_GIB * 8, &(PD_MMIO | PTE_P_W))?;
    m.write_obj(PD_MMIO + ABSENT_PD_INDEX * 8, &((ABSENT_GPA & !0x1F_FFFF) | PTE_P_W | PTE_LARGE))
}

fn check_absent(vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    use hvarch::x86::svm::vmcb::npf;
    let first: u32 = read(vm, RESULTS)?;
    let second: u32 = read(vm, RESULTS + 4)?;
    if first != u32::MAX || second != u32::MAX {
        return Err(alloc::format!("the reads came back as {:#x} and {:#x}, not all ones", first, second));
    }
    if r.absent != 1 {
        return Err(alloc::format!("{} pages of all ones mapped, not one", r.absent));
    }
    match r.stop {
        Stop::Fault { gpa, error, rip } if gpa == ABSENT_GPA && rip == ABSENT_WRITE_RIP => {
            if error & npf::WRITE == 0 || error & npf::PRESENT == 0 {
                return Err(alloc::format!("the fault was not a write to the read-only page: error {:#x}", error));
            }
            Ok(String::from("both reads found all ones through one read-only page, and the write to it stopped the guest"))
        }
        _ => Err(String::from("the write to the absent device did not stop the guest where it was told to")),
    }
}

const TRIPLE_CODE: &[u8] = &[
    0xCC,                                           // int3 ; through an IDT with no entries
    0xF4,                                           // hlt
];

fn build_triple(vm: &mut Vm) -> Result<()> {
    board(vm, TRIPLE_CODE)
}

fn check_triple(_vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    match r.stop {
        Stop::Shutdown { .. } => Ok(String::from("the guest's triple fault stopped the guest and nothing else")),
        _ => Err(String::from("it did not triple-fault")),
    }
}

fn build_refused(vm: &mut Vm) -> Result<()> {
    board(vm, SPIN_CODE)?;
    if vm.is_vmx() {
        /* VMX has no software pre-check to catch a CR0 combination -- it is
         * the CPU that refuses a guest, at entry, for invalid state. A
         * non-canonical guest RIP is such a state in a 64-bit guest: VM
         * entry fails and reports it, rather than running. */
        vm.vcpu_mut().save_mut().rip = 0x8000_0000_0000_0000;
    } else {
        /* Not-write-through without cache-disable: a combination the manual
         * lists among the ones `vmrun` refuses, caught by the software check
         * before the CPU ever sees the VMCB. */
        vm.vcpu_mut().save_mut().cr0 |= crate::svm::CR0_NW;
    }
    Ok(())
}

fn check_refused(vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    if vm.is_vmx() {
        return match r.stop {
            Stop::Invalid => Ok(String::from(
                "the CPU refused the VMCS at entry for invalid guest state, reported rather than run")),
            _ => Err(String::from("it was not refused for the state it breaks")),
        };
    }
    match r.stop {
        Stop::Refused(Refusal::Vmcb(rule)) if rule.contains("CR0.NW") && r.cpus == 0 => {
            Ok(String::from("the VMCB never reached the CPU, and the refusal named the rule"))
        }
        _ => Err(String::from("it was not refused for the rule it breaks")),
    }
}

const SPIN_CODE: &[u8] = &[
    0xFA,                                           // cli
    0xEB, 0xFE,                                     // jmp $
];

fn build_spin(vm: &mut Vm) -> Result<()> {
    board(vm, SPIN_CODE)
}

/* CR8 is the local APIC's task-priority register as 64-bit code reaches it,
 * and in root operation it is the host's: a guest let at the CPU's own could
 * set it to 15 and keep every interrupt but an NMI -- the host's tick, its
 * kick, the IPI a TLB shootdown waits for -- off that CPU for as long as it
 * liked, and after it had gone. VT-x has to be told to stop the guest there
 * (`Exit::Cr8`, answered from a shadow of the guest's own); AMD-V keeps the
 * shadow itself (`V_TPR`, under `V_INTR_MASKING`) and never stops. Either
 * way the guest has to read back what it wrote, and the CPU's own CR8 --
 * read by the host afterwards, on the CPU the guest ran on -- has to be what
 * it was. */
const TPR_CODE: &[u8] = &[
    0xB8, 0x0F, 0x00, 0x00, 0x00,                   // mov eax, 15
    0x44, 0x0F, 0x22, 0xC0,                         // mov cr8, rax
    0x44, 0x0F, 0x20, 0xC3,                         // mov rbx, cr8
    0x48, 0x89, 0x1C, 0x25, 0x00, 0x70, 0x00, 0x00, // mov [0x7000], rbx
    0xF4,                                           // hlt
];
const TPR_WRITTEN: u64 = 15;
const TPR_HLT: u64 = ENTRY + 0x15;
/// The host's own task priority, as this kernel's local APIC setup leaves
/// it: every interrupt let through.
const HOST_TPR: u64 = 0;

fn build_tpr(vm: &mut Vm) -> Result<()> {
    board(vm, TPR_CODE)
}

fn check_tpr(vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    halted_at(r, TPR_HLT)?;
    let read: u64 = read(vm, RESULTS)?;
    if read != TPR_WRITTEN {
        return Err(alloc::format!("the guest read {} back from CR8, not the {} it wrote", read, TPR_WRITTEN));
    }
    let host = hvarch::x86::cpu::read_cr8();
    if host != HOST_TPR {
        return Err(alloc::format!(
            "the host's CR8 is {} after the guest's write, not {}: the write reached the CPU's own", host, HOST_TPR));
    }
    let stops = if vm.is_vmx() { 2 } else { 0 };
    if r.cr8 != stops {
        return Err(alloc::format!("the guest was stopped at CR8 {} times, not {}", r.cr8, stops));
    }
    Ok(alloc::format!(
        "the guest wrote 15 to its CR8 and read it back, {}, and the host's CR8 is still {}",
        if vm.is_vmx() { "stopped at each and answered from the shadow" } else { "the CPU keeping the shadow itself" },
        HOST_TPR))
}

fn check_spin(_vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    match r.stop {
        Stop::Budget if r.host > 0 => Ok(alloc::format!(
            "the host's interrupts got through {} times with the guest's off, and the host stopped it", r.host)),
        Stop::Budget => Err(String::from("no host interrupt got through while it spun")),
        _ => Err(String::from("it stopped by itself")),
    }
}
