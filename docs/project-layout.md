# Project layout

Where things live in the tree. How the pieces work is in [Boot](boot.md),
[Paging and memory](paging.md), [Scheduler](scheduler.md),
[Interrupts](interrupts.md) and [Profiler](profiler.md); the conventions are
in `CLAUDE.md` and the roadmap in `plans/README.md`.

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
    igb/      Intel I210 (igb) gigabit network device driver
  hello/      Rust self-test module
  kernel/     Rust entry points (rust_main, rust_fini), global allocator
build/        Linker script, GRUB configs
scripts/      Build, run, debug, and GDB helpers
```
