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
  x2APIC               yes  -- the host's; a guest's is MSR exits either way
  1 GiB pages          yes  -- guest memory in one nested entry a gigabyte
hv: AMD-V (SVM) on for cpu none of 0-3
$ hv on
hv: turned on for cpu 0-3
hv: AMD-V (SVM) on for cpu 0-3 of 0-3
$ hv run hypercall 2
hv: guest hypercall -- memory above 4 GiB through its own page table, then every register across a hypercall
  ran on     cpu 2, 634 us
  said       "nos: long mode"
  exits      0 port in, 14 port out, 0 cpuid, 1 hypercall, 0 host interrupt
  stopped    hlt at 0x812b
  checked    guest physical 4 GiB holds what it wrote; 15 registers went out at the hypercall and 15 answers came back
hv: guest hypercall ok
$ rmmod hv
module: hv unloaded
```

This is [stage 3 of the roadmap](../plans/03-hypervisor.md), and what is
here today is the first two of its four steps: the machine's virtualization
extension, found, reported, and turned on and off for the CPUs that will run
guests; and a VM -- guest memory behind a nested page table, one CPU under
AMD-V, its exits decoded -- that runs guests of a few bytes in long mode and
checks that each did what it was told. The emulated UART and the Linux
loader come next.

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
                     the structures the hardware reads -- the VMCB, laid out
                     in x86/svm/vmcb.rs -- and entering a guest. All the
                     unsafe.
hv/                  everything else: the machine (machine.rs), guest memory
                     and its nested page table (memory.rs, npt.rs), the
                     VMCB as this hypervisor fills it in and the exits it
                     comes back with (svm.rs), the VM (vm.rs), and the
                     built-in guests (guests.rs).
modules/hv/          the module: the shell command, the task a guest's CPU
                     runs on, and the lifetime of it all.
```

`hv` and `hvarch` are workspace members but **not** default members: a plain
`cargo build`, and so the kernel's own build, does not compile them. They
exist only inside `hv.ko`. A kernel built without the module has no
hypervisor in it at all -- not a symbol.

The split is the point, and it is measurable:

```
$ scripts/unsafe-count.py hv hvarch
hv         1917 lines     3 blocks    0 unsafe fn    0 unsafe impl  = 3
hvarch     1991 lines    40 blocks   13 unsafe fn    1 unsafe impl  = 54
```

The long-term goal of this kernel is other people's Linux guests on this
machine, and the bug class that goal cannot survive is a guest reaching host
memory. So the line is drawn once, in the crate graph, where a script can
count it rather than a reviewer having to remember it. The three sites in
`hv` are the two calls into `hvarch` from the IPI handlers that turn the
extension on and off, and the one call that enters a guest (`Vm::enter`),
whose two preconditions are invariants of types in `hv` rather than
promises at the call: guest memory owns every page its nested table maps,
and the machine never names a host save area that has been freed. Guest
memory, the nested page table, the VMCB's policy, the exit decoder and the
guests above them -- 1450 lines -- have none.

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
svm yes   npt yes   vgif yes   x2apic yes (10.2; 8.2 has none)
nrip-save no   decodeassists no   flushbyasid no   v-vmsave-vmload no   vmx no
```

**Before QEMU 9.2, TCG does not put a guest with paging off through the
nested page table.** `get_physical_address` (`target/i386/tcg/.../excp_helper.c`)
translates through stage 2 only from inside a guest page-table walk; with
`CR0.PG` clear it takes the guest physical address for a host physical one,
and the guest runs out of the host's memory. 9.2 fixed it
(`env->cr[0] & CR0_PG_MASK || use_stage2`). It was found here: the first
real-mode guests all stopped at the same `#UD` with the same registers
whatever their code was, and the `fault` guest's write past its memory did
not fault. Real hardware has no such hole, but the development host's QEMU
is 8.2 and the build container's 6.2, so every built-in guest starts in long
mode with paging on -- which is how a 64-bit Linux kernel is started anyway.

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

    hv                          what the machine has, and which CPUs it is on for
    hv info                     the whole of what the CPU said, a line a feature
    hv on [cpu|all]             turn the extension on
    hv off [cpu|all]            turn it off
    hv run <guest|all> [cpu]    run a built-in guest, or all of them, on a
                                task of its own -- bound to cpu when one is named
    hv boot <bzImage> [mem=MiB] [secs=N] [initrd=path] [cmdline=...]
                                load a Linux bzImage and run it on a vCPU, its
                                early console coming back as the command's output

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

