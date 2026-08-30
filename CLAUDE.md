# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`nos` is a hobby x86-64 / arm64 OS kernel written in C++20, Rust, and assembly. It is freestanding (no libc, no STL, no C++ exceptions/RTTI) and boots via Multiboot2/GRUB on x86-64 and the Linux `Image` protocol on arm64. It targets QEMU/KVM, KVM-based clouds (Google Cloud, Yandex Cloud), and QEMU `virt` + HVF on Apple Silicon, and boots on one real machine — a Dell Latitude 5480 (Skylake-U, UEFI, no serial port, no PS/2), where the framebuffer console and the xHCI USB keyboard are the only console channels. See `README.md` for the full feature list and shell command reference, and `plans/README.md` for the roadmap (the long-term goal is a bare-metal cloud node running Linux guests under a Rust hypervisor — stages 0–5, currently at the end of stage 2).

## Build, run, test

The kernel needs an x86-64 cross toolchain (clang, nasm, ld, grub-mkrescue + `xorriso`/`mtools`, cppcheck, and a **nightly** rustup toolchain with the `rust-src` component — pinned by `src/rust/rust-toolchain.toml`; the Rust staticlib needs nightly for `-Z build-std` and `#![feature(alloc_error_handler)]`). On macOS / Apple Silicon you almost always build inside Docker, which packages the whole toolchain:

```sh
./scripts/build-iso-docker.sh   # produces nos.iso + bin/kernel64.elf (GDB symbols)
./scripts/build-disk.sh         # produces nos.qcow2 (MBR disk, needs --privileged Docker)
```

Native build (Linux with the toolchain installed):

```sh
make            # = `make check` (cppcheck static analysis) + build nos.iso
make nocheck    # skip cppcheck, just build
make check      # run cppcheck only (fails the build on any finding, exit code 22)
make rust       # build only the Rust staticlib (src/rust → libkernel.a)
make smoke      # boot smoke test: build in Docker + headless QEMU, assert serial markers
make clean
```

The build is parameterized by `ARCH` (default `x86_64`; `aarch64` selects the
arm64 toolchain/linker-script/Rust-target leg). Objects live in `out/$(ARCH)/`.
After moving or renaming headers run a clean build — stale `.d` files in
`out/` reference old paths.

**Both architectures must stay green.** CI (`.github/workflows/ci.yml`) runs
`make check` plus a build *and* a boot smoke test for `x86_64` and `aarch64`.
A change to common code that only builds on x86 is a broken change.

**Source lists are explicit, not globbed.** The Makefile has two hand-written
lists, `CXX_SRC_x86_64` and `CXX_SRC_aarch64` (plus `ASM_SRC_*` for NASM and
`ASM_S_SRC_*` for GNU-as). A new `.cpp` that is not added to the right list is
silently not compiled; portable code must be added to **both**. Common driver
code that references x86-only entry points gets an unreachable link stub in
`src/cpp/arch/arm64/x86_driver_stubs.cpp` rather than an `#ifdef`.

**Every refactor step must keep `./scripts/smoke-test.sh` green** (markers
`After test` → `Preempt is now on` → `boot: complete`, fail-fast on `PANIC:`;
`scripts/smoke-arm64.sh` is the arm64 equivalent). Gate on its exit code,
never on grepping its output. The x86 smoke boot attaches virtio-blk (modern),
virtio-scsi (legacy), NVMe, virtio-net and virtio-rng, so it covers the
Rust/MSI-X path too.

Run in QEMU (serial console is logged to `nos.log`):

```sh
./scripts/qemu.sh        # ISO boot, 2 CPUs, virtio-net, GDB stub on :1234
./scripts/qemu-disk.sh   # disk boot with virtio-blk/scsi/nvme/net/rng attached
./scripts/run.sh         # Linux/KVM, all host CPUs, isa-debug-exit device
```

arm64 (QEMU `virt`, HVF-accelerated on Apple Silicon — the fast dev loop):

