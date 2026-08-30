### nos

A hobby operating system kernel for x86-64 and arm64 (aarch64), written in C++20, Rust, and assembly.
Code is partially written by AI models (Claude, GPT) with human control over architecture decisions, testing, and code review.
Tested primarily in QEMU/KVM environments, including Google Cloud and Yandex Cloud VMs (MBR-based disk image, virtio devices), and on QEMU `virt` with HVF acceleration on Apple Silicon for the arm64 build. On x86-64 it boots under both legacy BIOS and UEFI. It also runs on real hardware — see [Real hardware](#real-hardware) — though only one machine has been tried, so anything beyond it is untested.

#### Features

- **Two architectures** — x86-64 (Multiboot2/GRUB, ISO or MBR disk boot, **legacy BIOS and UEFI firmware**) and arm64 (QEMU `virt` board, Linux `Image` boot protocol); portable code goes through a HAL layer (`src/cpp/hal/`), arch backends live in `src/cpp/arch/`
- **SMP** — up to 8 CPUs; AP bootstrap via INIT/SIPI on x86-64, PSCI `CPU_ON` on arm64
- **Preemptive multitasking** — per-CPU task queues, round-robin scheduling, load-balanced task placement
- **Virtual memory** — 4-level paging (4 KB pages), high-half kernel at `0xFFFF800001000000`, TLB shootdown across CPUs via IPI
- **Page allocator** — fixed-size block allocator (1–128 contiguous pages), pool allocator (32 B – 2 KB), `new`/`delete` support
- **ACPI** — RSDP/RSDT/MADT parsing for LAPIC/IOAPIC discovery and IRQ→GSI routing
- **Interrupts** — IDT with exception handlers, IOAPIC routing (edge + level-triggered), LAPIC IPI, PIC (remapped then disabled)
- **arm64 port** — GICv3 interrupt controller with ITS (PCIe MSI delivered as LPIs, `its=on` by default), EL1 exception vectors, ARM generic timer (per-CPU), PL011 UART, FDT (device tree) parsing, PCIe ECAM, virtio-mmio transport, broadcast TLBI, semantic memory barriers (`dmb`) throughout; NVMe over ITS-delivered MSI works end-to-end
- **Drivers** — serial (COM1), screen console (EGA text on BIOS, 8x16 font on the bootloader's pixel framebuffer under UEFI), PIT (10 ms tick, SeqLock-protected counters), RTC (CMOS wall clock), PS/2 keyboard (8042), **USB HID keyboard over xHCI** (BIOS/UEFI handoff, root-port and hub enumeration, boot-protocol reports), PCI bus scan, LAPIC, IOAPIC, **virtio-blk**, **virtio-net**, **virtio-scsi**, **virtio-rng** (legacy + modern virtio-pci transport), **NVMe** (Rust, MSI-X interrupt-driven)
- **Block I/O** — asynchronous, interrupt-driven block request queue with DMA slot pool, `BlockRequest` submission with `WaitGroup` completion, direct DMA from caller buffers (page-aligned), virtqueue locking (`RawSpinLock`) for safe interrupt/task concurrency, early-boot polling fallback, SoftIrq-based retry for ring-full conditions, block device abstraction, MBR partition discovery
- **Networking** — virtio-net driver with asynchronous interrupt-driven TX/RX, software frame queues (256-entry TX/RX) in `NetDevice` base class, reference-counted `NetFrame` descriptors for zero-copy DMA, TX slot pool with bitmask allocation, SoftIrq-based TX retry and RX processing, IP routing (subnet mask + gateway from DHCP, off-subnet traffic forwarded to gateway), ARP (cache, request, reply, dump), IPv4/UDP transmit, ICMP echo (ping reply + send, per-type statistics), DHCP client with lease renewal (sets IP, subnet mask, gateway, DNS server), DNS resolver with 32-entry cache (A-record queries, name compression, DHCP-provided server), **TCP** (connection state machine, 3-way handshake, sequence/ack tracking, retransmission timers, MSS negotiation, send/receive ring buffers, graceful close with FIN exchange, RST handling, ephemeral port allocation, granular locking: `Mutex` for ports, `RawSpinLock` for pool and per-connection state, SoftIrq-driven timer processing), **HTTP client** (URL parsing, DNS resolution, TCP connection, request/response, redirect following for 301/302/303/307/308 with loop limit, `wget` shell command), UDP remote shell (execute kernel commands over the network), network device abstraction with per-protocol packet counters, `MacAddress`/`IpAddress` structs (IPv6-ready tagged union)
- **Filesystem** — VFS layer with mount points and path resolution, ramfs (in-memory), nanofs (on-disk filesystem with 4 KB blocks, superblock with UUID, inode/data bitmaps, CRC32 checksums for superblock/inodes/data, file and recursive directory deletion, persistent across remount)
- **Entropy** — `EntropySource` interface, `EntropySourceTable` registry, virtio-rng hardware random number generator
- **Power management** — ACPI S5 shutdown, keyboard controller reset/reboot
- **Interactive shell** — trace output suppressed during shell session (dmesg only), restored on shutdown; commands: `ps`, `cpu`, `bt <pid>`, `dmesg [filter]`, `uptime`, `date`, `memusage`, `pci`, `disks`, `diskread`, `diskwrite`, `irqstat`, `net`, `arp`, `icmpstat`, `tcpstat`, `udpsend`, `ping`, `nslookup`, `dnsflush`, `dhcp`, `wget`, `random`, `format`, `mount`, `umount`, `ls`, `cat`, `write`, `mkdir`, `touch`, `del`, `panic`, `version`, `cls`, `help`, `poweroff`, `reboot`
- **Timekeeping** — TSC calibration via PIT channel 2 (multi-round median), KVM paravirt clock (`kvmclock`) for accurate VM time, RTC wall clock, layered clock source selection (kvmclock → calibrated TSC → PIT fallback), `GetBootTime()` / `GetWallTimeSecs()` API
- **Kernel infrastructure** — spinlocks, mutexes, SeqLock (single-writer/multi-reader), atomics, wait groups, SoftIrq deferred processing, IPI tasks, timers, watchdog, stack traces with symbol resolution, dmesg ring buffer (512 KB, 2048 messages), panic handler with backtrace and CPU/task context, per-device interrupt statistics, AP startup diagnostics, virtual-to-physical address translation (4-level page table walk), byte-order helpers (`Htons`/`Htonl`/`Ntohs`/`Ntohl`)
- **Optimized stdlib** — `MemSet`, `MemCpy`, `MemCmp`, `StrLen`, `StrCmp`, `StrStr` implemented in x86-64 assembly using `rep stosq`/`rep movsq`/`repe cmpsb`/`repne scasb` (portable C versions on arm64)
- **Rust support** — `#![no_std]` Rust crates linked into the kernel via `staticlib`, FFI bridge (`rust_ffi.cpp`) exposing kernel services to Rust: spinlocks, mutexes, wait groups, timers, SoftIRQ, MSI-X interrupts, legacy interrupts, DMA allocation, MMIO mapping, PCI config space, block device and network device registration, CPU/IPI/task APIs. **kcore** library provides safe Rust wrappers around kernel primitives. **NVMe driver** written entirely in Rust — PCI BAR mapping, admin + I/O queue pairs, MSI-X interrupt-driven completion, WaitGroup-based synchronous I/O, multi-device support, proper RAII cleanup on shutdown
- **Boot tests** — allocator, btree, ring buffer, stack trace, multitasking, contiguous page alloc (up to 128 pages), parsing helpers, block device table, memset, memcpy, memcmp, strlen, strcmp, strstr

#### Build

The build is parameterized by `ARCH` (default `x86_64`, or `aarch64`); objects go to `out/$(ARCH)/`.

Native (requires clang, nasm, ld, grub-mkrescue with `xorriso` + `mtools`, and a **nightly**
rustup toolchain with the `rust-src` component — the Rust staticlib uses `-Z build-std`
to rebuild `core`/`alloc` with `-Ccode-model=large`; `src/rust/rust-toolchain.toml` pins it):

```sh
make
```

Via Docker (works on macOS / Apple Silicon):

```sh
./scripts/build-iso-docker.sh
```

This produces `nos.iso` and `bin/kernel64.elf` (for GDB symbols).

arm64 (requires `ld.lld`, `llvm-nm` and the same nightly Rust toolchain; on macOS build in Docker):

```sh
make nocheck ARCH=aarch64
```

This produces `kernel-arm64.elf` and `nos-arm64.img` (Linux `Image` format, bootable with QEMU `-kernel`).

Build a bootable qcow2 disk image (MBR, 2 partitions):

```sh
./scripts/build-disk.sh
```

This produces `nos.qcow2` (1 GB, MBR, virtio-blk compatible, suitable for KVM-based public clouds including Google Cloud Compute Engine).

#### Firmware: BIOS and UEFI

Both firmware flavours are supported by the same `nos.iso`: `grub-mkrescue`
writes a hybrid image whose El Torito catalog carries an `i386-pc` boot image
*and* an EFI system partition (`/efi/boot/bootx64.efi`), and the kernel then
adapts to whichever firmware it woke up under.

| | Legacy BIOS | UEFI |
|---|---|---|
| GRUB platform | `i386-pc` El Torito image + MBR boot code | ESP with `bootx64.efi` |
| Console | EGA text at `0xB8000` (`drivers/vga.cpp`) | GOP pixel framebuffer, 8x16 font (`drivers/fb_console.cpp`) |
| Keyboard | 8042 PS/2 | USB HID over xHCI (`drivers/usb/`); real UEFI laptops often have no 8042 at all |

The multiboot2 header asks GRUB for a framebuffer but marks both the console and
framebuffer tags optional, so BIOS boots keep legacy text mode while UEFI boots
get a linear framebuffer; `drivers/screen.cpp` picks the console at runtime from
what actually arrived, and `insmod all_video` in `build/grub.cfg` is what lets
GRUB set the mode at all.

Two limits worth knowing:

- The UEFI half of the ISO only exists if `grub-mkrescue` finds the
  `x86_64-efi` modules (`grub-efi-amd64-bin`) plus `mtools` — the FAT ESP is
  built with them — on the build host. The Docker builder image and the CI
  runner both install them, so `scripts/build-iso-docker.sh`, a native `make`
  and the release artifacts all produce the hybrid ISO; a bare host missing
  those two packages silently gets a BIOS-only ISO instead.
- `nos.qcow2` from `scripts/build-disk.sh` is MBR with `i386-pc` GRUB in the
  boot code — BIOS-only by design, since that is how the KVM clouds boot it.
  There is no ESP on that image.

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

UEFI boot (OVMF instead of the legacy BIOS — see
[Firmware: BIOS and UEFI](#firmware-bios-and-uefi)). There is no EGA text mode
under UEFI, so GRUB hands the kernel a pixel framebuffer and the screen is
drawn with the 8x16 font console; the PS/2 keyboard is emulated by QEMU, so
the shell is usable on screen as well as on serial (on a real UEFI laptop it
would come from the xHCI USB keyboard instead):

```sh
cp /usr/share/OVMF/OVMF_VARS_4M.fd /tmp/ovmf_vars.fd
qemu-system-x86_64 -smp 2 -m 1G -cdrom nos.iso -serial file:nos.log \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,unit=1,file=/tmp/ovmf_vars.fd
```

Boot from disk image (with virtio-blk):

```sh
./scripts/qemu-disk.sh
```

arm64 on QEMU `virt` (HVF-accelerated on Apple Silicon, `NOS_TCG=1` forces TCG; virtio-mmio blk/net/rng attached, serial goes to `nos-arm64.log`, UDP shell forwarded on `:9000`):

```sh
./scripts/qemu-arm64.sh
./scripts/smoke-arm64.sh              # boot smoke test (SMOKE_HVF=1 for HVF)
python3 scripts/udpsh.py 127.0.0.1    # remote shell over UDP
```

Deploy to Google Cloud Compute Engine (select **Skip OS adaptation** when importing the image — the kernel already has the necessary drivers and no guest agent):

```sh
# Upload disk image to a GCS bucket
gcloud storage cp nos.qcow2 gs://YOUR_BUCKET/nos.qcow2

# Create a Compute Engine image from the disk (skip OS adaptation)
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

#### Real hardware

`nos` boots on bare metal. Verified on one machine so far: a **Dell Latitude
5480** (Intel Core i5-6200U, Skylake-U / 100-series PCH, BIOS 1.16.0) booted
under UEFI. That laptop has no serial port and no 8042/PS/2 controller, yet the
kernel comes up on its own screen and gives an interactive shell driven by the
built-in USB keyboard.

That machine is the reason for four pieces of the tree, none of which QEMU
ever demanded:

- **Screen** — under UEFI there is no EGA text mode. GRUB hands the kernel the
  pixel framebuffer and `drivers/fb_console.cpp` draws the console with an 8x16
  font; without it a UEFI boot is simply blind.
- **Keyboard** — a real UEFI laptop often has no 8042 at all, so the emulated
  PS/2 controller QEMU and the KVM clouds provide is not there. The xHCI driver
  (`drivers/usb/`) enumerates a USB HID boot keyboard and publishes into the
  same `KeyboardInput` sink the 8042 driver uses. `usb=off` skips it.
- **Firmware leftovers** — LAPIC LVTs the firmware left armed are masked, and a
  stray interrupt is named instead of panicking anonymously. The TCO watchdog is
  located on 100-series PCH and left alone when the ACPI WDAT table says the
  firmware owns it, so it cannot reset the box mid-boot.
- **Diagnostics** — with no UART the screen is the only channel, so boot-path
  failures print through the screen console and the panic handler rather than
  `Trace` alone.

Other firmware, chipsets, NICs and disks are untested; treat bare-metal support
as "works on the machine it was debugged on".

#### Debug

Start QEMU with `-s` (GDB server on port 1234), then:

```sh
gdb -ex "symbol-file bin/kernel64.elf" \
    -ex "set architecture i386:x86-64" \
    -ex "target remote :1234"
```

arm64 (start `./scripts/qemu-arm64.sh -s`, needs `gdb-multiarch`):

```sh
./scripts/gdb-arm64.sh
```

#### Kernel parameters

Pass via GRUB command line on x86-64 (edit `build/grub.cfg`) or QEMU `-append` on arm64:

- `smp=off` — disable SMP, run on BSP only
- `console=serial` — direct shell output to serial port only
- `console=vga` — direct shell output to the screen only (VGA text or framebuffer)
- `dhcp=auto` — start DHCP on `eth0` automatically at boot
- `dhcp=off` — disable DHCP entirely (even via shell command)
- `dhcp=on` — enable DHCP only via shell command (default)
- `dns=on` — enable DNS resolver (uses DHCP-provided DNS server; requires `dhcp=auto`)
- `udpshell=PORT` — start UDP remote shell on the given port (e.g. `udpshell=9000`)
- `netconsole=ip:port` — stream the kernel log over UDP to that collector (e.g. `netconsole=10.0.2.2:6666`); needs an address, so pair it with `dhcp=auto`
- `its=off` — arm64 only: disable the GICv3 ITS and degrade PCIe MSI gracefully (default `its=on`; virtio-mmio devices don't need it)
- `usb=off` — x86-64 only: skip xHCI bring-up (no USB keyboard; the 8042 keyboard is unaffected)

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
- **Warning:** the UDP shell has no authentication — anyone who can reach the port has full kernel shell access. Use only for testing or behind a firewall.

#### Netconsole

With `netconsole=ip:port` the kernel ships its whole log to a UDP collector as
each line is produced -- the debugging channel of choice on a machine with no
serial port (a laptop over Wi-Fi, a cloud VM).

Every line the tracer emits is copied into a 128 KiB ring buffer the moment it
is produced, from any context including IRQ; a dedicated `netcon` task drains
the ring and sends whole lines in ~1400-byte datagrams. Nothing is lost while
the network is still coming up: the ring is primed from `dmesg` at boot and
keeps buffering until the device has an IP, then the entire backlog goes out
and live lines follow within a couple of milliseconds. When the ring fills, the
oldest lines are evicted and counted (`netconsole` shell command). Panics are
included -- the panic report is captured and flushed synchronously, best-effort
and only if the collector MAC is already in the ARP cache, since a blocking ARP
resolve could never complete with the other CPUs halted.

Receive with the included collector:

```sh
python3 scripts/netconsole.py                    # listen on 0.0.0.0:6666
python3 scripts/netconsole.py -p 5555 -o boot.log
```

It reassembles lines per sender, prefixes each with the host receive time, and
optionally appends to a file. Under QEMU user networking the host is `10.0.2.2`,
so `netconsole=10.0.2.2:6666 dhcp=auto` plus a collector on the host works with
no port forwarding (outbound UDP needs none).

The stream carries the kernel log (everything `Trace()` writes, which is what
`dmesg` holds), not the interactive shell's own console echo. Lines the
netconsole task itself produces are deliberately not captured -- feeding the TX
path's own traces back into the ring would make the drain loop generate its own
work forever. They still reach `dmesg` and the console.

**Warning:** the log is sent in the clear and to whoever holds the address; use
it on a network you trust.

#### Shell commands

| Command | Description |
|---------|-------------|
| `cls` | Clear screen |
| `cpu` | Dump CPU state |
| `dmesg [filter]` | Dump kernel log (optional substring filter) |
| `uptime` | Show uptime |
| `date` | Show wall clock date and time (RTC + boot time) |
| `ps` | Show tasks |
| `bt <pid>` | Dump stack trace of a task (uses IPI for remote CPUs) |
| `watchdog` | Show watchdog stats |
| `memusage` | Show memory usage |
| `pci` | Show PCI devices |
| `disks` | List block devices |
| `diskread <disk> <sector>` | Read and hex-dump a sector |
| `diskwrite <disk> <sector> <hex>` | Write hex data to a sector |
| `irqstat` | Show per-device interrupt counters |
| `help` | List commands |
| `net` | List network devices and per-protocol stats |
| `arp` | Show ARP table |
| `netconsole` | Show netconsole target, buffered bytes, drop/send counters |
| `icmpstat` | Show ICMP statistics |
| `tcpstat` | Show TCP connections and statistics |
| `wget <url>` | Fetch a URL via HTTP GET (follows redirects) |
| `udpsend <ip> <port> <msg>` | Send a UDP packet |
| `ping <ip\|hostname>` | Send 5 ICMP echo requests with RTT (resolves hostnames via DNS) |
| `nslookup <hostname>` | Resolve hostname to IP via DNS |
| `dnsflush` | Flush DNS cache |
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
| `usb` | Show xHCI controllers, connected root ports and keyboard report counters |
| `panic [type]` | Trigger kernel panic (direct, pagefault, divzero, ud) |
| `version` | Show kernel version |
| `poweroff` / `shutdown` | Power off (ACPI S5) |
| `reboot` | Reset system (keyboard controller) |

#### Project layout

```
src/cpp/
  hal/        Portable HAL contracts: cpu, barriers, mmu/pte, irqchip, console, power, context, irq stubs
  arch/
    x86_64/   Multiboot2 entry + AP trampoline (NASM), CPU primitives (asm.asm), IDT/GDT, exceptions, TSC/kvmclock, LAPIC/IOAPIC/PIC, PTE encoding, GRUB info parsing, HAL backends
    arm64/    Linux-Image boot + PSCI SMP (boot.S), EL1 vectors, GICv3 + ITS (LPIs for PCIe MSI), generic timer, PL011, FDT parser, PCIe ECAM, PTE encoding, HAL backends
  kernel/     Core: scheduling, tasks, interrupt dispatch, SoftIrq, shell, timers, timekeeping, locks, panic, Rust FFI bridge, symbol table
  drivers/    Hardware: serial, VGA text + framebuffer console (screen.cpp picks one), PIT, HPET, RTC, 8042, PCI, MSI-X, ACPI, virtio blk/net/scsi/rng (virtio-pci on x86-64, virtio-mmio on arm64)
    usb/      xHCI host controller (rings, contexts, root-port and hub enumeration) + HID boot-protocol keyboard
  block/      Block I/O: device abstraction, async request queue, MBR partition discovery
  net/        Networking: device abstraction, protocol headers, ARP, ICMP, DHCP, DNS, TCP, HTTP client, UDP shell, netconsole
  fs/         Filesystem: VFS, ramfs, nanofs, block I/O helpers
  mm/         Memory: page tables (4-level walk, VirtToPhys), page allocator, pool allocator
  lib/        Utilities: list, vector, btree, ring buffer, bitmap, CRC32 checksum, stdlib
  include/    Shared headers
src/rust/
  ffi/        Raw extern "C" FFI declarations for kernel services
  kcore/      Safe Rust wrappers: sync, DMA, MMIO, MSI-X, interrupts, timers, tasks, PCI, block/net device
  drivers/
    nvme/     NVMe block device driver (PCI, MSI-X, admin/IO queues)
    r8168/    Realtek r8168 network device driver
  hello/      Rust self-test module
  kernel/     Rust entry points (rust_main, rust_fini), global allocator
build/        Linker script, GRUB configs
scripts/      Build, run, debug, and GDB helpers
```
