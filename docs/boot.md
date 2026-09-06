# Boot

How the kernel gets from the firmware's first instruction to `boot: complete`,
on both architectures. The short version of the flow is in
[CLAUDE.md](../CLAUDE.md); this is the long one, in the order the code runs,
with the ordering constraints that are not obvious from reading it.

Three markers divide the boot, and the smoke tests
(`scripts/smoke-test.sh`, `scripts/smoke-arm64.sh`) assert all three in
order: **`After test`** (the self-tests passed), **`Preempt is now on`**
(the scheduler is live and the APs are up), **`boot: complete`** (the shell
and the network services are running). A `PANIC:` anywhere fails the run.

## x86-64: from GRUB to long mode

GRUB loads `kernel64.elf` over Multiboot2 and jumps to `Start32` in
`arch/x86_64/boot64.asm` — 32-bit protected mode, paging off, the kernel
sitting at its physical load address while being linked at
`0xFFFF800001000000`.

The Multiboot2 header asks GRUB for a 1024x768x32 framebuffer and declares
EGA text acceptable. Both tags are marked optional, which is what lets the
same image boot under BIOS (GRUB keeps text mode) and under UEFI (there is
no text mode, so the framebuffer request is the only way to get a console at
all — without the tag GRUB reports no framebuffer and everything written to
`0xB8000` lands in plain RAM).

`Start32` then:

1. Saves the Multiboot signature (`eax`) and info pointer (`ebx`) into
   `mbsig`/`mbinfo` — the only two registers the protocol hands over.
2. Takes a stack slot out of `.trampolinedata` with a `lock xadd` on
   `stack_counter`. The same code runs on the BSP and on every AP, so the
   allocation has to be atomic; there is one 4 KiB page per CPU, and the
   linker script asserts the whole reservation stays clear of the top of low
   memory.
3. `check_multiboot`, `check_cpuid`, `check_long_mode` — each prints `ERR:`
   and a digit to the screen and hangs, which is all the diagnosis available
   before there is a console.
4. `setup_low_page_tables` / `setup_high_page_tables` build the bootstrap
   map: P4[0] and P4[256] each point at a PDPT of four page directories of
   512 2 MiB pages — the first 4 GiB identity-mapped, and the same 4 GiB
   again at `KernelSpaceBase` (`0xFFFF800000000000`). Everything is mapped
   write-back on purpose: the MTRRs keep the MMIO holes uncached on real
   hardware, and setting PCD here would run the entire kernel uncached on
   bare metal.
5. `enable_paging` loads CR3, sets CR4.PAE and CR4.MCE, sets EFER.LME, then
   turns on CR0.PG **and clears CD/NW in the same write**. A processor comes
   out of INIT with caches disabled; firmware clears that for the BSP long
   before GRUB runs, so only APs arrive here that way — and only on real
   hardware, since KVM and TCG both ignore CD. CR4.MCE matters for the same
   reason: without it a machine check puts the processor into shutdown,
   which the platform turns into a reset indistinguishable from a
   spontaneous reboot.
6. `lgdt gdt64` and a far jump to `long_mode_start`, which zeroes the data
   segment registers and calls `Main(mbinfo)`.

`Main` (`kernel/main.cpp`) points GS at a per-CPU slot that reads as "not
set up yet" (`PerCpuSetupBoot` — a GS base left as whatever firmware had
would make "which CPU am I on" a wild read), switches from the 4 KiB
trampoline stack to this CPU's 32 KiB static stack out of `Stack[MaxCpus][]`,
and calls `Main2`.

## x86-64: `Main2`, on the BSP

The order below is load-bearing; the notes say why.

