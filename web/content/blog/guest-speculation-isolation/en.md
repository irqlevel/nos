---
title: "Keeping a guest's speculation away from the host and other guests"
description: "How the nos hypervisor contains a guest's speculation at every VM exit (cleared registers, RSB refill, IBPB, IBRS, BHI_DIS_S, VERW), its cost and what is open."
date: 2026-10-21
tags:
  - hypervisor
  - safety
  - x86-64
  - performance
  - hardware
---

The goal of nos is to run other people's Linux guests on one machine. Architecturally, a guest can reach nothing it was not given. Its nested page table, which translates guest memory into host memory, maps only pages that the guest's own memory owns. That invariant is enforced by a type, not promised in a comment.

A VM exit is another matter. At an exit the CPU leaves the guest and hands control back to the hypervisor, so the same core goes straight from code the guest chose to the host's code. Both use the same branch predictors, the same return stack and the same internal buffers, and no page table covers what the guest leaves in them. Until 2026-10-12 the world switch, the code that moves the CPU between host and guest, did nothing about it. The commit that changed that, [a8fcec3](https://github.com/irqlevel/nos/commit/a8fcec3d1ba27e7eadafa536da0fb9591f97a097), starts by listing everything that was missing.

The run stub is the assembly routine that enters a guest and takes control back on an exit. After an exit, the guest's values stayed in the caller-saved registers the stub returned with. The return stack still held whatever return addresses the guest had left in it, so the host's returns could be predicted to go there. Nothing stood between two guests training the indirect branch predictors on the same CPU. And the host's own indirect branches were open to the guest too.

The guest was misinformed as well. The CPUID policy, the hypervisor's rules for what a guest sees in CPUID and the MSRs, passed EBX of leaf 0x80000008 through nearly whole. That told a guest on AMD it had IBPB, IBRS, STIBP and SSBD, the CPU's speculation controls. Yet every MSR the policy did not name read as 0 and swallowed writes. So the guest wrote `PRED_CMD` and `SPEC_CTRL` into nothing while its sysfs said it was mitigated, which is a failure that passes in silence.

