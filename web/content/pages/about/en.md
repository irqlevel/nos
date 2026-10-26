---
title: "What is nos"
description: "A kernel for x86-64 and arm64 in C++20 and Rust, built to be safe by design. It runs in clouds, on Apple Silicon and on bare metal, and hosts Linux guests."
---

nos is an operating system kernel for x86-64 and arm64, written in C++20, Rust and a little assembly. It is freestanding: it uses no libc, no STL, and no C++ exceptions or RTTI. On x86-64 it boots through GRUB with Multiboot2, from either legacy BIOS or UEFI. On arm64 it boots through the Linux `Image` protocol. It runs under QEMU/KVM, on KVM-based clouds, on Apple Silicon under HVF, and on three real machines: a laptop and two rented dedicated servers. The servers have no console except the network.

nos has its own SMP scheduler and memory manager. On top of them sit a TCP/IP stack with HTTPS, an SSH server, read-write ext2, an NVMe driver, and drivers for Realtek and Intel network cards. The kernel loads modules written in Rust, and one of those modules is a type-1 hypervisor that boots unmodified Linux distributions.

The roadmap in [`plans/README.md`](https://github.com/irqlevel/nos/blob/master/plans/README.md) leads to a bare-metal cloud node: one kernel that owns a whole server, with no Linux host underneath. It runs Linux guests under its own Rust hypervisor, is managed over the network, and can be updated while the guests keep running. The roadmap itself calls this "an engineering project and a portfolio piece, not a product play". The README is just as plain about its limits: only the machines listed below have been tried, and anything else is untested.

## The goal: safe by design and by code

The project's [`CLAUDE.md`](https://github.com/irqlevel/nos/blob/master/CLAUDE.md) holds the rules that every change follows. Before it says how to build anything, it says what the kernel is for:

> This is being built to be as safe as a kernel can be made: no undefined behaviour, no memory corruption, no data races, no deadlocks, no resource leaks on error paths, no failure that passes in silence.

Safety here is not something tested for afterwards. Every design and every line of code is chosen for it, and it ranks above speed, brevity and shipping a feature sooner. The reason is where the project is headed. A machine that runs other people's guests cannot survive one class of bug: a guest reaching host memory. In practice the goal comes down to five habits:

- **Design first.** Of two designs, pick the one in which the bug cannot be written at all. A lock owns the data it guards. A handle that comes from outside is looked up in a table, never dereferenced. The kernel image is W^X, so no page of it is both writable and executable. The kernel never runs `.init_array`, so a global object with a non-`constexpr` constructor would be left as zeroes. Both linker scripts therefore refuse to link one ([the story](/blog/static-constructors-refused)).
- **Then the code.** New code is written in Rust, with as little `unsafe` as the job allows. C++ gets a second read that looks for undefined behaviour and nothing else. Every return value is checked, every error path releases what it took, and nothing allocates while holding a spinlock ([why](/blog/no-allocation-under-spinlock)).
- **A failure must be loud.** `BUG_ON` and `PANIC` catch broken invariants. A TLB shootdown asks the other CPUs to drop stale address translations. If no answer comes within ten seconds, the kernel panics and names the CPUs that stayed silent, instead of hanging the machine. Netconsole carries a panic's backtrace over the network, off a box that has no other console. In the project's words, "a hang with no report is the worst outcome there is".
- **A bug is a question about the design.** Fixing the bug itself is half the job. The other half is closing off its whole class with a type, a link-time check or a gate (a test script that guards one subsystem). Then the next bug of the same kind does not compile, does not link, or fails a gate.
- **Neither speed nor safety is a guess.** Performance claims are measured. "No behaviour change" is not just asserted: it is shown by comparing the binaries and running the gates.

## How it is built

The source tree has two halves with a hard line between them. **C++ covers the fundamentals**: booting and bringing up each architecture, the HAL (hardware abstraction layer), page tables and allocators, the scheduler, locks, interrupt dispatch, timers, the core of the shell, the module loader, and the few drivers that everything else stands on (serial, consoles, timers, the 8042, PCI, MSI-X, ACPI). **Everything built on top of them is Rust**: the drivers for disks, network cards, USB and virtio, the block layer, the network stack, the filesystems, TLS, SSH, the random pool and the hypervisor. There is no `src/cpp/net`, `src/cpp/fs` or `src/cpp/block` any more. Those layers moved to Rust on 18 September 2026 ([how](/blog/cpp-to-rust-layers)).

```text
modules/*     sshd, netblk, netload, blkload, hv ...   loadable .ko, Rust only
------------------------------------------------------------------------------
drivers/*     nvme, virtio_{blk,net,scsi,rng}, r8168, r8125, igb, usb
layers        block   net (over netwire)   fs   virtio   tls   random
kcore         safe wrappers around kernel services
ffi           raw extern "C" declarations, nothing else
========================== C ABI: rust_ffi.cpp ===============================
C++ and asm   boot, hal/ over arch/x86_64 and arch/arm64, mm/, scheduler,
              interrupts, timers, locks, ACPI, PCI, MSI-X, consoles,
              the shell's core, the module loader
```

A kernel service reaches Rust in three steps: a C++ function in `rust_ffi.cpp` (the only C++ file of the bridge), its raw declaration in the `ffi` crate, and a safe wrapper in `kcore`, where a handle becomes a type with a `Drop`. Above that, the layers and drivers are ordinary Rust. The `unsafe` they still have sits either at the C ABI, where a pointer and a length arrive from C++ or from a module, or in an `unsafe impl` stating that a structure is plain data. `scripts/unsafe-count.py` keeps the count honest. On master in October 2026 it reports 68 sites in `net`, 14 in `block`, 47 in `fs`, 15 across the nine driver crates, and 268 in `kcore`.

**Modules** are how optional things ship: a server, a load generator, a diagnostic tool. A module is a Rust crate built into a `.ko` file and loaded into a running kernel with `insmod`. The loader links it against an export table generated from the `ffi` declarations. It gives each segment the permissions its header asks for, but never writable and executable at once. It also refuses a module built against a different kernel interface. The SSH server is a module. So is `netblk`, which serves an NVMe disk over UDP with zero copies, and so is the hypervisor.

**The hypervisor** is three crates. `hvarch` drives the CPU's virtualization extension and holds nearly all of the hypervisor's `unsafe`: 203 sites. `hv` holds everything above it (guest memory, the loop that handles guest exits, the emulated devices): about 13,000 lines of code with 4 `unsafe` sites. `modules/hv` holds the shell commands and has no `unsafe` at all. All three are compiled only into `hv.ko`, so a kernel without the module contains no hypervisor whatsoever.

The details are in [Rust in the kernel](/docs/rust) and [Project layout](/docs/project-layout).

## What it has

The full list is in [Features](/docs/features). In short:

- **Two architectures** behind one HAL. x86-64 boots from an ISO or an MBR disk, under BIOS or UEFI. arm64 runs on QEMU `virt` with a GICv3 interrupt controller, whose ITS delivers PCIe MSIs.
- **SMP** for up to 64 CPUs, 20 of them tested on real hardware. Each CPU has its own run queue and its own tick, and there is preemption, load balancing, and TLB shootdown by IPI.
- **Memory**: four-level paging, all of a machine's RAM up to 512 GiB, and page, pool and virtual-address allocators. The kernel image is W^X, and every mapping made at run time is NX (not executable).
- **Storage**: an asynchronous block layer over NVMe, virtio-blk and virtio-scsi, with MBR and GPT partition tables. A VFS with read-write ext2, nanofs (the kernel's own checksummed, copy-on-write filesystem), ramfs and procfs. The root filesystem can be chosen by device, label or UUID.
- **Networking**: ARP, IPv4, ICMP, UDP, TCP, DHCP, DNS, NAT, and an HTTP client that speaks HTTPS through rustls (TLS 1.3 and 1.2, with certificates checked against the Mozilla roots). Drivers for virtio-net, Realtek RTL8168 and RTL8125, and Intel 82576 and I210.
- **Ways into a machine with no screen**: the SSH server (with Ed25519 keys); a UDP shell, which stays off unless you ask for it; netconsole, which sends the kernel log over UDP; and a disk log for a boot that dies before its network is up.
- **Observability**: dmesg, backtraces with symbol names, a sampling profiler on the PMU (the CPU's performance counters), `top`, and a lock watchdog.
- **Randomness**: a ChaCha20 pool seeded from every entropy source the machine has.
- **The hypervisor**: AMD-V and Intel VT-x. Linux guests get up to 16 CPUs, virtio disks and NICs, a switch with DHCP and NAT, ACPI tables, emulated MMIO and an IO-APIC. Alpine, Debian and Ubuntu boot as they ship. A VM kept in a folder outlasts a reboot, and `hv create` makes one from a cloud image's URL ([more](/hypervisor)).
- **A system image for servers**: `nos-install` writes it to a partition next to the server's own Linux, then boots nos once through the server's own GRUB ([Install](/docs/install)).

## Where it runs

| Where | What it is | What it brought |
|---|---|---|
| QEMU/KVM, Google Cloud, Yandex Cloud | The everyday target: the ISO, or the MBR disk image `nos.qcow2` with virtio devices | Google Cloud imports the image with "Skip OS adaptation", because nos needs no guest agent |
| Apple Silicon | QEMU `virt` with HVF | The fast development loop, about four times faster than TCG |
| Dell Latitude 5480 | A Skylake-U laptop with UEFI, no serial port and no PS/2 controller | The pixel framebuffer console and the xHCI USB keyboard, the only console the laptop has |
| Hetzner EX44 | Intel i5-13500: 6 P-cores and 8 E-cores, 20 threads, a Realtek RTL8125 | Sparse APIC IDs, from 0 up to 62, which is why `MAX_CPUS` is 64. The first real Intel hardware that VT-x ran on (2026-10-20) |
| Hetzner AX41 | AMD Ryzen 5 3600 (Zen 2), an Intel I210 | The bring-up of the `igb` driver, the profiler on AMD's counters, two IO-APICs, the first Linux guest on real AMD-V |

Neither server has a serial port or IPMI. So on the AX41, the network driver we were bringing up was also the only channel the kernel had for reporting anything. Its I210 latched 10 Mb/s in the MAC while the PHY ran at 1000. The card received nothing, even though the link, the receiver, the queue and the descriptors all looked healthy ([the hunt](/blog/i210-ten-megabits)). [Real hardware](/docs/real-hardware) describes each machine in detail.

## How it is tested

There is no test runner. Testing comes in layers, from what runs most often to what runs least ([Tests and gates](/docs/testing) lists them all):

- **Self-tests at every boot**, in C++ and in Rust. One of them runs producers and consumers of the lockless ring on different CPUs. With the ring's sequence numbers made relaxed, this test failed eight boots out of eight under HVF. TCG on an x86 host might never show the bug.
- **Smoke boots.** CI builds both architectures and boots each one headless. It waits for the serial markers `After test`, `Preempt is now on` and `boot: complete`, and fails on `PANIC:`. The x86 boot attaches virtio-blk, virtio-scsi, NVMe, virtio-net and virtio-rng.
- **Builds that check for undefined behaviour.** `UBSAN=1` runs the C++ under clang's undefined-behaviour sanitizer. `RUSTUB=1` turns on the checks in Rust's core library for `unsafe` preconditions and for overflow. CI boots such a build on both architectures, and the first report is a panic. Among other things, the first runs found an arm64 boot stack that overran the page tables below it. They also found a log line that was dropped instead of truncated, which had been swallowing whole panic messages.
- **A gate per subsystem**: 22 scripts, each written for a failure that looks like success. One such failure is a filesystem driver that reads back its own writes, so it agrees with itself whatever offset it uses. Another is a frame pool that was never built, so every packet silently fell back to the allocator. A gate is judged by its exit code, never by grepping a log ([how to run them](/howto/run-the-gates)).
- **Fuzzers on the host.** `hv-fuzz`, `net-fuzz`, `fs-fuzz` and `cpp-fuzz` compile the kernel's own sources for the host and feed them random input: a guest's exits, network traffic, disk images, firmware tables. Each piece is held to a model of what it should do, and the C++ runs under ASan and UBSan. CI runs the fuzzers with fixed inputs ([what they found](/blog/fuzzing-the-kernel-on-the-host)).
- **Real Linux, by hand.** `hv-linux-test.py` and `hv-distro-test.py` boot real Linux kernels and distributions under the hypervisor and check what the guests saw.

## Where it is going

The roadmap has six stages, numbered in the order they are being done:

| Stage | Outcome | Where it stands |
|---|---|---|
| 0 | nos boots on real server hardware and stays manageable | The demo works: the EX44 and the AX41 get an address over DHCP, answer a shell over the network and stream their log, and a system image sets up a new server |
| 1 | New code in Rust, with `unsafe` confined to a few places | A discipline rather than a milestone, and everything written since follows it |
| 2 | A HAL, and arm64 under QEMU and HVF | The arm64 port landed in July 2026, and both architectures have stayed green in CI ever since. Real arm64 hardware is deferred |
| 3 | A Rust hypervisor boots Linux to a shell | The current stage. All four demos are done, and since then guests have gained SMP, disks, networking, ACPI and VT-x, folders that outlast a reboot and VMs made from a cloud image's URL. Before any guest can belong to someone else, the list from a code review has to be worked through. On arm64 the backend only reports what EL2 offers |
| 4 | An HTTP control plane for VMs | Not started. The VM lifecycle it will serve already exists as shell commands: `hv start`, `list`, `attach`, `stop` |
| 5 | Live update of the host kernel under running guests | Not started. It requires all VM state to be kept as plain serializable data, and the hypervisor has followed that rule since its first line |

## Where it came from

The first commit, `Init`, is dated 30 March 2017. Within days the kernel had an 8042 keyboard driver, an IDT, serial output and the PIT. Within weeks it had SMP and a round-robin scheduler. That year brought 133 commits, and 2018 brought 18 more, the last on 24 May 2018. Then nothing happened for nearly eight years.

Work resumed on 9 February 2026, at a very different pace. Of the 688 commits on master as of 21 October 2026, 537 are from 2026, and 275 of those are from September alone. Rust arrived on 28 March with an NVMe driver. The arm64 port printed its first line on 16 July. The laptop, the EX44 and the AX41 came between late August and early September. On 18 September the block, network and filesystem layers moved to Rust. Work on the hypervisor started on 22 September, and the next day it booted Linux to a BusyBox shell. The fuzzers followed at the end of the month, and the system image on 17 October.

## How it is written

We write nos together with AI coding assistants, and the README says so plainly. Part of the code is written by AI models (Claude, GPT), while a human stays in control of architecture decisions, testing and code review. The commit log shows it too: 397 of the 688 commits carry a `Co-Authored-By: Claude` trailer.

This works because of the same machinery that keeps the kernel honest. The rules are written down once, in `CLAUDE.md`. Every subsystem has a gate, judged by its exit code. Commit messages say what was checked: which gates passed, what was measured, and what could not be run. For a NIC driver that nothing emulates, for instance, the commit message says "compiled, diffed, not run".

## License

nos is released under the MIT license ([`LICENSE`](https://github.com/irqlevel/nos/blob/master/LICENSE)). The source is at [github.com/irqlevel/nos](https://github.com/irqlevel/nos).

## Where to go next

- [Build it](/howto/build), then [boot it in QEMU](/howto/run-in-qemu), on x86-64 or on an Apple Silicon Mac.
- [The documentation](/docs): every subsystem, from boot to the scheduler to the gates.
- [The hypervisor](/hypervisor): Linux guests under a Rust hypervisor that loads as a module.
- [The blog](/blog): stories from bring-up, debugging and design.
