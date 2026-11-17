# Live migration of VMs between a cluster's hosts

A running Linux VM moved to another host of the cluster: its memory copied
while it runs, then paused for the last of it and its CPUs' and devices'
state, its disks' writer and its network port handed over, and resumed
there -- a pause the guest sees as a hiccup, its TCP connections, which live
in its memory, kept. It is what [vmctl.md](vmctl.md)'s V3 builds on: a
host's evacuation for maintenance, and rebalancing.

Written on 2026-11-16 as a plan, at the user's word ("напиши план ... но
пока не делай"); begun on 2026-11-17 at the user's word ("реализуй живую
миграцию ... можешь сделать детальный свой план и проработать архитектуру"),
the decisions taken as the first version recommended them (below), on the
branch `vm-migration`. What follows is that plan worked out: the pieces,
where each goes in the tree, the protocol, and the steps with their gates.

**Where it stands (2026-11-17)**: M0 to M5 are done on `vm-migration` and
under AMD-V (TCG) held by their gates -- `hv save`/`hv restore`, `hv migrate
<vm> <host> [cold]`, the dirty log, the handover; the network (frames carried
on by the old host, vnet's word to every keeper, the firewall's flows
carried); the CPU model held by the answers themselves, the first leaf that
differs named, the cache and clock leaves a hybrid CPU's cores answer each
their own way left out; the `vm` provider's `migrate` job and the panel's button;
tracepoints in group `migrate` (docs/tracing.md). VT-x's half is written and
runs only where VT-x is: the EX44, the stand's. Left: vmctl's evacuation and
rebalancing (M5's last line), a public address's move between hosts held by
a gate. docs/hypervisor.md's "Moving a VM to another host" says what is.
Where the code went another way than written below, it says so where it
does.

## Where it starts

What is there, and why the disk and the network are most of the way:

- **The disk does not move.** A VM's disks are volumes of NBS
  ([nbs.md](nbs.md)), on every host of the cluster alike. What moves is
  their writer: NBS fences a volume's writer by its attach generation --
  every attach for writing a new one, the cluster's next, and the chunk
  servers refusing a write of an older one -- and an attach anew of the
  writer's own (nbsmeta's `Reattach`) is conditional: only where the writer
  is still that host's at that generation. nbsclient already pauses a
  writer for a snapshot.
- **The network does not move.** On a network of the cluster's
  ([vnet.md](vnet.md)) a VM's address is vnet's, its MAC `02:00` and the
  address's four bytes, whatever host it is on. A port has a host and a
  generation, `vnet port move` changes the host -- a compare and set, moved
  only from the node expected -- the old host's keeper shuts the port and
  the new one attaches it. A public address is served by the host it is
  routed to wherever its VM runs (V4), so a VM's connections through it
  survive a move. The cluster's firewalls are the same on every host.
- **Between hosts**: ipcnet, a VM's state and its memory going as
  messages of 1 MiB at most (`ipcwire::MESSAGE_MAX`), its bulk class's
  queue bounded -- what nbsclient's window keeps to.
- **The devices are few and the tree's own**: the local APIC, the IO-APIC,
  the PIC, the PIT, the RTC, the UART, the ACPI PM block, pvpanic, PCI with
  MSI-X, virtio's transport, its block and net devices
  (`src/crates/hypervisor/hv/src/devices/`) -- and the hv fuzzer runs that
  very code on the host. Each is a struct of a few dozen words: the state
  is small, and every piece of it is in one place.
- **What a guest's CPUs share is already apart from what each owns**
  (`run.rs`): the memory, the devices under one lock, a lock-free mailbox
  a CPU; each CPU's registers and local APIC its own task's. The VT-x
  backend keeps its guest state in the shape of an AMD-V save area, synced
  to the VMCS around an entry, so most of a CPU's state has one shape
  under both extensions already.
- **The jobs**: the vm provider's long calls are jobs with progress, the
  panel follows them.

