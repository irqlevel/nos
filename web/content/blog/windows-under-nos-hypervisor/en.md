---
title: "Windows on nos: firmware, synthetic timers and 61% fewer exits"
description: "How Windows Server 2022 went from a UEFI shell and failed VM entries to SAC, networking, save/restore and a measured 61% reduction in idle VM exits."
date: 2026-11-22
tags:
  - hypervisor
  - rust
  - x86-64
  - hardware
  - debugging
  - qemu
---

The first useful Windows boot on nos did not start with Windows Setup. It started with a 4 MiB firmware image, a processor register that stopped the firmware before it could print anything, and a serial console.

Today a prepared Windows Server 2022 Standard Core image boots under `hv.ko` to the Special Administration Console (SAC) in about 1.7 seconds on the Intel EX44. Its virtio disk driver is running, its network interface gets an address by DHCP, and ping reaches nos. We can open a CMD channel, save and restore the running VM, and issue `shutdown /s /t 0` instead of pulling the virtual plug. RDP's Hybrid/NLA negotiation also completes through `hv forward` once the guest firewall allows TCP 3389.

That is the useful part. It is not yet a claim that every Windows version installs or runs on nos. The supported path today is Windows Server from a prepared image with virtio-win already installed. Installation from an ISO inside nos, a graphical console, persistent UEFI variables, Secure Boot and TPM are still ahead. This post is the story of what it took to get here, including the parts that did not work first.

## W0: firmware before Windows

Our existing Linux path could load a kernel directly, or start the small `hvboot` program to read a Linux cloud image's own `/boot`. Neither helps with a Windows installation disk. Windows expects a PC platform and UEFI firmware, so we decided to run EDK2's OVMF `CloudHv` build as a PVH guest. Cloud Hypervisor already uses this route; our existing PVH loader could carry the firmware without growing a second firmware-loading mechanism.

W0 turned out to be seven pieces of platform work:

| Step | What it added |
|---|---|
| W0.1 | Reproducible RELEASE and DEBUG firmware builds from a pinned EDK2 source, with licenses and system-image packaging |
| W0.2 | Q35 for ordinary guests, and the distinct machine CloudHv firmware expects |
| W0.3 | Per-vCPU fixed and variable MTRRs, including `MTRR_DEF_TYPE` and its reset and fault rules |
| W0.4 | Virtio MSI-X BARs that the firmware can move without losing the host's backing or migration state |
| W0.5 | `firmware` in the guest commands and `vm.toml`, plus firmware save, restore and migration state |
| W0.6 | Bring-up on real VT-x, a UEFI shell, and Ubuntu booting through its own shim and GRUB |
| W0.7 | A repeatable UEFI gate: Boot Manager, a command typed into the shell, and save/restore of the running firmware VM |

The firmware build is pinned to `edk2-stable202608`. It has no SMM, TPM, Secure Boot or network iSCSI stack, but it does include the UEFI shell. `scripts/build-cloudhv.sh --check` rebuilds the RELEASE image and compares it byte for byte. That mattered while bring-up was changing daily: a surprising firmware symptom could be chased in source instead of blamed on an untracked binary.

Q35 arrived for ordinary guests as part of this work. They now get a modern PCI machine with ECAM for 256 buses. CloudHv is deliberately different: it identifies its platform by host bridge device ID `0x0D57`, uses CF8/CFC rather than ECAM, and expects low RAM to end no higher than `0xC0000000`. One "UEFI machine" setting would have hidden these differences behind a flag. The two layouts remain explicit in the machine description.

### The failures before the first shell

The firmware's SEC phase writes `MTRR_DEF_TYPE` before it has a console. Without MTRR support the write raised `#GP`, and the guest simply stopped. There was no firmware error to read because the code that could print one had not run yet.

Then the firmware began doing what firmware is supposed to do: enumerate PCI and move BARs. A virtio device's MSI-X table lives in a memory BAR, but the host cannot just follow the guest's address. The table has a stable host backing for device emulation and saved state, while the nested page-table mapping follows the BAR chosen by the guest. An invalid destination leaves it unmapped. Moving a BAR while capturing a dirty memory image has rules of its own, and a capture during a BAR sizing probe is refused rather than allowed to save a fiction.

Real firmware also made much larger disk requests than our Linux guests had. EDK2 issued reads of 896 KiB in a single virtio descriptor. The block backend works in 256 KiB pieces, so hv now serves an oversized request in parts and writes one final status when the whole request is complete. A failure in a later part still has to complete the guest's request exactly once. Those cases are now fuzzed alongside the successful split.

The result was not yet Windows. It was something more useful for debugging it: a DEBUG firmware that reached `Shell>` and executed commands on real Intel VT-x, and a RELEASE firmware that loaded Ubuntu 24.04's own shim and GRUB all the way to an EFI-aware Linux login.

## W1: Windows expects Hyper-V services

Booting Windows required more than UEFI and virtio drivers. Windows expects timer and interrupt services that our minimal firmware machine did not otherwise provide. In particular, without Hyper-V synthetic timers it expects HPET or periodic RTC interrupts. Rather than adding another collection of legacy devices, we implemented the documented Hyper-V interface Windows already knows how to use.