## A guest

A VM is guest memory behind a nested page table, the permission maps it runs
under, and one CPU (`hv::vm::Vm`). A guest's CPU is a task: `hv run` spawns
one, bound to a CPU when asked, and waits for it, and the task enters the
guest over and over from task context, handling each exit between entries.

### Its memory

`hv::GuestMemory` is regions of guest physical addresses, each backed by
host pages -- runs of 512 KiB, the most the page allocator hands out
contiguously, since the nested table translates page by page and needs no
more -- zeroed before the guest can see them, so that what a guest finds in
its memory is what it was given and never what the host left there.

Two rules make it the one place that decides what a guest can reach:

- **Nothing hands out a reference into it.** The guest writes its memory
  whenever it runs, on whatever CPU, and a Rust reference to memory that
  changes under the compiler is undefined behaviour however it is used
  ([`plans/01-rust-strategy.md`](../plans/01-rust-strategy.md) calls this
  the most important decision of the hypervisor). Every access is a copy --
  `read`, `write`, `read_obj::<T: Pod>`, `write_obj` -- made with volatile
  loads and stores through `DmaBuffer::load`/`store`, bounds-checked against
  the region it falls in; an address a guest gave, whatever it is, is at
  worst `Unmapped`.
- **The nested page table is inside it.** A guest reaches exactly what its
  nested table maps, and this table maps nothing but pages the same value
  owns: every page is owned before it is mapped, and freed only with the
  table. "The guest can reach host memory it was not given" is then not a
  mistake a caller can make, and the one `unsafe` call that enters a guest
  has that as a type's invariant rather than a comment's promise.

### The nested page table

AMD's nested table has the host's own long-mode format, four levels of 512
entries (`hv/src/npt.rs`), with one difference that bites: a nested walk is
a user-mode access at every level, so every entry has U/S set or the guest
faults on memory it was given. It is built from the host's side only and
walked by software only through its own indices -- an arena of tables, each
knowing the index of the table under each entry -- so an entry the CPU reads
is written here and never read back to find the next level. Accessed and
dirty are set from the start, so the CPU has nothing to write into it. The
tables a region needs are all made before any of its pages is mapped, so
mapping cannot fail halfway for want of memory, and a page already mapped is
refused rather than moved.

### What entering a guest cannot be told to do

The VMCB is laid out in `hvarch/src/x86/svm/vmcb.rs` field by field from
appendix B of the AMD manual, and every field anything touches has its
offset checked against the manual at compile time.

What goes in it is policy, and lives in `hv::svm`: which instructions stop
the guest, what state it starts in. None of that can hand the guest the
host, because `hvarch::x86::svm::Guest::run` sets the part the host depends
on on every entry, whatever the VMCB says:

