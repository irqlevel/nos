# Live migration of VMs between a cluster's hosts

A running Linux VM moved to another host of the cluster: its memory copied
while it runs, then paused for the last of it and its CPUs' and devices'
state, its disks' writer and its network port handed over, and resumed
there -- a pause the guest sees as a hiccup, its TCP connections, which live
in its memory, kept. It is what [vmctl.md](vmctl.md)'s V3 builds on: a
host's evacuation for maintenance, and rebalancing.

Not started: this is the plan, written on 2026-11-16 at the user's word
("напиши план ... но пока не делай"), the decisions below the user's to
make first.

## Where it starts

What is there, and why the disk and the network are most of the way:

- **The disk does not move.** A VM's disks are volumes of NBS
  ([nbs.md](nbs.md)), on every host of the cluster alike. What moves is
  their writer: NBS fences a volume's writer by its attach generation, and
  takes a writer anew by host, generation and token (nbsmeta's `Reattach`)
  -- the old one's writes refused from then on. nbsclient already pauses a
  writer for a snapshot.
- **The network does not move.** On a network of the cluster's
  ([vnet.md](vnet.md)) a VM's address is vnet's, its MAC `02:00` and the
  address's four bytes, whatever host it is on. A port has a host and a
  generation, `vnet port move` changes the host, the old host's keeper
  shuts the port and the new one attaches it. A public address is served by
  the host it is routed to wherever its VM runs (V4), so a VM's connections
  through it survive a move. The cluster's firewalls are the same on every
  host.
- **Between hosts**: ipcnet, a VM's state and its memory going as
  messages of 1 MiB at most (`ipcwire::MESSAGE_MAX`), its bulk class's
  queue bounded -- what nbsclient's window keeps to.
- **The devices are few and the tree's own**: the local APIC, the IO-APIC,
  the PIC, the PIT, the RTC, the UART, the ACPI PM block, pvpanic, PCI with
  MSI-X, virtio's transport, its block and net devices
  (`src/crates/hypervisor/hv/src/devices/`) -- and the hv fuzzer runs that
  very code on the host.
- **The jobs**: the vm provider's long calls are jobs with progress, the
  panel follows them.

What is not, checked in the code on 2026-11-16:

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
  pool is mapped in 2 MiB leaves.
- **A guest's TSC is the host's**: no offset, and no kvmclock -- the TSC
  being invariant and the host's ([policy.rs](../src/crates/hypervisor/hv/src/policy.rs)).
  On another host it would jump by the two hosts' uptimes' difference.
- **A guest's CPUID is its host's**, the policy built from the host's side:
  a guest moved to a CPU without a feature it uses faults, or worse.
- **A VM's folder is its host's**: /vms/<name> -- vm.toml, its cloud-init
  seed (`seed.iso`, `user-data`), console.log.
- **The way between hosts is slow for memory**: 1 GbE on the stand, and TLS
  in software (ChaCha20-Poly1305 about 200 MB/s a core, docs/tls.md). A
  4 GiB VM's first pass takes 35-40 s at best; a guest that dirties more
  than that a second never converges without being slowed.

## What it is

```
$ hv migrate web ax41b          # a job: rounds, dirty rate, what is left, the downtime
hv: web moving to ax41b: 4096 MiB, round 1 of its pages 38 s, round 2 3 s, round 3 0.4 s
hv: web paused 180 ms: its state and the last 1.2 MiB, its disks' writer and its port handed over
hv: web runs on ax41b
```

- **What moves**: a running VM whose NICs are on networks of the cluster's
  and whose disks are NBS volumes, to a host of the same CPU model and TSC
  rate with room for it.
- **What is refused, and said**: a disk of the host's own (a file, a
  device, a partition), a NIC on a host's network, a forward on the host's
  address (`hv forward`, the host's own), a CPU model or a TSC rate the
  other host cannot give, no room there, a name taken there.
- **What the guest sees**: a pause; time going on as it went (its TSC
  offset carried); its connections kept -- through a public address and
  inside the cluster's networks; those it made masqueraded from its old
  host's address as decision D1 says.
- **On the bus**: the vm provider's job `migrate` (progress: rounds, pages
  sent, the dirty rate, the estimated pause), cancelled while it copies; a
  button on a VM's page in the panel, the hosts it may go to offered.

## Decisions (the user's, before M0)

- **D1, connections masqueraded from the old host.** A VM with no public
  address goes out through its own host's NAT, from that host's address:
  after the move its new packets leave from another address, and the other
  ends drop those connections. (a) Accept it, and say so: the VMs that keep
  every connection have a public address. (b) Keep them: the new host sends
  the packets of the flows the old host's NAT had for the VM back to it
  through the tunnel -- V4's way for public addresses -- until they end.
  Recommended: (a) in M1-M2, (b) in M3 if wanted.