| Step | Why here |
|---|---|
| `Panicker` / `Watchdog` singletons | Instantiated before anything can need them — static constructors do not run in this kernel |
| Poison the static stacks | `StackProbe::Poison` fills them with a pattern; what survives is what a stack never reached, which is what `stacks` reports later. The stack this code is running on is filled only up to a margin below the current SP |
| `BuiltinPageTable::Setup()` + `SetCr3` | The kernel's own copy of the 4 GiB linear map replaces GRUB's tables, which are about to be overwritten |
| GDT, exception handlers, IDT | From here a fault is a panic report instead of a triple fault |
| PIC remap + disable | The 8259s are moved out of the exception vectors and then masked; interrupts come from the IOAPIC |
| `Dmesg::Setup()` | The ring buffer, so every trace from here is recoverable after the fact |
| `Grub::ParseMultiBootInfo` | The firmware memory map, the framebuffer, the kernel image bounds, and the command line — which is what `Parameters::Parse` reads. Until this point the trace level is necessarily the default |
| `Netconsole::Setup()` | Armed as soon as the command line is known, so the log is buffered from the beginning and can be shipped once the link comes up |
| `BuiltinPageTable::MapHighRam()` | Extends the bootstrap map over RAM above 4 GiB in 1 GiB blocks. It could not run in `Setup()` — there was no memory map yet |
| `PageTable::Setup()` + `SetCr3` + `SetupFreePagesList()` | The real 4-level page table, the page-descriptor array, and the free list. See [Paging](paging.md) |
| W^X | `EnableWxSupport` (EFER.NXE), `SetupMemoryTypes` (PAT entry 4 = write-combining), then `ProtectRange` three times: text RX, rodata RO+NX, data RW+NX. Both must precede the first `MapMmioRegion` — an NX bit without NXE faults as reserved, and a write-combining PTE needs the PAT entry to exist |
| `Screen::Setup()` | Picks EGA text (BIOS) or the pixel framebuffer (UEFI). Needs `MapMmioRegion`, so it cannot be earlier; must be on the BSP before the APs start, so it cannot be later |
| `Acpi::Parse()` | RSDP/RSDT/MADT: the LAPIC address, the IOAPIC, the IRQ→GSI overrides, and one `CpuTable::InsertCpu` per LAPIC entry — which is how the kernel learns how many CPUs it has |
| `PageAllocatorImpl::Setup()` + `AllocatorImpl` | The heap. Nothing may allocate before this line |
| `Hpet::Setup()` | Needs the heap for its MMIO mapping |
| `HaltTcoWatchdog()` | The BIOS may leave the PCH's TCO timer running with a short timeout; left alone it resets the machine during the self-tests |
| `Test::Test()` | The boot self-tests (`kernel/test.cpp`). `After test` is printed immediately after |
| `Pci::Scan()` | Enumerate the bus |
| Console observers | `console=` decides whether the shell listens on the keyboard, the serial line, or both |
| `Lapic::Enable()`, `SetBspIndex` | This CPU's local APIC and its identity |
| `cpu.Run(BpStartup)` | The BSP turns itself into its own idle task and never returns from here |

(The GDT, the exception table and the IDT are re-saved once more after the
page-table switch, between `SetupFreePagesList` and `Screen::Setup`.)

## x86-64: `BpStartup`, the BSP idle task

Everything below runs in task context, which is what lets it allocate, block
and take mutexes.

- The TSS, so `#DF` has an IST stack.
- The IOAPIC is enabled and the tick source chosen: HPET in legacy
  replacement mode if there is one, otherwise the PIT. Both land on the GSI
  that ACPI maps ISA IRQ 0 to. The 8042 keyboard takes vector 0x21, the
  serial port 0x24.
- virtio-blk and virtio-scsi come up, partitions are probed, and `root=auto`
  mounts a ramfs on `/` and the first ext2 it finds on `/boot`.
- `rust_init()` brings up the Rust drivers — NVMe, r8168, r8125, igb.
- `NetFramePool::Setup()` (sized by `netframes=`), then virtio-net and
  virtio-rng. The pool is built before any driver can want a frame.
- The IPI vector (0xFE), the LAPIC timer vector (0xFD) and the LAPIC
  spurious vector (0xFF) get their IDT entries, and interrupts are enabled.
- `TimeInit()` calibrates the TSC against PIT channel 2 and picks a clock
  source (kvmclock → calibrated TSC → PIT).