- intercepts of INTR, NMI, SMI and INIT (the host's interrupts end the
  guest's turn and are taken by the host), SHUTDOWN (a guest's triple fault
  would otherwise shut the CPU down), IOIO and MSR (without them every port
  and MSR is the guest's, whatever the maps say), INVD (discards the host's
  dirty cache lines), INVLPGA, VMRUN, VMLOAD and VMSAVE (which take host
  physical addresses), STGI and CLGI (the physical GIF), SKINIT, XSETBV
  (XCR0 is the CPU's and `vmrun` does not switch it);
- intercepts of #DB and #AC, because a guest can make delivering either
  raise it again -- a data breakpoint on the stack its own #DB frame goes
  to -- and the CPU then loops inside the delivery, where no instruction
  ever ends and no interrupt, NMI included, is ever taken: the host has lost
  the CPU (CVE-2015-8104 and CVE-2015-5307, which KVM closes the same way).
  Giving them back to a guest that expects them is the policy's to do;
- the intercept of #MC -- and since an intercepted machine check is not
  delivered to the host by the CPU, `run` raises vector 18 itself before
  interrupts come back on, into the kernel's own handler, which panics;
- physical interrupts masked by the host's flag, not the guest's
  (`V_INTR_MASKING`), so a guest sitting with interrupts off cannot keep the
  host's out;
- nested paging on, over the table the caller's memory owns; SEV, AVIC and
  virtual VMSAVE off, since each has the CPU work at physical addresses the
  VMCB gives it;
- I/O and MSR permission maps with every bit set, and no way yet to clear
  one: a port the guest reaches directly is one of the host's devices;
- nothing assumed clean, and **the whole TLB flushed on every entry**.
  Guests share one address space identifier until there is an allocator
  that knows when one may be reused, and a translation left over from
  another guest -- or from this one, to a page since freed and handed back
  to the host -- would be the host's memory in a guest's hands.

### Entering and leaving

`Guest::run` turns interrupts off, checks on the CPU it is on that SVM is
on and that `VM_HSAVE_PA` is the page this hypervisor gave that CPU -- not
one an earlier load left behind -- and only then enters, so that what was
checked is still so: turning the extension off is an IPI, which waits. The
machine keeps each CPU's page address in an atomic beside its table for
exactly this check, set once the CPU has taken the page and cleared before
the page is freed.

It also refuses a CPU with `CR4.LA57` set. A nested table is walked in the
host's own paging mode, and the one here has four levels: walked as five,
its top level would be taken for a fifth and the guest's own memory for the
last, and the guest would choose its own host physical addresses. Nothing
in this kernel turns five-level paging on; the check is where that would be
found out, rather than by a guest.

The entry is a naked function, `vmrun_stub`, and GIF is clear for all of
it, so nothing -- no interrupt, no NMI -- runs until the host's state is back
whole:

```
clgi                      nothing gets in from here
vmsave  host page         FS, GS, TR, LDTR, the syscall MSRs: vmrun switches none of them
vmload  guest VMCB
load the guest's registers
sti; vmrun; cli           the host's IF, which vmrun saves, decides that an interrupt exits
store the guest's registers
vmsave  guest VMCB
vmload  host page
stgi                      with IF still clear: the interrupt that ended the turn waits
```

The `vmsave`/`vmload` of the host's state is not optional here: this
kernel keeps its per-CPU data at the GS base. Taken out on purpose, the
first thing the host does after a guest's exit is a page fault at address 0
in `Hal::GetCurrentCpuHwId()` -- the gate's `hv run` panics the kernel.

### The exits

`hv::svm::Vcpu::exit` decodes what `#vmexit` wrote -- port I/O with its
size and direction, HLT, CPUID, MSR, VMMCALL, a nested page fault with its
address and error code, an exception with its error code, a machine check,
a shutdown, and the host's own interrupts, after which the guest goes
straight back in. Where an instruction ends comes from the CPU when it
saves next RIP; otherwise from the length of the one instruction each
intercept is for (HLT 1, CPUID 2, RDMSR/WRMSR 2, VMMCALL 3), and for port
I/O from the address every CPU hands over. Stepping past an instruction
also steps out of the interrupt shadow it was in.

An exit can cut an event's delivery short -- the host's interrupt arriving
while the guest was taking one of its own, a nested fault on its IDT or
stack -- and the event is then in `exit_int_info`, lost unless the next
entry injects it. After every exit it is put back into `event_inj` (the two
fields share a format), except a software interrupt or INT3 or INTO, whose
instruction the guest's RIP still points at and which running again raises
again.

### When the CPU says no

A VMCB `vmrun` refuses comes back as `VMEXIT_INVALID` and nothing more: no
field, no rule. `hv::svm::Vcpu::check` is the manual's list of
consistency checks that a VMCB filled in here could fail -- EFER.SVME,
CR0.NW without CD, CR0 above bit 31, CR3 above bit 51, reserved CR4 and EFER
bits, DR6 and DR7 above bit 31, long mode without PAE or PE, a 64-bit code
segment with D set, G_PAT's memory types, the ASID, the injected event's
type and vector -- and one of this hypervisor's own that the CPU does not
make, LMA against LME and PG. Every entry asks it first: a VMCB it finds
wrong is never handed to the CPU, and the refusal names the rule. It is the
SVM side of the "VM-instruction-error decoder" the plan asks for before it
is needed. When `vmrun` refuses anyway, the report dumps the guest's state.

## The built-in guests

Each is a handful of instructions, assembled once with NASM and kept in
`hv/src/guests.rs` as bytes beside the source they came from, run in a VM of
its own and checked against what it was told to do. All start in long mode
at CPL 0 with paging on, 1 MiB of memory with a GDT, a TSS and a page table
in it that maps the first 2 MiB to themselves -- the second of them with no
memory behind it -- and 1 GiB to guest physical 4 GiB:

| Guest | Does | Shows |
|---|---|---|
| `exits` | reads and writes the debug port 0xE9, asks CPUID a leaf only this hypervisor answers, writes what it got to its memory | port I/O both ways; CPUID answered by the host; the answers read back out of guest memory |
| `hypercall` | writes through its own page table to guest physical 4 GiB, puts a value of its own in every register, makes a hypercall, writes every register the host answered to its memory | long mode under nested paging, above 4 GiB; the run stub's register save and restore, all 15 registers both ways |
| `fault` | writes to 0x1FF000, which its page table maps and the nested one does not | stopped at the nested table, at that address and that instruction: a guest reaches nothing it was not given |
| `triple` | `int3` with no IDT | a triple fault stops the guest, not the CPU |
| `refused` | starts with CR0.NW set and CD clear | a VMCB that breaks a rule is refused before the CPU sees it, the rule named |
| `spin` | `cli; jmp $` | the host's interrupts still get through -- about a hundred a second -- and the host stops it when its 300 ms are up |

```
$ hv run all 3
...
hv: guest fault -- a write to memory its page table maps and the nested one does not
  ran on     cpu 3, 64 us
  exits      0 port in, 0 port out, 0 cpuid, 0 hypercall, 0 host interrupt
  stopped    nested page fault at gpa 0x1ff000, error 0x100000006, rip 0x8005
  checked    stopped at the nested table, at the address and the instruction it was told to
hv: guest fault ok
...
hv: guest spin -- cli; jmp $ -- for as long as the host lets it
  ran on     cpu 3, 307051 us
  exits      0 port in, 0 port out, 0 cpuid, 0 hypercall, 31 host interrupt
  stopped    by the host, after 300 ms
  checked    the host's interrupts got through 31 times with the guest's off, and the host stopped it
hv: guest spin ok
hv: 6 of 6 guests ok
```

A guest bound to a CPU the extension is not on for is not run, and says
which CPU: `hv: guest exits not run -- the extension is not on for cpu 2`.

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
  `EFER.SVME` is clear -- so `svm::disable` sets it first, as KVM does. The
  run stub clears GIF only for its own length and sets it before it
  returns; the off switch is written to be right whatever the stub does.

And while a guest runs:

- **Every entry flushes the whole TLB**, the host's entries included --
  the price of every guest sharing one address space identifier, paid until
  an allocator hands them out per CPU with generations, the way KVM does,
  and knows when one may be reused. Cheap for these guests; for a Linux
  guest it is to be measured, and then replaced.
- **The FPU and SSE state is not switched.** The host uses none of it -- its
  C++ is built without SSE and x87, its Rust is soft-float -- so a guest's
  x87 and SSE registers are whatever the CPU it is entered on holds: its own
  while it stays on one CPU and nothing else runs a guest there, another
  guest's or stale state otherwise, since the vCPU's task may be moved
  between exits unless it was bound. None of the built-in guests touches
  them; the Linux guest brings `xsave`/`xrstor` with it.
- **No speculative-execution mitigation yet**: no return-stack refill after
  an exit, no IBPB between guests. What KVM does there matters before this
  runs other people's guests, and is not done.
- Each entry reads `EFER` and `VM_HSAVE_PA` on the CPU, and checks the VMCB
  against the manual's rules, before `vmrun`.

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
will be exercised before it reaches real hardware. On an Intel host that is
VT-x, whose guests are not written yet, so the gate uses KVM only where the
host has AMD-V and TCG's otherwise.

The gate is `scripts/hv-test.py` ([Tests and gates](testing.md)):

```sh
scripts/hv-test.py                  # x86-64: the extension on and off and off again, and every guest
scripts/hv-test.py --arch aarch64   # arm64: that it reports and refuses
```

## A Linux guest

`hv boot` loads an unmodified 64-bit Linux `bzImage` by the boot protocol and
runs it on a vCPU. The kernel and the initrd are streamed from a file
straight into guest memory a chunk at a time, so neither has to fit in one
allocation; then `hv::linux` lays the guest out -- the zero page from the
setup header, the command line, an e820 map, identity page tables and a GDT,
all written through `GuestMemory`'s copying accessors -- and puts the vCPU at
the kernel's 64-bit entry with `RSI` at the zero page. It holds no reference
into guest memory: everything it writes the guest could have written itself.

Three things stand between that entry and a running kernel, and all three are
here:

- **A CPU cut down to what is emulated** (`hv::policy`). Every CPUID is the
  host's, masked: no local APIC, no x2APIC, no XSAVE and so no AVX -- the
  state switch around `vmrun` moves only the FXSAVE registers, so a guest is
  given nothing it could put in the part that is not switched -- and no
  paravirtualisation. Every MSR is intercepted: the system MSRs (EFER, the
  PAT, the segment bases, the SYSCALL and SYSENTER registers) are the guest's
  own state and are served from the VMCB save area, and every other MSR reads
  zero and swallows a write, which is what a guest's `rdmsr_safe` probes are
  ready for.
- **The devices early boot cannot do without** (`hv::devices`): the 8250
  serial port the console writes to, an 8254 PIT whose counter counts down at
  1.193182 MHz off the host clock and whose channel-2 output goes high at its
  terminal count (what `pit_calibrate_tsc` waits on), and an MC146818 RTC
  that answers a fixed date with the update-in-progress bit clear. Without
  the last two a guest hangs: it spins on a PIT counter that never counts and
  an RTC update bit that never clears.
- **The exit loop** (`hv::run`) that answers all of the above, plus a nested
  page fault (an unemulated device), a triple fault, and the host's own
  interrupts, streaming the guest's console to the kernel log a line at a
  time and stopping with a reason and a register dump.

What this reaches today, on a tinyconfig Linux 6.18 under AMD-V (QEMU's TCG,
where the guest is twice emulated and slow), is the early console in full:

```
$ hv on
$ hv boot /bzImage
hv: booting /bzImage -- 256 MiB, cmdline "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"
  --- ttyS0 ---
[    0.000000] Linux version 6.18.53 ...
[    0.000000] Command line: earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check
[    0.000000] NX (Execute Disable) protection: active
...
[    0.000000] printk: legacy console [ttyS0] enabled
[    0.000000] Failed to register legacy timer interrupt
  --- end ttyS0 ---
```

It stops there because nothing yet delivers the timer interrupt -- the fourth
demo, and what [What comes next](#what-comes-next) opens with. The gate is
`scripts/hv-linux-test.py`, a manual one (a `bzImage` is megabytes and CI
cannot build one in its time), pointed at a kernel by hand.

## What comes next

From [`plans/03-hypervisor.md`](../plans/03-hypervisor.md), in order, each a
thing that can be shown in half a minute:

1. ~~a VM object with nested page tables, and a guest of a few bytes that
   exits where it was told to~~ -- under AMD-V, `hv run exits`;
2. ~~a guest in long mode under NPT/EPT~~ -- NPT, `hv run hypercall`;
3. ~~an emulated 8250 on port I/O exits, and a `bzImage` printing its early
   console~~ -- `hv boot`, and the guest of [A Linux
   guest](#a-linux-guest) below;
4. a full boot to a shell over that UART, with an initramfs, on one vCPU.

What is left for step 4 is the timer interrupt, and the interrupt controller
to take it from: a Linux guest today reaches `console [ttyS0] enabled` and
its clocksource setup and then stops at `Failed to register legacy timer
interrupt`, because nothing yet delivers IRQ0 -- an 8259 PIC (or a local
APIC), the PIT's channel 0 raising it, and `event_inj` injecting it when the
guest has interrupts on. Then an initramfs and an `init` that opens the
console. Still after that, not in step order: the TLB flushed per address
space rather than whole, the VMX backend with `CR0.NE` on every CPU, and the
guests run on the AX41's real AMD-V, which checks the VMCB harder than
QEMU does.

Two constraints from stage 5 (live update) hold from the first line of it:
all VM state is serializable plain data -- the vCPU register set, every
emulated device, and one table mapping guest physical pages to the VM that
owns them -- and no emulated device holds a pointer into arbitrary kernel
memory, only indices and handles that survive a re-init. So far the VMCB
and the registers `vmrun` leaves to software are plain words, and guest
memory is regions of pages whose nested table is derived from them.
