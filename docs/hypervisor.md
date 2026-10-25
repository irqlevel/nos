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
fast. It is the whole of a backend now -- the VMCS, EPT, the launch stub and
the exit decoder -- and every built-in guest runs under it, brought up and
debugged under nested KVM on the Intel dev box (below). TCG has no VMX at
all, so the gate uses KVM there; on an AMD host `-cpu host` gives AMD-V, and
under TCG anywhere `-cpu max` gives AMD-V, so the two backends are covered
between the three.

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
the network among them, so it came with the VMX backend, where a guest shows
it working -- `boot64.asm`'s `enable_paging` sets NE beside where it clears
CD/NW after INIT, on the BSP and every AP, and `hv info` reads `host CR0/CR4
yes` on each.

### The VMX backend, and where it differs from AMD-V

The two backends are one shape above `hvarch`: `hv::vm::Backend` is `Svm` or
`Vmx`, and the run loop and the built-in guests call it without an `svm` or a
`vmx` in them. The guest state both keep is the AMD save-area layout
(`vmcb::Save` plus the general registers) -- for VMX a *shadow*, since the
real state lives in the VMCS and is reached only through `vmread`/`vmwrite`.
`Guest::run` in `hvarch::x86::vmx` syncs the shadow into the VMCS before an
entry and reads it back after, so the policy above -- CPUID, MSRs, the
loader -- is written once.

That sync is *lazy*, and has to be: a VMCS holds sixty-odd fields, and eight
segments of four fields each are most of them, so a full round-trip every
exit is sixty `vmread`/`vmwrite` -- and under a nested hypervisor, where each
one traps to the L0 kernel, that is most of the cost of running the guest.
But the guest owns nearly all of it: CR0/3/4, the segments, GDTR/IDTR, RSP,
RFLAGS it changes in the VMCS itself, with no exit (no CR or segment
interception), so the shadow need not carry them at all. The first entry
writes the whole state; after that only what the policy changes between
entries goes in -- RIP past an instruction, the system MSRs, the injected
event -- and only what it reads comes back: where the guest stopped, its
flags, and FS/GS base, which it *does* change through an intercepted `wrmsr`
(the per-CPU base, as this kernel keeps its own) as well as un-intercepted.
The control registers and segments are read only when a guest is being
stopped and dumped. A full sync every exit was ten times the work, and
turned a real distribution's boot from seconds into minutes. An EPT
violation, which is mostly an MMIO access performed for the guest
([MMIO](#mmio)), reads what performing it takes and no more: CR0, CR3 and
CR4 for the walk to its instruction, CS's rights for the mode (and CS's
base outside 64-bit code), RSP, the CPL. One that ends the guest after all
has the rest read for its report while the VMCS is still current on that
CPU (`Guest::read_whole_state`), and a report that could not read it says
so, rather than show the shadow's older values as the guest's.

And within the light set it is *dirty-tracked*: `Guest` keeps what the VMCS
last had of each field it may write between entries (`Synced`) and writes
only what the policy changed -- RIP after a skip, the interruptibility a
skip clears, a system MSR after a `wrmsr`, the interrupt-window control when
it toggles, the injected event when there is one (the CPU clears the field's
valid bit as it takes the event, so with none there is nothing to write).
That is exact for every field the guest cannot change without an exit. The
FS and GS bases it *can* -- `wrfsbase`, `swapgs` -- so a copy older than the
last exit must never be written back (Alpine wedged on exactly that once):
they are read on the exits where the policy may touch them, `rdmsr` and
`wrmsr`, and what it leaves is compared with that read alone. On the way out
the exit reason is read first and the rest by reason: the host's interrupt
needs nothing more, an instruction its length, port I/O its qualification
too, and only an exit that may have happened during an event's delivery --
an exception, an EPT violation -- or one nobody foresaw reads the
interruption and vectoring words and the guest-physical address. Twelve
`vmwrite`s and fourteen `vmread`s an exit became one or two and four.

The differences that are not hidden by the shadow at all:

- **The VMCS is opaque, and per CPU.** It is made as plain memory (a
  privileged instruction at construction would fault, since a VM is made
  where VMX may be off and on a CPU that will not run the guest); the first
  entry `vmclear`s it once, where VMX is known on, into the clear launch state
  `vmlaunch` needs. From there the VMCS is left *current* after an exit, not
  `vmclear`ed, and the next entry on the same CPU is a `vmresume`, which does
  not reload it -- **8.6% fewer nanoseconds an exit under nested KVM** (18.6 ->
  17.0 µs, `hv bench exits=50000` on the Intel dev box), where a `vmclear`
  flushes the whole shadow VMCS, and proportionally more on real silicon,
  where it is a memory flush the earlier per-exit `vmclear` paid every time.
  The one invariant a resume needs is that a VMCS is never current on two CPUs
  at once, kept two ways: before a guest's task runs on another CPU its VMCS
  is `vmclear`ed off the old one, and when a guest is dropped its VMCS is
  `vmclear`ed off its CPU before the page is freed -- each a `vmclear` run on
  that CPU by an IPI sent from task context, never with interrupts off, that
  waits for the CPU to answer. Without the drop one a freed VMCS page would
  take a `vmptrld`'s write of cached state on that CPU's next entry; the
  migration one is a safety net, since a guest's task is pinned to one CPU and
  does not in fact move. Nested KVM will `vmlaunch` an uncleared VMCS, so the
  first-entry `vmclear` and this whole ordering are for the hardware, not the
  gate. (The AMD VMCB is plain memory and had none of this to arrange.)
- **Host state is the hypervisor's to save.** AMD-V's `vmsave`/`vmload` move
  the host's segments and MSRs around `vmrun`; VMX restores the host from the
  VMCS host area, which `HostRegs::capture` fills on the CPU the entry runs
  on -- its CR3, its GS base (where its per-CPU data is), its TR, its GDTR
  and IDTR, and the host's five syscall MSRs into the exit MSR-load list
  (five `rdmsr`s, and under nested KVM four exits to it, that were paid on
  every entry before). Rewritten when the guest's task has moved CPU, not
  every entry -- and only then is the VMCS `vmptrld`ed: each CPU remembers
  which VMCS is current on it (`CURRENT_VMCS`, kept exact by the VMPTRLD,
  VMCLEAR and VMXOFF that are the only things to change it), and an entry
  whose VMCS is current already skips the instruction. The CR4 the exit
  loads back from the host area has to be the one an entry runs under, with
  OSFXSR: captured without it -- as it once was, the bit set around each
  entry and captured before -- the FXSAVE after the exit could leave the XMM
  registers out and the XSETBV after it was an undefined opcode. Now the
  bit is the CPU's for as long as VMX is on for it (below), and the capture
  is right by construction.
- **The host's NMI is delivered by hand.** Under AMD-V an intercepted NMI
  stays pending until `stgi` and the host takes it then; under VT-x the exit
  *is* its delivery -- the NMI is consumed, and NMIs stay blocked until an
  IRET. So an NMI exit ends with `int 2` into the host's own handler, whose
  IRET unblocks the next, the way `#MC` is raised with `int 0x12` on both
  sides and the way KVM does it. What would otherwise be lost is the panic
  path collecting this CPU's stack from another (`Cpu N did not answer the
  NMI`, exactly when the backtrace was wanted) and the profiler's counter
  overflow, after which its LVT stays masked and the profiler is dead on
  that CPU.
- **CR2 is nobody's on Intel.** `vmrun` keeps a guest CR2 in the VMCB; VMX
  keeps none, so `run` saves the host's and restores the guest's around the
  world switch by hand. The x87/SSE and XCR0 switch is the same as AMD-V --
  neither extension switches it, and the guest is given x87 alone.
- **CR8 is the host's on Intel.** In 64-bit mode CR8 is the local APIC's
  task priority register, and under VT-x a guest's `mov cr8` reaches the real
  one unless told not to: 15 there keeps every interrupt but an NMI off the
  host's CPU -- the tick, the kick, the IPI a TLB shootdown waits for, which
  would panic the machine ten seconds later -- for as long as the guest
  likes, and after it has gone, since the exit restores no TPR. AMD-V gives
  the guest `V_TPR` under `V_INTR_MASKING` and never exits. So the VMX
  controls stop the guest at every CR8 access (`Exit::Cr8`), and the policy
  answers from a shadow TPR of the guest's own, a #GP for a value CR8 cannot
  hold. The `tpr` built-in guest checks both backends.
- **CR0's PE and PG are the host's to watch.** Every entry checks that the
  "IA-32e mode guest" control agrees with EFER.LMA and that CR0 is one VMX
  allows, so a guest that turned paging off or on behind the host's back
  would fail its next entry. So PE and PG are in CR0's guest/host mask,
  with NE and the bits VMX fixes: a `mov` to CR0 or an `lmsw` that would
  change one of them exits (`Exit::Cr0Write`), and the policy does what the
  CPU would -- a #GP where the architecture has one, EFER.LMA set or
  cleared as paging goes on or off with LME set, the entry control made to
  follow at the next entry, and the translations cached under the guest's
  VPID dropped. Every CPU a start-up IPI starts climbs from real mode this
  way, which also takes *unrestricted guest*: a CPU without it runs guests
  of one CPU only ([More than one CPU](#more-than-one-cpu)).
- **An NMI waits for the guest's IRET.** One of a guest's CPUs can send
  another an NMI, and a second must wait until the handler of the first
  has returned. Under VT-x that is the "virtual NMIs" pin control: the
  guest's NMI blocking is its own, set as one is injected and ended by its
  IRET, and the NMI-window exit comes the moment a held one can go in.
  AMD-V has no such control; it intercepts the IRET instead, whose exit
  comes before the IRET has run, so the blocking ends once the guest's RIP
  has moved past it -- what KVM does.
- **The syscall MSRs are switched through load lists.** `STAR`, `LSTAR`,
  `CSTAR`, `FMASK` and `KERNEL_GS_BASE` are not VMCS fields, and a guest run
  with the host's would be catastrophic -- a userspace `SYSCALL` jumps to the
  host's `LSTAR`, and it was exactly this that a real Linux oopsed on at
  `RIP: 0x0` the moment it ran `/init`, the kernel itself having booted whole
  in ring 0 without them. AMD-V moves these with `vmsave`/`vmload`; VMX has
  no such instruction, so `Guest` keeps a VM-entry MSR-load list (the guest's
  values, from the shadow) and a VM-exit MSR-load list (the host's, off the
  CPU), and the CPU loads each in turn. `KERNEL_GS_BASE` needs one more list:
  `swapgs` changes it without a `wrmsr` the host hears, so a VM-exit MSR-store
  list saves the guest's live value back into the shadow -- otherwise the
  guest's `swapgs`-established kernel GS base is lost each round, and the next
  `swapgs` returns junk. That was a real distribution kernel double-faulting
  on a garbage RSP the moment an interrupt returned to user mode; a
  purpose-built guest that never took one from user mode did not show it.
  With all three lists, an unmodified Linux -- a tinyconfig to a BusyBox
  shell, and a full Alpine 3.24 to a root login with its clock, `apk`, disk
  and network -- runs under VT-x.
- **Controls, and one bitmap.** Every port exits by the processor-based
  controls (unconditional I/O exiting), not by the 12 KiB `iopm` a VMCB
  points at; every MSR exits by a 4 KiB bitmap with every bit set but three
  -- the FS, GS and KERNEL_GS bases, the guest's own and nobody else's,
  which VT-x saves and loads itself (two VMCS fields, and the exit MSR-store
  and entry MSR-load lists) and which a Linux guest writes at every context
  switch: two exits a switch, the most frequent a busy guest made, gone --
  on both sides, since the AMD `msrpm` lets the same three through, the
  stub's `vmsave` and `vmload` carrying them (`hvarch::x86::PASSTHROUGH_MSRS`
  is the one list). The rest of AMD-V's
  intercept set is controls too, since VT-x stops a guest at none of them
  unasked: CR8 (above), MONITOR, MWAIT and RDPMC, which CPUID says the guest
  has not got and which run on the host's CPU otherwise; WBINVD, which
  otherwise writes back and drops the package's whole shared cache -- every
  core stalled for milliseconds -- as often as the guest cares to; and
  PAUSE, so that a guest spinning comes out to be handed what is due to it
  ([Time](#time)); CPUID,
  INVD, VMCALL, XSETBV and a triple fault exit unconditionally, and RDTSCP,
  INVPCID and XSAVES are `#UD` in the guest because the secondary controls
  that would enable them are off. Each control field is written through
  `adjust`, which forces on the bits `IA32_VMX_*_CTLS` says must be 1 and
  off the ones it forbids -- a value that ignored them fails entry with
  nothing named. Guest CR4.VMXE is forced set (the fixed MSRs demand it) and
  masked to read 0, since the guest is told it has no VMX.
- **A refused entry is the CPU's `VMEXIT_INVALID`, not a software check.**
  AMD-V's VMCB is checked in software first (`Vcpu::check`) so a bad one is
  named before the CPU sees it; VMX has no such check -- a bad guest state is
  the CPU's to refuse, at `vmlaunch`, reported as an entry failure and shown
  as `Exit::Invalid` with the instruction error. An event an exit interrupted
  the delivery of is given back rebuilt from its vector, type and error code,
  not copied: bit 12 of what the exit wrote is undefined, and the entry field
  wants bits 30:12 clear.
- **VPIDs, and INVEPT and INVVPID once per CPU a guest runs on.** A VPID is
  VT-x's ASID: the tag the TLB keeps a guest's linear and combined mappings
  under, so that a VM entry or exit need drop neither the guest's nor the
  host's, which are tagged 0. Without one -- as the backend first ran --
  every transition dropped both, the host's included: every exit cost the
  host its translations, as every AMD-V entry did before ASIDs (-22% to -34%
  of an exit on the AX41). Here a VPID is the guest's for its life, the
  lowest free one from a lock-free bitmap of 65535 (`vpid_take`), given back
  when the guest is dropped; AMD's ASIDs are per CPU and per generation, and
  the third VM of the `asid` built-in guest, under VT-x, is checked to have
  been given a VPID one of the first two had. What must then never happen is
  a number's translations from an earlier holder served to the next: a
  guest's first entry on each CPU runs a single-context INVVPID for its
  number, and `enable` an all-context one after `vmxon`, for what an earlier
  load's guests left. EPT has the same hazard one level down: *guest-physical*
  mappings, the EPT's own translations, which the CPU tags with the EPT's
  address and keeps across transitions and across VMXOFF and VMXON. A guest destroyed
  frees its EPT; the next guest's EPT is likely made in the same page (the
  allocator hands back what it was last given), so its tag is the old one's,
  and the CPU would serve the old guest's translations -- to pages that are
  the host's again -- for the new guest's addresses, which are the same low
  addresses every kernel touches first. Nested KVM keeps its own shadow EPT
  and shows none of this. So `enable` follows `vmxon` with an all-context
  INVEPT, dropping what an earlier load's guests left, and a guest's first
  entry on each CPU (the moment its host state is captured there) is
  preceded by a single-context INVEPT for its EPT -- once per CPU per guest,
  which costs nothing, and after which the only translations under that tag
  are this guest's, an EPT that only grows never making a stale one. `usable`
  requires the instruction. This is the counterpart of the AMD side's ASID
  generations, which begin run out at every load for the same reason.

The `hypercall` built-in guest is the one whose *machine code* is a vendor's:
`vmmcall` (`0F 01 D9`) is AMD's and an invalid opcode on Intel, so the loader
patches it to `vmcall` (`0F 01 C1`) where the guest runs under VT-x. The
`refused` and `asid` guests test AMD-only mechanisms, and each has a VMX form
that tests the Intel equivalent -- the CPU refusing a non-canonical guest
RIP, and three VMs isolated by their EPTs; `tpr` runs unchanged, and is
stopped twice under VT-x and not at all under AMD-V.

What AMD-V's [entry keeps whatever the VMCB says](#what-entering-a-guest-cannot-be-told-to-do),
`hvarch::x86::vmx::Guest` keeps whatever the policy asked -- its own way,
since VT-x's controls are not AMD-V's intercepts:

- **CR4.MCE is the host's.** The CR4 guest/host mask holds MCE and VMXE
  whatever `hv` asks, and every bit `hv` does not let the guest own
  (`Guest::new`'s `cr4_own`: VME, PVI, TSD, DE, PSE, PAE, PGE, PCE, OSFXSR and
  OSXMMEXCPT, where its CPUID offers them): the VMCS's CR4 keeps MCE set and
  the guest reads the read shadow's. A `mov` to CR4 that changes a bit of
  the mask exits, naming its register and length, and `crate::cr` performs
  it from there -- a bit its CPUID does not offer the #GP a CPU without the
  feature gives, so PKE, and with it PKRU, stays the guest's to have only
  where it is offered -- and writes the register and the shadow at the next
  entry. A read never exits, the shadow answering it, and a write of the
  guest's own bits never does either: Linux's global TLB flush turns PGE
  over twice, with no exit, where AMD-V stops every `mov` of CR4. PAE
  cleared in long mode is then the CPU's own #GP.
- **The debug registers are the guest's.** Every `mov` of one exits ("MOV-DR
  exiting"), named by the exit -- register, direction, general-purpose
  register -- and `crate::dr` answers it: DR0-3 from four of the guest's
  own, made the CPU's before each entry through the same per-CPU record AMD-V
  uses (`svm::LoadedDebugAddrs`); DR6 from the guest's own, which is no VMCS
  field and is never put in the register -- every read of it exits -- and
  DR7 the VMCS's, which "load debug controls" puts in the register at the
  entry and the exit takes back out, leaving the host's with all four off.
  DR4 and DR5 are DR6 and DR7 again, or a #UD under CR4.DE; a `mov` above
  CPL 0 the #GP, under general detect the #DB, and a DR6 or DR7 written with
  a bit above 31 the #GP, which the exit comes before the CPU's check of.
- **A #DB of the guest's own goes back to it.** VT-x writes no DR6 at a #DB
  that exits -- the exit qualification has what it would have written -- so
  the exit puts it into the guest's DR6 as the CPU would have (B0-B3 cleared
  first, RTM's bit the other way up; KVM's `kvm_deliver_exception_payload`),
  and the run loop gives the #DB back with GD cleared from DR7, as a CPU
  entering the handler clears it; a single step taken out of a STI or MOV SS
  shadow with TF set is marked pending, which the CPU does at every exit but
  this one and the entry checks. ICEBP's #DB exits at the instruction, as a
  privileged software exception: stepped past, and its #DB given as the trap
  it is, as AMD-V's intercept of it is.
- **The controls the host needs are required.** `adjust` drops a control the
  CPU -- or a hypervisor under this one -- does not allow; one the host
  cannot do without dropped refuses the guest, naming the field and the bits
  (`Refusal::Controls`): the exits for its interrupts and NMIs, its ports,
  CR8, MONITOR, MWAIT, RDPMC, every `mov` of a debug register and WBINVD,
  nested paging, the 64-bit host, the switch of EFER, the PAT and DR7 -- and
  what the guest was promised, unrestricted guest and its VPID. A guest run
  without one of them would be the host's to lose.
