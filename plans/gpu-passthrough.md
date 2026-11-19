# A host's PCI device in a guest: VT-d, and an NVIDIA GPU in a Linux VM

A whole PCI function of the host -- first an NVIDIA GPU -- handed to one
Linux guest under hv, the guest's own driver driving it: what Linux's VFIO
does for QEMU, here with hv in the kernel and no user space between. The
three things VFIO gives a VMM become three types of `kcore` with a `Drop`:
a claimed PCI function, an IOMMU domain, and the function's interrupts
remapped. Nothing of the device is emulated but what must not reach it.

Out of scope for now: AMD-Vi (the AX41s have no GPU), arm64's SMMUv3, a
display or Windows in the guest (both need guest firmware -- OVMF -- to run
the card's GOP ROM, which hv has not), vGPU (NVIDIA's host driver), a GPU
shared by VMs, P2P between GPUs, live migration or `hv save` of a VM that
holds a host device (its DMA never reaches the dirty log, its state cannot
be taken: refused, not broken).

## Why an IOMMU, and why interrupt remapping

Without DMA remapping the guest programs the GPU with guest-physical
addresses, which are not where its memory is -- and with any address it
likes, which is every byte of the host. So the device's DMA goes through a
domain that maps the guest's RAM and nothing else.

Without interrupt remapping the guest has the device write to 0xFEExxxxx --
a DMA write there is an interrupt -- and sends the host any vector it
wants on any CPU: the TLB shootdown's, the scheduler's, an NMI. VFIO refuses
a platform without IR unless told `allow_unsafe_interrupts`; nos refuses it
too, unless the VM's own config says `unsafe_interrupts` -- the word stages
4 and 5 are run with before IR, stage 3, is done, and which every function
given says in the log. On VT-d IR is a unit's, not a device's: with it on and
compatibility-format interrupts blocked (CFI=0), every interrupt source the
unit covers -- the IO-APIC, the HPET, every MSI and MSI-X of the host's own
devices -- has to be remappable. So IR changes the host's interrupt
programming (`arch/x86_64/ioapic.cpp`, `drivers/msix.cpp`, `drivers/msi.cpp`)
for everyone, not only for a VM.

## The test machine: Hertz

`ssh root@139.100.197.201`: Ubuntu 24.04 (Linux 7.0), an Intel Core i3-14100
(Raptor Lake, 4 cores, 8 threads, 39 physical address bits), 31 GiB, an MSI
PRO B760M-P (no BMC: the network is the only way in), a static address
(139.100.197.201/24, gateway .1), an RTL8168, two 1 TB NVMe -- nvme1 Ubuntu's,
nvme0 empty -- and:

- **the GPU**: an NVIDIA RTX A2000 (GA106, Ampere), 10de:2531 at 01:00.0
  with its HD Audio function 10de:228e at 01:00.1, behind the CPU's x16 root
  port 00:01.0, whose ACS is on (SrcValid, ReqRedir, CmpltRedir, UpstreamFwd).
  The two are an IOMMU group of their own. BAR0 16 MiB 32-bit
  non-prefetchable, BAR1 8 GiB 64-bit prefetchable (a Resizable BAR: 64 MiB to
  8 GiB, at 8 GiB), BAR3 32 MiB 64-bit prefetchable, BAR5 128 I/O ports, a
  512 KiB ROM. **MSI only** (one vector, not maskable, 64-bit) -- no MSI-X;
  the PCIe capability at 0x78; FLR on 01:00.0, none on 01:00.1; no ATS, no
  SR-IOV.
- **DMAR** (136 bytes; HAW 39; flags INTR_REMAP and DMA_CTRL_PLATFORM_OPT_IN):
  - `dmar0` at 0xFED90000, VT-d 4.0, the graphics unit: its scope 00:02.0,
    the UHD 730, alone; an RMRR 0x7C000000-0x807FFFFF for it.
  - `dmar1` at 0xFED91000, VT-d 5.0, INCLUDE_PCI_ALL: everything else -- the
    GPU, the NVMe, the NIC -- and the IO-APIC (id 2, source-id 00:1e.7) and
    the HPET (0, 00:1e.6). CAP 0xd2008c40660462: 256 domains, 4-level tables
    (48-bit AGAW), MGAW 39, 2 MiB and 1 GiB leaves, one fault recording
    register at 0x400, page-selective invalidation, no caching mode. ECAP
    0xf050da: **not coherent** (C=0: every entry written is flushed with
    `clflush` before the unit is told), queued invalidation, IR with x2APIC
    (EIM), pass-through, snoop control, the IOTLB registers at 0x500; no
    device TLB, no PASID, no scalable mode -- the legacy root and context
    tables.
