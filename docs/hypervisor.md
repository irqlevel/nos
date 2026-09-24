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

This is [stage 3 of the roadmap](../plans/03-hypervisor.md), and all four of
its steps are here: the machine's virtualization extension, found, reported,
and turned on and off for the CPUs that will run guests; a VM -- guest memory
behind a nested page table, one CPU under AMD-V, its exits decoded -- that
runs guests of a few bytes in long mode and checks that each did what it was
told; and, over an emulated 8250, PIT, RTC and 8259 PIC, a real Linux
`bzImage` booted to an interactive BusyBox shell (`hv boot`, and [A Linux
guest](#a-linux-guest) below).

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
    hv boot <bzImage> [mem=MiB] [secs=N] [cpu=N] [initrd=path] [disk=path[:ro]]... [input=...] [cmdline=...]
                                load a Linux bzImage and run it on a vCPU for
                                secs, then print its console and how it ended
    hv start <bzImage> [mem=MiB] [cpu=N] [initrd=path] [disk=path[:ro]]... [input=...] [log] [restart] [net] [cmdline=...]
                                the same, left running until it is stopped;
                                restart boots it again when it resets itself
    hv list                     the started guests: running or how they ended,
                                their CPU, uptime, restarts and exits
    hv console <id> [bytes=N]   the end of one's console
    hv attach <id>              its console, live and typed at, over ssh -t;
                                ^] detaches
    hv send <id> <text>         type at it, \n for a newline
    hv exec <id> [secs=N] <line>
                                type a line, print what comes back up to the
                                prompt after it
    hv wait <id> [secs=N] [boot=N] <text>
                                until this boot's console shows text -- with
                                boot=N, once it has restarted N times -- or it
                                stops
    hv restart <id>             boot it again from its files, running or stopped
    hv stop <id|all>            stop it, say how it ended, take it off the list
    hv forward [add <port> <vm> <guest-port> | del <port>]
                                a port of nos's relayed to a guest's
    hv help                     all of these, a line each

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
- nothing assumed clean;
- **the ASID, and what the TLB is told on the way in, given by the CPU the
  entry is on** ([Address space identifiers](#address-space-identifiers)):
  a translation left over from another guest -- or from a guest that has
  gone, to a page since handed back to the host -- would be the host's
  memory in a guest's hands, so which ASID a guest runs under is not the
  policy's to say.

### Entering and leaving

`Guest::run` turns interrupts off, checks on the CPU it is on that SVM is
on and that `VM_HSAVE_PA` is the page this hypervisor gave that CPU -- not
one an earlier load left behind -- and only then enters, so that what was
checked is still so: turning the extension off is an IPI, which waits. The
machine keeps each CPU's page address in an atomic beside its table for
exactly this check, set once the CPU has taken the page and cleared before
the CPU is told to let it go -- so before the page is freed, and before the
extension is off there.

### Address space identifiers

The TLB tags every translation a guest makes with the ASID it ran under and
keeps it until something flushes it. Every entry flushed the whole TLB at
first, because every guest ran under ASID 1: always right, and every exit
cost the guest its translations and the host its own. Now each CPU hands its
ASIDs out itself, the way KVM does -- `Asids` in `hvarch/src/x86/svm.rs`, a
`CpuLocal` only `Guest::run` touches, on its own CPU with interrupts off:

- each ASID is handed out at most once a **generation**, and a generation
  ends when they run out -- 32767 on a Zen 2, 15 under TCG -- with a flush of
  every entry of every ASID at that CPU's next entry into any guest. Until an
  entry the CPU accepted has made that flush, every entry there asks for it:
  one the CPU refuses (`VMEXIT_INVALID`) flushes nothing;
- a guest keeps its ASID, and its translations with it, only while it keeps
  entering on the CPU, in the generation and over the nested table it was
  given it for; anything else -- another CPU, a generation over, another
  table -- and it is given the next one. So no ASID is live for two guests,
  or for a guest and one that has gone, without a flush in between;
- every CPU starts each load of the module run out, so its first entry
  flushes whatever an earlier load's guests left in its TLB.

What lets a guest keep its translations at all is that its nested table only
grows: an entry is written once, where there was none, and stays until the
table goes (`hv/src/npt.rs`), so a translation the TLB still holds is one
the table would still make. Every table has an id no other table has, and an
entry that took a mapping away or narrowed it would give the table a new one
-- moving its guest onto a fresh ASID, clear of the old translation. None
does today.

The `asid` built-in guest is the check: two VMs on one CPU taking turns, an
exit each, each reading its own page at the same address, and a third made
once they have gone, with the ASIDs a generation limited to two so that it
is given one they had, after the flush that ended their generation. A missed
flush, or two guests under one ASID, reads another guest's page -- for the
third, a page back with the host. Under TCG every entry flushes whatever the
VMCB says, so the check can fail only on a CPU that keeps translations.

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
segment with D set, G_PAT's memory types, the injected event's
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
| `uart` | brings up an 8250 the way a driver does -- divisor behind DLAB, 8N1, FIFO, a scratch-register presence test -- and sends a line polled out of LSR.THRE | the emulated serial port, over port I/O alone |
| `fault` | writes to 0x1FF000, which its page table maps and the nested one does not | stopped at the nested table, at that address and that instruction: a guest reaches nothing it was not given |
| `absent` | reads AMD's FCH reset-status register at 0xFED803C0 twice, then writes to it | a read of a device the platform does not have finds all ones -- what Linux on a Zen CPU reads there -- through one read-only page; the write stops the guest |
| `triple` | `int3` with no IDT | a triple fault stops the guest, not the CPU |
| `refused` | starts with CR0.NW set and CD clear | a VMCB that breaks a rule is refused before the CPU sees it, the rule named |
| `spin` | `cli; jmp $` | the host's interrupts still get through -- about a hundred a second -- and the host stops it when its 300 ms are up |
| `asid` | three VMs on one CPU read a page of their own at one address: two taking turns, and a third given an ASID one of them had | no guest reads another's page through the TLB, and a reused ASID is reused after a flush ([Address space identifiers](#address-space-identifiers)) |

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
hv: guest asid -- three VMs on one CPU, and none reads another's page through the TLB
  ran on     cpu 3, 13708 us
  exits      24 cpuid, 0 host interrupt
  checked    A and B each read their own page taking turns; vm C was given ASID 1, which vm A had, after 2 generation(s) ended, and read its own too
hv: guest asid ok
hv: 9 of 9 guests ok
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

- **The TLB is flushed only when a CPU's ASIDs run out** ([Address space
  identifiers](#address-space-identifiers)) -- once every 32767 guests, or
  guests moved between CPUs, on a Zen 2 -- rather than on every entry, the
  host's entries included, as it was. `hv bench ... flush` still does that,
  for what it costs ([What an exit costs](#what-an-exit-costs)).
- **The x87 and SSE state is switched on every entry and exit**, with
  FXRSTOR before `vmrun` and FXSAVE after, into an area each guest owns --
  so it is the guest's whichever CPU the vCPU's task is on and whichever
  guest ran there last. The host uses none of it (its C++ is built without
  SSE and x87, its Rust is soft-float), so CR4.OSFXSR, without which AMD's
  FXSAVE leaves the XMM registers out, and CR4.OSXSAVE are set only for the
  interrupts-off window around `vmrun` and given back before interrupts come
  on: outside it an SSE instruction in the host still faults. XCR0 is x87
  alone while a guest runs, and guests are given no XSAVE, so AVX and above
  fault in a guest even if it turns OSXSAVE on itself. XCR0 is written only
  when the host's is something else -- nothing in this kernel changes it from
  the x87 alone a CPU comes out of reset with, firmware that used AVX may
  have -- since XSETBV serializes the CPU, twice an exit.
- **A halted guest costs its CPU nothing** -- but waits at the host tick's
  grain. A HLT with interrupts on is stepped past, as a CPU an interrupt
  wakes resumes after it, and the vCPU is not entered again until the PIC
  has an interrupt for it; meanwhile its task sleeps to the timer's next
  edge. `task::sleep` blocks until then and is woken at its CPU's first
  scheduling point after it -- the host tick, at the latest -- so a guest's
  tick arrives up to 10 ms late. None is lost: every periodic edge that
  elapsed is owed to the guest and handed over one at a time, each once it
  has taken the last (`Pit::ch0_fire`), up to a second's worth. A guest at
  250 Hz was given one edge for every two or three periods before, and a
  guest keeping time in jiffies counted 40% of real time (see [On real
  hardware](#on-real-hardware)). Measured on the Linux
  guest over 120 s of the same boot and a typed `id`: 5,633,131 exits, 5.56
  million of them HLTs, became 76,334 and 11,410; the guest's timer ticks
  went from 11,973 to 11,943; the vCPU's task slept 89% of the run; and the
  QEMU process under it went from 106% of a host CPU to 46%.
- **No speculative-execution mitigation yet**: no return-stack refill after
  an exit, no IBPB between guests. What KVM does there matters before this
  runs other people's guests, and is not done.
- Each entry reads `EFER` and `VM_HSAVE_PA` on the CPU, and checks the VMCB
  against the manual's rules, before `vmrun`.

### What an exit costs

`hv bench` runs a guest that makes nothing but exits -- CPUIDs, each after a
read of each of `pages` pages 4 KiB apart -- on a task bound to one CPU, and
times it:

```
hv bench [cpu=N] [exits=N] [pages=N] [flush] [profile]
```

`flush` makes every entry flush the whole TLB, as every entry did before
ASIDs, so the two can be set side by side on one boot; `pages` is what a
flush costs the guest after it -- each read a translation to walk again --
where with none it is what it costs the host. `profile` times each entry in
its parts with the time-stamp counter: the checks and the ASID; the x87/SSE
registers and XCR0 put in; `vmrun` to `#vmexit`, the stub's VMSAVE and
VMLOAD and the guest's few instructions included; the registers taken back
out; and the rest, which is the exit handled and the loop round to the next
entry:

```
$ hv bench exits=20000 profile
hv: 20000 exits on cpu 3, 0 pages read between, ASIDs kept: 342 ms, 17130 ns an exit, 58374 a second (12 host interrupts among them)
hv: an exit, in ns: checks and ASID 127, x87/SSE in 605, vmrun to #vmexit 9012, x87/SSE out 731; the rest -- the exit handled, the loop -- 6655
```

Those are TCG's numbers, and say nothing about a CPU: TCG flushes its own
TLB on every `vmrun` whatever the VMCB asks, and emulates every instruction
of the host's side as well as the guest's. On the AX41 (Zen 2, 2026-09-24,
two rounds, the same to a few ns):

| Between exits | ASIDs kept | TLB flushed every entry | |
|---|---|---|---|
| nothing | 731 ns an exit, 1.37 million a second | 940 ns, 1.06 million | -22% |
| a read of 64 pages | 873 ns | 1321 ns | -34% |
| a read of 240 pages | 2180 ns | 2448 ns | -11% |

and the profile of an exit (with its own ~40 ns in it):

| | ASIDs kept | flushed |
|---|---|---|
| checks and ASID | 90 ns | 90 ns |
| x87/SSE and XCR0 in | 119 ns | 120 ns |
| `vmrun` to `#vmexit` | 314 ns | 424 ns |
| x87/SSE out | 102 ns | 105 ns |
| the exit handled, the loop | 148 ns | 249 ns |

The flush cost the world switch 110 ns and the host's side after it 100 ns
more, its own translations walked again; with 64 pages the guest's as well.
What is left to take is on the host's side of the switch: the x87/SSE
state and CR4 around it, 220 ns an exit, and the checks' two RDMSRs.

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
  paravirtualisation. And one CPU of its own, whichever host CPU its vCPU runs
  on: leaf 1 says one logical CPU with APIC ID 0, leaves 4 and 0x80000008 one
  core, and the topology leaves (0xB, 0x1F, 0x8000001E), the SVM leaf and the
  memory-encryption leaf are blank. Every MSR is intercepted: the system MSRs (EFER, the
  PAT, the segment bases, the SYSCALL and SYSENTER registers) are the guest's
  own state and are served from the VMCB save area, and every other MSR reads
  zero and swallows a write, which is what a guest's `rdmsr_safe` probes are
  ready for.
- **The devices a guest cannot boot without** (`hv::devices`): the 8250
  serial port the console writes to (which raises IRQ4 for the transmitter,
  so the driver sends past its first byte); an 8254 PIT whose counter counts
  down at 1.193182 MHz off the host clock, whose channel-2 output goes high
  at its terminal count (what `pit_calibrate_tsc` waits on), and whose
  channel 0 raises IRQ0 -- the system tick a guest cannot schedule without:
  an edge a period in the periodic modes 2 and 3, and one when a loaded count
  runs out in the one-shot modes 0 and 4, which a kernel's high-resolution
  timers drive a count at a time; an MC146818 RTC that answers the host's
  wall clock with the update bit clear; and a pair of 8259 PICs the guest
  takes its interrupts from, since it runs with no local APIC.
- **The exit loop** (`hv::run`) that answers all of the above, injects the
  highest-priority interrupt the PIC has when the guest can take one (and
  asks the CPU, through SVM's virtual-interrupt window, to exit the moment it
  can when it cannot), and halts the vCPU when the guest does: an idle `HLT`
  is stepped past -- a CPU an interrupt wakes resumes after it, so Linux's
  `sti; hlt; cli` returns to its idle loop, and out of the STI's interrupt
  shadow -- and the vCPU is not entered again until an interrupt is pending
  for it, its task asleep until the timer's next edge. It streams the
  guest's console to the kernel log a line at a time and stops with a reason
  and a register dump.

What this reaches today, on a tinyconfig Linux 6.18 under AMD-V (QEMU's TCG,
where the guest is twice emulated and slow), is a full boot to an interactive
shell:

```
$ hv on
$ hv boot /bzImage initrd=/initrd cmdline=console=ttyS0 nolapic rdinit=/init
  --- ttyS0 ---
[    0.000000] Linux version 6.18.53 ...
[    0.000000] NX (Execute Disable) protection: active
...
[    0.540000] Run /init as init process
nos-guest: init is up, / on ramfs, busybox v1.36.1
Linux (none) 6.18.53 #1 x86_64 GNU/Linux

BusyBox v1.36.1 built-in shell (ash)
~ # id
uid=0 gid=0
~ #
  --- end ttyS0 ---
```

The shell reads what is typed at it: `hv boot ... input='id\n'` hands the
guest a line once it is at a prompt, and the 8250's receive path -- with the
answer to the cursor-position query (`ESC[6n`) a line editor sends first --
carries it. The gate is `scripts/hv-linux-test.py`, a manual one (a `bzImage`
is megabytes and CI cannot build one in its time), pointed at a kernel by
hand; with `--initrd` it checks the guest reaches its `init` and a shell, and
`--input 'id\n' --expect uid=0` checks it runs the command.

## A disk

`disk=<path>`, on `hv boot` or `hv start` and as many times as there are
disks, gives the guest a virtio block device over a file of nos's: `vda`,
`vdb`, ... in that order, each the size of its file. The guest mounts it,
ext4 and all, as it would a disk of QEMU's:

```
$ hv start /bzImage initrd=/initrd disk=/disk.img cmdline=console=ttyS0 nolapic
$ hv exec 0 mount -t devtmpfs devtmpfs /dev
$ hv exec 0 mount /dev/vda /mnt
$ hv exec 0 cat /mnt/hello.txt
```

**Legacy virtio over PCI, reached by port I/O** -- so still no instruction
decoded anywhere. PCI's configuration space is the PC's mechanism #1, an
address written to 0xCF8 and the data at 0xCFC, for bus 0: a host bridge at
00:00.0, which is what Linux's sanity check of the mechanism looks for, then
a function for each disk (`hv::devices::pci`). Only a dword access at 0xCF8
reaches the address -- a byte at 0xCF9 is the chipset's reset control, which
the run loop sees first. Each disk's registers are in an I/O BAR, placed as a
BIOS would place it and movable as a BAR is; its interrupt is INTA, on IRQ 11
of the emulated slave PIC, raised as an edge when the device's interrupt
status goes from clear to set -- the PIC here is edge-triggered, and a line
re-raised while the driver has yet to look would be interrupts it finds
nothing for, which Linux ends up disabling a line over. (That exposed a slip
in the PIC: acknowledging a slave interrupt dropped the cascade line even
with another slave request waiting.)

**The rings are the guest's; the host trusts none of them** (`hv::devices::
virtio`). A queue's descriptors, its available ring and its used ring sit in
guest memory, where the guest may write any of them at any moment. Every
index, address and length read from them is checked before it is used: a
descriptor past the queue, a chain longer than the queue -- a loop --, more
chains than the queue holds, an indirect descriptor (not offered), or a
buffer outside guest memory stop the device, which takes nothing more until
the driver resets it. A request's header, data and status are taken as
streams across its readable and writable buffers, whatever the layout; its
sector range is checked against the disk's size; and the data crosses
through a 64 KiB buffer of the device's, a chunk at a time -- nothing is
allocated per request. A request is served before the notify that made it
available returns: the vCPU waits for its disk.

**The disk's bytes are a trait's** (`hv::disk::Backend`): the module's
`FileDisk` reads and writes the file where the guest asks
(`kcore::fs::read_at`, and `write_at`, which writes within the file's size
and never grows it), and a guest's flush -- offered, and taken by ext4 --
syncs nos's filesystems (`kcore::fs::sync`). The report counts each disk's
reads, writes, flushes and errors.

`scripts/hv-linux-test.py --disk` is its gate: an ext4 image with a file in
it on nos's root, the guest mounting it, reading the file, writing 4 MiB and
a file of its own, syncing and reading back after a remount -- and then the
image, taken back out of nos's root filesystem, judged by `e2fsck` and
holding what the guest wrote. It needs a guest kernel with PCI, legacy
virtio-pci, virtio-blk and ext4.

## A network

`net`, on `hv start`, gives the guest a NIC on the guests' switch, and with
it an address: 10.0.100.2 for the first port, .3 for the next, handed to its
kernel on the command line (`ip=`). nos is 10.0.100.1 on the same subnet, so
the guests reach it and each other, and it reaches them -- `ping 10.0.100.2`
from nos's shell goes out of the device whose subnet the address is on:

```
$ hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
$ hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
$ hv exec 0 ping -c 3 10.0.100.1        # nos
$ hv exec 0 ping -c 3 10.0.100.3        # the other guest
$ ping 10.0.100.2                       # from nos
$ hv forward add 8080 1 80              # nos's 8080, to guest 1's 80
```

**The guest's NIC is virtio-net, legacy, on the same transport as its
disks** (`hv::devices::net`): two queues, a MAC and a link status offered,
no checksum offload and no segmentation, so a frame either way is a whole
Ethernet frame after legacy's 10-byte header. A frame the guest sends is
handed on before the notify returns; what waits for it is put into the
buffers it has posted each time round the run loop, one frame held by the
device while it has posted none.

**nos's end is a virtual NIC, `hv0`, in its own stack** (`net/src/vnic.rs`).
A module cannot register a NIC -- there is no C name for a driver to be bound
by -- so the device is registered from inside the image, the first time a
module asks for it by name, and stays; the module trades frames with it
through `kcore::vnic`. What the stack sends out of `hv0` goes to the sink the
module attached, from the transmit path with interrupts off, and what the
module hands in arrives as if received, on the next receive pass. A detach
waits out any call of the sink still running, so the module can go after --
the UDP listeners' way.

**The switch is the module's** (`modules/hv/src/net.rs`). A guest's NIC is a
port, and a port's number is its address and its MAC (02:00:00:00:64:NN), so
frames go by their destination MAC with nothing learned: to a port, to `hv0`,
or to everyone for a broadcast or a MAC no port has. A port's inbox holds
256 frames, the guest's receive ring's worth, and is filled from any CPU --
`hv0`'s sink among them, interrupts off -- so it is a spin lock with
interrupts off over storage taken when the port is first claimed, and kept
for the next VM on it; a full one drops, counted in `hv list`. A frame put in wakes the guest's vCPU:
a halted guest waits on its VM's event, until its timer's next edge or
something for it -- a frame, a key typed at `hv attach` or `hv send` --
whichever comes first (`Event::WaitFor`, [the scheduler](scheduler.md#blocking-and-waking)).

**A guest that is running is kicked out of it.** Its turn ends when its
CPU takes an interrupt, and the device model hands frames over only between
turns, so a frame for a busy guest used to wait for the host's next tick --
ten milliseconds, while at line rate the port's 64-frame inbox fills in
under one. On the AX41 a guest that `apt-get update`d, fetching over several
connections while it decompressed, lost some 1,400 frames that way and took
39 s over 28.5 MB. Now the switch kicks the vCPU as KVM does
(`hvarch::x86::svm::Kick`): the vCPU marks itself on its way in before its
last look at the inbox, a sender that finds it so marks it exiting and
interrupts its CPU (`kernel_cpu_kick`, straight to the interrupt controller,
from any context), and `Guest::run` looks at the mark once more with
interrupts off: a kick from before that point turns the entry back
(`Exit::Kicked`), one after it is an interrupt held pending, which ends the
guest's turn as it begins. One interrupt per entry at most, however many
frames come; `hv list` counts them (`kicks`).

Measured on the AX41 in one boot, the same Debian guest and three builds
of the module loaded in turn, a cold `apt-get update` (28.5 MB) and a
gigabyte from Hetzner's speed-test server each:

| build | `apt-get update` | frames dropped | 1 GB |
|---|---|---|---|
| no kick, 64-frame inbox | 16 s, 10 s | 100, 44 | 52, 42 MB/s |
| kick, 64 frames | 7 s, 8 s | 437, 141 | 61, 60 MB/s |
| kick, 256 frames | 9 s, 7 s | 0, 0 | 68, 56 MB/s |

The kick halves the fetch and lifts the bulk rate by a fifth to two fifths
(about 220,000 kicks for the gigabyte, one per three frames). With frames
flowing that much faster the 64-frame inbox overflowed more on apt's bursts
than it had without the kick, and at 256 -- the guest's own ring -- none
was lost. What the gigabyte still loses (500 to 800 frames) is the guest's
own ceiling: one vCPU takes each frame through a legacy virtio interrupt
and the 8259's, several exits apiece, at 55 to 70 MB/s, and TCP paces to
it. That is the next thing to take out: fewer exits per frame, not more
room for them.

**`hv forward` is how a guest is reached from outside.** The guests have
addresses only on their switch; a forward listens on a port of nos's and,
for each connection, opens one to the guest's port through `hv0`
(`kcore::tcp::TcpStream::connect`, new for this) and relays the two with a
task of its own. The kernel has 64 TCP connections and a forwarded one takes
two, so a forward carries eight at once and refuses the rest. The other way,
the guests reach the world through NAT (next).

### The way out: NAT, DHCP and DNS

```
$ hv exec 0 wget -q -O - http://10.0.2.2:8000/    # a server beyond nos
$ hv exec 0 udhcpc -i eth0 -q                     # the switch answers
$ nat
nat: on, hv0 (10.0.100.1) out through eth0 (10.0.2.15); 4 of 4096 mappings, ports 32768-36863
  tcp 10.0.100.2:47410 -> 10.0.2.2:8000 as 32768, 7190 s left
  udp 10.0.100.2:37716 -> 10.0.2.3:53 as 32771, 297 s left
out 53  back 744  mapped 4  table full 0  no next hop 0  no frame 0  refused 0
```

**NAT is the network layer's** (`net/src/nat.rs`), not the module's: it is
forwarding between two of the stack's devices, and it has to look at every
packet either of them receives before the protocols do. While the switch
exists it holds NAT on (`kcore::net::Nat`, a guard: dropped with the switch,
it turns NAT off) from `hv0` out through the device nos's default route is
on -- eth0, with the gateway its DHCP lease named. A guest's packet for an
address off its subnet, and not one of nos's, arrives on `hv0` sent to its
gateway, 10.0.100.1; instead of the protocols it goes to NAT, which gives its
flow a mapping -- an external port of nos's, 32768 up -- rewrites its source
to eth0's address and that port, one hop less to live, and sends it out of
eth0 to the next hop. What eth0 receives for a mapped port, from the address
and port the flow went to and nothing else, is rewritten back and sent to the
guest; so is an ICMP error quoting one of the flow's packets, the packet it
quotes made the one the guest sent. Whatever else arrives is the stack's, as
before -- a port `hv forward` listens on is not NAT's unless a guest's flow
has it, which it cannot, being below 32768.

What it takes: TCP from a SYN on (a segment of a flow it has no mapping for
is dropped, not mapped mid-stream), UDP, ICMP echo; not fragments, not other
protocols, and nothing inbound that a guest did not start -- that is
`hv forward`'s. A mapping lives while its flow is seen and RFC 5382's and
4787's times after: two hours for a TCP connection that has been answered,
four minutes before that and after a FIN, ten seconds after a reset, five
minutes for UDP, one for an echo. The checksums are updated for what changed
rather than summed again (RFC 1624), so a packet corrupted on the way stays
detectably corrupt; an ICMP error, whose whole message changes, is checked
before it is rewritten. The boot self-test (`rust_net_selftest`) checks every
rewrite against checksums summed from scratch, and the table's filling,
expiry and reuse.

The table is 4096 mappings and a hash of the guests' side of them, taken
whole when NAT goes on, so the datapath allocates nothing but the frame a
packet goes out in, from the pool. It is under a spin lock held for the
lookup only; the frame is built and sent with it down. When all 4096 are
live the next flow is refused (`table full`) rather than made to evict one,
and the table is swept for flows past their time no sooner than the first of
them can be. The next hop's address comes from the ARP cache without waiting
-- NAT runs on the receive path, which cannot sleep: an entry past its time is
still used while it is asked for again, as Linux uses a stale neighbour, and
the gateway and an on-link DNS server are asked for when NAT goes on, so a
guest's first packet does not find them missing (`no next hop` counts those
that did). A request goes out at most every 100 ms whatever a guest floods.

**The switch answers DHCP** (`modules/hv/src/dhcp.rs`). A guest's DHCP
message never leaves its port: the switch answers it in place with the
port's address -- the same as `ip=` gives -- a day's lease, the /24 mask, nos
as the router, and nos's DNS server. Nothing is remembered, because there is
nothing to choose: a request for any other address is refused (NAK) and the
client starts again. A distribution that configures its ethernet by DHCP, as
most do, needs nothing on its command line; one that takes `ip=` gets the
same address from there.

**The DNS server the guests are given is nos's own** -- its resolver's, or
the one its lease named (`kcore::net::dns_server`): 10.0.2.3 under QEMU's user
network, Hetzner's resolvers on the Hetzner boxes. `ip=` carries it as `dns0`,
which the kernel shows in `/proc/net/pnp` and Alpine's initramfs writes to
`resolv.conf`, and DHCP as option 6, which systemd-resolved takes. The guests
ask it through NAT like any other server; nos runs no DNS server of its own.

`scripts/hv-linux-test.py --net` is the network's gate: two guests, their
addresses, pings between each guest and nos and between the guests, nos's
ping out of `hv0`, and a page from one guest's `httpd` fetched from outside
the machine through `hv forward`; then the way out -- `dns0` in the guest's
`/proc/net/pnp`, a page and a megabyte (md5 compared) from a web server on the
test machine through NAT, a ping there, `udhcpc` given its port's address,
nos as its router and nos's DNS server, and `nat` and `hv list` saying so.
With `--internet`, a name looked up through the DNS server it was given.

## Guests that stay up

`hv boot` is a guest for a set time, run from start to end inside one
command. `hv start` is the same guest left running: the command builds it,
hands it to a vCPU task of its own and returns, and the guest is reached from
then on by the commands above -- from the shell, over ssh or over the UDP
shell alike. They are the lifecycle stage 4's HTTP API is to put on the
network ([`plans/04-control-plane.md`](../plans/04-control-plane.md)); the
commands come first because they need nothing but a shell.

```
$ hv start /bzImage initrd=/initrd cmdline=console=ttyS0 nolapic
hv: vm 0 started on cpu 3 -- /bzImage, 256 MiB, cmdline "console=ttyS0 nolapic"
$ hv exec 0 secs=60 id
... the rest of its boot ...
~ # id
uid=0 gid=0
~ #
$ hv exec 0 uname -r
uname -r
6.18.53
~ #
$ hv send 0 echo nos$((6*7))nos\n
hv: vm 0: 20 bytes queued
$ hv wait 0 nos42nos
hv: vm 0 printed "nos42nos", 58 ms in
$ hv list
vm 0  running  cpu 3  256 MiB  1 s  exits 18237  irq 284  hlt 92  /bzImage
$ hv stop 0
hv: vm 0 stopped -- on request
  stopped    on request, after 1925 ms
  exits      ...
```

(From the gate's run under TCG, where the guest is at its shell a second
after it starts.)

**A VM is a task and a shared record** (`modules/hv/src/vms.rs`). The guest
-- its memory, its vCPU, its devices -- belongs to the vCPU task (`hv/vm<N>`)
alone, and is freed there when the guest stops. What the task and the
commands share is the console, its last 64 KiB in a ring; the bytes waiting to
be typed at it, 4 KiB at most; a stop flag; and the loop's counters, which it
publishes at every halt and every 4096 exits. The ring and the input queue
are taken whole when the VM is made, so nothing on the guest's datapath
allocates. Each vCPU is bound to one CPU the extension is on for: `cpu=` if
given, else the one with fewest running VMs -- the highest of a tie, so the
boot CPU's own work is the last to share its CPU with a guest.

**`hv exec` knows where its answer starts.** It types the line and a newline
and waits for a prompt -- but only one printed after the line's last byte was
handed to the guest's UART: the loop notes where the console was at that
moment, and only what comes after it can end the wait. So a line typed at a
guest that is still booting waits for its shell and comes back with its
answer, not with the first prompt the shell printed before it read the line;
and a line the guest never reads says so rather than timing out in silence.
It prints everything the guest printed since the line was typed.

**What reaches a terminal is made safe for one.** `hv console`, `hv exec` and
the report print the guest's console with escape sequences taken out whole,
carriage returns dropped and anything above ASCII shown as `?`: a guest
shell's `ESC [ 6 n`, printed to the terminal a person is reading nos on,
would make that terminal type its answer into whatever reads it next.

**The cursor query is answered with the cursor.** BusyBox's line editor asks
where the cursor is after it prints its prompt, and takes the column it is
told as the prompt's end. It was once told column 80 whatever the prompt, and
wrapped what was typed at the edge it thought it had reached -- `id` came back
as `i`, a newline, `d`. The UART now follows the guest's output as a terminal
would -- the column each byte leaves the cursor at, escape sequences moving
it nowhere -- and answers with that, from a buffer of its own: a guest that
asks and never reads the answer costs the host no more than one that asks
once, where the answer used to be appended to a queue without bound.

**Stopping is a flag and a join.** The loop checks the flag before every
entry, and a halted guest's task sleeps at most 10 ms at a time, so `hv stop`
returns within a tick or so of asking: it takes the VM off the list first and
joins its task after, with no lock held while it waits. A guest that stops by
itself -- a triple fault, a HLT with interrupts off, a reset -- stays on the
list as `stopped`, with its reason, until `hv stop` takes it off.

**A VM's task lives as long as the VM.** When its guest stops, the vCPU task
gives the guest's memory back and parks on the VM's event, for `hv restart` --
which builds the guest again from its files and boots it, a stopped guest or
a running one, as a reset button would -- or `hv stop`, which ends it. With
`restart`, a guest that resets itself or triple-faults is booted again by the
task straight away: a reboot. Five of those within a minute are taken for a
loop -- a guest that panics at boot with `panic=1` would otherwise have its
CPU for good -- and it is left stopped, `reset 6 times in 60 s, left
stopped`. `hv list` counts the restarts; the console runs on across them,
one log of every boot; a line `hv exec` typed at a boot that ended says so
rather than waiting for a prompt that will not come.

**`hv attach` is the console, live.** From an SSH session with a terminal
(`ssh -t <host> hv attach 0`) it shows the last kilobyte of the console, then
what the guest prints as it prints it, and types what is pressed as it is
pressed -- the guest's line editor, ^C and all -- until ^] detaches, the guest
stops, or the session ends. What it passes to the terminal is the guest's
output as it is, colours and cursor movement included, but for what a
terminal would answer or act on: a status or attributes query (`ESC [ 6 n`,
`ESC [ c`), whose answer would be typed into the guest after the UART's own,
and the string sequences (`ESC ]` and its kind), which set a terminal's
title, clipboard or palette -- nothing a guest should reach on the person's
machine. It takes a session that can type: a command reads what is typed
through its output (`Output::read_input`), and only an SSH session's has
anyone behind it -- from the console, the UDP shell or `/etc/rc` it says so
and returns. Keys typed at an attached guest are not held back for a prompt
the way a script's line is: the person decides when to type.

**A guest that reboots stops, and says so.** Linux reboots -- a `reboot`, a
panic with `panic=N` -- by pulsing the 8042's reset line (`0xFE` to port
`0x64`), then by the chipset's reset control (bit 2 of port `0xCF9`, where
it knows of one), then by a triple fault. Nothing here emulates an 8042 or a
chipset, and on the AX41 the first guest that rebooted spun at 100% of its
CPU for as long as anyone let it -- 1.3 million reads of the 8042's status in
48 seconds, waiting for a controller that is not there. Both writes now stop
the VM (`Stop::Reset`), `the guest asked for a reset, 0xfe to port 0x64 (the
8042's reset line)`; its ports still read as all ones, as on a PC without
one, so Linux spends its `kb_wait` polling them first -- 65536 reads, a
couple of seconds -- and then asks. What a reset should become -- the guest
booted again -- is the control plane's to decide.

**Idle guests cost their CPU next to nothing, sharing one or not.** A halted
vCPU sleeps until its guest's next timer edge, and `Sleep()` blocks
([the scheduler](scheduler.md#blocking-and-waking)): it used to be a loop
around `Schedule()`, and two halted guests on one CPU of the AX41 handed it to
each other without end -- 46% of it each, the idle task never running -- where
one alone took 0.1%. Blocking, the same two take 0.1% each, and sixteen idle
guests on the AX41's twelve CPUs -- four of them carrying two -- keep the
machine 1.6% busy, 0.1% a vCPU.

**The extension is not pulled from under a guest.** `hv off` refuses while a
started guest runs on one of the CPUs it names. That is a courtesy and not
what keeps the CPU safe: every entry checks, with interrupts off until the
guest is running, that the extension is on and its save area is the one this
module gave that CPU, so a guest racing an `hv off` finds it off and stops,
`not entered`. `rmmod hv` stops every guest before it turns the extension off
-- the guests first, an `hv boot` under way among them, so that an `hv exec`
or `hv wait` waiting on one returns; then the command, whose unregistration
waits out every call still running (an `hv start` still reading its files
finds the module going when it comes to add its guest, and stops it itself);
and only then the extension, on every CPU.

The gate is `scripts/hv-linux-test.py` with `--initrd`: two VMs started side
by side and one stopped mid-boot, a line typed at the other before its shell
is up and another at its prompt, `send`, `wait`, `console`, the refused
`hv off`, and an `rmmod` that stops the guest itself and leaves nothing on for
the next load.

## A distribution

The guests above run a kernel built for the purpose, with a BusyBox
initramfs. A distribution's is built for every machine instead -- SMP, ACPI,
KASLR, high-resolution timers, virtio as modules its initramfs loads -- and
Alpine's `virt` ISO boots under `hv start` as it ships: its kernel and
initramfs taken out of the ISO, and the ISO itself the guest's read-only
disk.

```
$ hv start /alpine/vmlinuz-virt mem=512 initrd=/alpine/initramfs-virt disk=/alpine/alpine.iso:ro net restart cmdline=console=ttyS0 nolapic acpi=off modules=loop,squashfs,sd-mod,usb-storage
$ hv wait 0 secs=600 login:
hv: vm 0 printed "login:", 36004 ms in
$ hv send 0 root\n
$ hv exec 0 cat /etc/alpine-release; uname -r
3.24.2
6.18.52-0-virt
localhost:~#
$ hv exec 0 date
Wed Sep 23 21:32:50 UTC 2026
```

(Under TCG, twice emulated: 36 s to the login prompt.) Its initramfs loads
virtio_blk, finds the ISO on `vda`, mounts it and installs the base system
from the ISO's packages into a tmpfs, and OpenRC brings it up to a getty on
ttyS0. `nolapic acpi=off` is the PC it is given -- no local APIC and no ACPI
tables -- so it takes its interrupts from the 8259s, finds its PCI devices
through configuration mechanism 1, and takes each virtio device's interrupt
line from its configuration space. On the switch (`net`) its initramfs
configures eth0 from the `ip=` the VM is given, and writes the `dns0` there
to `resolv.conf`; through NAT it reaches what nos reaches -- `apk update`
from Alpine's own mirror, the name looked up through nos's DNS server --
and `apk add openssh-server` installs from the ISO, its own sshd reached
from outside nos through `hv forward`. `reboot` resets it through the
keyboard controller, and with `restart` it boots again.

What it took that the purpose-built kernel did not:

- **The PIT's one-shot modes.** A kernel with high-resolution timers and a
  clocksource good enough for them -- the TSC, which the guest calibrates
  against the PIT by itself on a real CPU -- moves its tick from the PIT's
  periodic mode 2 to one-shot mode 4, a count at a time. Channel 0 raised
  IRQ0 only in the periodic modes, so the guest's timers would have stopped
  at the switch. Under TCG an exit costs more than the calibration loop
  allows and the guest stays on jiffies and a periodic tick; forced onto the
  TSC (`tsc_early_khz=... tsc=reliable`) it runs its tick through mode 4,
  and `sleep 2` takes 2.03 s.
- **A clock.** The RTC answered a fixed 2026-01-01, and OpenRC took every
  file of the ISO for one from the future, and said so at every service it
  started. It answers the host's wall clock now, read when the guest is made
  and counted on by the host's clock since boot; what the guest writes to
  the time registers does not stick.
- **One CPU of its own.** The topology CPUID gave was the host's: the guest
  took the APIC ID of the host CPU its vCPU ran on for its own ("APIC ID
  mismatch") and listed SVM's features for an extension it is not given.
- **Typing at a login prompt.** `hv exec` and `input=` wait for a shell's
  prompt -- BusyBox's line editor asking where the cursor is -- and a getty
  asks for nothing, and throws away what was typed before it printed its
  prompt. `hv send` types now, as at a terminal, and from then on for that
  boot what `exec` types goes in as it is typed too; `hv wait` is what finds
  the moment.
- **`hv wait` in the current boot.** It searched the whole console, so after
  a reboot it found the last boot's `login:`; it looks from where the
  current boot began.
- **`disk=path:ro`**: virtio's read-only feature, and a write the device
  refuses itself before any reaches the file.
- **An `ip=` that names no device.** A kernel told `eth0` waits twelve
  seconds for it to appear (`DEVICE_WAIT_MAX`), and a distribution's NIC
  driver is a module its initramfs loads after that; with the field empty
  the kernel finds no device at once and leaves `ip=` to the initramfs,
  whose own parser takes the first interface that comes up.

### Debian

Debian 13's `nocloud` cloud image boots the same way: its kernel
(6.12.107+deb13-amd64, the generic one) and initrd read out of the image's
own `/boot`, and the image itself -- 3 GiB, a raw file on nos's root -- the
guest's disk, whose root partition its initramfs-tools mounts read-write:

```
$ hv start /debian/vmlinuz mem=768 initrd=/debian/initrd disk=/debian/debian.raw net restart cmdline=root=/dev/vda1 ro console=ttyS0 nolapic acpi=off systemd.set_credential=passwd.plaintext-password.root:nos systemd.set_credential=firstboot.locale:C.UTF-8 systemd.set_credential=firstboot.keymap:us systemd.set_credential=firstboot.timezone:UTC
$ hv wait 0 secs=600 login:
hv: vm 0 printed "login:", 38931 ms in
$ hv send 0 root\n
$ hv send 0 nos\n
$ hv exec 0 cat /etc/debian_version; uname -r; systemctl is-system-running
13.7
6.12.107+deb13-amd64
running
```

Its root is locked (`!unprovisioned`) until systemd-firstboot sets it, and
firstboot asks at the console for the root password, the locale, the keymap
and the timezone -- a typed line that is not one is "Invalid data" -- so
`systemd.set_credential=` on the kernel command line gives it all four, as
systemd provisions a machine nobody sits at. systemd then reaches `running`
with no unit failed, 38 s after the VM starts under TCG, and a file written
to its root is still there after a reboot. Its networkd has no `.network`
for an ethernet interface -- the image expects cloud-init or the like to
write one -- and one more credential is that file:
`systemd.set_credential_binary=network.network.50-nos:<base64>` of
`[Match] Type=ether` / `[Network] DHCP=ipv4`, which systemd-network-generator
puts in `/run/systemd/network`. networkd then asks the switch's DHCP server,
takes its port's address, nos as its router and nos's DNS server for
systemd-resolved, and the guest reaches the world through NAT. That command
line runs past the 255 characters nos's `/etc/rc` allowed a line; it allows
1023.

After typing `reboot`, a script cannot wait for the next `login:` with a
plain `hv wait`: the boot going down still has the last one on its console,
and systemd's `reboot` gives the shell its prompt back before the system
goes. `hv wait 0 boot=1 login:` waits for the VM's first restart, and then
for the text in the boot after it.

And one thing in the kernel. Rebuilding the rebooted guest under TCG held
the page allocator's lock for more than ten seconds, with every other CPU
idle, and the watchdog panicked the machine: each page was zeroed under the
lock, and QEMU throws away its translations of a page that held a guest's
code as the page is written -- a guest that had run for half a minute took
36 s to rebuild. The zeroing is done off the lock now (`PageTable::AllocPage`).

The gate is `scripts/hv-distro-test.py --iso alpine-virt-*-x86_64.iso
[--debian debian-*-nocloud-amd64.raw] [--internet]` ([Tests and
gates](testing.md)): manual, since the images are downloads; `--internet`
adds what needs the test machine's own way out, a name looked up and a
mirror fetched from.

## On real hardware

The AX41 (a Ryzen 5 3600, Zen 2) has the whole of AMD-V -- 32768 ASIDs,
next-RIP save, decode assists, flush by ASID, VMCB clean bits, AVIC -- and
checks a VMCB as QEMU does not. There every built-in guest passes on the
first CPU and the last, the extension turns on and off for all twelve, and
the Linux guest boots to its shell and runs a typed `id`, at native speed:
`Run /init as init process` 18 ms into the guest's clock, against half a
second under TCG. The PIT calibrates the guest's TSC (3599.5 MHz), the guest
takes the TSC as its clocksource, its timer delivers a hundred ticks a
second, and idle at its prompt the vCPU's task sleeps 99% of the time.

The first run there stopped twice where TCG had never gone, which is what a
run on the real thing is for:

- **CPUID leaf 0x80000001 went through whole but for SVM**, and a Zen 2 has
  MWAITX in it. The guest's `udelay` became `monitorx`, whose intercept had
  no answer. The leaf is an allowlist now, as leaf 1 is, and an intercepted
  instruction CPUID did not offer is answered with the #UD a CPU without it
  gives.
- **Linux on a Zen CPU reads AMD's FCH at a fixed address** -- the
  reset-status register, 0xFED803C0 -- whether or not there is one, and takes
  all ones for "no such device". A read of the platform's MMIO window that
  nothing answers is now answered with a read-only page of all ones; the
  `absent` built-in guest keeps that under TCG, which is no Zen and would
  never read it.

**The second run, 2026-09-24** (8928493, one boot of nos): every built-in
guest on CPU 0 and CPU 13 again, `asid` among them -- two VMs taking turns
on one CPU under ASIDs 1 and 2, and a third given 1 again after its
generation's flush, each reading its own page, on a CPU that keeps
translations; the exit costs above; and both distributions ([A
distribution](#a-distribution)) as they ship:

- **Alpine** at its login prompt 22 s after the VM starts. It calibrates
  its TSC against the emulated PIT (3600.143 MHz), takes the TSC as its
  clocksource and drives its tick through the PIT's one-shot mode 4
  (`hrtimer_interrupt`) -- the path TCG never takes by itself; `sleep 2`
  takes 2.00 s. On the switch it pings nos in 0.05 ms, and `ssh -p 2222
  root@65.109.93.213`, from the dev host over the internet, lands in its own
  sshd through nos's I210, `hv forward` and the switch.
- **Debian** at its login prompt 8.5 s after the VM starts (systemd-analyze:
  1.0 s kernel, 3.5 s userspace), `running`, no unit failed, its root on the
  3 GiB image on nos's `nosenv` ext2 -- which `e2fsck` found clean after --
  a file written there still there after `reboot`, and on the switch it
  reaches nos and the Alpine guest.
- Idle, the two cost nos 0.1% of its twelve CPUs (`top`), 560 and 700 exits
  a second.

And one thing only the real thing showed. Debian's clocksource watchdog,
which checks the TSC against jiffies, found them 76 ms apart over 512 ms,
called the TSC unstable and fell back to jiffies -- and then kept 25 s of
time in 62: at 250 Hz its idle vCPU, woken at the host's 100 Hz tick, was
given one timer edge for every two or three periods (a hundred a second),
the rest thrown away. Alpine had kept the TSC and its time. The PIT now owes
every period that elapsed and hands them over one at a time, as KVM's does
([What the extension costs](#what-the-extension-costs-while-it-is-on)),
and `hv-distro-test` checks each guest's idle clock against nos's: 1.000
with the fix, 0.514 without it under TCG. Over SSH the command line was
also cut at 255 characters, too short for Debian's `hv start`; it takes
1023, as `/etc/rc` does.

Booted again with the fix (768a56b), the same day: Debian keeps the TSC --
`Switched to clocksource tsc` at 1.1 s, no watchdog complaint -- and counts
62.37 s of 62.4, Alpine 62.27; both dates the host's. The two boots were
faster for it too, Debian at its login prompt in 3.8 s and Alpine in 8.9 s
(8.5 and 22 s before): the guests' own timers had been waiting on ticks
that never came. Debian's 343-character `hv start` went through over SSH;
an exit cost 742 ns with ASIDs and 959 ns flushed, as before.

The way out, on the AX41 the same day (45df223): with both guests on the
switch, NAT went out through the I210 from the machine's own address, and
each guest was handed Hetzner's resolver (185.12.64.1) by `ip=` and by the
switch's DHCP. Alpine resolved its mirror, `apk update`d from it, and
fetched 100 MB from Hetzner's Helsinki speed-test server in 0.93 s; Debian
took its address and route by DHCP, synchronised its clock over NTP, pinged
1.1.1.1 in 1.2 ms, `apt-get update`d over HTTPS (28.5 MB), and fetched
100 MB over HTTPS in 1.0 s and 1 GB in 16.6 s (65 MB/s). A connection from
another host to a mapped port was refused by nos's own stack and never
reached a guest. Over the session NAT carried 10,841 packets out and
943,241 back, and needed no next hop, frame or mapping it lacked.

Two things it showed that are not NAT's. `apt-get update` fetched those
28.5 MB in 39 s where one download of the same index took 0.2 s, and the
guest's port dropped some 1,400 frames meanwhile: apt downloads over several
connections at once while it decompresses, and a frame for a guest that is
running -- not halted -- waits in the port's 64-frame inbox until its next
exit. The switch woke a halted guest and had nothing to hurry a running
one; at line rate the inbox fills in under a millisecond. It kicks a running
one now, and the inbox is the guest's ring's size ([A network](#a-network)).
And a command run
through nos's sshd for longer than the client's keepalive (`ssh -o
ServerAliveInterval=`) was cut off, the server answering no keepalive while
the command ran; commands run beside their session now, which tends the
connection meanwhile ([sshd](sshd.md#the-shell-behind-it)).

How to repeat it -- the kernel, the modules and the guest on the machine's
`nosenv` partition, one boot of nos by `nosboot`, the shell over ssh -- is
in [Real hardware](real-hardware.md) for the machine and in the gate's own
header for the guest; the session's recipe is short:

```sh
# on the AX41, in Ubuntu: build, and put the modules and the guest beside the kernel
make nocheck && cp out/x86_64/modules/*.ko /nosenv/ && mkdir -p /nosenv/hvguest
# bzImage and initrd into /nosenv/hvguest/, then one boot of nos
nosboot -n && reboot
# from outside, once nos is up
ssh root@<box> 'insmod /hv.ko' ; ssh root@<box> 'hv on'
ssh root@<box> 'hv boot /hvguest/bzImage initrd=/hvguest/initrd secs=20 input=id\n cmdline=console=ttyS0 nolapic rdinit=/init'
ssh root@<box> reboot     # the one-shot is spent: back to Ubuntu
```

## What comes next

From [`plans/03-hypervisor.md`](../plans/03-hypervisor.md), in order, each a
thing that can be shown in half a minute:

1. ~~a VM object with nested page tables, and a guest of a few bytes that
   exits where it was told to~~ -- under AMD-V, `hv run exits`;
2. ~~a guest in long mode under NPT/EPT~~ -- NPT, `hv run hypercall`;
3. ~~an emulated 8250 on port I/O exits, and a `bzImage` printing its early
   console~~ -- `hv boot`, and the guest of [A Linux
   guest](#a-linux-guest) below;
4. ~~a full boot to a shell over that UART, with an initramfs, on one
   vCPU~~ -- with an initramfs, a BusyBox shell.

All four demos are done, and a guest runs a command typed at its console
(`hv boot ... input='id\n'` → `uid=0 gid=0`) -- under TCG and on the AX41's
real AMD-V ([On real hardware](#on-real-hardware)). Guests also run until
they are stopped, reached from the shell ([Guests that stay
up](#guests-that-stay-up)): the lifecycle the control plane will serve. What
is left, not in step order: the VMX backend with `CR0.NE` on every CPU.
The TLB is no longer flushed whole on every entry ([Address space
identifiers](#address-space-identifiers)), guests have disks and a network
over legacy virtio ([A disk](#a-disk), [A network](#a-network)) with a way
out through NAT ([The way out](#the-way-out-nat-dhcp-and-dns)), and a
distribution boots as it ships ([A distribution](#a-distribution)). Beyond
stage 3: a local APIC and an SMP guest, modern virtio, and the control
plane's HTTP API (stage 4).

Two constraints from stage 5 (live update) hold from the first line of it:
all VM state is serializable plain data -- the vCPU register set, every
emulated device, and one table mapping guest physical pages to the VM that
owns them -- and no emulated device holds a pointer into arbitrary kernel
memory, only indices and handles that survive a re-init. So far the VMCB
and the registers `vmrun` leaves to software are plain words, and guest
memory is regions of pages whose nested table is derived from them.
