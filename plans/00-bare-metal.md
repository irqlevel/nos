# Stage 0 — Bare-metal server hardware

**Goal:** `nos` boots on a real Intel x86-64 server (Hetzner-class) and stays
remotely manageable, without depending on QEMU, kvmclock, or a serial console
being present.

**Why first:** Everything else assumes `nos` owns real hardware. This stage also
retires the biggest unknowns (real ACPI tables, large/holey memory maps, real
NICs, PCIe bridges, no console) before any hypervisor work.

**Demo:** Power on a rented bare-metal box; it DHCPs, and `python3
scripts/udpsh.py <ip>` gives an interactive shell over UDP. `dmesg` streams over
the network.

---

## What already works in our favor

- **Timekeeping** does not need a hypervisor: `tsc.cpp` calibrates via HPET or
  PIT channel 2. kvmclock is used only if present.
- **UEFI won't crash the kernel:** `vga.cpp` already checks the Multiboot2
  framebuffer type and avoids touching text buffer `0xB8000` when GRUB handed us
  a pixel (GOP) framebuffer — the screen is just blank.
- **NVMe (Rust)** and **r8168 (Rust)** cover common Hetzner storage/NIC.
- **UDP remote shell** already exists — this is the real console on a headless
  server.
- **SMP/ACPI/LAPIC/IOAPIC/MSI-X** bringup is already implemented.

## The real gap is operational, not architectural

The hard part is not new features; it is the **debug loop on a headless remote
box**. Hetzner's standard lines have no permanent IPMI/serial — a KVM console is
provisioned on request for a few hours. Consequences drive the task list below.

## Work items

### 0.1 Network-first observability (do this earliest)
- Bring DHCP + UDP shell up as early in boot as possible, before most subsystem
  init, so a machine that half-boots is still reachable.
- Stream `dmesg` over the network (UDP broadcast or a pull endpoint). This is the
  primary console on hardware with no serial port.
- Consider a tiny pre-init "network heartbeat" so a hang is at least visible.

### 0.2 Framebuffer console (~500 lines)
- Add a pixel framebuffer text console with an embedded bitmap font, driven by
  the Multiboot2 GOP framebuffer info already parsed in `grub.cpp`.
- Needed so that *something* is visible on the provisioned KVM console on a UEFI
  machine (where `0xB8000` text mode is unavailable).
- Keyboard input via the KVM console is USB HID; `nos` has no USB stack. Under
  legacy BIOS boot the firmware usually emulates PS/2 (the existing 8042 driver
  works); under UEFI it may not. **Do not rely on local keyboard — the network
  shell is the console.**

### 0.3 UEFI boot + disk image
- `build-disk.sh` currently produces an MBR image for legacy boot. Add a variant
  with an EFI System Partition and `grub-efi` (Multiboot2 works under both BIOS
  and UEFI). ~1 day.
- Keep legacy-boot as a fallback where the machine allows enabling it.

### 0.4 Real-hardware assumptions to validate
These only surface on real silicon:
- **Memory map:** allocator was exercised on small VMs. Validate on 64+ GB with
  reserved regions and holes. Feed the full E820/EFI map into
  `PageAllocatorImpl::Setup()` and exclude reserved ranges. (This same
  reserved-ranges capability is later reused by Stage 4 live update.)
- **PCIe bridges:** QEMU topology is flat; real chipsets have bridges. Verify
  `pci.cpp` enumeration recurses through bridges. Consider ECAM/MMCONFIG access
  in addition to legacy CF8/CFC.
- **r8168 on real silicon:** QEMU does not emulate this chip, so the driver has
  likely never touched real hardware. Expect surprises on first boot.
- **SMP scale:** 12–24 threads; verify AP bringup and per-CPU state at scale.
- **Real ACPI quirks:** vendor tables are messier than QEMU's.

### 0.5 If the NIC is Intel, not Realtek
- Intel-branded lines (e.g. some EX servers) often ship Intel I219/I226
  (e1000e/igc). That is a new driver, ~1–2k lines.
- **Mitigation:** before renting, check the exact model's `lspci` via the rescue
  system, and prefer a machine with NVMe + RTL8168 so both drivers already exist.

## Recommended path

1. **Buy local hardware first, not Hetzner.** A €50 used NUC / thin client / old
   desktop (ideally with a Realtek NIC) turns the debug loop from "request a KVM
   console + rescue reboot" into minutes. This kills ~80% of hardware bugs before
   renting anything.
2. Framebuffer console + EFI disk image + PCI-bridge enumeration.
3. Network dmesg + UDP shell as the primary console; harden r8168 on real silicon.
4. Deploy to Hetzner: boot rescue system, `dd` the raw image (adapting
   `build-disk.sh` output), power-cycle via the Robot API. Then harden on large
   RAM and high core counts.

## Effort

**1–2 months of hobby time**, dominated by debugging on real hardware rather than
by new code. Buying a local box up front is the single biggest schedule lever.

## Exit criteria

- Boots unattended on the target machine (UEFI and/or legacy).
- Full E820/EFI memory map honored; stable on 64+ GB.
- DHCP + UDP shell reachable within the first second of boot.
- `dmesg` observable over the network.
- Storage (NVMe) and NIC (r8168 or the machine's actual NIC) both functional.
- TCO watchdog usable (needed as the safety net in Stage 4).
