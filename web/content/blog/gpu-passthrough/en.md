---
title: "A real GPU in a guest: passing an NVIDIA card through nos's hypervisor"
description: "A Linux guest of nos's hypervisor now drives a host's NVIDIA GPU with NVIDIA's own driver and runs CUDA at full speed. How it works and what it can't do yet."
date: 2026-11-19
tags: [hypervisor, hardware, drivers, safety, testing, x86-64]
---

nos's [hypervisor](/hypervisor) can now give a guest a whole PCI device of the host's, and the first device we gave away was a GPU. An NVIDIA RTX A2000 sits in a small test server we call Hertz. nos boots on that server, takes the card away from itself and hands it to a Linux guest, which loads NVIDIA's own driver and runs CUDA programs on it. This is nos's log on Hertz during one such run, with the guest's console lines in it:

```
hv: 01:00.0 given to a guest -- its DMA reaches the guest's RAM alone,
    its interrupts remapped: what it sends raises its own vector and
    no other
gpu-guest: insmod nvidia exit 0
gpu-guest: nvidia-smi exit 0
gpu-guest: Device 0: "NVIDIA RTX A2000"
gpu-guest: Result = PASS
gpu-guest: Test PASSED
gpu-guest: Performance= 610.86 GFlop/s, Time= 0.215 msec, ...
gpu-guest: Checking computed result for correctness: Result = PASS
```

The three results are CUDA's own samples: deviceQuery, vectorAdd and matrixMul. `nvidia-smi` in the guest shows the card with its 6 GB at 00:02.0 of the guest's bus. matrixMul ran at 611 GFlop/s, the same number we got when that guest ran inside nos inside QEMU on the same machine, because it measures the GPU and not the way to it.

This post is about what had to be built for that, the things that surprised us on the way, and what doesn't work yet.

## Why a device needs an IOMMU

A PCI device reads and writes memory on its own, by DMA, at addresses its driver gives it. A guest's driver gives it guest-physical addresses, and those are not where the guest's memory really is in the host. Worse, a guest that wants to can give it any address at all, and then the device reads or writes any byte of the host. So the device's addresses have to go through a translation, the way a program's do through the CPU's page tables. That is the IOMMU's job, VT-d on Intel machines. We give the GPU a table of its own, a domain, which maps the guest's RAM at the guest's own addresses and nothing else. The GPU's DMA lands exactly where the guest's driver meant, and anything else is blocked and reported.

Interrupts are the other half. A PCI device interrupts the CPU by writing to an address in the 0xFEExxxxx range (that is MSI), and such a write is not translated like DMA: it goes to the processors as an interrupt, with the vector and the target CPU in the address and data. A guest that can point its device's DMA anywhere can therefore make it write there, and raise any interrupt on any CPU of the host, the TLB shootdown's or the scheduler's included. Linux's VFIO refuses to give a device to a VM on a machine without interrupt remapping unless it is told `allow_unsafe_interrupts`. nos refused the same way with a word of its own, `unsafe_interrupts`, until interrupt remapping was done.

## The IOMMU, in C++

The VT-d driver is new, and it is C++. It lives next to the interrupt and memory code it extends, and that was a deliberate choice: interrupt remapping changes how the kernel programs every interrupt it has, not only a VM's. With `iommu=on` the kernel reads the ACPI DMAR table, takes every remapping unit over and starts by passing every device through untranslated, so that no driver changes. Changes to the unit's tables go through its invalidation queue, every wait on the unit has a deadline, and every DMA a unit blocks is reported with the device and the reason. Hertz's units don't snoop the CPU's caches, so every table entry is written back with `clflush` before the unit is told about it.

A domain is a second-level page table of the guest's RAM, in 2 MiB pieces where the RAM comes from nos's huge page pool. It holds on to every page it maps until the device has been blocked and the unit's caches flushed, so no bug in the hypervisor can let the device write into memory that has already been freed. A device that leaves a domain is blocked, and it is not passed through again until it has been reset.

Interrupt remapping came last. Every unit gets a table of interrupt entries, and every interrupt the host has, each MSI and MSI-X vector and each IO-APIC pin, becomes an entry that names its vector and CPU and is accepted only from the device it belongs to. Interrupts in the old, untranslated format are blocked. A device's write to 0xFEExxxxx can then raise its own vectors and no others, and a guest no longer has to be trusted with the GPU.

## What the guest sees

The hypervisor shows the guest the GPU's real configuration space, read through to the device, with the host's rules laid over it. The BARs, the windows through which a driver reaches the card's registers and memory, are the guest's to move: it puts them where it likes in its address space, the real registers never change, and the hypervisor maps the real BARs into the guest's nested page table at the places the guest chose. Bus mastering goes to the device, but memory decoding stays on for as long as the guest holds the card, because a device that stops decoding answers the next access with an error a server may turn into a machine check (that one was a CVE in Linux's VFIO). MSI is emulated. The host owns the real MSI, and when the card fires it, the host's handler injects the vector the guest wrote into its emulated copy of the capability. The control bits that reach beyond the device, such as phantom functions, no-snoop and the function-level reset, stay the host's, and so do the link and power management. A device with MSI-X is refused for now: its vector table sits in a BAR, and until that page is trapped the guest could aim it anywhere.

