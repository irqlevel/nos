# nos roadmap: from bare-metal kernel to a Linux-hosting cloud node

This directory holds the design and implementation sequence for turning `nos`
from a KVM guest kernel into a **self-contained, bare-metal cloud node**: a
single ELF that boots on server hardware, runs Linux guests under its own
Rust hypervisor, exposes an HTTP control plane, updates itself live with
near-zero downtime, and eventually runs the same way on arm64.

The guiding idea is **elimination of the L0 Linux host**. In the AWS Nitro
model, a thin bare-metal component owns the hardware and the control plane
while guests run under hardware virtualization. `nos` is unusually well
positioned for this because it *already* contains the pieces a normal
hypervisor has to borrow from a Linux dom0: a TCP/IP stack, DHCP/DNS/HTTP,
virtio drivers, a block layer, filesystems, an SMP scheduler, and a remote
shell. The management plane can live directly in L0, so no Linux is needed on
the host at all.

## Why this is worth doing

- **Tiny TCB.** The whole host is tens of thousands of lines instead of the
  millions in a Linux+KVM host. The host attack surface reduces to the HTTP
  parser and the vmexit / device-emulation handlers.
- **Fast everything.** `nos` boots in milliseconds; direct `bzImage` boot puts
  a Linux guest at a shell in a fraction of a second. Live kernel update of the
  host itself targets sub-100 ms guest downtime.
- **Memory-safe by construction.** New code — including the hypervisor — is
  written in Rust with unsafe confined to a small arch crate, so the worst bug
  class (guest data corrupting host memory) is largely designed out.
- **One demo per stage.** Every stage below produces something showable in 30
  seconds, usually with a single `curl`.

## The stages

Each stage is a separate document, **numbered in execution order** (each stage is
a prerequisite for the next):

| Stage | Document | Outcome |
|-------|----------|---------|
| 0 | [00-bare-metal.md](00-bare-metal.md) | `nos` boots and is manageable on real server hardware (Hetzner-class Intel x86-64) |
| 1 | [01-rust-strategy.md](01-rust-strategy.md) | Cross-cutting: new code in Rust, unsafe confined to a small arch crate |
| 2 | [02-hal-arm64.md](02-hal-arm64.md) | **HAL + arm64 under QEMU/HVF** — a portability foundation, *before* the hypervisor |
| 3 | [03-hypervisor.md](03-hypervisor.md) | A Rust hypervisor (VMX/SVM + EPT/NPT) boots a Linux guest to a serial shell |
| 4 | [04-control-plane.md](04-control-plane.md) | HTTP API creates/destroys/consoles VMs; host-side virtio; guest networking |
| 5 | [05-live-update.md](05-live-update.md) | kexec-style live update of the host kernel under running guests, <100 ms downtime |

### Reordering history

The stages were renumbered from an earlier draft. Originally the hypervisor was
Stage 2 and arm64 was the final Stage 5; the HAL + arm64 work has since been
**pulled forward to Stage 2, before the hypervisor**, and the arm64 *hypervisor*
(EL2 + stage-2) folded into the hypervisor stage rather than being a separate
final stage:

```
was:  bare-metal → rust → hypervisor → control-plane → live-update → arm64
now:  bare-metal → rust → HAL+arm64 → hypervisor → control-plane → live-update
```

The decision and its rationale/trade-offs are recorded in
[02-hal-arm64.md](02-hal-arm64.md#sequencing-decision-the-explicit-reordering).
In one line: doing the HAL now buys a fast HVF-accelerated dev loop on Apple
Silicon for *every* later stage, is a cheaper refactor while the codebase is
small, and makes the eventual arm64 hypervisor backend a contained addition — at
the cost of a detour with no new user-visible capability and two architectures to
keep green.

## Sequencing notes

- **Stage 0 is non-negotiably first.** None of the rest matters if `nos` cannot
  run and stay manageable on real hardware. It also de-risks the biggest
  unknowns (real ACPI/memory maps, real NICs, no serial console).
- **Stage 1 is a discipline, not a milestone.** It applies to every stage after
  it. Adopt the crate layout before writing HAL or hypervisor code, not after.
- **Stage 2 (HAL + arm64) runs before the hypervisor.** This is a deliberate
  reordering (rationale in
  [02-hal-arm64.md](02-hal-arm64.md#sequencing-decision-the-explicit-reordering)).
  Scope is capped at QEMU `virt` + Apple HVF; real arm64 server hardware and the
  arm64 EL2 hypervisor are deferred. Key payoff: a fast HVF dev loop that
  accelerates every stage after it, and a HAL kept honest by a real second
  architecture.
- **Stage 5 (live update) must be designed into Stages 3/4, not retrofitted.**
  Live update is cheap only if all VM state is kept as serializable POD from day
  one. This is the single most important architectural constraint in the whole
  roadmap.
- **The arm64 hypervisor backend is folded into Stage 3**, not a separate final
  stage. Once the HAL exists and the safe `hv` logic crate is written for x86,
  its AArch64 backend (EL2 + stage-2 translation) is a contained addition rather
  than a second project — which is precisely the leverage the reordering buys.

## Current baseline (as of this writing)

What already exists in the tree and is reused by these plans:

- **Boot/SMP:** Multiboot2 entry, 32→64-bit transition, AP trampoline at
  physical `0x8000`, ACPI parsing, LAPIC/IOAPIC, MSI-X, INIT/SIPI AP bringup.
- **Time:** TSC calibrated via HPET or PIT channel 2 (kvmclock optional), so
  timekeeping does not depend on a hypervisor host.
- **MM:** 4-level page tables with a `VirtToPhys` walk, block-based page
  allocator, pool allocator, VA allocator. Kernel is linked at physical 16 MB
  (`build/linker64.ld`, virtual base `0xFFFF800000000000`).
- **Drivers:** serial, VGA text, PIT/HPET/RTC, 8042, PCI, virtio-blk/net/scsi/rng,
  **NVMe (Rust)**, **r8168 (Rust)**. A `TcoWatchdog` exists in `kcore`.
- **Net:** ARP/ICMP/DHCP/DNS/TCP/UDP, HTTP client, **UDP remote shell**.
- **FS:** VFS, ramfs, nanofs (on-disk), ext2 (ro), procfs.
- **Rust:** `ffi → kcore → drivers` layering already in place; unsafe is
  concentrated in `kcore` (~130 mentions / ~1800 lines) while drivers on top are
  ~2–4% unsafe. rustc 1.91, `x86_64-unknown-none`, `panic = abort`.

## Honest scope

This is an engineering project and a portfolio piece, not a product play. As a
commercial "cloud kernel" it cannot outcompete KVM+Firecracker/Cloud Hypervisor
on the open market. The category where a tiny bare-metal hypervisor *is* sold
(defense, avionics, automotive, industrial — Lynx, Green Hills, Bedrock) is
gated behind certification, formal-safety narratives, and multi-year sales
cycles — a venture, not a weekend. The realistic payoff is the work itself and
the career/consulting/content capital it generates. The MIT license and
single-holder copyright keep every future option (relicense, acqui-hire) open.