```sh
make nocheck ARCH=aarch64      # (in Docker on macOS) -> kernel-arm64.elf + nos-arm64.img
./scripts/qemu-arm64.sh        # boots nos-arm64.img with virtio-mmio blk/net/rng,
                               # serial -> nos-arm64.log, UDP shell on :9000
                               # (NOS_TCG=1 forces TCG; HVF is ~4x faster)
./scripts/smoke-arm64.sh       # arm64 boot smoke test (SMOKE_HVF=1 for HVF)
python3 scripts/udpsh.py 127.0.0.1   # remote shell over UDP
./scripts/gdb-arm64.sh         # attach GDB (qemu-arm64.sh -s), needs gdb-multiarch
```

PCIe on arm64 (ECAM + GICv3 ITS) works end-to-end: NVMe-over-MSI delivers
interrupt-driven completions via ITS LPIs. `its=on` is the default; boot
with `its=off` to disable the ITS and degrade MSI gracefully (virtio-mmio
devices don't need it).

Debug with GDB (QEMU must be started with `-s`):

```sh
gdb -ex "symbol-file bin/kernel64.elf" -ex "set architecture i386:x86-64" -ex "target remote :1234"
# or: ./scripts/gdb64.sh
```

### Kernel command line

Set via GRUB (`build/grub.cfg`) on x86-64, or QEMU `-append` on arm64. Useful
when bisecting a boot failure: `smp=off` (BSP only), `maxcpus=N` (start at most
N CPUs, the BSP included), `console=serial` / `console=vga`,
`dhcp=auto|on|off`, `dns=on`, `udpshell=PORT`, `usb=off`
(x86-64: skip xHCI bring-up), `its=off` (arm64). Parsing lives in
`kernel/parameters.cpp`. `netconsole=ip:port` streams the whole kernel log to
that UDP collector as each line is produced (buffered until the network is up,
panic report included) -- receive it with `scripts/netconsole.py`; on a machine
with no serial port this is the only way to see the log of a boot that dies.
Each datagram carries a sequence number so the collector can report gaps, and
the drain is paced at one datagram per millisecond -- an unpaced backlog burst
is dropped by the first narrow queue on the way out, which looks exactly like
a hang. `nctail=N` caps the pre-link backlog at the newest N KiB, so a machine
that wedges just after the network comes up sends the lines around the wedge
rather than the head of the boot log.

### Tests run at boot, not via a test runner

There is no separate test binary. Self-tests live in `src/cpp/kernel/test.cpp`; each is a `Stdlib::Error TestXxx()` function. They are run from `Test::Test()` (called in `main.cpp:Main2`) and `Test::TestMultiTasking()`, early during boot. To add a test, write a `TestXxx()` and register it in the `Test()` dispatcher. **To run a single test, edit `Test()` to call only that function**, rebuild, and boot — watch `nos.log`. A failing test returns a non-success `Stdlib::Error`.

### Symbol table is generated by a two-pass link

Stack traces resolve symbols from a table baked into the kernel. The build links `out/$(ARCH)/pass1.elf`, runs `nm` over it to generate `out/$(ARCH)/symtab_data.cpp`, then links the final kernel ELF. If you touch the link or symbol resolution, remember this two-pass flow (see the `symtab_data` rules in the `Makefile`).

## Architecture

Boot flow (x86-64): `arch/x86_64/boot64.asm` (Multiboot2 entry, 32→64-bit transition, AP trampoline) → `kernel/main.cpp` `Main2` (BSP: paging, dmesg, page allocator, runs `Test()`) → `BpStartup` (interrupts, IOAPIC, timers, brings up APs via INIT/SIPI, starts SoftIrq/TCP/shell, prints `boot: complete`). APs enter via `ApMain` → `ApStartup`. On arm64 the equivalent path is `arch/arm64/boot.S` → `arch/arm64/main_arm64.cpp`, with PSCI `CPU_ON` for APs.

Source layout (detailed in `README.md` "Project layout"):

- `src/cpp/hal/` — portable HAL contracts (cpu/atomics, semantic barriers, mmu+pte, irqchip, console, pci, power, Context, IRQ stub symbols); each header selects the arch backend at compile time
- `src/cpp/arch/x86_64/` — everything x86-specific: Multiboot2 entry + AP trampoline (`boot64.asm`), CPU primitives (`asm.asm`, `asm.h`), IDT/GDT, exceptions, TSC/kvmclock, LAPIC/IOAPIC/PIC, PTE encoding (`pte.h`), GRUB parsing, HAL inline/impl backends. Only arch code, the documented exemptions (`kernel/main.cpp`, `kernel/cmd.cpp`, `kernel/irq_balance.cpp`) and x86-only drivers may include these headers
- `src/cpp/arch/arm64/` — Linux-Image boot + PSCI SMP (`boot.S`), EL1 vectors, GICv3 + ITS, generic timer, PL011, FDT parser, PCIe ECAM, PTE encoding, HAL backends
- `src/cpp/kernel/` — scheduling, tasks, interrupt dispatch, SoftIrq, timers, timekeeping seam (`time.h`), locks (spinlock/mutex/seqlock/rwlock), panic/backtrace, dmesg ring buffer, the interactive shell (`cmd.cpp`), input layer (`input.cpp`), the Rust FFI bridge (`rust_ffi.cpp`), symbol table
- `src/cpp/mm/` — 4-level page tables (`VirtToPhys` walk, `ProtectRange`), page allocator (fixed-size block allocator), pool allocator, VA allocator, `new`/`delete`
- `src/cpp/drivers/` — serial, console (`screen.cpp` picks EGA text on BIOS vs. the 8x16-font pixel framebuffer under UEFI), PIT/HPET/RTC, 8042 keyboard, `usb/` (xHCI host controller + HID boot keyboard, for UEFI laptops with no PS/2), PCI, MSI-X, ACPI, virtio (blk/net/scsi/rng) behind the `VirtioTransport` interface (legacy+modern virtio-pci on x86, virtio-mmio on arm64)
- `src/cpp/block/` — async interrupt-driven block request queue, MBR partitions
- `src/cpp/net/` — device abstraction, ARP/ICMP/DHCP/DNS/TCP/UDP, HTTP client, UDP shell, netconsole (kernel log over UDP)
- `src/cpp/fs/` — VFS with mount points, ramfs, nanofs (on-disk), ext2 (ro), procfs
- `src/cpp/lib/` — freestanding stdlib equivalents (`Stdlib::`), containers (list/vector/btree/ringbuffer/bitmap), CRC32, formatting; some routines (`MemSet`/`MemCpy`/`StrLen`…) are in `arch/x86_64/stdlib_asm.asm` (portable C in `arch/arm64/stdlib_c.cpp`)

Rust (`src/rust/`, a cargo workspace) compiles to a `#![no_std]` `staticlib` (`libkernel.a`) linked into the kernel. The **NVMe driver is written entirely in Rust**. Layers: `ffi/` (raw `extern "C"` declarations only), `kcore/` (safe RAII wrappers around kernel services), `drivers/` (nvme, r8168), `kernel/` (entry points + global allocator), `hello/` (self-test). Adding a kernel service to Rust is a three-step process across `rust_ffi.cpp`, `ffi/`, and `kcore/` — see `.cursor/rules/rust-kernel-conventions.mdc`.

## Conventions (critical — full rules in `.cursor/rules/`)

These come from `.cursor/rules/kernel-conventions.mdc` (C++) and `rust-kernel-conventions.mdc` (Rust), which are authoritative. The non-obvious ones:

- **Freestanding constraints**: no `std::` (use `Stdlib::` from `lib/stdlib.h`), no exceptions/RTTI, **no lambdas/closures**, no `thread_local`, no heap before `PageAllocatorImpl::Setup()`. `volatile` compound assignments are banned — write `x = x + y`.
- **Static constructors are unreliable.** Never initialize via a static instance constructor. Use the singleton + explicit init pattern: `static Foo& GetInstance(){ static Foo i; return i; }` plus a `bool Setup()` called once at boot. Heap-allocate non-trivial objects (e.g. `Mm::TAlloc<T, Tag>()`) rather than using static arrays of them.
- **Error handling**: return `bool`/pointer for pass-fail, `Stdlib::Error` (`lib/error.h`) for rich errors, `Stdlib::Result<T>` (`lib/result.h`) for value+error. Build with `MakeError(...)` / `MakeSuccess()`. Always check return values. On error paths, release every resource acquired so far in reverse order. `BugOn(cond)` and `Panic("fmt", …)` for invariant violations.
- **OOM and `operator new`**: plain `new T(...)` **panics on OOM** and never returns `nullptr` — the compiler assumes the plain form is non-null, so a null check after it is dead code. Fallible allocations must use `new (Mm::NoThrow) T(...)` (the compiler keeps that null check) or `Mm::TAlloc<T, Tag>()`, and check the result. See the comment in `mm/new.h` for why the plain form cannot be made nullable.
- **Memory refcounting** is explicit and easy to get wrong — `MapPage` does `+1`, `GetPage` does `+1` (caller must `Put()`), `UnmapPage` is net-0. To free a mapped page: `UnmapPage` + `FreePage` + `Put`. See the table in the cursor rule.
- **Never map DMA/device memory at `physAddr + KernelSpaceBase` directly.** Always go through `Mm::MapPages` / `Mm::AllocMapPages` so the `VaAllocator` tracks the VA. See the allocation API table in `mm/new.h`.
- **The kernel image is W^X on both arches.** Late in boot `ProtectRange` splits it into text RX / rodata RO+NX / data RW+NX (`main.cpp`, `main_arm64.cpp`), and MMIO mappings are NX. Self-modifying code, executing from a heap buffer, or writing through a pointer into `.rodata` faults instead of silently working.
- **Memory barriers**: `Barrier()` is gone. Pick the semantic variant from `hal/barrier.h`: `Hal::SmpWmb/SmpRmb` (CPU↔CPU publish/consume, e.g. seqlock), `Hal::DmaWmb/DmaRmb` (CPU↔device rings/doorbells/OWN bits), `Hal::CompilerBarrier` (compiler-only). On x86 they all compile to a compiler barrier; on arm64 they become `dmb` — misclassification is invisible on x86 and bites on arm64. In Rust, device ordering uses `kcore::barrier::dma_wmb/dma_rmb` (`dmb oshst/oshld` on arm64) — `core::sync::atomic::fence` is NOT a substitute: it emits `dmb ish`, whose inner-shareable domain does not order against a PCIe master. Consuming device-written state needs a read barrier *after* the index/phase/OWN check and before the payload reads (an address-independent load pair is not ordered by a control dependency on arm64) — see `VirtQueue::GetUsed`, nvme `read_cqe`, r8168 `harvest`.
- **Arch discipline**: common code includes `hal/*` headers only. `Hal::` wrappers: `IsInterruptEnabled`, `IrqSave/IrqRestore` (not RFLAGS), `GetSp/SetSp/GetFp` (not GetRsp/GetRbp), `ReadCycleCounter` (not ReadTsc), `IrqEoi/SendIpi/GetCurrentCpuHwId` (not Lapic::), `TlbFlushPage/TlbFlushAll` (not Invlpg/CR3). Per-arch member functions (e.g. `CpuTable::StartAll`) live in arch TUs.
- **Style**: namespaces `Kernel::`, `Kernel::Mm::`, `Stdlib::`. Trace with `Trace(level, "fmt", …)` / Rust `trace!(...)`; level 0 is always visible and per-subsystem level constants (`KbdLL`, `UsbLL`, `PageAllocatorLL`, …) live at the top of `kernel/trace.h` — raise one there to debug a subsystem. No magic numbers — name constants. Preserve existing formatting; don't reflow unchanged code. 4-space indent.
- The C++ build uses `-Wall -Wextra -Werror`, so warnings break the build.
