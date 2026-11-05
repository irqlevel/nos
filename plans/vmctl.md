# vmctl: a cluster's VMs, and the host each runs on

Asked 2026-11-05: a scheduler that starts a VM on a host with room for it,
weighing how full each host is. A component of its own, after NBS. It is
sketched here so that what it will need is kept ready in the plans before
it ([raft.md](raft.md), [cluster.md](cluster.md), [nbs.md](nbs.md)).

## Where it starts

- **A VM belongs to a host today.** Its folder, `/vms/<name>/vm.toml`, is on
  that host's root, and that host's hv starts it (`hv start`). Its CPUs go on
  the cores the host's `hv::place` gives it: whole cores of its own by
  default (`Cores::Own`), shared only when asked. Its memory comes from the
  huge pool or the heap. `modules/hv/src/cores.rs` decides and takes those
  holds under one lock.
- **Nothing knows the cluster's VMs**: the panel asks every host's API in
  turn.
- **What it will stand on**: the raft crate; the registry (hosts, labels,
  maintenance); ipc; NBS, whose disks belong to no host; and later the VM
  networks across hosts (ipc.md), whose networks belong to no host either.

## What it is

- **`vmctl.ko` on every host**: a Raft group of its own (the raft crate's
  `rsm`). Its state:
  - every VM of the cluster: its definition (what a vm.toml says today),
    what it should be doing (running or stopped), the host it is placed on,
    and a generation;
  - each host's capacity as the host last reported it, and what is reserved
    on it.
- **The scheduler** is its leader's controller, as NBS's repair is:
  level-triggered, over a pure `place(state, vm) -> host`, deterministic and
  fuzzed.
- **An agent on every host**, in hv since hv runs the VMs, makes the host do
  what the state says of it. It starts what is placed there, stops what is
  not, and reports what runs and how much room is left.

## Where a VM goes

A host is a candidate only if:
- it is up, and not in maintenance (the registry's flag);
- it has room: free whole cores for the VM's vCPUs, counted as `hv::place`
  counts them, and the memory -- the huge pool's free 2 MiB pages, where the
  VM asks for them;
- it has what the VM needs that only some hosts have. A device disk
  (`part:`, `disk:`) or a file disk pins a VM to its host, and so does a
  host-local network; NBS disks and networks across hosts do not;
- its labels match the VM's: `gateway` for a VM with a public address, a
  rack;
- no other member of the VM's anti-affinity group is there -- three
  Kubernetes control-plane VMs on three hosts, say.

Among the candidates:
1. Hosts that hold replicas of the VM's NBS root volume come first, so its
   reads stay local.
2. Then, by the cluster's policy, either the emptiest host -- *spread*, the
   default, so that a lost host takes as few VMs with it as can be -- or the
   fullest that fits -- *pack*, so that big VMs still find room.
3. Ties are broken by a hash of the VM's id.

Room is reserved as the placement is committed, so two VMs never get the
last core of one host. The host's agent then places the VM exactly
(`hv::place`) and takes its holds. Where the controller's guess was wrong,
the agent says no, and the VM goes to the next host.

## A VM restarted where its host died

- **A VM's `restart`**: `never`, `here` (on its own host only), or
  `anywhere`.
- **Fence first, restart second.** A host that cannot reach the
  controller's majority for 20 s stops its VMs itself: its lease has lapsed.
  The controller starts them elsewhere only after 30 s and a margin. NBS's
  attach generation is the backstop: an old instance that kept running
  anyway cannot write its disks once the new attach has fenced them.
- **A host back** with VMs that are placed elsewhere now stops them: its
  agent goes by the state.
- **The VM's network** (the VM networks across hosts, later) moves its port
  with it, so its address answers at the new host.

## The API and the panel

- `vm create` without a host: the controller places it. Any host's API
  takes the call and forwards it over ipc -- the same cluster-wide API the
  CSI driver wants (nbs.md, "Later: Kubernetes on NBS").
- The panel's hosts page shows each host's room and its VMs, from the
  controller rather than from asking every host.

## Later: live migration

A running VM moved to another host, its memory copied while it runs. Its
disk on NBS and its network across hosts need not move. Live migration is
what maintenance as evacuation and rebalancing are built from.

## Kept ready for it now

- The raft crate is generic ([raft.md](raft.md)).
- The registry has hosts' labels, and a maintenance flag that NBS and
  vmctl both read ([cluster.md](cluster.md)).
- NBS says which hosts hold a volume's replicas, for locality, and fences a
  volume's writer by its attach generation, the backstop for HA
  ([nbs.md](nbs.md)).
- hv's placement (`hv::place`) is a pure function already, and hosts will
  report their capacity in its units.

## What hv will need

- To run a VM from a definition it is handed (the controller's), not only
  from a folder of its own.
- Its capacity as a read: free cores by topology, the huge pool's free
  pages, memory.
- Names unique in the cluster, not only on a host; the controller enforces
  that. A VM's UUID is its own already.

## Tested

- **A fuzz target** for `place` and the controller against a model of
  their own, with hosts failing, partitions and leases. It holds that room
  is never exceeded; that the constraints hold; and that a lost host's VMs
  are started again once, and never run twice.
- **`vmctl-test.py`** on `scripts/hub.py`:
  - VMs placed across hosts by room;
  - a host killed, and its VMs started elsewhere after the lease;
  - a partitioned host stopping its own VMs;
  - a host back, stopping the VMs placed elsewhere.

## Steps, after NBS

- **V1**: the state, the scheduler and the agents; VMs made and started
  where there is room.
- **V2**: HA: leases, fencing of its own, restarts elsewhere.
- **V3**: live migration, then maintenance as evacuation, and rebalancing.
