---
title: "The hypervisor"
description: "A type-1 hypervisor in Rust that loads into a running nos as a module. It boots unmodified Linux, Alpine, Debian and Ubuntu on AMD-V and Intel VT-x, and makes a VM of a cloud image from its URL."
updated: 2026-10-25
---

nos has a hypervisor of its own. It consists of two Rust crates: `hvarch` drives the CPU's virtualization extension, and `hv` does everything above it. Both go into a loadable module, `hv.ko`, which you load into a running kernel with `insmod` and take out again with `rmmod`. Once loaded, it turns on AMD-V or Intel VT-x and runs guests: tiny built-in ones that each check one mechanism, an unmodified Linux `bzImage`, and Alpine's ISO and Debian's and Ubuntu's cloud images as they ship. Guests can have several CPUs, virtio disks, a network with NAT, ACPI and a power button. A VM can live in a folder of its own on nos's disk and outlast a reboot, and `hv create` makes one from a cloud image given only its URL. On bare metal there is no Linux underneath: nos owns the machine.

The hypervisor is stage 3 of the [roadmap](https://github.com/irqlevel/nos/blob/master/plans/README.md). The roadmap ends with a bare-metal cloud node that has no Linux host, is managed over HTTP, and is updated live while its guests keep running. The goal is to run other people's Linux guests on the machine, so isolation comes before speed. The full reference, [The hypervisor](/docs/hypervisor), runs to some 3,800 lines. This page is the five-minute version.

## Why a module

There are three reasons ([Why a module](/docs/hypervisor#why-a-module)):

- **A machine that runs no guests carries none of it.** `hv` and `hvarch` are not default members of the Cargo workspace. A kernel built without the module has no hypervisor symbol at all, and its CPUs hold none of the hypervisor's state: `EFER.SVME` and `MSR_VM_HSAVE_PA` on AMD, `CR4.VMXE` and VMX root operation on Intel. The module turns them on when loaded and off when unloaded.
- **Rebooting is the worst way to debug the setup of a VMCB or VMCS**, the in-memory structures that describe a guest to AMD-V and VT-x. With a module, the loop takes seconds: `insmod`, try, `rmmod`, change a field, `insmod` again.
- **Unloading is checked by a gate.** [`hv-test.py`](/docs/testing#hv-testpy----the-extension-turned-on-and-off-again-and-guests-under-it) turns the extension on for every CPU, unloads the module *without* turning it off, loads it again, and asks every CPU, with an IPI that reads the register, what is still on. An `rmmod` that forgot a CPU would leave it saving host state into a freed page. Stage 5's live update will need this check, and here it exists years early.

The price is written down too: a module cannot register a block device or a NIC ([Loadable modules](/docs/modules)). So the module emulates a guest's disks and NICs on top of the kernel's own (`kcore::block::Disk`, `kcore::net::Nic`), which is what a hypervisor wants anyway.

## Where the unsafe lives

The line between code that touches the extension and code that does not is drawn once, in the crate graph, where a script can count it ([The crates](/docs/hypervisor#the-crates)). `hvarch` has the instructions, the VMCB and VMCS, and the world switch, the code that swaps the host's state for the guest's and back. `hv` has guest memory, the policy that decides what a guest sees and may do, the devices, the Linux loader and the run loop. Here is how [`scripts/unsafe-count.py`](https://github.com/irqlevel/nos/blob/master/scripts/unsafe-count.py) counts them on master on 25 October 2026:

| Crate | Lines | `unsafe` sites |
|---|---|---|
| `hvarch` (the extension) | 6,836 | 203 |
| `hv` (everything above it) | 19,499 | 4 |
| `modules/hv` (the module) | 7,653 | 0 |
| `diskimage` (qcow2 to a raw disk, and cloud-init's seed) | 1,845 | 0 |
| `hvboot` (runs in the guest) | 3,066 | 29 |

`hvboot` is a program of the guest's, not the host's. It boots the kernel the guest's own GRUB would, and its sites are the platform under it inside the guest: ports, physical memory, the heap, the virtio rings, the interrupt table and the jump into the kernel. The four sites in `hv` are the two IPI handlers that turn the extension on and off, and the two guest entries, one per backend. The entries are private to the crate. Each takes the `&GuestMemory` that holds the guest's nested page table, the table that translates the guest's physical addresses into the host's. So their preconditions are invariants of types:

- **Guest memory** is pages mapped nowhere but in the guest's nested page table, and they are zeroed before the guest sees them. Only a virtio queue's rings are also mapped for the host, and only while the queue is up. Nothing hands out a Rust reference into guest memory. The guest may write to it at any moment from any CPU, and a reference to memory that changes under the compiler is undefined behaviour. So every access is a bounds-checked copy, or, on those rings, one volatile load or store at a checked, aligned offset ([Its memory](/docs/hypervisor#its-memory)).
- **The nested page table lives inside the same value.** It maps only pages that this value owns, and it only grows. Letting a guest reach host memory it was not given is not a mistake a caller can make.

Keeping the CPU behind one crate also lets [`hv-fuzz.py`](/docs/testing#hv-fuzzpy----everything-a-guest-reaches-fuzzed) compile most of `hv` for the host, run it over a stand-in CPU and fuzz it in CI ([Fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)).

## AMD-V first, Intel VT-x second

The order was set by where the code is written and tried out ([AMD-V first](/docs/hypervisor#amd-v-first-intel-vt-x-second)). Development happens on a Mac, where the x86-64 kernel runs under QEMU's TCG, its software CPU emulator. TCG emulates SVM (another name for AMD-V), nested paging included, but it does not emulate VMX, Intel's extension, at all. AMD-V is also the simpler extension. The VMCB is a plain structure in memory. VMX needs `vmread`/`vmwrite` for every field, and when it refuses an entry it reports one error number that can mean a dozen things. Use `-cpu max`, because QEMU's default `qemu64` has SVM without nested paging. TCG has quirks of its own, and [When the emulator is the bug](/blog/qemu-tcg-amd-v-quirks) tells the story of two of them.

Intel VT-x is for the machines nos actually runs on: the Hetzner EX44 and the Dell laptop are both Intel. It was brought up under nested KVM on an i5-13500. Above `hvarch`, both backends have the same shape. `hv::vm::Backend` is either `Svm` or `Vmx`, and both keep guest state in AMD's save-area layout. For VMX that is a shadow copy, synced into the VMCS lazily. So the policy and the loader are written only once ([The VMX backend](/docs/hypervisor#the-vmx-backend-and-where-it-differs-from-amd-v)). The last obstacle to `vmxon` was outside the hypervisor. VMX requires `CR0.NE`, which nos had never set. So `hv on` refused, with `hv info` naming the bit, until the boot path set it on every CPU.

## What it runs

- **Built-in guests.** There are nineteen, each a handful of instructions kept as bytes next to its source in [`guests.rs`](https://github.com/irqlevel/nos/blob/master/src/crates/hv/src/guests.rs) and checked against what it was told to do. Between them they cover memory above 4 GiB, every register carried both ways across a hypercall, a write stopped by the nested page table, a triple fault that stops the guest and not the CPU, and `cli; jmp $`, which the host stops anyway. One starts a second CPU with INIT and a start-up IPI. Another sends a thousand IPIs to a spinning CPU, which takes them inside its guest where the CPU has posted interrupts ([The built-in guests](/docs/hypervisor#the-built-in-guests)).
- **An unmodified Linux.** `hv boot` loads a 64-bit `bzImage` through Linux's boot protocol. The guest gets an emulated 8250 serial port, an 8254 PIT, an MC146818 RTC and two 8259 interrupt controllers. CPUID and MSRs are allow-lists ([A Linux guest](/docs/hypervisor#a-linux-guest); the story is in [Linux under the nos hypervisor](/blog/linux-under-nos-hypervisor)).
- **Distributions as they ship.** Alpine's `virt` ISO boots to a root login, with the ISO itself as the guest's read-only disk. Debian 13's cloud image boots under systemd and reaches `running` with no failed unit ([A distribution](/docs/hypervisor#a-distribution)). Debian's and Ubuntu 24.04's cloud images also boot off their own `/boot`, with nothing taken out of them. hv starts **hvboot** in the guest by PVH, Xen's direct boot protocol, and hvboot reads the image's GPT, ext4 and `grub.cfg` and boots the entry GRUB would ([A guest's own kernel](/docs/hypervisor#a-guests-own-kernel-pvh-and-hvboot)).
- **VMs in folders, and from a URL.** A VM can be a folder: `/vms/<name>/vm.toml` says how to boot it, its disks sit beside it, and its console goes to `console.log`. `hv start <name>` boots it, and the folder outlasts a reboot of nos. `hv create <name> <url>` makes such a folder from a cloud image. The kernel's own HTTPS client downloads the image, a converter with no `unsafe` makes the qcow2 a raw disk, nos writes cloud-init's seed, and the VM boots by hvboot ([A VM in a folder](/docs/hypervisor#a-vm-in-a-folder), [A VM from a URL](/blog/vms-from-cloud-images)).
- **Several CPUs.** `cpus=N` gives a guest up to 16. Each has a local APIC, in x2APIC mode unless told `xapic`. All but the first start the way a PC's do, with INIT and a start-up IPI into real mode. Virtio interrupts arrive by MSI-X, and an IO-APIC is there on request ([More than one CPU](/docs/hypervisor#more-than-one-cpu)). Under VT-x, on a CPU with posted interrupts, the CPU itself delivers an x2APIC guest's interrupts, including other CPUs' IPIs and devices' MSIs, so the guest does not have to exit to the hypervisor for them ([Interrupts the CPU delivers itself](/docs/hypervisor#interrupts-the-cpu-delivers-itself-vt-xs-apic-virtualization)).
- **Disks and a network.** `disk=` adds a legacy virtio-blk disk backed by a file on nos, and the host trusts nothing in its rings. `net` adds a virtio-net port on the module's switch. nos sits at 10.0.100.1 behind its virtual NIC `hv0`. NAT leads out, the switch answers DHCP, and `hv forward` provides the way in. Checksums and TCP sends of up to 64 KiB are offloaded both ways, and a send travels whole from one guest to another. Receive interrupts are held at least 100 µs apart ([A disk](/docs/hypervisor#a-disk), [A network](/docs/hypervisor#a-network), [Faster guest networking](/blog/faster-guest-networking)).
- **ACPI and MMIO.** Every guest gets ACPI tables and the fixed hardware they describe. That lets `hv stop` press a power button that systemd-logind or BusyBox's `acpid` answers ([ACPI](/docs/hypervisor#acpi)). Memory-mapped devices (so far the xAPIC page and the IO-APIC) work like this: the host fetches the faulting instruction through the guest's page tables and decodes it, with no `unsafe` ([MMIO](/docs/hypervisor#mmio)).

## An example session

Here is the guest lifecycle as shell commands, which stage 4 will put behind HTTP. It comes from a run of the gate under TCG, where the guest reaches its shell a second after it starts ([Guests that stay up](/docs/hypervisor#guests-that-stay-up)):

```text
$ insmod /hv.ko
$ hv on
$ hv start /bzImage initrd=/initrd cmdline=console=ttyS0 nolapic
hv: vm 0 started on cpu 3 -- /bzImage, 256 MiB, cmdline "console=ttyS0 nolapic"
$ hv exec 0 secs=60 id
... the rest of its boot ...
~ # id
uid=0 gid=0
~ #
$ hv send 0 echo nos$((6*7))nos\n
hv: vm 0: 20 bytes queued
$ hv wait 0 nos42nos
hv: vm 0 printed "nos42nos", 58 ms in
$ hv list
vm 0  running  cpu 3  256 MiB  1 s  exits 18237  irq 284  hlt 92  /bzImage
$ hv stop 0
hv: vm 0 stopped -- on request -- nothing in it listens to its power button
  stopped    on request, after 1925 ms
$ rmmod hv
module: hv unloaded
```

`hv exec` waits for a prompt printed *after* its line went in. So a line typed at a guest that is still booting still comes back with its answer. For the live console, use `hv attach` over `ssh -t`. The [Linux guest how-to](/howto/linux-guest) goes through it step by step.

A VM made from a cloud image takes two commands. This run, under TCG on a Mac, downloaded Debian 13's image in 21 s and converted it in 62 s. The guest's own sshd answered 49 s after `hv start` ([Make a VM from a cloud image](/howto/cloud-vm)):

```text
$ hv create debian https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 size=8G password=nos-howto-1 forward=2201:22
...
hv: /vms/debian/disk.raw: 907 MiB written of 3072 MiB, the rest holes -- 14487 clusters inflated, 788 copied, 0 zeros, 33877 not there -- in 62.0 s
hv: /vms/debian/vm.toml written -- hv start debian boots it: hvboot, a 8192 MiB disk, cloud-init given 1 key and a password
$ hv start debian
hv: vm 0 (debian) started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3", restarted when it resets
hv: port 2201 forwarded to vm 0, 10.0.100.2:22
```

## Keeping guests apart

What keeps a guest away from the host and from other guests is set below the policy, on every entry into the guest, whatever the policy asked for ([What entering a guest cannot be told to do](/docs/hypervisor#what-entering-a-guest-cannot-be-told-to-do)):

- **Intercepts the host depends on.** An intercept makes the CPU leave the guest for the host. The host intercepts its own interrupts and NMIs, every I/O port, every MSR except the three holding the guest's segment bases, and the virtualization instructions. It also intercepts #DB and #AC, because a guest could otherwise make their delivery loop inside the CPU (CVE-2015-8104, CVE-2015-5307). Machine checks (#MC) are intercepted too, and CR4.MCE stays set, so a machine check during a guest reaches the host instead of passing for the guest's triple fault.
- **The TLB.** Under AMD-V each CPU hands out ASIDs, the tags that keep guests' TLB entries apart, in generations. Under VT-x a guest keeps one VPID for life and gets an INVEPT on its first entry on each CPU ([Address space identifiers](/docs/hypervisor#address-space-identifiers)).
- **Speculation at the world switch.** After every exit the registers are cleared and the return stack is refilled. An IBPB separates one guest from the next. Where the CPU needs them, there is also automatic IBRS on AMD, and IBRS with BHI_DIS_S, VERW and the L1D flush on Intel ([Guest speculation isolation](/blog/guest-speculation-isolation)).
- **Registers a guest believes are its own.** Each guest keeps its own DR0–DR3. A guest reads CR4 back exactly as it wrote it, while the CPU's real CR4 keeps MCE set.
- **Cores.** By default a guest gets whole cores that no other guest has a CPU on, because the two threads of a core share what MDS and L1TF exploit.
- **Network and host services.** A frame must carry its port's own MAC and address. A large send that a guest leaves its NIC to cut into frames makes 256 frames at most. A guest reaches neither nos's sshd nor its connections. NAT keeps guests away from the cloud's metadata address, the private networks and the uplink's subnet, unless `nat allow` says otherwise. Shared resources have per-guest quotas.
- **Memory.** A reserve of a sixteenth of RAM, and at least 64 MiB, is off limits to every guest. A device may map its rings for the host eight times, and after that once a second, because undoing a mapping interrupts every host CPU.

Each fix came with a gate: a built-in guest, a check in `hv-linux-test.py`, or a model in `hv-fuzz.py`. Most of them were shown to fail against the code they replaced. What is still open is listed too, including SRSO and Retbleed on Zen 1 to 4 ([Before other people's guests](/docs/hypervisor#before-other-peoples-guests)).

## What a VM exit costs

`hv bench` times a guest that does nothing but exit, on one CPU ([What an exit costs](/docs/hypervisor#what-an-exit-costs)):

| Where | Extension | One exit |
|---|---|---|
| QEMU's TCG on the Mac | AMD-V, emulated | 17,130 ns, which says nothing about a real CPU |
| nested KVM on an i5-13500 | VT-x | 17,900 ns, then 5,800 ns after the entry path was slimmed down; 4.6 µs of that is the outer kernel's |
| AX41, Ryzen 5 3600 (Zen 2), bare metal | AMD-V | 731 ns with ASIDs kept, 940 ns with the TLB flushed on every entry |
| EX44, i5-13500, bare metal, base clock | VT-x | 1.69 µs on a P-core, 2.07 µs on an E-core; 1.82 and 2.49 µs with speculation mitigations |
| EX44, cores at turbo | VT-x | 1,018 ns on a P-core, 1,512 ns on an E-core |

An MMIO access costs more, because the host has to walk the guest's page tables to the instruction, fetch it and decode it. On an EX44 P-core a CPUID exit takes 1.8 µs and an MMIO load 4.5. An idle guest costs almost nothing: sixteen of them on the AX41's twelve CPUs keep the machine 1.6% busy.

## Where it has run

- **QEMU's TCG** and **nested KVM** on AMD and Intel hosts. This is where the gates run.
- **The AX41** (Ryzen 5 3600, Zen 2), bare metal. Linux reaches `Run /init` 18 ms into its own clock, against half a second under TCG. Debian reaches its login prompt in under four seconds. A gigabyte fetched through NAT comes in at 117 MB/s, and a guest writes its disk at 1.2 GB/s. The first run there stopped twice where TCG never had. CPUID leaf 0x80000001 passed MWAITX through, and Linux on a Zen reads AMD's FCH at a fixed address whether or not one exists ([On real hardware](/docs/hypervisor#on-real-hardware)).
- **The EX44** (i5-13500, six P-cores and eight E-cores), bare metal, VT-x, since 2026-10-20. The built-in guests run on both P- and E-cores, and Linux runs with one, two and four CPUs. A virtio disk does 1.0–1.2 GB/s with `fsync`, and Alpine reaches its login prompt in 13 s. An `rmmod` with five VMs running leaves VMX off on all twenty CPUs ([The EX44, under VT-x](/docs/hypervisor#the-ex44-under-vt-x)). Since 2026-10-22, one guest there sends another 500 MB over TCP in 2.72–2.83 s, down from 5.72. Five fetches of 100 MB from a web server take 0.29 s, or 1.8 GB/s ([Faster guest networking](/blog/faster-guest-networking)).
- **Not yet**: the Dell's Skylake, and arm64. On arm64 a hypervisor is not an extension that a kernel turns on, but an exception level that it runs at. nos drops from EL2 to EL1 at boot, so that backend only reports what EL2 would offer ([arm64](/docs/hypervisor#arm64)).

## The commands

| Command | What it does |
|---|---|
| `hv`, `hv info` | show the extension and the CPUs it is on for, as the CPUs themselves report it |
| `hv on`, `hv off` | turn it on or off, for one CPU or for all |
| `hv run <guest\|all> [cpu]` | run built-in guests |
| `hv bench` | time an exit or an MMIO access |
| `hv boot <kernel> ...` | run a Linux guest for a set time, then show its console; the kernel is a `bzImage` or a PVH ELF such as hvboot |
| `hv start <kernel> ...` | the same, but the guest keeps running: `mem=`, `cpus=`, `initrd=`, `disk=`, `net`, `restart`, `xapic`, `ioapic`, `cmdline=` |
| `hv start <name>` | boot a VM kept in a folder, as its `vm.toml` says |
| `hv create <name> ...`, `hv vms` | write a VM's folder from `hv start`'s words, or from a cloud image's URL; list the folders |
| `hv convert <image> <disk>` | make a qcow2 image a raw disk |
| `hv list`, `hv console`, `hv attach` | list the guests; show one guest's console, past or live |
| `hv send`, `hv exec`, `hv wait` | type at a guest, run a command line, wait for some text |
| `hv restart`, `hv stop` | reboot a guest; press its power button, then stop it |
| `hv forward`, `hv cores` | relay a port to a guest; show whether a guest has its cores to itself |

The full syntax is in [Shell commands](/docs/shell-commands), and the gates are in [Run the gates](/howto/run-the-gates).

## What comes next

All four of stage 3's demos are done: a guest of a few bytes, long mode under nested paging, a `bzImage`'s early console, and a full boot to a shell. Disks, networking and SMP guests, which the plan had left for stage 4, came early. Here is what is next, from [What comes next](/docs/hypervisor#what-comes-next):

- **More MMIO devices**: the HPET and PCIe's ECAM, which the guests' ACPI tables leave out today.
- **Speed**, building on [posted interrupts, a one-shot timer, offloads and mapped rings](/blog/faster-guest-networking). Device work should move off the first CPU's task for guests with MSI-X and a CPU to spare. (For a guest with one CPU, a NIC task of its own measured no faster.) The disk path should get out from under the VFS's single lock, and requests should take fewer exits. The CPU should deliver xAPIC guests' interrupts itself, and AMD's AVIC should be used. Guests need XSAVE: without it there is no AVX, and an x86-64-v3 distribution does not start. Devices should move to modern virtio.
- **Cloud images**: checking an image against its distribution's `SHA256SUMS`, resuming a download that broke off, and running `hv create` on the servers, where PVH has not yet run under VT-x.
- **Stage 4, the control plane** ([plan](https://github.com/irqlevel/nos/blob/master/plans/04-control-plane.md)): an HTTP API on nos's own TCP stack to create, list and destroy VMs and to stream a console. A VM's definition already outlasts a reboot as its folder's `vm.toml`. The API is still to come, and so are quotas.
- **Stage 5, live update** ([plan](https://github.com/irqlevel/nos/blob/master/plans/05-live-update.md)): a new host kernel under running guests, with their RAM left in place and under 100 ms of downtime. It needs all VM state to be plain serializable data, and no device model may hold pointers into kernel memory. That is not fully true yet. The first milestone needs no kexec: `rmmod hv`, then `insmod hv`, with the guests still running.
- **arm64**: the EL2 backend, which needs a change to the boot path rather than to the module.
