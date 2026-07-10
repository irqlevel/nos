# Stage 2 — The hypervisor: boot Linux to a serial shell

**Goal:** A Rust type-1 hypervisor running inside `nos` that boots an unmodified
Linux `bzImage` with an initramfs, on one vCPU, to an interactive shell on an
emulated serial port. No BIOS, no disk emulation.

**Depends on:** Stage 1 (crate layout, `GuestMemory`) **and the HAL from
[05-arm64.md](05-arm64.md)**, which now precedes this stage (see the roadmap
[execution order](README.md#execution-order)). Runs on Stage 0 hardware, but
early bring-up happens under QEMU with nested virtualization (x86) or HVF (arm64).

**arm64 backend folds in here.** With the HAL and the safe `hv` logic crate in
place, the AArch64 hypervisor — running at **EL2** with **stage-2 translation**
(`HCR_EL2`/`VTTBR_EL2`/`VTCR_EL2`, `ESR_EL2` syndrome decoding, Linux `Image` +
DTB loader) — is a second `hv-arch` backend under the same safe logic, not a
separate stage. The x86 VMX/SVM path below is written first; the arm64 backend
reuses everything above the arch line.

**Demo, staged:** (1) VMXON + a trivial 16-byte guest halts as expected → (2) EPT
+ a guest in long mode → (3) `bzImage` prints its early console → (4) full boot
to a shell with initramfs.

---

## Choose VMX vs SVM based on the dev environment

This choice is driven by where you actually iterate:

- **TCG (Apple Silicon Docker, x86 emulated):** TCG emulates **AMD SVM** reliably
  and for a long time; its **Intel VMX** emulation is newer and incomplete. If
  the main dev loop is TCG, **start with SVM** — it is also simpler (the VMCB is
  a plain in-memory struct; no `vmread`/`vmwrite` ceremony).
- **Linux/Intel host with nested KVM (`kvm_intel nested=1`):** works well; pick
  **VMX**, which matches the Intel target clouds (GCE nested virt on Intel;
  Yandex Cloud is harder).

The safe logic layer (`hv`) is written to be vendor-agnostic; only `hv-arch`
differs. Picking one to start does not lock out the other.

## What already exists to build on

- `kernel/asm.asm` already exports nearly everything VMCS setup needs: `ReadMsr`/
  `WriteMsr`, `GetCr0/3/4`, `StoreGdt`/`StoreIdt`, `LoadTr`, segment registers.
  (In Rust, `hv-arch` re-implements these via `asm!` rather than calling the C++
  versions.)
- **EPT is the same 4-level walk as `mm/page_table.h`,** with different bits. The
  walk and page-allocation logic port almost verbatim. EPT does **not** require
  physically contiguous guest memory, so the existing page allocator works as-is.
- A **vCPU is just a scheduler task** running `vmresume → handle vmexit → repeat`.
- `boot/grub.h` already parses Multiboot2 **modules** — the simplest way to
  deliver `bzImage` and initramfs into memory: two `module2` lines in grub.cfg,
  no disk reads.

## Work items

### 2.1 `hv-arch`: VMX/SVM core (the tedious part, ~300–500 lines unsafe)
- CPUID checks; IA32_FEATURE_CONTROL (VMX) or EFER.SVME/VM_CR (SVM) enable.
- Allocate VMXON region / VMCB; execute VMXON / set up host save area.
- vmexit entry/exit naked stub (save/restore guest GPRs).
- `vmread`/`vmwrite` (VMX) or VMCB field access (SVM) wrappers.

### 2.2 `hv`: VMCS/VMCB configuration (~1–1.5k lines, safe)
- Typed field constants and a builder for host state, guest state, and execution
  controls. **This is the highest-friction part to debug:** Intel SDM vol. 3C
  defines dozens of fields and any mistake yields an uninformative VM-entry
  failure. Add a decoder for the VM-instruction-error field early.
- vmexit dispatcher + exit-qualification decoding.

### 2.3 EPT / NPT (~500 lines, safe)
- Adapt the `mm` page-table walk to EPT/NPT bit layout. Identity-ish map guest
  physical → host physical over the guest's assigned pages.

### 2.4 Linux loader (~500 lines, safe)
- x86 boot protocol: fill `boot_params` (the zero page), place cmdline and
  initramfs, jump to the 64-bit kernel entry point — the Firecracker/kvmtool
  path that skips real mode and BIOS entirely. Reference: rust-vmm
  `linux-loader`.

### 2.5 Minimal device emulation (~1.5k lines, safe) — what actually makes Linux boot
- **8250 UART** via port-I/O exits → the console. Simple: the exit qualification
  for IN/OUT gives everything; no instruction decoder needed.
- **Timer + interrupts:** minimally a LAPIC timer. **Give the guest x2APIC, not
  xAPIC** — x2APIC is MSR-based, and MSR exits need no instruction decoder.
  xAPIC's MMIO APIC page would force writing an x86 instruction emulator (the
  nastiest part of real hypervisors); x2APIC avoids nearly all of it.
- **CPUID / MSR filtering** for the guest (~300 lines).

### Explicitly out of scope for Stage 2
Disk and network emulation (initramfs covers it), SMP guest (one vCPU), real
mode, host-side virtio. All of that is Stage 3.

## Design constraints that Stage 4 depends on (adopt now)

Even though live update is Stage 4, these must be true from the first line of
Stage 2 or they become a painful retrofit:

- **All VM state is serializable POD:** vCPU register set (read out of VMCS/VMCB),
  emulated-device state, and a single table mapping guest physical pages to their
  owning VM.
- **Device emulation holds no pointers into arbitrary kernel memory** — only
  indices/handles that survive a re-init.

## The hard parts, honestly

Most time goes not to code volume but to debugging VMCS/VMCB setup (VM-entry
failures with poor diagnostics) and Linux boot-protocol quirks. Build the
VM-instruction-error decoder and a "dump guest state on unexpected exit" helper
before you need them.

## Effort

**1–3 months of hobby time** to first `dmesg` + shell, decomposed into the four
visible sub-demos above.

## Exit criteria

- Trivial guest runs under VMXON/vmrun and exits as expected.
- A long-mode guest runs under EPT/NPT.
- `bzImage` reaches its early console.
- Full boot to an interactive shell over the emulated UART, with initramfs, on
  one vCPU.
