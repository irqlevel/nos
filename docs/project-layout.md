# Project layout

Where things live in the tree. How the pieces work is in [Boot](boot.md),
[Paging and memory](paging.md), [Scheduler](scheduler.md),
[Interrupts](interrupts.md) and [Profiler](profiler.md); how the Rust side
is layered and written is in [Rust in the kernel](rust.md); the conventions
are in [`CLAUDE.md`](../CLAUDE.md) and the roadmap in `plans/README.md`.

C++ is the fundamentals, and everything that sits on them is Rust: there is
no `src/cpp/{block,net,fs}`, no virtio and no USB in `src/cpp/drivers/`.

```
src/cpp/
  hal/        Portable HAL contracts: cpu and atomics, the semantic barriers, mmu/pte, irqchip, console, pci, power, Context, IRQ stub symbols, the cpu's random instruction. Each header selects the arch backend at compile time, and common code includes nothing else of an architecture's
  arch/
    x86_64/   Multiboot2 entry + AP trampoline (boot64.asm, NASM), CPU primitives (asm.asm, asm.h), IDT/GDT, exceptions, TSC/kvmclock, LAPIC/IOAPIC/PIC, PTE encoding (pte.h), GRUB info parsing, the string routines in assembly (stdlib_asm.asm), HAL inline/impl backends. Only arch code, x86-only drivers and three documented exemptions (kernel/main.cpp, kernel/cmd.cpp, kernel/irq_balance.cpp) may include these headers
    arm64/    Linux-Image boot + PSCI SMP (boot.S), the boot path itself (main_arm64.cpp), EL1 vectors, GICv3 + ITS (LPIs for PCIe MSI), generic timer, PL011, FDT parser, PCIe ECAM, PTE encoding, the string routines in portable C (stdlib_c.cpp), link stubs for the x86-only entry points common driver code names (x86_driver_stubs.cpp), HAL backends
  kernel/     Core: scheduling, tasks, interrupt dispatch, SoftIrq, timers, the timekeeping seam (time.h), locks (spinlock, mutex, seqlock, rwlock), panic and backtrace, the dmesg ring buffer, the command line (parameters.cpp), the interactive shell (cmd.cpp), the input layer (input.cpp), the ChaCha20 random pool every source feeds and everything reads from (random.cpp, entropy.h), the boot self-tests (test.cpp), the module loader (module.cpp), the Rust FFI bridge (rust_ffi.cpp), the symbol table
  drivers/    Hardware: serial, the console (screen.cpp picks EGA text on BIOS or the 8x16-font pixel framebuffer under UEFI), PIT, HPET, RTC, the 8042 keyboard, PCI, MSI-X, ACPI. No virtio and no USB: both are Rust (src/rust/virtio, src/rust/drivers)
  mm/         Memory: 4-level page tables (the VirtToPhys walk, ProtectRange), the page allocator (fixed-size block allocators), the pool allocator, the VA allocator, new/delete
  lib/        Freestanding stdlib equivalents (Stdlib::): list, vector, btree, ring buffer, bitmap, CRC32, ChaCha20, formatting, errors and results, smart pointers. MemSet/MemCpy/StrLen and their like are per architecture (arch/x86_64/stdlib_asm.asm, arch/arm64/stdlib_c.cpp)
  include/    Shared headers
src/rust/
  ffi/        Raw extern "C" FFI declarations for kernel services
  kcore/      Safe Rust wrappers around kernel services: sync, DMA, MMIO, MSI-X, interrupts, timers, tasks, PCI -- and the block, net, TCP and file layers *as a loadable module reaches them*, across the C ABI (inside the image the crates below call each other as crates)
  drivers/
    nvme/     NVMe block device driver (PCI, MSI-X, admin/IO queues)
    usb/      xHCI host controller (rings, contexts, root-port and hub enumeration) + the HID boot-protocol keyboard on it
    r8168/    Realtek r8168 network device driver
    r8125/    Realtek RTL8125 2.5GbE network device driver
    igb/      Intel I210 (igb) gigabit network device driver
    virtio_rng/ virtio-rng, the host's entropy, on the Rust virtio foundation
    virtio_blk/ virtio-blk, the disk the root filesystem lives on, likewise
    virtio_net/ virtio-net, the card the network comes in on
    virtio_scsi/ virtio-scsi, a SCSI adapter and the disks behind it
  block/      The block layer: the device table a driver registers a `BlockDriver` in (table.rs), the `Disk` the rest of the image reads and writes through (disk.rs), its claims, the partition tables (MBR and GPT) a disk is cut up by, the shell's disk commands (shell.rs) and the kernel log written to a raw disk area (disklog.rs)
  netwire/    What is on the wire, with no kernel in it: the Ethernet, ARP, IP, UDP, ICMP and TCP headers read and written through byte slices, the internet checksum, a UDP datagram taken apart or put together whole, addresses parsed and printed. The network layer is built on it (as net::wire), and so is a module that builds or rewrites frames itself -- it cannot link the layer, but it can share this
  net/        The network layer, over netwire: ARP (arp.rs), ICMP (icmp.rs), UDP (udp.rs), the DHCP client (dhcp.rs), the DNS resolver (dns.rs), TCP (tcp.rs), the HTTP client over either TCP or TLS (http.rs), the shell over UDP (udp_shell.rs), the kernel log over UDP (netconsole.rs), the recycled frame pool (frame.rs), the devices, the `NetDriver` a NIC registers as and the queues between it and the stack (device.rs), a device as the layer's own services hold one (nic.rs), the boot self-test over the formats (selftest.rs), the shell's network commands (shell.rs), wget (wget.rs) and the C ABI C++ and the modules call them by (abi.rs)
  fs/         The filesystem layer: the VFS (vfs.rs -- the mount table, path resolution, open handles and the file API), the `FileSystem` trait and the filesystems that implement it (ext2.rs, nanofs.rs, ramfs.rs, procfs.rs), the arena of nodes each keeps its tree in (vnode.rs), what boot mounts where (rootfs.rs), the shell's filesystem commands (shell.rs) and the self-test (selftest.rs)
  virtio/     The virtio foundation: the split virtqueue, the transport contract, virtio-pci (modern and legacy) and virtio-mmio v2
  tls/        The TLS client: rustls + RustCrypto + webpki-roots, driven through rustls' unbuffered API, over a transport it is handed as a trait (docs/tls.md)
  ssh/        An SSH server's protocol, with no kernel in it; the sshd module serves it (docs/sshd.md)
  hvarch/     The CPU's virtualization extension: CPUID and the control MSRs, AMD-V (x86/svm.rs: turning it on, entering a guest; x86/svm/vmcb.rs: the VMCB's layout) and Intel VT-x (x86/vmx.rs), the Arm EL2 probe (arm64.rs). Almost all of the hypervisor's unsafe, so that the rest of it has next to none (docs/hypervisor.md)
  hv/         The hypervisor above the CPU, safe but for the one call that enters a guest: the machine's extension and which CPUs it is on for (machine.rs), guest memory and its nested page table (memory.rs, npt.rs), the VMCB's policy and the exits (svm.rs), the VM (vm.rs), the built-in guests (guests.rs). Neither this nor hvarch is a default member of the workspace: they are compiled only into the hv module, so a kernel without it carries no hypervisor at all
  hello/      Rust self-test module
  kernel/     Rust entry points (rust_init, rust_fini), global allocator, the `sha256` command
  kmod/       Runtime of a loadable module: allocator, panic handler, the module! header
  modules/    Loadable modules, each built into a .ko (hello; sshd, an SSH server; blkload, a block I/O load test; netload, a UDP load test from either end; netblk, a disk served over UDP, zero-copy; hv, the hypervisor; modtest, which the boot test loads; slowexit, an exit that takes its time)
  vendor/     The sources of the external crates (what tls and ssh depend on, and what std itself resolves to), so that a build runs `cargo --offline`; scripts/vendor.sh refreshes it
  rust-toolchain.toml  The dated nightly everything is built with
build/        Linker scripts, GRUB configs, the awk script that makes the module export table
scripts/      Build, run, debug and GDB helpers, and the gates (docs/testing.md)
docs/         This directory
plans/        The roadmap
```
