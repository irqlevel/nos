---
title: "What is nos"
description: "A hobby x86-64 and arm64 kernel in C++20 and Rust, built to be safe by design: it boots on clouds, Apple Silicon and bare metal, and runs Linux guests."
---

nos is a hobby operating system kernel for x86-64 and arm64, written in C++20, Rust and a little assembly. It is freestanding: no libc, no STL, no C++ exceptions or RTTI. On x86-64 it boots through GRUB and Multiboot2, under legacy BIOS and UEFI alike; on arm64 it boots through the Linux `Image` protocol. It runs under QEMU/KVM, on KVM-based clouds, on Apple Silicon under HVF, and on three real machines: a laptop, and two rented dedicated servers whose only console is the network.

It has its own SMP scheduler and memory manager, and on top of them a TCP/IP stack with HTTPS, an SSH server, ext2 read-write, NVMe and drivers for Realtek and Intel network cards. It loads kernel modules written in Rust, and one of those modules is a type-1 hypervisor that boots unmodified Linux distributions.

The roadmap in [`plans/README.md`](https://github.com/irqlevel/nos/blob/master/plans/README.md) heads for a bare-metal cloud node: one kernel that owns a server, runs Linux guests under its own Rust hypervisor, is managed over the network and can be updated under running guests, with no Linux host underneath. The roadmap itself calls this "an engineering project and a portfolio piece, not a product play", and the README is plain about its reach: only the machines listed below have been tried, and anything beyond them is untested.

## The goal: safe by design, and by code

The project's [`CLAUDE.md`](https://github.com/irqlevel/nos/blob/master/CLAUDE.md) holds the rules every change follows. Before it says how to build anything, it says what the kernel is for:

> This is being built to be as safe as a kernel can be made: no undefined behaviour, no memory corruption, no data races, no deadlocks, no resource leaks on error paths, no failure that passes in silence.

Safety here is not a property tested for afterwards but what every design and every line is chosen for, ranked above speed, brevity and getting a feature in sooner. The reason is where the project is going: the bug class a machine running other people's guests cannot survive is a guest reaching host memory. In practice the goal turns into five habits:

- **By design first.** Between two designs, take the one in which the bug cannot be written. A lock owns the data it guards. A handle from outside is looked up in a table, never dereferenced. The kernel image is W^X. The kernel runs no `.init_array`, so a global with a non-`constexpr` constructor would be left as zeroes, and both linker scripts refuse to link one ([the story](/blog/static-constructors-refused)).
- **By code.** New code is Rust, with as little `unsafe` as the job allows. C++ gets a second read that looks for undefined behaviour and nothing else. Every return value is checked, every error path releases what it took, and nothing allocates with a spinlock held ([why](/blog/no-allocation-under-spinlock)).
- **A failure must be loud.** `BugOn` and `Panic` catch broken invariants. A TLB shootdown that gets no answer panics after ten seconds and names the CPUs that stayed silent, rather than hanging the machine. Netconsole carries a panic's backtrace off a box that has no other console. In the project's words, "a hang with no report is the worst outcome there is".
- **A bug is a question about the design.** Fixing the instance is half the job. The other half is closing the class with a type, a link check or a gate, so that the next bug of its kind does not compile, does not link, or fails a gate.
- **Neither speed nor safety is a guess.** Performance claims are measured, and "no behaviour change" is shown by comparing binaries and running the gates, not asserted.

## How it is built

The tree has two halves with a hard line between them. **C++ is the fundamentals**: boot and the bring-up of each architecture, the HAL, page tables and allocators, the scheduler, locks, interrupt dispatch, timers, the core of the shell, the module loader, and the few drivers under everything (serial, consoles, timers, the 8042, PCI, MSI-X, ACPI). **Everything that sits on them is Rust**: the drivers of disks, network cards, USB and virtio, the block layer, the network stack, the filesystems, TLS, SSH, the random pool and the hypervisor. There is no `src/cpp/net`, `src/cpp/fs` or `src/cpp/block`; those layers moved to Rust on 18 September 2026 ([how](/blog/cpp-to-rust-layers)).

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

A kernel service reaches Rust in three steps: a C++ function in `rust_ffi.cpp` (the bridge's only C++ file), its raw declaration in the `ffi` crate, and a safe wrapper in `kcore`, where a handle becomes a type with a `Drop`. Above that the layers and drivers are ordinary Rust. Their remaining `unsafe` sits at the C ABI, where a pointer and a length arrive from C++ or a module, or is an `unsafe impl` saying a structure is plain words. `scripts/unsafe-count.py` keeps the count honest: on master in October 2026 it reports 68 sites in `net`, 14 in `block`, 47 in `fs` and 15 across the nine driver crates, with 268 in `kcore`.

**Modules** are how optional things ship: a server, a load generator, a diagnostic. A module is a Rust crate built into a `.ko` and loaded into a running kernel with `insmod`. The loader binds it against an export table generated from the `ffi` declarations, gives each segment the permissions its header asks for (never writable and executable at once), and refuses a module built against another kernel interface. The SSH server is a module; so is `netblk`, which serves an NVMe disk over UDP with zero copies; so is the hypervisor.

**The hypervisor** is three crates. `hvarch` holds the CPU's virtualization extension and nearly all of the hypervisor's `unsafe`: 195 sites. `hv` holds everything above it (guest memory, the exit loop, the emulated devices) in about 12,000 lines of code with 4 `unsafe` sites. `modules/hv` holds the shell commands and has none. All three are compiled only into `hv.ko`, so a kernel without the module contains no hypervisor at all.

The long form is in [Rust in the kernel](/docs/rust) and [Project layout](/docs/project-layout).

## What it has

The full list is in [Features](/docs/features). In short:

- **Two architectures** behind one HAL. x86-64 boots from an ISO or an MBR disk, under BIOS or UEFI. arm64 runs on QEMU `virt`, with GICv3 and PCIe MSIs delivered through the ITS.
- **SMP** for up to 64 CPUs, 20 of them exercised on real hardware: per-CPU run queues, preemption, load balancing, a per-CPU tick, TLB shootdown by IPI.
- **Memory**: four-level paging, all of a machine's RAM up to 512 GiB, page, pool and VA allocators, a W^X kernel image and NX on every runtime mapping.
- **Storage**: an asynchronous block layer over NVMe, virtio-blk and virtio-scsi, with MBR and GPT. A VFS with ext2 read-write, nanofs (the kernel's own checksummed, copy-on-write filesystem), ramfs and procfs; the root chosen by device, label or UUID.
- **Networking**: ARP, IPv4, ICMP, UDP, TCP, DHCP, DNS, NAT, and an HTTP client that speaks HTTPS through rustls (TLS 1.3 and 1.2, certificates checked against the Mozilla roots). Drivers for virtio-net, Realtek RTL8168 and RTL8125, and Intel 82576 and I210.
- **Ways into a machine with no screen**: the SSH server (Ed25519 keys); a UDP shell, off unless asked for; netconsole, the kernel log over UDP; and a disk log for a boot that dies before its network is up.
- **Observability**: dmesg, symbolized backtraces, a sampling profiler on the PMU, `top`, a lock watchdog.
- **Randomness**: a ChaCha20 pool seeded from every entropy source the machine has.
- **The hypervisor**: AMD-V and Intel VT-x; Linux guests of up to 16 CPUs with virtio disks and NICs, a switch with DHCP and NAT, ACPI tables, emulated MMIO and an IO-APIC. Alpine, Debian and Ubuntu boot as they ship ([more](/hypervisor)).
- **A system image for servers**: `nos-install` puts it on a partition beside the server's own Linux and boots nos once through the server's own GRUB ([Install](/docs/install)).

## Where it runs

| Where | What it is | What it brought |
|---|---|---|
| QEMU/KVM, Google Cloud, Yandex Cloud | The everyday target: the ISO, or the MBR disk image `nos.qcow2` with virtio devices | Google Cloud imports the image with "Skip OS adaptation": nos needs no guest agent |
| Apple Silicon | QEMU `virt` with HVF | The fast development loop, about four times as fast as TCG |
| Dell Latitude 5480 | A Skylake-U laptop under UEFI, with no serial port and no PS/2 controller | The pixel framebuffer console and the xHCI USB keyboard, which carry the whole session there |
| Hetzner EX44 | Intel i5-13500: 6 P-cores and 8 E-cores, 20 threads, a Realtek RTL8125 | APIC IDs as sparse as 0 to 62, which is why `MaxCpus` is 64. The first Intel silicon VT-x ran on, on 2026-10-20 |
| Hetzner AX41 | AMD Ryzen 5 3600 (Zen 2), an Intel I210 | The `igb` driver's bring-up, the profiler on AMD's counters, two IO-APICs, the first Linux guest on real AMD-V |

Neither server has a serial port or IPMI, so on the AX41 the NIC driver being brought up was also the only channel the kernel had to report through. Its I210 latched 10 Mb/s in the MAC while the PHY ran at 1000, and received nothing while link, receiver, queue and descriptors all read healthy ([the hunt](/blog/i210-ten-megabits)). [Real hardware](/docs/real-hardware) has each machine in detail.

## How it is tested

There is no test runner. Checking comes in layers, from the one that runs most often to the one that runs least ([Tests and gates](/docs/testing) has them all):

- **Self-tests at every boot**, in C++ and in Rust. One runs producers and consumers of the lockless ring on different CPUs: a ring whose sequence numbers were made relaxed failed eight boots out of eight under HVF, which TCG on an x86 host might never show.
- **Smoke boots.** CI builds both architectures and boots each headless, waiting for the serial markers `After test`, `Preempt is now on` and `boot: complete` and failing on `PANIC:`. The x86 boot attaches virtio-blk, virtio-scsi, NVMe, virtio-net and virtio-rng.
- **Builds that check for undefined behaviour.** `UBSAN=1` puts clang's sanitizer over the C++; `RUSTUB=1` turns on core's checks of `unsafe` preconditions and overflow. CI boots such a build on both architectures, and the first report is a panic. The first runs found, among other things, an arm64 boot stack overrunning the page tables below it, and a log line dropped instead of truncated, which had been swallowing panic messages whole.
- **A gate per subsystem**: 22 scripts, each written for a failure that looks like success. One is a filesystem driver that reads back its own writes and so agrees with itself whatever offset it uses. Another is a frame pool that was never built, so every packet silently fell back to the allocator. Gates are judged by exit code, never by grepping a log ([how to run them](/howto/run-the-gates)).
- **Fuzzers on the host.** `hv-fuzz`, `net-fuzz`, `fs-fuzz` and `cpp-fuzz` compile the kernel's own sources for the host, feed them random input (a guest's exits, network traffic, disk images, firmware tables) and hold each piece to a model of what it should do; the C++ runs under ASan and UBSan. CI runs them with fixed inputs ([what they found](/blog/fuzzing-the-kernel-on-the-host)).
- **Real Linux, by hand.** `hv-linux-test.py` and `hv-distro-test.py` boot real Linux kernels and distributions under the hypervisor and check what the guests saw.

## Where it is going

The roadmap has six stages, numbered in the order they are done:

| Stage | Outcome | Where it stands |
|---|---|---|
| 0 | nos boots and stays manageable on real server hardware | Its demo works: the EX44 and the AX41 take a DHCP lease, answer a shell over the network and stream their log, and a system image sets up a new server |
| 1 | New code in Rust, with `unsafe` confined | A discipline rather than a milestone, applied to everything since |
| 2 | A HAL, and arm64 under QEMU and HVF | The arm64 port landed in July 2026, and both architectures have stayed green in CI since. Real arm64 hardware is deferred |
| 3 | A Rust hypervisor boots Linux to a shell | Where the work is now. All four demos are done, and guests have since gained SMP, disks, a network, ACPI and VT-x. A code review's list comes before any guest belongs to someone else. On arm64 the backend only reports what EL2 offers |
| 4 | An HTTP control plane for VMs | Not started. The lifecycle it will serve exists as shell commands: `hv start`, `list`, `attach`, `stop` |
| 5 | Live update of the host kernel under running guests | Not started. Its constraint, all VM state kept as serializable plain data, has been honoured since the hypervisor's first line |

## Where it came from

The first commit, `Init`, is dated 30 March 2017. Within days the kernel had an 8042 keyboard driver, an IDT, serial output and the PIT; within weeks, SMP and a round-robin scheduler. That year brought 133 commits and 2018 brought 18 more, the last on 24 May 2018. Then nothing happened for nearly eight years.

Work resumed on 9 February 2026, at a different pace: of the 688 commits on master as of 21 October 2026, 537 are from 2026, 275 of them from September alone. Rust arrived on 28 March with an NVMe driver; arm64 printed its first line on 16 July; the laptop, the EX44 and the AX41 came between late August and early September. On 18 September the block, network and filesystem layers moved to Rust. The hypervisor started on 22 September and booted Linux to a BusyBox shell the next day; the fuzzers followed at the end of the month, the system image on 17 October.

## How it is written

We write nos together with AI coding assistants, and the README says so plainly: the code is partly written by AI models (Claude, GPT), while a human keeps control of architecture decisions, testing and code review. The log shows it too: 397 of the 688 commits carry a `Co-Authored-By: Claude` trailer.

The machinery that keeps the kernel honest is what makes this workable. The rules are written down once, in `CLAUDE.md`. Every subsystem has a gate judged by exit code. Commit messages say what was checked: which gates passed, what was measured, what could not be run. A NIC driver that nothing emulates, for instance, is recorded as "compiled, diffed, not run".

## License

nos is MIT-licensed ([`LICENSE`](https://github.com/irqlevel/nos/blob/master/LICENSE)); the source is at [github.com/irqlevel/nos](https://github.com/irqlevel/nos).

## Where to go next

- [Build it](/howto/build), then [boot it in QEMU](/howto/run-in-qemu), on x86-64 or an Apple Silicon Mac.
- [The documentation](/docs): every subsystem, from boot to the scheduler to the gates.
- [The hypervisor](/hypervisor): Linux guests under a Rust hypervisor loaded as a module.
- [The blog](/blog): stories from bring-up, debugging and design.