- **MCFG**: ECAM at 0xC0000000 for buses 0-255.

The DMAR and MCFG bytes are the C++ fuzzer's seeds (`tests/fuzz/cpp`).

## The host's half, in C++

The IOMMU is C++, beside the interrupt core and the MMU it extends: IR is
called from the C++ that programs the IO-APIC and the MSIs, a unit has one
owner (GCMD is written whole, there is one invalidation queue), and the C++
reaches no Rust. hv reaches it by the usual three steps (`rust_ffi.cpp`,
`ffi::iommu`, `kcore::iommu`).

- **ACPI** keeps the DMAR (its units and their device scopes, its RMRRs) and
  the MCFG, bounded, copied out as every table's fields are; a table that does
  not fit what is kept is said, and the IOMMU is then not turned on.
- **ECAM on x86** from the MCFG, the CAM kept for what is below 256 bytes
  where there is no MCFG: the extended space has ACS, AER and the Resizable
  BAR, which a reset loses. **Done** (`arch/x86_64/pci_x86.cpp`): the CAM
  for the first 256 bytes, ECAM past them, a bus mapped as the scan finds a
  function there and held to the CAM's reading of it, the MCFG's window
  refused over RAM; an access past 256 bytes with no ECAM reads all ones --
  it read the header again, the CAM's offset masked to 8 bits.
- **VT-d** (`arch/x86_64/vtd.{h,cpp}`): each unit's registers mapped; the
  root table and a context table a bus, every host device's entry
  pass-through (TT=10b), so that its driver changes in nothing; queued
  invalidation, every wait bounded and a unit that does not answer reported
  (not waited on forever); the fault events an interrupt whose handler says
  each fault -- source, address, reason -- at a bounded rate; `iommu=off` the
  default until a machine has run it. The graphics unit is left as the
  firmware left it: nothing of nos's is behind it.
- **Interrupt remapping**: a table a unit, an entry an interrupt source --
  the IO-APIC's pins, each MSI and MSI-X vector -- each validating its
  source-id (SVT=01, the requester's BDF; the IO-APIC's and the HPET's from
  their DMAR scopes), CFI=0, the interrupt entry cache invalidated after a
  change. Turned on before the first interrupt is programmed, so no source
  is ever left in the compatibility format.
- **Domains**: a second-level table (2 MiB leaves where the guest's RAM is
  the huge pool's), a domain id (0 reserved -- caching mode uses it -- 1 the
  pass-through entries'), attach (TT=00: untranslated requests only, so ATS's
  translated ones are blocked whatever the guest does to the device),
  detach to *blocked* (P=0), never back to pass-through, until the function
  is reset. A domain holds every page it maps until the device is blocked
  and the IOTLB flushed: no bug in hv lets the device write a freed page.
- **The claim**: a function given to a VM is the VM's -- no host driver and
  no second VM takes it (**done**: a function a driver of the host's has
  taken is refused -- `Pci::take`, said by every driver's bus mastering or
  by itself -- and `guestpci=` keeps the drivers off one at boot); an IOMMU
  group (a function and whatever the path above it does not isolate from
  it -- ACS, a multi-function device) is claimed whole; a function a DMAR
  RMRR names is refused.
