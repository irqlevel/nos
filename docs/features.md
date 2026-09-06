# Features

Everything the kernel does today, by subsystem. For where the code lives see
[Project layout](project-layout.md); for the interactive shell see
[Shell commands](shell-commands.md).

- **Two architectures** — x86-64 (Multiboot2/GRUB, ISO or MBR disk boot, **legacy BIOS and UEFI firmware**) and arm64 (QEMU `virt` board, Linux `Image` boot protocol); portable code goes through a HAL layer (`src/cpp/hal/`), arch backends live in `src/cpp/arch/`
- **SMP** — up to 64 CPUs (20 exercised on real hardware); AP bootstrap via INIT/SIPI on x86-64, PSCI `CPU_ON` on arm64, APs started one at a time, `maxcpus=N` to cap them
- **Preemptive multitasking** — per-CPU task queues, round-robin scheduling, load-balanced task placement
- **Virtual memory** — 4-level paging (4 KB pages), high-half kernel at `0xFFFF800001000000`, TLB shootdown across CPUs via IPI
- **Page allocator** — fixed-size block allocator (1–128 contiguous pages), pool allocator (32 B – 2 KB), `new`/`delete` support
- **ACPI** — RSDP/RSDT/MADT parsing for LAPIC/IOAPIC discovery and IRQ→GSI routing
- **Interrupts** — IDT with exception handlers, IOAPIC routing (edge + level-triggered), LAPIC IPI, PIC (remapped then disabled), round-robin IRQ balancing across CPUs once SMP is up (IOAPIC lines only over the apic ids a physical-mode redirection entry can name, MSI-X over all of them)
- **arm64 port** — GICv3 interrupt controller with ITS (PCIe MSI delivered as LPIs, `its=on` by default), EL1 exception vectors, ARM generic timer (per-CPU), PL011 UART, FDT (device tree) parsing, PCIe ECAM, virtio-mmio transport, broadcast TLBI, semantic memory barriers (`dmb`) throughout; NVMe over ITS-delivered MSI works end-to-end
- **Drivers** — serial (COM1), screen console (EGA text on BIOS, 8x16 font on the bootloader's pixel framebuffer under UEFI), PIT (10 ms tick, SeqLock-protected counters), RTC (CMOS wall clock), PS/2 keyboard (8042), **USB HID keyboard over xHCI** (BIOS/UEFI handoff, root-port and hub enumeration, boot-protocol reports), PCI bus scan, LAPIC, IOAPIC, **virtio-blk**, **virtio-net**, **virtio-scsi**, **virtio-rng** (legacy + modern virtio-pci transport), **NVMe** (Rust, MSI-X interrupt-driven), **Realtek RTL8168/RTL8125** and **Intel 82576/I210** (igb) NICs (Rust)
- **Block I/O** — asynchronous, interrupt-driven block request queue with DMA slot pool, `BlockRequest` submission with `WaitGroup` completion, direct DMA from caller buffers (page-aligned), virtqueue locking (`RawSpinLock`) for safe interrupt/task concurrency, early-boot polling fallback, SoftIrq-based retry for ring-full conditions, block device abstraction, MBR partition discovery
- **Networking** — virtio-net driver with asynchronous interrupt-driven TX/RX, software frame queues (256-entry TX/RX) in `NetDevice` base class, reference-counted `NetFrame` descriptors for zero-copy DMA, TX slot pool with bitmask allocation, SoftIrq-based TX retry and RX processing, IP routing (subnet mask + gateway from DHCP, off-subnet traffic forwarded to gateway), ARP (cache, request, reply, dump), IPv4/UDP transmit, ICMP echo (ping reply + send, per-type statistics), DHCP client with lease renewal (sets IP, subnet mask, gateway, DNS server), DNS resolver with 32-entry cache (A-record queries, name compression, DHCP-provided server), **TCP** (connection state machine, 3-way handshake, sequence/ack tracking, retransmission timers, MSS negotiation, send/receive ring buffers, graceful close with FIN exchange, RST handling, ephemeral port allocation, granular locking: `Mutex` for ports, `RawSpinLock` for pool and per-connection state, SoftIrq-driven timer processing), **HTTP client** (URL parsing, DNS resolution, TCP connection, request/response, redirect following for 301/302/303/307/308 with loop limit, `wget` shell command), [UDP remote shell](udp-shell.md) (execute kernel commands over the network), network device abstraction with per-protocol packet counters, `MacAddress`/`IpAddress` structs (IPv6-ready tagged union)
- **Filesystem** — VFS layer with mount points and path resolution, ramfs (in-memory), nanofs (on-disk filesystem with 4 KB blocks, superblock with UUID, inode/data bitmaps, CRC32 checksums for superblock/inodes/data, file and recursive directory deletion, persistent across remount)
- **Entropy** — `EntropySource` interface, `EntropySourceTable` registry, virtio-rng hardware random number generator
- **Power management** — ACPI S5 shutdown, keyboard controller reset/reboot
- **Interactive shell** — trace output suppressed during shell session (dmesg only), restored on shutdown; commands: `ps`, `top`, `profile`, `stacks`, `netload`, `cpu`, `lscpu`, `bt <pid>`, `dmesg [lines] [filter]`, `loglevel [N]`, `uptime`, `date`, `memusage`, `meminfo`, `memcheck`, `pci`, `disks`, `diskread`, `diskwrite`, `irqstat`, `net`, `arp`, `netpool`, `icmpstat`, `tcpstat`, `udpsend`, `ping`, `nslookup`, `dnsflush`, `dhcp`, `wget`, `random`, `format`, `mount`, `umount`, `ls`, `cat`, `write`, `mkdir`, `touch`, `del`, `panic`, `version`, `cls`, `help`, `poweroff`, `reboot` — see [Shell commands](shell-commands.md)
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
