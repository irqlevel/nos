---
title: "The hypervisor"
description: "A type-1 hypervisor in Rust that loads into a running nos as a module and boots unmodified Linux, Alpine and Debian as they ship, on AMD-V and Intel VT-x."
---

nos has a hypervisor of its own: two Rust crates — `hvarch`, the CPU's virtualization extension, and `hv`, everything above it — in a loadable module, `hv.ko`, that goes into a running kernel with `insmod` and comes back out with `rmmod`. Loaded, it turns AMD-V or Intel VT-x on and runs guests: tiny built-in ones that each check one mechanism, an unmodified Linux `bzImage`, and Alpine's ISO and Debian's cloud image as they ship — with several CPUs, virtio disks, a network with NAT, ACPI and a power button. On bare metal there is no Linux underneath; nos owns the machine.

It is stage 3 of the [roadmap](https://github.com/irqlevel/nos/blob/master/plans/README.md), whose end is a bare-metal cloud node with no Linux host, managed over HTTP and updated live under its guests. The goal is other people's Linux guests on the machine, so isolation comes before speed. The full reference is [The hypervisor](/docs/hypervisor), some 3,600 lines; this is the five-minute version.

## Why a module

Three reasons ([Why a module](/docs/hypervisor#why-a-module)):

- **A machine that runs no guests carries none of it.** `hv` and `hvarch` are not default workspace members, so a kernel built without the module has not one symbol of a hypervisor — and none of its CPU state: `EFER.SVME` and `MSR_VM_HSAVE_PA` on AMD, `CR4.VMXE` and VMX root operation on Intel. Loaded, the module turns them on; unloaded, off.
- **Rebooting is the worst way to debug VMCB and VMCS setup.** `insmod`, try, `rmmod`, change a field, `insmod` again is a loop of seconds.
- **Unloading is a gate.** [`hv-test.py`](/docs/testing#hv-testpy----the-extension-turned-on-and-off-again-and-guests-under-it) turns the extension on for every CPU, unloads *without* turning it off, loads again and asks every CPU — by an IPI that reads the register — what is still on. An `rmmod` that forgot a CPU would leave it saving host state into a freed page. Stage 5's live update needs this check; here it exists years early.

The price is stated too: a module cannot register a block device or a NIC ([Loadable modules](/docs/modules)), so the module emulates a guest's disks and NICs over the kernel's own (`kcore::block::Disk`, `kcore::net::Nic`) — which is what a hypervisor wants anyway.

## Where the unsafe lives

The line between code that touches the extension and code that does not is drawn once, in the crate graph, where a script can count it ([The crates](/docs/hypervisor#the-crates)): `hvarch` has the instructions, the VMCB and VMCS, and the world switch; `hv` has guest memory, the policy, the devices, the Linux loader and the run loop. As [`scripts/unsafe-count.py`](https://github.com/irqlevel/nos/blob/master/scripts/unsafe-count.py) counts them on master in October 2026:

| Crate | Lines | `unsafe` sites |
|---|---|---|
| `hvarch` — the extension | 6,339 | 195 |
| `hv` — everything above it | 17,647 | 4 |
| `modules/hv` — the module | 5,246 | 0 |

The four in `hv` are the two IPI handlers that turn the extension on and off and the two guest entries, one per backend. The entries are private to the crate and take the `&GuestMemory` whose table they enter over, so their preconditions are invariants of types:

- **Guest memory** is pages mapped nowhere but the guest's nested table, zeroed before the guest sees them. Nothing hands out a Rust reference into it — the guest may write it at any moment from any CPU, and a reference to memory changing under the compiler is undefined behaviour — so every access is a bounds-checked copy ([Its memory](/docs/hypervisor#its-memory)).
- **The nested page table lives inside it**, maps only pages that same value owns, and only grows. A guest reaching host memory it was not given is not a mistake a caller can make.

Keeping the CPU behind one crate is also what lets [`hv-fuzz.py`](/docs/testing#hv-fuzzpy----everything-a-guest-reaches-fuzzed) compile most of `hv` for the host, over a stand-in CPU, and fuzz it in CI ([Fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)).

## AMD-V first, Intel VT-x second

The order was set by where the code is iterated ([AMD-V first](/docs/hypervisor#amd-v-first-intel-vt-x-second)). Development happens on a Mac, where the x86-64 kernel runs under QEMU's TCG, and TCG emulates SVM, nested paging included, and no VMX at all. AMD-V is also the simpler extension: the VMCB is a plain structure in memory, while VMX needs `vmread`/`vmwrite` for every field and reports a refused entry through one error number that means a dozen things. Use `-cpu max` — QEMU's default `qemu64` has SVM without nested paging. TCG has corners of its own, two of them told in [When the emulator is the bug](/blog/qemu-tcg-amd-v-quirks).

Intel VT-x is for the machines nos actually runs on — the Hetzner EX44 and the Dell laptop are Intel — and was brought up under nested KVM on an i5-13500. Above `hvarch` the backends are one shape: `hv::vm::Backend` is `Svm` or `Vmx`, both keeping guest state in AMD's save-area layout — for VMX a shadow, synced into the VMCS lazily — so the policy and the loader are written once ([The VMX backend](/docs/hypervisor#the-vmx-backend-and-where-it-differs-from-amd-v)). The last obstacle to `vmxon` was outside the hypervisor: VMX requires `CR0.NE`, which nos never set, so `hv on` refused — `hv info` naming the bit — until the boot path set it on every CPU.

## What it runs

- **Built-in guests.** Eighteen, each a handful of instructions kept as bytes beside its source in [`guests.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/guests.rs) and checked against what it was told to do: memory above 4 GiB and every register across a hypercall, a write stopped by the nested table, a triple fault that stops the guest and not the CPU, `cli; jmp $` stopped by the host anyway, a second CPU started by INIT and a start-up IPI ([The built-in guests](/docs/hypervisor#the-built-in-guests)).
- **An unmodified Linux.** `hv boot` loads a 64-bit `bzImage` by the boot protocol over an emulated 8250, 8254 PIT, MC146818 RTC and two 8259s; CPUID and MSRs are allow-lists ([A Linux guest](/docs/hypervisor#a-linux-guest); the story is in [Linux under the nos hypervisor](/blog/linux-under-nos-hypervisor)).
- **Distributions as they ship.** Alpine's `virt` ISO, the ISO itself the guest's read-only disk, to a root login; Debian 13's cloud image under systemd, `running` with no failed unit ([A distribution](/docs/hypervisor#a-distribution)).
- **Several CPUs.** `cpus=N`, up to 16, each with a local APIC — x2APIC unless told `xapic` — all but the first started like a PC's, by INIT and a start-up IPI into real mode; virtio interrupts come by MSI-X, and an IO-APIC on request ([More than one CPU](/docs/hypervisor#more-than-one-cpu)).
- **Disks and a network.** `disk=` is a legacy virtio-blk over a file of nos's, whose rings the host trusts none of. `net` is a virtio-net port on the module's switch: nos at 10.0.100.1 behind its virtual NIC `hv0`, NAT out, DHCP answered by the switch, and `hv forward` for the way in ([A disk](/docs/hypervisor#a-disk), [A network](/docs/hypervisor#a-network)).
- **ACPI and MMIO.** Every guest gets ACPI tables and the fixed hardware they describe, so `hv stop` presses a power button that systemd-logind or BusyBox's `acpid` answers ([ACPI](/docs/hypervisor#acpi)). Memory-mapped devices — the xAPIC page and the IO-APIC so far — work by fetching the faulting instruction through the guest's page tables and decoding it, with no `unsafe` ([MMIO](/docs/hypervisor#mmio)).

## A session

The guest lifecycle as shell commands, which stage 4 will put behind HTTP — the gate's run under TCG, where the guest reaches its shell a second after it starts ([Guests that stay up](/docs/hypervisor#guests-that-stay-up)):

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

`hv exec` waits for a prompt printed *after* its line went in, so a line typed at a guest still booting comes back with its answer; `hv attach` over `ssh -t` is the live console. The [Linux guest how-to](/howto/linux-guest) goes through it step by step.

## Keeping guests apart

What keeps a guest from the host and from other guests is set below the policy, on every entry, whatever the policy asked ([What entering a guest cannot be told to do](/docs/hypervisor#what-entering-a-guest-cannot-be-told-to-do)):

- **Intercepts the host depends on**: its interrupts and NMIs, every port, every MSR but the guest's three segment bases, the virtualization instructions, #DB and #AC (whose delivery a guest could make loop inside the CPU — CVE-2015-8104, CVE-2015-5307), and #MC. CR4.MCE stays set, so a machine check during a guest reaches the host instead of passing for the guest's triple fault.
- **The TLB**: per-CPU ASID generations under AMD-V; under VT-x a VPID for the guest's life and an INVEPT on its first entry on each CPU ([Address space identifiers](/docs/hypervisor#address-space-identifiers)).
- **Speculation at the world switch**: registers cleared after every exit, the return stack refilled, an IBPB between guests, plus automatic IBRS on AMD, and IBRS with BHI_DIS_S, VERW and the L1D flush on Intel, where the CPU needs them ([Guest speculation isolation](/blog/guest-speculation-isolation)).
- **Registers a guest believes are its own**: DR0–DR3 kept per guest; CR4 read back as written while the CPU's keeps MCE.
- **Cores**: by default whole cores no other guest has a CPU on — two threads of a core share what MDS and L1TF exploit.
- **Network and host services**: a frame must carry its port's MAC and address; a guest reaches neither nos's sshd nor its connections; NAT keeps guests off the cloud's metadata address, the private networks and the uplink's subnet unless `nat allow` says otherwise; shared resources have per-guest quotas.
- **Memory**: a reserve no guest can take, a sixteenth of RAM and at least 64 MiB.

Each fix came with a gate — a built-in guest, a check in `hv-linux-test.py`, a model in `hv-fuzz.py` — most shown to fail against the code they replaced. What is still open is listed too, SRSO and Retbleed on Zen 1 to 4 among it ([Before other people's guests](/docs/hypervisor#before-other-peoples-guests)).

## What an exit costs

`hv bench` runs a guest that does nothing but exit, on one CPU, and times it ([What an exit costs](/docs/hypervisor#what-an-exit-costs)):

| Where | Extension | One exit |
|---|---|---|
| QEMU's TCG on the Mac | AMD-V, emulated | 17,130 ns — says nothing about a real CPU |
| nested KVM on an i5-13500 | VT-x | 17,900 ns, then 5,800 ns after the entry path was thinned; 4.6 µs of it is the outer kernel's |
| AX41, Ryzen 5 3600 (Zen 2), bare metal | AMD-V | 731 ns with ASIDs kept, 940 ns flushing the TLB on every entry |
| EX44, i5-13500, bare metal, base clock | VT-x | 1.69 µs on a P-core, 2.07 µs on an E-core; 1.82 and 2.49 µs with speculation mitigations |
| EX44, cores at turbo | VT-x | 1,018 ns on a P-core, 1,512 ns on an E-core |

An MMIO access costs more, since the host walks to, fetches and decodes the instruction: on an EX44 P-core a CPUID exit is 1.8 µs and an MMIO load 4.5. An idle guest costs almost nothing: sixteen of them on the AX41's twelve CPUs keep the machine 1.6% busy.

## Where it has run

- **QEMU's TCG** and **nested KVM** on AMD and Intel hosts, where the gates run.
- **The AX41** (Ryzen 5 3600, Zen 2), bare metal: Linux reaching `Run /init` 18 ms into its clock, against half a second under TCG; Debian at its login prompt in under four seconds; a gigabyte fetched through NAT at 117 MB/s; a guest's disk written at 1.2 GB/s. Its first run stopped twice where TCG never had — CPUID leaf 0x80000001 passed MWAITX through, and Linux on a Zen reads AMD's FCH at a fixed address whether or not one exists ([On real hardware](/docs/hypervisor#on-real-hardware)).
- **The EX44** (i5-13500, six P-cores and eight E-cores), bare metal, VT-x, since 2026-10-20: the built-in guests on P- and E-cores, Linux with one, two and four CPUs, a virtio disk at 1.0–1.2 GB/s with `fsync`, Alpine at its login prompt in 13 s, and `rmmod` with five VMs running leaving VMX off on all twenty CPUs ([The EX44, under VT-x](/docs/hypervisor#the-ex44-under-vt-x)).
- **Not yet**: the Dell's Skylake; and arm64, where a hypervisor is an exception level a kernel runs at, not an extension it turns on — nos drops from EL2 to EL1 at boot, so that backend only reports what EL2 would offer ([arm64](/docs/hypervisor#arm64)).

## The commands

| Command | What it does |
|---|---|
| `hv`, `hv info` | the extension, and which CPUs it is on for, asked of the CPUs themselves |
| `hv on`, `hv off` | turn it on or off, for one CPU or all |
| `hv run <guest\|all> [cpu]` | run built-in guests |
| `hv bench` | time an exit or an MMIO access |
| `hv boot <bzImage> ...` | a Linux guest for a set time, then its console |
| `hv start <bzImage> ...` | the same, left running: `mem=`, `cpus=`, `initrd=`, `disk=`, `net`, `restart`, `xapic`, `ioapic`, `cmdline=` |
| `hv list`, `hv console`, `hv attach` | the guests; one's console, past or live |
| `hv send`, `hv exec`, `hv wait` | type at a guest, run a line, wait for text |
| `hv restart`, `hv stop` | reboot a guest; press its power button, then stop it |
| `hv forward`, `hv cores` | relay a port to a guest; whether a guest's cores are its own |

The full syntax is in [Shell commands](/docs/shell-commands); the gates are in [Run the gates](/howto/run-the-gates).

## What comes next

Stage 3's four demos are done — a guest of a few bytes, long mode under nested paging, a `bzImage`'s early console, a full boot to a shell — and disks, networking and SMP guests, which the plan left for stage 4, came early. Next, from [What comes next](/docs/hypervisor#what-comes-next):

- **More MMIO devices**: the HPET and PCIe's ECAM, which the guests' ACPI tables leave out today.
- **Speed**: device work off the first CPU's task; the disk path off the VFS's single lock; fewer exits per request; posted interrupts; XSAVE for guests, without which there is no AVX and an x86-64-v3 distribution does not start; modern virtio.
- **Stage 4, the control plane** ([plan](https://github.com/irqlevel/nos/blob/master/plans/04-control-plane.md)): an HTTP API on nos's own TCP stack — create, list, destroy, stream a console — with typed VM specs, definitions kept across reboots, and quotas.
- **Stage 5, live update** ([plan](https://github.com/irqlevel/nos/blob/master/plans/05-live-update.md)): a new host kernel under running guests, their RAM left in place, under 100 ms of downtime. It needs all VM state to be plain serializable data and no device model to hold pointers into kernel memory — which does not fully hold yet. The first milestone needs no kexec: `rmmod hv`, then `insmod hv`, with the guests still running.
- **arm64**: the EL2 backend, a change to the boot path rather than to the module.
