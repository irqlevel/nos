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

And against **3.2 and 3.3**, under AMD-V -- the first two of the four demos:

- `hvarch`: the VMCB, laid out from appendix B with every offset checked at
  compile time; the run stub, a naked function that does `vmsave`/`vmload`
  of the host's FS/GS/TR/LDTR and syscall MSRs around `vmrun` with GIF clear
  (this kernel's per-CPU data is at the GS base: without them the first
  per-CPU read after an exit faults, which the gate shows); and
  `Guest::run`, which sets on every entry what keeps the host the host's --
  the intercepts of the host's interrupts, SHUTDOWN, I/O, MSRs, INVD,
  XSETBV and the SVM instructions, of #DB and #AC (whose delivery a guest
  can make loop forever inside the CPU) and of #MC (raised again into the
  host's handler, since the CPU will not), interrupt masking by the host's
  flag, nested paging, all-ones permission maps, AVIC/SEV/virtual VMSAVE
  off, the ASID and the TLB control the CPU it is on hands out (per-CPU
  generations, a full flush when a generation ends; a full flush on every
  entry until 2026-09) -- and refuses a CPU with five-level paging, which
  a four-level nested table would be walked as; so the policy above it can
  be wrong about a guest without being wrong about the host.
- `hv`: `GuestMemory` with copying volatile accessors and the nested table
  inside it, mapping only pages it owns; the nested page table as an arena
  walked by index; the VMCB's policy, the exit decoder, and a software
  consistency checker that names the rule a VMCB breaks before `vmrun` can
  answer with a bare `VMEXIT_INVALID`; the VM.
- Built-in guests, all in long mode: port I/O and CPUID, 4 GiB and every
  register across a hypercall, a write past its memory stopped at the
  nested table, a triple fault, a refused VMCB, and `cli; jmp $` stopped by
  the host. `hv run <guest|all> [cpu]` runs them on a task of their own,
  bound to a CPU when asked; `hv-test.py` runs them all on the first and the
  last CPU.
- Found on the way: QEMU's TCG before 9.2 does not translate an unpaged
  guest's addresses through the nested table -- a real-mode guest there
  runs out of host memory -- which is why every guest starts paged.

And against **3.4 and 3.5** -- the third demo, a `bzImage` printing its early
console (`hv boot`, `scripts/hv-linux-test.py`):

- guest memory is now [frames](../docs/hypervisor.md#a-linux-guest) -- pages
  of RAM mapped nowhere but the guest's nested table (`kcore::frame`), not
  512 KiB runs of the allocator's largest bucket, so a guest is as large as
  the machine has RAM and costs no kernel address space;
- `hv::linux` loads a bzImage by the 64-bit boot protocol -- the setup header
  into a zero page, the command line, an e820 map, identity page tables and a
  GDT, all in guest memory -- and streams the kernel and initrd from a file a
  chunk at a time (`kcore::fs::read_at`);
- `hv::policy` is the CPUID and MSR policy: a CPU cut down to what is
  emulated, the system MSRs served from the VMCB save area;
- `hv::devices` is the 8250, an 8254 PIT and an MC146818 RTC -- the last two
  because without them a guest spins on a counter that never counts and an
  update bit that never clears;
- the guest's FPU/SSE state and XCR0 are switched around `vmrun`
  (`hvarch::x86::svm`), and the vCPU runs on a task of its own.

And against **3.5 and the fourth demo** -- a full boot to a shell:

- an 8259 PIC pair (`hv::devices::pic`) the guest takes its interrupts from,
  since it runs with no local APIC;
- the PIT's channel 0 raising IRQ0, the system tick, and interrupt injection
  (`Vcpu::inject_extint` / `request_irq_window`): the run loop injects the
  highest-priority IRQ when the guest can take one and asks the CPU (SVM's
  VINTR) to exit the moment it can when it cannot;
- an idle `HLT` halts the vCPU: stepped past, as a CPU an interrupt wakes
  resumes after it, and not entered again until an interrupt is pending,
  its task asleep until the timer's next edge -- a halted guest costs its
  CPU nothing (on the Linux guest: 5.6 million HLT exits in 120 s became
  11,410, the task asleep 89% of the run, the timer ticks unchanged);
- the 8250 raising IRQ4 for its transmitter, and a receive path with a
  cursor-query answer, so the console can be typed at (`hv boot ... input=`).

A tinyconfig Linux 6.18 with a BusyBox initramfs now boots to an interactive
`ash` prompt. All four demos are done; `docs/hypervisor.md` ("What comes
next") has what is left.

And a first step toward stage 4's lifecycle, as shell commands before it is
an HTTP API: guests that run until they are stopped
([`docs/hypervisor.md`](../docs/hypervisor.md#guests-that-stay-up)).
`hv start` puts a guest on a vCPU task of its own and returns; `hv list`,
`hv console`, `hv send`, `hv exec`, `hv wait` and `hv stop` reach it while it
runs. `hv exec` types a line and returns the answer at the prompt printed
after the line went in; `hv off` will not turn the extension off under a
running guest, and `rmmod hv` stops every guest before it turns it off. On
the way, the 8250's answer to the cursor query became the cursor's real
column (it said 80, and BusyBox wrapped what was typed) from a buffer of its
own (the old queue grew with every query a guest never read).

Before the first VMX guest, one change outside the hypervisor: the boot path
has to set `CR0.NE` on every CPU. VMX requires it, the APs come out of INIT
without it and nothing sets it, so on the EX44 and the Dell `hv on` refuses
today with `HostState` -- by design, since `vmxon` would fault rather than
fail. The kernel has no x87 code for NE to affect (the C++ is built with
`-mno-80387`, the Rust is soft-float), so the change is one bit beside
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

All met, under AMD-V (SVM); the VMX backend and the arm64 EL2 backend remain.

- ~~Trivial guest runs under VMXON/vmrun and exits as expected~~ — `hv run`.
- ~~A long-mode guest runs under EPT/NPT~~ — NPT, `hv run hypercall`.
- ~~`bzImage` reaches its early console~~ — `hv boot`.
- ~~Full boot to an interactive shell over the emulated UART, with initramfs,
  on one vCPU~~ — a BusyBox shell that runs a command typed at it
  (`hv boot ... input='id\n'` → `uid=0 gid=0`). Gate:
  `scripts/hv-linux-test.py`.