- **D2, the CPU model.** (a) A VM moves only between hosts whose CPUs
  offer the same CPUID, checked. (b) A VM made `cpu=baseline` gets a
  CPUID of what every host of the cluster has, and only such a VM moves.
  (c) Named models, as QEMU's. Recommended: (a) to start, then (b).
  Between Intel and AMD: never.
- **D3, the TSC rate.** (a) Refused unless the two hosts' TSC rates agree
  within a bound. (b) TSC scaling: AMD's TSC ratio MSR, VT-x's TSC
  multiplier. Recommended: (a).
- **D4, the pause.** A target -- 300 ms -- the rounds stopping once what is
  left fits it; past N rounds the vCPUs slowed (auto-converge), and past a
  limit given up, the VM staying where it was. Post-copy (resuming before
  the memory is there, pages fetched on fault) not in this plan.
- **D5, the way.** ipcnet, as NBS goes: its keys, its routing, its bulk
  class; a mailbox of hv's on each host (`hv.migrate`, a `Cluster` name),
  flow control of its own as nbsclient's window. Or a stream of its own
  between the hosts. Recommended: ipcnet.
- **D6, the VM's definition.** Its folder copied to the new host and taken
  out of the old one once it runs there. Names are a host's today: one
  taken on the other host is refused. A definition of the cluster's is
  vmctl's.
- **D7, who moves a VM.** A person, by `hv migrate`, the job or the panel.
  A host's evacuation and rebalancing are vmctl's, after.

## The state (M0)

One versioned format for a VM's state, in a crate of its own under
`common/` (no kernel, no `unsafe`), which hv writes and reads and the
fuzzers link as they are -- the same as [05-live-update.md](05-live-update.md)'s
manifest, which carries it across a kexec instead of a network:

- **The VM**: the format's version, the VM's UUID, its CPU model, the TSC
  rate it was made at, its memory map (each range: a guest-physical
  address and a size).
- **Each vCPU**, in a form of neither backend's: the general registers,
  RIP and RFLAGS, the control registers, the segments (selector, base,
  limit, attributes), GDTR and IDTR, EFER, PAT, the SYSCALL and SYSENTER
  MSRs, KERNEL_GS_BASE, TSC_AUX, SPEC_CTRL, the debug registers, the XSAVE
  area, the guest's TSC at the pause, an event pending (its vector, type,
  error code), the interrupt shadow, NMIs blocked, halted or waiting for
  a SIPI, the PV EOI MSR -- each backend reading its VMCB or VMCS into it
  and back.
- **The local APIC**, APICv's virtual APIC page and the posted interrupts
  folded into it on VT-x before it is read.
- **Each device's state**: its registers, a virtio device's features, its
  queues' addresses, sizes and indices, MSI-X's table and pending bits,
  PCI's configuration space.
- **The pages** are not in it: they are sent apart, in rounds (M2), or
  written after it for a save to a file.

Each device is **drained** before its state is read -- every request in
flight answered, a virtio queue at a point the guest's driver can go on
from -- and its state **refused, never trusted**, as it is read: a size, an
index or an address outside what the VM has is an error. A state comes from
another host, which is authenticated, and from another build, which is
not the same code: the version says which, and an older one is read or
refused, never guessed.

The guest's TSC becomes the host's plus an offset (VMCB's `tsc_offset`,
VMCS's TSC offset), 0 at a VM's start as now: at a restore, the offset that
continues the TSC the state says. The local APIC's TSC-deadline timers, set
in the guest's TSC, go on.

## Memory: pages written while they are copied (M2)

- **Each page a guest writes is logged**: the VM's nested entries made
  read-only for a migration, a write's fault (NPF with the write bit on
  AMD-V, an EPT violation on VT-x) marking the page in the VM's bitmap and
  giving the entry its write back. Intel's PML later.
- **The tables change their rule**: written once today, with a
  migration their entries' write bit taken away and given back -- under
  the VM's lock, every vCPU's TLB flushed for it (kicked out, its ASID or
  VPID flushed before it enters again) before a round begins. The 2 MiB
  leaves of the huge pool split into 4 KiB ones for the migration, the
  frames staying where they are.
- **Rounds**: round 0 every page; each round after, the pages written
  during the one before; a page made read-only before it is copied, so a
  write after its copy is never lost. Stopped once what is left fits the
  pause (D4), or slowed, or given up.
- **The last round** with the vCPUs held: what is left, then the state.

## The handover (M1)

Between the old host's hv (S) and the new one's (D), over ipc:

