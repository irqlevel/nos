### nos

A hobby x86-64 operating system kernel written in C++14 and NASM.

#### Features

- **SMP** — up to 8 CPUs with INIT/SIPI AP bootstrap
- **Preemptive multitasking** — per-CPU task queues, round-robin scheduling, load-balanced task placement
- **Virtual memory** — 4-level paging (4 KB pages), high-half kernel at `0xFFFF800001000000`, TLB shootdown across CPUs via IPI
- **Page allocator** — fixed-size block allocator (1/2/4/8 pages), pool allocator (32 B – 2 KB), `new`/`delete` support
- **ACPI** — RSDP/RSDT/MADT parsing for LAPIC/IOAPIC discovery and IRQ→GSI routing
- **Interrupts** — IDT with exception handlers, IOAPIC routing, LAPIC IPI, PIC (remapped then disabled)
- **Drivers** — serial (COM1), VGA text mode, PIT (10 ms tick), PS/2 keyboard (8042), PCI bus scan, LAPIC, IOAPIC
- **Interactive shell** — commands: `ps`, `cpu`, `dmesg`, `uptime`, `memusage`, `pci`, `cls`, `help`, `shutdown`
- **Kernel infrastructure** — spinlocks, atomics, timers, watchdog, stack traces, dmesg ring buffer, panic handler
- **Boot tests** — allocator, btree, ring buffer, stack trace, multitasking

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

#### Project layout

```
boot/       Multiboot2 entry, 32→64-bit transition, AP trampoline
kernel/     Core: scheduling, tasks, interrupts, shell, timers, locks
drivers/    Hardware: serial, VGA, PIT, 8042, PCI, PIC, LAPIC, IOAPIC, ACPI
mm/         Memory: page tables, page allocator, pool allocator
lib/        Utilities: list, vector, btree, ring buffer, bitmap, stdlib
build/      Linker script, GRUB config
scripts/    GDB and KVM perf helpers
```
