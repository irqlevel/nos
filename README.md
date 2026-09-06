### nos

A hobby operating system kernel for x86-64 and arm64 (aarch64), written in C++20, Rust, and assembly.
Code is partially written by AI models (Claude, GPT) with human control over architecture decisions, testing, and code review.
Tested primarily in QEMU/KVM environments, including Google Cloud and Yandex Cloud VMs (MBR-based disk image, virtio devices), and on QEMU `virt` with HVF acceleration on Apple Silicon for the arm64 build. On x86-64 it boots under both legacy BIOS and UEFI. It also runs on real hardware — a laptop and two dedicated servers, one Intel and one AMD, see [Real hardware](docs/real-hardware.md) — though only those three machines have been tried, so anything beyond them is untested.

#### Highlights

- **Two architectures** — x86-64 (Multiboot2/GRUB, BIOS and UEFI) and arm64 (QEMU `virt`, Linux `Image` protocol) behind a HAL layer; both stay green in CI
- **SMP** — up to 64 CPUs, preemptive multitasking with per-CPU queues and load balancing, per-CPU tick, TLB shootdown via IPI
- **Memory** — 4-level paging, all of the machine's RAM up to 512 GiB, page and pool allocators, W^X kernel image
- **Interrupts** — IDT/IOAPIC/LAPIC and MSI-X on x86-64, GICv3 with ITS-delivered LPIs on arm64, IRQ balancing across CPUs
- **Drivers** — virtio blk/net/scsi/rng (virtio-pci and virtio-mmio), **NVMe in Rust**, Realtek RTL8168/RTL8125 and Intel I210 NICs in Rust, xHCI USB keyboard, 8042, serial, VGA text and pixel framebuffer consoles
- **Networking** — ARP, IPv4, ICMP, DHCP, DNS, UDP, TCP, an HTTP client, a [UDP remote shell](docs/udp-shell.md) and a [netconsole](docs/netconsole.md) that ships the kernel log over UDP
- **Storage** — async interrupt-driven block layer, MBR partitions, VFS with ramfs and the on-disk nanofs
- **Observability** — dmesg ring buffer, symbolized stack traces, a sampling profiler on the PMU, `top`, stack high-water marks, a lock watchdog

The complete list is in [docs/features.md](docs/features.md).

#### Quick start

```sh
./scripts/build-iso-docker.sh   # nos.iso + bin/kernel64.elf (or `make` with a native toolchain)
./scripts/qemu.sh               # boot the ISO in QEMU; serial console goes to nos.log
```

arm64:

```sh
make nocheck ARCH=aarch64       # kernel-arm64.elf + nos-arm64.img (in Docker on macOS)
./scripts/qemu-arm64.sh         # QEMU virt, HVF on Apple Silicon; serial goes to nos-arm64.log
```

The toolchain, the disk image for KVM clouds, and the BIOS/UEFI hybrid ISO are
described in [Build](docs/build.md); QEMU with KVM/TCG/OVMF and Google Cloud
deployment in [Run](docs/run.md).

#### Documentation

- [Features](docs/features.md) — everything the kernel does today, by subsystem
- [Build](docs/build.md) — native and Docker builds, arm64, the disk image, BIOS vs. UEFI
- [Run](docs/run.md) — QEMU on x86-64 and arm64, Google Cloud
- [Real hardware](docs/real-hardware.md) — Dell Latitude 5480 (UEFI, no serial, USB keyboard), Hetzner EX44 (20 CPUs, RTL8125 on the real internet) and Hetzner AX41 (AMD Ryzen, Intel I210, profiler on the PMU)
- [Debug](docs/debug.md) — GDB, boot and smoke tests, getting a log off a box with no serial port
- [Kernel parameters](docs/kernel-parameters.md) — boot-time options via GRUB or QEMU `-append`
- [UDP remote shell](docs/udp-shell.md) — `udpshell=PORT` + `scripts/udpsh.py`
- [Netconsole](docs/netconsole.md) — `netconsole=ip:port` + `scripts/netconsole.py`
- [Shell commands](docs/shell-commands.md) — the full command reference
- [Project layout](docs/project-layout.md) — where things live in `src/`

The roadmap — a bare-metal cloud node running Linux guests under a Rust
hypervisor, stages 0–5 — is in [plans/README.md](plans/README.md). Coding
conventions and the boot flow are in [CLAUDE.md](CLAUDE.md).
