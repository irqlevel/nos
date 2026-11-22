# Windows guests

Windows 10/11 and Windows Server 2022/2025 under `hv`, and the UEFI boot
path that gets there. The route is the one Cloud Hypervisor uses: EDK2's
OVMF `CloudHv` entered by the existing PVH loader, a disk image prepared
with virtio-win, and SAC/EMS on COM1. A Linux guest gets the same firmware
path for what hvboot cannot read: an EFI boot entry, a RHEL-family /boot,
BLS, LVM or LUKS.

The gap analysis and the W0 breakdown were made in the Claude Code session
of 2026-11-21. That session stopped during W0.1 before changing the source;
its downloaded EDK2 configuration and license research were carried over.
This branch starts from current `master`, not from the old remote branch
whose history predates the repository's history rewrite.

**Where it stands (2026-11-22):** W0.1 is implemented and verified on
`kimi-windows`: the pinned RELEASE and DEBUG firmware builds, the checked-in
PVH ELF and its notices, system-image packaging and rollback. `make check`,
`scripts/build-cloudhv.sh --check` and `tests/image-test.py` pass; the second
firmware build is byte-identical.

W0.2 is implemented too. Ordinary guests now use Q35 (`8086:29C0`) with
ECAM for 256 buses at `0xE0000000`, and `new_firmware` selects CloudHv's
`0x0D57`, no ECAM, xAPIC and IO-APIC even with one CPU. W0.3 adds per-vCPU
fixed and variable MTRRs and DEF_TYPE, checked against the CPU's rules but
not applied to NPT/EPT; vmstate version 3 carries them and reads versions 1
and 2 with reset MTRRs.

W0.4 makes virtio's MSI-X memory BAR movable through both CF8/CFC and ECAM.
The frame keeps a stable backing region for the host and vmstate while its
NPT/EPT mapping follows the BAR. Invalid destinations leave it unmapped until
another write. Dirty logging follows moves, restore recreates them, and a
capture during a BAR sizing probe is explicitly refused.

W0.5 adds `firmware` to `hv boot`/`hv start`/`hv create` and `firmware = true`
to vm.toml. With no explicit image it selects `/lib/CLOUDHV.fd`; an explicit
PVH path allows the DEBUG build. The firmware machine is selected by the same
checked `Spec` and receives its image through the existing PVH loader.
Firmware capture, restore and migration are refused until the host bridge is
part of the versioned state.

A real TCG run reaches OVMF's `BdsDxe: No bootable option or device was found`
with no MSR #GP. `hv create fw firmware` and `hv start fw` work through the
written vm.toml, and `hv save` refuses it and leaves it running. All 2.2
million inputs of `hv-fuzz.py`, `make check`, both architecture builds and
smoke tests, and `hv-test.py` on x86-64 and arm64 pass on the final W0.5 tree.
W0.6 is verified on the EX44's VT-x. The DEBUG firmware reaches `Shell>` and
runs commands there. The RELEASE firmware automatically loads Ubuntu
24.04.5's own shim/GRUB and reaches login; logging in as `ubuntu` confirmed
kernel 6.8.0-147, ext4 on `/dev/vda1`, and `/sys/firmware/efi`. This exposed
EDK2's 896 KiB single-descriptor reads: hv now serves oversized virtio-blk
requests in 256 KiB backend parts with one final status. The final boot had
no vda errors.

W0.7 is implemented: `hv-test.py` now boots CloudHv through Boot Manager and
executes a Shell command through `hv exec` on every x86-64 run. The final tree
passes all 2.2 million hv-fuzz inputs, the three new blk mutants, `make check`,
both architecture builds and smoke tests, both `hv-test.py` architectures and
`scripts/build-cloudhv.sh --check`.

W1.1 and the first Windows boot are implemented and verified on the EX44:
`hyperv` / `hyperv = true` selects Hv#1 only for a firmware VM. Besides the
basic CPUID/MSRs and hypercall overlay, hv now provides TIME_REF_COUNT, a
read-only reference TSC page with the documented sequence protocol, per-vCPU
SynIC and four synthetic timers in direct-vector and SynIC-message modes.
Their deadlines use the existing host-timer kick, including a halted vCPU.
Crash MSRs P0-P4 and CrashNotify report a Windows bugcheck in the VM report.

Windows Server 2022 Standard Core boots to `SAC>` in about 1.65 s. The CMD
channel accepts Administrator's login; `viostor` is RUNNING, NetKVM obtains
10.0.100.4/24 by DHCP and ping reaches 10.0.100.1. The last bring-up bugs
found and fixed were a VT-x TPR-threshold/IRQ-window conflict, a legacy
rather than hardware-reduced firmware FADT (`0xA5`), and an RTC UIP bit held
always clear (`0x7B` and an RTC polling loop). Failed VM entries now retain
their VMCS controls, invalid control bits, MSR-load and interrupt state in
the stopped vCPU's dump, checked by the existing `refused` guest.

