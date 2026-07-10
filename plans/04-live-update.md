# Stage 4 — Live host-kernel update (kexec-style) under running guests

**Goal:** Replace the running `nos` host kernel with a new build **without
killing the guests** — a warm handover that keeps guest RAM in place, restores
each vCPU and device, and resumes with sub-100 ms guest downtime.

**Depends on:** Stage 2/3, *and* on the POD-state / drainable-device discipline
those stages were required to follow.

**Demo:** Kernel update of the hypervisor under live Linux guests via one
`curl -X POST http://nos-host/system/update`, guest downtime ~50 ms — a hikkup,
not a reboot. This is what AWS (Nitro live update) and VMware (Quick Boot) can
show and no hobby project can.

---

## Why this is *easier* for `nos` than for Linux

Linux's equivalents (mainline KHO/Kexec HandOver + Live Update Orchestrator,
VMware ESXi Quick Boot, Hyper-V hot restart, AWS Nitro live update) are hard
because of enormous kernel state and retrofit. `nos` has crushingly small state
and total control of its own drivers, so the mechanism can be designed in rather
than bolted on.

## Key insight: guest RAM is never copied

Guest RAM is just physical pages. The entire trick is that the **new kernel must
not touch them** during boot. Only a small **manifest** is carried across:

- vCPU register sets (read out of VMCS/VMCB before the jump),
- emulated-device state (POD structs),
- the table mapping guest-physical pages → owning VM,
- host DHCP lease and minimal net config.

Kilobytes, not gigabytes.

## The update sequence

1. New kernel image arrives via the same HTTP API (`POST /system/update`).
2. Pause vCPUs; `VMCLEAR`; write guest state into the manifest at a known
   physical address. **The manifest format is versioned** — the old kernel writes
   it, the new kernel reads it, and the version guards against layout drift.
3. **Drain DMA.** Wait for in-flight NVMe and NIC requests to complete. This is
   where owning every driver pays off: device queue state need not be migrated —
   quiesce the devices and re-init them after the jump. (Stage 3 device models
   must be *drainable* for the same reason.)
4. Stop APs. Then a classic kexec trampoline: the kernel is linked at a fixed
   physical address (16 MB — `build/linker64.ld`, virtual base
   `0xFFFF800000000000`), so the new image is first staged elsewhere, and a tiny
   relocatable stub copies it over the old kernel and jumps to the entry point
   with a "warm handover" flag. **Precedent already in the tree:** the AP
   trampoline at physical `0x8000` in the same linker script.
5. New kernel boots (milliseconds for `nos`), sees the manifest, and **excludes
   the saved pages from the page allocator** — the one capability not yet present:
   `PageAllocatorImpl::Setup()` must accept a list of reserved ranges. (Stage 0
   already needed this for the E820/EFI reserved regions, so it is shared work.)
   Then it rebuilds VMCS/EPT from the manifest and `VMRESUME`s each guest.

## Nice physical properties

- **TSC is not reset** across the jump (no real CPU reset), so via `TSC_OFFSET`
  in the VMCS the guest sees monotonic time and no clock jump.
- **Guest TCP connections live in guest memory** — they survive the update.
- Net guest downtime = DMA drain + device re-init + kernel load ≈ **<100 ms**,
  seen by the guest as a scheduler hiccup.

## Safety net — mandatory, especially on Hetzner with no IPMI

A failed jump into a new kernel on a remote box with no console is a dead box.

- **TCO watchdog:** `kcore` already has `TcoWatchdog` (`probe`/`start`/`kick`).
  Arm it before the jump; the new kernel must reach "guests resumed, network up"
  and `kick()` it, or the hardware resets.
- **A/B on disk:** GRUB with fallback to the previous image (boot counter). Honest
  v1 rollback semantics: failed update → cold reboot → guests die but the machine
  stays alive and manageable.

## Design constraints (the whole reason Stages 2–3 followed the POD rule)

- All VM state is serializable POD.
- Guest pages are tracked in a single ownership table.
- Device emulation holds no pointers into arbitrary kernel memory, only
  handles/indices that survive re-init.
- Every device model is drainable.

If these hold, the feature is nearly free. If retrofitted, expect a miniature
version of Linux's KHO pain.

## Effort

The kexec mechanism itself (load ELF, trampoline, warm flag in the boot
protocol) is **a couple of weekends** *after* `nos` runs on hardware. The
handover part (manifest, drain, reserved-ranges allocator, VMCS/EPT restore) is
**a few weeks of evenings** — *conditional* on Stages 2–3 having been built to
the POD/drainable discipline.

## Exit criteria

- Host kernel is replaced under a live guest with no guest reboot.
- Guest sees monotonic time (TSC offset preserved) and keeps TCP connections.
- Measured guest downtime < 100 ms.
- Watchdog + A/B fallback verified by deliberately shipping a broken update and
  observing automatic recovery.
