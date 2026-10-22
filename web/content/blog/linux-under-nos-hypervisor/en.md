---
title: "From a CPU flag to Debian guests: a month of the nos hypervisor"
description: "Stage 3 from git log: in a month hv.ko went from switching AMD-V on to booting Alpine, Debian and multi-CPU Linux guests on AMD and Intel silicon."
date: 2026-10-21
tags:
  - hypervisor
  - rust
  - x86-64
  - hardware
  - qemu
  - modules
---

On 2026-09-22 the nos hypervisor was a loadable module that could turn the CPU's virtualization extension on and off. On 2026-10-21 the same module, `hv.ko`, boots Alpine and Debian as they ship and gives Linux guests several CPUs, virtio disks, a switched network with NAT, ACPI and an IO-APIC — under AMD-V on a Ryzen 5 3600 and under Intel VT-x on an i5-13500, both on bare metal. This is stage 3 of the [roadmap](https://github.com/irqlevel/nos/blob/master/plans/README.md), told from `git log`: what each step took, and what each new machine showed that the one before it could not.

## The shape, decided on day one

The first commit ([eec0894](https://github.com/irqlevel/nos/commit/eec0894a8e1acc3cf0196b457961e5bee269510f)) set out three pieces: `hvarch`, the CPU's extension — VMCB, VMCS, run stubs, control MSRs — and all of the hypervisor's `unsafe` (48 sites then, 195 at the Intel merge a month later); `hv`, everything above it, safe — guest memory and its nested page table, the devices, the Linux loader, the run loop (two `unsafe` sites then, four now); and `modules/hv`, the shell command and the tasks a guest's CPUs run on. `hv` and `hvarch` are not default workspace members, so a kernel built without the module has no hypervisor in it at all.

A module, because the state it takes belongs to the CPU — `EFER.SVME` and `VM_HSAVE_PA` on AMD, `CR4.VMXE` and VMX root operation on Intel — and a machine that runs no guests should never touch it. And because unloading is a gate: [`hv-test.py`](https://github.com/irqlevel/nos/blob/master/scripts/hv-test.py) turns the extension on for every CPU, unloads *without* turning it off, loads again and asks every CPU what it has. A control run with the off switch sabotaged failed six checks, which is how we knew the gate could fail at all.

AMD-V came first because development happens on a Mac, where the x86 kernel runs under QEMU's TCG, and TCG emulates SVM with nested paging and no VMX at all. It has holes of its own — before 9.2 it does not put a guest with paging off through the nested table — so the first built-in guests all started in long mode ([more on TCG](/blog/qemu-tcg-amd-v-quirks)).

## A month in one table

| Date | What landed |
|---|---|
| 2026-09-22 | the extension on and off per CPU; a VM and six built-in guests of a few bytes |
| 2026-09-23 | `hv boot`: a Linux early console, then a BusyBox shell running a typed `id`; the first real AMD-V; `hv start` and `hv attach`; a virtio disk and network |
| 2026-09-24 | Alpine and Debian as they ship; NAT, DHCP and DNS; the VT-x backend |
| 2026-09-25 | what nested KVM hid in the VT-x backend; an exit there from 17.9 to 5.8 µs |
| 2026-09-27 | guests of several CPUs; MSI-X |
| 2026-09-29 | ACPI; MMIO and the xAPIC; an IO-APIC; the hypervisor fuzzer |
| 2026-10-11/12 | a review's fixes, then its isolation section |
| 2026-10-20 | the first run on Intel silicon |
| 2026-10-21 | VT-x's halves of the review; the Intel round |

## A day to a shell

The first VM ([78fd761](https://github.com/irqlevel/nos/commit/78fd761d8c8017868b458a128386a6807db812ad)) ran guests of a handful of instructions: port I/O, a hypercall with all fifteen registers checked both ways, a write the nested table refuses, a triple fault, `cli; jmp $`. What keeps the host the host's is not left to policy: `hvarch` sets it on every entry, whatever the VMCB says — the host's interrupts, every port and MSR, `#DB` and `#AC` (a guest can make their delivery loop inside the CPU forever, CVE-2015-8104 and CVE-2015-5307), and `#MC`, raised again into the host's own handler.

Then Linux. The loader writes the zero page, the e820 map, page tables and a GDT through copying accessors — there is no Rust reference into guest memory anywhere, since the guest changes it under the compiler. The first boots hung in loops: 100,004 reads of the PIT's port 0x42 and 20,000 of the RTC's 0x71, waiting for a counter that never counted and an update bit that never cleared ([6688f58](https://github.com/irqlevel/nos/commit/6688f589081fee713444ac35a07d9dd6b686eccf)). With a PIT, an RTC, an 8259 pair and a timer tick, a tinyconfig 6.18 reached BusyBox — and its timer died a second later: an idle Linux runs `sti; hlt`, the HLT sits in the STI's interrupt shadow, and injection was refused 3.7 million times in a diagnostic ([ccbad38](https://github.com/irqlevel/nos/commit/ccbad3852ec970ff1561e9e2a570d3e668575d2d)). Then BusyBox's line editor asked where the cursor was (`ESC[6n`) and spun polling the port for an answer that was only fed at an idle HLT ([e0a2305](https://github.com/irqlevel/nos/commit/e0a2305b44da812165130f9e9ef1ee59e96b1c2f)). On 2026-09-23, the day after the first commit:

```text
$ hv boot /bzImage initrd=/initrd cmdline=console=ttyS0 nolapic rdinit=/init
  --- ttyS0 ---
[    0.000000] Linux version 6.18.53 ...
...
[    0.540000] Run /init as init process
nos-guest: init is up, / on ramfs, busybox v1.36.1
...
BusyBox v1.36.1 built-in shell (ash)
~ # id
uid=0 gid=0
```

All four demos of the [stage 3 plan](https://github.com/irqlevel/nos/blob/master/plans/03-hypervisor.md) were done. The next commit had an idle guest's HLT halt its vCPU instead of exiting some 46,000 times a second: over 120 s of the same boot, 5,633,131 exits became 76,334 ([aa8e263](https://github.com/irqlevel/nos/commit/aa8e263d7cc24dc5d238c4612a389f0ca247fa7c)).

## What real hardware said

The same day the guest ran on the AX41's Zen 2 — `Run /init` 18 ms into the guest's clock, against half a second under TCG — and stopped twice where TCG had never gone ([532219c](https://github.com/irqlevel/nos/commit/532219cec703a3b938ffe09dbdca461a29a0f0b1)). CPUID leaf 0x80000001 had passed through whole but for SVM, and a Zen 2 has MWAITX there: the guest's `udelay` became `MONITORX`, an intercept with no answer. And Linux on any Zen reads AMD's FCH reset-status register at 0xFED803C0, whether or not there is one. The leaf became an allow-list, an instruction CPUID did not offer became the `#UD` a CPU without it gives, and an unanswered read of the MMIO window now finds a read-only page of all ones — kept honest under TCG by a built-in guest, `absent`, that reads it.

The AX41 kept doing that. A guest that rebooted spun at 100% of its CPU: 1.3 million reads in 48 seconds of the status of an 8042 that is not there ([f58af03](https://github.com/irqlevel/nos/commit/f58af03dba389a8d07fcdfa697e9479e966be446)). Debian's clock counted 25 seconds out of 62: at 250 Hz, woken at the host's 100 Hz tick, its vCPU was handed one timer edge for every two or three periods. Every elapsed period is now owed and handed over one at a time, as KVM's PIT does, and `hv-distro-test.py` holds each guest's idle clock to nos's — 1.000 with the fix, 0.514 without ([768a56b](https://github.com/irqlevel/nos/commit/768a56b9ab428113a884c025c624157fab92c8cc)).

## Guests that stay up

`hv start` leaves a guest running; `hv list`, `exec`, `send`, `wait` and `stop` reach it, and `hv attach` gives its console live over `ssh -t` ([98100a6](https://github.com/irqlevel/nos/commit/98100a685f49497f2a28b3b54e2263bf47aa3670), [3aab12c](https://github.com/irqlevel/nos/commit/3aab12c9003ad10803a816e63fafa377430b6bd9)). Disks are legacy virtio-blk over PCI, reached by port I/O — still no instruction decoder — and the rings are the guest's: every index, address and length is checked before use ([cf32d7c](https://github.com/irqlevel/nos/commit/cf32d7c822d352b116097a61390c9142f5898395)). The network is virtio-net on a switch inside the module, nos's end a virtual NIC, `hv0`, in its own stack ([1f67325](https://github.com/irqlevel/nos/commit/1f673252321ed69aa81257d79513c8e6de71aef9)); next came NAT, a DHCP server on the switch and nos's DNS server for the guests ([45df223](https://github.com/irqlevel/nos/commit/45df223fb68845406afd8ad4097a9e2aa0c8aa95)).

Alpine's `virt` ISO then booted as it ships ([47568b5](https://github.com/irqlevel/nos/commit/47568b5320df5a9ff53ed86f7d15fc942a9fb55f)), needing what the purpose-built kernel never did: the PIT's one-shot mode for high-resolution timers; an RTC with the host's time — it had answered a fixed 2026-01-01, and OpenRC took every file on the ISO for one from the future; and a CPUID topology of one CPU, since the guest had taken the host CPU's APIC ID for its own. Debian 13's cloud image followed the same night, provisioned by `systemd.set_credential=` on its command line ([8928493](https://github.com/irqlevel/nos/commit/8928493243b0a186728bb5799125bbd7255940bd)). On the AX41 a cold `apt-get update` took 39 s and dropped some 1,400 frames, because a frame for a busy guest waited for the host's next tick; the switch now kicks a running vCPU out of its guest, as KVM does ([3c8bd36](https://github.com/irqlevel/nos/commit/3c8bd365f97c40dd3c086ea23095314fb46fc08c)).

## VT-x, and what nested KVM hid

The Intel backend arrived on 09-24 ([232a1d8](https://github.com/irqlevel/nos/commit/232a1d898dd880d1d50d3c3298301ae41d6dedec)), brought up under nested KVM on an Intel machine. One bit outside the hypervisor stood in its way: VMX requires `CR0.NE`, nos never set it, and `vmxon` would have faulted into a panic; `boot64.asm` now sets it on every CPU, and `hv on` checks the fixed bits first and names a CPU that refuses ([dc059db](https://github.com/irqlevel/nos/commit/dc059db4668c6c223dbfca79c4ea42ee604032f1)). After that, each fix answered a real kernel crashing:

- Linux booted whole, then oopsed at `RIP: 0x0` the moment it ran `/init`: userspace's first `SYSCALL` jumped to the host's `LSTAR`. VMX has no `vmsave`, so the syscall MSRs now go through the MSR-load lists ([0eb1531](https://github.com/irqlevel/nos/commit/0eb15313741d18bf91902773a8ea7d0eb6da4265)).
- Alpine double-faulted the first time an interrupt returned to user mode: `swapgs` changes `KERNEL_GS_BASE` with no `wrmsr` to intercept, so the guest's value is now stored back at every exit ([1d293b7](https://github.com/irqlevel/nos/commit/1d293b702d7514ac35a6aebb3f5b0402ad254e80)).
- Alpine took tens of minutes to its login prompt: sixty-odd `vmread`/`vmwrite` an exit, each a trap to the outer KVM. Synced lazily it took about 19 s ([495b58b](https://github.com/irqlevel/nos/commit/495b58b1c046cbd3ba12b2db1696aef19cd44abf)), and measured and thinned, an exit went from 17.9 µs to 5.8 ([0478d8b](https://github.com/irqlevel/nos/commit/0478d8bb85e860c151edd9294cf8fae9c141c4ba)).

A review against the manual then found what nested KVM covers up — every item passing the gates as they were ([e433276](https://github.com/irqlevel/nos/commit/e43327670e83cc0862d24a65efd8e9ce46995b3d)). The worst: CR8 was not intercepted, so a guest's `mov cr8, 15` wrote the host CPU's own task priority and kept every interrupt but NMIs off it, the TLB shootdown IPI included — a panic ten seconds later. A built-in guest, `tpr`, now checks that the host's CR8 is still 0 after a guest has written 15.

## The machine Linux expects

`cpus=N` ([14a03e0](https://github.com/irqlevel/nos/commit/14a03e0e23ab0f96e5063cbb8f32b829dd5ec374)) gives a guest up to 16 CPUs, each with an x2APIC, the others started as a PC starts them: INIT, then a start-up IPI into real mode. Linux promptly measured its APIC bus at 10,005 MHz and refused the timer — spinning in `calibrate_APIC_clock` it made no exits, then took the PIT's owed ticks in one burst; PAUSE exits fixed it. On the AX41 a guest's two CPUs on two CCXs saw their TSCs 2,052 cycles apart: on those AMD families LFENCE serializes only with a `DE_CFG` bit the firmware had left clear, which nos now sets at boot ([c94e584](https://github.com/irqlevel/nos/commit/c94e58437d7b43f663ea57afa87263c2b3650313)). MSI-X took a receive interrupt from six exits through the 8259 to one: a two-CPU Debian fetched a gigabyte three times at 117 MB/s without dropping a frame ([0f9480b](https://github.com/irqlevel/nos/commit/0f9480b1a082108d687e85a4e5d61bc873a64d77)).

On 09-29 came ACPI, whose DSDT must contain the PCI host bridge, because a Linux with ACPI never probes the bus without it ([bc9e15c](https://github.com/irqlevel/nos/commit/bc9e15c37d53f428bf48cbfb9a015ef2964f985d)) — and `hv stop` began by pressing the power button:

```text
$ hv stop 0 secs=300
hv: vm 0 stopped -- the guest powered itself off, S5 by its ACPI PM1 control register, at 0xffffffff8f1d5ae3
```

MMIO arrived the same day ([57c9826](https://github.com/irqlevel/nos/commit/57c9826d14753aff0d9bb0d1a7b1ce9f2e726e65)): the faulting instruction fetched through the guest's own page tables, decoded — only the `mov` forms Linux's MMIO accessors use, checked against clang's encodings of 1,054 instructions and two million random byte strings — and performed. The xAPIC page was its first device, an IO-APIC its second ([86a1721](https://github.com/irqlevel/nos/commit/86a1721359228cad00d174cf8aeb36f9c132d9fa)).

## Fuzzed, reviewed, isolated

That evening `hv-fuzz` compiled the hypervisor's own sources for the host and drove them — each device, the MMIO path, the loader, and the whole machine under the real run loop with a script in place of the CPU — and reported five findings, among them a disk status write whose address wrapped past 2^64 ([4ce4e77](https://github.com/irqlevel/nos/commit/4ce4e771b0f1f0d8cc2a662e0e10d1e913571b56)). A day later its `guestnet` target found, on its first input, that a guest could send as another and take its NAT flows; the switch now checks every frame where it enters ([d1aeecf](https://github.com/irqlevel/nos/commit/d1aeecf09004bba80ed5301eea6b25d60f1dc1d6); more in [the fuzzing post](/blog/fuzzing-the-kernel-on-the-host)).

On 10-10 a review of the whole hypervisor by five readers at once left a list, fixed a commit an item ([57347b1](https://github.com/irqlevel/nos/commit/57347b1fedf605b3459cdcab667c3288d519ee6e)), and then its isolation section ([19d0d14](https://github.com/irqlevel/nos/commit/19d0d142c81cafcf4d2b4eb742f5dc9606d6f24c)): guests had been able to reach nos's own sshd and hold its login slots, NAT let them reach a cloud's metadata address, and the CPUID policy told them they had speculation controls they did not have. The speculation part has [a post of its own](/blog/guest-speculation-isolation).

## Intel silicon, and the Intel round

On 2026-10-20 nos booted on the EX44 — an i5-13500, six P-cores of two threads and eight E-cores — from its [system image](/howto/install-on-a-server), beside Ubuntu. Every built-in guest passed on a P-core and on an E-core; an exit cost 1.67 µs and 2.07 µs, against 5.8 under nested KVM. Linux ran with one, two and four CPUs, a guest fetched a gigabyte through NAT in 9.1 s, and `rmmod` with five VMs running stopped them all and left VMX off on all twenty CPUs. The silicon also confirmed the VT-x gaps the review had listed; their fixes were merged the next day ([48bfaf5](https://github.com/irqlevel/nos/commit/48bfaf53cb7da7cd18a16f641ae1874e6a669fe2)).

The Intel round merged the same day ([3912397](https://github.com/irqlevel/nos/commit/39123972ca3197b653636c3373cf14594ac4b3ca)). Every core had been running at its 2.5 GHz base clock; with hardware P-states on, a guest's busy loop took 3.06 s where it had taken 5.55 (Linux on the box: 2.81). The first HWP version hung the EX44's boot with no console to show why, so `panic=N` now resets a machine N seconds after a panic's report. Guests are shown leaf 7 and ARCH_CAPABILITIES, so Alpine stopped believing its CPU open to Meltdown, and pause-loop exiting cut a two-CPU guest's 20-second boot from 299,508 exits to 99,297. The virtual APIC is in too, but opt-in (`apicv`): in a guest-to-guest TCP test it cut exits by 25 to 50% and still averaged 5.88 s against 5.70 without, because an interrupt for another vCPU still needs a kick that posted interrupts would remove.

## What is next

What is missing is listed in [the hypervisor doc](/docs/hypervisor#what-comes-next): device work off the first vCPU's task, posted interrupts, XSAVE and AVX for guests (a distribution built for x86-64-v3 does not start), modern virtio, the HPET and PCIe's ECAM; an arm64 backend that today reports and refuses, since nos drops EL2 at boot; and stage 4's control plane, which will put `hv start` behind an API. To try what exists, see [running a Linux guest](/howto/linux-guest), or [the hypervisor page](/hypervisor) for the whole picture.
