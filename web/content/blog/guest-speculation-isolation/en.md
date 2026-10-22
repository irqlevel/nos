---
title: "The world switch: keeping a guest's speculation off the host and the next guest"
description: "Cleared registers, a refilled return stack, IBPB, IBRS, BHI_DIS_S, VERW: what the nos hypervisor does around every VM exit, what it costs, what is open."
date: 2026-10-21
tags:
  - hypervisor
  - safety
  - x86-64
  - performance
  - hardware
---

The goal nos is built toward is other people's Linux guests on one machine. Architecturally a guest reaches nothing it was not given: its nested page table maps only pages its own memory owns, an invariant of a type rather than a promise in a comment. But a VM exit hands the CPU from code the guest chose to the host's code on the same core — the same branch predictors, the same return stack, the same internal buffers — and no page table covers what the guest leaves there. Until 2026-10-12 the world switch did nothing about it. The commit that changed that, [a8fcec3](https://github.com/irqlevel/nos/commit/a8fcec3d1ba27e7eadafa536da0fb9591f97a097), opens with the inventory:

> None of it was there: after an exit the guest's values stayed in the caller-saved registers the run stub returned with, the return stack held whatever return addresses the guest had left in it for the host's returns to be predicted to, nothing stood between two guests' training of the indirect branch predictors on one CPU, and the host's indirect branches were open to it too.

The guest had been misinformed as well: the CPUID policy passed leaf 0x80000008's EBX through nearly whole, telling a guest on AMD it had IBPB, IBRS, STIBP and SSBD, and every MSR the policy did not name read 0 and swallowed writes. The guest wrote `PRED_CMD` and `SPEC_CTRL` into nothing while its sysfs said it was mitigated — a failure that passes in silence. [61256c8](https://github.com/irqlevel/nos/commit/61256c852980267fbb8b1a0d37112154e21376db) made the [policy](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/policy.rs) an allow-list: no speculation control is offered until its MSR is switched with the guest, and `hv-fuzz` holds every answer to a model of what each CPUID bit promises.

What follows is the world switch as it stands now — [`hvarch::x86::spec`](https://github.com/irqlevel/nos/blob/master/src/rust/hvarch/src/x86/spec.rs) and the two run stubs, written with KVM's as the reference.

## After every exit: registers and the return stack

Both run stubs are naked functions. After an exit, once the guest's registers are saved, the stub clears every register the caller does not get back from its own pops: a register still holding a guest's value is a value the host may use past a branch it has not resolved yet. Then, before the host's first `ret`, it refills the return stack buffer. The RSB predicts where a `ret` goes from recent call sites; after a guest has run they are the guest's, and the host's returns would be predicted to addresses the guest chose. The fill is KVM's `FILL_RETURN_BUFFER` — 32 calls of the stub's own, each over an `int3` that a return predicted to it would land on, the stack put back after every two, an `lfence` for the loop's last branch:

```rust
        "xor ecx, ecx",
        "xor edx, edx",
        // ...
        "mov ecx, {rsb_pairs}",
        "2:",
        "call 3f",
        "int3",
        "3:",
        "call 4f",
        "int3",
        "4:",
        "add rsp, 16",
        "dec ecx",
        "jnz 2b",
        "lfence",
```

In AMD-V's [`vmrun_stub`](https://github.com/irqlevel/nos/blob/master/src/rust/hvarch/src/x86/svm.rs) this runs with GIF still clear, so nothing at all — no interrupt, no NMI — runs between `#vmexit` and the fill. VT-x's [`vmx_launch_stub`](https://github.com/irqlevel/nos/blob/master/src/rust/hvarch/src/x86/vmx.rs) does the same with interrupts off, as an exit leaves them.

## Between two guests: an IBPB

An indirect branch prediction barrier — bit 0 written to `IA32_PRED_CMD` — makes the predictors forget what earlier code trained them with. nos issues one before entering a guest that is not the last one entered on that CPU. Guests are told apart by their nested table's id, which a guest's own vCPUs share, so they pay no barrier between each other:

```rust
    pub(super) fn entering(&mut self, table: u64) {
        if self.last != Some(table) {
            SWITCHES.fetch_add(1, Ordering::Relaxed);
            if IBPB.load(Ordering::Relaxed) {
                /* PRED_CMD exists where CPUID offers IBPB, and IBPB is all
                 * a write of this bit does. */
                unsafe { cpu::wrmsr(MSR_PRED_CMD, PRED_CMD_IBPB) };
                BARRIERS.fetch_add(1, Ordering::Relaxed);
            }
            self.last = Some(table);
        }
    }
```

The `unsafe` is sound because `IBPB` is only ever set from CPUID's answer, taken once as the extension goes on — asking CPUID at every entry would be an exit of its own under a nested hypervisor. [`hv-test.py`](https://github.com/irqlevel/nos/blob/master/scripts/hv-test.py) holds `hv`'s count to at least two switches per built-in guest, each with an IBPB where the CPU has one; on the EX44's silicon one session issued 159.

## The host's own branches: IBRS and its relatives

The refill and the barrier still leave the host's own indirect branches predictable from what a guest trained. On AMD from Zen 4, automatic IBRS (`EFER` bit 21) covers that, and nos keeps it on while the extension is; the AX41's Zen 2 does not have it. The Intel half ([86deabd](https://github.com/irqlevel/nos/commit/86deabddfe35caef0024ac90188b857136e18161), merged in [48bfaf5](https://github.com/irqlevel/nos/commit/48bfaf53cb7da7cd18a16f641ae1874e6a669fe2)) is decided for each CPU as the extension goes on there, from CPUID and `IA32_ARCH_CAPABILITIES`:

```rust
    let enhanced = caps & CAP_IBRS_ALL != 0;
    // ...
    let mds = caps & CAP_MDS_NO == 0;
    let taa = tsx && caps & CAP_TAA_NO == 0;
    let rfds = caps & CAP_RFDS_NO == 0 && caps & CAP_RFDS_CLEAR != 0;
    let mmio = caps & CAP_MMIO_NO != CAP_MMIO_NO && caps & CAP_FB_CLEAR != 0;
    let mut intel = Intel {
        verw: features.edx & EDX_MD_CLEAR != 0 && (mds || taa || rfds || mmio),
        l1d_flush: caps & (CAP_RDCL_NO | CAP_SKIP_L1DFL_VMENTRY) == 0 && features.edx & EDX_L1D_FLUSH != 0,
        bhi_open: enhanced && caps & CAP_BHI_NO == 0 && !bhi_ctrl,
        ..Intel::default()
    };
```

- **IBRS** in `IA32_SPEC_CTRL` while the extension is on. Enhanced IBRS (Ice Lake and later; the EX44) is set once and holds. Legacy IBRS (Skylake; the Dell laptop) is a barrier only at the moment it is written, so the stub writes it again after every exit, before the host's first indirect branch.
- **BHI_DIS_S** beside it where the CPU has the control (Alder Lake and later): the branch history a guest leaves behind is not what the host's indirect branches are predicted by.
- **VERW** as the last instruction before every entry, where MDS, TAA, RFDS or MMIO stale data would let a guest sample what the host left in the CPU's buffers. Only its memory form clears them; the operand is a selector of 0.
- **An L1D flush** before every entry, where L1TF lets a guest read the cache (Skylake).

On the EX44's i5-13500 that comes to enhanced IBRS, BHI_DIS_S and VERW — for RFDS, which its E-cores have — and no L1D flush: what Linux reports for the same CPU.

## What it costs

`hv bench` on the EX44, bare metal, at the cores' 2.5 GHz base clock, each mitigation's share measured with a module built without it ([What an exit costs](/docs/hypervisor#what-an-exit-costs)):

| An exit, ns | P-core | E-core |
|---|---|---|
| without the mitigations | 1,690 | 2,065 |
| with all of them | 1,818 | 2,490 |
| the return stack refilled with 2 entries, not 32 | -18 | -30 |
| no BHI_DIS_S | -22 | -287 |
| no VERW | -30 | -106 |
| no IBRS, and so no BHI_DIS_S | -50 | -303 |
| none of the four | -97 | -431 |

That is 7% of an exit on a P-core and 20% on an E-core, where two thirds of it is BHI_DIS_S — a cost not of the switch but of every indirect branch the host takes while the extension is on, which Linux on the same box pays across its whole kernel. The full 32-entry refill stayed.

The first build measured worse, and the number pointed at the bug: the entry's checks went from 17 ns to 418, because an entry took three per-CPU records, each `CpuLocal::with` a round of calls into the kernel. They are one record now, `Entries`, taken once. One number is still unexplained: the first load of the module after a boot runs E-core exits at 2.2 µs, BHI_DIS_S's share gone, though `hv` reads the bit back as set.

## Making it loud

A mitigation someone else turned off is a failure that looks like success, so `hv` does not trust its own bookkeeping. It sends each CPU the extension is on for an IPI that reads `IA32_SPEC_CTRL` back, and warns, naming the CPUs, where the register no longer has what `hv on` set: "something wrote it behind this module's back". `hv-test.py` fails if the warning appears.

The report itself had a bug of that kind: it read the flag `on` keeps for the entries, so before the first `hv on` the EX44 — whose i5-13500 has IBPB — was reported to have none. [57a3c47](https://github.com/irqlevel/nos/commit/57a3c47a41b525b88deaaf9f0c340046d5e7b385) asks CPUID instead, and after `insmod`, before `hv on`, the EX44 says:

```text
hv: guests switched on a CPU 0 times, an IBPB between each two (0 issued)
```

So did a gate: six scripts, `hv-test` among them, took `out/<arch>/modules` whatever the build, so the `UBSAN=1 RUSTUB=1` run that gated the AMD-V half's merge ([19d0d14](https://github.com/irqlevel/nos/commit/19d0d142c81cafcf4d2b4eb742f5dc9606d6f24c)) ran the plain `hv.ko` unless one had been copied over by hand. [de70e8d](https://github.com/irqlevel/nos/commit/de70e8d54308d8b54681a356a2b23c02cf7fd47a) has them read the build's flavour stamp.

## The guest's view, and its neighbours

What a guest is told decides what it protects itself with. Until [d2b01bf](https://github.com/irqlevel/nos/commit/d2b01bfe150b27b2e564a036f7233e36d3364cea) leaf 7 was blank, which on Intel tells Linux there is no `ARCH_CAPABILITIES`: Alpine on the EX44 took its CPU for one open to everything — page-table isolation without PCID, a TLB flush at every system call, MDS and RFDS "Vulnerable: no microcode". Now `ARCH_CAPABILITIES` reads as the host's but for what is not a fact of the CPU — `IBRS_ALL` stays out, since `SPEC_CTRL` is not the guest's — and Alpine reports Meltdown, MDS and L1TF "Not affected", RFDS "Mitigation: Clear Register File". Under all of it, nos now loads the machine's own microcode, 0x3a to 0x3e on the EX44 ([8a6e8af](https://github.com/irqlevel/nos/commit/8a6e8afa0312174e6aa32bf74e2e42d289bbe931)): a mitigation is only as good as the microcode under it.

Hyper-threads are the other neighbourhood: two threads of one core share its execution units, caches and buffers — what MDS, L1TF and port contention stand on. Since [7cceb6f](https://github.com/irqlevel/nos/commit/7cceb6fd6c869164707594c415a0becd9f7fdf26) a guest's cores are its own, whole cores no other guest has a vCPU or a disk task on ([`hv::place`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/place.rs)). It costs: on the AX41 two busy loops in a guest took 14.7 s on one core's two threads and 9.7 s on two cores; `hv cores shared` is there for a machine whose guests are all one's own.

## What is still open

The list lives in the hypervisor doc, under [Before other people's guests](/docs/hypervisor#before-other-peoples-guests):

- **SRSO and Retbleed** on Zen 1 to 4 — the AX41 — want every `ret` of the host through an untrained return thunk, a compiler's job for the whole kernel, or an IBPB at every exit.
- **BTI without automatic IBRS**, the AX41 again, wants the host built with retpolines.
- **A guest's own speculation control** — IBRS, STIBP, SSBD, IBPB — is offered to none until `SPEC_CTRL` is switched with the guest; until then a guest uses retpolines and its store bypass is open.
- **Branch history cleared in software**, for a CPU with enhanced IBRS, open to BHI and without BHI_DIS_S — Ice Lake, Tiger Lake, or nos under QEMU 8.2's nested KVM, which hides the control. `hv` names such a CPU rather than pretend.
- **L1TF across hyper-threads**: the flush does not reach the sibling, and the host's own tasks, the network stack among them, still run on any core.
- **Never run on silicon**: the AMD-V half's IBPB and automatic IBRS writes — TCG offers neither, and the AX41's Zen 2, which has IBPB, is where the first will run — and the Dell's Skylake paths, legacy IBRS after every exit and the L1D flush.

Each mitigation that is in has a line in `hv`, a check in a gate and a number from the EX44; each that is not has a line on that list. The rest of the story is in [a month of the nos hypervisor](/blog/linux-under-nos-hypervisor) and on [the hypervisor page](/hypervisor).
