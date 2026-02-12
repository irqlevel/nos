### nos

A hobby x86-64 operating system kernel written in C++14 and NASM.

#### Features

- **SMP** — up to 8 CPUs with INIT/SIPI AP bootstrap
- **Preemptive multitasking** — per-CPU task queues, round-robin scheduling, load-balanced task placement
- **Virtual memory** — 4-level paging (4 KB pages), high-half kernel at `0xFFFF800001000000`, TLB shootdown across CPUs via IPI
- **Page allocator** — fixed-size block allocator (1/2/4/8 pages), pool allocator (32 B – 2 KB), `new`/`delete` support
- **ACPI** — RSDP/RSDT/MADT parsing for LAPIC/IOAPIC discovery and IRQ→GSI routing
- **Interrupts** — IDT with exception handlers, IOAPIC routing (edge + level-triggered), LAPIC IPI, PIC (remapped then disabled)
- **Drivers** — serial (COM1), VGA text mode, PIT (10 ms tick), PS/2 keyboard (8042), PCI bus scan, LAPIC, IOAPIC, **virtio-blk**, **virtio-net** (modern virtio-pci 1.0 MMIO transport)
- **Block I/O** — virtio-blk driver with virtqueue DMA, block device abstraction, disk discovery and enumeration
- **Networking** — virtio-net driver, ARP (cache, request, reply), IPv4/UDP transmit, ICMP echo (ping reply + send), DHCP client with lease renewal, network device abstraction
- **Filesystem** — VFS layer with mount points, ramfs (in-memory filesystem with directories and files)
- **Power management** — ACPI S5 shutdown, keyboard controller reset/reboot
- **Interactive shell** — commands: `ps`, `cpu`, `dmesg`, `uptime`, `memusage`, `pci`, `disks`, `diskread`, `diskwrite`, `net`, `udpsend`, `ping`, `dhcp`, `mount`, `umount`, `ls`, `cat`, `write`, `mkdir`, `version`, `cls`, `help`, `poweroff`, `reboot`
- **Kernel infrastructure** — spinlocks, atomics, timers, watchdog, stack traces, dmesg ring buffer, panic handler
- **Boot tests** — allocator, btree, ring buffer, stack trace, multitasking, contiguous page alloc, parsing helpers, block device table

#### Build

Native (requires clang, nasm, ld, grub-mkrescue):

```sh
make
```

Via Docker (works on macOS / Apple Silicon):

```sh
./scripts/build-iso-docker.sh
```

This produces `nos.iso` and `bin/kernel64.elf` (for GDB symbols).

Build a bootable qcow2 disk image (MBR, 2 partitions):

```sh
./scripts/build-disk.sh
```

This produces `nos.qcow2` (1 GB, MBR, virtio-blk compatible, suitable for KVM-based public clouds).

#### Run

With KVM (Linux):

```sh
qemu-system-x86_64 -enable-kvm -smp 8 -cdrom nos.iso -serial file:nos.log \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
```

Without KVM (macOS with TCG):

```sh
qemu-system-x86_64 -smp 2 -cdrom nos.iso -serial file:nos.log -s -vga std
```

Boot from disk image (with virtio-blk):

```sh
./scripts/qemu-disk.sh
```

#### Debug

Start QEMU with `-s` (GDB server on port 1234), then:

```sh
gdb -ex "symbol-file bin/kernel64.elf" \
    -ex "set architecture i386:x86-64" \
    -ex "target remote :1234"
```

#### Kernel parameters

Pass via GRUB command line (edit `build/grub.cfg`):

- `smp=off` — disable SMP, run on BSP only
- `console=serial` — direct shell output to serial port only
- `console=vga` — direct shell output to VGA only
- `dhcp=auto` — start DHCP on `eth0` automatically at boot
- `dhcp=off` — disable DHCP entirely (even via shell command)
- `dhcp=on` — enable DHCP only via shell command (default)

#### Shell commands

| Command | Description |
|---------|-------------|
| `cls` | Clear screen |
| `cpu` | Dump CPU state |
| `dmesg` | Dump kernel log |
| `uptime` | Show uptime |
| `ps` | Show tasks |
| `watchdog` | Show watchdog stats |
| `memusage` | Show memory usage |
| `pci` | Show PCI devices |
| `disks` | List block devices |
| `diskread <disk> <sector>` | Read and hex-dump a sector |
| `diskwrite <disk> <sector> <hex>` | Write hex data to a sector |
| `help` | List commands |
| `net` | List network devices and stats |
| `udpsend <ip> <port> <msg>` | Send a UDP packet |
| `ping <ip>` | Send 5 ICMP echo requests with RTT |
| `dhcp [dev]` | Obtain IP address via DHCP |
| `mount ramfs <path>` | Mount a ramfs at path |
| `umount <path>` | Unmount filesystem |
| `mounts` | List mount points |
| `ls <path>` | List directory contents |
| `cat <path>` | Show file contents |
| `write <path> <text>` | Write text to file (creates if needed) |
| `mkdir <path>` | Create directory |
| `version` | Show kernel version |
| `poweroff` / `shutdown` | Power off (ACPI S5) |
| `reboot` | Reset system (keyboard controller) |

#### Project layout

```
boot/       Multiboot2 entry, 32→64-bit transition, AP trampoline
kernel/     Core: scheduling, tasks, interrupts, shell, timers, locks
drivers/    Hardware: serial, VGA, PIT, 8042, PCI, PIC, LAPIC, IOAPIC, ACPI, virtio-blk, virtio-net
net/        Networking: device abstraction, protocol headers, ARP, ICMP, DHCP
fs/         Filesystem: VFS, ramfs
mm/         Memory: page tables, page allocator, pool allocator
lib/        Utilities: list, vector, btree, ring buffer, bitmap, stdlib
build/      Linker script, GRUB configs
scripts/    Build, run, debug, and GDB helpers
```