- `Lapic::CalibrateTimer(100 Hz)` and `StartTimer` — this CPU's own periodic
  tick. It has to precede `StartAll` so an AP can arm its own timer as it
  comes up.
- `CpuTable::StartAll()` unless `smp=off`; `maxcpus=N` caps how many.
- `IrqBalance::Balance()` spreads the device IRQs recorded so far across the
  CPUs that are now running. See [Interrupts](interrupts.md).
- `PreemptOn()` → **`Preempt is now on`**. Until this line `Schedule()` is a
  no-op, and the APs are spinning in `PreemptOnWait()`.
- An IPI round trip to every other CPU, `Test::TestMultiTasking()`,
  `rust_test()`.
- `SoftIrq::Init()` creates one softirq task per running CPU. Block I/O
  completions and the receive path run there, so several things below this
  line would silently do nothing above it: `PartitionDevice::ProbeNew()`
  (the disks that appeared during `rust_init`) and `DiskLog::Setup()` are
  here for exactly that reason.
- `Tcp::Init()`, then xHCI (unless `usb=off`) — on a laptop with no PS/2
  controller the USB keyboard is the only way in, so enumeration happens
  here, synchronously, while trace output still reaches the console.
- The shell task starts, then netconsole and the UDP shell if their
  parameters were given.
- **`boot: complete`**, and the BSP drops into its idle loop, watching for a
  `poweroff`/`reboot` request from the shell.

## x86-64: the application processors

`CpuTable::StartAll` (`arch/x86_64/cpu_start.cpp`) walks the CPUs ACPI
reported and brings them up **one at a time**: INIT, 10 ms, then up to two
SIPIs 200 µs apart pointing at `ApStart16 >> 12`, then poll for up to a
second. One at a time costs the 10 ms INIT delay per CPU and buys two
things — the trace line names the CPU being poked, so a machine that dies
during bring-up says which one it choked on, and the APs no longer race each
other through the dmesg lock and the heap on their way up.

The AP path is the BSP's, minus the parts that only happen once:

`ApStart16` (16-bit real mode, page-aligned below 1 MiB — the SIPI vector is
a page number) → `gdt32` → protected mode → `ap_start32`, which **reloads
SS/DS/ES before touching the stack**: the far jump reloaded only CS, and the
post-INIT descriptor caches would truncate ESP mod 64K and write straight
into the boot page tables → `AllocStack` → `enable_paging` on the same
bootstrap tables → `gdt64` → `ApMain`.

`ApMain2` is where the ordering matters again. Until it loads CR3 this CPU
is still on the boot table, which maps exactly two 4 GiB windows — and the
console may not be in either of them (a Raptor Lake iGPU aperture sits at
`0x40_0000_0000`). An AP has no IDT yet, so a page fault there is a triple
fault: the machine resets during `StartAll` with nothing on the screen and
nothing in any log. Hence: NXE, then the kernel page table, then GDT and
IDT, then the PAT, and only then anything that can print. After that the
LAPIC, kvmclock for this vcpu, its TSS, and `cpu.Run(ApStartup)`.

`ApStartup` arms this CPU's LAPIC timer, enables interrupts, publishes
itself as running, waits for `PreemptOn` on the BSP, runs the multitasking
self-test and enters the idle loop.

`ApStartedFlag` is incremented at five points along that path; if `StartAll`
gives up, its value says how far the AP got.

## arm64: from the Image header to EL1

The arm64 build boots over the Linux `Image` protocol: `_head` in
`arch/arm64/boot.S` carries the `ARM\x64` magic and the load offset, and the
loader (QEMU, or a real bootloader) enters at `Start64` with the DTB pointer
in `x0`, MMU off, at the load address.

- If entered at EL2, drop to EL1h: `HCR_EL2.RW` (EL1 is AArch64),
  `CNTHCTL_EL2` so the timers are usable at EL1, `CNTVOFF_EL2 = 0`, and
  `ICC_SRE_EL2` so GICv3 system-register access from EL1 does not trap —
  that one resets with Enable=0 on real hardware.
