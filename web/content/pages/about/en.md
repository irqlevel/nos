---
title: "What is nos"
description: "A hypervisor that runs on bare metal, on a kernel of its own in Rust and C++20. What it is made of, what it can do, where it runs and how it is tested."
---

nos is a hypervisor that you install straight onto a server. Under it is a kernel of its own, written in Rust, C++20 and a little assembly, with no libc, no STL and no C++ exceptions or RTTI. It has its own scheduler and memory manager, network stack, storage and filesystems, and it runs virtual machines on Intel VT-x and AMD-V. Several servers with nos can work as a cluster. VM disks are kept as copies on different servers, VMs on different servers can share one network, and a running VM can move from one server to another.

nos runs on x86-64 and arm64. On x86-64 it boots through GRUB from BIOS or UEFI. arm64 runs in QEMU, and virtual machines run on x86-64 only, for now. Besides QEMU/KVM and the clouds built on KVM, nos runs on the seven real machines listed below. That list is also the limit of what has been tried: other hardware hasn't been tested.

## The goal: safety

The rules every change follows are in the project's [`CLAUDE.md`](https://github.com/irqlevel/nos/blob/master/CLAUDE.md). Before it says how to build anything, it says what the kernel is for:

> This is being built to be as safe as a kernel can be made: no undefined behaviour, no memory corruption, no data races, no deadlocks, no resource leaks on error paths, no failure that passes in silence.

Safety isn't something tested for afterwards here. It decides every design and every line, and it comes before speed, brevity and getting a feature in sooner. The reason is what nos is for: a server that runs other people's virtual machines must never let a guest reach the host's memory. In practice it comes down to five habits:

- Of two designs, take the one where the bug can't be written at all. A lock owns the data it guards. A handle that comes from outside is looked up in a table, never dereferenced. No page of the kernel image is both writable and executable. The kernel never runs `.init_array`, so a global object with a constructor would be left as zeroes, and the linker refuses to build one ([the story](/blog/static-constructors-refused)).
- New code is written in Rust, with as little `unsafe` as the job allows. C++ gets a second read that looks for undefined behaviour and nothing else. Every return value is checked, every error path gives back what it took, and nothing allocates while holding a spinlock ([why](/blog/no-allocation-under-spinlock)).
- A failure must be loud. `BUG_ON` and `PANIC` catch broken invariants. If another CPU doesn't answer a TLB shootdown within ten seconds, the kernel panics and names that CPU instead of hanging the machine. Netconsole carries a panic's backtrace off a server that has no other console. As the project puts it, "a hang with no report is the worst outcome there is".
- A bug is a question about the design. Fixing it is half the job. The other half is closing off its whole kind with a type, a check at link time or a gate (a test script for one subsystem), so that the next bug like it doesn't compile, doesn't link or fails the gate.
- Speed and safety are measured, not guessed. "No change in behaviour" is shown by comparing the binaries and running the gates.

## How it is built

The source tree has two halves. C++ is the foundation: booting and bringing up each architecture, the hardware abstraction layer (HAL), page tables and allocators, the scheduler, locks, interrupts, timers, the core of the shell, the module loader, and the few drivers everything else stands on (serial, consoles, timers, the 8042 keyboard controller, PCI, MSI-X, ACPI, the VT-d IOMMU). Everything on top of that is Rust: the drivers for disks, network cards, USB and virtio, the block layer, the network stack, the filesystems, TLS, SSH, the random pool, the hypervisor and the cluster. The block, network and filesystem layers moved from C++ to Rust on 18 September 2026 ([how](/blog/cpp-to-rust-layers)).

```text
modules/      sshd, hv, api, cluster, nbs-mds, vnet ...  loadable .ko, Rust only
------------------------------------------------------------------------------
drivers/      block/ nvme ahci ata virtio_blk virtio_scsi   rng/ virtio_rng
              net/ igb e1000e r8125 r8168 virtio_net   usb/   virtio/
layers/       block   net   fs   tls   mgmt   ipc
common/       raft, nbsmeta, vmstate, ssh, web, extfmt ...  no kernel in them
base/         kcore: safe wrappers around kernel services; ffi: raw
              extern "C" declarations, nothing else; random, crypto
========================== C ABI: rust_ffi.cpp ===============================
C++ and asm   boot, hal/ over arch/x86_64 and arch/arm64, mm/, scheduler,
              interrupts, timers, locks, ACPI, PCI, MSI-X, VT-d, consoles,
              the shell's core, the module loader
```

