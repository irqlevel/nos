# The hypervisor

A type-1 hypervisor for `nos`, written as Rust crates and loaded into a
running kernel as a module:

```
$ insmod /hv.ko
module: hv loaded at 0xFFFF800031000000
$ hv info
hv: AMD-V (SVM) -- ready
  cpu                  AuthenticAMD, itself under a hypervisor
  revision             1, 16 ASIDs
  nested paging        yes  -- guest physical addresses translated by the CPU
  next-RIP save        no   -- where an intercepted instruction ends
  decode assists       no
  flush by ASID        no
  VMCB clean bits      no
  virtual VMSAVE       no
  virtual GIF          yes
  AVIC                 no
  pause filter         no
  SVM lock             no
  x2APIC               yes  -- the guest's local APIC is MSRs, not a page to decode
  1 GiB pages          yes  -- guest memory in one nested entry a gigabyte
hv: AMD-V (SVM) on for cpu none of 0-3
$ hv on
hv: turned on for cpu 0-3
hv: AMD-V (SVM) on for cpu 0-3 of 0-3
$ rmmod hv
module: hv unloaded
```

This is [stage 3 of the roadmap](../plans/03-hypervisor.md), and what is
here today is the first of its four steps: the machine's virtualization
extension, found, reported, and turned on and off for the CPUs that will run
guests. The VM, its nested page tables, its emulated devices and the exit
dispatcher come next.

## Why a module

Nothing about a hypervisor has to be in the kernel image, and three things
argue for keeping it out.

**A machine that runs no guests should carry none of it.** Not the code, and
above all not the CPU state: `EFER.SVME` and `MSR_VM_HSAVE_PA` on AMD,
`CR4.VMXE` and VMX root operation on Intel are the CPU's, not a task's, and
a kernel that never runs a guest has no business touching them. Loaded, the
module turns them on; unloaded, it turns them off, and the machine is in the
state it booted in.

**It is the subsystem least pleasant to debug by rebooting.** `insmod`, try
it, `rmmod`, change one field, `insmod` again is a loop of seconds;
VMCB and VMCS setup is where most of the time in this stage goes
(`plans/03-hypervisor.md` says so), and a rebuild-and-boot loop would spend
that time twice over.

**Unloading is a gate of its own.** A hypervisor that can be taken out has
to prove, every time, that it turned the extension off before its code was
freed -- and the check for that is the same check live update (stage 5) will
need, years earlier than it would otherwise be written. `scripts/hv-test.py`
loads, turns it on for every CPU, unloads *without* turning it off, and
loads again to ask every CPU what it has. If the first unload left anything
on, the second load says so.

What it costs is stated in [Loadable modules](modules.md): a module cannot
register a block device or a NIC, because those register with their layer as
Rust trait objects from inside the image and there is no C name for a module
to bind. That is a limit on how a guest's virtual devices will be *served*,
not on the hypervisor: a guest's disks and NICs are emulated by this module
over the kernel's own (`kcore::block::Disk`, `kcore::net::Nic`), which is
what a hypervisor wants anyway.

## The crates

```
hvarch/              the CPU's extension: the instructions, the control MSRs,
                     the structures the hardware reads. All the unsafe.
hv/                  everything else: the machine, and -- as this grows --
                     the VM, its memory, its devices, the exit dispatcher.
modules/hv/          the module: the shell command, and the lifetime of it all.
```

`hv` and `hvarch` are workspace members but **not** default members: a plain
`cargo build`, and so the kernel's own build, does not compile them. They
exist only inside `hv.ko`. A kernel built without the module has no
hypervisor in it at all -- not a symbol.

The split is the point, and it is measurable:

```
$ scripts/unsafe-count.py hv hvarch
hv          332 lines     2 blocks    0 unsafe fn    0 unsafe impl  = 2
hvarch     1137 lines    37 blocks   11 unsafe fn    0 unsafe impl  = 48
```

The long-term goal of this kernel is other people's Linux guests on this
machine, and the bug class that goal cannot survive is a guest reaching host
memory. So the line is drawn once, in the crate graph, where a script can
count it rather than a reviewer having to remember it. The two sites in `hv`
are the two calls into `hvarch` from the IPI handlers that turn the
extension on and off; every other line of the VM will be ordinary safe Rust
above them.

## AMD-V first, Intel VT-x second

The order is decided by where the code is iterated on, not by the hardware.