What is not, checked in the code on 2026-11-16 and again on 2026-11-17:

- **No state of a running VM is ever saved.** Its vCPUs' -- each backend's
  VMCB or VMCS, the registers `vmrun`/`vmlaunch` leave to software, the
  XSAVE area (`GuestFp`), TSC_AUX and SPEC_CTRL (`GuestMsrs`), the debug
  registers, APICv's virtual APIC page and posted interrupts on VT-x -- and
  every device's live only in their structs. There is no pause either: a
  VM runs, or is stopped.
- **Nothing tracks the pages a guest writes.** The nested page tables are
  built to be written once: NPT's entries have their accessed and dirty
  bits set from the start so the CPU writes nothing back, and "it only
  grows" ([npt.rs](../src/crates/hypervisor/hv/src/npt.rs)); EPT runs
  without accessed and dirty flags
  ([ept.rs](../src/crates/hypervisor/hv/src/ept.rs)). Memory from the huge
  pool is mapped in 2 MiB leaves. And the host writes guest memory too --
  a disk's reads, a NIC's frames, the used rings -- through
  `GuestMemory::write` and the rings it maps, which no nested fault sees.
- **A guest's TSC is the host's**: no offset, and no kvmclock -- the TSC
  being invariant and the host's ([policy.rs](../src/crates/hypervisor/hv/src/policy.rs)).
  On another host it would jump by the two hosts' uptimes' difference. The
  devices' clocks are the host's boot clock, read where each needs it.
- **A guest's CPUID is its host's**, the policy built from the host's side:
  a guest moved to a CPU without a feature it uses faults, or worse.
- **A VM's folder is its host's**: /vms/<name> -- vm.toml, its cloud-init
  seed (`seed.iso`, `user-data`), console.log.
- **NBS has no conditional attach for another host**: `Attach` with
  `force` takes a volume from whoever writes it, unconditionally, and
  `Reattach` is the writer's own. A handover needs "D writes it, only
  where S still does at generation g" (below).
- **The way between hosts is slow for memory**: 1 GbE on the stand, and TLS
  over it (ChaCha20-Poly1305 on CRYPTOGAMS' AVX2 code since 2026-11-16,
  no longer the bottleneck: docs/tls.md). A 4 GiB VM's first pass takes
  35-40 s at best, less its zero pages; a guest that dirties more than the
  way carries never converges without being slowed.

## What it is

```
$ hv migrate web ax41b          # a job: rounds, dirty rate, what is left, the downtime
hv: web moving to ax41b: 4096 MiB, round 1 of its pages 38 s, round 2 3 s, round 3 0.4 s
hv: web paused 180 ms: its state and the last 1.2 MiB, its disks' writer and its port handed over
hv: web runs on ax41b
$ hv save web /web.state        # one host: paused into a file, and gone
$ hv restore /web.state         # and running again from it
```

- **What moves**: a running VM whose NICs are on networks of the cluster's
  and whose disks are NBS volumes -- one at least, which decides the
  handover (below) -- to a host of the same CPU model and TSC rate with
  room for it.