A kernel service reaches Rust in three steps: a C++ function in `rust_ffi.cpp` (the bridge's only C++ file), its raw declaration in the `ffi` crate, and a safe wrapper in `kcore`, where a handle becomes a type with a `Drop`. Above that, the layers and drivers are ordinary Rust. Only 0.8% of the Rust lines are inside `unsafe` blocks, most of them in `kcore` and in `hvarch`, the part of the hypervisor that drives the CPU's virtualization extension. The rest sit at the C ABI, where a pointer and a length arrive from C++ or from a module. `scripts/unsafe-count.py` keeps the count.

Modules are for whatever a machine can boot and run without: a server, a load generator, a diagnostic tool. A module is a Rust crate built into a `.ko` file and loaded into the running kernel with `insmod`. The loader links it against an export table generated from the `ffi` declarations. It gives each segment the permissions its header asks for, but never writable and executable at once, and it refuses a module built against a different kernel interface. The SSH server, the host API, the cluster, NBS storage, vnet networks and the hypervisor itself are all modules.

The hypervisor is three crates. `hvarch` drives the CPU's virtualization extension and holds nearly all of the hypervisor's `unsafe`. `hv` holds everything above it: guest memory, the loop that handles guest exits, and the emulated devices. That's about 22,000 lines of code with 4 `unsafe` blocks. The `hv` module holds the shell commands, the VMs' folders, their disks and networks, and has no `unsafe` at all. All three are compiled only into `hv.ko`, so a kernel without the module has no hypervisor in it whatsoever.

The details are in [Rust in the kernel](/docs/rust) and [Project layout](/docs/project-layout).

## What it can do

The full list is in [Features](/docs/features). In short:

- Two architectures behind one HAL. x86-64 boots from an ISO or a disk, under BIOS or UEFI. arm64 runs on QEMU `virt` with a GICv3 interrupt controller, whose ITS delivers PCIe MSIs.
- Up to 64 CPUs, 48 of them tried on real hardware. Each CPU has its own run queue, with an EEVDF scheduler, preemption and load balancing. On a NUMA server the kernel keeps a task's memory, the pages it zeroes and a guest's memory on the node of the CPUs that use them.
- Memory: four-level paging and all of a machine's RAM, up to 512 GiB. The kernel image is W^X, and nothing mapped at run time is executable.
- Storage: an asynchronous block layer over NVMe, SATA (AHCI with NCQ, and IDE mode), virtio-blk and virtio-scsi, with MBR and GPT partitions, and a VFS with read-write ext4 and ext2, ramfs and procfs. The root filesystem can be chosen by device, label or UUID.
- Networking: ARP, IPv4, ICMP, UDP, TCP with SACK, DHCP, DNS, NAT, and an HTTPS client (TLS 1.3 and 1.2 through rustls, with certificates checked against the Mozilla roots). Drivers for virtio-net, Realtek RTL8168 and RTL8125, and Intel 82576, I210, I350 and 82574L.
- Getting into a server with no screen: an SSH server with Ed25519 keys, SFTP and scp; a UDP shell that stays off unless you ask for it; netconsole, which sends the kernel log over UDP; and a disk log for a boot that dies before its network is up.
- Observability: dmesg, backtraces with symbol names, tracepoints, a sampling profiler on the CPU's performance counters, `top`, a lock watchdog, and memsites, which shows who holds the Rust heap down to the line.
- Randomness: a ChaCha20 pool seeded from every entropy source the machine has, plus a seed kept on disk for machines that have none.
- The hypervisor: AMD-V and Intel VT-x. Guests get up to 16 CPUs, virtio disks and network cards, ACPI tables, an IO-APIC and emulated MMIO. Alpine, Debian and Ubuntu boot as they ship. `hv create` makes a VM from a cloud image's URL, and a VM kept in a folder survives a reboot. A guest can be given a host's PCI device whole, a GPU included, behind VT-d ([more](/hypervisor)).
- The cluster: a registry of hosts kept by raft; NBS, block storage that keeps every VM disk as copies on several hosts, with snapshots and clones; vnet, VM networks across hosts with a firewall on every VM; and live migration of a running VM to another host.
- Management: an HTTP API over mutual TLS, and a web panel that runs as a VM on the very host it manages ([how](/blog/management-panel)).
- Installing on a server: one command from a provider's rescue system, or the installer ISO. `nos-install` can also put nos on a spare partition next to the system already installed ([how](/howto/install-on-a-server)).

## Where it runs

| Where | What it is | What it brought |
|---|---|---|
| QEMU/KVM, Google Cloud, Yandex Cloud | The everyday target: the ISO, or the disk image `nos.qcow2` with virtio devices | Google Cloud imports the image with "Skip OS adaptation", because nos needs no guest agent |
| Dell Latitude 5480 | A Skylake-U laptop with UEFI, no serial port and no PS/2 controller | The pixel framebuffer console and the xHCI USB keyboard, the only console the laptop has |
| Hetzner EX44 | Intel i5-13500: 6 P-cores and 8 E-cores, 20 threads, a Realtek RTL8125 | Sparse APIC IDs, from 0 up to 62, which is why `MAX_CPUS` is 64. The first real Intel hardware VT-x ran on (20 October 2026) |
| Hetzner AX41 | AMD Ryzen 5 3600 (Zen 2), an Intel I210 | The `igb` driver, the profiler on AMD's counters, two IO-APICs, the first guest on real AMD-V |
| ASUS RS500-E6-PS4 | Two Xeon L5630 (Westmere-EP, 2010), 16 CPUs, legacy BIOS, SATA in IDE mode, an Intel 82574L, a network with no DHCP | The oldest CPU nos has run on, with no x2APIC, no AVX and no RDRAND. The `ata` and `e1000e` drivers, static addresses, a random seed kept on disk |
| Supermicro X11SSD-F | A Xeon E3-1230 v6 (Kaby Lake), UEFI, SATA behind the chipset's AHCI, Intel I350, a network with no DHCP | The `ahci` driver with NCQ, and the I350 in `igb` |
| MSI PRO B760M-P (Hertz) | A Core i3-14100 (Raptor Lake), UEFI, a Realtek RTL8168H, an NVIDIA RTX A2000, no BMC | GPU passthrough: VT-d with interrupt remapping, and the card in a guest running NVIDIA's own driver. The first real chip `r8168` ran on |
| Supermicro X10DRi (Newton) | Two Xeon E5-2670 v3 (Haswell-EP), 48 CPUs, 256 GiB in two NUMA nodes | NUMA: memory by node, and a guest's memory on its CPUs' node |

The two Hetzner servers have neither a serial port nor IPMI. So on the AX41, the network driver we were bringing up was also the only channel the kernel had for saying anything. Its I210 latched 10 Mb/s in the MAC while the PHY ran at 1000, and the card received nothing, even though the link, the receiver, the queue and the descriptors all looked healthy ([the hunt](/blog/i210-ten-megabits)). [Real hardware](/docs/real-hardware) describes each machine in detail.

## How it is tested

There is no test runner. The checks come in layers, from the ones that run most often to the ones that run least ([Tests and gates](/docs/testing) lists them all):

- Self-tests at every boot, in C++ and in Rust. One of them runs producers and consumers of the lockless ring on different CPUs. With the ring's sequence numbers made relaxed, it failed eight boots out of eight on real arm64 cores. TCG on an x86 host might never show that bug.
- Smoke boots. CI builds both architectures and boots each one without a screen. It waits for the serial markers `After test`, `Preempt is now on` and `boot: complete`, and fails on `PANIC:`. The x86 boot attaches virtio-blk, virtio-scsi, NVMe, virtio-net and virtio-rng.
- Builds that catch undefined behaviour. `UBSAN=1` runs the C++ under clang's undefined-behaviour sanitizer, and `RUSTUB=1` turns on the checks in Rust's core library for `unsafe` preconditions and for overflow. CI boots such a build on both architectures, and the first report is a panic. Among other things, the first runs found an arm64 boot stack that ran over the page tables below it, and a log line that was dropped instead of cut short, which had been swallowing whole panic messages.
- A gate per subsystem: about fifty scripts in `tests/`, each written for a failure that looks like success. One such failure is a filesystem driver that reads back its own writes, so it agrees with itself whatever offset it uses. Another is a frame pool that was never built, so every packet quietly fell back to the allocator. A gate is judged by its exit code, never by grepping a log ([how to run them](/howto/run-the-gates)).
- Fuzzers on the host. `hv-fuzz`, `net-fuzz`, `fs-fuzz`, `ipc-fuzz`, `nbs-fuzz` and `cpp-fuzz` compile the kernel's own sources for the host and feed them random input: a guest's exits, network traffic, disk images, cluster messages, firmware tables. Each piece is held to a model of what it should do, and the C++ runs under ASan and UBSan. CI runs the fuzzers with fixed inputs ([what they found](/blog/fuzzing-the-kernel-on-the-host)).
- Real guests, by hand. `hv-linux-test.py` and `hv-distro-test.py` boot real guest kernels and distributions under the hypervisor and check what the guests saw. `vm-migration-test.py` moves a VM between three nos hosts while it writes to its disk.

## Where it is going

The roadmap in [`plans/README.md`](https://github.com/irqlevel/nos/blob/master/plans/README.md) has six stages, numbered in the order they are being done:

| Stage | Outcome | Where it stands |
|---|---|---|
| 0 | nos boots on real servers and can be managed | Done. Seven machines, an address from DHCP or a static one, a shell and the log over the network, and an installer that puts nos on a new server with one command |
| 1 | New code in Rust, with as little `unsafe` as possible | A discipline rather than a milestone. Everything written since follows it |
| 2 | A HAL, and arm64 under QEMU | The arm64 port landed in July 2026, and both architectures have stayed green in CI since. Real arm64 hardware is put off |
| 3 | A Rust hypervisor that boots guests | In progress. Guests have SMP, disks, networking, ACPI, both VT-x and AMD-V, VMs made from a cloud image's URL, a host's GPU or NVMe passed through, and live migration between hosts. On arm64 the hypervisor only reports what EL2 offers |
| 4 | An HTTP API for VMs | In progress. The host API and the web panel create and delete VMs, show their consoles, and manage the cluster, its storage and its networks |
| 5 | Live update of the host kernel under running VMs | Not started. It needs all VM state kept as plain serializable data. The hypervisor has followed that rule from its first line, and live migration already moves that state between hosts |

## Where it came from

The first commit, `Init`, is dated 30 March 2017. Within days the kernel had an 8042 keyboard driver, an IDT, serial output and the PIT, and within weeks it had SMP and a round-robin scheduler. That year brought 133 commits and 2018 another 18, the last on 24 May 2018. Then nothing happened for nearly eight years.

Work picked up again on 9 February 2026, at a very different pace. Of the 1,316 commits on master as of 20 November 2026, 1,165 are from 2026: 275 in September, and 463 in November so far. Rust arrived on 28 March with an NVMe driver. The arm64 port printed its first line on 16 July. The laptop, the EX44 and the AX41 came between late August and early September. On 18 September the block, network and filesystem layers moved to Rust. The hypervisor was started on 22 September, and the next day it booted a guest kernel to a BusyBox shell. The fuzzers followed at the end of September, the system image on 17 October and the management panel on 28 October. November brought the cluster: NBS storage, vnet networks and live migration. Then came servers with SATA disks, a one-command installer, GPU passthrough and a two-socket NUMA server.

## How it is written

We write nos together with AI coding assistants, and the README says so plainly: most of the code is written by AI models (Claude, GPT), while a human stays in control of architecture decisions, testing and code review. The history shows it too: 1,020 of the 1,316 commits carry a `Co-Authored-By: Claude` trailer.

This works because of the same machinery that keeps the kernel honest. The rules are written down once, in `CLAUDE.md`. Every subsystem has a gate, judged by its exit code. Commit messages say what was checked: which gates passed, what was measured, and what couldn't be run. For a network driver that nothing emulates, the message says exactly that: "compiled, diffed, not run".

## License

nos is released under the MIT license ([`LICENSE`](https://github.com/irqlevel/nos/blob/master/LICENSE)). The source is at [github.com/irqlevel/nos](https://github.com/irqlevel/nos).

## Where to go next

- [Build it](/howto/build), then [boot it in QEMU](/howto/run-in-qemu) or [install it on a server](/howto/install-on-a-server).
- [The documentation](/docs): every subsystem, from boot to the scheduler to the gates.
- [The hypervisor](/hypervisor): guests under a Rust hypervisor that loads as a module.
- [The blog](/blog): stories from bring-up, debugging and design.