Selecting `hyperv` for a firmware guest gives it the `Hv#1` CPUID interface, the guest OS and hypercall MSRs, a read-only hypercall page, per-vCPU indices, a reference TSC page and a reference time counter. The reference page uses the documented sequence protocol so a guest never reads a half-updated clock.

Each vCPU also has SynIC state and four synthetic timers. A timer can deliver a direct interrupt vector or a SynIC message, run once or periodically, and wake a vCPU even when that vCPU is halted. These are not free-running host timers with a guest interrupt attached at the end: their deadlines are part of the vCPU's interrupt state and participate in save and restore.

The hypercall page is deliberately small in scope. An unknown or malformed call gets a defined Hyper-V status, not a blanket pass-through to the host. Crash MSRs P0–P4 and CrashNotify are recorded too, so a Windows bugcheck leaves something more useful than a stopped console.

### Three boot failures, three different machines

The last stretch to SAC produced a useful reminder that "Windows does not boot" is not a diagnosis.

One VT-x failure came from the interaction between the emulated APIC's TPR threshold and the processor's interrupt-window control. The VM entry was refused before Windows could make progress. We kept the diagnostics after fixing it: a failed entry now preserves the relevant VMCS controls, invalid bits, MSR-load and interrupt state in the stopped vCPU's dump. The next failed entry will not have to be debugged from a single error number.

Another failure was an ACPI mismatch. The firmware machine initially carried a legacy FADT, while CloudHv and Windows use the hardware-reduced ACPI path. The firmware FADT now describes real sleep-control and sleep-status registers and omits the legacy PM1 blocks. The DSDT also describes the processors, COM1 and the resources Windows actually sees.

A third boot stalled in an RTC polling loop. The RTC's update-in-progress bit was hard-wired clear. That sounds like a way to make the clock always safe to read, but a driver waiting for an update cycle can reasonably wait forever when no cycle ever occurs. The RTC now behaves like a clock rather than a very polite constant.

There was also a problem outside the hypervisor. The unattended image recipe used DISM's offline-only `/Add-Driver` with `/Online`, so the prepared image lacked a working viostor service. The recipe now installs virtio-win with `pnputil /add-driver ... /install`. We first verified that image with its system disk on virtio under QEMU, separating image-preparation mistakes from hv bugs before trying it under nos.

Once those pieces were in place, the serial console reached SAC. The CMD channel accepted the Administrator login, viostor was running, NetKVM obtained `10.0.100.4/24`, and ping reached `10.0.100.1`. The first Windows VM used about 1.65 seconds to reach SAC on the EX44.

### A shutdown is a device-model test too

A graceful Windows shutdown exposed one of our favourite small details in this project. Windows eventually reaches UEFI `ResetSystem(EfiResetShutdown)`. CloudHv's runtime implements that by writing byte `0x34` to its fixed sleep-control port at `0x600`.

Legacy Q35 also has a register at `0x600`, but there it is PM1 status, not a hardware-reduced sleep-control register. The same-looking write must therefore power off one machine and not the other. The PM fuzz target now checks both interpretations. A working `shutdown /s /t 0` is not just a pleasant command-line experience; it shows that the firmware, ACPI description, device model and guest agree about the machine.

At this stage we also saved a running Windows VM — 679 MiB written with a 5.9-second pause in that run — restored it, used CMD and ping again, and then let Windows power itself off. The UEFI save/restore gate passed under real AMD-V and Intel VT-x as well as TCG.

## W2: fewer exits, not more cleverness

Windows booted, but an idle VM was busy for the wrong reasons. A VM exit is a transition from the guest back to the hypervisor; handling an emulated register or interrupt this way costs far more than letting the vCPU continue.

We measured a roughly 2.666-second idle window before optimizing it. It contained 1,657 exits. Of those, 1,163 were nested-page faults on the xAPIC page, and the host saw 479 EOI writes and 665 ICR accesses. That gave us three concrete places to work: end-of-interrupt handling, inter-processor interrupts and TLB flushes.

The comparable optimized window lasted roughly the same time:

| Idle Windows measurement | Before | After |
|---|---:|---:|
| Total VM exits | 1,657 | 647 |
| xAPIC nested-page faults | 1,163 | 0 |
| EOI writes seen by the host | 479 | 1 |

```chart
type: before-after
unit: "exits"
max: 1800
step: 300
series: { before: "Before W2", after: "With W2" }
caption: "The same idle Windows workload in comparable 2.666-second windows. Total exits fell by 61%; the xAPIC nested-page faults disappeared."
groups:
  - rows:
      - { label: "all VM exits", before: 1657, after: 647 }
      - { label: "xAPIC nested-page faults", before: 1163, after: 0 }
      - { label: "EOI writes", before: 479, after: 1 }
```

