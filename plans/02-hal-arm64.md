# Stage 2 — HAL + arm64: a portability foundation before the hypervisor

> **Reordered.** This stage was originally the final one (arm64 last). It has
> been pulled forward to run **after Stage 1 (Rust strategy) and before Stage 3
> (hypervisor)**, and the docs were renumbered so stage numbers match execution
> order. See [README.md](README.md#reordering-history) for the roadmap and
> "Sequencing decision" below for the rationale.

**Goal (this stage):** Introduce a hardware abstraction layer (HAL) and bring
`nos` up on **arm64 under QEMU `virt` + Apple HVF**, so the codebase runs on two
architectures. Real arm64 server hardware and the arm64 EL2 hypervisor are
explicitly deferred (see "Deferred" below).

**Why arm64 at all:** It is increasingly the server-CPU direction (AWS Graviton,
Ampere Altra, Azure Cobalt, Google Axion). But the *near-term* reason to do it
now is not the server market — it is the dev loop and the refactor economics
below.

---

## Sequencing decision (the explicit reordering)

**Original roadmap:** arm64 was the last stage, after the hypervisor and live
update. **Revised roadmap:** the HAL refactor plus arm64-under-QEMU is pulled
**forward to Stage 2, before the hypervisor (now Stage 3).**

### Why earlier is better

1. **Dev-loop acceleration — the decisive reason.** The main dev machine is
   Apple Silicon. Today x86 `nos` runs under **QEMU TCG** (slow emulation). An
   **arm64 `nos` runs under native HVF acceleration** — the host CPU *is* arm64.
   Every subsequent stage (including all hypervisor debugging) gets a
   dramatically faster iteration loop. Paying for this early compounds across
   everything after it.
2. **Refactor economics.** The codebase is ~42k lines of C++ today. Introducing
   an arch-abstraction split is cheaper now than after the hypervisor adds
   thousands more arch-touching lines. Refactor cost grows with the code.
3. **A second architecture keeps the HAL honest.** With one architecture, a
   "HAL" inevitably rots into x86 with renamed symbols. A real second backend is
   the only true test that the abstraction is clean — and it forces the logic
   layers (net/fs/mm/block) to stay genuinely arch-independent, which directly
   benefits the hypervisor's portability later.
4. **Rust synergy.** The natural home for per-arch assembly is a Rust
   **`hal-arch`** crate (`global_asm!` / `naked_asm!`), the same pattern as the
   `hv-arch` crate in [01-rust-strategy.md](01-rust-strategy.md). Doing the HAL
   first establishes that pattern and moves the project off its NASM dependency
   before the hypervisor needs its own arch crate.
5. **The eventual arm64 hypervisor becomes cheap.** Once the arch tax is paid,
   Stage 3's EL2 + stage-2 arm64 backend is a small addition rather than a second
   project.

### The trade-off, stated honestly

- It is a **detour with no new user-visible capability**: `nos` on arm64 does
  exactly what `nos` on x86 does. This is an investment in velocity and future
  portability, not a feature.
- It creates **two architectures to keep green** from now on — roughly double the
  boot-test/CI burden for every later change.
- The **arm64 implementation is large** (see cost below). The HAL abstraction is
  the small part; the arm64 backend behind it is the bulk of the work.

**Decision:** accept the detour. The HVF dev-loop win plus cheaper-now refactor
economics outweigh the delay to the hypervisor, *provided* arm64 scope is capped
at QEMU/HVF (not real hardware) for this stage.

---

## What the code already gives us (good news)

- **The logic layer is already nearly arch-clean.** Outside `drivers/` and
  `boot/`, direct arch code (inline asm, port I/O, MSR, cpuid) appears in only ~5
  files (`cmd.cpp`, `asm.h`, `tsc.cpp`, `main.cpp`, `symtab_data.cpp`). So
  net/fs/block/lib/mm logic barely needs freeing — exactly what a HAL should
  expose, and it is mostly already exposed.
- **Rust ports more easily:** `x86_64-unknown-none` → `aarch64-unknown-none`,
  inline asm split via `#[cfg(target_arch)]`. The **NVMe driver is
  transport-agnostic and reused as-is**. `kcore` needs arm variants for its arch
  bits (cpu, interrupt wrappers) but its shape is unchanged.

## What the HAL must abstract — and where the real cost is (sober news)

A HAL is an *interface*; each arch still needs a full *implementation* behind it,
and that implementation is the expensive part:

- **NASM is the sharpest blocker.** All three `.asm` files (`boot/boot64.asm`,
  `kernel/asm.asm` — 128 exported symbols, `lib/stdlib_asm.asm`) are NASM, which
  is x86-only. They must be **rewritten**, not abstracted, for a different
  assembler and ISA. Use this as the reason to move the small arch asm into the
  Rust `hal-arch` crate (`global_asm!`/`naked_asm!`) and drop the NASM
  dependency.
- **MMU:** `mm/page_table.cpp` is bound to x86-64 PTE bits. arm64 uses
  long-descriptor tables (4 KB granule) — the walk is re-implemented; the
  allocator/VA logic above it is reused.
- **Interrupt controller:** LAPIC/IOAPIC/MSI-X → **GICv3/GICv4** (distributor +
  redistributors + CPU interface, LPIs via an ITS). A substantial new driver,
  not a wrapper — the single biggest arm64 item.
- **Timers:** TSC/PIT/HPET → the **ARM generic timer** (`CNTV`/`CNTP`). Actually
  *simpler* than x86 TSC calibration; architecturally specified.
- **Boot / firmware:** Multiboot2/GRUB → **UEFI + ACPI** (SBSA) or **device
  tree**. A completely different entry path and early init.
- **Exceptions:** IDT/vectors → AArch64 exception vectors and exception levels.
- **Serial:** 8250 → **PL011**.

## Work items

Split into the reusable, low-regret refactor (A) and the large implementation (B).

### A. HAL refactor of the existing x86 code (no behavior change)
- Introduce `arch/x86_64/` and `arch/arm64/` and a set of HAL interfaces:
  **CPU** (barriers, TLB, IRQ enable/disable, halt, pause, atomics),
  **MMU** (map/unmap/walk in arch-neutral terms over arch-specific PTE bits),
  **IRQ controller** (mask/unmask/EOI/route/send-IPI), **timer**, **boot/early
  init**, **console/serial**.
- Move `kernel/asm.asm` behind the CPU HAL; relocate the small arch asm into a
  Rust `hal-arch` crate per arch. Isolate `page_table.cpp` PTE bits behind the
  MMU HAL. Wrap LAPIC/timer access behind the IRQ/timer HALs.
- **Main risk: regressions in the working x86 build.** Prerequisite: a fast
  automated boot smoke-test (QEMU + serial/UDP-shell assertion) to run on every
  refactor step.

### B. arm64 bring-up behind the HAL (to QEMU `virt` + HVF only)
- Boot entry via UEFI or DTB; parse the memory map from ACPI/DT.
- MMU: long-descriptor page tables; enable the MMU; kernel VA layout.
- Exception vectors and context switch (AArch64 register/exception model).
- **GICv3** driver; **generic timer**; **PL011** console.
- virtio-mmio (QEMU `virt` exposes virtio over MMIO) — reuse the existing virtio
  logic against the new transport.
- Rust: add `aarch64-unknown-none`; `#[cfg(target_arch)]` for `kcore` arch bits;
  confirm NVMe/virtio reuse.

## Deferred to a later stage (NOT in scope here)

- **Real arm64 server hardware** (SBSA boxes, Ampere/Graviton-class). This is a
  second bare-metal hardening effort analogous to
  [00-bare-metal.md](00-bare-metal.md) and is deliberately postponed — chasing it
  now is a rabbit hole that QEMU/HVF avoids.
- **The arm64 hypervisor** (run at **EL2** with **stage-2 translation**,
  `HCR_EL2`/`VTTBR_EL2`/`VTCR_EL2`, `ESR_EL2` syndrome decoding, Linux `Image` +
  DTB loader). This is folded back into the hypervisor stage: once the HAL exists
  and the safe `hv` logic crate is written for x86, its AArch64 backend is a
  contained addition. Study refs: **KVM/arm64** (`arch/arm64/kvm`),
  **kvmtool/crosvm** arm64, **Cloud Hypervisor** arm64, the **ARM ARM** for EL2 /
  stage-2 / GIC / generic timer.

## Effort (hobby time)

- **A — HAL split of existing x86:** ~2–4 weeks of evenings. Mechanical but
  wide; the risk is regressions, so the boot smoke-test comes first.
- **B — arm64 bring-up to "boots + serial + timer + IRQ + virtio under QEMU":**
  ~2–4 months of evenings. This is the bulk, and it is *implementation*, not
  abstraction. GICv3 and UEFI/DTB entry are the meatiest pieces.
- **Total to "`nos` runs on x86 and arm64 under QEMU": ~3–5 months**, most of it
  the arm64 backend, not the HAL.

## Exit criteria

- `arch/x86_64` and `arch/arm64` exist behind stable HAL interfaces; the x86
  build is byte-for-byte behaviorally unchanged (boot smoke-test green).
- Per-arch assembly lives in the Rust `hal-arch` crate; the NASM dependency is
  gone (or reduced to nothing on the arm64 path).
- `nos` boots on QEMU `virt` (and under HVF on Apple Silicon) to an interactive
  UDP/serial shell, with GICv3 interrupts, generic timer, PL011 console, and
  virtio-mmio all functional.
- The logic layers (net/fs/mm/block/lib) compile and run unmodified on both
  arches — proof the HAL is real.
