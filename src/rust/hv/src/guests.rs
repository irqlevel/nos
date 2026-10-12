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
//!   mmio       a device's registers in the MMIO window stored to and loaded
//!              from by the forms of `mov` Linux's accessors are -- an
//!              immediate of each size, REX registers and a high byte,
//!              `movzx` and `movsx` -- each access a nested fault the host
//!              decodes and performs, what came back checked in RAM
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
//!   smp        a guest of two CPUs, each a task on a host CPU of its own:
//!              the first starts the second -- INIT and a start-up IPI, as
//!              a kernel does -- which comes up in real mode, climbs to long
//!              mode through protected mode, and sends the first an IPI;
//!              then the first's local APIC timer interrupts it
//!
//! Every one starts in long mode with paging on, as a 64-bit Linux kernel
//! is started -- but for the second CPU of `smp`, which starts where a
//! start-up IPI starts every CPU, in real mode. QEMU's TCG before 9.2 does
//! not put the accesses of a guest with paging off through the nested table
//! at all -- it takes a guest physical address for a host one -- so a
//! real-mode guest there runs out of the host's memory: `smp`, like a Linux
//! guest of more than one CPU, needs 9.2 or later under TCG (`hv-test.py`
//! refuses an older one), and a CPU's own extension anywhere else.

use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;

use hvarch::Result;
use kcore::time;

use crate::devices::Uart;
use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::mmio;
use crate::run::{Counts, GuestCpu, Host, LinuxGuest, Stop as GuestStop, Stopped};
use crate::smp::Doorbells;
use crate::svm::{Exit, LongMode};
use crate::vm::{Backend, Refusal, Vm};

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
    /// Accesses of the `mmio` guest's device, decoded and performed; and
    /// its registers, as they were written.
    mmio: u32,
    dev: [u8; MMIO_DEV_BYTES],
    /// Time-stamp counter ticks spent performing those accesses -- the walk
    /// to the instruction, its fetch and decoding, the device -- while the
    /// CPU's entries are profiled; `hv bench mmio profile` splits it out.
    emulate: u64,
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
        mmio: 0,
        dev: [0; MMIO_DEV_BYTES],
        emulate: 0,
        cr8: 0,
        at_hypercall: None,
        uart: Uart::new(),
        serial: String::new(),
        stop: Stop::Budget,
        ns: 0,
    };
    let start = time::boot_time_ns();
    let budget = budget_ms * kcore::consts::NS_PER_MS;
    let timed = vm.vcpu().profile().is_some();

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
            Exit::NestedFault { gpa, error } if gpa & !(PAGE - 1) == MMIO_DEV => {
                /* The `mmio` guest's device: the access performed, the
                 * instruction decoded -- or, one that cannot be, stopped. */
                use hvarch::x86::svm::vmcb::npf;
                let t0 = if timed { hvarch::x86::cpu::rdtsc() } else { 0 };
                let mut bytes = mmio::Bytes::default();
                let op = match mmio::begin(vm.vcpu(), vm.memory(), gpa, error & npf::WRITE != 0, &mut bytes) {
                    Ok(op) => op,
                    Err(_) => break Stop::Fault { gpa, error, rip },
                };
                let at = (gpa - MMIO_DEV) as usize;
                let n = usize::from(op.size());
                let Some(regs) = run.dev.get_mut(at..at + n) else {
                    break Stop::Fault { gpa, error, rip };
                };
                let mut value = 0u64;
                if op.is_write() {
                    regs.copy_from_slice(&op.value.to_le_bytes()[..n]);
                } else {
                    for (i, b) in regs.iter().enumerate() {
                        value |= u64::from(*b) << (8 * i);
                    }
                }
                mmio::finish(vm.vcpu_mut(), &op, value);
                run.mmio += 1;
                if timed {
                    run.emulate += hvarch::x86::cpu::rdtsc().saturating_sub(t0);
                }
            }
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
    /* Nothing enters the guest again: its state as that last exit left it,
     * whole, for a check that fails to show. */
    vm.vcpu_mut().read_whole_state();
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
        name: "mmio",
        about: "a device's registers stored to and loaded from by every form of mov Linux uses, each access decoded and performed",
        exceptions: ALL_EXCEPTIONS,
        budget_ms: 2000,
        build: build_mmio,
        check: check_mmio,
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

/* N loads of the `mmio` guest's device: each a nested fault whose
 * instruction the host fetches by the guest's paging, decodes and performs --
 * what an MMIO access costs beside a CPUID's round trip. N goes in at
 * `BENCH_ROUNDS_AT`, as for the CPUID loop. */
const BENCH_MMIO_CODE: [u8; 17] = [
    0xBE, 0x00, 0x00, 0x00, 0x00,                   // mov esi, N
    0xBF, 0x00, 0x00, 0xB0, 0xFE,                   // mov edi, 0xfeb00000
    0x8B, 0x07,                                     // .round: mov eax, [rdi]
    0xFF, 0xCE,                                     // dec esi
    0x75, 0xFA,                                     // jnz .round
    0xF4,                                           // hlt
];
const BENCH_MMIO_HLT: u64 = ENTRY + 0x10;

/// What `bench` measured.
pub struct Bench {
    pub exits: u64,
    pub ns: u64,
    /// Time-stamp counter ticks over the same stretch as `ns`.
    pub ticks: u64,
    /// The host's interrupts among the exits.
    pub host: u32,
    pub profile: Option<hvarch::x86::svm::Profile>,
    /// Of the time-stamp counter's ticks, those the host spent performing
    /// the MMIO loads, while profiled: 0 otherwise.
    pub emulate: u64,
}

/// What a VM exit costs on the CPU this runs on: `exits` CPUIDs, each after
/// a read of each of `pages` pages -- with a flush of the whole TLB on every
/// entry when `flush` says so, for what that costs, and each entry timed in
/// its parts when `profile` does -- or, with `mmio`, as many MMIO loads,
/// each decoded and performed. Timed from the first entry to the halt -- or
/// why it did not run to its halt.
pub fn bench(machine: &Machine, exits: u32, pages: u32, flush: bool, profile: bool, mmio: bool)
    -> core::result::Result<Bench, String>
{
    if mmio {
        return bench_mmio(machine, exits, profile);
    }
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
    Ok(Bench { exits: u64::from(r.cpuid), ns: r.ns, ticks, host: r.host, profile: vm.vcpu().profile(), emulate: 0 })
}

/// `bench`'s MMIO loop: `exits` loads of the device's first register.
fn bench_mmio(machine: &Machine, exits: u32, profile: bool) -> core::result::Result<Bench, String> {
    let mut code = BENCH_MMIO_CODE;
    code[BENCH_ROUNDS_AT..BENCH_ROUNDS_AT + 4].copy_from_slice(&exits.to_le_bytes());
    let mut vm = Vm::new(machine, ALL_EXCEPTIONS).map_err(|e| alloc::format!("no VM: {}", e))?;
    board(&mut vm, &code).map_err(|e| alloc::format!("no guest: {}", e))?;
    {
        let m = vm.memory_mut();
        m.write_obj(PDPT + MMIO_GIB * 8, &(PD_MMIO | PTE_P_W)).map_err(|e| alloc::format!("no guest: {}", e))?;
        m.write_obj(PD_MMIO + MMIO_PD_INDEX * 8, &((MMIO_DEV & !0x1F_FFFF) | PTE_P_W | PTE_LARGE))
            .map_err(|e| alloc::format!("no guest: {}", e))?;
    }
    vm.vcpu_mut().set_profile(profile);
    let t0 = hvarch::x86::cpu::rdtsc();
    let r = run(&mut vm, machine, BENCH_BUDGET_MS);
    let ticks = hvarch::x86::cpu::rdtsc().saturating_sub(t0);
    halted_at(&r, BENCH_MMIO_HLT)?;
    if r.mmio != exits {
        return Err(alloc::format!("{} MMIO loads performed of {}", r.mmio, exits));
    }
    Ok(Bench {
        exits: u64::from(r.mmio), ns: r.ns, ticks, host: r.host, profile: vm.vcpu().profile(), emulate: r.emulate,
    })
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
        .chain([SMP_NAME, IOAPIC_NAME, DEBUG_NAME, CR4_NAME, MSR_NAME, DR_NAME].into_iter())
}