The first mechanism is Hyper-V's virtual APIC interface: its EOI, ICR and TPR MSRs, plus a VP-assist page for each vCPU. VP-assist can mark an interrupt as needing no EOI. If the guest clears that state in a case where an EOI is still owed, hv completes it on the next exit. SynIC AutoEOI is handled at injection as well. When AutoEOI is configured, hv disables APIC virtualization for that arrangement rather than assuming that two interrupt optimizations will compose safely.

In an isolated measurement, VP-assist reduced EOI exits from 497 to 8.

The second mechanism is the synthetic ICR. A remote fixed IPI now joins the existing world-switch fast path instead of taking the longer generic exit route. The synthetic cluster-IPI hypercall supports simple 64-bit VP masks in its memory and fast forms. Median host handling for the synthetic ICR path fell from 2.2 microseconds to 715 nanoseconds.

The third piece is TLB invalidation. `HvFlushVirtualAddressSpace` and `HvFlushVirtualAddressList` flush the calling vCPU immediately and send other vCPUs an atomic request plus a doorbell. A remote vCPU handles the request before its next VM entry. VMX uses VPID invalidation and SVM forgets the affected ASID. A list flush conservatively flushes the whole TLB; sparse Ex and XMM forms are not advertised.

One wrong theory during this work was that Windows needed the sender to wait for every remote flush acknowledgement. Adding that wait did not fix the failure. The actual problem was in the hypercall control flags. We fixed their validation and removed the extra serialization instead of keeping a slowdown that had not solved anything.

A final Windows run reported 3,196 hypercalls — 3,195 TLB flushes and one synthetic IPI — with zero errors and zero flush timeouts. The restored VM answered through CMD and ping again, then shut down normally.

## The tests that stay behind

A successful boot is a poor regression test if it only exists in a terminal scrollback. The firmware phase now runs in every x86-64 `hv-test.py`: CloudHv reaches Boot Manager and the UEFI shell, executes a typed command, saves and restores a running firmware VM, and executes another command after restoration.

The Hyper-V CPUID and MSR policy, reference clock, SynIC, timers, VP-assist, APIC interface and vmstate formats have fuzz coverage. The whole-machine target drives valid flush and IPI hypercalls through the real run loop. Mutation checks deliberately remove individual checks and require a target to notice. The final W2 fuzz run covered 2,250,000 inputs without a finding.

The usual `make check`, x86-64 and arm64 builds and smoke tests stayed green. So did the x86 hypervisor test under TCG and on real AMD-V. Windows itself remains a manual gate on the EX44 because its image cannot be redistributed with the repository.

The full [Windows plan](https://github.com/irqlevel/nos/blob/master/plans/windows.md) records the measurements and the individual W0–W2 steps. The main implementation landed in [8b3e536a](https://github.com/irqlevel/nos/commit/8b3e536a2dae2231aae94dfac77285ab9f84c36c), followed by the [W2 performance work](https://github.com/irqlevel/nos/commit/75d8809b90f924899eced4c91d1c37fa6cecc706).

## What works, and what does not

It is worth keeping the current boundary explicit.

**Working now:**

- Windows Server 2022 Standard Core from a prepared image with virtio-win installed.
- UEFI boot through the checked-in CloudHv firmware, with DEBUG and RELEASE builds.
- Virtio storage and networking, DHCP and NAT, SAC/EMS and a CMD channel on COM1.
- RDP negotiation through a forwarded TCP port after the guest firewall permits it.
- Hyper-V reference time, synthetic timers, SynIC, APIC MSRs, VP-assist, synthetic IPIs and the advertised TLB-flush calls.
- Graceful shutdown and running-VM save/restore. The state format carries the firmware and Hyper-V state needed by the migration machinery as well.

**Not yet:**

- Installing Windows from its ISO directly on nos. W3 needs an emulated NVMe disk, an AHCI/ATAPI installation CD, an e1000e NIC and VHD/VHDX image readers. Until then, image preparation uses QEMU.
- A graphical Windows console. W4 needs GOP, a guest framebuffer, VNC/noVNC and suitable keyboard and pointer devices. SAC is the serial management path; RDP is forwarded rather than displayed by nos itself.
- Full Windows 11 platform compliance. W5 needs TPM 2.0, Secure Boot and persistent UEFI variables. Variables currently live in RAM and do not survive a restart. LabConfig can bypass installation checks, but that is not the same thing as supporting the platform requirements.
- A same-image Windows performance comparison against KVM. We have measured the W2 improvement against our own immediately preceding implementation, not against KVM running this image.
- Every possible Hyper-V call. Sparse Ex/XMM flush forms are intentionally not advertised, and a list flush currently invalidates the whole TLB.

There is performance work left too. The current Server 2022 image still runs in xAPIC mode. Hardware virtualization of the xAPIC page and AMD's AVIC may help, but they are next measurements, not automatic next features. Our earlier APIC experiments have already shown that fewer exits do not always mean a faster workload.

W0 gave us firmware we could trust. W1 made Windows agree to boot and shut down. W2 removed 61% of the exits from an idle Windows VM. The next milestones are less about persuading Windows to tolerate our machine and more about making the installation, console and platform complete enough that no preparation ritual is needed.