The installation recipe is fixed too: `tests/windows/autounattend.xml` uses
`pnputil /add-driver ... /install`; DISM's offline-only `/Add-Driver` had been
mistakenly used with `/Online`, leaving no viostor service in the image. The
prepared image was verified first under QEMU with its system disk on virtio,
then under hv. RDP's Hybrid/NLA negotiation also succeeds through `hv forward`
after allowing TCP 3389 in the VM's firewall. A graceful `shutdown /s /t 0`
now powers the VM off without `hv stop`: Windows reaches UEFI
`ResetSystem(EfiResetShutdown)`, whose CloudHv runtime writes `0x34` to its
fixed sleep-control port `0x600`. The PM fuzz target holds both that path and
legacy Q35's different meaning of the same port.

## Firmware facts that drive the design

- `CLOUDHV.fd` is a 4 MiB PVH ELF with one `PT_LOAD` at 1 MiB and its entry
  at `0x4FFFD0`. It is ordinary guest RAM, like hvboot. MEMFD at
  `0x800000-0x14FFFFF` must also be RAM.
- PCI 00:00.0 must have device ID `0x0D57`: that is how CloudHv selects its
  platform. Low RAM must end at or below `0xC0000000`; its 32-bit PCI
  aperture is `[0xC0000000, 0xFC000000)`, its I/O aperture `0xC000-0xFFFF`.
  PciBusDxe enumerates and assigns the BARs itself.
- SEC writes `MTRR_DEF_TYPE=WB|E`; a #GP there stops the firmware before it
  reaches its console. The local APIC timer runs at 1 GHz and the PM timer
  is at port `0x608`, both already hv's contract.
- ACPI comes from PVH `start_info.rsdp`: every XSDT entry and the DSDT named
  by FADT.X_DSDT. CloudHv's hardware-reduced shutdown writes byte `0x34` to
  sleep control at its fixed `0x600`, with status at `0x601`; the FADT names
  the same registers. Legacy Q35 keeps PM1 status at `0x600` instead, so its
  identical-looking status write must not power it off.
- UEFI variables live in RAM and do not survive a restart. There is no GOP
  driver; the serial console is COM1 at 115200 baud. An SMM build requires
  TSEG, which this machine does not have.
- Without Hyper-V stimer, Windows expects HPET or RTC periodic interrupts.
  Cloud Hypervisor makes its Hyper-V support mandatory for Windows for the
  same reason; hv now supplies the timers and reference time instead.

## W0 -- UEFI in a guest

The finish line is OVMF's `Shell>` through `hv attach`, on AMD-V and VT-x,
and then a Linux cloud image booted by its own GRUB EFI. The existing Linux
boot paths must remain unchanged.

1. **W0.1, reproducible firmware.** `scripts/build-cloudhv.sh` builds the
   pinned `edk2-stable202608` source (commit
   `2970e5699ba6267f3384ffab20f96647578aebc8`) in Docker with
   `NETWORK_ISCSI_ENABLE=FALSE`, no SMM, TPM or Secure Boot, and the shell
   included. It produces a RELEASE `build/image/CLOUDHV.fd` and a DEBUG
   `out/cloudhv/CLOUDHV-debug.fd`; `--check` rebuilds and compares. The
   ordinary nos build stays offline. The system image carries the firmware
   as `/lib/CLOUDHV.fd` and its BSD-2-Clause-Patent, MIT/Xen and LZMA notices.
2. **W0.2, Q35 and a firmware machine.** Move ordinary guests from i440FX
   to Q35: host bridge `8086:29C0`, ECAM for 256 buses at `0xE0000000` and
   MCFG, with the PCI hole on every Q35 guest so ECAM never overlaps RAM.
   The firmware variant keeps CloudHv's required host bridge DID `0x0D57`
   and CF8/CFC-only access, without ECAM. Both use low RAM ending at
   `0xC0000000`. CloudHv gets a local APIC and IO-APIC also for one vCPU,
   xAPIC at reset, and ACPI `_CRS` apertures of `[0xC0000000, 0xFC000000)`
   and I/O `0xC000-0xFFFF`.
3. **W0.3, per-vCPU MTRRs.** Fixed and variable ranges and DEF_TYPE with the
   SDM's reset and #GP rules, the same initial state on every vCPU, and no
   effect on NPT/EPT memory types (as KVM). Include migration state, the
   policy fuzzer, mutants and an `msr` built-in guest.