/// Run the built-in guest `name` on the CPU this is called on -- and, for a
/// guest of more than one CPU, its others on tasks of their own, on other
/// CPUs the extension is on for -- and say what it did. `None` when there is
/// no such guest; otherwise whether it did what it was told.
pub fn run_one(machine: &Arc<Machine>, name: &str, out: &mut dyn Write) -> Option<bool> {
    if name == SMP_NAME {
        return Some(run_smp(machine, out));
    }
    if name == IOAPIC_NAME {
        return Some(run_ioapic(machine, out));
    }
    if name == DEBUG_NAME {
        return Some(run_debug(machine, out));
    }
    if name == CR4_NAME {
        return Some(run_cr4(machine, out));
    }
    if name == MSR_NAME {
        return Some(run_msr(machine, out));
    }
    if name == DR_NAME {
        return Some(run_dr(machine, out));
    }
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
        Stop::Refused(Refusal::NotItsMemory) => writeln!(
            out, "not entered -- the CPU was handed memory other than its guest's"),
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

/* The smp guest: two CPUs, the first put in long mode at `ENTRY` on the
 * guests' machine (`board`'s tables, and a 32-bit code segment beside
 * them), the second waiting for a start-up IPI, as every CPU but the first
 * of a PC does. The first gives itself an IDT, sends the second INIT and two
 * start-up IPIs for `SMP_AP_AT` -- the MP protocol's sequence, as Linux sends
 * it -- and halts until the second's IPI comes; then sets its APIC timer
 * running, one-shot, and halts until that interrupts it. The second comes up
 * in real mode there, loads the GDT, turns on protected mode and then
 * paging with EFER.LME -- long mode, over the first's page table -- and says
 * who it is and sends its IPI. Each writes what its x2APIC and CPUID call
 * it, and each ends halted with interrupts off: when both are, the guest has
 * stopped, which is how it is found to be done. Each program is the output
 * of `nasm -f bin`, `bits 64` and `org 0x8000` for the first, `bits 16` to
 * `bits 64` and `org 0x9000` for the second. */
const SMP_NAME: &str = "smp";
const SMP_ABOUT: &str = "two CPUs: the first starts the second -- INIT and a start-up IPI, into real mode -- \
                         which climbs to long mode and sends it an IPI; then its APIC timer";

const SMP_BSP_CODE: &[u8] = &[
    0xB9, 0x02, 0x08, 0x00, 0x00,                           // mov ecx, 0x802 ; x2APIC ID
    0x0F, 0x32,                                             // rdmsr
    0x89, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,               // mov [0x7000], eax
    0xB8, 0x01, 0x00, 0x00, 0x00,                           // mov eax, 1
    0x0F, 0xA2,                                             // cpuid
    0xC1, 0xEB, 0x18,                                       // shr ebx, 24 ; CPUID's initial APIC ID
    0x89, 0x1C, 0x25, 0x04, 0x70, 0x00, 0x00,               // mov [0x7004], ebx
    0x48, 0x8D, 0x05, 0xC6, 0x00, 0x00, 0x00,               // lea rax, [rel ipi] ; gate 0x40: the AP's IPI
    0xBF, 0x00, 0x64, 0x00, 0x00,                           // mov edi, 0x6400
    0xE8, 0x96, 0x00, 0x00, 0x00,                           // call gate
    0x48, 0x8D, 0x05, 0xC2, 0x00, 0x00, 0x00,               // lea rax, [rel tick] ; gate 0x41: the APIC timer
    0xBF, 0x10, 0x64, 0x00, 0x00,                           // mov edi, 0x6410
    0xE8, 0x85, 0x00, 0x00, 0x00,                           // call gate
    0x0F, 0x01, 0x1D, 0xCF, 0x00, 0x00, 0x00,               // lidt [rel idtr]
    0xB9, 0x30, 0x08, 0x00, 0x00,                           // mov ecx, 0x830 ; ICR, to APIC ID 1
    0xBA, 0x01, 0x00, 0x00, 0x00,                           // mov edx, 1
    0xB8, 0x00, 0x45, 0x00, 0x00,                           // mov eax, 0x4500 ; INIT, asserted
    0x0F, 0x30,                                             // wrmsr
    0xB8, 0x00, 0x85, 0x00, 0x00,                           // mov eax, 0x8500 ; INIT, de-asserted
    0x0F, 0x30,                                             // wrmsr
    0xB8, 0x09, 0x06, 0x00, 0x00,                           // mov eax, 0x0609 ; start-up at 0x9000
    0x0F, 0x30,                                             // wrmsr
    0x0F, 0x30,                                             // wrmsr ; and again, as the MP protocol has it
    0xFB,                                                   // .ipi: sti
    0xF4,                                                   // hlt
    0xFA,                                                   // cli
    0x83, 0x3C, 0x25, 0x10, 0x70, 0x00, 0x00, 0x00,         // cmp dword [0x7010], 0
    0x74, 0xF3,                                             // je .ipi
    0xB9, 0x3E, 0x08, 0x00, 0x00,                           // mov ecx, 0x83e ; timer divide: by 1
    0x31, 0xD2,                                             // xor edx, edx
    0xB8, 0x0B, 0x00, 0x00, 0x00,                           // mov eax, 0xb
    0x0F, 0x30,                                             // wrmsr
    0xB9, 0x32, 0x08, 0x00, 0x00,                           // mov ecx, 0x832 ; LVT timer: one-shot, vector 0x41
    0xB8, 0x41, 0x00, 0x00, 0x00,                           // mov eax, 0x41
    0x0F, 0x30,                                             // wrmsr
    0xB9, 0x38, 0x08, 0x00, 0x00,                           // mov ecx, 0x838 ; initial count: 2 ms at 1 GHz
    0xB8, 0x80, 0x84, 0x1E, 0x00,                           // mov eax, 2000000
    0x0F, 0x30,                                             // wrmsr
    0xFB,                                                   // .tick: sti
    0xF4,                                                   // hlt
    0xFA,                                                   // cli
    0x83, 0x3C, 0x25, 0x14, 0x70, 0x00, 0x00, 0x00,         // cmp dword [0x7014], 0
    0x74, 0xF3,                                             // je .tick
    0xB9, 0x39, 0x08, 0x00, 0x00,                           // mov ecx, 0x839 ; current count: run out
    0x0F, 0x32,                                             // rdmsr
    0x89, 0x04, 0x25, 0x18, 0x70, 0x00, 0x00,               // mov [0x7018], eax
    0xC7, 0x04, 0x25, 0x0C, 0x70, 0x00, 0x00, 0x42, 0x53, 0x50, 0x30,// mov dword [0x700c], 0x30505342 ; 'BSP0'
    0xFA,                                                   // .dead: cli
    0xF4,                                                   // hlt
    0xEB, 0xFC,                                             // jmp .dead
    0x66, 0x89, 0x07,                                       // gate: mov [rdi], ax ; an interrupt gate for the handler at rax
    0x66, 0xC7, 0x47, 0x02, 0x08, 0x00,                     // mov word [rdi + 2], 0x08
    0x66, 0xC7, 0x47, 0x04, 0x00, 0x8E,                     // mov word [rdi + 4], 0x8e00
    0x48, 0xC1, 0xE8, 0x10,                                 // shr rax, 16
    0x66, 0x89, 0x47, 0x06,                                 // mov [rdi + 6], ax
    0x48, 0xC1, 0xE8, 0x10,                                 // shr rax, 16
    0x89, 0x47, 0x08,                                       // mov [rdi + 8], eax
    0xC7, 0x47, 0x0C, 0x00, 0x00, 0x00, 0x00,               // mov dword [rdi + 12], 0
    0xC3,                                                   // ret
    0xC7, 0x04, 0x25, 0x10, 0x70, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,// ipi: mov dword [0x7010], 1
    0xEB, 0x0B,                                             // jmp eoi
    0xC7, 0x04, 0x25, 0x14, 0x70, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,// tick: mov dword [0x7014], 1
    0x50,                                                   // eoi: push rax
    0x51,                                                   // push rcx
    0x52,                                                   // push rdx
    0xB9, 0x0B, 0x08, 0x00, 0x00,                           // mov ecx, 0x80b ; EOI
    0x31, 0xC0,                                             // xor eax, eax
    0x31, 0xD2,                                             // xor edx, edx
    0x0F, 0x30,                                             // wrmsr
    0x5A,                                                   // pop rdx
    0x59,                                                   // pop rcx
    0x58,                                                   // pop rax
    0x48, 0xCF,                                             // iretq
    0x1F, 0x04,                                             // idtr: dw 0x42 * 16 - 1
    0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,         // dq 0x6000
];

const SMP_AP_CODE: &[u8] = &[
    0xFA,                                                   // cli
    0x31, 0xC0,                                             // xor ax, ax
    0x8E, 0xD8,                                             // mov ds, ax
    0x66, 0x0F, 0x01, 0x16, 0x97, 0x90,                     // o32 lgdt [gdtr]
    0x0F, 0x20, 0xC0,                                       // mov eax, cr0
    0x0C, 0x01,                                             // or al, 1 ; PE
    0x0F, 0x22, 0xC0,                                       // mov cr0, eax
    0x66, 0xEA, 0x1B, 0x90, 0x00, 0x00, 0x28, 0x00,         // jmp dword 0x28:ap32
    0x66, 0xB8, 0x10, 0x00,                                 // mov ax, 0x10
    0x8E, 0xD8,                                             // mov ds, ax
    0x8E, 0xC0,                                             // mov es, ax
    0x8E, 0xD0,                                             // mov ss, ax
    0x0F, 0x20, 0xE0,                                       // mov eax, cr4
    0x83, 0xC8, 0x20,                                       // or eax, 0x20 ; PAE
    0x0F, 0x22, 0xE0,                                       // mov cr4, eax
    0xB8, 0x00, 0x20, 0x00, 0x00,                           // mov eax, 0x2000 ; the BSP's PML4
    0x0F, 0x22, 0xD8,                                       // mov cr3, eax
    0xB9, 0x80, 0x00, 0x00, 0xC0,                           // mov ecx, 0xc0000080 ; EFER.LME
    0x0F, 0x32,                                             // rdmsr
    0x0D, 0x00, 0x01, 0x00, 0x00,                           // or eax, 0x100
    0x0F, 0x30,                                             // wrmsr
    0x0F, 0x20, 0xC0,                                       // mov eax, cr0
    0x0D, 0x00, 0x00, 0x00, 0x80,                           // or eax, 0x80000000 ; PG: long mode
    0x0F, 0x22, 0xC0,                                       // mov cr0, eax
    0xEA, 0x56, 0x90, 0x00, 0x00, 0x08, 0x00,               // jmp 0x08:ap64
    0xBC, 0x00, 0xE0, 0x00, 0x00,                           // mov rsp, 0xe000
    0xB9, 0x02, 0x08, 0x00, 0x00,                           // mov ecx, 0x802 ; x2APIC ID
    0x0F, 0x32,                                             // rdmsr
    0x89, 0x04, 0x25, 0x00, 0x71, 0x00, 0x00,               // mov [0x7100], eax
    0xB8, 0x01, 0x00, 0x00, 0x00,                           // mov eax, 1
    0x0F, 0xA2,                                             // cpuid
    0xC1, 0xEB, 0x18,                                       // shr ebx, 24
    0x89, 0x1C, 0x25, 0x04, 0x71, 0x00, 0x00,               // mov [0x7104], ebx
    0xC7, 0x04, 0x25, 0x08, 0x71, 0x00, 0x00, 0x41, 0x50, 0x36, 0x34,// mov dword [0x7108], 0x34365041 ; 'AP64'
    0xB9, 0x30, 0x08, 0x00, 0x00,                           // mov ecx, 0x830 ; an IPI to APIC ID 0, vector 0x40
    0x31, 0xD2,                                             // xor edx, edx
    0xB8, 0x40, 0x00, 0x00, 0x00,                           // mov eax, 0x40
    0x0F, 0x30,                                             // wrmsr
    0xFA,                                                   // .dead: cli
    0xF4,                                                   // hlt
    0xEB, 0xFC,                                             // jmp .dead
    0x2F, 0x00,                                             // gdtr: dw 6 * 8 - 1
    0x00, 0x10, 0x00, 0x00,                                 // dd 0x1000
];

/// Where the second CPU starts: the page of the start-up IPI's vector,
/// 0x09 -- which `SMP_BSP_CODE` sends.
const SMP_AP_AT: u64 = 0x9000;
/// The 32-bit flat code segment the second CPU goes through on its way to
/// long mode, at selector 0x28: after `board`'s null, code, data and TSS.
const GDT_CODE32: u64 = 0x00CF_9B00_0000_FFFF;
const SMP_GDT_ENTRIES: u64 = GDT_ENTRIES + 1;
/* What each CPU leaves at `RESULTS`: the first its x2APIC ID and CPUID's
 * APIC ID at +0 and +4, 'BSP0' at +0xC once done, 1 at +0x10 when the
 * second's IPI came and at +0x14 when its timer ran out, and the timer's
 * count after at +0x18; the second the same two IDs at +0x100 and +0x104,
 * and 'AP64' at +0x108 once in long mode. */
const SMP_BSP_ID: u64 = RESULTS;
const SMP_BSP_CPUID: u64 = RESULTS + 0x4;
const SMP_BSP_DONE: u64 = RESULTS + 0xC;
const SMP_IPI: u64 = RESULTS + 0x10;
const SMP_TICK: u64 = RESULTS + 0x14;
const SMP_COUNT: u64 = RESULTS + 0x18;
const SMP_AP_ID: u64 = RESULTS + 0x100;
const SMP_AP_CPUID: u64 = RESULTS + 0x104;
const SMP_AP_DONE: u64 = RESULTS + 0x108;
const SMP_BSP_MARK: u32 = u32::from_le_bytes(*b"BSP0");
const SMP_AP_MARK: u32 = u32::from_le_bytes(*b"AP64");
/// IPIs the first CPU sends: INIT, its de-assert, and two start-up IPIs.
const SMP_BSP_IPIS: u64 = 4;
const SMP_BUDGET_MS: u64 = 10_000;

/// The host a built-in guest's CPUs run under: no console input, nothing
/// said, never a stop but its own.
struct Quiet;

impl Host for Quiet {
    fn output(&self, _byte: u8) {}
    fn input(&self, _at_prompt: bool) -> Option<u8> {
        None
    }
    fn stop_requested(&self) -> bool {
        false
    }
}

/// The second CPU of the smp guest, on a task of its own.
struct SmpAp {
    guest: Arc<LinuxGuest>,
    cpu: GuestCpu,
    machine: Arc<Machine>,
    deadline: u64,
    counts: Arc<kcore::sync::Mutex<Counts>>,
}

fn smp_ap(run: SmpAp) {
    let SmpAp { guest, mut cpu, machine, deadline, counts } = run;
    let c = guest.run(&mut cpu, &machine, deadline, &Quiet);
    *counts.lock() = c;
    /* Its CPU goes in this task, before the guest: under VT-x the VMCS is
     * cleared off its host CPU from task context. */
    drop(cpu);
}

/// The guests' machine for the smp guest: `board`'s tables in memory it
/// already has -- the guest's, made with it -- the 32-bit code segment
/// beside them, both CPUs' code, and the first CPU in long mode at `ENTRY`.
fn smp_board(m: &GuestMemory, bsp: &mut GuestCpu) -> Result<()> {
    let tss_low = TSS_LIMIT | (TSS & 0xFF_FFFF) << 16 | TSS_BUSY_PRESENT << 40 | (TSS >> 24 & 0xFF) << 56;
    let tss_high = TSS >> 32;
    for (i, entry) in [0, GDT_CODE64, GDT_DATA, tss_low, tss_high, GDT_CODE32].iter().enumerate() {
        m.write_obj(GDT + i as u64 * 8, entry)?;
    }
    m.write_obj(PML4, &(PDPT | PTE_P_W))?;
    m.write_obj(PDPT, &(PD_LOW | PTE_P_W))?;
    m.write_obj(PD_LOW, &(PTE_P_W | PTE_LARGE))?;
    m.write(ENTRY, SMP_BSP_CODE)?;
    m.write(SMP_AP_AT, SMP_AP_CODE)?;
    bsp.backend_mut().long_mode(&LongMode {
        entry: ENTRY,
        stack: STACK,
        cr3: PML4,
        gdt: GDT,
        gdt_limit: (SMP_GDT_ENTRIES * 8 - 1) as u16,
        idt_limit: NO_IDT,
        code_selector: 0x08,
        data_selector: 0x10,
        tss_selector: 0x18,
        tss: TSS,
    });
    Ok(())
}

/// Run the smp guest: its first CPU on this task, its second on a task of
/// its own on another CPU the extension is on for -- this one, when it is on
/// for no other.
fn run_smp(machine: &Arc<Machine>, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", SMP_NAME, SMP_ABOUT);
    let fail = |out: &mut dyn Write, why: &str| {
        let _ = writeln!(out, "hv: guest {} FAILED -- {}", SMP_NAME, why);
        false
    };
    let Some(doorbells) = Doorbells::new(2) else { return fail(out, "out of memory") };
    let (guest, mut cpus) = match LinuxGuest::new(machine, MEMORY, 2, Arc::new(doorbells), true, false) {
        Ok(made) => made,
        Err(e) => return fail(out, &alloc::format!("could not be made: {}", e)),
    };
    let Some(ap) = cpus.pop() else { return fail(out, "no second CPU") };
    let Some(mut bsp) = cpus.pop() else { return fail(out, "no first CPU") };
    if let Err(e) = smp_board(guest.memory(), &mut bsp) {
        return fail(out, &alloc::format!("could not be made: {}", e));
    }
    let guest = Arc::new(guest);
    let Some(ap_counts) = kcore::sync::Mutex::new(Counts::default()) else { return fail(out, "out of memory") };
    let ap_counts = Arc::new(ap_counts);

    let here = kcore::cpu::id();
    let enabled = machine.enabled_mask();
    let ap_cpu = (0..u64::BITS).find(|&c| c != here && enabled & (1u64 << c) != 0).unwrap_or(here);
    let start = time::boot_time_ns();
    let deadline = start.saturating_add(SMP_BUDGET_MS * kcore::consts::NS_PER_MS);
    let run = SmpAp { guest: guest.clone(), cpu: ap, machine: machine.clone(), deadline, counts: ap_counts.clone() };
    let Some(task) = kcore::task::spawn_on_with("hv/smp/cpu1", 1u64 << ap_cpu, run, smp_ap) else {
        return fail(out, "no task for its second CPU");
    };
    let bsp_counts = guest.run(&mut bsp, machine, deadline, &Quiet);
    drop(task);
    drop(bsp);
    let ns = time::boot_time_ns().saturating_sub(start);
    let counts = [bsp_counts, *ap_counts.lock()];
    let stopped = guest.take_stopped()
        .unwrap_or(Stopped { stop: GuestStop::Requested, cpu: 0, dump: String::new() });

    let states = guest.cpu_states();
    if let GuestStop::Refused(Refusal::NotOn(cpu)) = stopped.stop {
        let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first", SMP_NAME, cpu);
        return false;
    }
    let _ = write!(out, "  ran on     cpu");
    for (i, (_, host)) in states.iter().enumerate() {
        match host {
            Some(h) => { let _ = write!(out, " {}{}", h, if i == 0 { " and" } else { "" }); }
            None => { let _ = write!(out, " (cpu {} never entered)", i); }
        }
    }
    let _ = writeln!(out, ", {} us", ns / kcore::consts::NS_PER_US);
    let _ = writeln!(out, "  exits      cpu 0: {} ({} wrmsr, {} hlt, {} apic irq); cpu 1: {} ({} wrmsr, {} hlt)",
                     counts[0].exits, counts[0].msr_write, counts[0].hlt, counts[0].apic,
                     counts[1].exits, counts[1].msr_write, counts[1].hlt);
    let mut says = String::new();
    let _ = crate::guests::describe_stop(&stopped.stop, &mut says);
    let _ = writeln!(out, "  stopped    {}", says);

    match check_smp(guest.memory(), &stopped, &counts) {
        Ok(checked) => {
            let _ = writeln!(out, "  checked    {}", checked);
            let _ = writeln!(out, "hv: guest {} ok", SMP_NAME);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", SMP_NAME, why);
            let _ = writeln!(out, "  cpu {} as it stopped:", stopped.cpu);
            let _ = out.write_str(&stopped.dump);
            false
        }
    }
}

/// How a guest of the Linux runtime stopped, in a line.
fn describe_stop(stop: &GuestStop, out: &mut dyn Write) -> core::fmt::Result {
    match *stop {
        GuestStop::Halted { rip } => write!(out, "both CPUs halted with interrupts off, the last at {:#x}", rip),
        GuestStop::Budget => write!(out, "by the host, its {} ms up", SMP_BUDGET_MS),
        other => write!(out, "{:?}", other),
    }
}

fn check_smp(m: &GuestMemory, stopped: &Stopped, counts: &[Counts; 2]) -> core::result::Result<String, String> {
    if !matches!(stopped.stop, GuestStop::Halted { .. }) {
        return Err(String::from("it did not stop with both CPUs halted"));
    }
    let word = |gpa: u64| m.read_obj::<u32>(gpa).map_err(|e| alloc::format!("guest memory at {:#x}: {}", gpa, e));
    let (bsp_id, bsp_cpuid) = (word(SMP_BSP_ID)?, word(SMP_BSP_CPUID)?);
    let (ap_id, ap_cpuid) = (word(SMP_AP_ID)?, word(SMP_AP_CPUID)?);
    if word(SMP_AP_DONE)? != SMP_AP_MARK {
        return Err(String::from("the second CPU never reached long mode"));
    }
    if (bsp_id, bsp_cpuid, ap_id, ap_cpuid) != (0, 0, 1, 1) {
        return Err(alloc::format!("the CPUs call themselves {} (CPUID {}) and {} (CPUID {}), not 0 and 1",
                                  bsp_id, bsp_cpuid, ap_id, ap_cpuid));
    }
    if word(SMP_IPI)? != 1 {
        return Err(String::from("the second CPU's IPI never reached the first"));
    }
    if word(SMP_TICK)? != 1 || word(SMP_BSP_DONE)? != SMP_BSP_MARK {
        return Err(String::from("the first CPU's APIC timer never interrupted it"));
    }
    let count = word(SMP_COUNT)?;
    if count != 0 {
        return Err(alloc::format!("the one-shot timer reads {} after it ran out, not 0", count));
    }
    if counts[1].started != 1 || counts[0].ipi_sent != SMP_BSP_IPIS || counts[1].ipi_sent != 1
        || counts[0].ipi_taken != 1 || counts[0].timer != 1
    {
        return Err(alloc::format!(
            "the counts are off: cpu 1 started {} times, sent {} IPIs; cpu 0 sent {}, took {}, its timer ran out {} times",
            counts[1].started, counts[1].ipi_sent, counts[0].ipi_sent, counts[0].ipi_taken, counts[0].timer));
    }
    Ok(String::from("cpu 1 started by INIT and a start-up IPI, came up in real mode and reached long mode, \
                     x2APIC ID 1 and CPUID's the same; its IPI reached cpu 0, whose one-shot APIC timer then ran out and interrupted it"))
}

/* The ioapic guest: the smp guest's machine with an IO-APIC, its first CPU
 * alone running -- the second waits for a start-up IPI it is never sent --
 * and its local APIC in xAPIC mode, as a machine with an IO-APIC has it
 * (`LinuxGuest::new`). It reads the IO-APIC's version, then wires the
 * serial port's pin, 4, to itself through its page, as an edge: enabling
 * the THR-empty interrupt raises the line, an edge; writing a byte pulses
 * it, another; masked, a byte's pulse is lost, and unmasking an
 * edge-triggered pin sends nothing. Then as a level: the line up, sent,
 * and sent again at each EOI while it stays up -- the entry's remote IRR
 * read set in the handler -- until the third handler reads IIR, which
 * takes the THR-empty interrupt back and the line down; its EOI then sends
 * nothing, and the entry after reads its remote IRR clear. Every EOI is a
 * write of the xAPIC page. The output of clang (Intel syntax), `bits 64`
 * at `ENTRY`, shown in NASM's words. */
const IOAPIC_NAME: &str = "ioapic";
const IOAPIC_ABOUT: &str = "an IO-APIC, through its page: the serial port's pin as an edge, then as a level \
                            ended by EOI and sent again while its line stays up";

const IOAPIC_CODE: &[u8] = &[
    0x48, 0x8D, 0x05, 0xF6, 0x00, 0x00, 0x00,               // lea rax, [rel edge] ; gate 0x50: the edge-triggered pin's handler
    0xBF, 0x00, 0xA5, 0x00, 0x00,                           // mov edi, 0xA500
    0xE8, 0xC6, 0x00, 0x00, 0x00,                           // call gate
    0x48, 0x8D, 0x05, 0xFB, 0x00, 0x00, 0x00,               // lea rax, [rel level] ; gate 0x51: the level-triggered pin's
    0xBF, 0x10, 0xA5, 0x00, 0x00,                           // mov edi, 0xA510
    0xE8, 0xB5, 0x00, 0x00, 0x00,                           // call gate
    0x0F, 0x01, 0x1D, 0x26, 0x01, 0x00, 0x00,               // lidt [rel idtr]
    0xBE, 0x00, 0x00, 0xC0, 0xFE,                           // mov esi, 0xFEC00000 ; the IO-APIC
    0xC7, 0x06, 0x01, 0x00, 0x00, 0x00,                     // mov dword [rsi], 1 ; IOREGSEL: the version register
    0x8B, 0x46, 0x10,                                       // mov eax, dword [rsi + 0x10] ; IOWIN
    0x89, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,               // mov dword [0x7000], eax
    0xC7, 0x06, 0x19, 0x00, 0x00, 0x00,                     // mov dword [rsi], 0x19 ; pin 4's entry, high half: to APIC ID 0
    0xC7, 0x46, 0x10, 0x00, 0x00, 0x00, 0x00,               // mov dword [rsi + 0x10], 0
    0xC7, 0x06, 0x18, 0x00, 0x00, 0x00,                     // mov dword [rsi], 0x18 ; low half: vector 0x50, fixed, physical, edge
    0xC7, 0x46, 0x10, 0x50, 0x00, 0x00, 0x00,               // mov dword [rsi + 0x10], 0x50
    0x66, 0xBA, 0xF9, 0x03,                                 // mov dx, 0x3F9 ; IER: the THR-empty interrupt on, the line up -- an edge
    0xB0, 0x02,                                             // mov al, 2
    0xEE,                                                   // out dx, al
    0xFB,                                                   // .w1: sti ; its interrupt taken in the halt the sti's shadow covers
    0xF4,                                                   // hlt
    0xFA,                                                   // cli
    0x83, 0x3C, 0x25, 0x04, 0x70, 0x00, 0x00, 0x01,         // cmp dword [0x7004], 1
    0x72, 0xF3,                                             // jb .w1
    0x66, 0xBA, 0xF8, 0x03,                                 // mov dx, 0x3F8 ; a byte written: the line pulsed -- an edge
    0xB0, 0x78,                                             // mov al, 0x78
    0xEE,                                                   // out dx, al
    0xFB,                                                   // .w2: sti
    0xF4,                                                   // hlt
    0xFA,                                                   // cli
    0x83, 0x3C, 0x25, 0x04, 0x70, 0x00, 0x00, 0x02,         // cmp dword [0x7004], 2
    0x72, 0xF3,                                             // jb .w2
    0xC7, 0x46, 0x10, 0x50, 0x00, 0x01, 0x00,               // mov dword [rsi + 0x10], 0x10050 ; masked: a byte's pulse is lost
    0xEE,                                                   // out dx, al
    0xC7, 0x46, 0x10, 0x50, 0x00, 0x00, 0x00,               // mov dword [rsi + 0x10], 0x50 ; and unmasking an edge-triggered pin sends nothing
    0xFB,                                                   // sti
    0xB9, 0x10, 0x00, 0x00, 0x00,                           // mov ecx, 16
    0xE6, 0x80,                                             // .s1: out 0x80, al ; exits, at each of which one pending would go in
    0xE2, 0xFC,                                             // loop .s1
    0xFA,                                                   // cli
    0x66, 0xBA, 0xF9, 0x03,                                 // mov dx, 0x3F9 ; IER 0: the line down
    0x31, 0xC0,                                             // xor eax, eax
    0xEE,                                                   // out dx, al
    0xC7, 0x46, 0x10, 0x51, 0x80, 0x00, 0x00,               // mov dword [rsi + 0x10], 0x8051 ; vector 0x51, level-triggered
    0xB0, 0x02,                                             // mov al, 2 ; the line up: sent, and again at each EOI while it stays up
    0xEE,                                                   // out dx, al
    0xFB,                                                   // .w3: sti
    0xF4,                                                   // hlt
    0xFA,                                                   // cli
    0x83, 0x3C, 0x25, 0x08, 0x70, 0x00, 0x00, 0x03,         // cmp dword [0x7008], 3
    0x72, 0xF3,                                             // jb .w3
    0xC7, 0x06, 0x18, 0x00, 0x00, 0x00,                     // mov dword [rsi], 0x18
    0x8B, 0x46, 0x10,                                       // mov eax, dword [rsi + 0x10] ; the entry after: remote IRR clear
    0x89, 0x04, 0x25, 0x10, 0x70, 0x00, 0x00,               // mov dword [0x7010], eax
    0xC7, 0x04, 0x25, 0x14, 0x70, 0x00, 0x00, 0x49, 0x4F, 0x41, 0x50, // mov dword [0x7014], 0x50414F49 ; 'IOAP'
    0xFA,                                                   // .dead: cli
    0xF4,                                                   // hlt
    0xEB, 0xFC,                                             // jmp .dead
    0x66, 0x89, 0x07,                                       // gate: mov word [rdi], ax ; an interrupt gate for the handler at rax
    0x66, 0xC7, 0x47, 0x02, 0x08, 0x00,                     // mov word [rdi + 2], 0x08
    0x66, 0xC7, 0x47, 0x04, 0x00, 0x8E,                     // mov word [rdi + 4], 0x8E00
    0x48, 0xC1, 0xE8, 0x10,                                 // shr rax, 16
    0x66, 0x89, 0x47, 0x06,                                 // mov word [rdi + 6], ax
    0x48, 0xC1, 0xE8, 0x10,                                 // shr rax, 16
    0x89, 0x47, 0x08,                                       // mov dword [rdi + 8], eax
    0xC7, 0x47, 0x0C, 0x00, 0x00, 0x00, 0x00,               // mov dword [rdi + 12], 0
    0xC3,                                                   // ret
    0x50,                                                   // edge: push rax
    0xFF, 0x04, 0x25, 0x04, 0x70, 0x00, 0x00,               // inc dword [0x7004]
    0xB8, 0xB0, 0x00, 0xE0, 0xFE,                           // mov eax, 0xFEE000B0 ; EOI
    0xC7, 0x00, 0x00, 0x00, 0x00, 0x00,                     // mov dword [rax], 0
    0x58,                                                   // pop rax
    0x48, 0xCF,                                             // iretq
    0x50,                                                   // level: push rax
    0x52,                                                   // push rdx
    0xFF, 0x04, 0x25, 0x08, 0x70, 0x00, 0x00,               // inc dword [0x7008]
    0xB8, 0x00, 0x00, 0xC0, 0xFE,                           // mov eax, 0xFEC00000
    0xC7, 0x00, 0x18, 0x00, 0x00, 0x00,                     // mov dword [rax], 0x18
    0x8B, 0x50, 0x10,                                       // mov edx, dword [rax + 0x10] ; the entry in service: remote IRR set
    0x89, 0x14, 0x25, 0x0C, 0x70, 0x00, 0x00,               // mov dword [0x700C], edx
    0x83, 0x3C, 0x25, 0x08, 0x70, 0x00, 0x00, 0x03,         // cmp dword [0x7008], 3
    0x72, 0x05,                                             // jb .eoi
    0x66, 0xBA, 0xFA, 0x03,                                 // mov dx, 0x3FA ; the third: IIR read, the THR-empty interrupt taken back
    0xEC,                                                   // in al, dx
    0xB8, 0xB0, 0x00, 0xE0, 0xFE,                           // .eoi: mov eax, 0xFEE000B0 ; EOI: back to the IO-APIC
    0xC7, 0x00, 0x00, 0x00, 0x00, 0x00,                     // mov dword [rax], 0
    0x5A,                                                   // pop rdx
    0x58,                                                   // pop rax
    0x48, 0xCF,                                             // iretq
    0x1F, 0x05,                                             // idtr: dw 0x51F ; 0x52 gates
    0x00, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,         // dq 0xA000
];

/* What the guest leaves at `RESULTS`: the IO-APIC's version at +0, the edge
 * interrupts taken at +4 and the level ones at +8, pin 4's entry as the
 * last level handler read it at +0xC and after at +0x10, and 'IOAP' at
 * +0x14 once done. Its IDT is at 0xA000 (0x52 gates), and the IO-APIC's
 * and the local APIC's pages are mapped to themselves, 2 MiB each, through
 * `PD_MMIO`. */
const IOAPIC_VERSION: u64 = RESULTS;
const IOAPIC_EDGES: u64 = RESULTS + 0x4;
const IOAPIC_LEVELS: u64 = RESULTS + 0x8;
const IOAPIC_IN_SERVICE: u64 = RESULTS + 0xC;
const IOAPIC_AFTER: u64 = RESULTS + 0x10;
const IOAPIC_DONE: u64 = RESULTS + 0x14;
const IOAPIC_MARK: u32 = u32::from_le_bytes(*b"IOAP");
/// What the version register reads: version 0x20, 24 entries.
const IOAPIC_VERSION_READ: u32 = 0x0017_0020;
/// Pin 4's entry, level-triggered with vector 0x51, and its remote IRR.
const IOAPIC_LEVEL_ENTRY: u32 = 0x8051;
const IOAPIC_REMOTE_IRR: u32 = 1 << 14;
/// The interrupts the guest is to take: two edges, three levels.
const IOAPIC_EDGES_WANTED: u32 = 2;
const IOAPIC_LEVELS_WANTED: u32 = 3;

/// The guests' machine for the ioapic guest: `smp_board`'s, but the GDT
/// without the 32-bit segment, its code, and the IO-APIC's and the local
/// APIC's pages mapped for it.
fn ioapic_board(m: &GuestMemory, bsp: &mut GuestCpu) -> Result<()> {
    let tss_low = TSS_LIMIT | (TSS & 0xFF_FFFF) << 16 | TSS_BUSY_PRESENT << 40 | (TSS >> 24 & 0xFF) << 56;
    let tss_high = TSS >> 32;
    for (i, entry) in [0, GDT_CODE64, GDT_DATA, tss_low, tss_high].iter().enumerate() {
        m.write_obj(GDT + i as u64 * 8, entry)?;
    }
    m.write_obj(PML4, &(PDPT | PTE_P_W))?;
    m.write_obj(PDPT, &(PD_LOW | PTE_P_W))?;
    m.write_obj(PD_LOW, &(PTE_P_W | PTE_LARGE))?;
    m.write_obj(PDPT + MMIO_GIB * 8, &(PD_MMIO | PTE_P_W))?;
    for page in [crate::devices::ioapic::BASE, crate::lapic::DEFAULT_BASE] {
        let index = (page - MMIO_GIB * GIB) >> 21;
        m.write_obj(PD_MMIO + index * 8, &((page & !0x1F_FFFF) | PTE_P_W | PTE_LARGE))?;
    }
    m.write(ENTRY, IOAPIC_CODE)?;
    bsp.backend_mut().long_mode(&LongMode {
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

/// Run the ioapic guest: its first CPU on this task, its second -- which
/// only waits -- on a task of its own, so that it is found asleep when the
/// first stops.
fn run_ioapic(machine: &Arc<Machine>, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", IOAPIC_NAME, IOAPIC_ABOUT);
    let fail = |out: &mut dyn Write, why: &str| {
        let _ = writeln!(out, "hv: guest {} FAILED -- {}", IOAPIC_NAME, why);
        false
    };
    let Some(doorbells) = Doorbells::new(2) else { return fail(out, "out of memory") };
    let (guest, mut cpus) = match LinuxGuest::new(machine, MEMORY, 2, Arc::new(doorbells), true, true) {
        Ok(made) => made,
        Err(e) => return fail(out, &alloc::format!("could not be made: {}", e)),
    };
    let Some(ap) = cpus.pop() else { return fail(out, "no second CPU") };
    let Some(mut bsp) = cpus.pop() else { return fail(out, "no first CPU") };
    if let Err(e) = ioapic_board(guest.memory(), &mut bsp) {
        return fail(out, &alloc::format!("could not be made: {}", e));
    }
    let guest = Arc::new(guest);
    let Some(ap_counts) = kcore::sync::Mutex::new(Counts::default()) else { return fail(out, "out of memory") };
    let ap_counts = Arc::new(ap_counts);

    let here = kcore::cpu::id();
    let enabled = machine.enabled_mask();
    let ap_cpu = (0..u64::BITS).find(|&c| c != here && enabled & (1u64 << c) != 0).unwrap_or(here);
    let start = time::boot_time_ns();
    let deadline = start.saturating_add(SMP_BUDGET_MS * kcore::consts::NS_PER_MS);
    let run = SmpAp { guest: guest.clone(), cpu: ap, machine: machine.clone(), deadline, counts: ap_counts.clone() };
    let Some(task) = kcore::task::spawn_on_with("hv/ioapic/cpu1", 1u64 << ap_cpu, run, smp_ap) else {
        return fail(out, "no task for its second CPU");
    };
    let counts = guest.run(&mut bsp, machine, deadline, &Quiet);
    drop(task);
    drop(bsp);
    let ns = time::boot_time_ns().saturating_sub(start);
    let stopped = guest.take_stopped()
        .unwrap_or(Stopped { stop: GuestStop::Requested, cpu: 0, dump: String::new() });
    if let GuestStop::Refused(Refusal::NotOn(cpu)) = stopped.stop {
        let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first", IOAPIC_NAME, cpu);
        return false;
    }
    let host = guest.cpu_states().first().and_then(|&(_, h)| h);
    match host {
        Some(h) => { let _ = writeln!(out, "  ran on     cpu {}, {} us", h, ns / kcore::consts::NS_PER_US); }
        None => { let _ = writeln!(out, "  ran on     no cpu, {} us", ns / kcore::consts::NS_PER_US); }
    }
    let stats = guest.ioapic_stats().unwrap_or_default();
    let _ = writeln!(out, "  exits      {} ({} port in, {} port out, {} MMIO accesses performed, {} hlt); the IO-APIC sent {}, {} of them levels, {} ended by EOI",
                     counts.exits, counts.port_in, counts.port_out, counts.mmio_done, counts.hlt,
                     stats.sent, stats.level, stats.eois);
    let mut says = String::new();
    let _ = match stopped.stop {
        GuestStop::Halted { rip } => write!(says, "cpu 0 halted with interrupts off at {:#x}, cpu 1 never started", rip),
        ref other => describe_stop(other, &mut says),
    };
    let _ = writeln!(out, "  stopped    {}", says);

    match check_ioapic(guest.memory(), &stopped, &stats) {
        Ok(checked) => {
            let _ = writeln!(out, "  checked    {}", checked);
            let _ = writeln!(out, "hv: guest {} ok", IOAPIC_NAME);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", IOAPIC_NAME, why);
            let _ = writeln!(out, "  cpu {} as it stopped:", stopped.cpu);
            let _ = out.write_str(&stopped.dump);
            false
        }
    }
}

fn check_ioapic(m: &GuestMemory, stopped: &Stopped, stats: &crate::devices::ioapic::Stats)
    -> core::result::Result<String, String>
{
    if !matches!(stopped.stop, GuestStop::Halted { .. }) {
        return Err(String::from("it did not run to its halt"));
    }
    let word = |gpa: u64| m.read_obj::<u32>(gpa).map_err(|e| alloc::format!("guest memory at {:#x}: {}", gpa, e));
    if word(IOAPIC_DONE)? != IOAPIC_MARK {
        return Err(String::from("it never reached its end"));
    }
    let version = word(IOAPIC_VERSION)?;
    if version != IOAPIC_VERSION_READ {
        return Err(alloc::format!("the version register reads {:#x}, not {:#x}", version, IOAPIC_VERSION_READ));
    }
    let (edges, levels) = (word(IOAPIC_EDGES)?, word(IOAPIC_LEVELS)?);
    if edges != IOAPIC_EDGES_WANTED || levels != IOAPIC_LEVELS_WANTED {
        return Err(alloc::format!("it took {} edge interrupts and {} level ones, not {} and {}",
                                  edges, levels, IOAPIC_EDGES_WANTED, IOAPIC_LEVELS_WANTED));
    }
    let (in_service, after) = (word(IOAPIC_IN_SERVICE)?, word(IOAPIC_AFTER)?);
    if in_service != IOAPIC_LEVEL_ENTRY | IOAPIC_REMOTE_IRR || after != IOAPIC_LEVEL_ENTRY {
        return Err(alloc::format!("pin 4's entry read {:#x} in service and {:#x} after, not {:#x} and {:#x}",
                                  in_service, after, IOAPIC_LEVEL_ENTRY | IOAPIC_REMOTE_IRR, IOAPIC_LEVEL_ENTRY));
    }
    let wanted = u64::from(IOAPIC_EDGES_WANTED + IOAPIC_LEVELS_WANTED);
    let levels = u64::from(IOAPIC_LEVELS_WANTED);
    if stats.sent != wanted || stats.level != levels || stats.eois != levels || stats.dropped != 0 {
        return Err(alloc::format!("the IO-APIC sent {} ({} levels), ended {} and dropped {}, not {} ({}), {} and 0",
                                  stats.sent, stats.level, stats.eois, stats.dropped, wanted, levels, levels));
    }
    Ok(String::from("version 0x20's 24 pins; two edges taken, none while masked; three levels, each sent again \
                     at its EOI while the line stayed up, its remote IRR set in service and clear after"))
}

/* ---- debug: a guest's own #DB given back to it ---- */

const DEBUG_NAME: &str = "debug";
const DEBUG_ABOUT: &str = "a single step and an int1, each a #DB of the guest's own: intercepted, and given back \
                           to its handler by the run loop rather than stopping it";

/* A Linux guest's CPU, one, in long mode with an IDT of its own: TF set by
 * POPFQ, a NOP single-stepped -- a #DB after it, DR6.BS set -- and an ICEBP
 * (`int1`) -- a #DB after it too, no DR6 bit of its own. Its handler counts
 * each and keeps DR6 and where it returns to, clears TF in the frame and DR6
 * for the next, and the guest leaves its mark and halts. Assembled from
 * this source with clang, the offsets in the comments its listing's. */
const DEBUG_CODE: &[u8] = &[
    0x48, 0x8D, 0x05, 0x52, 0x00, 0x00, 0x00,               // 00: lea rax, [rip + db] ; gate 1, #DB
    0xBF, 0x10, 0xA0, 0x00, 0x00,                           // 07: mov edi, 0xA010
    0xE8, 0x22, 0x00, 0x00, 0x00,                           // 0c: call gate
    0x0F, 0x01, 0x1D, 0x83, 0x00, 0x00, 0x00,               // 11: lidt [rip + idtr]
    0x9C,                                                   // 18: pushfq
    0x48, 0x81, 0x0C, 0x24, 0x00, 0x01, 0x00, 0x00,         // 19: or qword [rsp], 0x100 ; TF
    0x9D,                                                   // 21: popfq ; set: the trap comes after the next
    0x90,                                                   // 22: nop ; single-stepped: a #DB after it
    0xF1,                                                   // 23: int1 ; a #DB, as a trap, after it
    0xC7, 0x04, 0x25, 0x48, 0x70, 0x00, 0x00,               // 24: mov dword [0x7048], 'DBUG'
    0x44, 0x42, 0x55, 0x47,
    0xFA,                                                   // 2f: cli
    0xF4,                                                   // 30: .dead: hlt
    0xEB, 0xFD,                                             // 31: jmp .dead
    0x66, 0x89, 0x07,                                       // 33: gate: mov word [rdi], ax ; an interrupt gate for rax
    0x66, 0xC7, 0x47, 0x02, 0x08, 0x00,                     // 36: mov word [rdi + 2], 0x08
    0x66, 0xC7, 0x47, 0x04, 0x00, 0x8E,                     // 3c: mov word [rdi + 4], 0x8E00
    0x48, 0xC1, 0xE8, 0x10,                                 // 42: shr rax, 16
    0x66, 0x89, 0x47, 0x06,                                 // 46: mov word [rdi + 6], ax
    0x48, 0xC1, 0xE8, 0x10,                                 // 4a: shr rax, 16
    0x89, 0x47, 0x08,                                       // 4e: mov dword [rdi + 8], eax
    0xC7, 0x47, 0x0C, 0x00, 0x00, 0x00, 0x00,               // 51: mov dword [rdi + 12], 0
    0xC3,                                                   // 58: ret
    0x50,                                                   // 59: db: push rax
    0x53,                                                   // 5a: push rbx
    0x8B, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,               // 5b: mov eax, dword [0x7000] ; how many so far
    0x83, 0xF8, 0x04,                                       // 62: cmp eax, 4
    0x73, 0x18,                                             // 65: jae .full
    0x0F, 0x21, 0xF3,                                       // 67: mov rbx, dr6
    0x48, 0x89, 0x1C, 0xC5, 0x08, 0x70, 0x00, 0x00,         // 6a: mov qword [0x7008 + rax*8], rbx
    0x48, 0x8B, 0x5C, 0x24, 0x10,                           // 72: mov rbx, qword [rsp + 16] ; where it returns to
    0x48, 0x89, 0x1C, 0xC5, 0x28, 0x70, 0x00, 0x00,         // 77: mov qword [0x7028 + rax*8], rbx
    0xFF, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,               // 7f: .full: inc dword [0x7000]
    0x48, 0x81, 0x64, 0x24, 0x20, 0xFF, 0xFE, 0xFF, 0xFF,   // 86: and qword [rsp + 32], ~0x100 ; TF off in the frame
    0xBB, 0xF0, 0x0F, 0xFF, 0xFF,                           // 8f: mov ebx, 0xFFFF0FF0
    0x0F, 0x23, 0xF3,                                       // 94: mov dr6, rbx ; for the next
    0x5B,                                                   // 97: pop rbx
    0x58,                                                   // 98: pop rax
    0x48, 0xCF,                                             // 99: iretq
    0x1F, 0x00,                                             // 9b: idtr: dw 0x1F ; two gates
    0x00, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,         // dq 0xA000
];

/* What the guest leaves at `RESULTS`: the #DBs its handler took at +0, the
 * DR6 each read at +8 on, where each returned to at +0x28 on, and 'DBUG'
 * at +0x48 once done. */
const DEBUG_COUNT: u64 = RESULTS;
const DEBUG_DR6: u64 = RESULTS + 0x8;
const DEBUG_RETURN: u64 = RESULTS + 0x28;
const DEBUG_DONE: u64 = RESULTS + 0x48;
const DEBUG_MARK: u32 = u32::from_le_bytes(*b"DBUG");
/// Where each #DB returns to: after the stepped NOP, and after the ICEBP.
const DEBUG_STEP_RETURN: u64 = ENTRY + 0x23;
const DEBUG_ICEBP_RETURN: u64 = ENTRY + 0x24;
/// DR6's single-step bit, and its breakpoint bits.
const DR6_BS: u64 = 1 << 14;
const DR6_B0_3: u64 = 0xF;
const DEBUG_BUDGET_MS: u64 = 10_000;

/// One CPU's machine for a guest run through the run loop a Linux guest's
/// go through: `code` at `ENTRY`, in long mode, with the GDT, the TSS and
/// the page table every guest here has, and no IDT until it loads its own.
fn one_cpu_board(m: &GuestMemory, bsp: &mut GuestCpu, code: &[u8]) -> Result<()> {
    let tss_low = TSS_LIMIT | (TSS & 0xFF_FFFF) << 16 | TSS_BUSY_PRESENT << 40 | (TSS >> 24 & 0xFF) << 56;
    let tss_high = TSS >> 32;
    for (i, entry) in [0, GDT_CODE64, GDT_DATA, tss_low, tss_high].iter().enumerate() {
        m.write_obj(GDT + i as u64 * 8, entry)?;
    }
    m.write_obj(PML4, &(PDPT | PTE_P_W))?;
    m.write_obj(PDPT, &(PD_LOW | PTE_P_W))?;
    m.write_obj(PD_LOW, &(PTE_P_W | PTE_LARGE))?;
    m.write(ENTRY, code)?;
    bsp.backend_mut().long_mode(&LongMode {
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

/// Run the debug guest: one CPU, through the run loop a Linux guest's go
/// through, where a #DB of the guest's own is given back.
fn run_debug(machine: &Arc<Machine>, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", DEBUG_NAME, DEBUG_ABOUT);
    let fail = |out: &mut dyn Write, why: &str| {
        let _ = writeln!(out, "hv: guest {} FAILED -- {}", DEBUG_NAME, why);
        false
    };
    let Some(doorbells) = Doorbells::new(1) else { return fail(out, "out of memory") };
    let (guest, mut cpus) = match LinuxGuest::new(machine, MEMORY, 1, Arc::new(doorbells), false, false) {
        Ok(made) => made,
        Err(e) => return fail(out, &alloc::format!("could not be made: {}", e)),
    };
    let Some(mut bsp) = cpus.pop() else { return fail(out, "no CPU") };
    if let Err(e) = one_cpu_board(guest.memory(), &mut bsp, DEBUG_CODE) {
        return fail(out, &alloc::format!("could not be made: {}", e));
    }
    let vmx = matches!(machine.ext(), Ok(hvarch::Ext::Vmx));
    let start = time::boot_time_ns();
    let deadline = start.saturating_add(DEBUG_BUDGET_MS * kcore::consts::NS_PER_MS);
    let counts = guest.run(&mut bsp, machine, deadline, &Quiet);
    drop(bsp);
    let ns = time::boot_time_ns().saturating_sub(start);
    let stopped = guest.take_stopped()
        .unwrap_or(Stopped { stop: GuestStop::Requested, cpu: 0, dump: String::new() });
    if let GuestStop::Refused(Refusal::NotOn(cpu)) = stopped.stop {
        let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first", DEBUG_NAME, cpu);
        return false;
    }
    let host = guest.cpu_states().first().and_then(|&(_, h)| h);
    match host {
        Some(h) => { let _ = writeln!(out, "  ran on     cpu {}, {} us", h, ns / kcore::consts::NS_PER_US); }
        None => { let _ = writeln!(out, "  ran on     no cpu, {} us", ns / kcore::consts::NS_PER_US); }
    }
    let _ = writeln!(out, "  exits      {}, {} exceptions given back", counts.exits, counts.reflected);
    let mut says = String::new();
    let _ = describe_stop(&stopped.stop, &mut says);
    let _ = writeln!(out, "  stopped    {}", says);

    match check_debug(guest.memory(), &stopped, vmx) {
        Ok(checked) => {
            let _ = writeln!(out, "  checked    {}", checked);
            let _ = writeln!(out, "hv: guest {} ok", DEBUG_NAME);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", DEBUG_NAME, why);
            let _ = writeln!(out, "  cpu {} as it stopped:", stopped.cpu);
            let _ = out.write_str(&stopped.dump);
            false
        }
    }
}

/// Under AMD-V: both #DBs reached the guest's handler, the first with DR6.BS
/// and after the stepped NOP, the second with no DR6 bit and after the ICEBP,
/// and the guest ran on to its halt. Under VT-x, which gives no #DB back yet:
/// the single step stopped the guest, after the NOP, its handler never run.
fn check_debug(m: &GuestMemory, stopped: &Stopped, vmx: bool) -> core::result::Result<String, String> {
    let qword = |gpa| m.read_obj::<u64>(gpa).map_err(|e| alloc::format!("its results cannot be read: {}", e));
    let count = m.read_obj::<u32>(DEBUG_COUNT).map_err(|e| alloc::format!("its results cannot be read: {}", e))?;
    if vmx {
        return match stopped.stop {
            GuestStop::Exception { vector: 1, rip } if rip == DEBUG_STEP_RETURN && count == 0 => {
                Ok(String::from("VT-x gives no #DB back yet: the single step stopped it, after the NOP, as it \
                                 should until it does"))
            }
            ref other => {
                let mut says = String::new();
                let _ = describe_stop(other, &mut says);
                Err(alloc::format!("under VT-x it should have stopped at its single step; it {}, its handler run {} times",
                                   says, count))
            }
        };
    }
    if !matches!(stopped.stop, GuestStop::Halted { .. }) {
        return Err(alloc::format!("it did not run to its halt: its handler ran {} times", count));
    }
    let done = m.read_obj::<u32>(DEBUG_DONE).map_err(|e| alloc::format!("its results cannot be read: {}", e))?;
    if done != DEBUG_MARK || count != 2 {
        return Err(alloc::format!("its handler ran {} times, not 2, and its mark is {:#x}", count, done));
    }
    let (step_dr6, step_at) = (qword(DEBUG_DR6)?, qword(DEBUG_RETURN)?);
    if step_dr6 & DR6_BS == 0 || step_at != DEBUG_STEP_RETURN {
        return Err(alloc::format!("the single step's #DB read DR6 {:#x} and returned to {:#x}: wanted BS, and {:#x}",
                                  step_dr6, step_at, DEBUG_STEP_RETURN));
    }
    let (icebp_dr6, icebp_at) = (qword(DEBUG_DR6 + 8)?, qword(DEBUG_RETURN + 8)?);
    if icebp_dr6 & (DR6_BS | DR6_B0_3) != 0 || icebp_at != DEBUG_ICEBP_RETURN {
        return Err(alloc::format!("the ICEBP's #DB read DR6 {:#x} and returned to {:#x}: wanted no step or breakpoint \
                                   bit, and {:#x}", icebp_dr6, icebp_at, DEBUG_ICEBP_RETURN));
    }
    Ok(String::from("both #DBs its own handler's: the single step's with DR6.BS, after the NOP; the ICEBP's with \
                     no bit, after it -- and it ran on to its halt"))
}

/* ---- dr: a guest's DR0-DR3 its own ---- */

const DR_NAME: &str = "dr";
const DR_ABOUT: &str = "two VMs on one CPU, one after the other: each reads DR0-DR3 as it starts, writes a pattern \
                        of its own into them and reads it back, and sets a breakpoint on an instruction of its own \
                        -- neither reads the other's, and each one's breakpoint fires";

/* A Linux guest's CPU, one, in long mode with an IDT of its own, whose #DB
 * handler counts each, keeps DR6 and where it returns to, and turns the
 * breakpoints off -- an execution breakpoint is a fault, and its
 * instruction would raise it again. It reads DR0-DR3 as it starts; writes a
 * pattern of its own into them -- the host's word at +0x100 and the three
 * after it -- through four registers, and reads it back through a fifth;
 * points DR0 at its breakpoint, the host's word at +0x108 -- one of the two
 * `ret`s, each VM's its own -- arms it in DR7 for execution and calls it --
 * a #DB at the `ret`, before it runs -- then reads DR0 once more, leaves its
 * mark and halts. An execution breakpoint rather than a data one: QEMU
 * 10.2's TCG never gets past a store a data breakpoint watches in a nested
 * guest -- it retries the store for good, takes no interrupt, and the host
 * has lost the CPU -- with these registers the guest's or the CPU's.
 * Assembled from this source with clang, the offsets in the comments its
 * listing's. */
const DR_CODE: &[u8] = &[
    0x48, 0x8D, 0x05, 0xE5, 0x00, 0x00, 0x00,               // 00: start: lea rax, [rip + db] ; gate 1, #DB
    0xBF, 0x10, 0xA0, 0x00, 0x00,                           // 07: mov edi, 0xA010
    0xE8, 0xB5, 0x00, 0x00, 0x00,                           // 0c: call gate
    0x0F, 0x01, 0x1D, 0x0A, 0x01, 0x00, 0x00,               // 11: lidt [rip + idtr]
    0x0F, 0x21, 0xC0,                                       // 18: mov rax, dr0 ; DR0-DR3 as the CPU starts: 0
    0x48, 0x89, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,         // 1b: mov [0x7000], rax
    0x0F, 0x21, 0xC8,                                       // 23: mov rax, dr1
    0x48, 0x89, 0x04, 0x25, 0x08, 0x70, 0x00, 0x00,         // 26: mov [0x7008], rax
    0x0F, 0x21, 0xD0,                                       // 2e: mov rax, dr2
    0x48, 0x89, 0x04, 0x25, 0x10, 0x70, 0x00, 0x00,         // 31: mov [0x7010], rax
    0x0F, 0x21, 0xD8,                                       // 39: mov rax, dr3
    0x48, 0x89, 0x04, 0x25, 0x18, 0x70, 0x00, 0x00,         // 3c: mov [0x7018], rax
    0x48, 0x8B, 0x1C, 0x25, 0x00, 0x71, 0x00, 0x00,         // 44: mov rbx, [0x7100] ; a pattern of its own into them
    0x0F, 0x23, 0xC3,                                       // 4c: mov dr0, rbx
    0x48, 0x8D, 0x4B, 0x01,                                 // 4f: lea rcx, [rbx + 1]
    0x0F, 0x23, 0xC9,                                       // 53: mov dr1, rcx
    0x48, 0x8D, 0x53, 0x02,                                 // 56: lea rdx, [rbx + 2]
    0x0F, 0x23, 0xD2,                                       // 5a: mov dr2, rdx
    0x4C, 0x8D, 0x4B, 0x03,                                 // 5d: lea r9, [rbx + 3]
    0x41, 0x0F, 0x23, 0xD9,                                 // 61: mov dr3, r9
    0x41, 0x0F, 0x21, 0xC2,                                 // 65: mov r10, dr0 ; and back, through R10
    0x4C, 0x89, 0x14, 0x25, 0x20, 0x70, 0x00, 0x00,         // 69: mov [0x7020], r10
    0x41, 0x0F, 0x21, 0xCA,                                 // 71: mov r10, dr1
    0x4C, 0x89, 0x14, 0x25, 0x28, 0x70, 0x00, 0x00,         // 75: mov [0x7028], r10
    0x41, 0x0F, 0x21, 0xD2,                                 // 7d: mov r10, dr2
    0x4C, 0x89, 0x14, 0x25, 0x30, 0x70, 0x00, 0x00,         // 81: mov [0x7030], r10
    0x41, 0x0F, 0x21, 0xDA,                                 // 89: mov r10, dr3
    0x4C, 0x89, 0x14, 0x25, 0x38, 0x70, 0x00, 0x00,         // 8d: mov [0x7038], r10
    0x48, 0x8B, 0x3C, 0x25, 0x08, 0x71, 0x00, 0x00,         // 95: mov rdi, [0x7108] ; its breakpoint: one of the two rets
    0x0F, 0x23, 0xC7,                                       // 9d: mov dr0, rdi
    0xB8, 0x01, 0x04, 0x00, 0x00,                           // a0: mov eax, 0x401 ; DR7: L0, on execution
    0x0F, 0x23, 0xF8,                                       // a5: mov dr7, rax
    0xFF, 0xD7,                                             // a8: call rdi ; a #DB at the ret, before it runs
    0x0F, 0x21, 0xC0,                                       // aa: mov rax, dr0
    0x48, 0x89, 0x04, 0x25, 0x58, 0x70, 0x00, 0x00,         // ad: mov [0x7058], rax
    0xC7, 0x04, 0x25, 0x60, 0x70, 0x00, 0x00,               // b5: mov dword [0x7060], 'DREG'
    0x44, 0x52, 0x45, 0x47,
    0xFA,                                                   // c0: cli
    0xF4,                                                   // c1: dead: hlt
    0xEB, 0xFD,                                             // c2: jmp dead
    0xC3,                                                   // c4: ret_a: ret ; vm A's breakpoint
    0xC3,                                                   // c5: ret_b: ret ; vm B's
    0x66, 0x89, 0x07,                                       // c6: gate: mov word [rdi], ax
    0x66, 0xC7, 0x47, 0x02, 0x08, 0x00,                     // c9: mov word [rdi + 2], 0x08
    0x66, 0xC7, 0x47, 0x04, 0x00, 0x8E,                     // cf: mov word [rdi + 4], 0x8E00
    0x48, 0xC1, 0xE8, 0x10,                                 // d5: shr rax, 16
    0x66, 0x89, 0x47, 0x06,                                 // d9: mov word [rdi + 6], ax
    0x48, 0xC1, 0xE8, 0x10,                                 // dd: shr rax, 16
    0x89, 0x47, 0x08,                                       // e1: mov dword [rdi + 8], eax
    0xC7, 0x47, 0x0C, 0x00, 0x00, 0x00, 0x00,               // e4: mov dword [rdi + 12], 0
    0xC3,                                                   // eb: ret
    0x50,                                                   // ec: db: push rax
    0x53,                                                   // ed: push rbx
    0x48, 0xFF, 0x04, 0x25, 0x40, 0x70, 0x00, 0x00,         // ee: inc qword [0x7040] ; how many
    0x0F, 0x21, 0xF3,                                       // f6: mov rbx, dr6
    0x48, 0x89, 0x1C, 0x25, 0x48, 0x70, 0x00, 0x00,         // f9: mov [0x7048], rbx
    0x48, 0x8B, 0x5C, 0x24, 0x10,                           // 101: mov rbx, [rsp + 16] ; where it returns to: the ret
    0x48, 0x89, 0x1C, 0x25, 0x50, 0x70, 0x00, 0x00,         // 106: mov [0x7050], rbx
    0xBB, 0xF0, 0x0F, 0xFF, 0xFF,                           // 10e: mov ebx, 0xFFFF0FF0
    0x0F, 0x23, 0xF3,                                       // 113: mov dr6, rbx ; for the next
    0xBB, 0x00, 0x04, 0x00, 0x00,                           // 116: mov ebx, 0x400
    0x0F, 0x23, 0xFB,                                       // 11b: mov dr7, rbx ; off, or the ret faults again
    0x5B,                                                   // 11e: pop rbx
    0x58,                                                   // 11f: pop rax
    0x48, 0xCF,                                             // 120: iretq
    0x1F, 0x00,                                             // 122: idtr: dw 0x1F ; two gates
    0x00, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,         // dq 0xA000
];

/* What the guest leaves at `RESULTS`: DR0-DR3 as it started at +0, as it
 * read them back at +0x20, the #DBs its handler took at +0x40, the DR6 and
 * where the last returned to at +0x48 and +0x50, DR0 at the end at +0x58,
 * and 'DREG' at +0x60 once done; and what the host gives it, at +0x100 and
 * +0x108. */
const DR_FIRST: u64 = RESULTS;
const DR_BACK: u64 = RESULTS + 0x20;
const DR_COUNT: u64 = RESULTS + 0x40;
const DR_DR6: u64 = RESULTS + 0x48;
const DR_RETURN: u64 = RESULTS + 0x50;
const DR_LAST: u64 = RESULTS + 0x58;
const DR_DONE: u64 = RESULTS + 0x60;
const DR_MARK: u32 = u32::from_le_bytes(*b"DREG");
const DR_PATTERN: u64 = RESULTS + 0x100;
const DR_BREAK: u64 = RESULTS + 0x108;
/// The two `ret`s, one each VM's breakpoint: in a place of its own, so that
/// a breakpoint left at the other's would not fire.
const DR_RET_A: u64 = ENTRY + 0xC4;
const DR_RET_B: u64 = ENTRY + 0xC5;
/// DR6's bit for a breakpoint of DR0's.
const DR6_B0: u64 = 1 << 0;
/// The `mov`s of DR0-DR3 each VM makes: four reads, four writes, the four
/// read back, DR0 pointed at its breakpoint and read at the end.
const DR_MOVES: u64 = 14;
/// The two VMs: the first word of each one's pattern -- a kernel's
/// address, nothing a CPU out of reset holds -- and its breakpoint.
const DR_VMS: [(u64, u64); 2] = [(0xFFFF_FFFF_8100_0000, DR_RET_A), (0xFFFF_FFFF_8200_0000, DR_RET_B)];
const DR_BUDGET_MS: u64 = 10_000;

/// What one VM of the dr guest did: where it ran, what the run loop
/// counted, how it stopped, and what it left.
struct DrRun {
    cpu: Option<u32>,
    us: u64,
    counts: Counts,
    stopped: Stopped,
    /// Its VMCB asked the CPU to stop it at every read and write of
    /// DR0-DR3.
    asked: bool,
    first: [u64; 4],
    back: [u64; 4],
    count: u64,
    dr6: u64,
    returned: u64,
    last: u64,
    done: u32,
}

/// Run one VM of the dr guest, with `pattern` and the breakpoint `at` its
/// own, on the CPU this runs on.
fn run_dr_vm(machine: &Arc<Machine>, pattern: u64, at: u64) -> core::result::Result<DrRun, String> {
    let made = |e: hvarch::Error| alloc::format!("could not be made: {}", e);
    let Some(doorbells) = Doorbells::new(1) else { return Err(String::from("out of memory")) };
    let (guest, mut cpus) = LinuxGuest::new(machine, MEMORY, 1, Arc::new(doorbells), false, false).map_err(made)?;
    let Some(mut bsp) = cpus.pop() else { return Err(String::from("no CPU")) };
    let m = guest.memory();
    one_cpu_board(m, &mut bsp, DR_CODE)
        .and_then(|_| m.write_obj(DR_PATTERN, &pattern))
        .and_then(|_| m.write_obj(DR_BREAK, &at))
        .map_err(made)?;
    let start = time::boot_time_ns();
    let deadline = start.saturating_add(DR_BUDGET_MS * kcore::consts::NS_PER_MS);
    let counts = guest.run(&mut bsp, machine, deadline, &Quiet);
    use hvarch::x86::svm::vmcb::intercept::dr::{READ_DR0_3, WRITE_DR0_3};
    let asked = matches!(&*bsp.backend_mut(), Backend::Svm(v)
        if v.control().intercept_dr & (READ_DR0_3 | WRITE_DR0_3) == READ_DR0_3 | WRITE_DR0_3);
    drop(bsp);
    let us = time::boot_time_ns().saturating_sub(start) / kcore::consts::NS_PER_US;
    let stopped = guest.take_stopped()
        .unwrap_or(Stopped { stop: GuestStop::Requested, cpu: 0, dump: String::new() });
    let cpu = guest.cpu_states().first().and_then(|&(_, h)| h);
    let unread = |e: hvarch::Error| alloc::format!("its results cannot be read: {}", e);
    let qword = |at: u64| m.read_obj::<u64>(at).map_err(unread);
    let four = |at: u64| -> core::result::Result<[u64; 4], String> {
        Ok([qword(at)?, qword(at + 8)?, qword(at + 16)?, qword(at + 24)?])
    };
    Ok(DrRun {
        cpu,
        us,
        counts,
        stopped,
        asked,
        first: four(DR_FIRST)?,
        back: four(DR_BACK)?,
        count: qword(DR_COUNT)?,
        dr6: qword(DR_DR6)?,
        returned: qword(DR_RETURN)?,
        last: qword(DR_LAST)?,
        done: m.read_obj::<u32>(DR_DONE).map_err(unread)?,
    })
}

/// Run the dr guest: two VMs of one CPU each, one after the other on the
/// CPU this runs on, through the run loop a Linux guest's go through.
fn run_dr(machine: &Arc<Machine>, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", DR_NAME, DR_ABOUT);
    let vmx = matches!(machine.ext(), Ok(hvarch::Ext::Vmx));
    let mut runs = alloc::vec::Vec::new();
    for &(pattern, at) in DR_VMS.iter() {
        match run_dr_vm(machine, pattern, at) {
            Ok(run) => {
                if let GuestStop::Refused(Refusal::NotOn(cpu)) = run.stopped.stop {
                    let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first",
                                     DR_NAME, cpu);
                    return false;
                }
                runs.push(run);
            }
            Err(why) => {
                let _ = writeln!(out, "hv: guest {} FAILED -- {}", DR_NAME, why);
                return false;
            }
        }
    }
    let [a, b] = runs.as_slice() else {
        let _ = writeln!(out, "hv: guest {} FAILED -- {} VMs run, not two", DR_NAME, runs.len());
        return false;
    };
    let on = |c: Option<u32>| c.map_or(String::from("no cpu"), |c| alloc::format!("cpu {}", c));
    let _ = writeln!(out, "  ran on     {}, {} us -- vm A; vm B on {}, {} us", on(a.cpu), a.us, on(b.cpu), b.us);
    let _ = writeln!(out, "  exits      {} and {}, {} and {} of DR0-DR3 performed, {} and {} exceptions given back",
                     a.counts.exits, b.counts.exits, a.counts.dr, b.counts.dr, a.counts.reflected,
                     b.counts.reflected);
    let (mut says_a, mut says_b) = (String::new(), String::new());
    let _ = describe_stop(&a.stopped.stop, &mut says_a);
    let _ = describe_stop(&b.stopped.stop, &mut says_b);
    let _ = writeln!(out, "  stopped    vm A {}; vm B {}", says_a, says_b);

    match check_dr(a, b, vmx) {
        Ok(checked) => {
            let _ = writeln!(out, "  checked    {}", checked);
            let _ = writeln!(out, "hv: guest {} ok", DR_NAME);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", DR_NAME, why);
            for (name, run) in [("A", a), ("B", b)] {
                let _ = writeln!(out, "  vm {}'s cpu {} as it stopped:", name, run.stopped.cpu);
                let _ = out.write_str(&run.stopped.dump);
            }
            false
        }
    }
}

/// Under AMD-V: both VMs ran on one CPU -- where the first's addresses would
/// be the second's to read, were the CPU's DR0-DR3 the guests' -- each to
/// its halt; each read 0 in all four as it started, the second where the
/// first had left its own; each read back the pattern it wrote; each one's
/// breakpoint, at an address of its own, fired once -- DR6.B0, at its `ret`
/// -- which only DR0 made its own before its entry can do; and each VMCB
/// asked to stop every `mov` of DR0-DR3, every one performed by the host.
/// Under VT-x, which does not keep a guest's four apart yet: what the second
/// read is said, and nothing is held to it.
fn check_dr(a: &DrRun, b: &DrRun, vmx: bool) -> core::result::Result<String, String> {
    if vmx {
        return Ok(alloc::format!("VT-x does not keep a guest's DR0-DR3 apart yet: vm B read {} as it started",
                                 dr_four(&b.first)));
    }
    if a.cpu.is_none() || a.cpu != b.cpu {
        return Err(alloc::format!("the VMs ran on {:?} and {:?}: not one CPU, where the first's registers would be \
                                   the second's", a.cpu, b.cpu));
    }
    /* Each property for both VMs before the next: what each was to see --
     * 0 where the other had left its own -- before how the host made it so,
     * so that a world switch which does not keep them apart is told by that
     * and not by its mechanism. */
    let runs = [("A", a, DR_VMS[0]), ("B", b, DR_VMS[1])];
    for (name, run, _) in runs {
        if !matches!(run.stopped.stop, GuestStop::Halted { .. }) || run.done != DR_MARK {
            return Err(alloc::format!("vm {} did not run to its halt: its mark is {:#x}", name, run.done));
        }
    }
    for (name, run, _) in runs {
        if run.first != [0; 4] {
            return Err(alloc::format!("vm {} read DR0-DR3 {} as it started, not 0", name, dr_four(&run.first)));
        }
    }
    for (name, run, (pattern, _)) in runs {
        let want = [pattern, pattern + 1, pattern + 2, pattern + 3];
        if run.back != want {
            return Err(alloc::format!("vm {} read back {}, not the {} it wrote", name, dr_four(&run.back),
                                      dr_four(&want)));
        }
    }
    for (name, run, (_, at)) in runs {
        if run.count != 1 || run.dr6 & DR6_B0 == 0 || run.returned != at || run.last != at
            || run.counts.reflected != 1
        {
            return Err(alloc::format!("vm {}'s breakpoint at {:#x}: its handler ran {} times ({} given back), read \
                                       DR6 {:#x} and returned to {:#x}, and DR0 was {:#x} at the end: wanted once, \
                                       B0, and the breakpoint's address both", name, at, run.count,
                                      run.counts.reflected, run.dr6, run.returned, run.last));
        }
    }
    for (name, run, _) in runs {
        if !run.asked || run.counts.dr != DR_MOVES || run.counts.dr_refused != 0 {
            return Err(alloc::format!("vm {}'s VMCB asked to stop its movs of DR0-DR3: {}; {} performed and {} not \
                                       made, where it made {}", name, run.asked, run.counts.dr,
                                      run.counts.dr_refused, DR_MOVES));
        }
    }
    Ok(alloc::format!("vm B read DR0-DR3 as 0 where vm A had left its own, each read back the pattern it wrote -- \
                       {} movs each, performed by the host -- and each one's breakpoint fired at its own address: \
                       DR6.B0, at its ret", DR_MOVES))
}

/// Four registers' values, on one line.
fn dr_four(r: &[u64; 4]) -> String {
    alloc::format!("{:#x} {:#x} {:#x} {:#x}", r[0], r[1], r[2], r[3])
}

/* ---- cr4: the guest's CR4 its own to read, MCE the host's to keep ---- */

const CR4_NAME: &str = "cr4";
const CR4_ABOUT: &str = "a guest's CR4: MCE cleared in what it reads back and kept in the CPU's, and a bit its CPUID \
                         does not offer, PAE cleared in long mode and a reserved bit each a #GP";

/* A Linux guest's CPU, one, in long mode with an IDT of its own, whose #GP
 * handler counts each, keeps its error code and steps over the 3-byte `mov`
 * that raised it. It reads CR4; writes PAE alone, MCE cleared; then PAE and
 * SMEP, which its CPUID does not offer; then 0, PAE cleared in long mode;
 * then PAE and PGE through R9, read back through R13; then a bit above 31 --
 * reading CR4 after each -- and leaves its mark and halts. Assembled from
 * this source with clang, the offsets in the comments its listing's. */
const CR4_CODE: &[u8] = &[
    0x48, 0x8D, 0x05, 0xB5, 0x00, 0x00, 0x00,               // 00: lea rax, [rip + gp] ; gate 13, #GP
    0xBF, 0xD0, 0xA0, 0x00, 0x00,                           // 07: mov edi, 0xA0D0
    0xE8, 0x85, 0x00, 0x00, 0x00,                           // 0c: call gate
    0x0F, 0x01, 0x1D, 0xC4, 0x00, 0x00, 0x00,               // 11: lidt [rip + idtr]
    0x0F, 0x20, 0xE0,                                       // 18: mov rax, cr4
    0x48, 0x89, 0x04, 0x25, 0x08, 0x70, 0x00, 0x00,         // 1b: mov qword [0x7008], rax ; as it started
    0xB8, 0x20, 0x00, 0x00, 0x00,                           // 23: mov eax, 0x20 ; PAE alone: MCE cleared
    0x0F, 0x22, 0xE0,                                       // 28: mov cr4, rax
    0x0F, 0x20, 0xE0,                                       // 2b: mov rax, cr4
    0x48, 0x89, 0x04, 0x25, 0x10, 0x70, 0x00, 0x00,         // 2e: mov qword [0x7010], rax
    0xB8, 0x20, 0x00, 0x10, 0x00,                           // 36: mov eax, 0x100020 ; and SMEP: #GP
    0x0F, 0x22, 0xE0,                                       // 3b: mov cr4, rax
    0x0F, 0x20, 0xE0,                                       // 3e: mov rax, cr4
    0x48, 0x89, 0x04, 0x25, 0x18, 0x70, 0x00, 0x00,         // 41: mov qword [0x7018], rax
    0x31, 0xC0,                                             // 49: xor eax, eax ; PAE cleared in long mode: #GP
    0x0F, 0x22, 0xE0,                                       // 4b: mov cr4, rax
    0x0F, 0x20, 0xE0,                                       // 4e: mov rax, cr4
    0x48, 0x89, 0x04, 0x25, 0x20, 0x70, 0x00, 0x00,         // 51: mov qword [0x7020], rax
    0x41, 0xB9, 0xA0, 0x00, 0x00, 0x00,                     // 59: mov r9d, 0xA0 ; PAE and PGE
    0x41, 0x0F, 0x22, 0xE1,                                 // 5f: mov cr4, r9
    0x41, 0x0F, 0x20, 0xE5,                                 // 63: mov r13, cr4
    0x4C, 0x89, 0x2C, 0x25, 0x28, 0x70, 0x00, 0x00,         // 67: mov qword [0x7028], r13
    0x48, 0xB8, 0xA0, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // 6f: mov rax, 0x1000000A0 ; bit 32: #GP
    0x0F, 0x22, 0xE0,                                       // 79: mov cr4, rax
    0x0F, 0x20, 0xE0,                                       // 7c: mov rax, cr4
    0x48, 0x89, 0x04, 0x25, 0x30, 0x70, 0x00, 0x00,         // 7f: mov qword [0x7030], rax
    0xC7, 0x04, 0x25, 0x38, 0x70, 0x00, 0x00,               // 87: mov dword [0x7038], 'CR4!'
    0x43, 0x52, 0x34, 0x21,
    0xFA,                                                   // 92: cli
    0xF4,                                                   // 93: .dead: hlt
    0xEB, 0xFD,                                             // 94: jmp .dead
    0x66, 0x89, 0x07,                                       // 96: gate: mov word [rdi], ax ; an interrupt gate for rax
    0x66, 0xC7, 0x47, 0x02, 0x08, 0x00,                     // 99: mov word [rdi + 2], 0x08
    0x66, 0xC7, 0x47, 0x04, 0x00, 0x8E,                     // 9f: mov word [rdi + 4], 0x8E00
    0x48, 0xC1, 0xE8, 0x10,                                 // a5: shr rax, 16
    0x66, 0x89, 0x47, 0x06,                                 // a9: mov word [rdi + 6], ax
    0x48, 0xC1, 0xE8, 0x10,                                 // ad: shr rax, 16
    0x89, 0x47, 0x08,                                       // b1: mov dword [rdi + 8], eax
    0xC7, 0x47, 0x0C, 0x00, 0x00, 0x00, 0x00,               // b4: mov dword [rdi + 12], 0
    0xC3,                                                   // bb: ret
    0x50,                                                   // bc: gp: push rax
    0xFF, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,               // bd: inc dword [0x7000] ; how many
    0x8B, 0x44, 0x24, 0x08,                                 // c4: mov eax, dword [rsp + 8] ; its error code
    0x89, 0x04, 0x25, 0x04, 0x70, 0x00, 0x00,               // c8: mov dword [0x7004], eax
    0x48, 0x83, 0x44, 0x24, 0x10, 0x03,                     // cf: add qword [rsp + 16], 3 ; past the mov
    0x58,                                                   // d5: pop rax
    0x48, 0x83, 0xC4, 0x08,                                 // d6: add rsp, 8 ; the error code
    0x48, 0xCF,                                             // da: iretq
    0xDF, 0x00,                                             // dc: idtr: dw 0xDF ; fourteen gates
    0x00, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,         // dq 0xA000
];

/* What the guest leaves at `RESULTS`: the #GPs its handler took at +0, the
 * last one's error code at +4, the six CR4s it read at +8 on, and 'CR4!' at
 * +0x38 once done. */
const CR4_GPS: u64 = RESULTS;
const CR4_ERROR: u64 = RESULTS + 0x4;
const CR4_READS: u64 = RESULTS + 0x8;
const CR4_DONE: u64 = RESULTS + 0x38;
const CR4_MARK: u32 = u32::from_le_bytes(*b"CR4!");
/// How many CR4s it reads, how many times it writes it, and how many of the
/// writes a CPU refuses: SMEP, PAE cleared, bit 32.
const CR4_READ_COUNT: usize = 6;
const CR4_WRITE_COUNT: u64 = 5;
const CR4_REFUSED: u32 = 3;
const CR4_BUDGET_MS: u64 = 10_000;
/* CR4's bits as the guest has them. */
const CR4_PAE: u64 = 1 << 5;
const CR4_MCE: u64 = 1 << 6;
const CR4_PGE: u64 = 1 << 7;

/// Run the cr4 guest: one CPU, through the run loop a Linux guest's go
/// through, where AMD-V stops it at every `mov` of CR4.
fn run_cr4(machine: &Arc<Machine>, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", CR4_NAME, CR4_ABOUT);
    let fail = |out: &mut dyn Write, why: &str| {
        let _ = writeln!(out, "hv: guest {} FAILED -- {}", CR4_NAME, why);
        false
    };
    let Some(doorbells) = Doorbells::new(1) else { return fail(out, "out of memory") };
    let (guest, mut cpus) = match LinuxGuest::new(machine, MEMORY, 1, Arc::new(doorbells), false, false) {
        Ok(made) => made,
        Err(e) => return fail(out, &alloc::format!("could not be made: {}", e)),
    };
    let Some(mut bsp) = cpus.pop() else { return fail(out, "no CPU") };
    if let Err(e) = one_cpu_board(guest.memory(), &mut bsp, CR4_CODE) {
        return fail(out, &alloc::format!("could not be made: {}", e));
    }
    let vmx = matches!(machine.ext(), Ok(hvarch::Ext::Vmx));
    let start = time::boot_time_ns();
    let deadline = start.saturating_add(CR4_BUDGET_MS * kcore::consts::NS_PER_MS);
    let counts = guest.run(&mut bsp, machine, deadline, &Quiet);
    /* The CR4 the guest believes it has, the one the CPU ran it with, and
     * whether its VMCB asked to stop its reads of CR4. */
    let backend = bsp.backend_mut();
    let held = Held {
        believed: backend.cr4(),
        register: backend.save().cr4,
        reads_asked: matches!(&*backend, Backend::Svm(v)
            if v.control().intercept_cr & hvarch::x86::svm::vmcb::intercept::cr::READ_CR4 != 0),
    };
    drop(bsp);
    let ns = time::boot_time_ns().saturating_sub(start);
    let stopped = guest.take_stopped()
        .unwrap_or(Stopped { stop: GuestStop::Requested, cpu: 0, dump: String::new() });
    if let GuestStop::Refused(Refusal::NotOn(cpu)) = stopped.stop {
        let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first", CR4_NAME, cpu);
        return false;
    }
    let host = guest.cpu_states().first().and_then(|&(_, h)| h);
    match host {
        Some(h) => { let _ = writeln!(out, "  ran on     cpu {}, {} us", h, ns / kcore::consts::NS_PER_US); }
        None => { let _ = writeln!(out, "  ran on     no cpu, {} us", ns / kcore::consts::NS_PER_US); }
    }
    let _ = writeln!(out, "  exits      {}, {} of CR4 performed, {} refused", counts.exits, counts.cr4, counts.cr4_gp);
    let mut says = String::new();
    let _ = describe_stop(&stopped.stop, &mut says);
    let _ = writeln!(out, "  stopped    {}", says);

    match check_cr4(guest.memory(), &stopped, &counts, &held, vmx) {
        Ok(checked) => {
            let _ = writeln!(out, "  checked    {}", checked);
            let _ = writeln!(out, "hv: guest {} ok", CR4_NAME);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", CR4_NAME, why);
            let _ = writeln!(out, "  cpu {} as it stopped:", stopped.cpu);
            let _ = out.write_str(&stopped.dump);
            false
        }
    }
}

/// What the host held of the cr4 guest's CPU when it stopped.
struct Held {
    /// The CR4 the guest believes it has.
    believed: u64,
    /// The CR4 the CPU ran it with.
    register: u64,
    /// Its VMCB asked the CPU to stop it at a read of CR4.
    reads_asked: bool,
}

/// Whether this machine is QEMU's TCG, by its hypervisor leaf's signature:
/// whose AMD-V checks a `mov` from any control register against CR0's read
/// intercept, not the register's own -- its decoder adds the register's
/// number to the exit code only when the code is not 0, and a read of CR0's
/// is (`decode_op` in QEMU 10.2's `target/i386/tcg/decode-new.c.inc`) -- and
/// so never stops a guest at a read of CR4. A real AMD CPU, and KVM's
/// nested AMD-V, do.
fn under_qemu_tcg() -> bool {
    hvarch::x86::cpu::hypervisor_signature() == Some(*b"TCGTCGTCGTCG")
}

/// Under AMD-V: the guest read back what it wrote -- MCE cleared, then PGE
/// set -- while the CPU's CR4 kept MCE; SMEP, which its CPUID does not
/// offer, PAE cleared in long mode and bit 32 were each a #GP, with no
/// error code, CR4 as it was; and every read and write was performed by the
/// host. On QEMU's TCG, which does not stop a guest at a read of CR4 though
/// its VMCB asks (`under_qemu_tcg`), the reads are the CPU's own, and give
/// the register, MCE in it; the writes are the host's all the same. Under
/// VT-x, which does not stop a guest at CR4 yet, what any CPU does: the
/// last two refused, and it ran to its halt.
fn check_cr4(m: &GuestMemory, stopped: &Stopped, counts: &Counts, held: &Held, vmx: bool)
    -> core::result::Result<String, String>
{
    let unread = |e: hvarch::Error| alloc::format!("its results cannot be read: {}", e);
    if !matches!(stopped.stop, GuestStop::Halted { .. }) {
        return Err(String::from("it did not run to its halt"));
    }
    let done = m.read_obj::<u32>(CR4_DONE).map_err(unread)?;
    let gps = m.read_obj::<u32>(CR4_GPS).map_err(unread)?;
    let error = m.read_obj::<u32>(CR4_ERROR).map_err(unread)?;
    let mut reads = [0u64; CR4_READ_COUNT];
    for (i, r) in reads.iter_mut().enumerate() {
        *r = m.read_obj::<u64>(CR4_READS + 8 * i as u64).map_err(unread)?;
    }
    if done != CR4_MARK {
        return Err(alloc::format!("its mark is {:#x}", done));
    }
    if vmx {
        /* PAE cleared in long mode, and bit 32: refused by any CPU, CR4 as
         * it was. SMEP is the host's to have or not. */
        if gps < 2 || reads[3] != reads[2] || reads[5] != reads[4] {
            return Err(alloc::format!("its #GPs were {} and it read CR4 {:#x?}: wanted PAE cleared and bit 32 \
                                       refused, CR4 unchanged by each", gps, reads));
        }
        return Ok(String::from("VT-x does not stop a guest at CR4 yet: the CPU took every write itself -- PAE \
                                cleared in long mode and bit 32 refused -- and keeps no MCE for the host"));
    }
    if !held.reads_asked {
        return Err(String::from("its VMCB does not ask to stop it at a read of CR4"));
    }
    /* What it reads: the CR4 it wrote, its reads stopped and answered by the
     * host -- or on QEMU's TCG, the register's, MCE in it. */
    let tcg = under_qemu_tcg();
    let (want, performed) = if tcg {
        ([CR4_PAE | CR4_MCE, CR4_PAE | CR4_MCE, CR4_PAE | CR4_MCE, CR4_PAE | CR4_MCE,
          CR4_PAE | CR4_PGE | CR4_MCE, CR4_PAE | CR4_PGE | CR4_MCE], CR4_WRITE_COUNT)
    } else {
        ([CR4_PAE | CR4_MCE, CR4_PAE, CR4_PAE, CR4_PAE, CR4_PAE | CR4_PGE, CR4_PAE | CR4_PGE],
         CR4_READ_COUNT as u64 + CR4_WRITE_COUNT)
    };
    if reads != want {
        return Err(alloc::format!("it read CR4 {:#x?}, not {:#x?}{}", reads, want,
                                  if tcg { ", the register's on QEMU's TCG" } else { "" }));
    }
    if gps != CR4_REFUSED || error != 0 {
        return Err(alloc::format!("its #GPs were {}, the last's error code {:#x}: wanted {} and 0", gps, error,
                                  CR4_REFUSED));
    }
    if held.believed != CR4_PAE | CR4_PGE || held.register != CR4_PAE | CR4_PGE | CR4_MCE {
        return Err(alloc::format!("it believes CR4 is {:#x} and the CPU ran it with {:#x}: wanted {:#x}, and that \
                                   with MCE", held.believed, held.register, CR4_PAE | CR4_PGE));
    }
    if counts.cr4 != performed || counts.cr4_gp != u64::from(CR4_REFUSED) {
        return Err(alloc::format!("the host performed {} of its CR4 accesses and refused {}: wanted {} and {}",
                                  counts.cr4, counts.cr4_gp, performed, CR4_REFUSED));
    }
    let writes = "while the CPU's CR4 kept MCE; SMEP, PAE cleared and bit 32 each a #GP, CR4 as it was";
    Ok(if tcg {
        alloc::format!("its writes the host's, MCE cleared in the CR4 the host keeps for it, {} -- its reads the \
                        CPU's own, QEMU's TCG not stopping a read of CR4 though the VMCB asks", writes)
    } else {
        alloc::format!("it read back what it wrote, MCE cleared and then PGE set, {}", writes)
    })
}

/* ---- msr: the MSRs a guest is answered, a #GP for every other ---- */

const MSR_NAME: &str = "msr";
const MSR_ABOUT: &str = "a guest's MSRs: SPEC_CTRL and PRED_CMD, which its CPUID does not offer, an MSR nobody \
                         names and MTRRs turned on, which nothing keeps, each a #GP; the machine-check registers \
                         and the PAT answered; and CPUID with no speculation control and no PMU in it";

/* A Linux guest's CPU, one, in long mode with an IDT of its own, whose #GP
 * handler counts each, keeps the MSR it was refused (ECX, eight at most)
 * and its error code, and steps over the 2-byte `rdmsr` or `wrmsr` that
 * raised it. It reads SPEC_CTRL and writes an IBPB to PRED_CMD; reads AMD's
 * LS_CFG, which this policy names for no CPU; turns its MTRRs on,
 * write-back; reads MCG_CAP, MCG_STATUS and the PAT; turns the MTRRs off --
 * which they are; then asks CPUID for 0x8000_0008, 0xA, 0 and 1 -- and
 * leaves its mark and halts. Assembled from this source with clang, the
 * offsets in the comments its listing's. */
const MSR_CODE: &[u8] = &[
    0x48, 0x8D, 0x05, 0xF3, 0x00, 0x00, 0x00,               // 00: lea rax, [rip + gp] ; gate 13, #GP
    0xBF, 0xD0, 0xA0, 0x00, 0x00,                           // 07: mov edi, 0xA0D0
    0xE8, 0xC3, 0x00, 0x00, 0x00,                           // 0c: call gate
    0x0F, 0x01, 0x1D, 0x13, 0x01, 0x00, 0x00,               // 11: lidt [rip + idtr]
    0xB9, 0x48, 0x00, 0x00, 0x00,                           // 18: mov ecx, 0x48 ; SPEC_CTRL: #GP
    0x0F, 0x32,                                             // 1d: rdmsr
    0xB9, 0x49, 0x00, 0x00, 0x00,                           // 1f: mov ecx, 0x49 ; PRED_CMD, IBPB: #GP
    0xB8, 0x01, 0x00, 0x00, 0x00,                           // 24: mov eax, 1
    0x31, 0xD2,                                             // 29: xor edx, edx
    0x0F, 0x30,                                             // 2b: wrmsr
    0xB9, 0x20, 0x10, 0x01, 0xC0,                           // 2d: mov ecx, 0xC0011020 ; LS_CFG: #GP
    0x0F, 0x32,                                             // 32: rdmsr
    0xB9, 0xFF, 0x02, 0x00, 0x00,                           // 34: mov ecx, 0x2FF ; MTRRs on, write-back: #GP
    0xB8, 0x06, 0x08, 0x00, 0x00,                           // 39: mov eax, 0x806
    0x31, 0xD2,                                             // 3e: xor edx, edx
    0x0F, 0x30,                                             // 40: wrmsr
    0xB9, 0x79, 0x01, 0x00, 0x00,                           // 42: mov ecx, 0x179 ; MCG_CAP
    0x0F, 0x32,                                             // 47: rdmsr
    0x89, 0x04, 0x25, 0x10, 0x70, 0x00, 0x00,               // 49: mov dword [0x7010], eax
    0x89, 0x14, 0x25, 0x14, 0x70, 0x00, 0x00,               // 50: mov dword [0x7014], edx
    0xB9, 0x7A, 0x01, 0x00, 0x00,                           // 57: mov ecx, 0x17A ; MCG_STATUS
    0x0F, 0x32,                                             // 5c: rdmsr
    0x89, 0x04, 0x25, 0x18, 0x70, 0x00, 0x00,               // 5e: mov dword [0x7018], eax
    0x89, 0x14, 0x25, 0x1C, 0x70, 0x00, 0x00,               // 65: mov dword [0x701C], edx
    0xB9, 0x77, 0x02, 0x00, 0x00,                           // 6c: mov ecx, 0x277 ; the PAT
    0x0F, 0x32,                                             // 71: rdmsr
    0x89, 0x04, 0x25, 0x20, 0x70, 0x00, 0x00,               // 73: mov dword [0x7020], eax
    0x89, 0x14, 0x25, 0x24, 0x70, 0x00, 0x00,               // 7a: mov dword [0x7024], edx
    0xB9, 0xFF, 0x02, 0x00, 0x00,                           // 81: mov ecx, 0x2FF ; MTRRs off, as they are
    0x31, 0xC0,                                             // 86: xor eax, eax
    0x31, 0xD2,                                             // 88: xor edx, edx
    0x0F, 0x30,                                             // 8a: wrmsr
    0xB8, 0x08, 0x00, 0x00, 0x80,                           // 8c: mov eax, 0x80000008
    0x31, 0xC9,                                             // 91: xor ecx, ecx
    0x0F, 0xA2,                                             // 93: cpuid
    0x89, 0x1C, 0x25, 0x28, 0x70, 0x00, 0x00,               // 95: mov dword [0x7028], ebx
    0xB8, 0x0A, 0x00, 0x00, 0x00,                           // 9c: mov eax, 0xA
    0x31, 0xC9,                                             // a1: xor ecx, ecx
    0x0F, 0xA2,                                             // a3: cpuid
    0x89, 0x04, 0x25, 0x2C, 0x70, 0x00, 0x00,               // a5: mov dword [0x702C], eax
    0x31, 0xC0,                                             // ac: xor eax, eax
    0x0F, 0xA2,                                             // ae: cpuid
    0x89, 0x04, 0x25, 0x30, 0x70, 0x00, 0x00,               // b0: mov dword [0x7030], eax
    0xB8, 0x01, 0x00, 0x00, 0x00,                           // b7: mov eax, 1
    0x0F, 0xA2,                                             // bc: cpuid
    0x89, 0x0C, 0x25, 0x34, 0x70, 0x00, 0x00,               // be: mov dword [0x7034], ecx
    0xC7, 0x04, 0x25, 0x38, 0x70, 0x00, 0x00,               // c5: mov dword [0x7038], 'MSR!'
    0x4D, 0x53, 0x52, 0x21,
    0xFA,                                                   // d0: cli
    0xF4,                                                   // d1: .dead: hlt
    0xEB, 0xFD,                                             // d2: jmp .dead
    0x66, 0x89, 0x07,                                       // d4: gate: mov word [rdi], ax ; an interrupt gate for rax
    0x66, 0xC7, 0x47, 0x02, 0x08, 0x00,                     // d7: mov word [rdi + 2], 0x08
    0x66, 0xC7, 0x47, 0x04, 0x00, 0x8E,                     // dd: mov word [rdi + 4], 0x8E00
    0x48, 0xC1, 0xE8, 0x10,                                 // e3: shr rax, 16
    0x66, 0x89, 0x47, 0x06,                                 // e7: mov word [rdi + 6], ax
    0x48, 0xC1, 0xE8, 0x10,                                 // eb: shr rax, 16
    0x89, 0x47, 0x08,                                       // ef: mov dword [rdi + 8], eax
    0xC7, 0x47, 0x0C, 0x00, 0x00, 0x00, 0x00,               // f2: mov dword [rdi + 12], 0
    0xC3,                                                   // f9: ret
    0x50,                                                   // fa: gp: push rax
    0x8B, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,               // fb: mov eax, dword [0x7000] ; how many so far
    0x83, 0xE0, 0x07,                                       // 102: and eax, 7
    0x89, 0x0C, 0x85, 0x40, 0x70, 0x00, 0x00,               // 105: mov dword [0x7040 + rax * 4], ecx ; which MSR
    0xFF, 0x04, 0x25, 0x00, 0x70, 0x00, 0x00,               // 10c: inc dword [0x7000]
    0x8B, 0x44, 0x24, 0x08,                                 // 113: mov eax, dword [rsp + 8] ; its error code
    0x89, 0x04, 0x25, 0x04, 0x70, 0x00, 0x00,               // 117: mov dword [0x7004], eax
    0x48, 0x83, 0x44, 0x24, 0x10, 0x02,                     // 11e: add qword [rsp + 16], 2 ; past the rdmsr or wrmsr
    0x58,                                                   // 124: pop rax
    0x48, 0x83, 0xC4, 0x08,                                 // 125: add rsp, 8 ; the error code
    0x48, 0xCF,                                             // 129: iretq
    0xDF, 0x00,                                             // 12b: idtr: dw 0xDF ; fourteen gates
    0x00, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,         // dq 0xA000
];

/* What the guest leaves at `RESULTS`: the #GPs its handler took at +0, the
 * last one's error code at +4; MCG_CAP, MCG_STATUS and the PAT at +0x10,
 * +0x18 and +0x20; CPUID 0x8000_0008's EBX, 0xA's EAX, 0's EAX and 1's ECX
 * at +0x28 on; 'MSR!' at +0x38 once done; and the MSR of each #GP, in turn,
 * at +0x40. */
const MSR_GPS: u64 = RESULTS;
const MSR_ERROR: u64 = RESULTS + 0x4;
const MSR_MCG_CAP: u64 = RESULTS + 0x10;
const MSR_MCG_STATUS: u64 = RESULTS + 0x18;
const MSR_PAT: u64 = RESULTS + 0x20;
const MSR_EXT8_EBX: u64 = RESULTS + 0x28;
const MSR_PMU_EAX: u64 = RESULTS + 0x2C;
const MSR_TOP: u64 = RESULTS + 0x30;
const MSR_LEAF1_ECX: u64 = RESULTS + 0x34;
const MSR_DONE: u64 = RESULTS + 0x38;
const MSR_REFUSALS: u64 = RESULTS + 0x40;
const MSR_MARK: u32 = u32::from_le_bytes(*b"MSR!");
/// The MSRs it is refused, in turn: SPEC_CTRL, PRED_CMD, LS_CFG and the
/// MTRRs' default type, written with them on.
const MSR_REFUSED: [u32; 4] = [0x48, 0x49, 0xC001_1020, 0x2FF];
/// What it reads of MSRs, and writes: five and three.
const MSR_READS: u64 = 5;
const MSR_WRITES: u64 = 3;
/// What a guest is not told of in 0x8000_0008's EBX: INVLPGB, IBPB, IBRS,
/// STIBP, SSBD, VIRT_SSBD and PSFD -- each the host's to back, and none
/// backed.
const MSR_SPECULATION: u32 = (1 << 3) | (1 << 12) | (1 << 14) | (1 << 15) | (1 << 24) | (1 << 25) | (1 << 28);
/// The highest basic leaf a guest may be told of, and leaf 1's hypervisor
/// bit.
const MSR_BASIC_TOP: u32 = 0x16;
const MSR_HYPERVISOR: u32 = 1 << 31;
const MSR_BUDGET_MS: u64 = 10_000;

/// Run the msr guest: one CPU, through the run loop a Linux guest's go
/// through, where every MSR but the three segment bases stops it.
fn run_msr(machine: &Arc<Machine>, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "hv: guest {} -- {}", MSR_NAME, MSR_ABOUT);
    let fail = |out: &mut dyn Write, why: &str| {
        let _ = writeln!(out, "hv: guest {} FAILED -- {}", MSR_NAME, why);
        false
    };
    let Some(doorbells) = Doorbells::new(1) else { return fail(out, "out of memory") };
    let (guest, mut cpus) = match LinuxGuest::new(machine, MEMORY, 1, Arc::new(doorbells), false, false) {
        Ok(made) => made,
        Err(e) => return fail(out, &alloc::format!("could not be made: {}", e)),
    };
    let Some(mut bsp) = cpus.pop() else { return fail(out, "no CPU") };
    if let Err(e) = one_cpu_board(guest.memory(), &mut bsp, MSR_CODE) {
        return fail(out, &alloc::format!("could not be made: {}", e));
    }
    let start = time::boot_time_ns();
    let deadline = start.saturating_add(MSR_BUDGET_MS * kcore::consts::NS_PER_MS);
    let counts = guest.run(&mut bsp, machine, deadline, &Quiet);
    drop(bsp);
    let ns = time::boot_time_ns().saturating_sub(start);
    let stopped = guest.take_stopped()
        .unwrap_or(Stopped { stop: GuestStop::Requested, cpu: 0, dump: String::new() });
    if let GuestStop::Refused(Refusal::NotOn(cpu)) = stopped.stop {
        let _ = writeln!(out, "hv: guest {} not run -- the extension is not on for cpu {}: hv on first", MSR_NAME, cpu);
        return false;
    }
    let host = guest.cpu_states().first().and_then(|&(_, h)| h);
    match host {
        Some(h) => { let _ = writeln!(out, "  ran on     cpu {}, {} us", h, ns / kcore::consts::NS_PER_US); }
        None => { let _ = writeln!(out, "  ran on     no cpu, {} us", ns / kcore::consts::NS_PER_US); }
    }
    let _ = writeln!(out, "  exits      {}, {} rdmsr, {} wrmsr, {} refused", counts.exits, counts.msr_read,
                     counts.msr_write, counts.msr_gp);
    let mut says = String::new();
    let _ = describe_stop(&stopped.stop, &mut says);
    let _ = writeln!(out, "  stopped    {}", says);

    match check_msr(guest.memory(), &stopped, &counts, &guest.msr_faults()) {
        Ok(checked) => {
            let _ = writeln!(out, "  checked    {}", checked);
            let _ = writeln!(out, "hv: guest {} ok", MSR_NAME);
            true
        }
        Err(why) => {
            let _ = writeln!(out, "hv: guest {} FAILED -- {}", MSR_NAME, why);
            let _ = writeln!(out, "  cpu {} as it stopped:", stopped.cpu);
            let _ = out.write_str(&stopped.dump);
            false
        }
    }
}

/// The msr guest's four refusals, in turn and each its own handler's, with
/// no error code; what it was answered; and what CPUID told it -- on either
/// backend, whose MSR exits both come to the same policy.
fn check_msr(m: &GuestMemory, stopped: &Stopped, counts: &Counts, faults: &[(u32, u64, bool)])
    -> core::result::Result<String, String>
{
    let unread = |e: hvarch::Error| alloc::format!("its results cannot be read: {}", e);
    if !matches!(stopped.stop, GuestStop::Halted { .. }) {
        return Err(String::from("it did not run to its halt"));
    }
    let done = m.read_obj::<u32>(MSR_DONE).map_err(unread)?;
    if done != MSR_MARK {
        return Err(alloc::format!("its mark is {:#x}", done));
    }
    let gps = m.read_obj::<u32>(MSR_GPS).map_err(unread)?;
    let error = m.read_obj::<u32>(MSR_ERROR).map_err(unread)?;
    let mut refused = [0u32; MSR_REFUSED.len()];
    for (i, r) in refused.iter_mut().enumerate() {
        *r = m.read_obj::<u32>(MSR_REFUSALS + 4 * i as u64).map_err(unread)?;
    }
    if gps as usize != MSR_REFUSED.len() || refused != MSR_REFUSED || error != 0 {
        return Err(alloc::format!("its #GPs were {}, at MSRs {:#x?}, the last's error code {:#x}: wanted {:#x?} \
                                   and 0", gps, refused, error, MSR_REFUSED));
    }
    let host_saw: alloc::vec::Vec<u32> = faults.iter().map(|&(msr, _, _)| msr).collect();
    if counts.msr_gp != MSR_REFUSED.len() as u64 || host_saw != MSR_REFUSED
        || counts.msr_read != MSR_READS || counts.msr_write != MSR_WRITES {
        return Err(alloc::format!("the host refused {} MSRs ({:#x?}) of {} reads and {} writes: wanted {:#x?} of {} \
                                   and {}", counts.msr_gp, host_saw, counts.msr_read, counts.msr_write, MSR_REFUSED,
                                  MSR_READS, MSR_WRITES));
    }
    let mcg_cap = m.read_obj::<u64>(MSR_MCG_CAP).map_err(unread)?;
    let mcg_status = m.read_obj::<u64>(MSR_MCG_STATUS).map_err(unread)?;
    let pat = m.read_obj::<u64>(MSR_PAT).map_err(unread)?;
    if mcg_cap != 0 || mcg_status != 0 || pat != crate::svm::PAT_RESET {
        return Err(alloc::format!("it read MCG_CAP {:#x}, MCG_STATUS {:#x} and the PAT {:#x}: wanted no banks, \
                                   nothing to report and {:#x}", mcg_cap, mcg_status, pat, crate::svm::PAT_RESET));
    }
    let ext8_ebx = m.read_obj::<u32>(MSR_EXT8_EBX).map_err(unread)?;
    let pmu = m.read_obj::<u32>(MSR_PMU_EAX).map_err(unread)?;
    let top = m.read_obj::<u32>(MSR_TOP).map_err(unread)?;
    let leaf1_ecx = m.read_obj::<u32>(MSR_LEAF1_ECX).map_err(unread)?;
    if ext8_ebx & MSR_SPECULATION != 0 || pmu != 0 || top > MSR_BASIC_TOP || leaf1_ecx & MSR_HYPERVISOR == 0 {
        return Err(alloc::format!("CPUID told it 0x8000_0008 EBX {:#x}, 0xA EAX {:#x}, a top of {:#x} and leaf 1 ECX \
                                   {:#x}: wanted none of {:#x}, no PMU, no higher than {:#x} and the hypervisor bit",
                                  ext8_ebx, pmu, top, leaf1_ecx, MSR_SPECULATION, MSR_BASIC_TOP));
    }
    Ok(String::from("SPEC_CTRL, PRED_CMD, LS_CFG and MTRRs turned on each a #GP its own handler took; MCG_CAP, \
                     MCG_STATUS and the PAT answered; CPUID offers no speculation control and no PMU, and says a \
                     hypervisor is there"))
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

/* The null DS is for the report: loaded without an exit, it is in the state
 * a fault's report shows only if that state was read at the fault -- under
 * VT-x, whose EPT violation reads only what performing an access takes, by
 * `read_whole_state` after the run. */
const FAULT_CODE: &[u8] = &[
    0x31, 0xC0,                                     // xor eax, eax
    0x8E, 0xD8,                                     // mov ds, eax       ; a null DS, fine in 64-bit code
    0xBF, 0x00, 0xF0, 0x1F, 0x00,                   // mov edi, 0x1ff000 ; mapped by its page table, past its memory
    0xC6, 0x07, 0x5A,                               // mov byte [rdi], 0x5a
    0xF4,                                           // hlt
];
const FAULT_GPA: u64 = 0x1F_F000;
const FAULT_RIP: u64 = ENTRY + 9;

fn build_fault(vm: &mut Vm) -> Result<()> {
    board(vm, FAULT_CODE)
}

fn check_fault(vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    use hvarch::x86::svm::vmcb::npf;
    match r.stop {
        Stop::Fault { gpa, error, rip } if gpa == FAULT_GPA && rip == FAULT_RIP => {
            if error & npf::WRITE == 0 || error & npf::PRESENT != 0 || error & npf::FINAL == 0 {
                return Err(alloc::format!("the fault was not a write to nothing: error {:#x}", error));
            }
            let ds = vm.vcpu().save().ds.selector;
            if ds != 0 {
                return Err(alloc::format!(
                    "the state a report would show is not the guest's at its fault: DS {:#x}, where it had loaded a null one", ds));
            }
            Ok(String::from("stopped at the nested table, at the address and the instruction it was told to, its state read whole"))
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

/* A device of 32 bytes of registers in the MMIO window, stored to and
 * loaded from by the forms of `mov` Linux's accessors are, the loads' values
 * put where the check reads them. Assembled by clang (AT&T source), shown
 * here in NASM's syntax as the others are. */
const MMIO_CODE: &[u8] = &[
    0xBE, 0x00, 0x00, 0xB0, 0xFE,                   // mov esi, 0xfeb00000
    0xC7, 0x06, 0x44, 0x33, 0x22, 0x11,             // mov dword [rsi], 0x11223344
    0xC6, 0x46, 0x04, 0x55,                         // mov byte [rsi+4], 0x55
    0x66, 0xC7, 0x46, 0x06, 0x77, 0x88,             // mov word [rsi+6], 0x8877
    0x48, 0xB8, 0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x99, 0x88, // mov rax, 0x8899aabbccddeeff
    0x48, 0x89, 0x46, 0x08,                         // mov [rsi+8], rax
    0x48, 0xC7, 0x46, 0x10, 0xFE, 0xFF, 0xFF, 0xFF, // mov qword [rsi+16], -2
    0xB7, 0xA5,                                     // mov bh, 0xa5
    0x88, 0x7E, 0x18,                               // mov [rsi+24], bh
    0x41, 0xBB, 0x04, 0x03, 0x02, 0x01,             // mov r11d, 0x01020304
    0x44, 0x89, 0x5E, 0x1C,                         // mov [rsi+28], r11d
    0x44, 0x8B, 0x0E,                               // mov r9d, [rsi]
    0x0F, 0xB6, 0x4E, 0x04,                         // movzx ecx, byte [rsi+4]
    0x48, 0x0F, 0xBF, 0x56, 0x06,                   // movsx rdx, word [rsi+6]
    0x4C, 0x8B, 0x66, 0x08,                         // mov r12, [rsi+8]
    0x49, 0xC7, 0xC5, 0xFF, 0xFF, 0xFF, 0xFF,       // mov r13, -1
    0x66, 0x44, 0x8B, 0x6E, 0x06,                   // mov r13w, [rsi+6]
    0xB8, 0x78, 0x56, 0x34, 0x12,                   // mov eax, 0x12345678
    0x8A, 0x66, 0x18,                               // mov ah, [rsi+24]
    0x0F, 0xBE, 0x7E, 0x18,                         // movsx edi, byte [rsi+24]
    0x4C, 0x89, 0x0C, 0x25, 0x00, 0x70, 0x00, 0x00, // mov [0x7000], r9
    0x48, 0x89, 0x0C, 0x25, 0x08, 0x70, 0x00, 0x00, // mov [0x7008], rcx
    0x48, 0x89, 0x14, 0x25, 0x10, 0x70, 0x00, 0x00, // mov [0x7010], rdx
    0x4C, 0x89, 0x24, 0x25, 0x18, 0x70, 0x00, 0x00, // mov [0x7018], r12
    0x4C, 0x89, 0x2C, 0x25, 0x20, 0x70, 0x00, 0x00, // mov [0x7020], r13
    0x48, 0x89, 0x04, 0x25, 0x28, 0x70, 0x00, 0x00, // mov [0x7028], rax
    0x48, 0x89, 0x3C, 0x25, 0x30, 0x70, 0x00, 0x00, // mov [0x7030], rdi
    0xF4,                                           // hlt
];
/// The device's page, in the MMIO window, and how many bytes of registers
/// it has.
const MMIO_DEV: u64 = 0xFEB0_0000;
const MMIO_DEV_BYTES: usize = 32;
const PAGE: u64 = 4096;
const MMIO_HLT: u64 = ENTRY + 0x9A;
/// The 2 MiB entry of the fourth gigabyte's page directory that covers it.
const MMIO_PD_INDEX: u64 = (MMIO_DEV - MMIO_GIB * GIB) >> 21;
/// The accesses it makes: seven stores and seven loads.
const MMIO_ACCESSES: u32 = 14;
/// What its registers hold after the stores, byte by byte.
const MMIO_DEV_AFTER: [u8; MMIO_DEV_BYTES] = [
    0x44, 0x33, 0x22, 0x11, 0x55, 0x00, 0x77, 0x88,
    0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x99, 0x88,
    0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xA5, 0x00, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01,
];
/// And the registers the loads left, in the order they are put in RAM:
/// R9 (a 32-bit load clears the top half), RCX (`movzx`), RDX (`movsx` of
/// a word), R12 (64 bits), R13 (a 16-bit load leaves the rest), RAX (AH),
/// RDI (`movsx` of a byte into 32 bits, and the top half cleared).
const MMIO_LOADED: [u64; 7] = [
    0x1122_3344,
    0x55,
    0xFFFF_FFFF_FFFF_8877,
    0x8899_AABB_CCDD_EEFF,
    0xFFFF_FFFF_FFFF_8877,
    0x1234_A578,
    0xFFFF_FFA5,
];

fn build_mmio(vm: &mut Vm) -> Result<()> {
    board(vm, MMIO_CODE)?;
    let m = vm.memory_mut();
    m.write_obj(PDPT + MMIO_GIB * 8, &(PD_MMIO | PTE_P_W))?;
    m.write_obj(PD_MMIO + MMIO_PD_INDEX * 8, &((MMIO_DEV & !0x1F_FFFF) | PTE_P_W | PTE_LARGE))
}

fn check_mmio(vm: &Vm, r: &Run) -> core::result::Result<String, String> {
    match r.stop {
        Stop::Halted { rip } if rip == MMIO_HLT => {}
        Stop::Fault { gpa, rip, .. } => {
            return Err(alloc::format!("the access of {:#x} at {:#x} was not performed", gpa, rip));
        }
        _ => return Err(String::from("it did not run to its halt")),
    }
    if r.mmio != MMIO_ACCESSES {
        return Err(alloc::format!("{} accesses performed, not {}", r.mmio, MMIO_ACCESSES));
    }
    if r.dev != MMIO_DEV_AFTER {
        return Err(alloc::format!("the device holds {:02x?}, not {:02x?}", r.dev, MMIO_DEV_AFTER));
    }
    for (i, want) in MMIO_LOADED.iter().enumerate() {
        let got: u64 = read(vm, RESULTS + 8 * i as u64)?;
        if got != *want {
            return Err(alloc::format!("load {} left {:#x}, not {:#x}", i, got, want));
        }
    }
    Ok(alloc::format!("{} stores and loads of every size decoded and performed, each value where it belonged",
                      MMIO_ACCESSES))
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
