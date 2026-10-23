---
title: "From a CPU flag to Debian guests: the first month of the nos hypervisor"
description: "Stage 3 as told by git log: in a month hv.ko went from switching AMD-V on to booting Alpine, Debian and multi-CPU Linux guests on AMD and Intel hardware."
date: 2026-10-21
tags:
  - hypervisor
  - rust
  - x86-64
  - hardware
  - qemu
  - modules
---

On 22 September 2026 the nos hypervisor was a loadable module that could turn the CPU's virtualization extension on and off. On 21 October the same module, `hv.ko`, boots Alpine and Debian exactly as they ship. It gives Linux guests several CPUs, virtio disks, a switched network with NAT, ACPI and an IO-APIC. It does this under AMD-V on a Ryzen 5 3600 and under Intel VT-x on an i5-13500, both on bare metal. This post tells the story of stage 3 of the [roadmap](https://github.com/irqlevel/nos/blob/master/plans/README.md) from `git log`: what each step took, and what each new machine showed that the one before it could not.

## The structure, decided on day one

The very first commit ([eec0894](https://github.com/irqlevel/nos/commit/eec0894a8e1acc3cf0196b457961e5bee269510f)) split the hypervisor into three pieces. `hvarch` drives the CPU's extension: the VMCB and the VMCS (the structures that describe a guest CPU to AMD-V and to VT-x), the stubs that enter the guest, and the control MSRs. It also holds all of the hypervisor's `unsafe` code: 48 sites then, 195 at the Intel merge a month later. `hv` is everything above it: guest memory and its nested page table, the devices, the Linux loader and the run loop. It is safe code, apart from two `unsafe` sites then and four now. `modules/hv` holds the shell command and the tasks that a guest's CPUs run on. `hv` and `hvarch` are not default workspace members, so a kernel built without the module has no hypervisor in it at all.

Why a module? First, the state the hypervisor takes over belongs to the CPU: `EFER.SVME` and `VM_HSAVE_PA` on AMD, `CR4.VMXE` and VMX root operation on Intel. A machine that runs no guests should never touch it. Second, unloading becomes something a gate (a test that every change must pass) can check. [`hv-test.py`](https://github.com/irqlevel/nos/blob/master/scripts/hv-test.py) turns the extension on for every CPU, unloads the module *without* turning it off, loads it again and asks every CPU what state it is in. A control run with the off switch deliberately broken failed six checks. That is how we knew the gate could fail at all.

AMD-V came first because development happens on a Mac. There the x86 kernel runs under TCG, QEMU's software CPU emulator, which emulates SVM (AMD-V) with nested paging and has no VMX at all. TCG has gaps of its own: before version 9.2 it does not translate a guest that has paging off through the nested page table. So the first built-in guests all started in long mode ([more on TCG](/blog/qemu-tcg-amd-v-quirks)).

## A month in one table

| Date | What landed |
|---|---|
| 2026-09-22 | the extension switched on and off on each CPU; a VM and six built-in guests of a few bytes each |
| 2026-09-23 | `hv boot`: Linux's early console, then a BusyBox shell running a typed `id`; the first run on real AMD-V hardware; `hv start` and `hv attach`; a virtio disk and network |
| 2026-09-24 | Alpine and Debian as they ship; NAT, DHCP and DNS; the VT-x backend |
| 2026-09-25 | what nested KVM had hidden in the VT-x backend; an exit there cut from 17.9 to 5.8 µs |
| 2026-09-27 | guests with several CPUs; MSI-X |
| 2026-09-29 | ACPI; MMIO and the xAPIC; an IO-APIC; the hypervisor fuzzer |
| 2026-10-11/12 | fixes from a review, then the review's isolation section |
| 2026-10-20 | the first run on Intel hardware |
| 2026-10-21 | the VT-x halves of the review's fixes; the Intel round |

## One day to a shell

The first VM ([78fd761](https://github.com/irqlevel/nos/commit/78fd761d8c8017868b458a128386a6807db812ad)) ran guests just a handful of instructions long: port I/O, a hypercall that checks all fifteen registers in both directions, a write that the nested page table refuses, a triple fault, and `cli; jmp $`. What keeps the host in charge is not left to policy. `hvarch` sets the necessary intercepts on every entry, whatever the VMCB says. The host's interrupts cause exits, and so does every access to an I/O port or an MSR. So do `#DB` and `#AC`, because a guest can make the CPU loop forever delivering them (CVE-2015-8104 and CVE-2015-5307). So does `#MC`, the machine check, which is raised again into the host's own handler.

Then came Linux. The loader writes the zero page, the e820 memory map, the page tables and a GDT through accessors that copy the data. There is no Rust reference into guest memory anywhere, because the guest can change that memory behind the compiler's back. The first boots hung in loops. Linux made 100,004 reads of the PIT's port 0x42 and 20,000 of the RTC's port 0x71, waiting for a counter that never counted and an update bit that never cleared ([6688f58](https://github.com/irqlevel/nos/commit/6688f589081fee713444ac35a07d9dd6b686eccf)). With a PIT, an RTC, a pair of 8259 interrupt controllers and a timer tick, a tinyconfig 6.18 kernel reached BusyBox. A second later its timer died. An idle Linux runs `sti; hlt`, and the HLT sits in the STI's interrupt shadow (the one instruction after STI during which interrupts stay blocked). In one diagnostic run, injecting an interrupt was refused 3.7 million times ([ccbad38](https://github.com/irqlevel/nos/commit/ccbad3852ec970ff1561e9e2a570d3e668575d2d)). Next, BusyBox's line editor asked where the cursor was (`ESC[6n`) and spun, polling the port for an answer. But the answer was only fed to the guest when it went idle in HLT ([e0a2305](https://github.com/irqlevel/nos/commit/e0a2305b44da812165130f9e9ef1ee59e96b1c2f)). On 23 September, the day after the first commit:

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

With that, all four demos from the [stage 3 plan](https://github.com/irqlevel/nos/blob/master/plans/03-hypervisor.md) were done. The next commit made an idle guest's HLT halt its vCPU, instead of exiting some 46,000 times a second. Over 120 s of the same boot, the number of exits went from 5,633,131 to 76,334 ([aa8e263](https://github.com/irqlevel/nos/commit/aa8e263d7cc24dc5d238c4612a389f0ca247fa7c)).

## What real hardware showed

The same day the guest ran on the Zen 2 CPU of the AX41. It reached `Run /init` 18 ms into the guest's clock, against half a second under TCG. It also stopped twice in places TCG had never reached ([532219c](https://github.com/irqlevel/nos/commit/532219cec703a3b938ffe09dbdca461a29a0f0b1)). CPUID leaf 0x80000001 had been passed through whole except for SVM, and on a Zen 2 that leaf reports MWAITX. So the guest's `udelay` used `MONITORX`, which the hypervisor intercepted and had no answer for. And Linux on any Zen reads AMD's FCH reset-status register at 0xFED803C0, whether or not one exists. The fixes: the leaf became an allow-list, and an instruction that CPUID does not offer now gets the `#UD` that a CPU without it would raise. A read of the MMIO window that nothing answers now finds a read-only page of all ones. A built-in guest, `absent`, reads that page, so the TCG runs check it too.

The AX41 kept doing this. A guest that rebooted spun at 100% of its CPU: it read the status of a nonexistent 8042 keyboard controller 1.3 million times in 48 seconds ([f58af03](https://github.com/irqlevel/nos/commit/f58af03dba389a8d07fcdfa697e9479e966be446)). Debian's clock counted only 25 seconds out of 62. Its timer runs at 250 Hz, but its vCPU was woken at the host's 100 Hz tick and got one timer edge for every two or three periods. Now every elapsed period is owed and handed over one at a time, as KVM's PIT does. `hv-distro-test.py` checks each idle guest's clock against nos's: 1.000 with the fix, 0.514 without ([768a56b](https://github.com/irqlevel/nos/commit/768a56b9ab428113a884c025c624157fab92c8cc)).

## Guests that keep running

`hv start` starts a guest and leaves it running. `hv list`, `exec`, `send`, `wait` and `stop` work with it, and `hv attach` shows its console live over `ssh -t` ([98100a6](https://github.com/irqlevel/nos/commit/98100a685f49497f2a28b3b54e2263bf47aa3670), [3aab12c](https://github.com/irqlevel/nos/commit/3aab12c9003ad10803a816e63fafa377430b6bd9)). Disks are legacy virtio-blk over PCI, reached by port I/O, so there was still no need for an instruction decoder. The rings belong to the guest, so every index, address and length is checked before use ([cf32d7c](https://github.com/irqlevel/nos/commit/cf32d7c822d352b116097a61390c9142f5898395)). The network is virtio-net on a switch inside the module. On nos's side the switch connects to a virtual NIC, `hv0`, in nos's own network stack ([1f67325](https://github.com/irqlevel/nos/commit/1f673252321ed69aa81257d79513c8e6de71aef9)). Next came NAT, a DHCP server on the switch and nos's DNS server for the guests ([45df223](https://github.com/irqlevel/nos/commit/45df223fb68845406afd8ad4097a9e2aa0c8aa95)).

Then Alpine's `virt` ISO booted as it ships ([47568b5](https://github.com/irqlevel/nos/commit/47568b5320df5a9ff53ed86f7d15fc942a9fb55f)). It needed three things that our purpose-built kernel had never needed. One was the PIT's one-shot mode, for high-resolution timers. Another was an RTC with the host's time. Until then it had answered with a fixed 2026-01-01, so OpenRC thought every file on the ISO came from the future. The third was a CPUID topology of one CPU, because the guest had taken the host CPU's APIC ID for its own. Debian 13's cloud image followed the same night, provisioned by `systemd.set_credential=` on its command line ([8928493](https://github.com/irqlevel/nos/commit/8928493243b0a186728bb5799125bbd7255940bd)). On the AX41 a cold `apt-get update` took 39 s and dropped some 1,400 frames, because a frame for a busy guest waited for the host's next tick. Now the switch kicks a running vCPU out of its guest, as KVM does ([3c8bd36](https://github.com/irqlevel/nos/commit/3c8bd365f97c40dd3c086ea23095314fb46fc08c)).

## VT-x, and what nested KVM hid

The Intel backend arrived on 24 September ([232a1d8](https://github.com/irqlevel/nos/commit/232a1d898dd880d1d50d3c3298301ae41d6dedec)). We brought it up under nested KVM on an Intel machine. One bit outside the hypervisor stood in the way. VMX requires `CR0.NE`, and nos never set it, so `vmxon` would have faulted and ended in a panic. Now `boot64.asm` sets the bit on every CPU, and `hv on` first checks the bits that VMX requires in CR0 and CR4 and names any CPU that refuses ([dc059db](https://github.com/irqlevel/nos/commit/dc059db4668c6c223dbfca79c4ea42ee604032f1)). After that, each fix answered a crash of a real kernel:

- Linux booted all the way, then oopsed at `RIP: 0x0` the moment it ran `/init`. The first `SYSCALL` from userspace had jumped to the host's `LSTAR`, the MSR that holds the system call entry point. VMX has no `vmsave`, so the syscall MSRs now go through the MSR-load lists ([0eb1531](https://github.com/irqlevel/nos/commit/0eb15313741d18bf91902773a8ea7d0eb6da4265)).
- Alpine double-faulted the first time an interrupt returned to user mode. `swapgs` changes `KERNEL_GS_BASE` without a `wrmsr` that could be intercepted, so the guest's value is now saved back at every exit ([1d293b7](https://github.com/irqlevel/nos/commit/1d293b702d7514ac35a6aebb3f5b0402ad254e80)).
- Alpine took tens of minutes to reach its login prompt. Every exit ran sixty-odd `vmread`/`vmwrite` instructions, and each of them trapped to the outer KVM. With the state synced lazily, the boot took about 19 s ([495b58b](https://github.com/irqlevel/nos/commit/495b58b1c046cbd3ba12b2db1696aef19cd44abf)). After we measured the exit path and thinned it out, an exit went from 17.9 µs to 5.8 ([0478d8b](https://github.com/irqlevel/nos/commit/0478d8bb85e860c151edd9294cf8fae9c141c4ba)).

Then a review against the manual found what nested KVM covers up. Every item on its list had passed the gates as they were then ([e433276](https://github.com/irqlevel/nos/commit/e43327670e83cc0862d24a65efd8e9ce46995b3d)). The worst one: CR8 was not intercepted. A guest's `mov cr8, 15` wrote the host CPU's own task priority and blocked every interrupt except NMIs, including the TLB shootdown IPI. That means a panic ten seconds later. A built-in guest, `tpr`, now checks that the host's CR8 is still 0 after a guest has written 15.

## The machine Linux expects

`cpus=N` ([14a03e0](https://github.com/irqlevel/nos/commit/14a03e0e23ab0f96e5063cbb8f32b829dd5ec374)) gives a guest up to 16 CPUs, each with an x2APIC. The extra CPUs start the way a PC starts them: INIT, then a start-up IPI into real mode. Linux promptly measured its APIC bus at 10,005 MHz and refused the timer. While it spun in `calibrate_APIC_clock` it made no exits, and then it got all the PIT ticks it was owed in one burst. Exiting on PAUSE fixed it. On the AX41, a guest's two CPUs on two CCXs (core complexes) saw their TSCs 2,052 cycles apart. On those AMD families LFENCE is serializing only when a `DE_CFG` bit is set, and the firmware had left it clear. Now nos sets it at boot ([c94e584](https://github.com/irqlevel/nos/commit/c94e58437d7b43f663ea57afa87263c2b3650313)). MSI-X cut a receive interrupt from six exits through the 8259 to one. A two-CPU Debian fetched a gigabyte three times at 117 MB/s without dropping a frame ([0f9480b](https://github.com/irqlevel/nos/commit/0f9480b1a082108d687e85a4e5d61bc873a64d77)).

ACPI arrived on 29 September. Its DSDT must describe the PCI host bridge, because a Linux with ACPI never probes the bus without it ([bc9e15c](https://github.com/irqlevel/nos/commit/bc9e15c37d53f428bf48cbfb9a015ef2964f985d)). From then on, `hv stop` starts by pressing the power button:

```text
$ hv stop 0 secs=300
hv: vm 0 stopped -- the guest powered itself off, S5 by its ACPI PM1 control register, at 0xffffffff8f1d5ae3
```

MMIO arrived the same day ([57c9826](https://github.com/irqlevel/nos/commit/57c9826d14753aff0d9bb0d1a7b1ce9f2e726e65)). When a guest's access to device memory faults, the hypervisor fetches the faulting instruction through the guest's own page tables, decodes it and performs it. The decoder knows only the `mov` forms that Linux's MMIO accessors use. It was checked against clang's encodings of 1,054 instructions and against two million random byte strings. The xAPIC page was the first device on this path, and an IO-APIC the second ([86a1721](https://github.com/irqlevel/nos/commit/86a1721359228cad00d174cf8aeb36f9c132d9fa)).

## Fuzzed, reviewed, isolated

That evening came `hv-fuzz`. It compiles the hypervisor's own sources for the host and drives them: each device, the MMIO path, the loader, and the whole machine under the real run loop, with a script standing in for the CPU. It reported five findings, among them a disk status write whose address wrapped around past 2^64 ([4ce4e77](https://github.com/irqlevel/nos/commit/4ce4e771b0f1f0d8cc2a662e0e10d1e913571b56)). A day later its `guestnet` target found, on its first input, that a guest could send frames as another guest and take over its NAT flows. The switch now checks every frame where it enters ([d1aeecf](https://github.com/irqlevel/nos/commit/d1aeecf09004bba80ed5301eea6b25d60f1dc1d6); more in [the fuzzing post](/blog/fuzzing-the-kernel-on-the-host)).

On 10 October five reviewers read the whole hypervisor at once and left a list of problems. We fixed them one commit per item ([57347b1](https://github.com/irqlevel/nos/commit/57347b1fedf605b3459cdcab667c3288d519ee6e)), and then the list's isolation section ([19d0d14](https://github.com/irqlevel/nos/commit/19d0d142c81cafcf4d2b4eb742f5dc9606d6f24c)). Guests had been able to reach nos's own sshd and hold its login slots. NAT let them reach a cloud's metadata address. And the CPUID policy told them they had speculation controls they did not have. The speculation part has [a post of its own](/blog/guest-speculation-isolation).

## Intel hardware, and the Intel round

On 20 October nos booted on the EX44 from its [system image](/howto/install-on-a-server), installed beside Ubuntu. The EX44 has an i5-13500 with six P-cores of two threads each and eight E-cores. Every built-in guest passed on a P-core and on an E-core. An exit cost 1.67 µs on the P-core and 2.07 µs on the E-core, against 5.8 under nested KVM. Linux ran with one, two and four CPUs, a guest fetched a gigabyte through NAT in 9.1 s, and `rmmod` with five VMs running stopped them all and left VMX off on all twenty CPUs. The hardware also confirmed the VT-x gaps that the review had listed. Their fixes were merged the next day ([48bfaf5](https://github.com/irqlevel/nos/commit/48bfaf53cb7da7cd18a16f641ae1874e6a669fe2)).

The Intel round was merged the same day ([3912397](https://github.com/irqlevel/nos/commit/39123972ca3197b653636c3373cf14594ac4b3ca)). Until then every core had been running at its 2.5 GHz base clock. With hardware P-states (HWP) on, a busy loop in a guest took 3.06 s instead of 5.55; Linux running directly on the machine took 2.81. The first HWP version hung the EX44's boot, and there was no console to show why. So now `panic=N` resets the machine N seconds after a panic is reported. Guests now see CPUID leaf 7 and ARCH_CAPABILITIES, so Alpine no longer believes its CPU is open to Meltdown. Pause-loop exiting (an exit when a guest spins on PAUSE for too long) cut a two-CPU guest's 20-second boot from 299,508 exits to 99,297. The virtual APIC is in too, but it is opt-in (`apicv`). In a guest-to-guest TCP test it cut exits by 25 to 50%, yet the test still averaged 5.88 s against 5.70 without it. The reason is that an interrupt for another vCPU still needs a kick, which posted interrupts would remove.

## What is next

[The hypervisor doc](/docs/hypervisor#what-comes-next) lists what is still missing. That includes moving device work off the first vCPU's task, posted interrupts, XSAVE and AVX for guests (a distribution built for x86-64-v3 does not start), modern virtio, the HPET and PCIe's ECAM. The arm64 backend still only reports and refuses to run, because nos drops out of EL2 at boot. And stage 4's control plane will put `hv start` behind an API. To try what already exists, see [running a Linux guest](/howto/linux-guest), or [the hypervisor page](/hypervisor) for the whole picture.

> Update, 22 October 2026: posted interrupts were merged the day after this post. With them, interrupt delivery by the CPU itself became the default on every CPU that has posted interrupts. Then came a one-shot host timer, moderated receive interrupts, NIC offloads and mapped virtqueue rings, and with all of them the guest-to-guest TCP test above takes 2.72–2.83 s. We also tried moving a NIC's work to a task of its own: for a one-CPU guest it measured no faster, so we left it out. The full story is in [Guest networking from 5.7 seconds to 2.8](/blog/faster-guest-networking).