- A known-good `SCTLR_EL1` either way: a direct EL1 entry inherits the
  implementation's reset value, and SA/A/WXN/EE vary by part.
- Zero BSS — the bootstrap tables live in it.
- Build the bootstrap translation, mirroring x86's: **TTBR1** L0[256] → L1
  with four 1 GiB blocks at `KernelSpaceBase + phys`, block 0
  Device-nGnRE (so the PL011, the GIC and the virtio-mmio slots are visible
  immediately) and blocks 1–3 Normal write-back; **TTBR0** an identity 1 GiB
  block over the kernel, live only for the instant the MMU turns on.
- MAIR and TCR (48-bit both halves, 4 KiB granules), then clean the four
  table pages out of the D-cache by line — the stores went straight to
  memory with the MMU off, but reset-time cache contents are architecturally
  UNKNOWN and could shadow them once SCTLR.C goes on — invalidate TLB and
  I-cache, and set SCTLR.M|C|I.
- Branch to the link-time (virtual) address, then set **TCR.EPD0** to
  disable TTBR0 walks for good. A live identity alias would keep kernel RAM
  writable *and* executable at its physical address, straight past W^X, and
  would let wild low-half pointers hit RAM instead of faulting.
- Cache the CPU index in `TPIDR_EL1`, set SP to `BootStackTop`, and call
  `MainArm64` with the DTB as a linear-map VA.

## arm64: `MainArm64` and `BpStartupArm`

`MainArm64` (`arch/arm64/main_arm64.cpp`) is the `Main2` twin. It installs
the EL1 vectors, poisons the boot stack, and parses the device tree
(`Board::Setup`) for memory regions, the CPU list, the PL011, the GIC
distributor/redistributors, the ITS, the virtio-mmio slots, the PCIe ECAM
window and the kernel command line. `TPIDR_EL1` is then re-seeded with this
CPU's *linear* index from the DTB CPU list — `boot.S` could only guess
MPIDR.Aff0, which is right on QEMU virt and wrong on clustered-affinity
hardware.

From there it follows the x86 sequence: early PL011, dmesg, parameters,
netconsole, bootstrap map + `MapHighRam`, the real page table, W^X, the page
allocator, `TimeInit`, `Test::Test()`, then the GICv3, the ITS (unless
`its=off`), PCIe over ECAM, the CPU table from the DTB, and
`cpu.Run(BpStartupArm)`.

One arm64-only step: `InstallEarlyDeviceBlock` copies the device GiB into
the real table *before* the translation root is switched, so the UART does
not disappear mid-boot.

`BpStartupArm` sets up the generic timer (per-CPU by construction, so
`SetPerCpuTimer` is unconditional), the PL011 interrupt, the virtio-mmio
devices discovered from the DTB slots, then `StartAll`, `PreemptOn`, the
self-tests, `rust_init`/`rust_test`, softirqs, TCP, the shell, netconsole
and the UDP shell — and prints `boot: complete`.

APs come up through PSCI `CPU_ON`, with the CPU index passed as the context
argument, landing at `SecondaryEntry`. The BSP publishes the kernel
translation root in `Arm64ApTtbr1` and a stack top per CPU in
`Arm64ApStackTop[]`, and cleans both to the point of coherency — the
secondary reads them with the MMU and caches off. `ApMainArm64` then
installs the vectors, runs `Gic::CpuInit` for its own redistributor, and
enters `ApStartupArm`, which arms its own generic timer and idles.

## What to do when it does not boot

Bisect with the kernel command line rather than with rebuilds — see
[Kernel parameters](kernel-parameters.md):

- `smp=off` / `maxcpus=2` — is it the AP bring-up?
- `loglevel=4` — the boot itself is the chatty part; the runtime `loglevel`
  command cannot help with a boot that already happened.
- `console=vga` / `console=serial`, `usb=off`, `its=off` — take a subsystem
  out of the path.
- `netconsole=ip:port` with `nctail=N` — on a machine with no serial port,
  this is the only way to see the log of a boot that dies. See
  [Netconsole](netconsole.md) and [Debug](debug.md).