QEMU's TCG emulates **SVM**, nested paging included, and emulates **no VMX at
all**. On the development machine -- a Mac, where the x86 kernel runs under
TCG -- AMD-V is the only extension under which a guest can be brought up at
all. It is also the simpler of the two: the VMCB is a plain structure in
memory, where VMX needs `vmread`/`vmwrite` for every field, and a VM-entry
failure there reports itself through one error number that means a dozen
things.

What QEMU gives, and what it does not, is worth knowing before debugging
against it:

```
$ qemu-system-x86_64 -cpu max ...
svm yes   npt yes   vgif yes   x2apic yes
nrip-save no   decodeassists no   flushbyasid no   v-vmsave-vmload no   vmx no
```

Two of those absences matter. Without `nrip-save` the hypervisor has to
work out for itself where an intercepted instruction ended -- which is a
short table of lengths (`cpuid` is 2 bytes, `vmmcall` 3, `rdmsr`/`wrmsr` 2),
not an instruction decoder, because every instruction that is intercepted on
purpose is one whose length is known; and an `IOIO` intercept hands over the
next instruction's address in the VMCB regardless. Without `decodeassists`
the same holds for the rest. Both are present on real AMD parts.

**Use `-cpu max`.** The default `qemu64` model reports SVM *without* nested
paging, and a hypervisor that will not walk the guest's page tables itself
-- this one, deliberately -- has no use for that.

Intel VT-x is written for the hardware this kernel actually runs on: the
Hetzner EX44 and the Dell laptop are both Intel, where it is native and
fast. Today `hvarch::x86::vmx` probes and enters root operation; the VMCS
and the guest come with the rest of the VM.

**One bit stands between this kernel and `vmxon` on those machines, and it
is not in the hypervisor.** VMX operation requires `CR0.NE` (bit 5; it is in
`IA32_VMX_CR0_FIXED0` on every Intel part), and `nos` never sets it. The APs
come out of INIT with `CR0 = 0x60000010`, NE clear, and the boot path adds
PE and PG (`boot64.asm`) and WP (`EnableWxSupport`) and nothing else; the BSP
has whatever GRUB and the firmware left, which Multiboot2 calls undefined.
`vmxon` does not fail on that -- it faults, and this kernel panics on a #GP
-- so `vmx::enable` checks the fixed bits on each CPU before it, the error
is `HostState`, `hv on` names the CPU that refused, and `hv info` names the
bit:

```
  host CR0/CR4         no   -- on this CPU vmxon would fault: CR0 needs NE
```

The check is per CPU and not part of the machine-wide verdict, because the
registers are per CPU: a verdict that changed with the CPU `insmod` happened
to run on would be no verdict. The fix is one bit on every CPU, beside where
`EnableWxSupport` sets WP -- what every other x86-64 kernel does. NE chooses
native `#MF` over the PC's FERR#/IRQ 13 for x87 errors, and this kernel has
no x87 code to raise one: the C++ is built with `-mno-80387`, so there is no
x87 instruction in the image and none can appear, and the Rust targets are
soft-float. It is
still a change to how every x86 machine boots, the two whose only console is
the network among them, so it belongs to the change that brings up the VMX
backend, where a guest can show it working -- not slipped in here.

## What it does today

    hv                 what the machine has, and which CPUs it is on for
    hv info            the whole of what the CPU said, a line a feature
    hv on [cpu|all]    turn the extension on
    hv off [cpu|all]   turn it off

**Turning it on** allocates one page per CPU -- AMD's host state save area,
Intel's VMXON region -- and then sends that CPU an IPI that does nothing but
write MSRs. The page is allocated before the IPI and never inside it: a page
allocation shoots down every other CPU's TLB and waits for every CPU to
answer, and a CPU inside an interrupt handler has interrupts off and cannot
(the spinlock rule in [`CLAUDE.md`](../CLAUDE.md)).

**The status line asks the CPUs, not the module.** `hv` sends each CPU an IPI
that reads `EFER` or `CR4` and reports what came back, and says so loudly if
that disagrees with what the module thinks it turned on. Nothing else in
this kernel touches those bits, so a disagreement can only be a bug here --
and it is exactly the kind that is invisible to code that reads only its own
bookkeeping.

**Turning it off checks first, on the CPU itself.** `vmxoff` on a CPU that is
not in root operation is an undefined-opcode fault, and this kernel's
handler panics; so the IPI handler reads the register and acts on what it
finds. That also makes `hv off` the way back from a CPU some earlier load
left the extension on for -- the page it was using is gone, but turning it
off needs no page.

