# nos documentation

- [Features](features.md) — everything the kernel does today, by subsystem
- [Build](build.md) — native and Docker builds, arm64, the disk image, BIOS vs. UEFI on the ISO
- [Run](run.md) — QEMU on x86-64 (KVM, TCG, OVMF) and arm64 (HVF), Google Cloud
- [Real hardware](real-hardware.md) — the Dell Latitude 5480 laptop and the Hetzner EX44 (Intel) and AX41 (AMD) servers
- [Debug](debug.md) — GDB, boot tests and smoke tests, getting a log off a box with no serial port
- [Kernel parameters](kernel-parameters.md) — `smp=`, `maxcpus=`, `console=`, `dhcp=`, `udpshell=`, `netconsole=`, `loglevel=`, …

How it works, by subsystem:

- [Boot](boot.md) — GRUB/Multiboot2 and the Linux `Image` protocol to `boot: complete`, on both architectures, and how the APs come up
- [Paging and memory](paging.md) — the bootstrap linear map, the real page table, the free list, the allocators, MMIO and W^X, TLB shootdown
- [Scheduler](scheduler.md) — per-CPU run queues, the context switch, preemption, blocking and load balancing
- [Interrupts](interrupts.md) — the IDT and IOAPIC, MSI-X, GICv3 and the ITS, IPIs, NMI, IRQ balancing, deferred work
- [Profiler](profiler.md) — sampling on a performance counter or the tick, and how to read a `profile` report

Tools:

- [UDP remote shell](udp-shell.md) — run shell commands over the network with `scripts/udpsh.py`
- [Netconsole](netconsole.md) — stream the kernel log over UDP to `scripts/netconsole.py`
- [Shell commands](shell-commands.md) — the full command reference
- [Project layout](project-layout.md) — where things live in `src/`

The roadmap (bare-metal cloud node running Linux guests under a Rust
hypervisor, stages 0–5) is in [`plans/README.md`](../plans/README.md); coding
conventions and the boot flow are in [`CLAUDE.md`](../CLAUDE.md).