- **What is refused, and said**: a disk of the host's own (a file, a
  device, a partition), a NIC on a host's network, a forward on the host's
  address (`hv forward`, the host's own), a CPU model or a TSC rate the
  other host cannot give, no room there, a name taken there, a VM with no
  volume of NBS's, a VM not running.
- **What the guest sees**: a pause; time going on as it went (its TSC and
  its devices' clocks carried, the pause left out); its connections kept --
  through a public address and inside the cluster's networks; those it
  made masqueraded from its old host's address lost (D1).
- **On the bus**: the vm provider's job `migrate` (progress: rounds, pages
  sent, the dirty rate, the estimated pause), cancelled while it copies; a
  button on a VM's page in the panel, the hosts it may go to offered.

## Decisions, as taken

- **D1, connections masqueraded from the old host: (a) lost, and said so.**
  The VMs that keep every connection have a public address. (b) -- the new
  host sending a flow's packets back through the tunnel to the NAT that has
  it, until it ends -- is left for later.
- **D2, the CPU model: (a)**: a VM moves only between hosts whose CPUs give
  a guest the same CPUID, checked by a digest of what the policy answers
  (below); (b), `cpu=baseline`, later. Between Intel and AMD: never.
- **D3, the TSC rate: (a)**: refused unless the two hosts' TSC rates agree
  within 250 ppm (KVM's `tsc_tolerance_ppm`); no scaling.
- **D4, the pause**: a target of 300 ms -- the rounds stopping once what is
  left fits it at the rate the last round went; past five rounds the
  vCPUs slowed (auto-converge), and past thirty, or a limit of time, given
  up, the VM staying where it was. No post-copy.
- **D5, the way: ipcnet**, as NBS goes: its keys, its routing, its bulk
  class; a mailbox of hv's on each host (`hv.migrate`), and a window of
  its own as nbsclient's.
- **D6, the VM's definition**: its folder copied to the new host, marked
  coming until the VM runs there, then taken out of the old one. Names are
  a host's: one taken on the other host is refused.
- **D7, who moves a VM**: a person, by `hv migrate`, the job or the panel.
  A host's evacuation and rebalancing are vmctl's, after.

## The pieces

| Piece | Where | New or changed |
|---|---|---|
| The state's format: a VM's header, each CPU, the local APICs, the mailboxes, every device; versioned, every field checked as it is read | `common/vm/vmstate` | new crate |
| Each device's state out and back in: `save(now)`, `restore(state, now)` | `hv/src/devices/*.rs`, `lapic.rs`, `smp.rs` | changed |
| Each CPU's state out and back in, in the backends' common shape | `hv/src/svm.rs`, `vmx.rs`, `vm.rs`; `hvarch/src/x86/{svm,vmx,fp,msrs}.rs` | changed |
| A guest paused, drained, captured, resumed or ended; built from a state | `hv/src/run.rs` | changed |
| The TSC offset; device clocks carried as relative times | `hv/src/run.rs`, `policy.rs`, both backends | changed |
| The dirty log: leaves split and write-protected, a write's fault logged, the host's writes logged, the TLBs flushed by an epoch every CPU takes | `hv/src/memory.rs`, `npt.rs`, `ept.rs`, `run.rs` | changed |
| The CPU model's digest, the TSC rate | `hv/src/policy.rs` | changed |
| The protocol's messages and both sides' decisions, with no I/O | `common/vm/migwire` | new crate |
| The two sides over ipc, the job, `hv migrate`, `hv save`, `hv restore` | `modules/hypervisor/hv/src/migrate.rs`, `save.rs` | new |
| A VM started from a state rather than a kernel; a VM's end by migration | `modules/hypervisor/hv/src/vms.rs`, `guest.rs` | changed |
| A disk's backend opened at the handover and swapped in | `modules/hypervisor/hv/src/disk.rs`, `hv/src/devices/blk.rs` | changed |
| The conditional attach: `expect`, the generation the writer must be at | `common/storage/nbsmeta`, `nbswire`, `nbsclient`, `modules/storage/nbs-mds`, `modules/storage/nbsdrive` | changed |
| A port moved; the keepers told now rather than in a second | `modules/hypervisor/hv/src/vnet.rs`, `modules/net/vnet` | changed |
| The vm provider's `migrate` job, the panel's button | `modules/hypervisor/hv/src/provider.rs`, `vm.descriptor.json`, `panel/` | changed |

Two crates under `common/` and not under `hypervisor/`, because each is
linked by three things that cannot link the hypervisor: the format by hv,
the module, the fuzzers and, later, live update's manifest
([05-live-update.md](05-live-update.md)); the protocol by the module and the
fuzzer that runs both sides with every failure in between.

## The state (M0)

One versioned format for a VM's state, `vmstate`: no kernel, no `unsafe`,
encoded with `codec` (little-endian, lengths in front), and every read
checked -- a state comes from another host, which is authenticated, and
from another build, which is not the same code: the version says which, an
older one is read or refused, never guessed. What it holds:

- **The VM**: the format's version, the VM's UUID, its memory map (each
  range: a guest-physical address and a size, as `linux::Ram` laid it out),
  its CPUs' count, what it was built with (x2APIC or xAPIC, an IO-APIC,
  APICv, its NICs' offloads and receive moderation), the shape its CPUID
  tells it (`place::Shape`), its CPU model's digest and the TSC rate it was
  made at, and the guest's TSC at the pause.
- **Each vCPU**, in a form of neither backend's: the general registers,
  RIP and RFLAGS, CR0, CR2, CR3, the CR4 the guest believes it has and
  CR8, the segments (selector, base, limit, and the attributes as their
  bits are in a descriptor: type, S, DPL, P, AVL, L, D/B, G, and unusable),
  GDTR and IDTR, EFER, PAT, the SYSCALL and SYSENTER MSRs, KERNEL_GS_BASE,
  TSC_AUX, SPEC_CTRL, XCR0, DR0-DR3, DR6, DR7, the XSAVE area (its standard
  form, no compaction), an event pending (its vector, type, error code), the
  interrupt shadow, NMIs blocked and an NMI pending, an exception the host
  owes it, what it is doing (running, halted, halted with interrupts off,
  waiting for a SIPI), KVM's PV EOI MSR -- each backend reading its VMCB or
  VMCS into it and back.
- **The local APIC**: its registers, the request, in-service and trigger
  bits, the timer as a count and the time left of it, the TSC deadline;
  APICv's virtual APIC page and the posted interrupts folded into it on VT-x
  before it is read. **The mailbox**: what other CPUs sent the CPU and it
  had not taken -- edges, levels, an NMI, INIT, a start-up vector.
- **Each device's state**: the UART's registers and what it had received;
  the PIC's two chips and ELCR; the PIT's channels, each as its count and
  the time since it was loaded; the RTC's registers and CMOS; the PM
  block's registers and its timer as an offset; pvpanic's; the IO-APIC's
  redirection table, its lines and remote IRR; PCI's configuration space of
  every function and each device's MSI-X pending bits; a virtio device's
  features, status, ISR and each queue's size, address, indices and its
  event index; a NIC's MAC, queue pairs and the flows its steering keeps.
- **The pages** are not in it: they are sent apart, in rounds (M2), or
  written after it for a save to a file.

Each device is **drained** before its state is read: a disk's every request
in flight answered -- read into the guest, or written to the volume -- and
its completion in the used ring; a NIC's frame held for want of a buffer
let go, as a frame lost on the wire is, and its interrupts held for
moderation given. A queue then stands where its driver can go on from: what
the driver had made available and the device not taken stays in the
guest's memory, and the restored device takes it at its first look. And its
state is **refused, never trusted**, as it is read: a queue's size not a
power of two or past the most, a ring's address outside the guest's RAM,
an index past what its size allows, a selector past what the device has, a
segment's attributes no descriptor could have, an XSAVE area whose header
names a component this CPU does not have or whose MXCSR has a reserved bit
set -- an `XRSTOR` of which would fault in the host -- is an error, and
nothing of the state taken.

What the state does not carry is what is the host's: where the vCPUs run
(D places them itself), the VPIDs and ASIDs, the VMCS's host fields, the
MSI-X ring mappings (made again by the restored device), the counters
(started from zero, and said so).

## Pausing a running VM

`LinuxGuest::pause()` asks every CPU to park and rings each: a CPU at the
top of its loop -- before `prepare`, so outside its guest -- reads its own
state into the parked slot and waits on its doorbell. It reads it there,
on its own host CPU, because that is where its VMCS is current: under VT-x
an exit reads only what its handling needs, and the rest
(`read_whole_state`) only while the VMCS is still current on that CPU. A
halted CPU is rung out of its halt; one asleep for good is rung too. The
caller waits for every CPU to have parked, with a time limit, and then
drains the devices -- the disks' tasks still serve while the CPUs are
parked, and their interrupts go into the mailboxes, which the state
carries -- and reads the devices under the platform's lock. Then either:

- `resume()`: every CPU goes on from where it parked, nothing changed --
  a migration given up, a save that failed;
- `end(Stop::Migrated)`: every CPU's loop returns, the guest stopped as by
  `hv stop` but saying where it went, and its memory given back.

A guest built from a state (`LinuxGuest::from_state`) is a guest built as
`hv start` builds one -- its memory, its devices, its CPUs -- with no kernel
loaded and none of its furniture laid out (the ACPI and SMBIOS tables, the
MP table, the zero page are in the memory that comes), the state then
applied: each device's, each CPU's into its backend's shadow, which its
first entry writes whole into its VMCB or VMCS.

## Time

- **The TSC**: the guest's is the host's plus an offset, VMCB's
  `tsc_offset`, the VMCS's TSC offset with "use TSC offsetting" on, 0 at a
  VM's start as now. At a restore the offset that makes the guest's TSC go
  on from the value the state says, the same for every CPU. The policy's
  read of IA32_TSC adds it; a TSC deadline the guest writes, in its TSC, is
  made host time by taking it off (`tsc_ns`).
- **The devices' clocks** -- the PIT's channels, the local APICs' timers,
  the PM timer, the moderation of a NIC's interrupts -- are kept as times on
  the host's boot clock. The state carries them as times relative to the
  pause, and a restore puts them back relative to its own now: the pause is
  left out of the guest's time on both, the TSC and the devices agreeing.
  The RTC is the wall clock, read again on the new host.
- What the guest's wall clock lags by -- the pause -- NTP takes back, as
  after any pause of a VM.

## Memory: the dirty log (M2)

- **A bit a page**, per region, in chunks no larger than the kernel's heap
  gives: set by a write the guest makes to a page made read-only for the
  log -- the nested fault (NPF with the write bit on AMD-V, an EPT
  violation on VT-x) on a page of RAM marks it and gives the leaf its write
  back, under the table's lock, and the guest's instruction runs again --
  and set by every write the host makes for the guest: `GuestMemory::write`
  and `write_u16` mark the pages they touch. The rings a device has mapped
  (`GuestMemory::map`) are written with no call here; the devices name
  them (`rings()`), and every round sends them whatever the log says.
- **The tables change their rule**: written once today, with a log their
  entries' write bit taken away and given back. The 2 MiB leaves of the
  huge pool are split into 4 KiB ones as the log begins -- the tables made
  and filled while the guest runs, swapped in with every CPU parked for
  the moment it takes, so that no CPU fetches through a page whose size
  changes under it (Intel's erratum, KVM's iTLB multihit) -- the frames
  staying where they are. A split is never undone: a migration given up
  leaves 4 KiB leaves, said in the report.
- **Every CPU's TLB flushed before a round copies**: the guest's
  `tlb_epoch` moved on and every CPU kicked; a CPU looks at the epoch after
  `prepare`, and one that finds it moved enters under a fresh ASID (AMD-V)
  or after an INVEPT of its EPT (VT-x). The kick's protocol makes that a
  barrier: once every CPU's mode has been seen outside its guest after the
  kick, none can enter again without having looked.
- **Rounds**: round 0 every page; each round after, the pages written
  during the one before -- the bits taken (swapped out a word at a time),
  those pages made read-only, the TLBs flushed, then copied: a write after
  the flush is logged again, so a page's last write is never missed.
  Stopped once what is left fits the pause at the last round's rate (D4),
  or the vCPUs slowed -- each CPU's task made to wait a while between
  entries, more each round -- or given up.
- **The last round** with the vCPUs parked and the devices drained: what is
  left, the rings, then the state.
- **Zero pages** go as a mark, no data: a guest's untouched memory is most
  of a fresh one.

## The way between hosts

A mailbox of hv's on each host, `hv.migrate`, a `Cluster` name, reached at
`hv.migrate@<host>` over ipcnet. Messages of 496 KiB at most -- *as built*:
not ipcnet's 1 MiB, which a mailbox drops whole past the heap's largest
block, 512 KiB: a page run of up to 112 pages of data and 4096 numbers --
each page its number and whether it is zero --
or up to 4096 zero pages; the state in pieces; the folder's files in
pieces. A window of 2 MiB unanswered, as nbsclient's: D answers each
message it has written, so that ipcnet's queue is never what bounces one.
Every message names its migration (a random 64-bit id S chose for its
offer -- *as built*; D's window is 2 MiB and 32 messages unanswered),
and anything of another is refused.

## The handover (M1)

Between the old host's hv (S) and the new one's (D):

1. **S offers** the VM: its vm.toml, its memory and CPUs, its volumes, its
   ports, its CPU model's digest and TSC rate, the files of its folder.
   **D checks** it can run it -- its CPU model and TSC rate, memory and
   the huge pool's pages, cores, its networks, the volumes there, the name
   free -- holds the name, places its CPUs, builds the guest's memory and
   devices with no backend behind its disks and NICs yet, writes the folder
   marked coming, and accepts with the migration's id; or refuses, saying
   why, and S says so.
2. **The pages**, in rounds (M2), or all at once with the VM paused (M1's
   cold move).
3. **The pause on S**: its vCPUs parked, its devices drained, its volumes'
   sessions idle -- nothing in flight, nothing more taken -- the last pages
   and the state sent, then `Done`, with each volume's writer generation
   and each port's node.
4. **The commit, on D**: the state applied -- refused, and the migration
   given up, where it does not hold. Then the first volume attached for
   writing **only where its writer is S's at the generation `Done`
   named** -- NBS's new conditional attach, `expect` -- the rest after it
   the same way, their backends swapped into the disks; the ports moved to
   D (`vnet port move`, a compare and set from S's node), the NICs' backends
   swapped in; then D resumes it and tells S it runs.
5. **S lets go of it** on hearing so: its memory freed, its sessions
   closed (fenced already: their close attaches nobody), its folder taken
   out, the VM off its list.

**The decision is one compare and set, in NBS's raft: the first volume's
writer.** D commits by taking it from S's generation; S gives up -- before
the commit -- by taking it anew itself (*as built*: an attach of its own
with `expect` set to its generation and a token of its own, whose new
session takes the place of the guest's first disk's; first written as
`Reattach`, the writer's own). Whichever lands first decides, and the
other's then fails: D's attach is refused where S took it anew, S's where D
took it -- refused as `NotTheWriter`, which each side tells from no answer,
and asks again with the same token where it had none. So:

- Anything that fails before `Done` leaves the VM on S, resumed: D frees
  what it built.
- D finding the state wrong, or its attach refused, gives up and says so:
  S resumes.
- S hearing nothing after `Done` -- D gone, or the way -- asks NBS by its
  own take: taken, it resumes the VM, and D's attach will be refused if
  it ever comes; refused, D has it, and S lets go -- never on a guess.
- D dying after its attach and before it runs the VM leaves the VM on NBS,
  stopped: started again cold where there is room, by hand with `force`,
  or by vmctl's HA later. Its folder is on D, marked coming, and on S,
  marked gone to D; `hv vms` says both.
- After the commit, S never runs it again: its writer fenced, its ports
  moved away. A port's move that fails after the commit is retried by D,
  the VM running meanwhile without its network, and said.

A VM with no volume of NBS's has nothing to decide by, and is refused.

## The network (M3)

- **Frames between the move and every host's table**: each host's keeper
  asks vnet for its table every second, so for up to a second the other
  hosts send a moved port's frames to S, whose switch has shut it. S sends
  what comes for a port it gave up to its new host through the tunnel for
  a few seconds after the move, and vnet's provider wakes the keepers of a
  change as it applies one rather than waiting to be asked. (*As built*:
  ten seconds, from the handover on, its VM still ending (`moved_away`);
  D takes those for that port alone from S alone though their sender is a
  third host's port, which the tunnel would otherwise refuse
  (`came_here`); vnet's member tells the mailbox `hv.keeper` of each new
  revision as it applies it, `vnetmeta::KEEPER`.)
- **The firewall's flows** -- what the port's stateful firewall let out,
  whose answers it lets in -- go with the state and are put into D's port
  as it is claimed: without them the answers to the VM's own connections
  are dropped on D until it sends again.
- **Public addresses** need nothing: their host serves them for the port
  wherever it is, the route's node changed (V4's `same_mapping`).
- **The gateway** a guest has cached is the network's, its address and
  MAC the same on every host, so the guest's ARP cache stays right.
- **Masqueraded connections** are lost (D1).

## The CPU (M4)

- **The model's digest** (*as built*: the offer carries the answers
  themselves, `policy::model_answers`, and D holds its own to them, naming
  the first leaf that differs; `policy::model_digest` -- a 256-bit digest of
  four FNV lanes, which tells two CPUs apart and holds off no forger -- is
  what `hv info` says per CPU. The MSR covered is ARCH_CAPABILITIES; the
  cache leaves 2, 4, 0x80000005, 0x80000006 and 0x8000001D and the clock's,
  0x16, are left out, as a hybrid CPU's P and E cores answer them each their
  own way), first written as SHA-256 over the policy's answer to every CPUID
  leaf and subleaf a guest can ask (leaves 0 to the policy's last, the
  hypervisor range, 0x80000000 to the last extended, leaf 4's, 7's, 0xB's,
  0xD's, 0x1F's and 0x8000001D's subleaves) for the VM's shape, and to the
  MSRs it answers from the host's (`ARCH_CAPABILITIES`, the speculation
  controls offered, the MTRR capabilities, `PLATFORM_INFO`, `HWCR`): two
  hosts whose digests agree give a guest the same CPU. D refuses a VM whose
  digest is not its own, and names the first leaf that differs. Between
  Intel and AMD the vendor alone differs: never.
- **The TSC rate**: D refuses one more than 250 ppm from its own.
- A VM's own CPU model in its vm.toml (`cpu=baseline`, D2's (b)) is later.

## Steps

Each step a commit or a few on `vm-migration`, its gates green before the
next; what each changes and what holds it:

- **M0a. The format**: `common/vm/vmstate` -- the types, `encode`,
  `decode` with every check; hv-fuzz's `vmstate` target: hostile bytes
  refused and never a panic, every state written read back the same, every
  field's bound refused one past it.
- **M0b. The devices and the local APIC**: each device's, the local APIC's
  and the mailbox's `save` and `restore`, a drain for the disks and the
  NICs; hv-fuzz's device targets round-tripping each device at random
  points: saved, restored into a fresh one, saved again -- the same -- and
  both run on alike for the rest of the input.
- **M0c. The CPUs**: each backend's CPU into the common form and back --
  AMD-V's VMCB, VT-x's VMCS through its shadow and a full sync, the XSAVE
  area out of `GuestFp` and back in checked, the events, the NMI and
  interrupt-window state, APICv's page folded -- and the TSC offset;
  hv-test's built-in guests: a new one, `saved`, stopped at an exit in the
  middle of its work, its CPU read out and written into a fresh VM's, and
  going on there to finish it: under AMD-V on TCG and under KVM, and under
  VT-x on the EX44.
- **M0d. A guest paused, saved and restored**: `pause`, the park point,
  `resume`, `end`, `from_state`; `hv save <vm> <file>` and
  `hv restore <file>` -- the state and the pages in one file, the VM's
  vm.toml with them, the VM off its host until restored. Held by:
  hv-fuzz's `platform` target pausing and resuming at random points, and
  saving and restoring into a guest of its own -- the run going on with
  every invariant it has; a Linux guest saved and restored under load,
  many times, under both backends, by hand (hv-linux-test.py `--save`).
  The biggest step, and the one a forgotten field makes a rare corruption
  of: the round trips are its gate.
- **M1a. NBS's conditional attach**: `expect` through nbsmeta, nbswire,
  nbs-mds, nbsclient and nbsdrive; nbs-fuzz's `meta` target holding it to
  its model, its mutants catching its rule taken out; raft-test's
  `nbsmeta` target.
- **M1b. The protocol**: `common/vm/migwire` -- the messages and both
  sides' state machines with no I/O; hv-fuzz's `handover` target: S, D,
  NBS's writer and vnet's port as models, messages lost, late and
  duplicated, either host failing at every step -- the VM run by one host
  at most at every moment, and once both are quiet, by exactly one unless
  D died after its commit; its mutants.
- **M1c. A cold move**: `migrate.rs` on both sides, the VM paused, its
  pages and state sent, the handover, D's guest from the state;
  `hv migrate <vm> <host> cold`; the folder copied and marked. Held by a
  gate of its own on `tests/hub.py` (`vm-migration-test.py`, manual, as
  hv-nbs-test is): three nos with ipcnet, the registry, NBS and vnet, a
  Linux guest on a volume and a network of the cluster's moved from a to
  b and going on -- a counter it keeps in memory still counting, a file it
  wrote before still there, one it writes after landing on the volume --
  and the refusals.
- **M2. Live**: the dirty log, the split leaves, the rounds, the pause's
  target, the slowing, zero pages. Held by: hv-fuzz's `dirtylog` target --
  vCPUs and devices writing while rounds copy, the copy the same as the
  memory at the end, a write after its page's copy never lost -- and its
  mutants; the gate above, with the guest writing its memory as it moves.
- **M3. The network kept**: the old host's sending on, the keepers told,
  the firewall's flows. Held by the gate above: a TCP stream through a
  probe on a third host, and pings, through a move.
- **M4. The CPU model and the TSC rate**: the digest and the checks, in
  D's answer to the offer; hv-fuzz's `policy` target: the digest the same
  for the same host and shape, another for any leaf changed.
- **M5. The job and the panel**: the vm provider's `migrate` (a job, its
  progress, cancelled while it copies), the hosts a VM may go to; the
  panel's button; web-test for the descriptor, the panel's Playwright
  tests. Then vmctl's evacuation and rebalancing on top.

Docs as each step lands: docs/hypervisor.md's "Moving a VM to another
host", docs/testing.md's sections for the new gates, CLAUDE.md's table.

## Limits

- Only between CPUs of one vendor -- of one model until D2's (b). On the
  stand today the EX44 is Intel and the AX41's two nos AMD, nested under
  KVM: ax41a and ax41b alone, each a guest of a guest.
- Only VMs on the cluster's networks with their disks on NBS.
- VT-x's half is tested only where VT-x is: the EX44, which is the stand's.
- The memory goes at what the way between hosts carries: a gigabit,
  until the hosts have 10 GbE.
- A VM's huge pages, its own cores (`hv cores`): D must have the same
  room, or the move is refused. Where its CPUs run on D is D's to say:
  `cpu=` and a switch port pinned in its vm.toml are the old host's.
- A migration given up leaves the guest's huge-pool memory mapped in 4 KiB
  leaves until it next boots.

## Later

- Post-copy, for a VM that writes faster than the way carries.
- Intel's PML, and EPT's and NPT's dirty bits, for the dirty log.
- The memory compressed on the way.
- D1's (b): masqueraded flows sent back through the tunnel.
- A host's evacuation and rebalancing, vmctl's V3.
