### nos

A hobby operating system kernel for x86-64 and arm64 (aarch64), written in C++20, Rust, and assembly.
Code is partially written by AI models (Claude, GPT) with human control over architecture decisions, testing, and code review.
Tested primarily in QEMU/KVM environments, including Google Cloud and Yandex Cloud VMs (MBR-based disk image, virtio devices), and on QEMU `virt` with HVF acceleration on Apple Silicon for the arm64 build. On x86-64 it boots under both legacy BIOS and UEFI. It also runs on real hardware — a laptop and a dedicated server, see [Real hardware](#real-hardware) — though only those two machines have been tried, so anything beyond them is untested.

#### Features

- **Two architectures** — x86-64 (Multiboot2/GRUB, ISO or MBR disk boot, **legacy BIOS and UEFI firmware**) and arm64 (QEMU `virt` board, Linux `Image` boot protocol); portable code goes through a HAL layer (`src/cpp/hal/`), arch backends live in `src/cpp/arch/`
- **SMP** — up to 64 CPUs (20 exercised on real hardware); AP bootstrap via INIT/SIPI on x86-64, PSCI `CPU_ON` on arm64, APs started one at a time, `maxcpus=N` to cap them
- **Preemptive multitasking** — per-CPU task queues, round-robin scheduling, load-balanced task placement
- **Virtual memory** — 4-level paging (4 KB pages), high-half kernel at `0xFFFF800001000000`, TLB shootdown across CPUs via IPI
- **Page allocator** — fixed-size block allocator (1–128 contiguous pages), pool allocator (32 B – 2 KB), `new`/`delete` support
- **ACPI** — RSDP/RSDT/MADT parsing for LAPIC/IOAPIC discovery and IRQ→GSI routing
- **Interrupts** — IDT with exception handlers, IOAPIC routing (edge + level-triggered), LAPIC IPI, PIC (remapped then disabled), round-robin IRQ balancing across CPUs once SMP is up (IOAPIC lines only over the apic ids a physical-mode redirection entry can name, MSI-X over all of them)
- **arm64 port** — GICv3 interrupt controller with ITS (PCIe MSI delivered as LPIs, `its=on` by default), EL1 exception vectors, ARM generic timer (per-CPU), PL011 UART, FDT (device tree) parsing, PCIe ECAM, virtio-mmio transport, broadcast TLBI, semantic memory barriers (`dmb`) throughout; NVMe over ITS-delivered MSI works end-to-end
- **Drivers** — serial (COM1), screen console (EGA text on BIOS, 8x16 font on the bootloader's pixel framebuffer under UEFI), PIT (10 ms tick, SeqLock-protected counters), RTC (CMOS wall clock), PS/2 keyboard (8042), **USB HID keyboard over xHCI** (BIOS/UEFI handoff, root-port and hub enumeration, boot-protocol reports), PCI bus scan, LAPIC, IOAPIC, **virtio-blk**, **virtio-net**, **virtio-scsi**, **virtio-rng** (legacy + modern virtio-pci transport), **NVMe** (Rust, MSI-X interrupt-driven)
- **Block I/O** — asynchronous, interrupt-driven block request queue with DMA slot pool, `BlockRequest` submission with `WaitGroup` completion, direct DMA from caller buffers (page-aligned), virtqueue locking (`RawSpinLock`) for safe interrupt/task concurrency, early-boot polling fallback, SoftIrq-based retry for ring-full conditions, block device abstraction, MBR partition discovery
- **Networking** — virtio-net driver with asynchronous interrupt-driven TX/RX, software frame queues (256-entry TX/RX) in `NetDevice` base class, reference-counted `NetFrame` descriptors for zero-copy DMA, TX slot pool with bitmask allocation, SoftIrq-based TX retry and RX processing, IP routing (subnet mask + gateway from DHCP, off-subnet traffic forwarded to gateway), ARP (cache, request, reply, dump), IPv4/UDP transmit, ICMP echo (ping reply + send, per-type statistics), DHCP client with lease renewal (sets IP, subnet mask, gateway, DNS server), DNS resolver with 32-entry cache (A-record queries, name compression, DHCP-provided server), **TCP** (connection state machine, 3-way handshake, sequence/ack tracking, retransmission timers, MSS negotiation, send/receive ring buffers, graceful close with FIN exchange, RST handling, ephemeral port allocation, granular locking: `Mutex` for ports, `RawSpinLock` for pool and per-connection state, SoftIrq-driven timer processing), **HTTP client** (URL parsing, DNS resolution, TCP connection, request/response, redirect following for 301/302/303/307/308 with loop limit, `wget` shell command), UDP remote shell (execute kernel commands over the network), network device abstraction with per-protocol packet counters, `MacAddress`/`IpAddress` structs (IPv6-ready tagged union)
- **Filesystem** — VFS layer with mount points and path resolution, ramfs (in-memory), nanofs (on-disk filesystem with 4 KB blocks, superblock with UUID, inode/data bitmaps, CRC32 checksums for superblock/inodes/data, file and recursive directory deletion, persistent across remount)
- **Entropy** — `EntropySource` interface, `EntropySourceTable` registry, virtio-rng hardware random number generator
- **Power management** — ACPI S5 shutdown, keyboard controller reset/reboot
- **Interactive shell** — trace output suppressed during shell session (dmesg only), restored on shutdown; commands: `ps`, `top`, `profile`, `stacks`, `netload`, `cpu`, `lscpu`, `bt <pid>`, `dmesg [lines] [filter]`, `loglevel [N]`, `uptime`, `date`, `memusage`, `meminfo`, `memcheck`, `pci`, `disks`, `diskread`, `diskwrite`, `irqstat`, `net`, `arp`, `netpool`, `icmpstat`, `tcpstat`, `udpsend`, `ping`, `nslookup`, `dnsflush`, `dhcp`, `wget`, `random`, `format`, `mount`, `umount`, `ls`, `cat`, `write`, `mkdir`, `touch`, `del`, `panic`, `version`, `cls`, `help`, `poweroff`, `reboot`
- **Timekeeping** — TSC calibration via PIT channel 2 (multi-round median), KVM paravirt clock (`kvmclock`) for accurate VM time, RTC wall clock, layered clock source selection (kvmclock → calibrated TSC → PIT fallback), `GetBootTime()` / `GetWallTimeSecs()` API
- **All of the machine's RAM, up to 512 GiB** — the bootstrap linear map
  covers the first 4 GiB at 2 MiB granularity, then one 1 GiB block per GiB
  of RAM above it (x86-64: needs CPUID PDPE1GB, which everything since
  Nehalem/Barcelona has; arm64: an L1 block, always available at a 4 KiB
  granule). 512 GiB is where one PDPT/L1 table runs out. Blocks with no
  usable RAM in them are left unmapped. Where the hardware cannot do it the
  kernel falls back to 4 GiB and says so; `meminfo` and two `mm:` lines at
  boot always report RAM reported vs. RAM reachable, so the difference is
  never silent. The page-descriptor array that comes with all that RAM (one
  32-byte `Page` per 4 KiB frame, so 512 MiB on a 64 GiB box) gets a
  contiguous carve-out of its own and is mapped with 2 MiB pages, since every
  free-list walk touches it at random. Cost: building the free list still
  touches every page twice, about 0.6 µs in total each, so a 64 GiB machine
  spends around ten seconds of its boot there.
- **Per-CPU tick** — each CPU runs its own periodic timer (x86-64: the local
  APIC timer, calibrated once against the established clock; arm64: the
  generic timer), rather than one HPET interrupt on one CPU fanned out to
  every other by IPI. That removes a write to the interrupt command register
  per CPU per tick, and removes the single point of failure: a CPU that stops
  answering interrupts now stops only its own tick, while the others keep
  ticking and the watchdog notices it. The HPET stays as the timekeeper.
- **Kernel infrastructure** — spinlocks, mutexes, SeqLock (single-writer/multi-reader), atomics, wait groups, SoftIrq deferred processing, IPI tasks, timers, watchdog, stack traces with symbol resolution, dmesg ring buffer (512 KB, 2048 messages, recycled lines counted and reported), panic handler with backtrace and CPU/task context, per-device interrupt statistics, AP startup diagnostics, virtual-to-physical address translation (4-level page table walk), byte-order helpers (`Htons`/`Htonl`/`Ntohs`/`Ntohl`)
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

`nos` boots on bare metal. Two machines so far, and they pull in opposite
directions: a laptop whose only console is its own screen, and a dedicated
server whose only console is the network.

##### Dell Latitude 5480

A **Dell Latitude 5480** (Intel Core i5-6200U, Skylake-U / 100-series PCH, BIOS
1.16.0) booted under UEFI. That laptop has no serial port and no 8042/PS/2
controller, yet the kernel comes up on its own screen and gives an interactive
shell driven by the built-in USB keyboard.

That machine is the reason for four pieces of the tree, none of which QEMU
ever demanded:

- **Screen** — under UEFI there is no EGA text mode. GRUB hands the kernel the
  pixel framebuffer and `drivers/fb_console.cpp` draws the console with an 8x16
  font; without it a UEFI boot is simply blind.
- **Keyboard** — a real UEFI laptop often has no 8042 at all, so the emulated
  PS/2 controller QEMU and the KVM clouds provide is not there. The xHCI driver
  (`drivers/usb/`) enumerates a USB HID boot keyboard and publishes into the
  same `KeyboardInput` sink the 8042 driver uses. `netframes=N` (frames in the network pool; watch `netpool` for misses), `rxpoll=on` (have the tick look at the receive path as well as the NIC's interrupt -- off by default, see `NetDeviceTable::PollRx`), `usb=off` skips it.
- **Firmware leftovers** — LAPIC LVTs the firmware left armed are masked, and a
  stray interrupt is named instead of panicking anonymously. The TCO watchdog is
  located on 100-series PCH and left alone when the ACPI WDAT table says the
  firmware owns it, so it cannot reset the box mid-boot.
- **Diagnostics** — with no UART the screen is the only channel, so boot-path
  failures print through the screen console and the panic handler rather than
  `Trace` alone.

##### Hetzner EX44 dedicated server

A **Hetzner EX44** (Intel Core i5-13500 — Raptor Lake, 6 P-cores + 8 E-cores,
20 threads) booted with `maxcpus=20`, which on that box is every CPU it has.
All 20 come up and stay scheduled: `ps` lists 20 `idleN` and 20 `softirq/N`
tasks, and the load balancer spreads the multitasking self-test across them.

The hybrid topology is what makes it worth recording. The APIC IDs are sparse
and non-contiguous — `0, 1, 8, 9 … 40, 41` for the SMT pairs on the P-cores,
then `48, 50 … 62`, even-numbered, for the E-cores — so the largest apic id is
62 on a 20-CPU machine. That is why `MaxCpus` is 64 rather than a number near
the CPU count, and why IOAPIC redirection entries are only aimed at apic ids a
physical-mode entry can actually name: that field is four bits wide, ids 0..15,
so round-robining an IOAPIC line onto an E-core silently aliased it onto an id
no CPU has and the line stopped being delivered. MSI-X keeps the full 8-bit
destination and the whole CPU mask.

Networking works on the real internet, and this is the first machine to
exercise the Rust **RTL8125** driver against silicon rather than a datasheet —
QEMU has no model of that chip, so until this boot `drivers/r8125` had never
seen the hardware it was written for. The DHCP client takes a public /26 lease
from the host's network, the DNS resolver comes up on the server handed out
with it, the box answers pings from anywhere, and the UDP shell
(`udpshell=9000` + `scripts/udpsh.py`) is usable across the internet — `ps` and
friends answer from a remote machine.

`netconsole=ip:port` is how any of this was seen at all. With no screen and no
serial console reachable from outside, the whole boot log — self-tests, AP
bring-up, xHCI enumeration, DHCP — went out over UDP to `scripts/netconsole.py`
as it was produced. xHCI also came up on that board: 26 root ports, three hubs,
and a USB keyboard enumerated behind one of them.

Other firmware, chipsets, NICs and disks are untested; treat bare-metal support
as "works on the two machines it was debugged on".

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
- `maxcpus=N` — bring up at most N CPUs, the BSP included (`maxcpus=2` = BSP + one AP); the rest stay parked. Bisects an SMP bring-up failure without giving up SMP entirely
- `console=serial` — direct shell output to serial port only
- `console=vga` — direct shell output to the screen only (VGA text or framebuffer)
- `dhcp=auto` — start DHCP on `eth0` automatically at boot
- `dhcp=off` — disable DHCP entirely (even via shell command)
- `dhcp=on` — enable DHCP only via shell command (default)
- `dns=on` — enable DNS resolver (uses DHCP-provided DNS server; requires `dhcp=auto`)
- `udpshell=PORT` — start UDP remote shell on the given port (e.g. `udpshell=9000`)
- `loglevel=N` — trace level to boot with (0-5, default 1). The `loglevel` shell command moves it afterwards, but only a boot parameter can make the *boot* chatty; the task lifecycle and the multitasking self-test are at level 3
- `netconsole=ip:port` — stream the kernel log over UDP to that collector (e.g. `netconsole=10.0.2.2:6666`); needs an address, so pair it with `dhcp=auto`
- `nctail=N` — ship only the newest N KiB of the log buffered before the link came up (e.g. `nctail=16`); the rest is dropped. For a machine that wedges shortly after the network appears, this spends the little airtime it has on the lines around the wedge instead of on the head of the boot log
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
- A reply is assembled in a 32 KB buffer and sent as chunks of 1384 bytes,
  one per millisecond -- paced for the same reason the netconsole drain is,
  since twenty-odd datagrams released at once lose their tail to the first
  narrow queue on the way. Whatever does not fit the buffer is dropped and
  marked `[output truncated]`. The client collects chunks by index, so one
  reordered datagram no longer discards the whole answer, and it names any
  that never arrived.
- `dmesg` still takes a line count, because the log holds 2048 messages and
  even 32 KB is a fraction of it: a dump from the head returns the first
  minutes of boot and nothing since, so on a machine that has been up a while
  `dmesg 40` is the useful form. Same reasoning as `nctail=N` for the
  netconsole backlog -- what you want is the end.
- `loglevel N` raises the trace level on the running kernel, which is the only
  way to see a subsystem whose level constant is above the default 1 (`UsbLL`
  and `KbdLL` are 3, the allocators and MMIO are 4) without a rebuild and a
  reflash. It is loud: level 4 traces every allocation.
- **Warning:** the UDP shell has no authentication — anyone who can reach the port has full kernel shell access. Use only for testing or behind a firewall.

#### Netconsole

With `netconsole=ip:port` the kernel ships its whole log to a UDP collector as
each line is produced -- the debugging channel of choice on a machine with no
serial port (a laptop over Wi-Fi, a cloud VM).

Every line the tracer emits is copied into a 128 KiB ring buffer the moment it
is produced, from any context including IRQ; a dedicated `netcon` task drains
the ring and sends whole lines in ~1400-byte datagrams, one per millisecond.
Nothing is lost while the network is still coming up: the ring is primed from
`dmesg` at boot and keeps buffering until the device has an IP, then the
backlog goes out and live lines follow within a couple of milliseconds. When
the ring fills, the oldest lines are evicted and counted (`netconsole` shell
command). Panics are included -- the panic report is captured and flushed
synchronously, best-effort and only if the collector MAC is already in the ARP
cache, since a blocking ARP resolve could never complete with the other CPUs
halted.

Each datagram carries an eight-byte header: `NOSC` and a little-endian
sequence number, which the collector uses to report gaps. UDP drops datagrams
without saying so, and a log that ends because the network ate the rest of a
burst reads exactly like a log that ends because the machine wedged -- which
is the one distinction that matters when debugging a hang. The same reasoning
sets the pace: by the time the link comes up there can be a hundred datagrams
of boot log queued, and sending them at the rate the NIC accepts overruns
whatever is narrowest on the way to the collector, losing the far end of the
burst. `nctail=N` goes further and caps that backlog at the newest N KiB, so
a machine with a few milliseconds of network left spends them on the lines
nearest its death.

Receive with the included collector:

```sh
python3 scripts/netconsole.py                    # listen on 0.0.0.0:6666
python3 scripts/netconsole.py -p 5555 -o boot.log
```

It reassembles lines per sender, prefixes each with the host receive time,
marks any gap in the sequence (`--- netconsole: N datagram(s) lost ---`) and a
sender that restarted, and optionally appends to a file. Datagrams with no
header, from an older kernel, are still printed -- just without gap
detection. Under QEMU user networking the host is `10.0.2.2`,
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
| `lscpu` | Identify the CPU and report the features the kernel depends on (1 GiB pages, NX, PAT, invariant TSC, ARAT), the performance counters it has (Intel architectural perfmon, or AMD core counters and PerfMonV2), and which of them `profile` will actually sample with |
| `dmesg [lines] [filter]` | Dump kernel log: newest `lines` messages (all of it if omitted), optional substring filter |
| `loglevel [N]` | Show or set the trace level (0-5) on a running kernel |
| `uptime` | Show uptime |
| `date` | Show wall clock date and time (RTC + boot time) |
| `nicdump` | The r8125's own state: command register (is the receiver still enabled?), interrupt status and mask, and whose the head receive descriptor is. For a machine that has stopped receiving and can still be typed at |
| `netload [start [port] [sink]\|stop\|reset]` | UDP load target, so `profile` has something to look at other than an idle machine: echoes every datagram from the receive softirq (or drops it, in sink mode) and counts packets, bytes and rate per CPU. Drive it with `scripts/netload.py` (`--pps` to hold a rate, `--threads` to raise the ceiling) |
| `stacks` | Stack high-water marks: every stack is filled with a pattern when it is created, and what is still intact is what it never reached. Catches a spike that lasted microseconds during boot, and costs nothing while the machine runs |
| `ps` | Show tasks |
| `top [ms]` | Per-task CPU use over a sampling window, percent per CPU (a busy thread reads 100%, a 20-CPU box tops out at 2000%), plus the number of tasks moved between CPU queues since boot |
| `profile [ms] [pid\|all] [chains]` | Sampling profiler: where the kernel spends its time, as whole call chains -- samples are folded by their entire stack, not by the leaf symbol, and the hottest chains are printed in full. `chains` caps how many, for a console with no scrollback -- `profile 2000 all 3` fits a screen where the default does not. Every frame carries its offset: the leaf as the instruction the sample landed on (a span when they spread across the body, so a hot spinlock says whether it sat on the exchange or in the pause loop), each caller as the return address that names which call site led there. Samples on a performance counter overflowing into an NMI (~1 kHz, and catches code running with interrupts off) where the CPU has one -- Intel's architectural fixed counter 1, or AMD's PMCx076 on family 15h and later, both counting unhalted core cycles; falls back to the 100 Hz per-CPU tick where it does not, which includes any machine whose hypervisor answers CPUID for a PMU it declines to virtualise (the counter is asked to prove it counts before the profiler trusts it). The report names which |
| `bt <pid>` | Dump stack trace of a task (uses IPI for remote CPUs) |
| `watchdog` | Watchdog stats: locks watched, table walks, and the slice of the bucket table each CPU walks (the table is divided among the CPUs, so a bucket is visited once per tick rather than once per tick per CPU) |
| `memusage` | Show memory usage |
| `meminfo` | Show the firmware memory map, and how much of it the kernel actually uses |
| `memcheck` | Verify no reserved, kernel-image or absent page reached the free list |
| `pci` | Show PCI devices |
| `disks` | List block devices |
| `diskread <disk> <sector>` | Read and hex-dump a sector |
| `diskwrite <disk> <sector> <hex>` | Write hex data to a sector |
| `irqstat` | Show per-device interrupt counters |
| `help` | List commands |
| `net` | List network devices and per-protocol stats |
| `arp` | Show ARP table |
| `netpool` | Show the recycled net frame pool: frames in the ring, in per-CPU caches, in flight |
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
    r8125/    Realtek RTL8125 2.5GbE network device driver
  hello/      Rust self-test module
  kernel/     Rust entry points (rust_main, rust_fini), global allocator
build/        Linker script, GRUB configs
scripts/      Build, run, debug, and GDB helpers
```