- **Reset**: FLR where the function has it, else a secondary bus reset of the
  bridge above (all of its functions then the VM's); the config space saved
  before -- BARs, command, the MSI capability, the PCIe control registers,
  the Resizable BAR's sizes -- and written back after, the function asked
  for its vendor id until it answers (bounded). **Done** (`ResetPlan`): the
  bus reset made once a device, by the first of its functions the guest
  names and before any is set up, where the bus is the device's alone; ACS's
  controls saved too. `hv-passthrough-test.py`'s virtio disk has no FLR.
- **MSI** comes from branch `testsrv-boot` (`MsixTable::setup_msi`): the GPU
  has no MSI-X. **MSI-X**, for everything else (an NVMe, a NIC): the real
  table the host's, a vector of the host's an entry given (16 at most), each
  a Rust MSI-X slot -- 64 of them now, their entry points generated by
  `asm.asm`'s `%rep` -- and an entry of the unit's table.

## The guest's half, in hv

- **A function of the host's on the guest's bus**: its config space read
  through to the real one, with what is virtual laid over it -- the BARs (the
  guest's addresses; the real registers never change), the command register
  (bus mastering the guest's; memory decoding on for as long as the guest
  holds the device, the guest's memory and I/O bits its own view, since a
  deaf device's BAR is an Unsupported Request -- VFIO's CVE-2020-12888; INTx
  disable kept), the MSI capability emulated, and the PCIe controls that
  would reach past the device -- phantom functions, no snoop, FLR, the
  payload sizes, error reporting, the link -- the host's. What is built is
  `hypervisor/hv/src/devices/host.rs`, and docs/hypervisor.md, "A host's PCI
  device", says it register by register.
- **A root port above each device** (**done**: `devices/port.rs`): the
  device's functions behind it at their own numbers, as on the machine -- a
  GPU's audio function at 00.1, where `hda_intel` looks for its GPU, a NIC's
  ports in one slot -- its link the device's, read live, with no ASPM, slot
  or interrupt of its own; registers alone, nothing reaching the device, the
  bus below it routed by the number the guest gives it. Its windows hold the
  device's BARs, which are placed as firmware places them behind a bridge:
  64-bit prefetchable above the RAM, the rest below 4 GiB.
- **BARs**: mapped straight into the nested table -- a new kind of leaf, UC
  or WC rather than WB, onto a physical address that is not a frame of the
  guest's; `GuestMemory`'s rule "the table maps only what this value owns"
  becomes "its frames and the BARs of the functions it holds the claim to",
  the claim held inside it. Pages that must not be the device's are trapped
  and decoded (`mmio.rs`): the MSI-X table and PBA (a vector's mask has to
  reach the hardware), and NVIDIA's mirror of the config space at BAR0 +
  0x88000 (and 0x1800), through which a GeForce driver acknowledges an MSI by
  writing 0xFF to the capability's ID -- a write passed through, as QEMU's
  quirk does -- and through which, untrapped, the guest would write the real
  config space past every rule above.
- **Interrupts**: the host's handler of each vector finds its route in a
  static table (a function's slot and the message's index, no leaked
  object) and posts the guest's vector (`post_edge`: posted interrupts on
  VT-x, the mailbox and the doorbell otherwise -- from interrupt context, as
  they are built for). VT-d's posted interrupts would put it straight into
  the vCPU's `PostedInterrupts`, with no host interrupt -- but a unit has to
  have them (CAP.PI, bit 59), and neither of Hertz's has (dmar1's CAP
  0xd2008c40660462, dmar0's 0x1c0000c40660462), nor QEMU's: that waits for
  a Xeon Scalable or the like.
- **The guest's memory map**: RAM below 4 GiB ends at 0xFE000000 today; a VM
  with a host function gets a PCI hole for its 32-bit BARs and a 64-bit
  window above its RAM for the rest (BAR1 is 8 GiB), both in the DSDT's
  `_CRS`.
- **vm.toml**: `pci = ["01:00.0"]`, the word `pci=01:00.0`; refused where the
  IOMMU is off, or without `unsafe_interrupts` until IR is done. A group is
  for now a device: its other functions are blocked while the guest holds
  one, unless the guest holds them too, and a device one of whose other
  functions is the host's own (mastering) or another guest's is refused.

## The stages, and their gates

1. **Done.** ACPI's DMAR and MCFG, the fuzzer's `acpi` target reading them --
   Hertz's two among its inputs.
2. **Done.** VT-d with every device passed through and the faults said:
   the smoke boots under `-machine q35 -device intel-iommu` with `iommu=on`,
   the NVMe's DMA through the unit; `tests/iommu-test.py`, a DMA `iommu
   block` blocks said from the fault event with its requester and reason,
   the machine going on. It found that nos's NVMe driver waits with no
   deadline for a controller that has stopped (QEMU's, on the blocked fetch).
3. IR **done**, ECAM too: each unit taken over gets a table of 256
   entries, IRTA set, the entry cache invalidated, IR on with CFI clear --
   compatibility-format interrupts blocked; every entry SVT=01, verified
   against its source's requester id, the IO-APIC's from the DMAR scope
   that names it (by the MADT's id; a scope that names no PCI function --
   Hertz's 00:1e.7, QEMU's ff:00.0 -- taken by its path alone);
   `MsixTable` (MSI and MSI-X) and `IoApic` program handles in the
   remappable format, IrqBalance moves entries in the table. Turned on in
   `Vtd::setup`, before the IO-APIC's pins and the first MSI are
   programmed, the pins masked first. `pci=` no longer needs
   `unsafe_interrupts` where it is on. Gates: the smoke boots under
   `intel-iommu,intremap=on` (and on a `UBSAN=1` build), `iommu-test.py`
   with and without IR, `gpu-passthrough-test.py` with no
   `unsafe_interrupts`, and Hertz's metal. A device sending what IR blocks:
   `hv-passthrough-test.py`'s hostile phase has QEMU's NVMe write messages
   in the remappable format naming an entry nos never filled -- refused by
   the table, said. QEMU's VT-d cannot show the rest: it delivers a
   compatibility-format message whatever CFI says and gives a DMA's message
   no requester id, so CFI's block and the requester's check are the
   metal's still.
4. **Done**: domains (C++: `Vtd::domain_*`, held to the gate by `iommu
   identity`, the NVMe working through a table of its own), the claim, the
   reset, `kcore::iommu`. **And** an emulated device in a Linux guest under
   nos in QEMU: `hv-passthrough-test.py` (TCG, in CI) -- QEMU's NVMe behind
   a `pcie-root-port`, kept from nos's driver (`guestpci=01:00.0`), given by
   its MSI-X (its table and PBA trapped, 16 of its 65 vectors), the guest's
   driver reading, writing and taking its interrupts on three vectors; then
   the hostile phase, the controller driven by hand from the guest's user
   space (`tests/hv-passthrough/nvmepoke.c`): a write where the guest has no
   RAM blocked and said with the NVMe's requester id, the messages above
   refused, the controller answering after, nos whole. It found a
   capability's next pointer passed through to the device -- the list the
   guest walks, and that the host hides a capability in, was the guest's to
   change on a device that took the write: read-only now.
5. hv's host function (**built**: `devices/host.rs`, the run loop's bus,
   mirror and BAR moves, a guest of a holed memory map, the module's
   `passthrough.rs`; `hv-fuzz.py`'s `hostfn` target, the `platform`
   target's guest holding a GPU, the `linux` and `pvh` targets' holed
   layouts, each rule a mutant the gate catches). **On Hertz**
   (`tests/gpu-passthrough-test.py`): the A2000 in nos's guest under the
   dev loop below -- BAR0, BAR1's 8 GiB and BAR3 where nos put them, BOOT0
   read, nouveau up with its GSP (RM 570.144) booted by DMA through the
   domain, VRAM sized, and a thousand fences each signalled on the GPU's
   MSI, 55 to 90 us a round from the guest through nos and KVM and back.
   It found the 8 GiB BAR1 given up for having no address bit in its low
   register (`decode_bar` now), and nouveau's MSI re-arm since Pascal --
   a dword at 0x704 of the mirror, past the 256 bytes given -- dropped (now
   written through; a fence at a time does not need it). **NVIDIA's own
   driver** (580.178.04, the open modules Ubuntu builds for the guest's
   kernel, on a disk of the guest's): `nvidia-smi` names the A2000 -- RM up
   on its GSP -- and CUDA 12.9's deviceQuery, vectorAdd and matrixMul pass,
   matrixMul at 611 GFlop/s (`tests/gpu-passthrough-test.py`, its default).
   It took three more fixes: the function's INTx pin given (NVIDIA's probe
   refuses a function with neither an IRQ nor MSI-X), wired to a line
   nothing raises; `PciBus::host_target` held to the configuration data's
   ports -- every port past 0xCFF, a virtio disk's notify among them, had
   been taken for the GPU's space once the address latched named it, and
   the guest's disk stopped as the driver probed; and the 64-bit BAR window
   in the DSDT cut to the BARs, leaving Linux's HMM the room UVM needs.
6. **Done**: nos on Hertz's metal -- the real VT-d -- booted once from
   Ubuntu by `tests/gpu-passthrough/install-bare.sh` (nos-install on the
   empty NVMe, a one-shot GRUB entry, `deadman=900`, `panic=10`, the disk
   log, netconsole to a collector, an `/etc/rc` that ends in `reboot`): the
   graphics unit left be, `dmar1` taken over, the A2000 given to a guest
   running NVIDIA's own driver -- nvidia-smi, deviceQuery, vectorAdd and
   matrixMul (611 GFlop/s) pass -- and the machine back on Ubuntu 20
   seconds after `boot: complete`. It found the RTL8168H's INTx line left
   unrouted by UEFI (r8168 now takes MSI-X, then MSI), the console being EGA
   text in the A2000's legacy window (let go of with the GPU), and the TCO
   timer that does not count there (docs/real-hardware.md, Hertz).

### The dev loop on Hertz

Ubuntu stays the host: nos runs under QEMU/KVM (`nested=Y`), `-machine
q35,kernel-irqchip=split -device intel-iommu,intremap=on,caching-mode=on`,
the A2000 and its audio function given to it with `vfio-pci` behind a
`pcie-root-port`. nos's VT-d driver then drives QEMU's emulated unit, the GPU
behind it real; nos gives the GPU to its own Linux guest (L2), whose
nvidia-open drives it. The machine is never out of reach, and a nos that
hangs is a QEMU that is killed. Real VT-d silicon waits for stage 6.