4. **W0.4, movable MSI-X memory BARs — implemented.** The existing
   passthrough `move_bar` approach now covers virtio's MSI-X table page:
   nested mappings and the guest's view move together through either
   configuration interface, with stable backing for the host and migration.
   The PCI fuzz target covers free, occupied and invalid destinations,
   sizing, moving back and moves under the dirty log; three mutants are
   caught.
5. **W0.5, start a firmware guest — implemented.** `firmware` on
   `hv boot`/`hv start`/`hv create`, or `firmware = true` in vm.toml, selects
   `/lib/CLOUDHV.fd` by default and the firmware machine. Q35 remains the
   default for Linux, and an explicit PVH path can select the DEBUG firmware.
   Firmware save, restore and migration are refused until the host bridge is
   versioned into that state. The interface, a real OVMF boot and the refusal
   are documented in `docs/hypervisor.md`.
6. **W0.6, bring-up on the EX44 — implemented.** OVMF reaches `Shell>` and
   runs commands under VT-x. An Ubuntu cloud image boots automatically through
   its own shim/GRUB EFI to a working login and EFI runtime. Virtio-blk serves
   EDK2's oversized single-descriptor requests in backend-buffer-sized parts;
   the fuzz target covers 896 KiB reads and writes, split chains, 512e, a
   failure in a later part, and the three corresponding mutants. The old panel,
   web and cluster stand was dismantled with the user's permission and is not
   restored; only the test VM is stopped after the check.
7. **W0.7, gates and documentation — implemented.** `hv-fuzz.py` covers policy,
   BAR moves, ACPI and oversized virtio-blk requests, with mutants for the
   latter. `hv-test.py` has a UEFI phase: CloudHv through Boot Manager to Shell,
   then a command through `hv exec`. `docs/testing.md` and the gate table in
   `CLAUDE.md` describe it; both architectures build and pass their smoke and
   hv tests. A manual Windows gate stays out of CI because the Windows image
   cannot be redistributed.

## W1 -- Windows Server from a prepared image

Windows Server 2022/2025 with virtio-win already installed, SAC in the
serial console and RDP through `hv forward`.

- **W1.1 — implemented.** Hyper-V CPUID leaves `0x40000000-05` with `Hv#1`,
  GUEST_OS_ID, HYPERCALL and VP_INDEX, the read-only hypercall overlay,
  TIME_REF_COUNT and the reference TSC page. `hyperv` / `hyperv = true`
  selects them for a firmware VM; Linux keeps KVM by default. Per-vCPU SynIC
  and four stimers cover direct and message delivery, one-shot and periodic
  operation. Crash MSRs report a Windows bugcheck and `/logs/hv.log` records
  it. Port `0x402` carries OVMF DEBUG output during bring-up.
- **Firmware ACPI — implemented for this boot.** The firmware FADT is
  hardware-reduced with no FACS or legacy PM1 blocks, with real sleep
  control/status registers, and the DSDT has processor objects. COM1 as
  PNP0501 and SPCR remain for a fuller firmware description. Legacy ports
  stay out of the PCI0 `_CRS` I/O window.
- Match Cloud Hypervisor's 8 MiB alignment for 64-bit BARs and retain the
  IO-APIC version-write behaviour Windows expects.
- Add newly discovered MSR reads and writes deliberately, not as a blanket
  pass-through. Save all new state in vmstate for migration.

## W2 -- Windows at a useful speed

Hyper-V vapic and a path to x2APIC, hv-time, and hypercalls for TLB flushes
and IPIs. Measure against KVM on the EX44 with the same VM and benchmark
kit, and measure each change. AVIC and virtualizing the xAPIC page come
after the measurements.

## W3 -- Installation and built-in devices

An emulated NVMe disk for stornvme, an AHCI/ATAPI CD for the installation
ISO, and an e1000e NIC, each with its hv-fuzz target and boot gate. VHD and
VHDX readers belong in `common/storage/diskimage`. Only then is installation
from an ISO on nos itself, without a QEMU preparation step, supported.

## W4 -- Windows 10/11 consoles

A firmware build with a GOP driver, a guest framebuffer using the existing
dirty log, a VNC server as a module and noVNC in the panel. Input needs an
i8042 and an absolute pointer: USB tablet over emulated xHCI, or virtio-input.
SAC is a Feature on Demand on current desktop Windows; RDP is the other path.

## W5 -- Windows 11's platform requirements

A TPM 2.0 and Secure Boot with persistent UEFI variables. A TPM register
interface is not a TPM: a C reference implementation cannot be added to a
Rust-only module, and there is no suitable Rust TPM today. Secure Boot also
needs a firmware design without this platform's SMM/TSEG hang. Until those
are solved, Windows 10/11 installation can use LabConfig, and Server remains
the supported target; neither is presented as full Windows 11 compliance.