- **The world switch keeps a guest's speculation off the host**
  (`hvarch::x86::spec`), what CPUID and IA32_ARCH_CAPABILITIES say decided
  for each CPU as the extension is turned on there, with KVM's as the
  reference: after every exit the guest's values cleared out of every
  register the stub returns with and the return stack refilled before its
  first `ret`, as AMD-V's stub does; an IBPB between two guests on a CPU;
  IBRS in IA32_SPEC_CTRL while the extension is on -- enhanced, set once, on
  the EX44, and legacy, a barrier only when written, written again after
  every exit on a CPU without it (Skylake) -- with BHI_DIS_S beside it where
  the CPU has the control; VERW, the CPU's buffers cleared, the last thing
  before every entry where MDS, TAA or RFDS apply (the EX44's E-cores have
  RFDS); and the L1 data cache flushed before every entry where L1TF lets a
  guest read it (Skylake). `hv` says which of them the CPU it runs on has,
  and reads every CPU's IA32_SPEC_CTRL back, warning where it no longer
  has what `hv on` set there. What each costs an exit on the EX44 is in
  [What an exit costs](#what-an-exit-costs).

What the VMX backend does not do yet, and says so rather than pretends:

- **Branch history cleared by hand.** A CPU with enhanced IBRS that is open
  to branch history injection and has no BHI_DIS_S -- Ice Lake, Tiger Lake,
  or an Alder Lake and later under a hypervisor that does not pass the
  control through, as QEMU 8.2's `-cpu host` does not -- wants the software
  sequence that clears the history at every exit; `hv` names such a CPU
  instead. None of the three machines here is one.

## What it does today

    hv                          what the machine has, and which CPUs it is on for
    hv info                     the whole of what the CPU said, a line a feature
    hv on [cpu|all]             turn the extension on
    hv off [cpu|all]            turn it off
    hv run <guest|all> [cpu]    run a built-in guest, or all of them, on a
                                task of its own -- bound to cpu when one is named
    hv boot <kernel> [mem=MiB] [cpus=N] [secs=N] [cpu=N] [xapic] [ioapic] [apicv|noapicv] [rxmod=US] [nooffload] [initrd=path] [disk=path[:ro]]... [input=...] [cmdline=...]
                                load a Linux bzImage -- or a PVH ELF, such as
                                hvboot, which boots the guest's own kernel
                                -- and run it on a vCPU -- on cpus of them,
                                each on a host CPU of its own -- for secs,
                                then print its console and how it ended
    hv start <kernel> [mem=MiB] [cpus=N] [cpu=N] [xapic] [ioapic] [apicv|noapicv] [rxmod=US] [nooffload] [initrd=path] [disk=path[:ro]]... [input=...] [log] [restart] [net] [cmdline=...]
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
    hv stop <id|all> [secs=N]   press its power button and give it secs (30)
                                to turn itself off, then stop it; say how it
                                ended, take it off the list
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
off needs no page. And on Intel it checks one thing more: that no guest's
VMCS is current on the CPU. A VMCS is left current after an exit (the next
entry resumes it), and VMXOFF under one leaves the CPU's cached copy of it
nowhere good -- the manual wants every active VMCS `vmclear`ed first -- so
`hvarch` counts the VMCSs current on each CPU, up where one is made current
and down where it is cleared off, and `vmxoff` is refused while the count is
not zero: the CPU stays on, keeps its page, and `hv off` names it (`a guest's
VMCS is still current on cpu 3 -- left on; stop the guest first`). The CPU
clears its own slot in the host-area table as it goes off, in the same IPI,
so there is no moment at which an entry is turned away from a CPU that then
stays on. `rmmod` never meets this: it drops every guest before it turns
anything off, and a dropped guest's VMCS is current nowhere.

## A guest

A VM is guest memory behind a nested page table, the permission maps it runs
under, and one CPU (`hv::vm::Vm`) -- or, for a Linux guest, as many as it is
given ([More than one CPU](#more-than-one-cpu)). A guest's CPU is a task:
`hv run` spawns one, bound to a CPU when asked, and waits for it, and the
task enters the guest over and over from task context, handling each exit
between entries.

### Its memory

`hv::GuestMemory` is regions of guest physical addresses, each backed by
host pages that are mapped nowhere (`kcore::frame::Frame`): pages of RAM the
kernel hands out by their address and puts in no table of its own, so that
a guest of any size costs pages and no kernel address space, and the
guest's nested table is the only mapping they have. They are zeroed before
the guest can see them, so that what a guest finds in its memory is what it
was given and never what the host left there.

Two rules make it the one place that decides what a guest can reach:

- **Nothing hands out a reference into it.** The guest writes its memory
  whenever it runs, on whatever CPU, and a Rust reference to memory that
  changes under the compiler is undefined behaviour however it is used
  ([`plans/01-rust-strategy.md`](../plans/01-rust-strategy.md) calls this
  the most important decision of the hypervisor). Every access is a copy --
  `read`, `write`, `read_obj::<T: Pod>`, `write_obj` -- bounds-checked
  against the region it falls in, and made by the kernel through its
  temporary window onto each page it touches -- the CPU's own slot of it,
  mapped for that page's part of the copy and nowhere after -- a page's part
  one call of the architecture's memcpy, which the compiler sees nothing
  of; an address a guest gave, whatever it is, is at worst `Unmapped`.
  A queue's rings are the one exception to the window, and not to the
  rule: a device reads and writes them at every request, and a copy maps
  and unmaps a page -- a TLB flush -- at every access, a dozen a frame. So
  they are mapped while the queue is up (`GuestMemory::map`, three pages
  a queue, through `Mm::MapPages`) and still reached a value at a time --
  one volatile load or store at a checked, aligned offset, an index with
  acquire or release (`kcore::frame::Mapped`). The mapping holds a share of
  its region's frames (`kcore::frame::Run`), so none is freed while it is
  mapped, whichever of the guest and its devices goes last. Undoing one
  shoots its address down on every host CPU, so a device makes eight and
  then one a second (`virtio::Maps`): a guest that set its queues up over
  and over would otherwise have every CPU of the host interrupted at will,
  and past the budget its rings are copied, as before, and the report
  says so. On the EX44, between two guests of two CPUs: `dd | nc` 2.72 s
  where it took 4.15, and 2.83 where it took 4.45 with the offloads, five
  runs of each; BusyBox's `httpd` serving 100 MB five times over 0.29 s
  where it took 0.47 -- 1.8 GB/s -- and 0.62 where it took 1.63-1.68
  without the offloads. A dozen pages mapped and flushed a frame had been
  most of what the host spent on one.
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
- CR4.MCE set in the CR4 the guest runs with, and the intercept of every
  write of CR4, so that the guest cannot clear it in the register either:
  with MCE clear a machine check is no exception but a shutdown -- the
  guest's triple fault, as the exit has it -- and the host never hears of
  it. What the guest believes CR4 holds is the policy's to keep: `hv`
  intercepts its reads of CR4 as well, and answers them, and performs its
  writes, from the CR4 the guest wrote (`hv/src/cr.rs`, `crate::cr`) -- the
  instruction fetched by the guest's paging and decoded, as an MMIO
  access's is, since a CPU without decode assists (QEMU's, a nested
  hypervisor's) does not name the register. A write of a bit the guest's
  CPUID does not offer is the #GP a CPU without the feature gives, so PKE,
  LA57, OSXSAVE and CET stay off; one a CPU would not take
  where the guest is -- PAE cleared in long mode, PCIDE with a PCID in CR3 --
  is a #GP too; and a write that changes a bit a CPU translates by flushes
  the guest's TLB, as the CPU would. QEMU's TCG never stops a guest at a
  read of CR4, whatever the VMCB asks: its decoder checks a read of any
  control register against CR0's read intercept, adding the register's
  number to the exit code only when the code is not 0 -- and CR0's read is
  (`decode_op`, `target/i386/tcg/decode-new.c.inc`, QEMU 10.2). There the
  guest reads the register, MCE in it, and its writes are the host's all
  the same. Intercepting CR0's reads too would catch them there, and on a
  real CPU catch SMSW, which a guest's user process may run;
- what of the guest's speculation the world switch can keep from the host
  and from the next guest (`hvarch::x86::spec`): after the exit, the
  guest's values cleared out of every register the run stub returns with,
  and the return stack refilled before the host's first `ret` -- a guest's
  return addresses left there would have the host's returns predicted to
  them; before the entry, an IBPB where the guest is another than the last
  on the CPU, told apart by its nested table's id, where the CPU has IBPB;
  and automatic IBRS turned on with the extension, where the CPU has it;
- the intercept of every read and write of DR0-DR3, and the CPU's four made
  the guest's own before each entry. The breakpoint addresses are the
  CPU's, not the VMCB's -- `vmrun` switches DR6 and DR7, not them -- so
  without both a guest would read whatever the last guest on its CPU left
  there, and a breakpoint it set would be moved by the next guest's write,
  or left behind when its vCPU moved to another CPU. Each `Guest` keeps its
  own four, from which the policy answers the guest's `mov`s and into which
  it performs them (`hv/src/dr.rs`, `crate::dr`, the instruction decoded as
  CR4's is); its own DR7, which `vmrun` loads, arms them. A record per CPU
  of what its four hold (`LoadedDebugAddrs`) has an entry write only the
  ones that differ -- nothing else in the kernel writes them, the guest
  cannot, and neither `vmrun` nor `#vmexit` does -- so a guest that never
  sets a breakpoint costs no write at all, where four writes of a debug
  register, each serializing, would cost every entry. A `mov` above CPL 0
  is the #GP a CPU gives, and one under DR7's general detect the #DB --
  DR6.BD set, GD cleared, the move not made -- where a CPU checks either
  after the intercept rather than before. QEMU's TCG, whose slip with CR0's
  read code is above, adds the register's number to a debug register's
  code, which is never 0: there the intercepts are each register's own;
- physical interrupts masked by the host's flag, not the guest's
  (`V_INTR_MASKING`), so a guest sitting with interrupts off cannot keep the
  host's out;
- nested paging on, over the table the caller's memory owns; SEV, AVIC and
  virtual VMSAVE off, since each has the CPU work at physical addresses the
  VMCB gives it;
- I/O and MSR permission maps with every bit set but the three of the
  guest's own base MSRs (`hvarch::x86::PASSTHROUGH_MSRS`, which the stub's
  `vmsave`/`vmload` carry), and no way to clear another: a port the guest
  reaches directly is one of the host's devices;
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
ASIDs out itself, the way KVM does -- `Asids` in `hvarch/src/x86/svm.rs`,
the CPU's own, in the one `CpuLocal` both `Guest::run`s take once an entry
with interrupts off (`Entries` in `hvarch/src/x86.rs`, beside the CPU's
debug addresses and its speculation state):

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

Each is a handful of instructions, assembled once -- with NASM, or with
clang for `mmio` and `ioapic` -- and kept in `hv/src/guests.rs` as bytes
beside the source they came from, run in a VM of its own and checked
against what it was told to do. All start in long mode
at CPL 0 with paging on, 1 MiB of memory with a GDT, a TSS and a page table
in it that maps the first 2 MiB to themselves -- the second of them with no
memory behind it -- and 1 GiB to guest physical 4 GiB; all but the second
CPU of `smp`, which starts where a start-up IPI starts every CPU, in real
mode:

| Guest | Does | Shows |
|---|---|---|
| `exits` | reads and writes the debug port 0xE9, asks CPUID a leaf only this hypervisor answers, writes what it got to its memory | port I/O both ways; CPUID answered by the host; the answers read back out of guest memory |
| `hypercall` | writes through its own page table to guest physical 4 GiB, puts a value of its own in every register, makes a hypercall, writes every register the host answered to its memory | long mode under nested paging, above 4 GiB; the run stub's register save and restore, all 15 registers both ways |
| `uart` | brings up an 8250 the way a driver does -- divisor behind DLAB, 8N1, FIFO, a scratch-register presence test -- and sends a line polled out of LSR.THRE | the emulated serial port, over port I/O alone |
| `fault` | loads a null DS, then writes to 0x1FF000, which its page table maps and the nested one does not | stopped at the nested table, at that address and that instruction: a guest reaches nothing it was not given -- and the state its report shows is the guest's at the fault, the null DS in it, which under VT-x only reading the whole state for the report gives |
| `absent` | reads AMD's FCH reset-status register at 0xFED803C0 twice, then writes to it | a read of a device the platform does not have finds all ones -- what Linux on a Zen CPU reads there -- through one read-only page; the write stops the guest |
| `mmio` | stores to and loads from 32 bytes of a device's registers in the MMIO window, by every form of `mov` Linux's accessors are -- an immediate of each size, REX's registers and a high byte, `movzx` and `movsx` | each access's instruction fetched through the guest's paging, decoded and performed, every value where it belonged ([MMIO](#mmio)) |
| `triple` | `int3` with no IDT | a triple fault stops the guest, not the CPU |
| `refused` | starts with CR0.NW set and CD clear | a VMCB that breaks a rule is refused before the CPU sees it, the rule named |
| `spin` | `cli; jmp $` | the host's interrupts still get through -- about a hundred a second -- and the host stops it when its 300 ms are up |
| `tpr` | writes 15 to CR8 -- the task priority register, which masks every interrupt priority -- and reads it back | the guest gets a shadow TPR of its own (AMD-V's `V_TPR`; under VT-x the virtual APIC's VTPR, or where the CPU has none, a `mov cr8` stops the guest and is answered from a shadow), and the host's CR8, read afterwards on the CPU the guest ran on, is still 0: a guest cannot hold the host's interrupts off its CPU |
| `asid` | three VMs on one CPU read a page of their own at one address: two taking turns, and a third given an ASID one of them had | no guest reads another's page through the TLB, and a reused ASID is reused after a flush ([Address space identifiers](#address-space-identifiers)) |
| `smp` | two CPUs, on two host CPUs: the first sends the second INIT and a start-up IPI; the second comes up in real mode at the vector's page, climbs through protected mode to long mode, writes down its x2APIC ID and CPUID's, and sends the first an IPI; the first, woken by it, arms its APIC timer for one shot and halts until it fires | an application processor started the way Linux starts one, an IPI between two CPUs, and the local APIC timer ([More than one CPU](#more-than-one-cpu)) |
| `ioapic` | on the machine of `smp` with an IO-APIC, its first CPU alone, in xAPIC mode: reads the IO-APIC's version, then wires the serial port's pin to itself -- as an edge, the THR-empty interrupt enabled and a byte written, then one written while masked; as a level, its handler ending it without reading IIR twice, and the third time reading it | edges sent as the line rises and lost while masked; a level sent again at each EOI for as long as its line is up, its remote IRR set until then; every IO-APIC access and every EOI a decoded MMIO access ([The IO-APIC](#the-io-apic)) |
| `posted` | on the machine of `smp`, asking for its interrupts to be the CPU's: the second CPU, started as `smp`'s is, sends the first a thousand fixed IPIs, one at a time, while the first spins with interrupts on and no HLT, counting them | where the CPU has posted interrupts each is posted and notified and taken by the first CPU in its guest, which leaves it a handful of times rather than once an IPI; elsewhere the host hands each over ([Interrupts the CPU delivers itself](#interrupts-the-cpu-delivers-itself-vt-xs-apic-virtualization)) |
| `apicv` | on the machine of `ioapic`, its first CPU's local APIC taken to x2APIC mode by the base MSR: with CR8 at 5 sends itself 0x50 and 0x30 and reads both in IRR; with CR8 at 0 takes 0x50, then 0x30, whose handler sends itself 0x70 and takes it nested, each handler reading ISR; then the serial port's pin through the IO-APIC as a level, three times, each ended by the x2APIC's EOI; and ISR read whole at the end | the same to the guest whoever delivers its interrupts: under VT-x with APIC virtualization the CPU delivers all six and takes their EOIs, stopping the guest after the three level-triggered ones for the IO-APIC; elsewhere the host injects them ([Interrupts the CPU delivers itself](#interrupts-the-cpu-delivers-itself-vt-xs-apic-virtualization)) |
| `debug` | one CPU, through the run loop a Linux guest's go through, with an IDT of its own: sets TF with POPFQ and single-steps a NOP, then runs an ICEBP (`int1`) | each a #DB of the guest's own -- intercepted for the delivery loops a guest could make of it -- given back to its handler, the single step's with DR6.BS -- under VT-x, which writes no DR6 at a #DB that exits, the DR6 its exit said, and every `mov` of DR6 the host's |
| `dr` | two VMs of one CPU each, one after the other on the CPU the guests run on, through the run loop a Linux guest's go through, with an IDT of its own: each reads DR0-DR3, writes a pattern of its own into them through four registers and reads it back through a fifth, then points DR0 at a `ret` of its own -- each VM's elsewhere -- arms it in DR7 for execution and calls it, its #DB handler turning the breakpoint off | every `mov` of DR0-DR3 the host's: the second VM reads 0 where the first left its own, each reads back its pattern, and each one's breakpoint fires -- DR6.B0, at its `ret` -- which only its own DR0, made the CPU's before its entry, can do. An execution breakpoint and not a data one: QEMU 10.2's TCG never gets past a store a data breakpoint watches in a nested guest, retrying it for good with no interrupt taken, whoever's DR0-DR3 are the CPU's ([What entering a guest cannot be told to do](#what-entering-a-guest-cannot-be-told-to-do)); under VT-x, which stops a guest at every `mov` of a debug register, the same, its DR6 and DR7 the host's too |
| `cr4` | one CPU, through the run loop a Linux guest's go through, with an IDT of its own: reads CR4, writes PAE alone -- MCE cleared -- then PAE and PKE, 0, PAE and PGE through R9 (read back through R13), and bit 32, reading CR4 after each | every `mov` of CR4 performed by the host: the guest reads back what it wrote while the CPU's CR4 keeps MCE, and PKE -- which its CPUID never offers -- PAE cleared in long mode and a reserved bit each a #GP, CR4 as it was ([What entering a guest cannot be told to do](#what-entering-a-guest-cannot-be-told-to-do)) -- the last two, which no CR4 takes in long mode, the CPU's own #GPs before its intercept where it makes them so, as the AX41's Zen 2 does under KVM; on QEMU's TCG, which stops no read of CR4, its reads are the register's, MCE in it, and the writes the host's all the same; under VT-x its reads are the read shadow's, and three of its writes -- those that change a bit the host keeps -- the host's, PAE cleared the CPU's own #GP |

```
$ hv run all 3
...
hv: guest fault -- a write to memory its page table maps and the nested one does not
  ran on     cpu 3, 51 us
  exits      0 port in, 0 port out, 0 cpuid, 0 hypercall, 0 host interrupt
  stopped    nested page fault at gpa 0x1ff000, error 0x100000006, rip 0x8009
  checked    stopped at the nested table, at the address and the instruction it was told to, its state read whole
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
hv: guest smp -- two CPUs: the first starts the second -- INIT and a start-up IPI, into real mode -- which climbs to long mode and sends it an IPI; then its APIC timer
  ran on     cpu 3 and 0, 18192 us
  exits      cpu 0: 15 (9 wrmsr, 2 hlt, 2 apic irq); cpu 1: 6 (2 wrmsr, 0 hlt)
  stopped    both CPUs halted with interrupts off, the last at 0x80c3
  checked    cpu 1 started by INIT and a start-up IPI, came up in real mode and reached long mode, x2APIC ID 1 and CPUID's the same; its IPI reached cpu 0, whose one-shot APIC timer then ran out and interrupted it
hv: guest smp ok
hv: guest ioapic -- an IO-APIC, through its page: the serial port's pin as an edge, then as a level ended by EOI and sent again while its line stays up
  ran on     cpu 3, 23669 us
  exits      50 (1 port in, 21 port out, 22 MMIO accesses performed, 3 hlt); the IO-APIC sent 5, 3 of them levels, 3 ended by EOI
  stopped    cpu 0 halted with interrupts off at 0x80d4, cpu 1 never started
  checked    version 0x20's 24 pins; two edges taken, none while masked; three levels, each sent again at its EOI while the line stayed up, its remote IRR set in service and clear after
hv: guest ioapic ok
hv: 15 of 15 guests ok
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
- **The x87 and SSE state is the guest's, switched at the exit and, when
  it has to be, at the entry.** FXSAVE after every exit into an area each
  guest owns; FXRSTOR before an entry only when the CPU's registers are not
  this guest's already -- each CPU remembers whose FXRSTOR was the last on
  it (`fp::OWNER`, by a number no two guests ever share), and the host uses
  none of the registers (its C++ is built without SSE and x87, its Rust is
  soft-float), so between a guest's exit on a CPU and its next entry there
  they hold what its FXSAVE saved unless another guest's entry came between.
  CR4.OSFXSR, without which FXSAVE leaves the XMM registers out, is set for
  a CPU when its extension is turned on and cleared when it is turned off
  (`hvarch::x86::fp`), not around each entry as before: that was two
  serializing writes of CR4 and an XGETBV an exit, and under a nested
  hypervisor an exit to it for each. What is given up is that, while the
  module is loaded, an SSE instruction in the kernel would run on those CPUs
  instead of faulting -- a guard against a toolchain slip, not a guest. XCR0
  is made the x87 alone at the same moment, if firmware left it with more,
  and put back when the extension goes off; guests are given no XSAVE, so
  AVX and above fault in a guest even if it turns OSXSAVE on itself.
- **A halted guest costs its CPU nothing.** A HLT with interrupts on is
  stepped past, as a CPU an interrupt wakes resumes after it, and the vCPU
  is not entered again until the PIC has an interrupt for it; meanwhile its
  task sleeps to the timer's next edge, and is woken at it: the host CPU's
  one-shot timer is armed for the deadline as the task blocks ([the local
  timer](scheduler.md#the-local-timer), [Time](#time)). It used to be woken
  at its CPU's first scheduling point after the edge -- the host tick, at
  the latest -- so a guest's tick arrived up to 10 ms late. None is lost: every periodic edge that
  elapsed is owed to the guest and handed over one at a time, each once it
  has taken the last (`Pit::ch0_fire`), up to a second's worth. A guest at
  250 Hz was given one edge for every two or three periods before, and a
  guest keeping time in jiffies counted 40% of real time (see [On real
  hardware](#on-real-hardware)). Measured on the Linux
  guest over 120 s of the same boot and a typed `id`: 5,633,131 exits, 5.56
  million of them HLTs, became 76,334 and 11,410; the guest's timer ticks
  went from 11,973 to 11,943; the vCPU's task slept 89% of the run; and the
  QEMU process under it went from 106% of a host CPU to 46%.
- **Speculation, AMD-V's half of it** (`hvarch::x86::spec`): after every
  exit the guest's values are cleared out of the registers the run stub
  returns with, and the return stack is refilled before the host's first
  `ret` -- 32 calls, KVM's fill; automatic IBRS is on while the extension
  is, where the CPU has that (Zen 4 and later); and what else a CPU needs
  is decided from its family and CPUID as the extension goes on there,
  with Linux's list of which family is open to what as the reference. An
  IBPB right after every exit -- in the stub, as soon as the guest's
  registers are stored, before the return stack's refill and the host's
  first `ret` -- where a guest could otherwise steer the host's branches or
  returns: BTI on a CPU without automatic IBRS, since this kernel has no
  retpolines; Retbleed (Zen 1 and 2, and the two families before); and
  SRSO (Zen 1 to 5) where the IBPB flushes return predictions too, which
  Zen 1 and 2's does and a later one's where CPUID says IBPB_BRTYPE. That
  is Linux's `retbleed=ibpb` and `spec_rstack_overflow=ibpb-vmexit` for a
  host whose only way in from a guest is an exit, and it is also the IBPB
  between two guests on a CPU, which goes in by itself only where there is
  none after every exit. STIBP in IA32_SPEC_CTRL where a core has two
  threads: the IBPB clears what a guest trained on this thread, not what
  one on the sibling goes on training. And HLT and MWAIT always exit
  (`svm::HOST_MISC1`): a thread idling switches its core between SMT modes,
  and on Zen 1 and 2 a `ret` on the other thread may then be predicted from
  what the idler left in the return stack (SMT_RSB) -- the host's, refilled,
  when only the host idles one. `hv` counts the guests switched on a CPU
  and the IBPBs issued, says what each one closes, and names what a CPU is
  open to with nothing to close it: BTI or Retbleed without IBPB, SRSO
  without IBPB_BRTYPE, two threads without STIBP -- QEMU's TCG, under
  which hv-test runs where there is no KVM, is the first and the last.
- Each entry reads `EFER` and `VM_HSAVE_PA` on the CPU, and checks the VMCB
  against the manual's rules, before `vmrun`.

### What an exit costs

`hv bench` runs a guest that makes nothing but exits -- CPUIDs, each after a
read of each of `pages` pages 4 KiB apart -- on a task bound to one CPU, and
times it:

```
hv bench [cpu=N] [exits=N] [pages=N] [flush] [profile] [mmio]
```

`flush` makes every entry flush the whole TLB, as every entry did before
ASIDs, so the two can be set side by side on one boot (AMD-V only: under
VT-x there is no such entry to ask for); `pages` is what a flush costs the
guest after it -- each read a translation to walk again -- where with none
it is what it costs the host. `profile` times each entry in its parts with
the time-stamp counter: the checks and the ASID; the x87/SSE registers put
in; `vmrun` to `#vmexit`, the stub's VMSAVE and VMLOAD and the guest's few
instructions included; the registers taken back out; and the rest, which is
the exit handled and the loop round to the next entry. Under VT-x the parts
are the checks and VMPTRLD, the shadow written into the VMCS, the registers
in, `vmresume` to the exit, the registers out, and the exit and guest state
read out of the VMCS:

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

Those numbers are from before the IBPB after every exit that a Zen 2 now
gets ([What entering a guest cannot be told to
do](#what-entering-a-guest-cannot-be-told-to-do)), and from before what the
month since took off an exit. On the silicon -- nos booted on the AX41
beside Ubuntu, 2026-10-23, `hv bench cpu=3 exits=200000`, three runs and two
loads of each module, each the same to a few ns -- that IBPB costs an exit
a microsecond, which makes it nearly three times what it was, and STIBP
nothing:

| An exit, ns | on the AX41 | a read of 64 pages between |
|---|---|---|
| neither (the commit before) | 535-537 | 653-660 |
| STIBP alone | 537-540 | 647-703 |
| the IBPB after every exit, and STIBP | 1,543-1,545 | 1,723-1,763 |

A third of the microsecond is the barrier itself, in `vmrun` to `#vmexit`
(330 to 687 ns), where the stub writes PRED_CMD; the other two thirds are
the host's code after it running on the predictors it emptied -- the
checks (93 to 113 ns), the x87/SSE registers put in (8 to 50) and the exit
handled with the loop (117 to 682). Nested under KVM on the same machine
(`-cpu host` with cores of two threads, 2026-10-22) the same change cost
4.7 µs of an exit's 10.2, the barrier and the cold predictors there each
inflated by the outer kernel's round trips, and STIBP nothing there either,
Linux keeping STIBP on itself. Every built-in guest passes on the silicon
with the IBPB on, on the first CPU and the last -- the `cr4` guest's two
impossible values refused by the CPU itself there as under KVM. The cheaper
way to the same end -- the host built with an untrained return thunk and
retpolines, so that no IBPB is needed at all -- is left in [What a review
found](#what-a-review-found), item 1.

Under VT-x the first numbers were nested KVM's on the Intel dev box
(`-cpu host`, `hv bench exits=50000`), where every unshadowed `vmread` or
`vmwrite`, every `mov cr4`, VMPTRLD and intercepted `rdmsr` is an exit to
the outer kernel, and so where the host's side of an exit weighs far more
than on the silicon; but the parts they took away are the same parts:

| Step, 2026-09-25 | ns an exit | -- |
|---|---|---|
| after the review's fixes (CR8, INVEPT, NMI, the intercept set) | 17,900 | |
| host MSRs cached, VMPTRLD skipped, OSFXSR/XCR0 set once at `hv on` | 12,500 | -30% |
| VPIDs, FXRSTOR only when another guest ran in between | 11,700 | -7% |
| the VMCS sync dirty-tracked, the exit read by reason | 5,800 | -50% |

and the profile of the last (`hv bench exits=20000 profile`): checks and
VMPTRLD 13 ns, VMCS written 22, x87/SSE in 38, `vmresume` to exit 4,610,
x87/SSE out 53, VMCS read 51, the exit handled and the loop 1,010. The
4.6 µs in the middle is the outer kernel's round trip for the nested exit,
not this code's; of the 12 `vmwrite`s and 14 `vmread`s an exit, 4,700 and
1,500 ns before the dirty tracking, 73 ns are left. On real Intel silicon
the same instructions are tens of cycles each, so the shape of the gain is
the same and its size smaller: measured there is what is left to do.

On the silicon -- the EX44's i5-13500, 2026-10-20, nos booted beside Ubuntu
from its system image ([On real hardware](#on-real-hardware)), `hv bench
exits=200000`, each number the same to a few ns over two to six loads of
the module -- an exit costs 1.69 µs on a P-core (CPU 0) and 2.07 µs on an
E-core (CPU 62) without the speculation mitigations ([What a review
found](#what-a-review-found), item 1), and 1.82 and 2.49 µs with them. Each
one's share, from a module built without it:

| An exit, ns | P-core | E-core |
|---|---|---|
| without the mitigations (master's module, f3b0285) | 1,690 | 2,065 |
| with all of them | 1,818 | 2,490 |
| -- the return stack refilled with 2 entries, not 32 | -18 | -30 |
| -- no BHI_DIS_S | -22 | -287 |
| -- no VERW | -30 | -106 |
| -- no IBRS, and so no BHI_DIS_S | -50 | -303 |
| -- none of the four | -97 | -431 |

All of it is 7% of an exit on a P-core and 20% on an E-core, two thirds of
that BHI_DIS_S: a cost not of the switch but of every indirect branch the
host takes there while the extension is on, which the profile puts in "the
rest" (625 ns without the mitigations, 903 with). Linux on the same machine
runs its whole kernel under BHI_DIS_S and pays it everywhere. What is left
between none of the four and master, 30 ns on the P-core and nothing to
measure on the E-core, is the rest of the review's fixes: DR7 switched by
the entry and the exit themselves, the registers cleared, the record of
the CPU's debug registers. One thing not explained: the first load of the
module after a boot runs its E-core exits at 2.2 µs, BHI_DIS_S's share
gone, though `hv`, reading every CPU's IA32_SPEC_CTRL back, finds it set
there; every later load is at 2.49.

The P-core's profile, without the mitigations and with them:

| | without | with |
|---|---|---|
| checks and VMPTRLD | 17 ns | 21 ns |
| VMCS written | 20 | 21 |
| x87/SSE in | 63 | 63 |
| `vmresume` to exit | 950 | 1,041 |
| x87/SSE out | 86 | 86 |
| VMCS read | 20 | 20 |
| the exit handled, the loop | 550-573 | 605 |

Those are numbers at the base clock, 2.5 GHz, where firmware left every
core and nos kept it until it turned hardware P-states on ([Boot](boot.md),
the clock): with the cores up to their turbo an exit costs 1,018 ns on a
P-core and 1,512 on an E-core, an MMIO access 2.5 us, and the profile reads
checks 12 ns, the VMCS written 12, x87/SSE in 35, `vmresume` to exit 587,
x87/SSE out 49, the VMCS read 11, the rest 339.

The first build with them took three `CpuLocal`s an entry -- the CPU's
debug registers, the last guest it entered, its mitigations -- and its
checks were 418 ns, each `with` being calls into the kernel to turn
interrupts off and on and to ask which CPU this is. They are one now
(`Entries` in `hvarch/src/x86.rs`), which the entry takes once as what
holds it on its CPU, as AMD-V's did with its ASIDs alone.

`mmio` has the guest make MMIO loads instead -- `mov eax, [rdi]` from a
device's page in the MMIO window, each a nested fault whose instruction the
host walks to, fetches, decodes and performs ([MMIO](#mmio)) -- and with
`profile` says what of an exit the access itself took. Under nested KVM,
2026-09-29, 200,000 of each on one CPU (the access's share from a run of
20,000 under `profile`):

| | a CPUID exit | an MMIO load | the access itself |
|---|---|---|---|
| AX41 (Zen 2, AMD-V) | 10.3 µs | 11.7 µs | 1.1 µs |
| rserver (i5-13500, VT-x) | 5.8 µs | 8.7 µs | 2.4 µs |
| rserver, the whole guest state read at every EPT violation | 5.8 µs | 30.8 µs | |

The last line is the first measurement, and why an EPT violation now reads
only what performing an access takes ([The VMX
backend](#the-vmx-backend-and-where-it-differs-from-amd-v)): of the
forty-odd fields a report wants, most are ones a nested KVM keeps out of
the shadow VMCS it gives L1, each an exit to it, and 25 µs of every access
went there. What is left above a CPUID exit is L0's longer way through a
nested fault (0.3 µs under VT-x, none to measure under AMD-V) and the
access itself -- three reads of the guest's page tables (the bench's code
is on a 2 MiB page; four for a 4 KiB one) and one of its instruction, each
a copy through the frame window with a mapping of its own made and
dropped, and the decoding, which is a few dozen branches. The copies are
where to look if an MMIO-heavy guest makes it matter: on the EX44's silicon
(2026-10-20, a P-core) a CPUID exit is 1.8 µs, an MMIO load 4.5, and the
access 2.5 of that -- no less than under nested KVM, being the host's own
work, and so there most of what an MMIO access costs.

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
`kvm_amd nested=1`), which is far faster than TCG and is how the VMX path is
exercised before it reaches real hardware. `scripts/hv-test.py` uses it on
both: AMD-V on an AMD host, Intel VT-x (nested) on an Intel one, and AMD-V
under TCG's `-cpu max` where there is no KVM -- TCG has no VMX, so the Intel
backend is reached only through KVM. The two guests whose verdict differs by
vendor (`refused`, `asid`) have their expected output chosen from `hv info`.

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

- **A CPU cut down to what is emulated** (`hv::policy`), and named whole.
  CPUID is an allow-list: a leaf the policy does not name is all zeros, as
  is every leaf past the tops it reports -- 0x16 and 0x8000_0021, or the
  host's if lower -- and of the leaves it names a guest is told the host's
  bits it lets through: no XSAVE and so no AVX -- the state switch around
  `vmrun` moves only the FXSAVE registers, so a guest is given nothing it
  could put in the part that is not switched -- no paravirtualisation, no
  speculation control (none has its MSR behind it yet); of the structured
  leaf, 7, what runs as it is and owns no state of the host's -- SMEP, SMAP
  and UMIP for the guest's kernel, FSGSBASE over bases the VMCS and the VMCB
  keep, BMI, ADX, ERMS and FSRM, RDSEED, SHA, CLFLUSHOPT and CLWB,
  SERIALIZE -- with MD_CLEAR, VERW clearing the CPU's buffers in a guest as
  anywhere, and ARCH_CAPABILITIES, whose MSR reads as the host's but for
  the bits that are not facts of the CPU (IBRS_ALL, whose SPEC_CTRL is not
  the guest's, and those naming an MSR nothing answers); PCID, but not
  INVPCID, which VT-x would need a control for; of the power leaf ARAT
  alone, and for a guest of one CPU no local APIC and no x2APIC (one of
  more has an emulated x2APIC: [More than one CPU](#more-than-one-cpu)); and
  the hypervisor bit, since there is one, which has Linux leave the
  microcode loader and the bare-metal chicken bits alone. Each of its CPUs
  is a package of one, whichever host CPU its vCPU runs on: leaf 1 says one
  logical CPU, with that CPU's APIC ID -- 0 for a guest of one -- leaves 4
  and 0x80000008 one core, and the topology leaves (0xB, 0x1F, 0x8000001E),
  the SVM leaf and the memory-encryption leaf are blank. Every MSR is
  intercepted but the three segment bases, and every one is an allow-list's
  too: the system MSRs (EFER, the PAT, the segment bases, the SYSCALL and
  SYSENTER registers) are the guest's own state and are served from the
  VMCB save area -- a PAT entry that is no memory type, an address that is
  not canonical, an EFER bit its CPUID does not offer each a #GP, as the CPU
  gives -- the x2APIC's by the local APIC of a guest that has one, the TSC
  as the host's; and a few answer as a CPU with nothing behind them would,
  each taking a write that leaves it as it is and no other: the MTRRs (none,
  and off) and the machine-check registers (no banks, nothing to report),
  which leaf 1 offers and Linux reads with no fallback for a fault -- a
  fault on a machine-check register is a panic to it -- the microcode
  revision, none, and of each vendor what Linux reads of every such CPU
  without asking: Intel's platform ID and MISC_ENABLE, its fast strings as
  the host has them; ARCH_CAPABILITIES where leaf 7 offers it, read-only;
  AMD's HWCR, DE_CFG and NB_CFG, with the bits a Linux
  on AMD checks set as the hardware has them. Every other MSR is a #GP,
  read or written -- what a guest's `rdmsr_safe` probes are ready for, and
  a line in the VM's report. One a guest's user can reach is refused on
  purpose: DEBUGCTL, whose BTF `ptrace`'s block-step sets -- Linux says so
  once ("unchecked MSR access error") and steps by instruction instead,
  where a write kept nowhere would have it believe it steps by branch.
  LBR virtualization in the VMCB would let BTF work.
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
  takes its interrupts from -- every one of them, for a guest of one CPU,
  which runs with no local APIC.
- **The exit loop** (`hv::run`) that answers all of the above, injects the
  highest-priority interrupt the PIC has when the guest can take one (and
  asks the CPU, through SVM's virtual-interrupt window, to exit the moment it
  can when it cannot), and halts the vCPU when the guest does: an idle `HLT`
  is stepped past -- a CPU an interrupt wakes resumes after it, so Linux's
  `sti; hlt; cli` returns to its idle loop, and out of the STI's interrupt
  shadow -- and the vCPU is not entered again until an interrupt is pending
  for it, its task asleep until the timer's next edge. It streams the
  guest's console to the kernel log a line at a time -- a boot's worth, a
  thousand lines, and then fifty a second, what it leaves out said once a
  second at most (`... N lines not logged`), so that a guest printing as
  fast as it can does not have the host's log, serial line and netconsole
  for its own; the console itself, which the report and `hv console` show,
  keeps every byte -- and stops with a reason and a register dump.

And what a PC's firmware leaves a kernel, which it reads instead of asking
a BIOS: the e820 map, an MP table for a guest of more than one CPU ([More
than one CPU](#more-than-one-cpu)), and the ACPI tables, with the fixed
hardware they describe ([ACPI](#acpi)).

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
`vdb`, ... in that order, each the size of its file -- past 4 GiB too, as
big as nos's ext2 lets a file be ([Filesystems](filesystems.md#ext2): almost
2 TiB of 4 KiB blocks), and blank from `truncate /vm/disk.img 20G` if it is
to be made on nos. The guest mounts it, ext4 and all, as it would a disk of
QEMU's:

```
$ hv start /bzImage initrd=/initrd disk=/disk.img cmdline=console=ttyS0 nolapic
$ hv exec 0 mount -t devtmpfs devtmpfs /dev
$ hv exec 0 mount /dev/vda /mnt
$ hv exec 0 cat /mnt/hello.txt
```

**An image is one writer's, or any number of readers'.** A guest's disk
claims its image as it opens it, by the file rather than the path -- its
mount and inode, so two names of one image are one: a disk that writes it
has it alone, and disks that only read it (`:ro`) share it. Two guests'
filesystems on one set of blocks would each wreck the other's with nothing
to say so; a start or a boot that cannot have its claim is refused, saying
whose the image is --

```
$ hv start /bzImage initrd=/initrd disk=/disk.img:ro cmdline=console=ttyS0 nolapic
hv: vm 1 not started -- /disk.img: vm 0's disk writes it
```

-- and the claim goes with the disk, when its VM stops. It holds among the
VMs only: nos's own shell is not held to it (`modules/hv/src/disk.rs`).

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
sector range is checked against the disk's size, and its size against what
the driver was told a request may carry.

**The disk works beside the guest.** A request used to be served before the
notify that made it available returned, the vCPU waiting for its disk -- and
on the AX41 a guest writing back what `apt-get update` had fetched stalled
for seconds at a time ([On real hardware](#on-real-hardware)). Now the notify
only takes the requests off the ring -- a write's data copied out of guest
memory into a buffer of the device's -- and hands them to the backend, which
serves them in order on a task of its own, on a CPU other than the vCPU's.
Each time round the run loop what it has served is given back: a read's
data copied into the guest, the status written, the chain on the used ring,
an interrupt; and as it finishes each the task wakes a halted vCPU or kicks
a running one out of its guest, the NICs' way ([A network](#a-network)).
Guest memory is touched on the vCPU's task and nowhere else. At most eight
requests are out at once, each in a 256 KiB buffer taken when the disk is
made -- the driver is told a request carries at most 64 segments of a page
(`seg_max`, `size_max`), and the ring's 256 entries hold three of the
largest -- so nothing is allocated on the way, and a guest that keeps its
ring full waits for its disk rather than growing anything of nos's.

**The disk's bytes are a trait's** (`hv::disk::Backend`, which is handed a
request and gives it back served): the module's `FileDisk` holds the image
open (`kcore::fs::File`, whose handle the kernel looks up on every call; the
file cannot be removed, nor its filesystem unmounted, while the disk has
it), reads and writes it where the guest asks -- within the file's size,
never growing it -- and for a guest's flush, offered and taken by ext4,
syncs the filesystem it is on. The report counts each disk's reads, writes,
flushes and errors.

**What a guest's write costs nos's ext2** ([Filesystems](filesystems.md)):
one into a hole of the image flushes nos's disk once and writes the pointers
to what it filled after that, where it used to cost a flush and four FUA
writes, and every block it took a write of its indirect block besides; one
over blocks the image has flushes nothing, and writes its inode only when
the second of its mtime changed. The image is also no longer looked up by
its path for every 64 KiB. The same 48 MiB `dd if=/dev/zero ... conv=fsync`
in the guest under TCG, as the guest timed it, and what nos asked of its own
disk over the whole run, its boot's hundred-odd flushes included (QEMU's
`info blockstats`):

| the image on nos's root | before | after |
|---|---|---|
| with holes: the guest's writes allocate | 11.3 s; 32,640 writes, 134 MB, 4,716 flushes | 2.2 s; 13,530 writes, 55 MB, 317 flushes |
| whole: the guest's writes overwrite | 3.4 s; 15,534 writes, 64 MB, 1,978 flushes | 0.83 s; 14,559 writes, 60 MB, 112 flushes |

On the AX41 a Debian guest writes at 1.2 GB/s and reads at 1.6 GB/s, and
what `apt-get update` leaves dirty goes down in a few hundredths of a
second where it took half a minute ([On real hardware](#on-real-hardware)).

`scripts/hv-linux-test.py --disk` is its gate: an ext4 image with a file in
it on nos's root, the guest mounting it, reading the file, writing 4 MiB and
a file of its own, syncing and reading back after a remount -- and then
nos's root judged by `e2fsck`, and the image, taken back out of it, judged
too and holding what the guest wrote. It needs a guest kernel with PCI,
legacy virtio-pci, virtio-blk and ext4.

## A disk from a cloud's image

A cloud image comes as QEMU's qcow2 -- Ubuntu's
`ubuntu-24.04-server-cloudimg-amd64.img`, Debian's `genericcloud`,
Fedora's -- and a guest's disk here is a raw file. `hv convert <image>
<disk>` makes the one the other (`modules/hv/src/image.rs`, over the
`diskimage` crate, which has no kernel in it):

```
$ hv convert /img.qcow2 /img.raw
hv: /img.qcow2: qcow2 version 3, a disk of 7 MiB in clusters of 64 KiB -- into /img.raw
hv: /img.raw: 2 MiB written of 7 MiB, the rest holes -- 34 clusters inflated, 8 copied, 0 zeros, 55 not there -- in 0.0 s
```

-- a progress line every few seconds on a big one, as wget's. Ubuntu 24.04's
image (serial 20261017, its SHA-256 the one `SHA256SUMS` gives) converts, by
the same code on the host, to a disk `qemu-img compare` calls identical.

**The holes are kept.** The disk is a new file, grown to the image's size
with a hole (`File::set_len`), and only its blocks that are not all zeros
are written: Ubuntu's 3.5 GiB disk costs the 1.7 GiB it holds, and grows as
its guest writes. `truncate` it bigger, for a guest to grow its root into
(cloud-init's growpart does): hvboot reads the image's GPT from its primary
header, wherever the disk ends.

**What it reads**: versions 2 and 3; clusters of 512 bytes to 2 MiB;
clusters stored whole, compressed by zlib -- 27,971 of the 28,663 that
Ubuntu's image has data in -- all zeros, or not there; and extended L2 entries, their
subclusters each of those. A compressed cluster is inflated by a DEFLATE
decoder of the crate's own (`diskimage/src/inflate.rs`) whose every rule is
zlib's, checked in zlib's order -- what it needs of a stream's bits before it
calls a code bad included: QEMU reads those clusters through zlib, so a
cluster QEMU reads must give its bytes here and one QEMU refuses must be
refused. Internal snapshots are left out: the disk is the image as it is
now.

**What it refuses, saying why**: an overlay on a backing file, which it does
not contain; encryption, AES or LUKS; an external data file; zstd
compression; an image QEMU marked corrupt; features it does not know; a disk
that is there already, which is not written over; and a file that is no
qcow2 -- by what it is: a raw disk is given to a guest as it is, an ISO
read-only, a gzip or xz stream is unpacked first, a VMDK, VHD, VHDX or VDI
is converted elsewhere. A conversion that fails leaves no disk behind.

**The image is a download, and untrusted.** Its every offset is held to the
format's alignment and to the file, every table to the bounds QEMU puts on
it, and nothing it says makes the converter read, write or hold more than
64 KiB at once, whatever its cluster size -- a 2 MiB cluster is never one
allocation. QEMU reads past a file's end as zeros; here a table or a cluster
that *starts* past it, and a compressed one that runs past its last sector
and does not decode, are refused, asking "is the image whole?": what has
them is a download cut short, and a disk of zeros where its data should be
is a failure nobody would hear of. Its gate is `scripts/diskimage-test.py`
(host, in CI): images qemu-img and qemu-io write, of every version, cluster
size and kind, converted and held byte for byte to what qemu-img reads; the
refusals; the decoder against zlib over streams made and broken at random;
and the images' metadata fuzzed. `hv-test.py` converts in nos, on both
architectures, and holds the disk to its SHA-256 ([Testing](testing.md)).

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
and offloads both ways (below), legacy's header in front of every frame
either way -- ten bytes, twelve once the driver takes mergeable buffers. A
send is cut, or handed on whole, before the notify returns; what waits for
the guest is put into the buffers it has posted each time round the run
loop, one frame held by the device while it has posted too few.

**Offloads, both ways.** The device completes a checksum the driver leaves
to it (`VIRTIO_NET_F_CSUM`) and cuts TCP over IPv4 the driver hands it as
one send of up to 64 KiB into frames (`HOST_TSO4`; `hv::devices::offload`):
the headers of the send in front of each, its IP length, ID and checksum,
its sequence number and its TCP checksum its own, FIN and PSH the last
one's, CWR the first's. What the driver says about either -- where a
checksum goes, where the headers end, the segment size -- is checked against
the frame before it is used, and a send that does not hold up is dropped,
counted. Not offered: ECN on a cut send, IPv6, UDP; and a segment under 256
bytes is refused, so that one notify is 256 frames at most and not 65,000
that a guest could ask for at no cost to itself.

Between two guests, the switch does neither where it need not. A guest
whose driver takes checksums left to finish (`GUEST_CSUM`) is handed a
frame its sender left so as it was sent, the header saying where -- Linux
takes it as checked, as it does what comes over its own loopback; one that
takes sends whole too (`GUEST_TSO4`, with mergeable buffers to spread one
over, `MRG_RXBUF`) is handed a send of up to 64 KiB as its sender made it,
the header saying the segment size to cut it by should it be forwarded.
Everyone else -- nos's stack through `hv0`, NAT, a guest whose driver takes
neither -- is handed whole frames, finished and cut on the way out of the
switch, as before. The device tells the switch what its driver takes when
it says so; a frame put in before a driver took other features is the
device's to finish, and a send its driver no longer takes whole, to drop.
A port keeps room for eight sends of 64 KiB beside its frames, taken when
it is claimed.

`nooffload` on `hv boot` or `hv start` offers the device without any of
them. On the EX44 two guests of two CPUs, one serving a file of 100 MB five
times over with BusyBox's `httpd` -- `sendfile`, sends of 64 KiB -- and the
other fetching it: 0.47 s with them, 1.63-1.68 without, 1.1 GB/s against
320 MB/s -- the fetching guest took 8,500 sends whole where it had taken
362,000 frames. With the sending side's alone, the sends cut in the switch,
it was 1.11 s. A sender whose TCP hands the device a segment at a time,
`dd | nc`, is 7% slower with them: 4.45 s against 4.15, five runs each.
Offered one at a time, most of that is the sending side's checksum (4.34 s;
TSO adds nothing to it, and mergeable buffers alone cost nothing, 4.16),
with the exits and the guest's IPIs alike either way: a Linux driver that
leaves its checksums to the device gathers each frame from pages too --
scatter-gather comes on with them -- three or four descriptors a frame
where it was two, and every descriptor and buffer the device reads is a
guest page mapped and unmapped on the host, one access at a time: the
likeliest cost, and the rings' pages mapped once took most of it away: 2.83
s against 2.72, the two within their spread ([Its memory](#its-memory)). A checksum the device finishes it sums eight
bytes at a time, since two bytes at a time made that 3-5% slower than the
guest's own, taken as it copies. hv-fuzz's `offload` target holds every
send, well made and not, to a model of its own, frame by frame; two mutants
-- PSH on every frame, one IP ID for all -- each failed within three inputs.
Its `netrx` target holds what goes into the driver's buffers to another,
and `switch` where each send and frame goes, whole or finished; six mutants
-- the buffer count one too many, a partial checksum not finished for a
driver that takes none, a send whole to one that takes none, the switch not
told what a driver takes, a partial checksum or a whole send handed to
every port -- each failed within 17 inputs.

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

**What the guests share is shared out.** `hv0`'s backlog -- the 256 frames
handed in and not yet taken by the stack's receive pass
(`kcore::vnic::BACKLOG`), which every guest's traffic to nos and through NAT
goes on -- takes from a port, between two of the stack's passes over it, no
more than its share: the backlog over the ports a VM has, so a guest alone
has all of it and one of several sending as fast as it can leaves the rest
theirs. The switch learns of a pass from the count of them the virtual NIC
keeps (`Vnic::drains`) and counts each port's share afresh from it. And what
a guest sends to every port -- a broadcast, a multicast, a MAC no port has
-- is a copy into every inbox and a wake or a kick of every VM: 64 at once
and 256 a second after, a token bucket a port. What either keeps out is
dropped and counted against the guest in `hv list`.

**What a guest sends has to be its own.** A frame is checked where it comes
in at a port (`ingress`), as a cloud's virtual switch checks it: IPv4 or ARP
-- nothing of nos's speaks anything else, and nothing the switch cannot
check goes through it -- from the port's MAC; an IPv4 packet from the
port's address; an ARP message with the port's MAC as its sender's, and
the port's address as its sender's or none yet, as a probe has. The one
IPv4 packet with no address yet is a DHCP client's, which the switch
answers itself. The rest is dropped: a frame sent as another guest counted
against the guest that sent it in `hv list`, one of another protocol --
IPv6, which every Linux sends at boot -- on `hv0`'s line. Without it a guest
could take another's flows through NAT, which answers a flow at the MAC its
last packet came from -- one packet with the other's address and the flow's
ports was enough -- and tell `hv0`'s ARP table another's address was its
own. `hv-fuzz`'s `guestnet` target found that
([testing](testing.md#hv-fuzzpy----everything-a-guest-reaches-fuzzed)).

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

**Receive interrupts, moderated.** A guest that takes its frames faster than
they come takes an interrupt for every one and acknowledges every one or
two: two guests of two CPUs sending TCP to each other through the switch got
1.1 frames an interrupt, and the receiver sent 140,000-180,000 ACKs for 500
MB. So a NIC's receive interrupts are held at least 100 us apart
(`Net::set_rx_moderation`; `rxmod=US` on `hv boot` and `hv start`, 0 for
none), as a real NIC's interrupt throttling holds them: what arrives goes
into the guest's buffers at once, its interrupt waits until the time since
the last is up, and then one takes all that came; a frame after a quiet
spell is interrupted for at once. Meanwhile the switch kicks nobody for a
frame -- a kick each would hand each over alone -- unless the port's inbox
is half full (`Backend::quiet_until`), and the vCPU that does the devices'
work comes back at the end of the window on its own (`Net::rx_due`): slept
to when its guest is halted, and armed on its host CPU's timer before each
entry otherwise, one more timer edge ([Time](#time)). On the EX44, the TCP
pair, three runs at each window, taken in turns:

| window | frames an interrupt | receiver's ACKs | 500 MB |
|---|---|---|---|
| none | 1.12-1.17 | 137,000-176,000 | 4.57-5.15 s (4.88) |
| 25 us | 2.6-2.8 | 140,000-168,000 | 4.47-5.22 s (4.72) |
| 50 us | 4.7-5.0 | 102,000-144,000 | 4.13-4.80 s (4.57) |
| 100 us | 9.7-10.2 | 63,000-87,000 | 3.94-4.07 s (4.01) |
| 200 us | 20.0-20.5 | 33,000-47,000 | 3.70-3.86 s (3.77) |

The cost is latency where frames come close together: a hundred small HTTP
fetches between the two, each its own connection, took 0.07 s with none,
0.05-0.07 s at 50 and 100 us and 0.08-0.09 s at 200 us; a ping's round trip,
0.24-0.30 ms on average, was the same at every window. Hence 100 us. hv-fuzz
holds the NIC to raising what it owes by the time it said it would look
again -- which found a queue taken down owing for good, a round the run loop
would have made without a pause -- the switch's wakes to a model that knows
each port's window, and runs whole machines with it on and off. What it did
not see is a window opened behind that vCPU's back: a driver refills its
receive ring from the CPU its interrupt is taken on, and that CPU's notify
delivers what waited and raises the interrupt itself -- so the first CPU,
asleep with the old window's end in mind, slept to its 10 ms cap while the
frames that came in the new one woke nobody. Pings between two guests had
a 10 ms answer every few hundred; a notify from another CPU that moves the
window now rings the first CPU, and 2,400 pings had none, 0.19 ms on
average and 0.28 at most.

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
out 53  back 744  mapped 4  taken from the most 0  no room 0  no next hop 0  no frame 0  refused 0  denied 0
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
`hv forward`'s.

**Where a guest may go is the world past nos's edge, not what is behind
it.** NAT keeps the guests from link-local (169.254/16, where a cloud's
metadata server answers at 169.254.169.254, the host VM's credentials
behind it), the private networks (10/8, 172.16/12, 192.168/16), carrier-grade
NAT's shared space (100.64/10), where some clouds keep theirs, and the
uplink's own subnet -- the host's LAN, its neighbours -- whatever that is. A
guest's packet for any of them is not mapped: it is counted (`denied`) and
answered with an ICMP destination unreachable, communication
administratively prohibited, from 10.0.100.1, quoting it -- so its connect
fails at once, "No route to host", rather than waiting out its retries. The
DNS server the guests are given is theirs to ask at port 53, wherever it is
(10.0.2.3 under QEMU's user network; on a cloud, the metadata server's
address). `nat allow <net>` lets a network through -- the test machine,
under QEMU, is on the uplink's subnet at 10.0.2.2 -- and `nat deny <net>`
takes that back; up to 16, kept while NAT goes off and on, as the host's
word. `nat` says what is kept and what is let through. The rule is a pure
function (`kept`) the boot self-test checks; hv-fuzz's `nat` target holds
every packet to a model of its own (nothing to a kept network out, every
reject right, the list as `nat allow` and `nat deny` left it). A mapping lives while its flow is seen and RFC 5382's and
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
live, the next flow takes one: the one used least lately of the guest that
holds the most -- its own, when the asker holds as many as any (`taken from
the most`). Each guest's mappings are a list in the order they were last
used, in either direction, so the one to give up is found at once; a guest
that opens flows as fast as it can takes them from itself, and cannot have
every mapping there is, as it could while a full table refused every new
flow. The table keeps a share for 64 guests at once, the switch having 16
ports; a packet of a guest past them is refused (`no room`). It is swept for
flows past their time no sooner than the first of them can be -- the soonest
of every flow's time, kept as each is made or kept alive, since a UDP flow's
five minutes can run out under a TCP flow's two hours. The next hop's address comes from the ARP cache without waiting
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
hv: vm 0 stopped -- on request -- nothing in it listens to its power button
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

**Stopping is a question, then a flag and a join.** `hv stop` takes the VM
off the list first and presses its power button, and a guest that shuts
down when it is pressed is given `secs` to ([ACPI](#acpi)); the one above,
a kernel without ACPI, never hears the button, and is stopped at once. Then
the flag: the loop checks it before every entry, and a halted guest's task
sleeps at most 10 ms at a time, so the stop comes within a tick or so, and
the command joins the task after, with no lock held while it waits. A guest
that stops by itself -- a triple fault, a HLT with interrupts off, a reset, a
power-off -- stays on the list as `stopped`, with its reason, until `hv
stop` takes it off.

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
panic with `panic=N` -- by the reset register its ACPI tables name, when it
has them (the chipset's reset control, `0x06` to port `0xCF9`); then by
pulsing the 8042's reset line (`0xFE` to port `0x64`); then by the chipset's
reset control where it knows of one; then by a triple fault. Nothing here emulates an 8042 or a
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
`not entered` -- on AMD-V; on Intel the CPU itself refuses to go off while a
guest's VMCS is current on it ([above](#what-the-extension-costs-while-it-is-on)),
so an `hv boot` or `hv run` the courtesy check does not see keeps its CPU
too. `rmmod hv` stops every guest before it turns the extension off -- where
it is, without pressing its power button; `hv stop all` before it shuts them
down -- the guests first, an `hv boot` under way among them, so that an `hv exec`
or `hv wait` waiting on one returns; then the command, whose unregistration
waits out every call still running (an `hv start` still reading its files
finds the module going when it comes to add its guest, and stops it itself);
and only then the extension, on every CPU.

The gate is `scripts/hv-linux-test.py` with `--initrd`: two VMs started side
by side and one stopped mid-boot, a line typed at the other before its shell
is up and another at its prompt, `send`, `wait`, `console`, the refused
`hv off`, and an `rmmod` that stops the guest itself and leaves nothing on for
the next load.

## A VM in a folder

A guest started by its kernel's path is gone with its words: the next `hv
start` says them all again. A VM kept in a folder is not. Its folder under
`/vms` -- or wherever `insmod` was told to keep them ([below](#where-the-folders-are))
-- holds what to boot it with -- `vm.toml` -- and beside it whatever the
file names: its kernel, its initrd, its disks; and its console's log. `hv
start <name>` boots it as the file says (`modules/hv/src/store.rs`), and
every command that reaches a VM takes the name as well as the number:

```
$ hv create web /boot/bzImage initrd=/boot/initrd disk=root.img mem=1024 cpus=2 net port=3 forward=2222:22 restart cmdline=console=ttyS0 root=/dev/vda rw
hv: /vms/web/vm.toml written -- hv start web boots it
hv: /vms/web/root.img is not there yet
$ truncate /vms/web/root.img 20G
$ hv vms
web  not running  bzImage  1024 MiB  2 cpus  disks root.img  net port 3 (10.0.100.5)  forward 2222:22  restart
$ hv start web
hv: vm 0 (web) started on cpus 2,3 -- /boot/bzImage, 1024 MiB, 2 cpus, cmdline "..."
hv: port 2222 forwarded to vm 0, 10.0.100.5:22
$ hv exec web uname -r
$ hv list
vm 0 web  running  cpus 2,3  ...
$ hv stop web
```

`hv create <name> <words>` writes the file from the words `hv start` takes
-- their paths the folder's unless they begin with `/` -- checked as a start
would check them, and never over a `vm.toml` that is there; it says which of
the files it names are not there yet. `hv start /some/where/web` boots a
folder anywhere by its path, the folder's name the VM's. `hv vms` lists the
folders under `/vms`, what each boots, and which run.

### A VM of a cloud's image

`hv create <name> <url>` makes the folder of a distribution's cloud image
(`modules/hv/src/cloud.rs`), for `hv start <name>` to boot it as a cloud
does -- with nothing done anywhere but on nos:

```
$ hv create noble https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img size=20G mem=2048 cpus=2 forward=2201:22
hv: https://cloud-images.ubuntu.com/... -- into /vms/noble/ubuntu-24.04-server-cloudimg-amd64.img
download: ... MiB of 592 MiB, ...%, ... MiB/s
hv: /vms/noble/ubuntu-24.04-server-cloudimg-amd64.img: qcow2 version 3, a disk of 3584 MiB in clusters of 64 KiB -- into /vms/noble/disk.raw
hv: /vms/noble/disk.raw: 1785 MiB written of 3584 MiB, the rest holes -- 27971 clusters inflated, 692 copied, 0 zeros, 28681 not there -- in ... s
hv: /vms/noble/vm.toml written -- hv start noble boots it: hvboot, a 20480 MiB disk, cloud-init given 1 key
$ hv start noble
```

- **The image** is fetched by the kernel's own HTTP client -- the one
  `wget` is -- through `kcore::http::fetch` (the export
  `kernel_http_fetch`, since a module cannot link the network layer): TLS
  with the certificate checked, redirects followed, and the bulk receive
  window a download of gigabytes needs, which a module's own TCP
  connection is not given. Only a 2xx answer is a body -- "the server
  answered 404" otherwise -- a body cut short is no image, and a file there
  already is not written over; a failed download leaves no file.
- **The disk**: a qcow2 image is converted ([A disk from a cloud's
  image](#a-disk-from-a-clouds-image)) and the download removed; a raw one
  is taken as it is; anything else is refused by what it is. `size=` (K,
  M, G, T) grows it with a hole -- never under what the image holds -- for
  the guest to grow its root into: cloud-init's growpart does, on its first
  boot.
- **The seed**: `seed.iso`, the VM's second disk, read-only -- an ISO 9660
  labelled `cidata` with Rock Ridge's names (`diskimage::iso`), which
  cloud-init's NoCloud finds by its label. Its meta-data names the machine
  after the VM, with an instance-id of its own (`nos-<name>-<time>`), so
  that cloud-init takes it for a new machine; its user-data gives the
  image's own user -- `ubuntu`, `debian` -- the keys nos's sshd lets in
  (`/etc/ssh/authorized_keys`), and with `password=` a password, for its
  console. Every string in it is YAML's double-quoted, so that a key's
  comment is never read as YAML.
- **The vm.toml**, written last: hvboot as its kernel (`/lib/hvboot`, the
  system image's, or `hvboot=`), which boots the kernel the image's own
  GRUB would ([A guest's own kernel](#a-guests-own-kernel-pvh-and-hvboot));
  `disks = ["disk.raw", "seed.iso:ro"]`, a `disk=` given after them; on
  the switch; booted again when it reboots itself; 1024 MiB unless `mem=`
  says otherwise. The other words are `hv start`'s, but a kernel and an
  initrd: `cpus=`, `port=`, `forward=` -- `forward=2201:22` for its own
  sshd -- `cmdline=`, which hvboot puts after the entry's own line.
- **Nothing is left of a failure**: what the command made -- the download,
  the disk, the seed, and the folder when it made that too -- goes, and
  why it failed is said.

What it does not do yet: check the image against its distribution's
`SHA256SUMS` (TLS's certificate is what vouches for the bytes now), resume
a download that broke off -- the client gives up after ten seconds of
silence -- or hold less than the download and the disk at once on nos's
root while it converts. `scripts/hv-test.py` (x86-64) makes VMs so from a
web server of the test's own, and `scripts/hv-distro-test.py --create
<url>` makes and boots one, cloud-init provisioned by the seed
([Testing](testing.md)).

### Where the folders are

`/vms`, unless `insmod` says otherwise: `insmod /hv.ko vms=/data/vms` keeps
them under `/data/vms` -- an absolute path with no `.`, `..` or empty step
in it, its trailing `/` let go, and not `/` itself, whose every folder would
be taken for a VM's -- and `hv` says where they are kept. `hv create` makes
the folder, and those above it that are missing; `hv start <name>` and `hv
vms` look there, and only there. A parameter hv has not got refuses the
load, said on `insmod`'s output -- as a misspelt key does in a `vm.toml`,
and for the same reason:

```
$ insmod /hv.ko vmz=/data/vms
hv: "vmz" is not a parameter of hv's -- vms=<folder> is
module: hv: init failed
insmod: /hv.ko failed, error 502
$ insmod /hv.ko vms=/data/vms
module: hv loaded at 0xFFFF8000...
$ hv
hv: AMD-V on for cpu none of 0-3
hv: vms kept in /data/vms, as insmod was told
```

`rc add insmod /hv.ko vms=/data/vms` loads it so at every boot. `vms=` is
at most 208 bytes: the VFS takes paths of 255, and under the folder go a
VM's name of up to 32 and the longest file hv writes there,
`console.log.1`. On arm64, which runs no Linux guest yet, hv takes no
parameter, and refuses one. How a module is handed its parameters is in
[Loadable modules](modules.md#parameters).

**The file** is a strict subset of TOML (`modules/hv/src/config.rs`): a
`key = value` a line, `#` to the end of a line a comment, and for a value a
string -- `"basic"`, with TOML's escapes, or `'literal'` -- a whole number,
`true` or `false`, or a list of strings in brackets, over as many lines as it
takes. No tables, no dotted or quoted keys: what it cannot read it refuses,
saying the line, and so a key it does not know and one given twice --
`memroy = 4096` would otherwise boot a guest of the default size without a
word.

```toml
# A VM of nos's hypervisor: hv start <this folder's name> boots it (docs/hypervisor.md)
kernel = "/boot/bzImage"
initrd = "/boot/initrd"
cmdline = "console=ttyS0 root=/dev/vda rw"
memory = 1024
cpus = 2
disks = ["root.img"]
net = true
port = 3
forward = ["2222:22"]
restart = true
console_log = true
```

| key | | unless said |
|---|---|---|
| `kernel` | its bzImage | -- (the one key a file must have) |
| `initrd` | its initramfs | none |
| `cmdline` | its kernel's command line | `console=ttyS0 nolapic`; with several CPUs `console=ttyS0` |
| `memory` | MiB | 256 |
| `cpus`, `cpu` | how many CPUs, and the host CPU its first runs on | 1, and where it is placed |
| `disks` | its disks, `vda` first, `:ro` after one it only reads | none |
| `net` | a NIC on the guests' switch | `false` |
| `port` | its port of the switch, 0..15: its address `10.0.100.(port+2)` and its MAC the same at every start | the first free one |
| `forward` | `"<nos's port>:<the guest's>"`s, relayed once it runs and taken away with it | none |
| `restart` | booted again when it resets itself | `false` |
| `console_log` | its console to `console.log` in the folder | `true` |
| `log` | its console to the kernel log too | `false` |
| `input` | typed at its first prompt | none |
| `ioapic`, `xapic`, `apicv`, `rxmod`, `offload` | `hv start`'s words of those names (`offload = false` is `nooffload`) | as `hv start` |

**The name** is the folder's: a letter or a digit, then letters, digits,
`.`, `_` and `-`, at most 32 -- a word of a command and a component of a
path, with nothing in it either reads otherwise. One VM of a name is on the
list at a time: a second `hv start web` is refused while vm 0 is web, even
stopped -- `hv restart web` boots it again, `hv stop web` takes it off. Its
disks are claimed as any guest's are (`disk::Claims`), so no other guest
writes them meanwhile. A number is a VM's number before it is a name: a VM
named by digits alone is found by its name only where no VM has that number.

**`console.log`** (`modules/hv/src/vmlog.rs`) is the guest's console as it
wrote it, control sequences and all, appended a second at a time by a task
of the VM's own (`hv/vm<N>/log`) from the console's ring -- never by the
guest's CPUs, whose exits would otherwise wait on the disk. Between the
guest's lines, where they happened, hv's own: `--- hv: <date> UTC vm <N>
started on cpus 2,3 -- ... ---`, `booting again -- <why>`, `stopped --
<why>`, and `off the list` when `hv stop` -- or the unload -- takes it away.
What the ring no longer has when the task gets to it, a guest printing more
than 64 KiB in a second, is lost to the log, and the log says how much. Past
8 MiB the file moves to `console.log.1`, the one before it gone.

What it reaches the folder with is three calls of the file ABI made for it:
`kcore::fs::list`, `append` and `rename` (`kernel_dir_entry`,
`kernel_file_append`, `kernel_file_rename` in `fs/src/files.rs`), and
`remove`, which was there already.

**What it does not do yet:** read the file again at `hv restart`, which
boots with what the start read -- `hv stop` and `hv start` take a changed
file; start a VM when the module loads; or make, fetch or take away a disk
-- `truncate` makes one, `wget` fetches a raw image into the folder, `rm`
takes a folder away.

**Checked by:** `hv-linux-test.py`'s VM phase, which makes a folder with `hv
create` -- and is refused a second time -- boots it by its name, types at it
by its name, is refused a second start, finds its boot and hv's lines in its
`console.log`, boots it again, and is refused a `vm.toml` with a key it does
not know; then loads hv again with `vms=/data/vms`, makes a VM there -- and
the folder above it -- boots it and types at it by its name, finds its
`console.log` there, and is told `/vms`'s VM is none of this folder's; its
network phase (`--net`), whose VM from its folder asks for
port 5 and a forward -- its eth0 has the port's address and MAC, and its
httpd answers from outside through the forward its `vm.toml` asked for;
`hv-test.py`, whose last loads keep a `vms=` and say it, and refuse a
parameter hv has not got and a `vms=` that is no absolute path;
`hv-fuzz.py`'s `config` target, which holds the parser to never
panicking on any text and `render` and `parse` to giving each other back what
they were given, and whose `switch` target claims a port asked for as well as
the first free one; and `fs-fuzz.py`'s `vfs` target, which holds the three
file calls to its model of the tree.

## ACPI

A PC's firmware leaves its OS tables that say what the machine is, and a
kernel with ACPI reads them before anything else: where its CPUs are, how to
turn the machine off and reset it, where its PCI bus is and which interrupt
each device has. Every Linux guest is given them (`hv::acpi`), and the fixed
hardware they describe (`hv::devices::pm`). A kernel without ACPI, or told
`acpi=off`, finds nothing of it in its way and boots as it always did.

```
[    0.039122] ACPI: RSDP 0x00000000000E0000 000024 (v02 NOS   )
[    0.042362] ACPI: XSDT 0x00000000000E0240 00002C (v01 NOS    HV       00000001 NOS  00000001)
[    0.047220] ACPI: FACP 0x00000000000E0120 000114 (v06 NOS    HV       00000001 NOS  00000001)
[    0.051699] ACPI: DSDT 0x00000000000E0080 00009B (v02 NOS    HV       00000001 NOS  00000001)
[    0.056005] ACPI: FACS 0x00000000000E0040 000040
[    0.102725] ACPI: PM-Timer IO Port: 0x608
...
[    0.545105] ACPI: Interpreter enabled
[    0.545535] ACPI: PM: (supports S0 S5)
[    0.549186] ACPI: Using PIC for interrupt routing
[    0.565105] ACPI: PCI Root Bridge [PCI0] (domain 0000 [bus 00-ff])
[    0.577561] acpi PNP0A03:00: fail to add MMCONFIG information, can't access extended configuration space under this bridge
[    0.589327] pci_bus 0000:00: root bus resource [io  0x0000-0x0cf7 window]
[    0.593165] pci_bus 0000:00: root bus resource [io  0x0d00-0xffff window]
[    0.597229] pci_bus 0000:00: root bus resource [bus 00-ff]
[    0.631608] PCI: Using ACPI for IRQ routing
[    0.657669] clocksource: acpi_pm: mask: 0xffffff max_cycles: 0xffffff, max_idle_ns: 2085701024 ns
[    0.769271] input: Power Button as /devices/LNXSYSTM:00/LNXPWRBN:00/input/input0
[    0.773949] ACPI: button: Power Button [PWRF]
```

(The gate's guest of one CPU under TCG: a Linux 6.18 with ACPI, its power
button and evdev.)

- **The tables** go in the BIOS area from 0xE0000, which the e820 map
  reserves: the RSDP first, where a kernel scanning for it finds it, and the
  zero page's `acpi_rsdp_addr` pointing at it for one that looks there. An
  XSDT names the FADT and, for a guest whose CPUs have local APICs, a MADT;
  the FADT names the FACS and the DSDT. They are ACPI 6.3's, laid out with
  their checksums into a buffer and copied into guest memory as the MP
  table is -- nothing of the guest's is kept.
- **The fixed hardware is all on ports** (0x600-0x60B, as an ICH lays it
  out): the PM1 event block's status and enable registers, the PM1 control
  register, and the PM timer -- 24 bits at 3.579545 MHz, counting the host's
  time from when the guest was made. The host keeps its time by its TSC --
  under KVM by kvmclock, the TSC scaled -- and the guest reads that TSC and
  is told its rate, so the two agree; Linux, which checks its TSC against
  the PM timer every half second once it has one, finds them agreeing. The SCI is the 8259's IRQ 9, and a level: requested while an
  enabled event is set, requested again if one still is when the guest ends
  the interrupt, withdrawn when none is. There is no SMI command port -- the
  machine is in ACPI mode from the start, SCI_EN reading set -- and no GPE
  block. The edge/level control registers beside the 8259s (0x4D0) keep what
  is written to them, and start with IRQ 9 level-triggered: what a kernel
  sets them to, and, finding them so, need not warn that it did. The reset
  register is the chipset's 0xCF9, trapped already: a kernel with ACPI
  reboots through it first, `the guest asked for a reset, 0x06 to port
  0xcf9`.
- **The MADT** lists a local APIC for each CPU and NMI on every LINT1 --
  and, for a guest given one, the IO-APIC, with the two overrides a PC's
  has: the timer's IRQ 0 on its pin 2, and the SCI a level, active high
  ([The IO-APIC](#the-io-apic)). The MP table is still there: a kernel with
  ACPI finds its CPUs in the MADT, one without in the MP table, and both
  are told the same.
- **The DSDT** is the one table in AML, and small: `\_S5`, and the PCI host
  bridge. The bridge has to be there. A Linux with ACPI finds PCI through
  the namespace alone and never probes the bus -- `pci_legacy_init` is not
  even called -- so without one the guest would have no disks and no NICs.
  Its `_CRS` gives bus 0-255, the configuration ports, the I/O either side
  of them and the window its functions' MSI-X pages are in (a BAR outside
  every window is one an OS moves, and these are fixed); its `_PRT` routes
  each device's INTA to the 8259 IRQ its interrupt line register names. As
  ACPICA's `iasl` disassembles it, for a guest of several CPUs with a NIC
  and two disks:

```
Name (_S5, Package (0x04) { 0x05, 0x05, Zero, Zero })
Scope (\_SB)
{
    Device (PCI0)
    {
        Name (_HID, EisaId ("PNP0A03") /* PCI Bus */)
        Name (_UID, Zero)
        Name (_CRS, ResourceTemplate ()
        {
            WordBusNumber (ResourceProducer, MinFixed, MaxFixed, PosDecode,
                0x0000, 0x0000, 0x00FF, 0x0000, 0x0100,,,)
            IO (Decode16, 0x0CF8, 0x0CF8, 0x01, 0x08, )
            WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                0x0000, 0x0000, 0x0CF7, 0x0000, 0x0CF8,,, , TypeStatic, DenseTranslation)
            WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                0x0000, 0x0D00, 0xFFFF, 0x0000, 0xF300,,, , TypeStatic, DenseTranslation)
            DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                0x00000000, 0xFE000000, 0xFE007FFF, 0x00000000, 0x00008000,,, , AddressRangeMemory, TypeStatic)
        })
        Name (_PRT, Package (0x03)
        {
            Package (0x04) { 0x0001FFFF, Zero, Zero, 0x0A },
            Package (0x04) { 0x0002FFFF, Zero, Zero, 0x0B },
            Package (0x04) { 0x0003FFFF, Zero, Zero, 0x0B }
        })
    }
}
```

What is left out is MMIO this does not emulate yet ([MMIO](#mmio)) -- the
HPET and PCIe's ECAM, whose absence Linux names in the line above; the
IO-APIC is there for a guest given one -- and what a guest does without: processor objects (there are no C- or P-states to describe), the
ISA devices (a kernel finds a PC's serial port, RTC and timer where they
always are), GPEs, and any sleep state but S5. A guest of one CPU gets no
MADT: it has no APIC.

**`hv stop` asks first.** It presses the guest's power button -- sets the
fixed button's status, and raises the SCI if the guest's OS has enabled the
event -- and gives the guest `secs` (30 unless told) to turn itself off; one
that does ends `the guest powered itself off, S5 by its ACPI PM1 control
register`. A kernel hears the button as a key, KEY_POWER on its input
device, and what answers it is userspace's: systemd-logind shuts a
distribution down, BusyBox's `acpid` runs `/etc/acpi/PWRF/00000080`. A guest
that hears it and does nothing is stopped when its time is up, `its power
button went unanswered for 30 s`. One whose OS never enabled the event -- no
ACPI, `acpi=off`, not booted that far -- cannot hear it, and the first CPU's
loop, which takes the press from the command, says so: it is stopped at
once, `nothing in it listens to its power button`. `secs=0` stops a guest
without asking, and `hv stop all` presses every button at once and waits for
the guests together. `rmmod` does not ask: an unload is no place to wait on
guests -- a module's exit has five seconds before `rmmod` goes on without
it -- so `hv stop all` first is the way to shut guests down before one.

**A power-off is a write to a port**, as a reset is: SLP_EN with S5's sleep
type in PM1a's control register. ACPICA writes the type first and the enable
bit second, and the second stops the VM where the guest wrote it --
`Stop::PowerOff`, which is not a reset: a guest started with `restart` that
turns itself off stays off. A sleep type the DSDT does not offer is left
alone, and the report says the guest asked for it.

The gate is `scripts/hv-linux-test.py --acpi`, with a kernel built with
ACPI, its button and evdev: the boot's lines above and nothing of a firmware
bug, a TSC the watchdog marked unstable or a PM timer that failed its checks;
reboots through 0xCF9; the SCI on IRQ 9 and the PM timer a clocksource;
`hv stop` answered by `acpid` with a power-off; and a guest that hears the
button and does nothing, stopped when its time is up. The tables themselves
were checked with `iasl` as they were written. `scripts/hv-distro-test.py`
boots Alpine and Debian on their ACPI, and has `hv stop` shut Debian down --
or, with `--acpi-off`, boots them as before.

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

(Under TCG, twice emulated: 36 s to the login prompt -- recorded before the
machine had ACPI, when a distribution was booted `acpi=off`.) Its initramfs
loads virtio_blk, finds the ISO on `vda`, mounts it and installs the base
system from the ISO's packages into a tmpfs, and OpenRC brings it up to a
getty on ttyS0. `nolapic` makes it a PC of one CPU with no local APIC,
taking its interrupts from the 8259s. With `acpi=off`, as above, it finds
its PCI devices through configuration mechanism 1 and each virtio device's
interrupt line in its configuration space; without, as the gate boots it
now, it takes both from its ACPI tables -- the DSDT's host bridge, and its
`_PRT` ([ACPI](#acpi)) -- and is at its login prompt 14 s after the VM
starts, nested under KVM on the i5-13500. On the switch (`net`) its
initramfs configures eth0 from the `ip=` the VM is given, and writes the
`dns0` there to `resolv.conf`; through NAT it reaches what nos reaches --
`apk update` from Alpine's own mirror, the name looked up through nos's DNS
server -- and `apk add openssh-server` installs from the ISO, its own sshd
reached from outside nos through `hv forward`. `reboot` resets it -- through
the reset register its FADT names, or with `acpi=off` the keyboard
controller -- and with `restart` it boots again.

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

(The transcript above is from before the machine had ACPI, booted `acpi=off`
under TCG.) On its ACPI, as the gate boots it now, systemd-logind hears its
power button, and `hv stop` shuts it down -- its services stopped, its root
unmounted -- and it turns itself off, nested under KVM on the i5-13500:

```
$ hv stop 0 secs=300
hv: vm 0 stopped -- the guest powered itself off, S5 by its ACPI PM1 control register, at 0xffffffff8f1d5ae3
  stopped    the guest powered itself off, S5 by its ACPI PM1 control register, at 0xffffffff8f1d5ae3, after 7624 ms
  ...
  acpi       taken by the guest's OS; 2 SCIs, the power button pressed 1 times
  vda        3072 MiB: 2499 reads (75589 KiB), 215 writes (3805 KiB), 36 flushes, 0 errors
```

-- 7.6 s for that whole boot, from its start through the login, the gate's
commands and the shutdown.

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

## A guest's own kernel: PVH and hvboot

A guest booted as above runs the kernel it was handed, at every restart
again: its own `/boot` is never read, so a distribution's kernel updates
are never booted, and its kernel and initrd have to be taken out of the
image first. A guest booted by **hvboot** runs the kernel its own disk's
GRUB would, as a cloud's does:

```
$ hv start /lib/hvboot mem=1024 disk=/vms/ubuntu/ubuntu.raw disk=/vms/ubuntu/seed.iso:ro net restart
hv: vm 0 started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3", restarted when it resets
$ hv console 0
hvboot: started by PVH: 1023 MiB of RAM, its command line "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3 tsc_early_khz=1000019"
hvboot: vda16/grub/grub.cfg: "Ubuntu", the default, 0
hvboot: vda16/vmlinuz-6.8.0-147-generic: boot protocol 2.15, 14710 KiB at 0x1000000, initrd 29988 KiB at 0x3e2b6000, read in 497 ms
hvboot: its command line: BOOT_IMAGE=/vmlinuz-6.8.0-147-generic root=LABEL=cloudimg-rootfs ro console=tty1 console=ttyS0 console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3 tsc_early_khz=1000019
[    0.000000] Linux version 6.8.0-147-generic (buildd@lcy02-amd64-...) ...
```

-- Ubuntu 24.04's cloud image as it ships, made raw, and its NoCloud seed:
no kernel, initrd or command line taken out of it. hvboot is at the kernel
0.6 s after the VM starts, under TCG; the guest at its login prompt 150 s
in, as with its kernel taken out by hand (138 s), the rest of the boot
being the guest's own. Debian 13's `nocloud` image boots the same way,
hvboot at the kernel 0.6 s in and the login prompt 34 s in, its root
found by the PARTUUID its own grub.cfg gives and its systemd credentials
the VM's `cmdline=`, which goes after the entry's line. After `apt upgrade` installs a kernel and
`update-grub` names it, the next boot -- a restart included, the VM being
built again from its files -- boots that one.

**PVH** (`hv::pvh`) is how hv starts it. `hv boot` and `hv start` take an
ELF as well as a bzImage, and tell them apart by the file's first bytes:
an ELF is booted by the x86/HVM direct boot ABI Xen made (its
`docs/misc/pvh.pandoc`), which Linux's `vmlinux`, QEMU's `-kernel`, Cloud
Hypervisor and the firmware written for them -- rust-hypervisor-firmware,
OVMF's `CloudHv` -- speak. Its loadable segments go at their physical
addresses; the entry is the one its `Xen` note of type 18
(`XEN_ELFNOTE_PHYS32_ENTRY`) names; and hv lays out an `hvm_start_info`
where a bzImage's zero page would be -- the command line (with what hv adds
a bzImage's: the TSC's rate, `noapic`, `ip=`), the initrd as its one
module, the ACPI RSDP, and the memory map, the same e820 a bzImage is given
(`linux::e820_entries`, one list for both). A guest of several CPUs gets
the same MP table. Its first CPU starts at the entry in 32-bit protected
mode -- flat segments, paging and interrupts off, `EBX` at the start info:
what reset leaves but for CR0's PE (`svm::protected_mode`, for both
backends). Paging off takes unrestricted guest under VT-x, as a guest of
several CPUs' real-mode start does already, and a start where the CPU has
none is refused, saying so.

The image is a file of nos's, and its headers are checked before anything
in them is used: ELF32 or ELF64, at most 64 program headers and 4 KiB of
notes read; a segment outside the file or outside the guest's RAM above the
tables written below 1 MiB, two segments over the same bytes, an entry that
is not in a segment's file bytes, or two entries -- each refused.
`hv-fuzz.py`'s `pvh` target holds the reader to that over ELFs of either
class made from its input, the start info to what the guest was given --
its magic, its one module, a memory map of RAM up to the guest's last byte
in order -- and the entry to paging off. hv's unsafe count is still 4.

**hvboot** (`src/rust/hvboot`, built by `make` and kept as `/lib/hvboot`
in the system image) is a PVH ELF of 164 KiB, linked at 1 MiB, that runs in
the guest. It reads:

- its start info: the memory map, the RSDP, and its command line --
  `hvboot.entry=` (an entry by number, `N>M`, title or id, as GRUB's
  `default` takes one), `hvboot.disk=N` (look on that disk alone) and
  `hvboot.verbose` its own, every other word for the kernel (`start.rs`);
- the guest's legacy virtio disks on PCI bus 0, a request at a time with
  the interrupt never taken: each poll reads the ISR, a port, and the exit
  that takes is where hv hands back what is done -- a request not answered
  in 30 s is said, not waited on (`virtio.rs`);
- each disk's partitions, from its GPT -- header and array each checked by
  their CRC -- or an MBR, an XBOOTLDR partition (a /boot of its own, as
  Ubuntu's) first, and a disk with no table looked at whole (`gpt.rs`);
- ext2, ext3 and ext4, read-only: extents and block maps, 64-bit
  descriptors, flex_bg, htree directories read as the linear ones they
  also are, inline data, symlinks, holes; an incompatible feature it does
  not read is refused by name, and the journal is not replayed, as GRUB
  does not replay it (`ext.rs`);
- the first `/boot/grub/grub.cfg` or `/grub/grub.cfg` (or grub2's) on
  them, and grubenv beside it: the menu, submenus too, each entry's `linux`,
  `initrd` and the `search` or `set root` naming its filesystem -- by UUID,
  label, file or `hdN,gptM` -- and the default, grubenv's `next_entry` and
  `saved_entry` where the script defers to them, as grub-mkconfig's does.
  It is not GRUB's interpreter: it reads the script's words as GRUB does --
  quotes, escapes, comments, variables -- and its blocks, but runs no
  condition; every branch of an `if` is read, the last `search` and `linux`
  an entry has win, and a variable has a value only where a `set` outside a
  condition gives it one. What it cannot place it says (`grub.rs`).

It loads the kernel at its `pref_address` and the initrds high, one after
another at GRUB's 4-byte alignment, into RAM the memory map has, clear of
itself; builds the zero page -- the start info's memory map as its e820,
the RSDP -- and the command line GRUB would give, `BOOT_IMAGE=` first, then
its own line's words after the entry's; resets the disks; and enters the
kernel's 64-bit entry, as hv enters a bzImage (`bzimage.rs`, `boot.rs`).
Whatever it cannot do it says on the console and turns the guest off through
its ACPI -- the VM stops with "the guest powered itself off", the reason in
its console, rather than sitting halted with nothing said; a fault in it is
said too, with where it happened.

It runs in the guest, and what it reads is the guest's own: a bug in it is
the guest's, not nos's, and the host's half of this is the PVH loader
above. Its parsers -- `gpt`, `ext`, `grub`, `bzimage` -- are plain Rust over
byte slices with no `unsafe`; its 29 unsafe sites are the platform's, in
ports, physical memory, the heap, the virtio rings, the IDT and the jump.
The parsers are compiled for the host too (`fuzz/hvboot`): read off Ubuntu
24.04's and Debian 13's images, the kernels and initrds come out byte for
byte as `debugfs` reads them, and `scripts/hvboot-test.py` holds them to
the entry GRUB would boot on disks it makes -- GPT, MBR and none; ext4,
ext3, ext2 of 1 KiB blocks into the double indirect ones; a default, a
`next_entry`, a `saved_entry` into a submenu -- and fuzzes each disk's
metadata.

What it does not do: boot what is not Linux -- Windows has no boot
protocol of its own to be entered by, and needs firmware, which OVMF's
`CloudHv` on the same PVH would be; read XFS or btrfs, so not a /boot of
the RHEL family's, nor an LVM or LUKS one; read BLS entries (`blscfg`);
or EFI -- the kernel is entered by the boot protocol, as hv enters a
bzImage, and has no EFI runtime. And the image is still to be made raw: the
disk is a file of nos's, as any guest's.

The gates are `hv-fuzz.py` (the `pvh` target), `hv-test.py` (hvboot
started by PVH with no disk: it reads the start info, says it has no disk
and turns the guest off, on whichever backend the machine has),
`hvboot-test.py` (host, in CI) and `hv-distro-test.py --ubuntu ...
--debian ... --hvboot` (manual): the same checks as without, a guest
provisioned, networked, rebooted -- hvboot run again -- and shut down, and
two more: hvboot reached the kernel, and booted the entry the image's own
grub.cfg makes its default.

## More than one CPU

`cpus=N` on `hv boot` and `hv start` gives a guest N CPUs, up to 16. Each is
a task of its own on a host CPU of its own, and where they go is
`hv::place`'s to say, over `hv::topology` -- which reads a CPU's core and
cache from its APIC ID and CPUID, and on a hybrid part asks each CPU which
kind of core it is (leaf 0x1A) -- by the rule `hv cores` sets. Two CPUs of
one guest never share a host CPU -- a guest CPU spinning while it waits for
another would hold the host CPU the other needs to finish -- so a guest has
at most as many CPUs as `hv on` has turned the extension on for.

**A guest's cores are its own** (`hv cores own`, the default). Two CPUs on
one core's two threads share its execution units, its caches and its
buffers -- what every SMT side channel stands on, MDS, L1TF and port
contention among them -- so a guest is given whole cores no other guest has
a CPU on, its vCPUs or the task that serves its disks, and its vCPUs fill
their threads: a vCPU a thread, as a cloud sells them, where a vCPU a core
would leave a thread idle beside each. On a hybrid part all of a guest's
vCPUs are of one kind of core where one kind has room for all of them --
the faster, or the kind of the CPU `cpu=` names -- since a guest's kernel
takes its CPUs for equals, and an EX44 E-core runs a busy loop 16% slower
than a P-core at one clock and 58% slower at their turbos; both kinds only
where neither has room alone. The first where `cpu=` says, or on the free
core with the most threads -- CPU 0's last, where the boot CPU's own work
runs, and the higher threads of a core first, CPU 0 the last of all -- then
the threads left on it, then on cores sharing its last-level cache, the
faster kind first. Its disks' task goes on a spare thread of its own cores,
then on a core no guest has, of the slower kind where its cache has one --
the task waits on the disk, and a fast core is a guest's; it then holds that
core as well -- then beside a vCPU of its own. A guest
that would need a core another has -- `cpu=` on one, or more CPUs than the
free cores have threads -- is refused, saying so. The place and the hold on
it are taken under one lock (`modules/hv/src/cores.rs`), so two guests
started at once from two shells cannot both be given a core; an `hv start`
guest holds its cores until `hv stop` -- parked too, for the restart that
runs it there again -- and an `hv boot` guest until its run is over. `hv
cores` says the rule and who holds which CPUs; `hv list` says where each
guest's disks are served.

What it costs: a guest's own CPUs share their core. On one core's two
threads (13 and 12 on the AX41) two busy loops in a Debian guest took 14.7 s
where one alone took 9.7; on two cores of one CCX (13 and 11) they took 9.7,
and four loops on a guest of four CPUs (13, 11, 9 and 5) 10.0 -- the
placement `hv cores shared` gives: the first on the least loaded core, of
the faster kind where loads tie, each other on a core none of the guest's
CPUs is on yet where there is one, sharing the first's last-level cache
where it can, of the first's kind, the least loaded of those, whoever else
is there. That is for a machine whose guests are all one's own.

```
$ hv boot /bzImage mem=256 secs=60 cpus=2 initrd=/initrd cmdline=earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check
  --- ttyS0 ---
[    0.000000] Linux version 6.18.54 ...
[    0.000000] x2apic: enabled by BIOS, switching to x2apic ops
[    0.000000] MPTABLE: APIC at: 0xFEE00000
...
[    0.004000] APIC: Switched APIC routing to: cluster x2apic
...
[    0.388000] smp: Bringing up secondary CPUs ...
[    0.388000] smpboot: x86: Booting SMP configuration:
[    0.412000] smp: Brought up 1 node, 2 CPUs
...
nos-guest: init is up, / on ramfs, BusyBox v1.30.1
nos-guest: 2 cpus, online 0-1
  --- end ttyS0 ---
  stopped    by the host, its time up, after 60006 ms
  ...
  apic       30963 interrupts from the local APICs, 29663 of their timers; 1305 IPIs sent, 1300 taken
  cpus       2, 1 started by the guest (1 INITs, 1 start-up IPIs taken)
  cpu 0      60310 exits, 15477 apic irq, 679 ipi sent, 6593 hlt, slept 97% -- halted, on host cpu 3
  cpu 1      31923 exits, 15486 apic irq, 626 ipi sent, 6629 hlt, slept 97% -- halted, on host cpu 2
$ hv start /bzImage mem=256 cpus=2 initrd=/initrd cmdline=earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check
hv: vm 0 started on cpus 3,2 -- /bzImage, 256 MiB, 2 cpus, cmdline "earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check"
$ hv list
vm 0  running  cpus 3,2  256 MiB  0 s  exits 0  irq 0  hlt 0  kicks 0  /bzImage
$ hv exec 0 grep -e LOC -e RES -e CAL /proc/interrupts
LOC:        266        223   Local timer interrupts
RES:         11         15   Rescheduling interrupts
CAL:        125        144   Function call interrupts
```

(A Linux 6.18 built for the gate -- `hv-linux-test.py --cpus 2` on the
AX41, nested under KVM; its output trimmed. `noapic` goes on the command
line by itself, below.)

A guest of one CPU is given exactly the machine it always was -- no APIC in
CPUID, no MP table, `nolapic` on its default command line -- so a kernel that
booted before boots the same. What a guest of more is given, and how its CPUs
run, is below. `hv::run` holds the loop and the platform, `hv::lapic` the
local APIC, `hv::smp` the mailboxes and doorbells, and `hv::linux` the MP
table.

### The machine it sees

**A local APIC for every CPU, as an x2APIC unless told** (`hv::lapic`).
The x2APIC is its registers as MSRs 0x800-0x8FF, and an MSR access is an
exit whose register and value the CPU hands over; the xAPIC is the same
registers as a page of MMIO at 0xFEE00000, each access a nested fault whose
instruction has to be decoded ([MMIO](#mmio)). So the APIC comes out of
reset with x2APIC mode already on, `IA32_APIC_BASE` saying EN and EXTD, as
firmware leaves it on a machine with more than 255 CPUs, and a kernel that
keeps it never touches the page -- or, with `xapic`, in xAPIC mode, as most
PCs' firmware leaves it. A kernel may move between the modes as the
architecture allows -- off and back, as `nox2apic` has Linux do -- and the
APIC answers in the mode it is in. The nested table never maps the page: a
guest that reaches for it in x2APIC mode, when it is none of the APIC's, is
stopped and told why rather than answered wrongly. The boot CPU's APIC starts in virtual-wire mode, with
the spurious-vector register enabled, LINT0 ExtINT and LINT1 NMI, as
firmware hands it over. What the model has is the register file and what
each register does to the others: the ID and the logical ID made from it
(x2APIC cluster mode), the task and processor priorities, the in-service,
request and trigger-mode registers, EOI -- a level-triggered interrupt's
going back to the IO-APIC it came from ([The IO-APIC](#the-io-apic)) --
the error register, the local vector table, the interrupt command
register, and the timer (below). A write of the spurious
register that turns the APIC off masks every LVT entry, as the silicon does.

**An MP table** (`hv::linux`, spec 1.4) in the last kilobyte of low
memory, at 0x9FC00, where the e820 map has ended RAM and where Linux looks:
a processor entry for each CPU, the first marked the boot CPU; the PCI bus
and an ISA bus; an IO-APIC entry, marked usable only for a guest given
one; and the interrupt assignments -- the 8259's output on the IO-APIC's
pin 0 and on LINT0 of the first CPU, each ISA IRQ but the cascade on the
IO-APIC's pin of its own number but the timer's, which is on pin 2, each
PCI function's INTA on the pin of its line, level-triggered and active low
(the ISA entry of that line left out), and NMI to LINT1 of every CPU.
Without the assignments Linux complains of a BIOS bug and makes up its
own. The MP table is the least a kernel finds
its CPUs by; one with ACPI finds them in the MADT ([ACPI](#acpi)), which
says the same, and takes the MP table only for what the MADT leaves out.

**No IO-APIC, unless asked** ([The IO-APIC](#the-io-apic)). The 8259
pair stays wired to the first CPU's LINT0 in virtual-wire mode, so every
device interrupt -- the PIT's tick, the serial port's, the disks' and the
NICs' -- is the first CPU's, and its task does the devices' work between
its guest's turns, as it did for a guest of one CPU. `noapic` is added to
the command line when it is not there already. Without it, x86-64 Linux
looks for an IO-APIC whatever the table says -- it ignores the table's
`pic_mode` -- and on the way `setup_local_APIC` masks LINT0 on the boot
CPU, and the 8259 is cut off from the only CPU it reaches. With `ioapic`
the machine has one, and the command line is left as it is.

**CPUID.** Leaf 1 says the local APIC is there, and the x2APIC, and gives
each CPU its APIC ID in EBX[31:24]; leaf 6 says the timer keeps running in
deep C-states (ARAT), which is true of one counted off the host's clock; and
where the host's own leaf 0x15 gives its TSC's frequency -- an Intel host's
does -- the guest's gives the timer's clock, a crystal of 1 GHz, and the
TSC's ratio to it, so a kernel need not measure either. Passed through, the
leaf would name the host's crystal, tens of MHz, and a kernel that took the
timer's rate from it would have its timer fire tens of times too soon;
where the host has no frequency to give, the leaf is blank and the kernel
measures. The topology leaves stay blank, as for one CPU. Of leaf
0x80000007 only the invariant TSC is left: its RAS bits (SUCCOR among them)
sent Linux's machine-check code to set up AMD's deferred-error interrupt
through the APIC's extended LVT -- MSR 0x852, a register this APIC has not
got -- and the #GP came back as an "unchecked MSR access" and two firmware
bugs in its log. A guest on an AMD host reads DE_CFG's LFENCE-serializing
bit set, as KVM answers it, and HWCR's TscFreqSel set, as every AMD CPU
since family 10h has it; read as zero, that one was a "TSC doesn't count
with P0 frequency!" firmware bug of its own.

**What it needs of the guest's kernel**: Linux 6.6 or later. Before 6.6, a
kernel given an MP table and an x2APIC already on reads its APIC ID through
the xAPIC's page before it switches to x2APIC operations
(`register_lapic_address` -> `read_apic_id` -> `native_apic_mem_read`), and
faults on the page the guest does not have -- a 5.15 kernel oopses there
before its first line of SMP output. And `CONFIG_SMP`, `CONFIG_X86_X2APIC`
(which needs `CONFIG_IRQ_REMAP` or `CONFIG_HYPERVISOR_GUEST`) and
`CONFIG_X86_MPPARSE`. A distribution's kernel has all of them: Alpine's
`virt` 6.18 brings up two CPUs as it ships. An older kernel still boots on
one CPU.

### Starting the others

The first CPU starts at the kernel's 64-bit entry, as a guest of one CPU
does. The others start the way a PC's do: waiting for a start-up IPI. INIT
puts a CPU back in that state -- its registers the reset state's, anything
it was about to be injected dropped, its local APIC reset -- and a start-up
IPI starts one that waits, in real mode at the vector times 4 KiB, CS the
vector shifted left by eight. A second start-up IPI finds the CPU running
and is ignored, as the silicon ignores it: Linux sends two. From there
Linux's trampoline climbs to long mode itself, through protected mode, PAE
and paging, and joins the kernel.

- **Under AMD-V** real mode needs nothing new: with nested paging the
  CPU translates the guest's physical addresses through the nested table
  whatever the guest's own paging is doing. A CPU that is started is given
  a fresh ASID, so nothing translated for it before the reset is used.
- **Under VT-x** real mode needs *unrestricted guest*, the secondary control
  that lets a guest run with paging, or protection, off: a CPU without it
  refuses a guest of more than one CPU (`NoUnrestrictedGuest`) rather than
  starts one whose other CPUs could never start. And the climb is a guest
  changing CR0.PE and CR0.PG, which VMX will not let a guest do behind the
  host's back where the "IA-32e mode guest" entry control has to agree with
  EFER.LMA. So PE and PG are in CR0's guest/host mask with NE, a `mov` to
  CR0 or an `lmsw` that would change one of them exits (`Exit::Cr0Write`),
  and the policy does what the CPU would: the checks that make it a #GP --
  bits 63:32 set, NW without CD, PG without PE, paging on in long mode
  without PAE, paging off from 64-bit code or with PCIDs on -- LMA set or
  cleared as paging goes on or off with LME set, the entry control made to
  follow at the next entry, and what the TLB holds under the guest's VPID
  dropped, as the `mov` would drop it. That also lifts what the VMX
  backend could not do before: a guest can leave long mode, the way
  `kexec` does -- which nothing here has run yet.

QEMU's TCG before 9.2 does not put the accesses of a guest with paging off
through the nested table at all -- it takes a guest physical address for a
host one -- so a CPU started in real mode there runs out of nos's own memory
and corrupts it. `hv-test.py` and `hv-linux-test.py --cpus` refuse an older
QEMU under TCG; KVM, and a CPU's own extension, have always done it right.

### What the CPUs share, and how they reach each other

Each CPU's own state -- its registers, its local APIC -- is its task's
alone, and may be inside the CPU under a VMCB or a VMCS at any moment. What
the CPUs share is guarded by what it is:

- **Memory** is copied into and out of by every CPU at once, as the guest's
  own CPUs write it: `GuestMemory`'s accessors take `&self`, and each copy
  goes through the calling CPU's own slot with interrupts off
  (`kcore::frame`), with no reference into a guest page anywhere. The
  nested table is one for all of a guest's CPUs, and grows under a lock --
  the read-only page answering a read of a device the platform does not
  have (`map_absent`) is mapped once, whichever CPUs fault on it together.
- **The devices** are one platform under one lock, taken for each port
  access and for each round of the first CPU's device work.
- **What one CPU sends another** -- a fixed interrupt, an NMI, an INIT, a
  start-up IPI -- goes into the target's mailbox (`hv::smp`), lock-free: a
  bitmap of the vectors sent and a word each for an NMI, an INIT and a
  start-up IPI, taken whole by the target before it next enters. The
  sender then rings the target's doorbell, which wakes its task if it
  sleeps and, if it is in its guest, interrupts the host CPU it runs on so
  that it leaves (the same `Kick` the switch uses for a frame).
  The vCPU marks itself on its way in before its last look at the mailbox,
  and a sender posts before it rings, so a message is either found by that
  look or rings a vCPU that is in its guest or about to be.

The interrupt command register sends to a physical or a logical
destination -- a cluster and a bitmap of its CPUs, x2APIC's only logical
mode -- or by shorthand, to itself, to all, or to all but itself; fixed and
lowest-priority interrupts (the latter to the first CPU it names), NMIs,
INIT and start-up IPIs. The first CPU takes interrupts from two places, the
8259 through LINT0 and its own APIC, and when both have one they take turns,
so that neither can keep the other out. The APIC's task priority is CR8's,
under AMD-V through the VMCB's `V_TPR`, and under VT-x the virtual APIC's
VTPR (below) -- or, on a CPU without one, through the CR8 exits that keep
the host's own CR8 out of the guest's reach.

An NMI one CPU sends another needs NMI blocking to be the guest's own: set
as the NMI is injected, ended by the guest's IRET, and the next one held
until then. Under VT-x that is *virtual NMIs*, with the NMI-window exit to
inject a held one the moment the guest's IRET unblocks it; under AMD-V it is
the IRET intercept, whose exit comes before the IRET has run, so blocking
ends once the guest's RIP has moved past it -- what KVM does.

**Halting.** A CPU that halts with interrupts on is not entered again until
an interrupt it can take is pending, its task asleep meanwhile, as for one
CPU. A CPU halted with interrupts off, or waiting for a start-up IPI, can
be woken only by another CPU. It sets its bit in a mask of such CPUs as it
goes to wait -- and looks at its mailbox once more after, for mail that
came meanwhile -- and clears the bit as it wakes, before it takes anything.
So a set bit is a CPU that has sent nothing since it last found its mailbox
empty, and a CPU whose own bit makes the mask every CPU's, and that then
finds every mailbox empty and the mask still whole, knows nobody is left
awake to send anybody anything: the guest has stopped -- `halted` in its
report, as a guest of one CPU that runs `cli; hlt` is. The order is the
point: with the bit cleared by whoever sent the mail instead, a CPU that
took its mail before the sender got there would run with its bit set, and
the last of the others to halt could stop a guest that is running.

### Interrupts the CPU delivers itself: VT-x's APIC virtualization

Where the CPU has the TPR shadow, x2APIC virtualization and
virtual-interrupt delivery -- `hv info`'s *APIC virtualization*; the EX44's
i5-13500 has them, and KVM hands them to a nested hypervisor -- each of a
guest's CPUs under VT-x has a *virtual APIC*: a page of its own
(`hvarch::x86::vmx::Guest`) whose task priority, request and in-service
registers (VTPR, VIRR, VISR) the CPU itself works. CR8 is VTPR, for every
guest, and no `mov` of it exits.

By default where the CPU has posted interrupts too -- `hv info` says --
and for a guest that asks for it (`apicv` on `hv boot` or `hv start`;
`noapicv` turns it off), while the guest's local APIC is in x2APIC mode
and on, the run loop turns virtual-interrupt delivery on for each entry
(`hv::vmx::Vcpu::load_apic`): where the APIC stands -- what it has
requested and in service, and its task priority -- goes into the page, and
the CPU, not the run loop, delivers the highest request the priorities let
through as soon as the guest can take it, and performs the guest's EOIs,
its writes of the task priority and its self-IPIs on the page, none of
them an exit. At the exit the run loop takes the page back into the APIC
before anything looks at it (`Lapic::from_cpu`), so the rest of the loop --
a halted CPU's wake-up, a read of IRR, an IPI's acceptance -- sees what the
CPU did. The MSR bitmap lets the guest's writes of those three registers
through only while this is on; every read of an x2APIC register still
exits, since without APIC-register virtualization a read let through would
be of the CPU's own APIC. An EOI of a level-triggered vector -- an
IO-APIC's, which it has to hear of to send again while its line is up --
the CPU still stops the guest after (the EOI-exit bitmap, from the APIC's
trigger-mode register: `Exit::Eoi`). In xAPIC mode, with the APIC off or
software-disabled, on a CPU without APIC virtualization, under AMD-V
(whose AVIC is not used) and for a guest that turned it off, the run loop
injects every interrupt as before.

**Posted interrupts.** What comes from outside the CPU -- another CPU's
IPI, a device's MSI -- each guest CPU takes posted: its fixed, edge-triggered
interrupts go into a descriptor of its own (`hvarch::x86::vmx::PostedInterrupts`,
64 bytes the VMCS names), and if it is in its guest, or on its way in, the
sender sends the CPU it runs on the notification vector, 0xFC
(`kcore::cpu::notify`, `Kick::notify`): the CPU takes what was posted into
VIRR there and delivers it, no exit. A CPU out of its guest is rung as
before, and folds what was posted into its APIC before it next looks at it
(`take_posted`); one that left its guest just then takes the notification
as a host interrupt that only ends itself. Level-triggered interrupts, NMIs,
INITs and start-up IPIs still go through the mailbox and a kick. Posted
interrupts need every external interrupt's exit to acknowledge the
interrupt -- the host's, delivered to nothing yet -- so `hvarch` hands each
to the host by hand, by an `int` of its vector through the host's own IDT,
once the CPU's entries are let go of, since its handler may switch tasks
(`deliver_host_interrupt`).

**Why the default waits for posted interrupts.** Without them an interrupt
for a CPU in its guest has to kick it out to be handed over, and with no
EOI exit for the run loop to find the next one's mail at, more of them do:
a kick, an IPI and an exit, is dearer than the EOI's exit it replaced. On
the EX44, two guests of two CPUs sending 500 MB of TCP to each other through
the switch, 13 runs each way: without posted interrupts the guests exited
25-50% less but took 5.88 s on average (4.3-6.8 s) against 5.70 s (5.5-5.8
s) -- 4.3 s when the sending guest took its NIC's interrupts on its first
CPU, whose task does the devices' work and posts the MSI to itself, and
5.9-6.8 s when it took them on its second, its host-interrupt exits going
from 132,000 to 179,000. With posted interrupts, 10 runs each way, taken
in turns: 3.7-5.9 s (4.85 on average) against 5.6-5.8 s (5.72), faster in
9 of the 10, the two guests exiting 0.67-1.58 million times against 2.3-2.6
million.

**Halt polling** -- a halted CPU watching for an interrupt a while before
its task sleeps -- is not a lever on that load; what looked like one was
interrupt moderation, by accident. Measured apart, posted interrupts on,
two runs each: a 200 us watch of a CPU's mailbox and its doorbell took the
transfer from 3.9-5.1 s to 6.3-6.5 s, a 50 us one to 4.7-6.0 s. A watch
hands each frame's interrupt over at once, so the receiving guest takes one
frame an interrupt (1.00, against 1.4-2.0 with no watch) and acknowledges
every one or two, 265,000-277,000 ACKs against 111,000-151,000, and the
sender's TCP, answered that often, sends more and smaller frames -- each
one more exit on both sides. The 200 us watch that had taken the transfer
from 5.7 s to 3.9 s watched the mailbox alone: on the receiving guest's
first CPU, halted, it left the doorbell the switch rings for the devices'
work unanswered until it ran out, so the frames for that guest waited up to
200 us and went into its ring together -- 18.9 an interrupt, 41,000 ACKs,
3.9 s; where that CPU was not halted nothing waited, 1.8 an interrupt and
4.9 s. Adaptive watches came to 5-7 s for the same reason: they shrank the
first CPU's watch to nothing, and answered the rest at once. Moderation
done on purpose -- an interrupt held a bounded while for the frames behind
it -- needs a host timer finer than the tick, which there is now ([the local
timer](scheduler.md#the-local-timer)): the same wait slept rather than
spun, which the 100 Hz tick made 5-10 ms, held 260-290 frames an interrupt
and took 12.5-14.6 s, the TCP window run dry. It is in the tree now, at 100
us ([A network](#a-network)); halt polling is not.

The `apicv` built-in guest asks for it, and holds both ways to one result:
self-IPIs held by CR8 and taken by priority, one nested, IRR and ISR read
back, and three levels from the IO-APIC ended by the x2APIC's EOI. The
`posted` guest's second CPU sends its first a thousand IPIs while it spins:
taken in its guest, each notified, where the CPU has posted interrupts --
on the EX44 the first CPU left its guest 9 times -- and handed over by the
host elsewhere. The hv fuzzer's `platform` target models the CPU's side --
delivery by priority, the EOI, task priority and self-IPI writes it
performs, the EOI exit, a posted interrupt taken in the guest and the
notification's exit where it is not -- and holds the run loop to losing
nothing the CPU left in the page between an exit and the next entry, to
taking what was posted before every entry and to never waiting with it
untaken.

### Time

**A timer's edge, on time.** A guest's timers -- the PIT's channel 0 and
each CPU's local APIC timer -- have their next edge armed on the host CPU
the vCPU runs on: as its task sleeps, halted, until the edge
(`Event::WaitFor`, whose deadline arms the CPU's one-shot timer -- [the
local timer](scheduler.md#the-local-timer)), and before each entry into
the guest (`kcore::cpu::timer_at`), so that a guest computing with no exit
is taken out of it at the edge, by a host interrupt, and handed its own then.
Both used to wait for the host's next tick, at 100 Hz: on the EX44 a halted
guest's `usleep 1000` took 10 ms, a hundred of them 0.99 s, and a CPU
computing with no exit was given its ticks in bursts of up to ten. Now a
hundred take 0.16 s -- halted, and with a CPU-bound loop running in the
guest -- on one CPU, whose timer is the PIT, and on two, each with its APIC
timer delivered by the CPU itself; twenty of `usleep 100000` 2.01 s, where
they took 2.19. The owed periods below keep the count right whatever the
host does, and PAUSE exits keep a spinning CPU prompt. hv-fuzz's stand-in
CPU leaves its guest at the deadline it was armed for, as a host interrupt.

Each local APIC has its timer: one-shot and periodic, counting down at a bus
clock of 1 GHz -- KVM's, and one that makes a count nanoseconds -- with the
divide configuration a guest writes. A periodic timer that fell behind is
owed its periods, handed over one at a time as the guest takes each, a
second's worth at most, as the PIT's channel 0 is. There is no TSC-deadline
mode, and CPUID says so.

A kernel measures the timer against the PIT before it trusts it
(`calibrate_APIC_clock`), and while it measures it spins, `cpu_relax()` --
PAUSE -- round a loop that watches the PIT's ticks go by. A spinning guest
made no exit, so it left its guest only at the host's own tick, every 10 ms,
and took the PIT's ticks owed since in a burst there: Linux counted ten
ticks' worth of APIC timer as one tick, reported a bus of 10005 MHz where
there is one of 1000, then saw its APIC timer run ten times too slow and
refused it (`APIC timer disabled due to verification failure`, `jiffies
delta = 1001`). So PAUSE exits now -- under VT-x at a loop of them in the
guest's kernel that has spun 65536 TSC ticks (pause-loop exiting, 26 us at
the EX44's 2.5 GHz, each PAUSE within 1024 ticks of the last), and at every
one where the CPU has no pause-loop exiting, as a nested KVM may not pass it
on; under AMD-V every 128th where the CPU has a pause filter and every one
where it has not -- and each exit goes round the loop that hands a tick over
as it falls due. An exit at every PAUSE cost a spinning CPU an exit's 1.8
us for each PAUSE of tens of cycles. The gap is wider than KVM's 128 ticks:
a loop's turn on Skylake and later, whose PAUSE alone is some 140 cycles of
a core that may run below its TSC's clock, is longer than that, and a gap
too short finds no loop at all. Measured on the AX41, the guest finds a bus of 999 MHz and keeps
its timer (`jiffies delta = 100`, `jiffies result ok`), as it does on an
Intel host (an i5-13500, nested under KVM). A CPU spinning on a lock
another of the guest's CPUs holds comes out the same way, and is handed its
tick when it falls due rather than at the host's next one.

Under TCG a guest refused it once more before it was told its TSC's rate
(below). The check allows two ticks of 100, and a guest there is slow
enough that the PIT's ticks owed from the lines it printed with interrupts
off just before -- through a UART that is itself exits -- were still being
handed over when the check started: it counted 106. It then kept its PIT
tick on the first CPU and broadcast it to the others by IPI, which works,
and costs an IPI a tick per CPU. Told the rate, it has kept its timer in
both runs since.

**The TSC's rate, handed over.** A kernel on an AMD host measures its TSC
against the PIT, every read an exit, and gives up if one read takes ten
times the fastest -- which a vCPU sharing its host CPU with the host's own
work (sshd, the network stack; cpu 0's, on the AX41) did one boot in two,
before SMP and after, and ran on jiffies. So every Linux guest's command
line gets `tsc_early_khz=` with the host's TSC rate, which is the guest's:
nothing offsets or scales the TSC a guest reads. Linux takes it as
authoritative from 5.7 on; `hv` leaves one the command line gives alone.

**Two CPUs' TSCs agree.** Placed on two CCXs of the AX41 (0 and 13), Debian
measured its CPUs' TSCs 2052 cycles apart -- "turning off TSC clock" -- where
Ubuntu, on the same CPUs, found them in step. Linux's check reads the TSC
with `lfence; rdtsc` after taking a lock the other CPU held, and trusts the
LFENCE to hold the RDTSC back until the load before it has completed. On an
AMD CPU of families 10h to 17h LFENCE does that only with DE_CFG bit 1 set,
which Linux sets on every CPU and nos did not, and the firmware had not:
the RDTSC ran ahead of a cache line on its way from the other CCX, and the
time that took looked like a warp. nos sets the bit on every CPU now, the
BSP's boot log saying when the firmware left it clear
(`Hal::SetupSerializingLfence`); a guest of four CPUs across both CCXs (13,
11, 9 and 5) keeps its TSC.

### Device interrupts, by MSI-X

A guest with local APICs has its disks and NICs interrupt it by MSI-X, as a
PCI device on a real machine does, and not through the 8259. Each virtio
function has an MSI-X capability (`devices/pci.rs`) and a memory BAR 1 of
one page: the vector table at its start, the pending bits after it. That
page is guest memory -- a region of its own, at 0xFE000000 and a page a PCI
slot, in the MMIO window and not in the e820 map -- so a guest writes its
table with plain `mov`s and makes no exit doing it, and no instruction has
to be decoded. A guest's RAM ends below these pages, at 4064 MiB at most
(`run::MAX_MEM_BYTES`, what `mem=` is bounded by): more would run over them,
and over the IO-APIC's and the local APIC's, and the guest would read and
write its devices' registers as memory, none of them hearing of it -- an
APIC in xAPIC mode that is not there, and nothing to say so. The device
reads an entry when it has an interrupt to send:
the message's address and data (`lapic::msi`), a fixed or lowest-priority
vector to an x2APIC ID or, in cluster mode, to the first cluster's CPUs,
the eight an 8-bit field can name -- which is where Linux puts them without
interrupt remapping. It goes as an IPI would: into the APIC of the CPU
doing the device's work, or another CPU's mailbox, with a ring. An entry
masked, or the whole function, sets its pending bit and is sent once it is
unmasked. With MSI-X on, virtio's legacy header grows the two vector
registers, and the device's configuration moves from 0x14 to 0x18
(`devices/virtio.rs`). The BAR stays where "firmware" put it: sizing it
reads back its size, and another address written reads back the old one.

What that changes is the cost of an interrupt. Through the 8259 each costs
the guest six exits -- a mask, two EOIs, an unmask, the interrupt status
read and a dummy one -- and at a gigabit a receive interrupt came every
other frame; by MSI-X it costs one, the EOI's MSR write. And the guest
places each vector itself: Debian put its NIC's receive queue on its first
CPU and its transmit queue and its disk on its second. On the AX41 a
Debian of two CPUs fetched a gigabyte three times at 117 MB/s -- what TCP
carries over gigabit Ethernet -- with no frame dropped, its first vCPU's
host CPU 88% busy and its second's 21%, where through the 8259 the first
was 96.5% busy, the second idle, the fetch 113 MB/s and a few hundred
frames dropped a gigabyte. A guest of one CPU has no APIC, and keeps the
8259 and that ceiling: 111 MB/s and 586 frames dropped.

### What it does not do yet

- **The first CPU's task does every device's work.** Between its guest's
  turns it serves a disk's answer and a NIC's frames, with or without an
  IO-APIC; only the interrupts that follow go where the guest steers them
  -- through MSI-X, and with `ioapic` the PIT's and the serial port's too.
  Without one, those two reach the first CPU only.
- **No TSC-deadline timer.** A kernel built with neither `X86_MPPARSE` nor
  ACPI finds one CPU.

### Gates

Both run under AMD-V -- QEMU's TCG, and nested KVM on the AX41 -- and under
VT-x, nested KVM on an i5-13500, where the second CPU's climb from real mode
is the two CR0 writes the policy emulates.

- `scripts/hv-test.py` runs the built-in `smp` guest on the first and the
  last CPU: two CPUs, the second started by INIT and a start-up IPI into
  real mode, climbing to long mode and sending the first an IPI, whose
  one-shot APIC timer then interrupts it. A run with the start-up IPI
  ignored, and one with the timer never firing, both fail it.
- `scripts/hv-linux-test.py --cpus N` boots a real Linux with N CPUs --
  `smp: Brought up 1 node, N CPUs`, and the report's `N-1 started by the
  guest` -- then runs the VM phase with them: every CPU online, each taking
  its own local timer's interrupts, rescheduling IPIs between them, and
  `hv list` naming the host CPU of each. `--disk`, `--disk --nvme-root`,
  `--net` and `--attach` take `--cpus` too. The guest kernel needs what is
  above, plus POSIX timers for BusyBox's `ping`, which paces itself with
  `alarm()` (a tinyconfig leaves them out).

## MMIO

A device whose registers are memory -- the xAPIC's page and the IO-APIC's
first, and next the HPET's, a PCIe function's BARs -- is reached by instructions
that fault on a page the nested table does not map, and the exit says where
and whether it wrote: not the register, not the size, not the value, not the
instruction's length. So a device like that is emulated by decoding the
instruction, which is three pieces here, none with any `unsafe`:

- **The walk** (`hv::walk`): the guest's RIP to a guest physical address by
  its own paging -- long mode's four or five levels and their 1 GiB and
  2 MiB pages, or none -- and the instruction's up to fifteen bytes read from
  guest RAM a page at a time: an instruction may cross into a page mapped
  elsewhere, or not at all. The permissions are not checked: the CPU fetched
  the instruction already, and faulted on its data.
- **The decoder** (`hv::insn`): what Linux's MMIO accessors are. `readl`,
  `writel` and their kind are `mov`s in inline assembly -- a register loaded
  from memory or stored to it, an immediate stored, and `movzx`/`movsx` for
  the narrow reads -- the set Linux itself decodes when it emulates MMIO for
  a confidential guest (`insn_decode_mmio`). The memory operand may take any
  form a compiler picks (a fixmap address is an absolute displacement
  through a SIB byte), so every ModRM and SIB form is stepped over; the
  address is not worked out, the fault having given it. Anything else --
  another opcode, a string move, a lock, 16-bit addressing -- is refused.
  It was checked against what `clang` assembles for 1054 instructions (every
  opcode over sixteen addressing forms, the registers REX and the high bytes
  name, immediates of each size), each also cut short at every length --
  refused, never mistaken for a shorter instruction -- and against two
  million random byte strings: no panic, and no length past what was there.
- **The emulator** (`hv::mmio`), in two steps, the device being the caller's.
  `begin` fetches and decodes, and checks the instruction against the fault
  -- that the fault was an instruction's at all, and not an interrupt's
  delivery pushing to a stack or reading an IDT on the page; a load for a
  read and a store for a write, which an instruction another of the guest's
  CPUs changed under the fault need not be; the access inside the page; no
  load into RSP, which VT-x writes back only on its full sync -- and hands
  back the access and a store's value. The caller reads or writes
  its device. `finish` puts a load's value in its register -- zero- or
  sign-extended, a 32-bit destination clearing the top half as the CPU's
  does -- and steps RIP past. Nothing in the guest changes before `finish`,
  and an access that cannot be performed stops the guest with its bytes:
  `an access of guest physical 0x... at rip 0x... not performed -- ...; its
  bytes ...`.

`hv run`'s `mmio` guest checks the three together on each backend: a device
of 32 bytes of registers in the MMIO window, stored to by an immediate of
each size, a 64-bit register, REX's and BH, and loaded from by `mov` into
32-, 64- and 16-bit registers and AH, `movzx` and `movsx` -- fourteen
accesses, each value checked where it went. What an access costs, beside an
exit the CPU decodes itself, is under [What an exit
costs](#what-an-exit-costs): `hv bench mmio`.

### The xAPIC

The first device on it is the local APIC's page. Its APIC comes out of
reset in x2APIC mode, and a kernel that keeps it there -- Linux does --
never touches the page; one that leaves the mode, or whose firmware left
the APIC in xAPIC mode, now reaches the same register file through the page
(`Lapic::mmio_read`, `mmio_write`), where the two modes differ as the
architecture has them differ: the ID in bits 31:24, a writable logical ID
and destination format -- flat or cluster, which the other CPUs' IPIs and a
device's MSIs are matched against, each CPU publishing its own -- the ICR
in two halves, and what x2APIC mode faults xAPIC mode ignoring. The page
stays where firmware put it: a write of `IA32_APIC_BASE` moving it is a
#GP, since a page that is a device to one CPU and RAM to another is no
machine. Two guests use it:

```
$ hv boot /bzImage cpus=2 xapic initrd=/initrd cmdline=console=ttyS0 acpi=off
  --- ttyS0 ---
...
[    0.622101] smp: Brought up 1 node, 2 CPUs
...
  apic       59776 interrupts from the local APICs, 59486 of their timers; 294 IPIs sent, 290 taken; 0 MSIs
  mmio       60828 accesses performed for the guest, its instructions decoded: the xAPIC page's
```

Linux 6.1 -- Debian 12's -- booted without ACPI, `xapic` having its APICs
come out of reset in xAPIC mode, brings up both its CPUs with every EOI,
IPI and timer write through the page. Without `xapic` it cannot: its MP
table path reads the page before it looks at the mode firmware left, and
with x2APIC on it had not mapped the page and page-faulted in its own
`register_lapic_address`. With its ACPI it needs neither -- the MADT has it
choose the x2APIC driver before anything reads the page -- so a 6.1 of a
distribution boots SMP here as it is. And a Linux 6.18 told `nox2apic`,
which takes its APIC out of x2APIC mode itself ("x2apic disabled"), does
the same through the page.

### The IO-APIC

The second device on it is the IO-APIC (`hv::devices::ioapic`), for a
guest of more than one CPU given `ioapic`: 24 pins at 0xFEC00000, version
0x20, reached through the page's select register and window, and its EOI
register. Each pin's redirection entry says what an interrupt on it is --
a vector, a delivery mode, a destination, physical or logical, masked or
not, edge- or level-triggered -- and a device drives its pin as it drives
the 8259's line: the PIT's tick on pin 2, the serial port's interrupt on
pin 4, the SCI on pin 9, each PCI function's INTx on the pin of its line,
10 or 11; the 8259's own output is pin 0's, as on a PC, and its interrupts
go to the first CPU as they always did, while its lines are unmasked there.
The tables say so: the MP table marks the IO-APIC usable and routes each
PCI function's INTA, level and active low, and the MADT lists it with the
timer's and the SCI's overrides ([ACPI](#acpi)); `noapic` is not added.

An edge-triggered pin sends as it rises; a level-triggered one while its
line is up and its last interrupt has been ended -- its remote IRR set as
it sends, and cleared by the EOI of the local APIC that took it, which the
APIC broadcasts for a vector it took as a level (its trigger-mode
register), or by a write of the IO-APIC's EOI register; a line still up
then sends again. A message goes as a device's MSI does: into the local
APIC of the CPU whose loop sent it, or through another's mailbox, the
level bit with it. The serial port had to become a 16550 for this: its
transmitter's interrupt pending until IIR reports it or a byte is written,
and each byte written a pulse of its line -- the edge an edge-triggered pin
sees, where the 8259 was asked again for as long as the line was up. And
the PIT's next edge waits for the last to be taken where it went: on the
8259 while its IRQ 0 is unmasked there, and through the IO-APIC while its
vector is still requested in the first CPU's APIC.

**Its local APICs come out of reset in xAPIC mode.** Linux keeps a
firmware-enabled x2APIC with an IO-APIC only with interrupt remapping, or
under a hypervisor it knows to support x2APIC without -- KVM, Xen, Hyper-V
-- and this one names none (CPUID's hypervisor range is blank). Finding
its firmware left x2APIC on, a Linux 6.1 or 6.18 turns it off, reading its
APIC's ID through a page it has not mapped, and faults. So a machine with
an IO-APIC is the PC most firmware makes, in xAPIC mode, and the kernel
takes it as it is:

```
$ hv boot /bzImage cpus=2 ioapic initrd=/initrd cmdline=console=ttyS0
...
[    0.081741] IOAPIC[0]: apic_id 2, version 32, address 0xfec00000, GSI 0-23
[    0.225009] APIC: Switch to symmetric I/O mode setup
[    0.232303] x2apic: IRQ remapping doesn't support X2APIC mode
[    0.260088] ..TIMER: vector=0x30 apic1=0 pin1=2 apic2=0 pin2=0
...
  mmio       65657 accesses performed for the guest, its instructions decoded: the xAPIC page's and the IO-APIC's
  ioapic     431 interrupts sent, 0 of them level-triggered, 0 ended by EOI, 0 of no delivery mode sent here
```

Every access of its APICs is then a decoded MMIO access ([What an exit
costs](#what-an-exit-costs)); keeping x2APIC mode with an IO-APIC would
take an IOMMU's interrupt remapping, or a name Linux trusts. Linux checks
the timer through the IO-APIC itself (`..TIMER`, and its count of ticks
unless told `no_timer_check`), and routes the ISA lines where it likes --
the serial port's, here, to the second CPU, which through the 8259 it
could never have been:

```
  0:         65          0  IO-APIC   2-edge      timer
  4:          0        219  IO-APIC   4-edge      ttyS0
```

With ACPI it routes interrupts `Using IOAPIC`, takes its SMP configuration
from the MADT with the overrides (`INT_SRC_OVR (bus 0 bus_irq 9
global_irq 9 high level)`), and the SCI arrives on pin 9 as a level,
`IO-APIC 9-fasteoi acpi`: the power button's one SCI a level-triggered
interrupt, ended by EOI, and the guest off by `acpid`. Told `pci=nomsi`, a
virtio device's INTx comes the same way, `fasteoi` on pin 11 or 10, each
interrupt ended and sent again while the device's status is unread.

`hv run`'s `ioapic` guest checks it without a kernel: the serial port's pin
as an edge -- the enable an edge, a byte's pulse another, one while masked
lost, unmasking sending nothing -- then as a level, sent again at each EOI
until the handler reads IIR, the entry's remote IRR set in service and
clear after; every access of the IO-APIC and every EOI through the pages.

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

Booted again with both fixed (50746e4): a 40 s `hv wait` over ssh with
`ServerAliveInterval=5 ServerAliveCountMax=2` ran its full 40 s, where it
had been cut off at 14; and a gigabyte through NAT into the Debian guest
took 9.8 s -- 109.8 MB/s, the link's rate. Not every time: right after a
cold `apt-get update` the same download took 44 to 47 s. The guest had 125
MB of apt's lists to write back, a `sync` of them took 35 s -- 3.5 MB/s --
and the download after that ran at the link's rate again: the guest's disk
writes stall its vCPU, a virtio-blk request being served on the vCPU's own
task, synchronously, through nos's ext2 to the NVMe. Its clocksource
watchdog saw the stalls too ("Long readout interval", gaps of up to 10 s).
That is the disk's next thing to take out, as fewer exits per frame is the
network's.

Booted again with the disk working beside the guest and ext2's writes
costing what they change (2c72334, [A disk](#a-disk)), the same guest with
1 GiB, the same afternoon:

| in the Debian guest | before | after |
|---|---|---|
| a cold `apt-get update`, 28.5 MB | fetched in 15 to 26 s, 47 to 52 s in all | fetched in 2 s, 3.8 s in all |
| a gigabyte to `/dev/null` right after it | 44 to 47 s, 23 to 25 MB/s | 9.9 s, 108 MB/s -- the link's rate |
| `sync` of what `apt` left dirty | 125 MB in 35 s | 90 MB in 0.36 s |
| `dd` 1 GiB, `conv=fsync`, a new file / over it again | | 261 / 284 MB/s |
| reading it back, the cache dropped | | 231 MB/s |
| a gigabyte fetched to the guest's disk | | 13.1 s, 82 MB/s, and `sync` 0.2 s after |

"Long readout interval" came up in none of it. Afterwards `e2fsck` found
nos's `nosenv` clean, and the guest's ext4 in its image too, taken
read-only through a loop device; the image had 0.3 GiB of holes filled on
the way. What bounded the rate then was ext2 writing a request's blocks one
4 KiB command at a time, each synchronous: the disk's task was at a whole
CPU while the guest wrote, `profile` putting most of it in the NVMe
driver's `WaitGroup::Wait` yielding until the command completed.

Booted once more with a file's blocks going to the disk in batches, 32
commands in flight at once (63a7bb2, [Filesystems](filesystems.md)):

| in the Debian guest | a block at a time | in batches |
|---|---|---|
| `dd` 1 GiB, a new file / over it again | 261 / 284 MB/s | 363 / 358 MB/s |
| reading it back, the cache dropped | 231 MB/s | 397 MB/s |
| the disk's task, while `dd` writes | a whole CPU | 11% of one |
| `sync` of what `apt-get update` left dirty | ~90 MB in 0.36 s | 102 MB in 0.25 s |
| a gigabyte fetched to the guest's disk | 82 MB/s | 87 MB/s |

and the guest at its login prompt in 3.5 s, its disk's task having spent a
third as much CPU reading the boot in. `e2fsck` found `nosenv` and the
guest's ext4 clean again. What bounds the guest now is its vCPU, at 85% of
its CPU while `dd` writes: two thirds of that is copying each request's
data out of guest memory, a page at a time through the kernel's temporary
window and eight bytes at a time inside it (`FrameCopy`, a `MemCpy` call a
word), and a fifth the guest itself running. A page's part of a copy is
one `MemCpy` now: under TCG the guest's 48 MiB `dd ... conv=fsync` onto a
whole image takes 0.42 s where it took 0.64, and reading it back 0.24 s
where it took 0.44, on virtio-blk and on NVMe alike.

On the AX41 with it (6bf9e84), the same guest:

| in the Debian guest | a word a call | a page a call |
|---|---|---|
| `dd` 1 GiB, a new file / over it again | 363 / 358 MB/s | 966 / 932 MB/s |
| reading it back, the cache dropped | 397 MB/s | 1.3 GB/s |
| a gigabyte fetched to the guest's disk | 87 MB/s | 102 MB/s |

and its NIC dropped no frame over the downloads, where it had dropped a
few hundred a gigabyte. While `dd` writes 1.5 GiB at 925 MB/s the vCPU is
at 99.9% of its CPU and the disk's task at 31%; half of the vCPU's time is
now the guest itself running, and most of the rest the copies and the
mapping of the temporary window for each page. `e2fsck` found `nosenv` and
the guest's ext4 clean.

One thing that boot showed is not nos's: `apt-get update` took 31 s where
it had taken 2 -- a flat 30 s of it on one request, then a new connection
that had the answer in 20 ms. apt pipelines its requests, and the CDN
behind deb.debian.org left the last of them on a connection unanswered: a
capture in the guest has every byte the CDN sent acknowledged, and nothing
more from it, data or FIN or RST, until the client gave up and closed --
then the CDN's answer went on from the very byte after the last one. From
outside nos the same pipelining has the CDN close the connection in the
middle of an answer. Whether the faster guest meets it at a new point or
the CDN changed that afternoon, apt with `-o
Acquire::http::Pipeline-Depth=0` takes 2 s again.

Then the mapping itself: a copy's page went through the temporary window's
shared slots, under their lock, with a reference taken and two TLB flushes,
and that cost the vCPU as much as the copy. Each CPU has a slot of its own
for it now ([Paging](paging.md#tmpmap-the-window-onto-physical-memory)), a
page still mapped only for the length of its copy. On the AX41 (56b6da8):

| in the Debian guest | shared slots | a slot per CPU |
|---|---|---|
| `dd` 1 GiB, a new file / over it again | 966 / 932 MB/s | 1.2 / 1.2 GB/s |
| reading it back, the cache dropped | 1.3 GB/s | 1.6 GB/s |
| `dd` 1.5 GiB, and the vCPU meanwhile | 925 MB/s, 99.9% | 1.2 GB/s, 70% |
| a gigabyte fetched to `/dev/null` / to the guest's disk | 110 / 102 MB/s | 117 / 107 MB/s |

117 MB/s is what TCP carries over gigabit Ethernet at an MTU of 1500. The
vCPU's time is two thirds the guest itself now, the copy an eighth, and
what is left of the mapping -- the flush when a slot is cleared -- under
4%. The boot's frame self-test passed on all twelve CPUs, each through its
own slot; `apt-get update` took 2 s with pipelining on and off; `e2fsck`
found `nosenv` and the guest's ext4 clean.

Then guests of more than one CPU ([More than one CPU](#more-than-one-cpu)),
the same Debian image with `cpus=2` and without `nolapic`: at its login
prompt in 3.2 s, both CPUs online, systemd `running` with no unit failed, a
gigabyte fetched at 113 to 116 MB/s, 1 GiB written with `fsync` at 1.2 GB/s
and read back at 1.5; its `reboot` came back with both CPUs, a CPU taken
offline and back was started again by INIT and a start-up IPI, and its
`poweroff` ended as it should -- "hlt with interrupts off, on every CPU it
has". What the first boot of it showed, and the next one fixed:

| in the Debian guest | 14a03e0 | with the fixes |
|---|---|---|
| where `cpus=2` put it | 13, 12: one core's two threads | 13, 11: two cores, one CCX |
| two busy loops, one alone taking 9.7 s | 14.7 s | 9.7 s |
| its CPUs on two CCXs | a 2052-cycle TSC "warp", jiffies | four CPUs on 13, 11, 9, 5: TSC kept, four loops 10.0 s |
| one CPU, on host cpu 0 | TSC lost one boot in two | TSC kept, three boots of three |
| its log | an unchecked MSR access (0x852), three firmware bugs | none |

The frames its port dropped on the way were not new: the kernel and module
before these, booted once more for it, dropped 431 and 608 a gigabyte for a
guest of one CPU, the module of 14a03e0 on the same kernel 139 and none,
and with the fixes a guest of two dropped 256, 500 and 129 -- the ceiling
of the vCPU that took every receive interrupt through the 8259, six exits
apiece. Its sampled profile over that fetch: 44% of its host CPU the guest
running, and the rest the exits, spread thin. MSI-X took the ceiling away
for a guest of more than one CPU: three gigabytes at 117 MB/s and not a
frame dropped ([Device interrupts, by MSI-X](#device-interrupts-by-msi-x)).
`e2fsck` found `nosenv` and the guest's ext4 clean.

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

### The EX44, under VT-x

The EX44 (an i5-13500: six P-cores of two threads and eight E-cores, with
EPT, VPIDs and unrestricted guest) is the first Intel silicon VT-x has run
on; everything before it was nested KVM. nos booted there from its system
image, beside Ubuntu, on 2026-10-20 ([Real hardware](real-hardware.md)).

**The first run** (master's module, f3b0285): `hv info` ready, every
built-in guest passed on a P-core (CPU 0) and an E-core (62), and an exit
cost 1.67 and 2.07 µs on them -- against 5.8 µs nested. Linux 6.18 ran with
one, two and four CPUs; a virtio disk took writes with `fsync` at 1.0 to 1.2
GB/s and gave reads at 1.5; a guest fetched a gigabyte through NAT in 9.1 s
(118 MB/s) and served a page to the internet through `hv forward`; Alpine
3.24.2 came to its login prompt in 13 s and rebooted through 0xCF9 into a
restart; and `rmmod` with five VMs running stopped them all and left VMX off
on all twenty CPUs. It also showed, on the silicon, the VT-x halves of the
review that were still open -- a guest's #DB not given back, its CR4 not
intercepted, DR0-DR3 readable by the next VM, no speculation control -- and
two things that are not bugs of the extension: a guest of four CPUs placed
on one P-core's two threads and two E-cores (where its CPUs are placed knows
nothing of P and E yet), and the first write into a sparse disk image's
holes at 170 to 180 MB/s, against a gigabyte a second once its blocks are
there.

**The second** (the review's VT-x halves done -- item 1, 7 and 8, bugs 2 and
5): seventeen of seventeen built-in guests on a thread of each of two
P-cores (CPUs 0 and 41) and on two E-cores (48 and 62), `debug`, `cr4` and
`dr` among them; every CPU deciding its own mitigations -- enhanced IBRS
with BHI_DIS_S, VERW for RFDS, no L1D flush, which is Linux's reading of the
same CPU -- and an IBPB between guests, 159 in the session; Linux with one
CPU and with two, both online and each taking its own timer, and a disk
written at 1.1 GB/s with `fsync` and read at 1.5; Alpine at its login prompt
in 13 s with the TSC its clock and not a line of its log an error, fetching
its package index through NAT; `rmmod` and a load again with nothing left
on. What the mitigations cost an exit is in [What an exit
costs](#what-an-exit-costs): 7% on a P-core, 20% on an E-core.

To repeat it there: build in Ubuntu, mount nos's root by its UUID, copy
`hv.ko` and the guests onto it, `grub-editenv <root>/grubenv set
nos_next=nos` and reboot -- nos for one boot, Ubuntu after it ([Install:
nos on a server](install.md)) -- then `insmod`, `hv on` and the rest over
ssh as above.

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
up](#guests-that-stay-up)): the lifecycle the control plane will serve. The
VMX backend is there beside AMD-V, with `CR0.NE` set on every CPU ([The VMX
backend](#the-vmx-backend-and-where-it-differs-from-amd-v)).
The TLB is no longer flushed whole on every entry ([Address space
identifiers](#address-space-identifiers)), guests have disks and a network
over legacy virtio ([A disk](#a-disk), [A network](#a-network)) with a way
out through NAT ([The way out](#the-way-out-nat-dhcp-and-dns)), a
distribution boots as it ships ([A distribution](#a-distribution)), and a
guest has as many CPUs as it is given, each with a local APIC ([More than
one CPU](#more-than-one-cpu)) -- under AMD-V and VT-x both.
A guest of more than one CPU takes its devices' interrupts by MSI-X
([Device interrupts, by MSI-X](#device-interrupts-by-msi-x)), and every
guest has ACPI's tables and fixed hardware -- its power button what `hv
stop` presses ([ACPI](#acpi)). MMIO is emulated -- the instruction that
faulted fetched by the guest's paging, decoded and performed -- and the
xAPIC page is its first device, the IO-APIC its second ([MMIO](#mmio));
next on it are the HPET and PCIe's ECAM the tables leave out today.
Beyond stage 3: device work off the first CPU's task, modern virtio, and the
control plane's HTTP API (stage 4) -- and before any of it, what a review of
the whole found ([What a review found](#what-a-review-found)).

Two constraints from stage 5 (live update) hold from the first line of it:
all VM state is serializable plain data -- the vCPU register set, every
emulated device, and one table mapping guest physical pages to the VM that
owns them -- and no emulated device holds a pointer into arbitrary kernel
memory, only indices and handles that survive a re-init. So far the VMCB
and the registers `vmrun` leaves to software are plain words, and guest
memory is regions of pages whose nested table is derived from them.

### What a review found

A review of the whole of it on 2026-10-10 -- `hvarch`, `hv` and the module,
at cf87887 -- by five readers at once, each finding read again in the code
before it was written down here, left the list below. Each item says where,
what goes wrong and what would fix it, and is struck through when it is
done, as the steps above are. Where an item rests on what Linux does rather
than on this tree it says so: that part was read in Linux's source, not run.

The order to take them in: the bugs first -- each small, each with the fuzz
target or the gate that would have caught it; then the CPUID and MSR policy
as an allow-list, which is isolation and speed at once; then the guests'
network kept off the host's services and the world's metadata; then the
speculative-execution work and whole cores per guest, before any guest is
someone else's; then speed.

#### Bugs

1. ~~**A guest can hold one of its CPUs' tasks in the host for good.**~~
   Done: a pass takes a queue's worth of chains at most -- all a driver can
   have available at once -- and leaves the rest to the next round of the
   devices (`PASS` in `devices/net.rs` and `devices/blk.rs`); hv-fuzz's
   racing CPU (`Racer` in `fuzz/hv/src/memory.rs`) tops a ring up while a
   notify or a round runs, and found both old loops in its first ten
   inputs. What it was: the
   virtio-net transmit loop (`Net::send`) and the virtio-blk one
   (`Blk::fill`) go round until `Queue::pop` finds nothing, and `pop` reads
   the driver's `avail` index afresh each time -- with no budget, no look at
   the stop flag, and the platform's lock held (the doorbell's port write
   runs them under it, as does the first CPU's round of the devices). A
   second vCPU that keeps the ring topped up, by plain writes to its own
   memory and no exit, keeps the one in the loop there forever: `hv stop`
   waits on its task (`finish`), and so does `rmmod`; the host CPU spins,
   every other vCPU that touches a port waits on the lock, and every frame
   goes into the switch at memcpy speed. Fix: at most a queue's worth of
   chains a pass, the rest left for the next round.
2. ~~**An unprivileged process stops its whole VM.**~~ Done: the run loop
   gives a guest its own #AC and #DB back on both backends -- a single step,
   and ICEBP, stepped past -- once nothing an exit cut short is on its way
   in (`GuestCpu::exception`); an instruction of VMX's, SVM's or GETSEC is
   answered #UD, RDPMC #GP. Under VT-x, whose exit writes no DR6, the DR6
   its qualification says goes into the guest's at the exit, and a single
   step out of an interrupt shadow is marked pending for the entry
   ([The VMX backend](#the-vmx-backend-and-where-it-differs-from-amd-v)).
   The `debug` built-in guest single-steps and runs an `int1` and its own
   handler must see both, DR6 right, on both backends; hv-fuzz's platform
   target gives the run loop the guest's #DB and #AC, now and then over an
   event still on its way in. What it was: every intercepted
   exception was a stop (`Stop::Exception` in `run.rs`), #DB and #AC are
   intercepted always (CVE-2015-5307 and 8104), and an exit the run loop
   does not know is a stop too (`Stop::Unexpected`). A single step -- gdb's
   `stepi`, ptrace, a uprobe -- `int1`, an unaligned access with EFLAGS.AC
   set (Linux runs with CR0.AM), or under VT-x any VMX instruction (`vmxoff`
   at CPL 3: in non-root operation it exits before the CPL is checked) ends
   the VM; and since that is not a reset, `restart` does not bring it back.
   Fix: keep the intercepts and give the events back -- #DB with its DR6
   (the VMCB's under AMD-V, made from the exit qualification under VT-x),
   #AC with error code 0, ICEBP as a #DB, #UD for the VMX instructions and
   for RDPMC. Half of the job [the VMX backend's debug-register
   item](#the-vmx-backend-and-where-it-differs-from-amd-v) names.
3. ~~**A virtqueue's indices are copied a byte at a time.**~~ Done: a
   ring's indices and flags are read and written as one aligned access --
   `GuestMemory::read_u16` and `write_u16`, over `Frame::load_u16` and
   `store_u16` and the kernel's `kernel_frame_load16` and `store16` -- the
   load an acquire and the store a release. What it was: `read_u16` of
   `avail.idx` and `Queue::push`'s store of `used.idx` go through
   `GuestMemory::read` and `write` to `FrameCopy`, whose `MemCpy` is a
   `rep movsb` of two bytes: nothing makes the two one access. When a
   driver on another CPU moves the index across a multiple of 256 while the
   host reads it, 0x0100 can be read as 0x01FF -- `Broken::Overrun`, the
   device dead until a reset Linux never does -- and the guest can read a
   used index half written. Fix: an aligned load and store of 2, 4 and 8
   bytes as one access in `kcore::frame`, through the CPU's slot, for the
   ring's indices and flags.
4. ~~**A CPU's x87/SSE owner goes stale when a vCPU moves.**~~ Done: a
   guest that loads its registers on a CPU takes its mark off every other
   (`fp::registers_loaded`), so its number is on one CPU at most -- the
   last it loaded on -- and `registers_are` means what it says. What it
   was: `fp::OWNER` says
   whose registers a CPU holds, and an entry skips its FXRSTOR when they are
   the guest's own; nothing takes the mark away when the same guest loads
   them on another CPU. A task that runs on A, then B, then A again resumes
   on A with the registers of its first stay there. A Linux guest's tasks
   are bound to a CPU each, so today only `hv run` without a CPU can move;
   anything that moves a vCPU later meets it. Fix: skip the FXRSTOR only on
   the CPU the guest last loaded its registers on.
5. ~~**CR4.MCE is the guest's.**~~ Done -- under AMD-V: `hvarch`'s
   `Guest::run` sets MCE in the CR4 the guest runs with and intercepts
   every write of CR4, whatever the policy asks; `hv` intercepts the reads
   too, keeps the CR4 the guest wrote and answers from it (`hv/src/cr.rs`:
   the instruction fetched and decoded, `insn::decode_mov_cr`, since QEMU's
   and a nested hypervisor's CPUs give no decode assists), refuses with #GP
   a bit the guest's CPUID does not offer -- PKE among them, which closes the
   PKRU half of item 7 below under AMD-V -- and a write no CPU takes in long
   mode, and flushes the guest's TLB where the CPU would. The `cr4` built-in
   guest, hv-fuzz's platform target (every read and write held to what it
   should have done, at the CPU's next entry) and insn-test's control
   registers are its gates; a Linux guest of one CPU and of two boots
   through it. The cost is an exit for every `mov` of CR4 -- two for each of
   Linux's global TLB flushes, which toggle PGE. QEMU's TCG, where the gates
   run, stops no read of CR4 (a bug of its own, [above](#what-entering-a-guest-cannot-be-told-to-do)):
   there the reads are checked by the fuzzer alone, and the `cr4` guest says
   which it saw. And under VT-x, where the CR4 guest/host mask was VMXE
   alone: MCE in the mask with every bit the guest does not own, the
   guest's value in the read shadow, the register keeping MCE, and a write
   that changes a bit of the mask performed by `crate::cr` as AMD-V's are --
   the `cr4` guest read back what it wrote while the CPU's kept MCE, on the
   EX44 and under nested KVM. What it was: AMD-V intercepted no CR at all
   and VT-x's mask was VMXE alone, so a guest could clear MCE -- a kernel
   built without `X86_MCE` does, from
   Linux's source -- and a machine check on that CPU while it ran was a
   shutdown: the host saw the guest's triple fault, restarted it, and never
   heard of the #MC. KVM keeps the host's MCE in the hardware CR4 under both
   vendors.
6. ~~**#GP goes in with an error code whatever the CPU's mode.**~~ Done:
   `inject_gp` gives the error code only with CR0.PE set, on both
   backends. What it was: both
   backends' `inject_gp` set the error code. A CPU in real mode -- an AP
   between its start-up IPI and PE -- takes none: VT-x refuses the entry
   (the guest stops as `Invalid`) and AMD-V pushes a word its handler does
   not expect. Fix: no error code with CR0.PE clear.
7. **In the module.**
   - ~~`hv exec` stops working for good after a boot that threw away unread
     input~~ -- done: what a new boot throws away is counted as fed and
     noted as lost (`Input::lost`), and an `hv exec` waiting on a line of it
     says the line went with that boot. What it was: `Shared::requeue`
     cleared the queue but `fed` never caught up with `queued`, so `fed_at`
     was `None` from then on.
   - ~~Forwards and NAT's mappings outlive their VM~~ -- done: `hv stop`
     takes a VM's forwards away with it (`Forwards::drop_vm`), and the
     switch has NAT forget its address's flows as it gives the port back
     (`Switch::release`, `Nat::forget`, `kernel_net_nat_forget`); hv-fuzz's
     `nat` target forgets guests and holds the table to it, and
     `hv-linux-test.py --net` stops a VM with a forward and flows and looks.
     What it was: `hv stop` gave the port back to the switch, and the next
     `hv start ... net` took the lowest free one -- the same MAC and the
     same address (`port_ip`) -- and with it the old VM's forwards and the
     answers to its flows.
   - ~~Two VMs can open one image read-write~~ -- done: a guest's disk
     claims its image as it opens it (`disk::Claims`), by the file -- its
     mount and inode (`kernel_file_mount`, `kernel_file_key`), so two
     names of one image are one -- a disk that writes it alone and any
     number that only read it together, and a start or a boot that cannot
     have its claim is refused saying whose the image is; the claim goes
     with the disk.
     Among the VMs only: nos's own shell is not held to it.
     `hv-linux-test.py` refuses a writer and a reader beside a writer, and
     a writer beside two readers; `fs-fuzz.py` holds every open file's
     identity to the model's. What it was: `FileDisk::open` opened the
     file read-write for every VM that named it, and the VFS has no
     exclusive writer -- two ext4s on one set of blocks.
   - ~~Nothing keeps host memory back~~ -- done: a frame is not taken
     past a reserve the kernel keeps for itself, a sixteenth of the machine
     and no less than 64 MiB (`kernel_frame_alloc`, over
     `PageTable::AllocPageKeeping`), so a guest too large is refused out of
     memory rather than taking the last page; and `guest::build` reads and
     plans the image before it takes any of the guest's memory. The size of
     the reserve is a guess to revisit. What it was: `mem=` was checked
     against 4064 MiB alone, for up to 16 VMs, guest memory taken a frame at
     a time from the free list everything else uses -- a start could empty
     it, and the next infallible allocation anywhere panicked the host.
   - ~~`hv stop` takes the VM off the table before it waits for the guest
     to shut down~~ -- done: it stays on the list, `stopping`, while its
     power button has its time -- its CPUs counted, `hv off` and an unload
     seeing it, a second stop and a restart refused -- and an unload ends
     the wait. What it was: for up to `secs`, `rmmod` did not see it, `hv
     list` did not show it, and its CPUs could be given to a new VM.
   - ~~An unload that finds a guest's VMCS still current on a CPU says so
     -- and then frees that CPU's VMXON page anyway~~ -- done: the
     machine's drop leaks such a page, and says so, rather than leave a CPU
     in VMX operation over freed RAM. What it was: `Machine::disable` kept
     the page and the machine's drop freed it.
8. ~~**The safe half enters a guest over any table it is handed.**~~ Done:
   both `enter`s are the crate's own and take the `&GuestMemory` whose
   table they enter over -- a table comes only from the memory that owns
   its pages, and the module cannot reach either. What it was:
   `svm::Vcpu::enter` and `vmx::Vcpu::enter` are safe `pub fn`s taking a
   `Nested` whose `root` and `id` are public, and `Vm::vcpu_mut` reaches
   them: the invariant the crate split is for -- a guest's nested table maps
   its own pages -- is a promise of the one caller, `Cpu::enter`, not a
   property of the types. Fix: `enter` crate-private and taking
   `&GuestMemory`, a `Nested` that only `npt` and `ept` can make.
9. ~~**An unchecked add on a guest's RIP.**~~ Done: it wraps, as the CPU
   does. What it was: the VT-x I/O exit's `next_rip` is
   `rip + length` with no wrap, where everything else wraps: an `in` at the
   top of the address space is a host panic in a `RUSTUB=1` build. The
   fuzzer does not compile `hv/src/vmx.rs`, which is why it went unseen.

#### Before other people's guests

1. **Speculative-execution mitigation: the world switch done, under both
   extensions; what is not the world switch's, not.** Done under AMD-V
   ([What entering a guest cannot be told to
   do](#what-entering-a-guest-cannot-be-told-to-do)): after every exit the
   guest's values are cleared out of the registers the run stub returns
   with, and the return stack is refilled before the host's first `ret`;
   an IBPB goes between two guests on a CPU where the CPU has one; and
   automatic IBRS is on while the extension is where the CPU has that.
   hv-test runs every built-in guest through the first two on CPU 0 and on
   CPU 3, and holds `hv` to at least two switches between guests a guest,
   each with an IBPB where the CPU offers one. QEMU's TCG offers neither
   IBPB nor automatic IBRS; the AX41's Zen 2 is where the IBPB first ran,
   under KVM and on the silicon, and automatic IBRS has run on no machine
   yet. What it was:
   none of it, the guest's values left in the caller-saved registers the
   stub returned with. ~~On Zen 1 to 4 (the AX41), SRSO and Retbleed,
   and BTI where there is no automatic IBRS.~~ Done by an IBPB after every
   exit, decided for each CPU from its family and CPUID, with STIBP where a
   core has two threads and HLT and MWAIT always exits (SMT_RSB) -- what
   Linux does for a host whose only way in from a guest is an exit
   ([What entering a guest cannot be told to
   do](#what-entering-a-guest-cannot-be-told-to-do)); `hv` names what a CPU
   is open to with nothing to close it, and hv-test gives its machine cores
   of two threads, so that under KVM on an AMD host it holds every CPU to
   the STIBP `hv on` set. Left: the cheaper way to the same end, the host's
   every `ret` through an untrained return thunk and its indirect branches
   through retpolines -- a compiler's job, for the whole kernel, C++, Rust
   and the modules; a Zen 3 or 4 whose microcode has IBPB_BRTYPE but whose
   CPUID does not say so, which Linux finds by a write to PRED_CMD and this
   takes for open to SRSO; Zen 5's cheaper control of SRSO and TSA's VERW
   (Zen 3 and 4); and a guest's own speculation control (IBRS, STIBP, SSBD,
   IBPB), offered to none until its MSR is switched with it. Done under VT-x
   ([The VMX backend](#the-vmx-backend-and-where-it-differs-from-amd-v)),
   decided for each CPU from what CPUID and IA32_ARCH_CAPABILITIES say of
   it, with KVM's as the reference: the registers cleared and the return
   stack refilled after every exit, as AMD-V's stub does; the IBPB between
   guests; IBRS while the extension is on -- enhanced, set once, and on a
   CPU without it written again after every exit -- with BHI_DIS_S where
   the CPU has the control; VERW before every entry where MDS, TAA or RFDS
   apply; and the L1 data cache flushed before every entry where L1TF does.
   On the EX44's i5-13500 that is enhanced IBRS, BHI_DIS_S and VERW -- RFDS,
   by its E-cores -- what Linux reports for it ("Enhanced / Automatic IBRS;
   BHI: BHI_DIS_S", "Clear Register File"); on the Dell's Skylake it would be
   legacy IBRS, VERW and the L1D flush. hv-test holds VT-x to the switches
   and IBPBs as it does AMD-V, and `hv` reads every CPU's IA32_SPEC_CTRL
   back, warning where it lost what `hv on` set. On the EX44 they cost an
   exit 7% on a P-core and 20% on an E-core, most of that BHI_DIS_S ([What
   an exit costs](#what-an-exit-costs)). Left under VT-x: the software
   sequence that clears the branch history, for a CPU with enhanced IBRS
   open to BHI and without BHI_DIS_S -- none of the machines here, but nos
   under nested KVM on QEMU 8.2, which does not pass BHI_DIS_S through, and
   `hv` says so -- and L1TF with hyper-threading, which an L1D flush does
   not reach: the sibling's guest reads the cache meanwhile, and only a
   guest's whole cores with the host's work kept off them would.
2. ~~**The CPUID and MSR policy is a block-list, and it swallows.**~~ Done:
   both are allow-lists (`hv/src/policy.rs`, [A Linux
   guest](#a-linux-guest)). A leaf the policy does not name is all zeros,
   as is every leaf past the tops it reports, 0x16 and 0x8000_0021; of
   leaf 6 a guest is told ARAT alone, of 0x80000008 the address widths,
   CLZERO, the FP error pointers and two attacks the CPU is not open to, of
   0x80000021 what is true of the silicon and asks nothing of the host. No
   speculation control is offered -- none has its MSR behind it yet (item 1)
   -- nor INVLPGB or AutoIBRS; the hypervisor bit is set. An MSR the policy
   does not name is a #GP both ways; what it names is the guest's own
   state, the TSC, or a constant that takes a write leaving it as it is and
   refuses any other: the MTRRs off, machine checks with no banks, the
   microcode revision none, and of each vendor what Linux reads of every
   such CPU without asking. The guest's own state takes what the CPU would:
   a PAT entry that is no memory type, an address that is not canonical
   and an EFER bit the CPUID does not offer (FFXSR) are #GPs, where they were
   a VMCB `vmrun` refused; and since NB_CFG says AMD's extended
   configuration is on, a CONFIG_ADDRESS that names a register past 255
   reads all ones. hv-fuzz's `policy` target holds the policy to a model
   written from the manuals, with a table of what each CPUID bit promises:
   nine mutants, the old swallowing and IBPB offered among them, each failed
   within 1,600 inputs. `hv-test.py`'s `msr` guest has the run loop refuse
   SPEC_CTRL, PRED_CMD, an MSR nobody names and MTRRs turned on, and answer
   the machine-check registers and the PAT; a build that refused
   MCG_STATUS failed it. `hv-linux-test.py` holds every boot's console to
   no "unchecked MSR access error" and no "MCA architectural violation",
   run with a guest kernel that has machine checks, MTRRs and PAT, as a
   distribution's does, on QEMU's `max`, on a Zen 2 (`--qemu-cpu
   EPYC-Rome-v1`) and on an Intel CPU given AMD-V (`--qemu-cpu
   Skylake-Client-v1,+svm,+npt,+nrip-save`): the Zen 2's kernel probed
   LS_CFG, SYSCFG and the legacy PMU and was refused each, which it was
   ready for -- it no longer takes LS_CFG for an SSBD that wrote into
   nothing -- and the Intel's no longer turns its fast strings off or calls
   its microcode old; with MCG_STATUS refused, the Zen 2's guest panicked
   at boot ("MCA architectural violation!") and the check failed. What it
   was: every MSR the policy did not name read 0
   and took any write without a #GP -- though `rdmsr`'s comment promised a
   `None` for one -- and every leaf it did not name was the host's.
   Extended leaf 0x80000008 EBX went through but for two bits, so a guest
   on AMD was told of IBPB, IBRS, STIBP and SSBD and wrote PRED_CMD and
   SPEC_CTRL into nothing while its sysfs said it was mitigated: a failure
   that passes in silence. On AMD newer than the AX41 it was worse, from
   Linux's source: Zen 3's INVLPGB was offered while the VMCB does not
   enable it (a #UD in Linux 6.15's TLB flushes), and Zen 4's AutoIBRS was
   offered while the EFER bit that turns it on was refused -- which Linux
   does not notice, and ran with neither AutoIBRS nor retpolines.
3. ~~**The guests reach the host's own TCP services.**~~ Done: a TCP
   listener and a connection belong to their device (`net/src/tcp.rs`, a
   slot's `device`) -- a listener takes a SYN only from the device it was
   opened on, as `kcore::tcp::TcpListener` always said it did, and a
   connection a segment or an ICMP error only from its own -- so sshd and
   `hv forward`'s ports, on eth0, refuse a guest on `hv0`, at nos's address
   there and at eth0's, and an ICMP error from a guest cannot abort a
   connection of nos's over eth0. UDP's listeners were each device's
   already. net-fuzz's `tcp` target has a guest send SYNs from a second
   device; `hv-linux-test.py --net` has two guests `nc` nos's sshd and the
   test machine reach it from outside. What it was: `find_listener`
   matched the address and the port, not the device, and sshd listened at
   every address: a guest connected to 10.0.100.1:22 -- the control run's
   guests had the banner both ways -- and four connections that never
   logged in held every slot for logging in, on the Hetzner boxes the only
   console there is. Forwards still draw on the same 64 connections the
   machine has (item 5).
4. ~~**NAT lets a guest go anywhere nos can.**~~ Done: NAT keeps the
   guests from link-local, the private networks, the shared space and the
   uplink's own subnet unless `nat allow` lets a network through, the DNS
   server they are given at port 53 aside, and answers a guest's packet for
   any of them with an ICMP "administratively prohibited"
   ([The way out](#the-way-out-nat-dhcp-and-dns)). hv-fuzz's `nat` target
   holds every packet to a model of the rule -- control runs with nothing
   denied, the DNS server not let through, `nat allow` ignored and the
   reject's code wrong each failed within thirty inputs -- and
   `hv-linux-test.py --net` has a guest refused at the test machine and at
   169.254.169.254 before `nat allow 10.0.2.2/32`; on the stack before, the
   guest had the test machine's page and NAT mapped its connection to the
   metadata address. What it was: `routable` refused 0/8, 127/8, multicast
   and the subnet's broadcast and nothing else, so 169.254.169.254 -- the
   host VM's credentials -- the private networks and the host's own LAN
   were all open.
5. ~~**What the guests share has no quota.**~~ Done. NAT's half: a full
   table gives a new flow the mapping used least lately of the guest that
   holds the most -- the asker's own when it holds as many as any -- where
   it refused every new flow, so one guest that filled it kept every other
   from the world ([The way out](#the-way-out-nat-dhcp-and-dns)). The boot
   self-test holds the shares to the rule -- a guest's own flow, another
   guest's taken from the one holding the most, a tie, every slot held --
   and hv-fuzz's `nat` target floods the table and holds each flow taken to
   it; refusing when full, as before, failed it at once. And the soonest a
   sweep may find a flow run out is kept as each flow's time is, where a
   flow made after the last sweep could run out unseen and be counted live
   for up to two hours. And the switch's: a port takes no more of `hv0`'s
   256-frame backlog, between two of the stack's passes over it, than the
   backlog over the ports in use, and sends to every port at 64 at once and
   256 a second ([A network](#a-network)); hv-fuzz's `switch` target bursts
   a guest's frames past both and holds every port's counts to a model of
   its own -- no share at all, no rate, a share not divided and one never
   counted afresh each failed within 40 inputs. What it was: one NAT table
   of 4096 mappings that refused a new flow when full rather than take the
   flooder's; one 256-frame backlog for `hv0` that a guest sending as fast
   as it could filled before the others' frames came; a broadcast copied to
   every port, each a wake or a kick, at any rate.
6. ~~**Two VMs can share a core.**~~ Done: a guest's cores are its own
   ([More than one CPU](#more-than-one-cpu)) -- whole cores no other guest
   has a CPU on, its vCPUs or its disks' task, its vCPUs filling their
   threads, its disks' task on a spare thread of its own cores, then on a
   core no guest has; refused where there is none, and `hv cores shared`
   for a machine whose guests are all one's own. The place and the hold on
   it are taken under one lock, so two starts at once are given cores
   apart. hv-fuzz's `place` target holds every placement and every refusal
   to a model of its own over made-up hosts -- threads a core, caches,
   CPUs missing and off -- and five mutants (later cores not checked, one
   vCPU a core, the disks' task on a held core, `cpu=`'s core not checked,
   the disks' task elsewhere before a spare thread of its own) each failed
   within two inputs; hv-linux-test puts vm 0 and vm 1 on cores apart,
   refuses a third while both are held, and serves a guest's disk on its
   own core. What it was: `pick_cpus` kept a guest's own CPUs off each
   other's cores, for speed, and nothing kept another guest off them, nor
   the task serving another guest's disk off the sibling thread. Left: the
   host's own tasks -- the network stack among them, which carries every
   guest's frames -- still run on any core; a core of the host's own for
   them is core scheduling, not placement.
7. ~~**Debug registers and PKRU, under VT-x.**~~ Done, under both. Under
   AMD-V a guest's DR0-3 are its own, every `mov` of them intercepted and
   performed on a copy per vCPU, and the CPU's made that copy before each
   entry --
   writing only what differs from what the CPU holds ([What entering a
   guest cannot be told to do](#what-entering-a-guest-cannot-be-told-to-do)).
   hv-test's `dr` guest runs two VMs on one CPU, the second reading 0 where
   the first left its own and each one's breakpoint firing at its own
   address -- the world switch before this failed it, the second VM
   reading the first's breakpoint address and pattern, and one without the
   load, or loading only when the guest's DR7 enabled a breakpoint, never
   fired the first's breakpoint; hv-fuzz's platform target holds every
   `mov` of them -- a read, a write, at CPL 3, under general detect -- to a
   model of its own, and
   seven mutants of `hv/src/dr.rs` each failed within 145 inputs. What it
   was: DR0-3 switched by neither backend, one guest's breakpoint addresses
   the next's to read and to move. Under VT-x every `mov` of a debug
   register exits and `crate::dr` answers it -- DR0-3 from the guest's own
   four, made the CPU's before its entries as AMD-V's are, DR6 from the
   guest's own, DR7 the VMCS's, which the entry loads
   ([above](#the-vmx-backend-and-where-it-differs-from-amd-v)); the `dr`
   guest's second VM reads 0 there too, each breakpoint firing at its own
   address, all eighteen `mov`s of a debug register the host's, and the
   platform target holds DR4-DR7 to its model as well -- three mutants of
   `dr.rs` (DR6 written whole, the #UD of DR4 and DR5 checked after the CPL,
   no #GP for a bit above 31) each failed at once. And PKRU: a guest's write
   of CR4.PKE is a #GP under both now (item 5), its CPUID not offering PKU,
   and WRPKRU is then the #UD it is without PKE -- under VT-x it was the
   guest's to set, and nothing switched the register.
8. ~~**VT-x's controls are taken on trust.**~~ Done: a control the host
   needs that `adjust` dropped -- the exits for interrupts and NMIs, ports,
   CR8, MONITOR, MWAIT, RDPMC, every `mov` of a debug register and WBINVD,
   EPT, the 64-bit host, EFER's, the PAT's and DR7's switch -- or one the
   guest was promised, unrestricted guest and its VPID, refuses the guest,
   naming the field and the bits (`Refusal::Controls`). What it was:
   `adjust` dropped a control the CPU, or an outer hypervisor, did not
   allow, and only the MSR bitmap was looked at again: a CR8, WBINVD, I/O,
   external-interrupt or NMI exit gone missing would have gone unnoticed.
9. ~~**A guest's console is the host's log.**~~ Done: a guest's console
   goes to the kernel log a boot's worth at once -- a thousand lines --
   and fifty lines a second after that, per VM (`modules/hv/src/guest.rs`,
   `LogLine::admit`); what is left out is counted, and said once a second
   at most, before the next line logged, and when the boot ends -- a notice
   for every line logged after a gap would have doubled the rate -- and the
   console itself keeps every byte. `hv-linux-test.py` floods one guest's
   console with twenty thousand lines and holds the log to fewer than half
   of them -- by dmesg's count of the lines it pushed out, read before the
   flood and after -- every notice to the gaps in the numbers it came
   between, and the guest's console to all of them; on the stack before,
   the log took all of them and said nothing. What it was: `hv boot`, and
   `hv start ... log`, put every console line into the kernel's log at
   level 0 with no limit -- dmesg, the serial console, netconsole.

#### The CPU a guest is shown

1. **SSE4.2, and nothing after it.** Half done: leaf 7 is open bit by bit
   -- SMEP, SMAP, UMIP, FSGSBASE, BMI, ADX, ERMS and FSRM, RDSEED, SHA,
   CLFLUSHOPT and CLWB, SERIALIZE, MD_CLEAR -- PCID is offered, and
   ARCH_CAPABILITIES reads as the host's but for what is not a fact of the
   CPU ([A Linux guest](#a-linux-guest)). Alpine's kernel on the EX44 said
   before "meltdown: Mitigation: PTI" -- its page tables isolated, a whole
   TLB flush at every system call, no PCID -- MDS and RFDS "Vulnerable: no
   microcode", L1TF mitigated by PTE inversion, and none of SMEP, SMAP or
   UMIP in its flags; after, Meltdown, MDS, L1TF and the iTLB multihit "Not
   affected", RFDS "Mitigation: Clear Register File", and every one of
   those in its flags. PCID it turns off itself there ("Incomplete global
   flushes", an Alder Lake and Raptor Lake erratum); on another CPU it is
   used. Left: XSAVE, with XCR0 switched and leaf 0xD made up for it -- no
   AVX, so a distribution built for x86-64-v3 (RHEL 10 and its rebuilds)
   does not start -- and the speculation controls, SPEC_CTRL switched with
   the guest: Spectre v2 retpolines in the guest, its store bypass open.
   (The hypervisor bit, clear until the policy became an allow-list -- the
   guest believing it was on bare metal -- is set now:
   [above](#before-other-peoples-guests), item 2.)
2. **MSRs that read 0 and should not.** Intel's MISC_ENABLE without fast
   strings, which Linux answered by turning REP MOVS's fast paths off
   ("Disabled fast string operations"), has them as the host has now.
   Left: MTRR offered but MTRRcap and the default type 0 -- MTRRs "disabled
   by BIOS" -- which a Linux 6.1 (Debian 12) answers by leaving PAT off,
   from its source; a default type of write-back, written back as the guest
   sets it, would close it, with the MTRRs kept per CPU.
3. **Devices, smaller.** The 16550 never sets IIR's FIFO bits, so Linux
   takes it for a 16450 and writes it a byte an interrupt; the RTC never
   interrupts (status B is read-only, C always 0, IRQ 8 never raised), so
   no update, alarm or periodic interrupt ever comes; the 8259 takes ICW4
   and ignores it, auto-EOI with it; virtio-net offers no MTU, so a guest
   that raises its own past 1500 loses those frames; MSI-X entries start
   unmasked, their table being zeroed guest memory; VT-x never loads the
   PDPTE fields, so a 32-bit PAE guest that turns paging on faults.
4. ~~**The guest's kernel is nos's file.**~~ A guest booted by hvboot
   boots the kernel its own disk's `grub.cfg` names, at every restart
   again ([A guest's own kernel](#a-guests-own-kernel-pvh-and-hvboot)); one
   booted by its bzImage still boots that. BLS entries are not read yet.

#### Speed

1. **The CPU a guest is shown**, above, is the largest lever on Intel.
2. **The disk is one request at a time, machine-wide.** `Vfs::read_at` and
   `write_within` hold the VFS's one lock across the whole synchronous ext2
   read or write, and a guest's FLUSH is a sync of the whole filesystem
   under it: every guest's disks and nos's own files go in single file.
   Fix: the image's block map resolved under the lock, the I/O through the
   block layer's asynchronous path, more than `IN_FLIGHT` (8) in flight.
3. **Fewer exits a request.** No EVENT_IDX and no NO_NOTIFY, though both
   work on legacy virtio; and a guest of one CPU has no local APIC, so no
   MSI-X -- six exits an interrupt through the 8259.
4. ~~**The EX44's E-cores first, with shared cores.**~~ Done: `hv::topology`
   asks each CPU of a hybrid part which kind of core it is (leaf 0x1A), and
   placement puts a guest's vCPUs on one kind where one has room for them
   all, the faster first; a tie under `hv cores shared` goes to the faster
   kind; and a disks' task given a core of its own takes a slow one. The
   hv fuzzer's `place` target holds it to hybrid hosts made up an input,
   and three mutants -- no kind kept, the slower first, a disks' task on a
   fast core -- each failed at its first input. What it was: no rule knew
   a P-core from an E-core; `hv cores shared` gave a tie to the highest
   CPU, which on the EX44 is an E-core, and a guest of four CPUs went on
   one P-core's two threads and two E-cores.
5. ~~**PAUSE.**~~ Done under VT-x: pause-loop exiting where the CPU has it
   ([More than one CPU](#more-than-one-cpu)), every PAUSE where it has not.
   On the EX44 a guest of two CPUs booting for twenty seconds exited 99,297
   times where it had exited 299,508, its APIC timer calibrated as before
   (`jiffies result ok`).
   Left: AMD-V sets the pause filter's count and leaves its threshold
   unused.
6. **The larger ones.** ~~APICv and posted interrupts~~ done under VT-x, in
   x2APIC mode ([Interrupts the CPU delivers
   itself](#interrupts-the-cpu-delivers-itself-vt-xs-apic-virtualization));
   ~~a NIC's receive interrupts moderated~~ done ([A network](#a-network)),
   halt polling having turned out to do it by accident;
   left: the xAPIC
   page's virtualization, and AMD's AVIC; device work off the first CPU's
   task, with a lock a device (~~the ring pages mapped once rather than at
   every access~~ done, [Its memory](#its-memory)) -- tried for a NIC, a
   task of its own on its guest's spare thread, polling with the driver's
   notifies off, and left out: for a guest of one CPU, the only kind its
   own cores leave a thread for, whose interrupts come through the 8259
   and take it out of its guest all the same, no faster -- `dd | nc` 2.7-2.8
   s against 2.66, BusyBox's `httpd` 0.42-0.59 s against 0.21; guests with
   MSI-X and a CPU to spare are where it could pay; nested leaves of 2
   MiB and 1 GiB, over guest memory taken in runs that size; the VMX
   preemption timer, which would take a computing guest out at its timer's
   edge without the host interrupt that does it now ([Time](#time));
   ~~virtio-net's offloads~~ done both ways ([A network](#a-network)): a
   stream between two guests is a fortieth of the frames it was, and a
   driver's scatter-gather, which cost a sender of one segment at a time 7%
   while every ring access mapped a page, 4% since -- inside the spread.
7. **What has not been measured.** The time each kind of exit takes
   inside `LinuxGuest::run`; fio and iperf in a guest. (An exit under VT-x
   on silicon is measured now: [What an exit costs](#what-an-exit-costs).)

#### Lifecycle, and stages 4 and 5

- A host `poweroff` or `reboot` stops every guest as `rmmod` does
  (`stop_all` with no grace): no power button is pressed.
- ~~`finish` and `FileDisk::drop` wait for their tasks with no deadline: a
  disk task stuck in a read hangs `hv stop`, `rmmod` and the host's
  shutdown with nothing said.~~ Done for every join in the kernel
  (`Task::Wait`, [Scheduler](scheduler.md#waiting-for-one-to-end)): a wait
  still going after ten seconds is reported -- who waits, for which task
  (`hv/vm3`, `hv/vm3/vda`, `hv/vm3/cpu1`), for how long, and where that task
  is, its frames named, or the CPU it is on that takes no interrupt -- and
  again each time the wait has doubled. It still waits: a task that has
  not exited is still using what its waiter would free.
- Stage 4 needs what the shell's strings stand in for today: a typed
  `start(Spec) -> Result<Id>` and status, names, VM definitions kept across
  a reboot, images handled -- claims, copies -- and quotas.
- Stage 5's constraints, above, do not hold yet. Guest memory is frames of
  4 KiB from the general allocator, with no machine-wide table of who owns
  them; the devices' timers keep absolute `boot_time_ns` stamps a new
  kernel's clock would not continue; the guests run on the host's TSC with
  no offset; there is no pause and no snapshot of a guest. A first
  milestone that needs all of it and no kexec: `rmmod hv` and `insmod hv`
  with the guests kept running.

#### Tests

- CI runs `insn-test.py` and `hv-fuzz.py` on the host and never loads the
  module. `hv-test.py --arch aarch64` can go in as it is; x86 under TCG
  wants QEMU 9.2 or later for the `smp` guest, or a way to skip it.
- `hv-linux-test.py` and `hv-distro-test.py` are run by hand; a pinned
  tinyconfig bzImage and a BusyBox cpio, built once and cached, would put
  the first in CI.
- `fuzz/hv` compiles neither `hv/src/{svm,vmx,vm,machine,npt,ept}.rs` nor
  any of `hvarch` but the VMCB's layout, nor the module's `vms`, `guest`,
  `disk` and `forward`, and puts its own `memory.rs` in the real one's
  place. Three targets would have caught this review's bugs: the exit
  decoders over made-up VMCBs and VMCSs (bug 9), a model of the input
  queue's `type_in`, `requeue` and `fed_at` (bug 7's `hv exec`), and a
  second vCPU that keeps re-posting chains (bug 1 -- there now, in the
  `blk` and `net` targets).
- No gate covers leaks over many starts and stops, a `mem=` past free
  memory, sixteen guests, `rmmod` or `poweroff` with I/O in flight, a port
  or a forward used again, one image given twice, or a run of hours.
- Hardware: VT-x has run on the EX44's silicon by hand ([On real
  hardware](#on-real-hardware)), and its gates run under nested KVM; the
  Dell's Skylake, whose paths -- legacy IBRS written after every exit, the
  L1D flush -- no other machine here takes, never; the arm64 backend
  reports and does not enter.
