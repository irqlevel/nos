---
title: "A real NVIDIA card inside a nos VM"
description: "A Linux VM on nos's hypervisor now drives a real NVIDIA GPU with NVIDIA's own driver and runs CUDA at full speed. How it works, what broke on the way, and what's missing."
date: 2026-11-19
updated: 2026-11-19
tags: [hypervisor, hardware, drivers, safety, testing, x86-64]
---

*Most of what this post listed as missing has been done since it went up. That part is near the end, under [the second round](#the-second-round).*

Until now, every device a VM on nos's [hypervisor](/hypervisor) could see was one we emulated in software: a virtio disk, a virtio network card, a serial port. Now a VM can get a real PCI device of the host's, and the first one we tried was a graphics card.

The card is an NVIDIA RTX A2000 in a small test server we call Hertz. nos boots on Hertz, gives the card to a Linux VM, and the VM loads NVIDIA's own driver and runs CUDA on it. Here is the end of one run, as nos logged it. The lines that start with `gpu-guest:` come from the VM's console:

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

The three results come from NVIDIA's CUDA samples: deviceQuery, vectorAdd and matrixMul. Inside the VM, `nvidia-smi` sees the card with its 6 GB of memory. matrixMul does 611 GFlop/s, the same number we got earlier when the same VM ran inside nos running inside QEMU on the same machine. That benchmark only measures the GPU, so it doesn't care how many layers sit between the card and the code.

## Why you can't just hand the card over

A PCI card doesn't wait for the CPU to move its data. It reads and writes memory on its own, by DMA, at whatever addresses its driver gives it. Inside a VM the driver hands out the VM's own addresses, and that is not where the VM's memory really lives on the host. Worse, nothing stops a malicious driver from handing out the address of the host's kernel instead.

The fix is an IOMMU, called VT-d on Intel machines. It sits between the card and memory and translates every address the card uses, the same way the CPU's page tables translate the addresses of a program. We give the card a table of its own that maps the VM's memory and nothing more. When the driver asks the card to write into one of its buffers, the write lands in the right place. When it asks for anything outside the VM, the IOMMU blocks it, and nos writes to its log which device tried and what it tried to do.

Interrupts are the trickier half. A modern PCI card interrupts the CPU by writing a few bytes to a special range of addresses, 0xFEExxxxx. That's MSI, and these writes aren't translated like ordinary DMA. The target CPU and the interrupt vector are encoded in the address and the data. So a driver that controls where the card writes can make it fire any interrupt on any CPU of the host, including the ones the kernel uses to flush TLBs or to run its scheduler. Linux's VFIO won't give a device to a VM on a machine without interrupt remapping unless you set `allow_unsafe_interrupts`. We had the same rule and our own flag for it, `unsafe_interrupts`, until we built interrupt remapping as well.

## The kernel side

The VT-d driver is new. It's written in C++ next to the kernel's interrupt and memory code, and that was on purpose: interrupt remapping changes how the kernel sets up every interrupt it has, not only the ones that belong to VMs.

When nos boots with `iommu=on`, it finds the remapping units in the ACPI DMAR table and takes them over. At first nothing changes for the devices: their addresses pass through untranslated, and no driver had to be touched. Updates to a unit's tables go through its invalidation queue, every wait on the hardware has a deadline, and every blocked access shows up in the log. Hertz's units don't watch the CPU's caches, so each table entry gets flushed to memory with `clflush` before the unit is told about it.

A device given to a VM gets a domain, which is a page table that maps the VM's memory. Where that memory comes from nos's pool of 2 MiB pages, the table uses 2 MiB entries as well. The domain holds on to every page it maps until the device has been blocked and the unit has thrown away its cached translations, so a bug in the hypervisor can't leave the card pointing at memory that has already been freed. A device that leaves its domain stays blocked until it is reset.

Interrupt remapping came last. Each unit gets a second table, this one for interrupts. Every interrupt source on the host, each MSI and MSI-X vector and each IO-APIC pin, gets an entry there that says which vector to raise on which CPU, and the unit only accepts it from the device the entry was made for. Interrupts in the old, untranslated format are blocked and reported. With that in place, the worst a card can do by writing to 0xFEExxxxx is raise its own interrupt, and the VM no longer has to be trusted.

## What the VM sees

The hypervisor shows the VM the card's real configuration space, but filters what goes through. Most of it is read from and written to the card directly. A few parts stay with the host:

- The BARs, the address windows through which the driver reaches the card's registers and memory. The VM can move them anywhere in its address space, and the hypervisor maps the real BARs to wherever the VM put them. The card's own BAR registers never change.
- Memory decoding stays on as long as the VM has the card. If the VM turned it off and then touched the card, the card would answer with an error that some servers turn into a machine check. Linux's VFIO had a CVE for exactly that.
- MSI is emulated. The host owns the card's real interrupt, and when it fires, the host delivers whatever vector the VM asked for.
- The bits that reach beyond the card itself (phantom functions, no-snoop, the function reset), the link controls and power management stay with the host.

At first, cards with MSI-X were turned away. Their interrupt table sits inside a BAR, and until we trapped that page, the VM could have pointed those interrupts anywhere it liked. That page is trapped now, as described in [the second round](#the-second-round).

NVIDIA added one more thing to worry about. Their GPUs keep a copy of the PCI configuration space inside BAR0, at offset 0x88000. If that page were mapped into the VM as it is, the driver could change the real configuration through it and skip all of the filtering above. So that one page is trapped, and every access to it goes through the same filter. Two kinds of writes are passed on to the real copy, because the driver uses them to re-arm the card's MSI after each interrupt: a write to the MSI capability's ID on older cards, and one at 0x704 on Pascal and newer. Without them the card sends exactly one interrupt and then goes quiet.

The VM's memory layout changes too. A VM that holds a real card gets its RAM below 4 GiB cut off at 3 GiB. The card's 32-bit BARs go into the gap, and its 64-bit BARs go above all of the RAM. The A2000's BAR1, its window into video memory, is 8 GiB by itself. Both regions are described in the VM's ACPI tables, so Linux leaves every BAR where we put it.

## Taking the card away from the host

Before the VM gets the card, the hypervisor claims it so that no other VM can take it. The A2000 also has an HDMI audio function, and functions of one device can reach each other, so that one is blocked with an empty domain. Then the card gets a function-level reset, with its configuration saved before and restored after, and its MSI is hooked up to a handler on the host. When the VM is done, all of this is undone in reverse order, and the card is reset once more and left blocked.

On Hertz the A2000 is also the card the firmware boots with, and nos's text console turned out to be writing into that card's old VGA memory. So now, when the card goes to a VM, the console stops writing there for good. Linux's vfio-pci does the same to efifb.

## How we tested it

A lot of this was tested before we went anywhere near the real card. The hypervisor's fuzzer got a model of a real PCI function and checks two things: whatever the VM writes, only what the host allows reaches the device, and whatever the VM reads back follows the PCI spec. For each rule we cared about, we deleted it from the code once and made sure the fuzzer caught it. A separate test under QEMU takes over QEMU's emulated VT-d, reads an NVMe disk through it with and without a domain of its own, checks that a blocked access gets logged, and runs with interrupt remapping both on and off.

For the real card, we first ran nos inside QEMU and KVM on Hertz, with QEMU's virtual IOMMU in front of the GPU and Linux's VFIO underneath. nos took over that virtual IOMMU and passed the card to a VM of its own, so we had a VM inside a VM with a real GPU at the bottom. QEMU ran its IOMMU in caching mode, which makes it copy every domain nos creates into the real IOMMU. The first driver we tried was nouveau, the open-source one. It booted the card's firmware through our domain and got a thousand GPU fences signalled by the card's interrupt, each round trip taking 55 to 90 microseconds through both hypervisors. Then came NVIDIA's own driver, the open kernel modules Ubuntu builds for the VM's kernel, with `nvidia-smi` and the three CUDA samples. That's now `gpu-passthrough-test.py`. It builds the VM's disk on the test machine itself, in chroots, separate from the machine's own system.

## On the real hardware

Then nos had to boot on Hertz itself. Hertz has no BMC and no serial port, so if nos had hung there, only the hosting provider could have rebooted it. Before trying, we set up several ways back:

- GRUB boots nos only once, so any reset after that brings Ubuntu back.
- `panic=10` reboots a nos that panics.
- A new boot parameter, `deadman=N`, makes the kernel panic N seconds after boot unless someone types `deadman off`. That covers a nos that boots fine but can't be reached.
- `/etc/rc` runs the test and then reboots.
- The kernel log goes to a log area on nos's own disk, and over the network to a collector on another server.

Going from Ubuntu to nos and back takes about three and a half minutes, most of it in the firmware. nos itself prints `boot: complete` after seven seconds, and twenty seconds after boot the VM has run everything and the machine is rebooting.

It took three boots. The first one came up without any network at all. The test still ran and passed, and we only found out by reading the disk log after Hertz was back in Ubuntu. The second one had network and a live log, and the screen went away with the card. The third one was the first with interrupt remapping on real hardware: no `unsafe_interrupts`, and the unit didn't have to block a single interrupt during the whole run.

## What went wrong along the way

The 8 GiB BAR vanished without a trace. To find a BAR's size, you write all ones into its register and read back which bits stuck. An 8 GiB 64-bit BAR has no address bits at all in its lower half, and our first decoder took that for an unused BAR and quietly dropped it. The VM saw the GPU, just without its window into video memory.

Next, the VM's disk froze every time NVIDIA's driver probed the card. To reach a device's configuration space, software first writes the device's address to I/O port 0xCF8, then uses ports 0xCFC to 0xCFF for the data. Our check for "is this access meant for the passed-through card" looked only at the latched address and at whether the port was 0xCFC or higher. As soon as the driver latched the GPU's address, every port above that went to the GPU too, including the one the VM uses to tell its virtio disk about new requests. The disk stopped in the middle of the probe. nos's tracepoints on the VM's disk showed us where its requests were getting lost.

NVIDIA's driver also wouldn't start without a legacy interrupt line. The card runs on MSI, and its old INTx line is switched off on purpose, but the driver checks that some IRQ exists before it switches to MSI. Now the VM sees the card's real interrupt pin, wired to a line that never fires.

After that, CUDA failed with "initialization error", which turned out to mean that the VM had run out of free address space. NVIDIA's unified memory driver asks Linux for a free range of physical addresses as big as the card's video memory. Our ACPI tables said the window for 64-bit BARs ran from the end of RAM all the way to the top of the address space, so there was nothing left to give, and the kernel answered with error 34. The window now stops at the last BAR, rounded up to a gigabyte.

On the real hardware, the network card was a story of its own. Its driver, r8168, had been written for a server that turned out to have a different Realtek chip, so Hertz's RTL8168H was the first real one it ever met. Under UEFI the firmware doesn't route the card's legacy interrupt line, and the driver knew no other kind of interrupt, so nos came up with no network. Now it tries MSI-X first, as the driver for the newer RTL8125 already did.

The hardware watchdog was a letdown. Intel chipsets have one that reboots a machine that hangs, and newer chipsets keep its "don't reboot" bit in a new place. We taught nos where to find it, but on Hertz the timer doesn't count at all, even when we start it by hand from Linux. A hard hang there still needs a human, and that is why `deadman` is done in software.

Linux kept renaming the disks. Hertz's two NVMe drives swap names between boots, so `/dev/nvme0n1` is a different drive every other time. Once, reading nos's disk log, we got Ubuntu's EFI partition instead (luckily the reader never writes). Everything that touches nos's disk now finds it by serial number.

QEMU 10 quietly turned interrupt remapping on. Its emulated IOMMU now sets `intremap` to `auto` by default, which means "on" under TCG, so our test's mode without interrupt remapping had in fact been running with it. The test now asks for the mode it wants.

And the IO-APIC lives at an address where nobody answers. The DMAR table identifies it by a PCI path, 00:1e.7 on Hertz and ff:00.0 in QEMU, but no PCI function sits there to answer anything. Our code expected one, so the first boot with interrupt remapping under QEMU refused to remap anything at all.

On the bright side, in the last test run the card raised 89 interrupts, and every one of them reached the VM.

## The second round

After this post went up, we went through the list of what was missing and did most of it. The A2000 didn't need any of it, since it already worked. But most network cards and NVMe drives use MSI-X, so until now they couldn't go to a VM at all.

### MSI-X, and a test that attacks the IOMMU

A device with MSI-X keeps its table of interrupts inside one of its BARs. The hypervisor now traps the pages that table and its pending bits are on, and the VM fills in an emulated copy. The real table is filled in by the host, one host vector per entry, and the VM gets at most 16 of them.

nos has drivers of its own for NVMe drives and network cards, and a function one of them has already taken can't be given away. A new boot parameter keeps the drivers off it: `guestpci=01:00.0`.

There's a test for all of this now, and it runs in CI with no KVM and no real hardware. nos runs under QEMU, takes over QEMU's emulated VT-d and gives an emulated NVMe drive to a Linux VM. The VM's driver reads a pattern the test wrote into the drive's image, writes its own and reads it back past the page cache, and afterwards the test finds that pattern in the image.

Then the VM turns hostile. A small program in it takes over the NVMe controller from user space and tells it to do what no normal driver would: write a page where the VM has no memory, and write into the interrupt address range, in the remapped format, naming table entries nos never filled in. The IOMMU blocks both, nos logs them, and the controller still answers afterwards:

```
vtd: dmar0: write of 0x40000000 by 01:00.0 blocked: 0x05, write to a page not mapped writable
vtd: dmar0: interrupt 0xFF from ff:1f.7 blocked: 0x22, interrupt entry not present
```

The first line names the NVMe drive by its PCI address. The second names no real device: QEMU doesn't say who sent an interrupt that came in as a plain DMA write, so the sender shows up as ff:1f.7, which stands for nobody. That means this test can't show the IOMMU checking who sent an interrupt. Nor can it show that interrupts in the old format are blocked, because QEMU's VT-d delivers those even when told not to. Both still need a hostile run on real hardware.

### What the VM sees now

In a real computer, a device sits behind a PCIe root port, on a bus of its own, and now that's how it looks in the VM too. The hypervisor puts an emulated root port in front of each device it gives away, the same generic one QEMU uses, so Linux already knows it. The device's functions sit behind it with their own numbers. If the VM also gets the A2000's audio function, the two sit side by side as functions 0 and 1, which is where the audio driver looks for its GPU. The port reports the device's real link speed and width.

The VM can also reach the extended configuration space now, through ECAM, which maps it into memory. nos uses ECAM on the host as well. On x86 it used to go through the old I/O ports only, and those don't reach past the first 256 bytes. In the VM, the ECAM window sits at the top of the gap below 4 GiB and is reserved in the ACPI tables and the memory map, because Linux won't use one that isn't. The VM can read the device's extended capabilities but not write them, and it only sees the ones that are useful read-only, like AER's error status, the serial number or the link. Anything that only works when written to, like SR-IOV, ATS or Resizable BAR, is hidden.

There's a ROM BAR now too, though not the real one. nos reads the device's ROM once when it gives the device away, and when the VM switches its ROM BAR on, a copy is mapped there read-only. At first we trapped the ROM like the MSI-X table, and the VM stopped as soon as Linux read the NVMe's ROM. Linux copies a ROM with `memcpy_fromio`, which is a `rep movs`, one instruction that moves the whole block, and the hypervisor's emulator for trapped device memory doesn't handle string instructions. A read-only copy keeps the emulator out of it.

### Resets and IOMMU groups

A device without a function-level reset used to get no reset at all. Now, if the device is alone on its bus, as it is behind a PCIe port, nos resets it through the bridge above it, with a secondary bus reset. A device with neither kind of reset still goes to the VM, with a line in the log saying so. Around a reset, nos saves and restores the device's state, now including the size of each Resizable BAR, since a reset can put those back to their defaults.

The bigger change is isolation. Before, nos looked at the device and stopped there. But what one function sends can reach another function directly, peer to peer, without passing through the IOMMU: inside a multi-function device, or through a switch port or root port that doesn't stop it. PCIe has a capability for stopping it, called ACS. Functions that can reach each other this way form an IOMMU group, and a VM gets a whole group or nothing. nos now turns ACS on wherever it exists, works out the groups, and blocks the rest of a group while a VM holds one of its functions. A group with a function nos itself uses, or one another VM holds, is refused. So is a device behind an old PCI bridge, since the IOMMU sees all its requests with the bridge's ID. The same goes for a device the firmware reserved memory for in the DMAR table (an RMRR), because the firmware expects its DMA to keep reaching that memory. Our groups are a bit stricter than Linux's: a switch port without ACS puts the whole switch into one group, and a function of a multi-function device without ACS joins all of its siblings.

### Seeing what can be given

With all that, it got hard to tell by eye what can go to a VM. `hv pci` now lists every PCI function of the host the way nos would give it: whether a driver of nos's has it or it's kept for VMs, how it would be reset, its interrupts, its group, the VMs that name it, and, if it can't be given, why. Here's part of the list from the test's QEMU machine:

```
> hv pci
00:02.0 8086:10d3 02.00  a driver's  reset none  MSI-X 5  -- not given: a driver of the host's has it -- guestpci=00:02.0 keeps it
00:04.0 1b36:000c 06.04  free  reset none  MSI-X 1  -- not given: a bridge
01:00.0 1b36:0010 01.08  kept for guests  reset FLR  MSI-X 65  group 01:00.1
01:00.1 1af4:1042 01.00  kept for guests  reset FLR  MSI-X 3  group 01:00.0
02:00.0 1af4:1042 01.00  kept for guests  reset bus reset  MSI-X 3
03:00.0 1af4:1044 00.ff  kept for guests  reset FLR  MSI-X 2  group 03:00.1  -- not given: its IOMMU group holds 03:00.1, the host's
```

That machine is set up to hit each case. The NVMe drive and the virtio disk next to it are two functions of one device, so they share a group. The second virtio disk has no function-level reset and gets reset through its bridge. The random number generator has two functions, and nos refuses to give the first because it uses the second itself.

`hv pci attach` and `detach` add a function to a stopped VM's config or take it off, and refuse anything the list says can't be given. The same list is in the API and in the [management panel](/blog/management-panel), on the "Virtual machines" page, with a "Give to a VM" button on each row. There's still no hotplug: a VM gets its devices when it starts.

### What broke this time

The first run of the test with the second disk read that disk's queue out of nos's own memory, and QEMU reported `Guest says index 32768 is available`. QEMU's virtio devices go around its IOMMU unless you add `iommu_platform=on`, so the disk took the VM's addresses for real ones. That was our mistake in setting up the test, not a bug in nos, but it shows nicely what a device does when nothing translates its addresses.

The hostile test was flaky at first: now and then one of the blocked attacks never made it into the log. nos's VT-d fault handler read the unit's fault status once. If another fault arrived while it ran and the unit had nowhere left to record it, the unit set its overflow flag, and the handler left that flag set. A unit with the flag set doesn't record any new faults, so QEMU's dropped every later one without a word. The handler now goes around until nothing is pending.

Linux also complained about our ACPI tables, calling it a firmware bug: the host bridge claimed buses 0 to 255, while the ECAM window covered only 0 to 7. In a VM, the firmware is us, so the bridge now claims the same eight buses.

As before, we deleted each new rule from the code once and made sure the hypervisor's fuzzer caught it, 57 more rules in all. On the way, the fuzzer found one real hole: the VM's writes to a capability's "next" pointer went through to the device. That byte is read-only on a well-behaved device, but it's the very list the host edits to hide capabilities from the VM, so the VM can't write it at all now.

With all of this in, we ran the GPU test on Hertz again, nos under QEMU there as in the first test. NVIDIA's driver didn't mind the root port in front of the card: `nvidia-smi` and the three samples passed, and matrixMul did 610.78 GFlop/s. We haven't booted Hertz on bare metal with the second round yet.

## What's missing

- Only Intel. AMD's IOMMU needs a driver of its own, which we haven't written yet, so our AMD servers can't pass devices through.
- Every interrupt from a device still goes through a handler on the host. VT-d can deliver interrupts straight into the VM's virtual CPU, but only on units with posted interrupts, and neither of Hertz's two units has them. Neither does QEMU's, so this waits for a Xeon Scalable or something like it.
- The VM can read a device's extended configuration space but not write to it, so it gets no Resizable BAR, SR-IOV, ATS or anything else that only works when written to.
- A device with MSI-X shows the VM at most 16 of its vectors. QEMU's NVMe has 65.
- A device that has no function-level reset and shares its bus with others can't be reset, and goes to the VM as it is.
- Our IOMMU groups are stricter than Linux's, so a device Linux would pass through may be refused here.
- A VM with a real device can't be saved or [moved to another host](/blog/live-migration). The device's DMA doesn't show up in the dirty log, and part of the VM's state lives inside the device.
- A VM gets at most four devices, and only when it starts. There's no hotplug.
- The hostile test runs only under QEMU, which can't show that interrupts in the old format get blocked or that an interrupt's sender gets checked. We haven't tried either on real hardware yet.

The code lives in the kernel's [VT-d driver](https://github.com/irqlevel/nos/blob/master/src/base/arch/x86_64/vtd.cpp), in the hypervisor's [view of the host's device](https://github.com/irqlevel/nos/blob/master/src/crates/hypervisor/hv/src/devices/host.rs), and in the [module that hands it over](https://github.com/irqlevel/nos/blob/master/src/crates/modules/hypervisor/hv/src/passthrough.rs). The plan and its stages are in [plans/gpu-passthrough.md](https://github.com/irqlevel/nos/blob/master/plans/gpu-passthrough.md), the register-by-register details are in the [hypervisor docs](/docs/hypervisor#a-hosts-pci-device), and the tests are [gpu-passthrough-test.py](https://github.com/irqlevel/nos/blob/master/tests/gpu-passthrough-test.py) and, for the bare-metal runs, [install-bare.sh](https://github.com/irqlevel/nos/blob/master/tests/gpu-passthrough/install-bare.sh). It all came in with [one merge](https://github.com/irqlevel/nos/commit/ff6c28be).

The second round added an [emulated root port](https://github.com/irqlevel/nos/blob/master/src/crates/hypervisor/hv/src/devices/port.rs), the [IOMMU groups](https://github.com/irqlevel/nos/blob/master/src/crates/modules/hypervisor/hv/src/groups.rs), [`hv pci`](https://github.com/irqlevel/nos/blob/master/src/crates/modules/hypervisor/hv/src/hostpci.rs) and a new test, [hv-passthrough-test.py](https://github.com/irqlevel/nos/blob/master/tests/hv-passthrough-test.py), which runs in CI. All of it is in [pull request 23](https://github.com/irqlevel/nos/pull/23).
