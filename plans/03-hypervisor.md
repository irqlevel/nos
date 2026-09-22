# Stage 3 — The hypervisor: boot Linux to a serial shell

**Goal:** A Rust type-1 hypervisor running inside `nos` that boots an unmodified
Linux `bzImage` with an initramfs, on one vCPU, to an interactive shell on an
emulated serial port. No BIOS, no disk emulation.

**Depends on:** Stage 1 (crate layout, `GuestMemory`) **and the HAL from
[02-hal-arm64.md](02-hal-arm64.md)**, which now precedes this stage (see the
roadmap [reordering history](README.md#reordering-history)). Runs on Stage 0
hardware, but early bring-up happens under QEMU with nested virtualization (x86)
or HVF (arm64).

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

## Where it stands

Started. Two decisions were taken at the first commit and are worth not
re-litigating; [`docs/hypervisor.md`](../docs/hypervisor.md) is the page that
explains them at length.

**It is a loadable module, not part of the image.** `hvarch` (the CPU's
extension, all of the `unsafe`) and `hv` (everything above it) are workspace
members but not *default* members, so the kernel's own build does not compile
them; they exist only inside `modules/hv` → `hv.ko`. A machine that runs no
guests therefore carries none of it, and above all leaves its CPUs in the
state they booted in. The iteration loop is `insmod`/`rmmod` rather than
rebuild-and-boot, which matters most for exactly the part this document says
the time goes into. And unloading is a gate: a hypervisor that can be taken
out has to prove every time that it turned the extension off before its code
was freed -- which is the same check stage 5 needs, years early.

The cost, stated so it is not discovered later: a module cannot register a
block device or a NIC ([modules.md](../docs/modules.md)). That constrains how
a guest's virtual devices are *served* -- this module emulates them over the
kernel's own disks and NICs, which is what a hypervisor wants anyway -- not
what the hypervisor can do.

**SVM before VMX**, as the section below reasons, and now with the numbers:
QEMU 10.2's TCG reports `svm yes, npt yes, x2apic yes, vgif yes` under
`-cpu max` and `vmx no`, so on the development machine AMD-V is the only
extension a guest can be brought up under at all. It also lacks `nrip-save`
and `decodeassists`, which means the hypervisor works out where an
intercepted instruction ended from a short table of known lengths -- not an
instruction decoder, since every instruction intercepted on purpose has a
length that is known, and an `IOIO` intercept hands over the next
instruction's address regardless. Use `-cpu max`: the default `qemu64`
reports SVM *without* nested paging.

Done so far, against **3.1**:

- `hvarch`: CPUID and the control MSRs; AMD-V and Intel VT-x probed and
  reported feature by feature; the extension turned on and off for a CPU
  (`EFER.SVME` + `MSR_VM_HSAVE_PA`, or `IA32_FEATURE_CONTROL` + `CR4.VMXE` +
  `vmxon`), with the CR0/CR4 fixed bits checked before VMXON rather than
  found out by #GP; an arm64 backend that reports the exception level and
  what stage-2 would offer, and says plainly that EL2 is a boot-path change
  and not a module's.
- `hv`: the machine, the per-CPU pages (allocated in task context, because
  the IPI that hands one over may not allocate) and the enabled mask, both
  under one lock, freed only after the CPU says it is done.
- `modules/hv`: the `hv` command, and a `Drop` that turns the extension off
  and reads back what is left.
- `scripts/hv-test.py`, on both architectures.

Next, in order: the VMCB, a nested page table, and a guest of a few bytes
that exits where it was told to (3.2, 3.3).

Before the first VMX guest, one change outside the hypervisor: the boot path
has to set `CR0.NE` on every CPU. VMX requires it, the APs come out of INIT
without it and nothing sets it, so on the EX44 and the Dell `hv on` refuses
today with `HostState` -- by design, since `vmxon` would fault rather than
fail. The kernel has no x87 code for NE to affect (none in the image, no
floating point in the C++, soft-float Rust), so the change is one bit beside
`EnableWxSupport`'s WP; it goes in with the VMX backend, where a guest can
show it working. [`docs/hypervisor.md`](../docs/hypervisor.md) has the
detail.

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

### 3.1 `hv-arch`: VMX/SVM core (the tedious part, ~300–500 lines unsafe)
- CPUID checks; IA32_FEATURE_CONTROL (VMX) or EFER.SVME/VM_CR (SVM) enable.
- Allocate VMXON region / VMCB; execute VMXON / set up host save area.
- vmexit entry/exit naked stub (save/restore guest GPRs).
- `vmread`/`vmwrite` (VMX) or VMCB field access (SVM) wrappers.

### 3.2 `hv`: VMCS/VMCB configuration (~1–1.5k lines, safe)
- Typed field constants and a builder for host state, guest state, and execution
  controls. **This is the highest-friction part to debug:** Intel SDM vol. 3C
  defines dozens of fields and any mistake yields an uninformative VM-entry
  failure. Add a decoder for the VM-instruction-error field early.
- vmexit dispatcher + exit-qualification decoding.

### 3.3 EPT / NPT (~500 lines, safe)
- Adapt the `mm` page-table walk to EPT/NPT bit layout. Identity-ish map guest
  physical → host physical over the guest's assigned pages.

### 3.4 Linux loader (~500 lines, safe)
- x86 boot protocol: fill `boot_params` (the zero page), place cmdline and
  initramfs, jump to the 64-bit kernel entry point — the Firecracker/kvmtool
  path that skips real mode and BIOS entirely. Reference: rust-vmm
  `linux-loader`.

### 3.5 Minimal device emulation (~1.5k lines, safe) — what actually makes Linux boot
- **8250 UART** via port-I/O exits → the console. Simple: the exit qualification
  for IN/OUT gives everything; no instruction decoder needed.
- **Timer + interrupts:** minimally a LAPIC timer. **Give the guest x2APIC, not
  xAPIC** — x2APIC is MSR-based, and MSR exits need no instruction decoder.
  xAPIC's MMIO APIC page would force writing an x86 instruction emulator (the
  nastiest part of real hypervisors); x2APIC avoids nearly all of it.
- **CPUID / MSR filtering** for the guest (~300 lines).

### Explicitly out of scope for Stage 3
Disk and network emulation (initramfs covers it), SMP guest (one vCPU), real
mode, host-side virtio. All of that is Stage 4.

## Design constraints that Stage 5 depends on (adopt now)

Even though live update is Stage 5, these must be true from the first line of
Stage 3 or they become a painful retrofit:

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
