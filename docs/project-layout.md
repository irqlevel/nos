# Project layout

Where things live in the tree. How the pieces work is in [Boot](boot.md),
[Paging and memory](paging.md), [Scheduler](scheduler.md),
[Interrupts](interrupts.md) and [Profiler](profiler.md); the conventions are
in `CLAUDE.md` and the roadmap in `plans/README.md`.

```
src/cpp/
  hal/        Portable HAL contracts: cpu, barriers, mmu/pte, irqchip, console, power, context, irq stubs, random instruction
  arch/
    x86_64/   Multiboot2 entry + AP trampoline (NASM), CPU primitives (asm.asm), IDT/GDT, exceptions, TSC/kvmclock, LAPIC/IOAPIC/PIC, PTE encoding, GRUB info parsing, HAL backends
    arm64/    Linux-Image boot + PSCI SMP (boot.S), EL1 vectors, GICv3 + ITS (LPIs for PCIe MSI), generic timer, PL011, FDT parser, PCIe ECAM, PTE encoding, HAL backends
  kernel/     Core: scheduling, tasks, interrupt dispatch, SoftIrq, shell, timers, timekeeping, locks, panic, the random pool, Rust FFI bridge, symbol table
  drivers/    Hardware: serial, VGA text + framebuffer console (screen.cpp picks one), PIT, HPET, RTC, 8042, PCI, MSI-X, ACPI (every virtio device and the bus itself are Rust now, src/rust/virtio)
    usb/      xHCI host controller (rings, contexts, root-port and hub enumeration) + HID boot-protocol keyboard

  net/        Networking: device abstraction, protocol headers, ARP, ICMP, DHCP, DNS, TCP, HTTP client, UDP shell, netconsole
  mm/         Memory: page tables (4-level walk, VirtToPhys), page allocator, pool allocator
  lib/        Utilities: list, vector, btree, ring buffer, bitmap, CRC32 checksum, ChaCha20, stdlib
  include/    Shared headers
src/rust/
  ffi/        Raw extern "C" FFI declarations for kernel services
  kcore/      Safe Rust wrappers: sync, DMA, MMIO, MSI-X, interrupts, timers, tasks, PCI, block/net device
  drivers/
    nvme/     NVMe block device driver (PCI, MSI-X, admin/IO queues)
    r8168/    Realtek r8168 network device driver
    r8125/    Realtek RTL8125 2.5GbE network device driver
    igb/      Intel I210 (igb) gigabit network device driver
    virtio_rng/ virtio-rng, the host's entropy, on the Rust virtio foundation
    virtio_blk/ virtio-blk, the disk the root filesystem lives on, likewise
    virtio_net/ virtio-net, the card the network comes in on
    virtio_scsi/ virtio-scsi, a SCSI adapter and the disks behind it
  block/      The block layer: the device table, its claims, the partition tables (MBR and GPT) a disk is cut up by, the shell's disk commands (shell.rs) and the kernel log written to a raw disk area (disklog.rs)
  net/        The network layer: the wire formats and the internet checksum (wire.rs), ARP (arp.rs), ICMP (icmp.rs), UDP (udp.rs), the DHCP client (dhcp.rs), the DNS resolver (dns.rs), TCP (tcp.rs), the HTTP client over either TCP or TLS (http.rs), the shell over UDP (udp_shell.rs), the kernel log over UDP (netconsole.rs), the load target (net_load.rs), the recycled frame pool (frame.rs), the devices and the queues between them and their drivers (device.rs), the boot self-test over the formats (selftest.rs) and the C ABI C++ calls them by (abi.rs)
  fs/         The filesystem layer: the VFS (vfs.rs -- the mount table, path resolution, open handles and the file API), the filesystems under it (ext2.rs, nanofs.rs, ramfs.rs, procfs.rs), the vnode they are made of (vnode.rs), what boot mounts where (rootfs.rs), the shell's filesystem commands (shell.rs) and the self-test (selftest.rs)
  virtio/     The virtio foundation: the split virtqueue, the transport contract, virtio-pci (modern and legacy) and virtio-mmio v2
  hello/      Rust self-test module
  kernel/     Rust entry points (rust_main, rust_fini), global allocator
  kmod/       Runtime of a loadable module: allocator, panic handler, the module! header
  modules/    Loadable modules, each built into a .ko (hello; blkload, a block I/O load test; netblk, a disk served over UDP, zero-copy; modtest, which the boot test loads; slowexit, an exit that takes its time)
build/        Linker script, GRUB configs
scripts/      Build, run, debug, and GDB helpers
```
