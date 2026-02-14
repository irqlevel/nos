### nos

A hobby x86-64 operating system kernel written in C++20 and NASM.

#### Features

- **SMP** — up to 8 CPUs with INIT/SIPI AP bootstrap
- **Preemptive multitasking** — per-CPU task queues, round-robin scheduling, load-balanced task placement
- **Virtual memory** — 4-level paging (4 KB pages), high-half kernel at `0xFFFF800001000000`, TLB shootdown across CPUs via IPI
- **Page allocator** — fixed-size block allocator (1–128 contiguous pages), pool allocator (32 B – 2 KB), `new`/`delete` support
- **ACPI** — RSDP/RSDT/MADT parsing for LAPIC/IOAPIC discovery and IRQ→GSI routing
- **Interrupts** — IDT with exception handlers, IOAPIC routing (edge + level-triggered), LAPIC IPI, PIC (remapped then disabled)
- **Drivers** — serial (COM1), VGA text mode, PIT (10 ms tick), PS/2 keyboard (8042), PCI bus scan, LAPIC, IOAPIC, **virtio-blk**, **virtio-net**, **virtio-scsi**, **virtio-rng** (legacy + modern virtio-pci transport)
- **Block I/O** — virtio-blk driver with virtqueue DMA, block device abstraction, disk discovery and enumeration
- **Networking** — virtio-net driver, ARP (cache, request, reply, dump), IPv4/UDP transmit, ICMP echo (ping reply + send, per-type statistics), DHCP client with lease renewal, UDP remote shell (execute kernel commands over the network), network device abstraction with per-protocol packet counters, `MacAddress`/`IpAddress` structs (IPv6-ready tagged union)
- **Filesystem** — VFS layer with mount points and path resolution, ramfs (in-memory), nanofs (on-disk filesystem with 4 KB blocks, superblock with UUID, inode/data bitmaps, CRC32 checksums for superblock/inodes/data, file and recursive directory deletion, persistent across remount)
- **Entropy** — `EntropySource` interface, `EntropySourceTable` registry, virtio-rng hardware random number generator
- **Power management** — ACPI S5 shutdown, keyboard controller reset/reboot
- **Interactive shell** — trace output suppressed during shell session (dmesg only), restored on shutdown; commands: `ps`, `cpu`, `dmesg [filter]`, `uptime`, `memusage`, `pci`, `disks`, `diskread`, `diskwrite`, `net`, `arp`, `icmpstat`, `udpsend`, `ping`, `dhcp`, `random`, `format`, `mount`, `umount`, `ls`, `cat`, `write`, `mkdir`, `touch`, `del`, `version`, `cls`, `help`, `poweroff`, `reboot`
- **Kernel infrastructure** — spinlocks, mutexes, atomics, timers, watchdog, stack traces, dmesg ring buffer (512 KB, 2048 messages), panic handler, byte-order helpers (`Htons`/`Htonl`/`Ntohs`/`Ntohl`)
- **Optimized stdlib** — `MemSet`, `MemCpy`, `MemCmp`, `StrLen`, `StrCmp`, `StrStr` implemented in x86-64 assembly using `rep stosq`/`rep movsq`/`repe cmpsb`/`repne scasb`
- **Boot tests** — allocator, btree, ring buffer, stack trace, multitasking, contiguous page alloc (up to 128 pages), parsing helpers, block device table, memset, memcpy, memcmp, strlen, strcmp, strstr

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

This produces `nos.qcow2` (1 GB, MBR, virtio-blk compatible, suitable for KVM-based public clouds including Google Cloud Compute Engine).

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

Deploy to Google Cloud Compute Engine:

```sh
# Upload disk image to a GCS bucket
gcloud storage cp nos.qcow2 gs://YOUR_BUCKET/nos.qcow2

# Create a Compute Engine image from the disk
gcloud compute images create nos-image \
    --source-uri=gs://YOUR_BUCKET/nos.qcow2

# Launch a VM (serial console recommended)
gcloud compute instances create nos-vm \
    --image=nos-image \
    --machine-type=e2-small \
    --metadata=serial-port-enable=true

# Connect via serial console
gcloud compute connect-to-serial-port nos-vm
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
- `udpshell=PORT` — start UDP remote shell on the given port (e.g. `udpshell=9000`)

#### UDP remote shell

When `udpshell=PORT` is set, the kernel listens for commands on that UDP port.
A lightweight protocol header (16 bytes: magic, sequence number, chunk index, flags, payload length) frames every packet, enabling the client to validate replies, reassemble multi-chunk responses in order, and detect the end of a response without relying on timeouts.

Connect with the included Python client:

```sh
python3 scripts/udpsh.py <vm-ip> [port] [timeout]
```

- Default port is `9000`, default timeout is `30` seconds (long enough for blocking commands like `ping`).
- On protocol errors or timeouts, the client reconnects automatically and resets state.
- All shell commands work over the UDP session (including blocking ones like `ping`).

#### Shell commands

| Command | Description |
|---------|-------------|
| `cls` | Clear screen |
| `cpu` | Dump CPU state |
| `dmesg [filter]` | Dump kernel log (optional substring filter) |
| `uptime` | Show uptime |
| `ps` | Show tasks |
| `watchdog` | Show watchdog stats |
| `memusage` | Show memory usage |
| `pci` | Show PCI devices |
| `disks` | List block devices |
| `diskread <disk> <sector>` | Read and hex-dump a sector |
| `diskwrite <disk> <sector> <hex>` | Write hex data to a sector |
| `help` | List commands |
| `net` | List network devices and per-protocol stats |
| `arp` | Show ARP table |
| `icmpstat` | Show ICMP statistics |
| `udpsend <ip> <port> <msg>` | Send a UDP packet |
| `ping <ip>` | Send 5 ICMP echo requests with RTT |
| `dhcp [dev]` | Obtain IP address via DHCP |
| `random [len]` | Get random bytes as hex string |
| `format nanofs <disk>` | Format disk with nanofs |
| `mount ramfs <path>` | Mount a ramfs at path |
| `mount nanofs <disk> <path>` | Mount nanofs from disk at path |
| `umount <path>` | Unmount filesystem |
| `mounts` | List mount points |
| `ls <path>` | List directory contents |
| `cat <path>` | Show file contents |
| `write <path> <text>` | Write text to file (creates if needed) |
| `mkdir <path>` | Create directory |
| `touch <path>` | Create empty file |
| `del <path>` | Remove file or directory |
| `version` | Show kernel version |
| `poweroff` / `shutdown` | Power off (ACPI S5) |
| `reboot` | Reset system (keyboard controller) |

#### Project layout

```
boot/       Multiboot2 entry, 32→64-bit transition, AP trampoline
kernel/     Core: scheduling, tasks, interrupts, shell, timers, locks
drivers/    Hardware: serial, VGA, PIT, 8042, PCI, PIC, LAPIC, IOAPIC, ACPI, virtio-blk, virtio-net, virtio-scsi, virtio-rng
net/        Networking: device abstraction, protocol headers, ARP, ICMP, DHCP, UDP shell
fs/         Filesystem: VFS, ramfs, nanofs, block I/O helpers
mm/         Memory: page tables, page allocator, pool allocator
lib/        Utilities: list, vector, btree, ring buffer, bitmap, CRC32 checksum, stdlib
build/      Linker script, GRUB configs
scripts/    Build, run, debug, and GDB helpers
```