1. **S asks D** whether it can run the VM: its CPU model and TSC rate, its
   memory and cores, the huge pool's pages, its networks and firewall,
   its volumes. D holds the room and the name.
2. **The folder** -- vm.toml, the seed, user-data -- written on D, marked
   coming; the VM made there, its memory taken, not running.
3. **The pages**, in rounds (M2), or all at once with the VM paused (M1's
   cold move).
4. **The pause on S**: its vCPUs held, its devices drained, its volumes'
   sessions flushed, its state sent.
5. **The commit**: D takes its volumes' writer -- NBS's `Reattach`,
   committed by NBS's raft: from then on S's writes are refused -- and its
   ports (`vnet port move` to D, their generation up: S's keeper shuts
   them); then D resumes it.
6. **D says it runs**, and S lets go of it: its memory freed, its folder
   taken out.

Before the commit anything that fails leaves the VM on S, resumed. After
it, S never runs it again: its writer fenced, its ports shut. Should D die
then, the VM is on NBS, started again cold where there is room (by hand,
or vmctl's HA). The commit is NBS's writer: S, cut off from D after
sending its state, asks NBS who writes the volume before it resumes or
gives the VM up -- never on a guess.

## The network (M3)

- **The port's new host reaches the other hosts** by vnet's table, which
  each host's keeper asks for every second: up to a second of packets to
  S, whose switch has shut the port. Either S sends what comes for a port
  it gave up to D through the tunnel for a while, or vnet tells the keepers
  of a change rather than waiting to be asked.
- **The firewall's flows** -- what the port's stateful firewall let out,
  whose answers it lets in -- sent with the state and taken on D: or the
  answers to the VM's own connections are dropped on D until it sends
  again.
- **Public addresses** need nothing: their host serves them for the port
  wherever it is, the route's node changed (V4's `same_mapping`).
- **Masqueraded connections** as D1 says.

## The CPU (M4)

- A VM's CPU model (D2) in its vm.toml; hv's policy giving it that CPUID
  and the MSRs that go with it, the same on both hosts; a host refusing a
  model it cannot give.
- The TSC rate checked (D3).

## Steps

- **M0. A VM's state as data**: the format's crate; each backend's vCPU and
  each device read into it and written back; drained devices; the TSC
  offset; `hv save <vm> <file>` and `hv restore <file>` on one host -- a VM
  paused into a file, and running again from it. Held by: a fuzz target of
  the format (hostile bytes refused, every state written read back the
  same); the hv fuzzer's guests saved at random points and restored into a
  machine of their own, going on as the one never saved does; a Linux guest
  saved and restored under load, many times, under both backends (by hand,
  as `hv-linux-test.py`). The biggest step, and the one a forgotten field
  makes a rare corruption of: the fuzz target is its gate.
- **M1. A cold move**: the VM paused, its pages and state sent, its writer
  and ports handed over, resumed on D; the protocol above, and its undoing.
  Held by: a fuzz target of the handover -- S, D, NBS's writer and vnet's
  port as models, either host failing at every step: never run twice,
  never lost; a gate of its own on `tests/hub.py` (`vm-migration-test.py`):
  two nos with ipcnet, the registry, NBS and vnet, one of hv's built-in
  guests moved and going on.
- **M2. Live**: the dirty log, the split leaves, the rounds, the pause's
  target, the slowing. Held by: a fuzz target of the log -- vCPUs writing
  while rounds copy, the copy the same as the memory at the end, a write
  after its page's copy never lost; a Linux guest moved under load on real
  hosts.
- **M3. The network kept**: the old host's sending on, or vnet's telling;
  the firewall's flows; masquerade per D1. Held by: a TCP stream and an
  ssh session through a move, on the stand.
- **M4. CPU models and the TSC rate.**
- **M5. The job and the panel**: progress, cancelling, the hosts a VM may
  go to; then vmctl's evacuation and rebalancing on top.

## Limits

- Only between CPUs of one vendor -- of one model until D2's (b). On the
  stand today the EX44 is Intel and the AX41's two nos AMD, nested under
  KVM: ax41a and ax41b alone, each a guest of a guest.
- Only VMs on the cluster's networks with their disks on NBS.
- The memory goes at what the way between hosts and its TLS carry: a
  gigabit, and ChaCha20-Poly1305 in software, until the kernel has SIMD
  crypto (docs/tls.md) and the hosts 10 GbE.
- A VM's huge pages, its own cores (`hv cores`), its placement: D must
  have the same room, or the move is refused.

## Later

- Post-copy, for a VM that writes faster than the way carries.
- Intel's PML for the dirty log.
- The memory compressed, or its zero pages skipped, on the way.
- A host's evacuation and rebalancing, vmctl's V3.