Then there is NVIDIA's mirror. NVIDIA GPUs keep a copy of their own configuration space inside BAR0, at offset 0x88000. Left mapped, it would let the guest rewrite the real configuration space and skip every rule above, so that page is trapped and every access to it goes through the same view. Two kinds of write through it are let through to the real mirror, because the driver re-arms its MSI that way: a write to the MSI capability's ID on older GPUs, and a dword at 0x704 on Pascal and later. Without it the card sends one interrupt and then no more.

The guest's memory map needs room for the BARs. A guest that holds a host's device gets its RAM below 4 GiB cut at 3 GiB, its 32-bit BARs go into that hole, and its 64-bit BARs go above its RAM. The A2000's BAR1, the window into its video memory, is 8 GiB alone. Both windows are described in the guest's ACPI tables so that Linux leaves every BAR where it was put.

## Taking the card from the host

Before the guest gets the card, the hypervisor module claims it, so no other guest can take it. It blocks the card's other functions (the A2000 also has an HDMI audio function) in domains that map nothing, since functions of one device can reach each other, resets the card with a function-level reset, saving its configuration before and writing it back after, and takes its MSI for the host's handler. When the guest is done, the same happens backwards: the route of the interrupt to the guest goes first, then the interrupt, bus mastering, another reset, the domain, and the card stays blocked.

On Hertz the A2000 is also the card the firmware boots with, and nos's own text console turned out to be written into that card's legacy VGA memory. So giving the card away now takes the screen with it: the console stops writing there for good, much as Linux's vfio-pci takes efifb off a card it is handed.

## How we tested it

Most of the rules were tested on a laptop first. The hypervisor's fuzzer got a target with a model of a real function: whatever the guest writes to the configuration space, what reaches the device has to keep to the host's rules, and what the guest reads back has to keep to the PCI specification. For each rule we cared about, we took it out of the code once and checked that the fuzzer noticed. A gate under QEMU checks the IOMMU itself: QEMU's emulated VT-d taken over, an NVMe disk read through it and through a domain of its own, a blocked DMA reported, with interrupt remapping on and off.

The rest needed the real card. For the development loop, nos ran under QEMU and KVM on Hertz, with QEMU's virtual IOMMU in front of the real GPU and Linux's VFIO underneath. nos took that virtual IOMMU over and gave the card to a guest of its own, a VM inside a VM, with a real GPU at the bottom of the stack. QEMU ran its IOMMU in caching mode, which makes it follow every domain nos sets up and program the real IOMMU to match. The first driver was nouveau, the open one: the card's firmware booted through the domain, and a thousand GPU fences each signalled by the card's interrupt, 55 to 90 microseconds a round trip through two hypervisors. Then NVIDIA's own driver, the open kernel modules Ubuntu ships for the guest's kernel, with `nvidia-smi` and the three CUDA samples. That is now `gpu-passthrough-test.py`, and it builds the guest's disk on the machine itself, in chroots kept apart from the machine's own system.

## On the machine itself

Then nos had to boot on Hertz itself. Hertz has no BMC and no serial port, so if nos hung there, nobody but the hosting provider could reset it. Before the first boot we gave it several ways back. nos boots once from a GRUB entry, so the next reset of any kind lands in Ubuntu again. `panic=10` resets a nos that panics. A new boot parameter, `deadman=N`, panics N seconds into the boot unless someone has typed `deadman off` by then, which brings back a nos that runs fine but can't be reached. `/etc/rc` runs the test and ends with `reboot`. The kernel's log goes to a disk log on nos's own disk and over the network to a collector on another server. A round trip from Ubuntu to nos and back takes about three and a half minutes, most of it the firmware. nos itself says `boot: complete` after seven seconds, and twenty seconds after boot the guest has run everything and the machine is rebooting.

It took three boots. The first had no network at all. The test ran anyway and passed, and we read it from the disk log once Hertz was back in Ubuntu. The second had its network back, a working netconsole and the screen released. The third was the first with interrupt remapping on real silicon: the card was given away without `unsafe_interrupts`, and the unit blocked nothing during the whole run.

## Things that surprised us

The 8 GiB BAR went missing in silence. A BAR's size is found by writing all ones into its register and reading back which bits stuck. A 64-bit BAR of 8 GiB has none of its address bits in the low register, so our first decoder took the low half for an unassigned BAR and dropped it without a word. The guest saw a GPU with no window into its video memory.