## What the extension costs while it is on

- **On Intel, a CPU in VMX root operation ignores INIT.** It cannot be
  brought back up through the INIT/SIPI sequence `CpuTable::StartAll` starts
  APs with until `vmxoff`. Nothing in `nos` re-INITs a running CPU today;
  kexec-style live update (stage 5) will have to.
- **On AMD, `EFER.SVME` only enables the instructions**, and costs nothing
  else. `MSR_VM_HSAVE_PA` points at a page that must stay allocated for as
  long as it does -- which is what `CpuPage` is, and why the table that owns
  the pages and the mask of enabled CPUs are set and cleared together under
  one lock.
- Either way the page is freed only after the CPU it belonged to has said it
  is done with it.
- **One Intel write is not given back.** If firmware left
  `IA32_FEATURE_CONTROL` unlocked, the first `hv on` locks it with VMXON
  allowed -- the CPU refuses VMXON otherwise -- and the lock holds until the
  next reset, whatever `rmmod` does. It is how every other OS leaves the MSR
  and how almost every firmware hands it over already, and it is done only
  after every check that could refuse, so a refusal leaves no trace; but it
  is the one exception to "unloaded, the machine is as it booted".
- **On AMD, GIF is set before SVM goes off.** With the global interrupt flag
  clear, INIT and NMI stay blocked, and `stgi` is an undefined opcode once
  `EFER.SVME` is clear -- so `svm::disable` sets it first, as KVM does. There
  is no `clgi`/`vmrun` loop yet to clear it; the off switch is written to be
  right whatever that loop does.

## arm64

On Arm a hypervisor is not an extension a kernel turns on -- it is an
exception level a kernel runs *at*. `nos` is handed the machine at EL2 when
firmware boots it there and drops straight to EL1 in its first hundred
instructions (`arch/arm64/boot.S`), and a module cannot climb back: EL2 is
entered by taking an exception to it, and a kernel already at EL1 has no way
to ask.

So the arm64 backend reports and does not run:

```
$ hv info
hv: Arm virtualization (EL2) -- not implemented on this architecture
  running at           EL1
                       -- boot.S drops EL2 to EL1; a module cannot climb back
  VMIDs                8 bits
  VHE                  no
  stage-2 output       40 bits
```

The EL2 hypervisor is a change to the boot path -- stay at EL2, run the
kernel there with the virtualization host extensions, install stage-2
translation -- and that is where it will be written. Everything above
`hvarch` is unchanged by it, which is the leverage the
[roadmap's reordering](../plans/README.md#reordering-history) bought by
doing the HAL and arm64 before the hypervisor rather than after.

## Running it

The module is built with the kernel (`make modules`, or any `make`), and
goes on a root filesystem the way any module does:

```sh
make nocheck                                 # out/x86_64/modules/hv.ko
scripts/mkrootfs.sh root.img 64 <dir> 1024   # with hv.ko in <dir>
qemu-system-x86_64 -cpu max -smp 4 -m 1G -cdrom nos.iso \
    -drive file=root.img,format=raw,id=d0,if=none \
    -device virtio-blk-pci,drive=d0,disable-legacy=on,disable-modern=off
```

Under KVM on a Linux host, `-enable-kvm -cpu host` gives the guest the host's
own extension (nested virtualization: `kvm_intel nested=1` or
`kvm_amd nested=1`), which is far faster than TCG and is how the VMX path
will be exercised before it reaches real hardware.

The gate is `scripts/hv-test.py` ([Tests and gates](testing.md)):

```sh
scripts/hv-test.py                  # x86-64: the extension, on and off and off again
scripts/hv-test.py --arch aarch64   # arm64: that it reports and refuses
```

## What comes next

From [`plans/03-hypervisor.md`](../plans/03-hypervisor.md), in order, each a
thing that can be shown in half a minute:

1. a VM object with nested page tables, and a guest of a few bytes that
   exits where it was told to;
2. a guest in long mode under NPT/EPT;
3. an emulated 8250 on port I/O exits, and a `bzImage` printing its early
   console;
4. a full boot to a shell over that UART, with an initramfs, on one vCPU.

Two constraints from stage 5 (live update) hold from the first line of it:
all VM state is serializable plain data -- the vCPU register set, every
emulated device, and one table mapping guest physical pages to the VM that
owns them -- and no emulated device holds a pointer into arbitrary kernel
memory, only indices and handles that survive a re-init.