[61256c8](https://github.com/irqlevel/nos/commit/61256c852980267fbb8b1a0d37112154e21376db) made the [policy](https://github.com/irqlevel/nos/blob/master/src/crates/hv/src/policy.rs) an allow-list. No speculation control is offered until its MSR is switched with the guest, that is, given the guest's value on entry and the host's on exit. And `hv-fuzz` checks every answer against a model of what each CPUID bit promises.

What follows is the world switch as it stands now: [`hvarch::x86::spec`](https://github.com/irqlevel/nos/blob/master/src/crates/hvarch/src/x86/spec.rs) and the two run stubs, written with KVM's as the reference.

## After every exit: registers and the return stack

Both run stubs are naked functions: pure assembly, with no prologue or epilogue from the compiler. After an exit, once the guest's registers are saved, the stub clears every register that the caller does not get back from the stub's own pops. A register that still holds a guest's value is a value the host may use speculatively, past a branch the CPU has not resolved yet.

Then, before the host's first `ret`, the stub refills the return stack buffer (RSB). The RSB predicts where a `ret` goes from recent call sites. After a guest has run, those call sites are the guest's, and the host's returns would be predicted to addresses the guest chose. The fill is KVM's `FILL_RETURN_BUFFER`. The stub makes 32 calls of its own, and each call jumps over an `int3` placed at its return address, so a return predicted there lands on a trap. The stack pointer is put back after every two calls, and an `lfence` follows the loop's last branch:

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

In AMD-V's [`vmrun_stub`](https://github.com/irqlevel/nos/blob/master/src/crates/hvarch/src/x86/svm.rs) this runs while GIF, AMD-V's global interrupt flag, is still clear. So nothing runs between `#vmexit` and the fill: no interrupt, not even an NMI. VT-x's [`vmx_launch_stub`](https://github.com/irqlevel/nos/blob/master/src/crates/hvarch/src/x86/vmx.rs) does the same with interrupts off, which is how an exit leaves them.

## Between two guests: an IBPB

An indirect branch prediction barrier (IBPB) makes the predictors forget what earlier code trained them with. Writing bit 0 of `IA32_PRED_CMD` issues one. nos does that before it enters a guest other than the last one entered on that CPU. Guests are told apart by the id of their nested page table. All the vCPUs of one guest share that id, so moving between them costs no barrier:

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

The `unsafe` here is sound because the `IBPB` flag is only ever set from CPUID's answer, read once when the extension is switched on. Asking CPUID at every entry would cost an exit of its own whenever nos itself runs under another hypervisor. [`hv-test.py`](https://github.com/irqlevel/nos/blob/master/scripts/hv-test.py) checks `hv`'s counters: at least two switches per built-in guest, each with an IBPB where the CPU has one. On the EX44's real hardware, one session issued 159 of them.

## The host's own branches: IBRS and its relatives

The refill and the barrier still leave the host's own indirect branches open to prediction from what a guest trained. On AMD, Zen 4 and later close that gap with automatic IBRS (bit 21 of `EFER`), and nos keeps it on for as long as the extension is on. The AX41's Zen 2 does not have it. The Intel half ([86deabd](https://github.com/irqlevel/nos/commit/86deabddfe35caef0024ac90188b857136e18161), merged in [48bfaf5](https://github.com/irqlevel/nos/commit/48bfaf53cb7da7cd18a16f641ae1874e6a669fe2)) is decided per CPU when the extension goes on there, from CPUID and `IA32_ARCH_CAPABILITIES`:

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

- **IBRS** in `IA32_SPEC_CTRL`, set while the extension is on. Enhanced IBRS (Ice Lake and later, including the EX44) is set once and holds. Legacy IBRS (Skylake, as in the Dell laptop) works as a barrier only at the moment it is written, so the stub writes it again after every exit, before the host's first indirect branch.
- **BHI_DIS_S** next to it, where the CPU has that control (Alder Lake and later). With it, the host's indirect branches are not predicted from the branch history a guest left behind.
- **VERW** as the last instruction before every entry, on CPUs where MDS, TAA, RFDS or MMIO stale data would let a guest sample what the host left in the CPU's internal buffers. Only its memory-operand form clears them, and the operand is a selector of 0.
- **An L1D flush** before every entry, which empties the L1 data cache, on CPUs where L1TF lets a guest read that cache (Skylake).

On the EX44's i5-13500 this comes to enhanced IBRS, BHI_DIS_S and VERW, with no L1D flush. VERW is there for RFDS, which affects its E-cores. That is also what Linux reports for this CPU.

## What it costs

We measured with `hv bench` on the EX44, on bare metal, at the cores' 2.5 GHz base clock. Each mitigation's share comes from a module built without it ([What an exit costs](/docs/hypervisor#what-an-exit-costs)):

| Cost of an exit, ns | P-core | E-core |
|---|---|---|
| without the mitigations | 1,690 | 2,065 |
| with all of them | 1,818 | 2,490 |
| return stack refilled with 2 entries, not 32 | -18 | -30 |
| without BHI_DIS_S | -22 | -287 |
| without VERW | -30 | -106 |
| without IBRS, and so without BHI_DIS_S | -50 | -303 |
| without any of the four | -97 | -431 |

That is 7% of an exit on a P-core and 20% on an E-core. On the E-core two thirds of it is BHI_DIS_S, and that cost is not in the switch at all. It is paid on every indirect branch the host takes while the extension is on, and Linux on the same machine pays it across its whole kernel. We kept the full 32-entry refill.

The first build measured worse, and the number pointed straight at the bug. The checks on entry went from 17 ns to 418, because each entry fetched three per-CPU records, and every `CpuLocal::with` (an accessor for per-CPU data) is a round of calls into the kernel. They are one record now, `Entries`, fetched once.

One number is still unexplained. On the first load of the module after a boot, exits on an E-core take 2.2 µs. BHI_DIS_S's share is missing from them, although `hv` reads the bit back as set.

## Making failures loud

A mitigation that someone else turned off is a failure that looks like success, so `hv` does not trust its own bookkeeping. It sends an IPI to each CPU where the extension is on, and the IPI reads `IA32_SPEC_CTRL` back. Where the register no longer holds what `hv on` set, `hv` prints a warning naming those CPUs: "something wrote it behind this module's back". `hv-test.py` fails if the warning appears.

The report itself had a bug of that kind. It read the flag that `on` keeps for guest entries, so before the first `hv on` it said the EX44 had no IBPB, even though its i5-13500 has one. [57a3c47](https://github.com/irqlevel/nos/commit/57a3c47a41b525b88deaaf9f0c340046d5e7b385) asks CPUID instead. Now, after `insmod` and before `hv on`, the EX44 says:

```text
hv: guests switched on a CPU 0 times, an IBPB between each two (0 issued)
```

A gate had the same kind of bug. Six scripts, `hv-test` among them, took their modules from `out/<arch>/modules` whatever the build. So the `UBSAN=1 RUSTUB=1` run that gated the merge of the AMD-V half ([19d0d14](https://github.com/irqlevel/nos/commit/19d0d142c81cafcf4d2b4eb742f5dc9606d6f24c)) ran the plain `hv.ko`, unless someone had copied the instrumented one over by hand. Since [de70e8d](https://github.com/irqlevel/nos/commit/de70e8d54308d8b54681a356a2b23c02cf7fd47a) the scripts read the build's flavour stamp.

## What the guest sees, and its neighbours

What a guest is told about its CPU decides how it protects itself. Until [d2b01bf](https://github.com/irqlevel/nos/commit/d2b01bfe150b27b2e564a036f7233e36d3364cea), CPUID leaf 7 was blank, and on Intel that tells Linux there is no `ARCH_CAPABILITIES`. So Alpine on the EX44 took its CPU for one open to everything. It ran page-table isolation without PCID, which means a TLB flush on every system call, and reported MDS and RFDS as "Vulnerable: no microcode".

Now `ARCH_CAPABILITIES` reads as the host's, except for bits that are not facts about the CPU. `IBRS_ALL` stays out, because `SPEC_CTRL` does not belong to the guest. With that, Alpine reports Meltdown, MDS and L1TF as "Not affected", and RFDS as "Mitigation: Clear Register File". Under all of this, nos now loads the machine's own microcode, which takes the EX44 from 0x3a to 0x3e ([8a6e8af](https://github.com/irqlevel/nos/commit/8a6e8afa0312174e6aa32bf74e2e42d289bbe931)). A mitigation is only as good as the microcode under it.

Hyper-threads are the other neighbourhood. Two threads of one core share its execution units, caches and buffers, and MDS, L1TF and port contention attacks all rely on that sharing. Since [7cceb6f](https://github.com/irqlevel/nos/commit/7cceb6fd6c869164707594c415a0becd9f7fdf26), a guest's cores are its own: whole cores where no other guest has a vCPU or a disk task ([`hv::place`](https://github.com/irqlevel/nos/blob/master/src/crates/hv/src/place.rs)). That has a price. On the AX41, two busy loops in a guest took 14.7 s on one core's two threads and 9.7 s on two cores. For a machine whose guests are all your own, there is `hv cores shared`.

## What is still open

The list lives in the hypervisor documentation, under [Before other people's guests](/docs/hypervisor#before-other-peoples-guests):

- **SRSO and Retbleed**, two attacks through return prediction, on Zen 1 to 4, which includes the AX41. They need either every `ret` in the host to go through an untrained return thunk, which is a compiler's job for the whole kernel, or an IBPB at every exit. (The IBPB at every exit is in now; see the update below.)
- **BTI (branch target injection) without automatic IBRS**, the AX41 again. It needs the host built with retpolines. (The same IBPB closes it now; see the update below.)
- **A guest's own speculation controls** (IBRS, STIBP, SSBD, IBPB). No guest is offered them until `SPEC_CTRL` is switched with the guest. Until then a guest relies on retpolines, and it stays exposed to speculative store bypass.
- **Branch history cleared in software**, for a CPU that has enhanced IBRS and is open to BHI but has no BHI_DIS_S: Ice Lake, Tiger Lake, or nos under QEMU 8.2's nested KVM, which hides the control. `hv` names such a CPU rather than pretend.
- **L1TF across hyper-threads.** The flush does not reach the sibling thread, and the host's own tasks, the network stack among them, still run on any core.
- **Paths never run on real hardware.** On AMD-V, these are the IBPB and automatic IBRS writes. TCG, QEMU's software CPU emulation, offers neither, and the AX41's Zen 2, which has IBPB, is where the IBPB write will run for the first time. (It has run there since, under KVM and on bare metal; see the update below.) On Intel, they are the Dell's Skylake paths: legacy IBRS after every exit, and the L1D flush.

Every mitigation that is in place has a line in `hv`'s output, a check in a gate and a number from the EX44. Every one that is not has a line on that list. The rest of the story is in [a month of the nos hypervisor](/blog/linux-under-nos-hypervisor) and on [the hypervisor page](/hypervisor).

## Update, 22 October 2026: an IBPB after every exit on AMD

The first two items of that list are now closed wherever the CPU's IBPB can close them, the AX41 included ([64ce5ca](https://github.com/irqlevel/nos/commit/64ce5ca922d40f7651e1b1a81582dedf433675cb), merged in [b235726](https://github.com/irqlevel/nos/commit/b235726b72db128f36fd84ad239578600001e6f5)). Without automatic IBRS, which no AMD CPU before Zen 4 has, a guest could steer the host's indirect branches, because this kernel has no retpolines. That is BTI: the guest trains the branch target buffer, the table where the CPU remembers where indirect branches went. On Zen 1 and 2 a guest could also steer the host's returns and its branches of every type (Retbleed), and on Zen 1 to 5 its returns (SRSO). Linux on the AX41 closes these with return thunks, retpolines and STIBP. nos has neither thunks nor retpolines, but it has only one way in from a guest: an exit. So that is where the fix sits.

As each CPU turns the extension on, `spec::amd_decide` works out what that CPU is open to from its vendor, family and CPUID, with Linux's list of which family is open to what (`cpu_vuln_blacklist`) as the reference. Then:

- **An IBPB right after every exit.** The run stub issues it as soon as the guest's registers are stored, before the return stack's refill and the host's first `ret`. It closes SRSO only where the IBPB flushes return predictions too: Zen 1 and 2's does, and a later CPU's does where CPUID says IBPB_BRTYPE. Linux does the same with `retbleed=ibpb` and `spec_rstack_overflow=ibpb-vmexit`. The IBPB between two guests described above is now issued on its own only where there is no IBPB after every exit.
- **STIBP** (single thread indirect branch predictors), a bit in `IA32_SPEC_CTRL` that keeps what one thread of a core trains from steering the other thread's indirect branches. It is set while the extension is on, wherever a core has two threads, because the IBPB clears only what a guest trained on this thread. `hv off` puts the register back, and `hv` reads it back from every CPU.
- **HLT and MWAIT always exit.** A thread that idles switches its core between SMT modes, and on Zen 1 and 2 a `ret` on the other thread may then be predicted from what the idling thread left in the return stack (SMT_RSB). When only the host idles a thread, what is left there is the host's own, refilled at the exit.

Where a CPU is open to something with nothing to close it (BTI or Retbleed without IBPB, SRSO without IBPB_BRTYPE, two threads without STIBP), `hv` says so in a line of its own. Under QEMU's TCG, which offers neither IBPB nor STIBP, it says the CPU is open to BTI and that a core's two threads share their predictors. On the AX41 under KVM, `hv-test.py` passed 71 checks, and `hv` said "an IBPB after every exit -- BTI, Retbleed, SRSO" (6,930 issued for 79 switches between the built-in guests) and "STIBP on". The test's machine now has cores of two threads, so under KVM on an AMD host the test also checks every CPU's STIBP.

The cost is real. Nested, with nos under KVM on the AX41, an exit went from 10.2 µs to 14.9, and all of the difference was the IBPB. On 23 October we booted nos on the AX41 itself, beside Ubuntu, and measured it on bare metal ([b2fe701](https://github.com/irqlevel/nos/commit/b2fe70110dff6a64fd67d7414f246dc5a2e76b10)): three builds of `hv.ko`, each loaded twice, with three runs of `hv bench cpu=3 exits=200000` per load, each the same to a few ns.

| An exit on the AX41, ns | nothing between exits | a read of 64 pages between |
|---|---|---|
| neither (the commit before) | 535–537 | 653–660 |
| STIBP alone | 537–540 | 647–703 |
| the IBPB after every exit, and STIBP | 1,543–1,545 | 1,723–1,763 |

STIBP costs nothing. The IBPB adds a microsecond, which makes an exit nearly three times what it was. A third of that microsecond is the barrier itself, in `vmrun` to `#vmexit` (330 ns became 687). The other two thirds are the host's own code after it, running on predictors the barrier has just emptied: the checks went from 93 ns to 113, putting the x87/SSE registers in from 8 to 50, and handling the exit and the loop from 117 to 682. Every built-in guest passes on bare metal with the IBPB on, on CPU 0 and on CPU 13.

What is still open, per [the hypervisor documentation](/docs/hypervisor#before-other-peoples-guests):

- **The cheaper way to the same end**: every `ret` of the host through an untrained return thunk and its indirect branches through retpolines, so that no IBPB is needed. That is a compiler's job for the whole kernel: C++, Rust and the modules.
- **A Zen 3 or 4 whose microcode has IBPB_BRTYPE but whose CPUID does not say so.** Linux finds that out by writing `PRED_CMD`; nos takes such a CPU for open to SRSO, and `hv` says so.
- **Zen 5's cheaper control of SRSO** (BpSpecReduce), and **TSA** (transient scheduler attacks) on Zen 3 and 4, which wants a VERW before every entry.
- **Automatic IBRS** (Zen 4 and later) has still run on no machine.