The guest's disk stopped when NVIDIA's driver probed. To reach a device's configuration space, software first writes the device's address to I/O port 0xCF8 and then reads or writes the data ports 0xCFC to 0xCFF. Our check for "is this an access to the passed-through card's configuration space" looked only at the latched address and at whether the port was at least 0xCFC. Once the driver had latched the GPU's address, every port above that went to the card's configuration space, including the I/O port where the guest's virtio disk is told about new requests. The disk stopped in the middle of the probe. nos's tracepoints on the guest's disk showed where its requests stopped, and that led to the bug.

NVIDIA's driver refuses a device without a legacy interrupt line. The card uses MSI, and its INTx line is turned off on purpose, but the driver's probe checks that some IRQ exists before it switches to MSI. Now the guest sees the card's real interrupt pin, wired to a line that nothing ever raises.

CUDA's "initialization error" meant the guest had no free address space. NVIDIA's unified memory driver asks Linux for a free range of physical addresses as large as the card's video memory. Our ACPI tables told the guest that the PCI window for 64-bit BARs reached from the end of its RAM to the top of the address space, so there was nothing left to give, and the kernel answered with error 34. The window now ends where the last BAR does, rounded up to a gigabyte.

The network card had never been run before. Its driver, r8168, was written for a server that turned out to have a different Realtek chip, so Hertz's RTL8168H was the first real one it ever saw. Under UEFI the firmware leaves the card's legacy interrupt line unrouted, and the driver used nothing else, so nos came up with no network. The driver now takes the card's MSI-X first, as the driver of its bigger sibling already did.

The watchdog doesn't tick. Intel's chipsets have a hardware watchdog that resets a machine that hangs, and newer ones keep its "don't reset" bit in a different place. We taught nos to find it there, but on Hertz the timer doesn't count at all, not even when we start it by hand from Linux, so a hard hang there still needs a person. That is why `deadman` is software.

Linux renames the disks. Hertz's two NVMe drives swap their names between boots, so `/dev/nvme0n1` is a different disk every other time. Everything that touches nos's disk now names it by its serial number, after one read of the disk log landed on Ubuntu's EFI partition (it only read, so nothing was lost).

QEMU 10 turns interrupt remapping on by itself. The emulated IOMMU's `intremap` option now defaults to `auto`, which means on under TCG, so our test's "without interrupt remapping" mode was quietly testing with it. The test now says which one it wants.

The IO-APIC lives at an address nobody answers. The DMAR table names the IO-APIC by a PCI path, 00:1e.7 on Hertz and ff:00.0 in QEMU, and that path is all there is: there is no function there to answer a configuration read. Our scope resolver insisted on one, so the first interrupt remapping boot under QEMU refused to remap anything.

In the last test run the card raised 89 interrupts, and the host passed every one of them on to the guest. Not one was lost.

## What it can't do yet

Only Intel machines can do this. AMD's IOMMU has a driver of its own to be written, so our AMD servers can't pass a device through. Devices with MSI-X are refused, which rules out most network cards and NVMe drives for now: the GPU we have uses plain MSI. The guest sees only the first 256 bytes of the card's configuration space, because nos doesn't use the extended configuration space (ECAM) on x86 yet, so it gets no Resizable BAR, no AER and no ACS. There is no ROM BAR either. NVIDIA's driver finds the card's firmware another way, but other drivers may not.

A device is reset only by its function-level reset. One without it isn't reset at all, because resetting through the bridge above it isn't done yet. Isolation is per device: the device's own functions are blocked or held by the same guest, but the bridges above it aren't checked for ACS, and a device the firmware has reserved memory for (an RMRR) isn't refused. Every interrupt of the card still goes through a handler on the host. VT-d can post a device's interrupt straight into a virtual CPU, and that is one of the next steps. A VM holding a host's device can't be saved or [moved to another host](/blog/live-migration), since the device's DMA doesn't reach the dirty log and part of its state lives in the device. A guest takes at most four devices, and they are given when it starts. And there is no test yet in which a device actually sends something interrupt remapping has to block, and checks that it is blocked and reported.

The code is in the kernel's [VT-d driver](https://github.com/irqlevel/nos/blob/master/src/base/arch/x86_64/vtd.cpp), the hypervisor's [view of a host's function](https://github.com/irqlevel/nos/blob/master/src/crates/hypervisor/hv/src/devices/host.rs) and the [module that hands it over](https://github.com/irqlevel/nos/blob/master/src/crates/modules/hypervisor/hv/src/passthrough.rs). The design and its stages are in [plans/gpu-passthrough.md](https://github.com/irqlevel/nos/blob/master/plans/gpu-passthrough.md), the details register by register in the [hypervisor docs](/docs/hypervisor#a-hosts-pci-device), and the tests are [gpu-passthrough-test.py](https://github.com/irqlevel/nos/blob/master/tests/gpu-passthrough-test.py) and, for the bare-metal runs, [install-bare.sh](https://github.com/irqlevel/nos/blob/master/tests/gpu-passthrough/install-bare.sh). All of it came in with [one merge](https://github.com/irqlevel/nos/commit/ff6c28be).
