# NBS: replicated block storage for VM disks, images and snapshots

Asked 2026-11-05: fault-tolerant network block storage for nos, NBS, for
VMs' disks, images and snapshots. An mds (metadata service) and a cs (chunk
service) on every nos server; three replicas; Raft for consensus, a leader
and followers; every mds keeping the whole state on its disk and in memory;
every cs given its own pool of block devices, partitions or whole disks; the
mds's data and the configuration in a folder of nos's root; a VM given a
disk of a size from NBS, its requests going first to the local mds, which
knows where the replicas are, and on to them; a fixed service port; the
services as kernel modules, `nbs-mds`, `nbs-cs` and so on. Built on the
hv-local-disks branch (device ids, claims, `DiskQueue`), over the kernel's
message channel ([ipc.md](ipc.md)), which the user proposed as the one
channel every service talks over, and on a Raft crate of its own, generic,
for other users after NBS ([raft.md](raft.md)).

## The ask, as read -- to confirm

- **The mds is not on the data path.** The client asks it where (the mds of
  its own host, which answers from memory) and sends reads and writes to the
  cs's itself. Through the mds, every byte would take one more hop and the
  mds's CPU.
- **A volume has one writer**: attached to one VM at a time, the attach
  fenced, so that a VM started again elsewhere is safe from its old host's
  late writes.
- **"Allocated from NBS"**: a volume's room is reserved and its chunks placed
  when it is made; its data is written when the guest writes, and what was
  never written reads as zeros.

## Where it starts

- **hv-local-disks** (21b67b1e, not merged): devices by id (`part:<GUID>`,
  `disk:nvme-<model>_<serial>`, `Disk::find_ident`), claims with a holder's
  name, `kcore::block::DiskQueue` (asynchronous; flush and discard; no FUA),
  and the refusals (an EFI or BIOS boot partition never, a device with
  something on it only with `force`). A cs's pool is that.
- **A guest's disk in hv**: `blk::Backend` over `QueuedDisk`, whose `Media`
  is an `ImageQueue` or a `DiskQueue` (`modules/hypervisor/hv/src/disk.rs`); 64 requests
  in flight (`IN_FLIGHT`), 256 KiB the largest (`REQUEST_BYTES`); a flush is
  synchronous in the disk's task.
- **Files for the mds**: `kcore::fs::append` (by path, not synced),
  `File::sync` (every write to the file's filesystem, on the medium),
  `rename`, `set_len`, `create_dir`; the root is ext4, journalled.
- **The heap gives at most 512 KiB at once**: a state of tens of MiB is held
  in pieces (fs-fuzz's heap ceiling is how that is held to).
- **What a sync costs**: on the boxes' consumer NVMe drives 1 to 4 ms (the
  AX41's: a FUA write 4.3 ms, a flush 1.4 ms; FUA on the EX44's Samsung costs
  a flush). Every synced step on the write path costs that.
- **netblk** serves a disk over UDP at the 1 GbE line rate with zero copy on
  both Hetzner boxes, with no authentication: what the transport was weighed
  against (ipc.md, "TCP, not UDP").
- **Two servers in the data centre** (EX44, AX41): three replicas on three
  hosts, and a Raft majority, need a third.

## The pieces

```
  host A (B and C the same)
  +--------------------------------------------------------------+
  | hv.ko -- vm web, vda = nbs:web-root                          |
  |   nbsclient: a read from one replica, a write to all three --+--ipc--> nbs.cs on B, on C
  |      | where is chunk 17?        | this host's replica       |
  |      v                           v                           |
  |   nbs.mds (nbs-mds.ko) <======== Raft over ipc ==============+======> nbs.mds on B, on C
  |     the whole state, in memory and under /nbs/mds            |
  |   nbs.cs  (nbs-cs.ko) --- DiskQueue ---> part:..., disk:nvme-...
  |   ipcnet.ko ============== TLS over TCP, port 39240 =========+======> B, C
  +--------------------------------------------------------------+
```

Crates with no kernel in them and no `unsafe`, each run on the host by its
fuzzer:

- `raft`: Raft as a crate of its own, generic, with nothing of NBS or of nos
  in it -- the protocol with no I/O, its wire, a durable log over a trait of
  files, and a replicated state machine with client sessions and reads
  ([raft.md](raft.md)).
- `nbswire`: the messages between clients, mds's and cs's, every length
  checked.
- `nbsmeta`: the mds's state and the commands that change it -- what Raft
  replicates.
- `nbscs`: a cs's device format and its rules (slots, fences, open marks,
  rebuilds) over a device trait.
- `nbsclient`: the client's rules (the map, writes to all, reads from one,
  flushes, failures, reconciling) over a channel trait.

Modules, the glue to kcore, the channel, the files and the devices:
`nbs-mds.ko`, `nbs-cs.ko`, the client in `hv.ko`, and `nbsload.ko` -- a
verifying load, as `blkload` is -- for the tests and the numbers. Under
them, `ipcnet.ko` between hosts and `cluster.ko`, the registry of the
cluster's hosts ([cluster.md](cluster.md)).

## Who is in the cluster

Two things say it, at two levels:

- **The hosts that trust each other** are the cluster registry's
  ([cluster.md](cluster.md)): each host joined it once, with a one-time
  token, and every member's ipcnet pins it from the list the registry keeps.
  Every connection proves both of its ends ([ipc.md](ipc.md), "Who may send
  to whom"). That says who may talk, not who stores what.
- **The NBS cluster** is its Raft group: an id made by `nbs init`, and the
  members of its committed configuration. A host is in it when that
  configuration has it, and in no other way. Every mds holds the whole
  membership, since it is part of the state.

What carries the NBS cluster's identity:

- **Its id**: 128 random bits made at `nbs init`. It is kept in
  `/nbs/nbs.toml` and in the state, and written into every NBS message's
  header, every snapshot's header and every pool device's superblock. A
  message with another id is dropped and logged; a device with another id
  is not used.
- **A member's id**: its host's ipc name and an incarnation, 64 random bits
  made when that host's mds state was made, at `init` or at its first
  contact from a leader. A host whose `/nbs/mds/` was lost comes back with a
  new incarnation, and it is not the old member: Raft must never count the
  vote of a member that forgot its log. The host is told so, and taken back
  only as a new member (`nbs host replace ax41`: the old member removed, the
  new one added).
- **The sender of every message is checked twice**: the member id it names
  must be in the configuration, and it must be on the host the ipc
  connection proves.

Joining is one command, since the registry vouches for the host already:

1. On a member, `nbs host add ax41` -- a host the registry has, with the key
   it has for it. The leader adds the host as a learner and sends its mds the
   snapshot and the log. Once it has caught up, it is made a voter if there
   are fewer than five.
2. ax41's mds, with no state yet, makes its member id and takes that first
   contact from an NBS leader on a host the registry has, and from nowhere
   else. A wiped host answers a leader of its old cluster that it has no
   such member, since its incarnation is new.
3. Its cs's devices: `nbs devices add <id>` writes the cluster's id and the
   cs's onto each. A device that names another cluster or another host -- a
   disk moved between servers -- is refused with whose it is; `force`
   formats it into this cluster, and its data is lost.

Leaving: `nbs host remove ax41`, on any member, drains its cs first -- its
chunks copied onto three other hosts -- and then takes it out of the
configuration. Without `force` it is refused where that would leave a chunk
with fewer than three replicas, or the cluster with fewer than three hosts.

## What is stored

- **The cluster**: an id, a name; three replicas and 4 MiB chunks, both
  fixed when it is made.
- **A host**: its ipc name; its mds a voter (up to five) or a learner.
- **A cs**, one a host: its devices, each by its id, its size in slots and
  its state (active, draining, failed).
- **A volume**: an id, a name, a size (whole MiB), a kind (volume, image,
  snapshot), the image or snapshot it was cloned from, its attach (host,
  generation, read-write or read-only), and its map, chunk by chunk.
- **A replica set**: three cs's on three hosts, each active or rebuilding,
  and a version. Sets are shared: the chunks placed on the same three cs's
  name the same set, so a host that is lost changes the few sets it is in,
  not every chunk it holds.
- **A chunk**: 4 MiB; the set it is in -- that set and its version are the
  chunk's epoch; how many maps name it (a volume's and its snapshots', an
  image's and its clones'); whether anything was ever written to it.

A chunk costs 12 bytes:
- 4 in its volume's map;
- 4 for its set;
- 2 for its count;
- 1 of flags;
- 1 spare.

A TiB of volumes is 262 144 chunks, so 3 MiB of state. Every byte of a
volume is kept three times, so that is about 1 MiB for every TiB of the
pools' disks. 10 TiB of volumes is 30 MiB, held in pieces.

The floor is 4 bytes a chunk, its reference in the map alone, and it is not
worth reaching: it takes two kinds of map entry, one for a chunk only one
volume has and one for a shared chunk.

## The mds

- **On every host, all of it**: in memory, and on the root under
  `/nbs/mds/` -- `hardstate` (term, vote), `log/*.seg`, `snapshot`.
- **Raft** is the `raft` crate's: its `rsm` with `nbsmeta` as the state
  machine, its `log` on the root through `kcore::fs`, its messages over the
  channel. Up to five voters, and any further host a learner, which holds
  everything and does not vote. A commit is one round trip and a sync on each
  of a majority; three voters survive one loss and five two.
- **Through Raft** goes every change to what is stored: a volume made,
  deleted, attached, detached, resized, snapshotted, cloned; a chunk's
  replica set and epoch; the cs's, their devices, the hosts. **Not through
  it**: liveness, capacity in use, and I/O.
- **Deterministic.** Placement is computed in a command's apply from the
  committed state alone -- no clock, no randomness, no floats, no hash map's
  order -- so making a 1 TiB volume is one small entry, and every mds places
  its 262 144 chunks alike, by the rules of "Where replicas go" below. Every
  so many entries the leader proposes a digest of
  the state, and an mds whose own differs stops serving and says so: a
  determinism bug is a report, not a silent fork.
- **Reads at an index**: the local mds answers from memory once it has
  applied as far as the asker says (its map's index, or the one a refusal
  named), so an answer is as fresh as the commit it rests on, with no round
  trip to the leader. Changes go to the leader; a follower forwards them.
  The local mds is asked first and another host's when it is not there --
  over the channel the two are the same call.
- **On disk**: the Raft log and a snapshot of the state, below.
- **As leader**: it runs the controller that keeps three replicas of every
  chunk ("How three are kept", below), and the garbage collection.
- **Membership**: `nbs init <cluster>` on the first host; then `nbs host
  add`, `nbs host remove` and `nbs host replace` on any member ("Who is in
  the cluster", above). Voters are changed one at a time.

## The mds's state on disk

The state is in memory whole, by the ask; the disk is for keeping it across
a restart. So the disk holds two things, and nothing has to be looked up
there:

- **The Raft log**, in `raft::log`'s format, under `/nbs/mds/log/`: entries
  appended and synced before a follower acknowledges them or the leader
  counts itself (Raft's rule), several entries to a sync.
- **The newest snapshot**, `/nbs/mds/snapshot`: every table of the state
  dumped in order, in pieces of at most 256 KiB, each with its CRC, behind a
  header (index, term, members, the state's digest). It is written to a new
  file, synced and renamed over the old one; then the log before it goes.
- **A start** reads the snapshot and replays the log after it.
- **When a snapshot is taken**: once the log since the last one is as large
  as the state (64 MiB at least). So every byte of log costs at most one
  byte of snapshot, which is twice the writing in all; and a start never
  replays more log than the state's own size.
- **Taken without stopping**: the tables are kept in pieces of at most 1024
  records, each piece shared copy-on-write (an `Arc`, `make_mut`). A
  snapshot takes the pieces' handles at its index and a task of its own
  writes them out. The apply goes on meanwhile, copying a piece only where
  it changes one the snapshot still holds.
- **The digest that hosts compare** is the snapshot's: its bytes are the
  state's in a fixed order, so two hosts with the same state write the same
  bytes.
- **Sizes**: some 3 MiB of state for every TiB of volumes. For the boxes'
  few TiB that is about ten MiB, written in well under a second and read
  back in about as long. The EX44 and the AX41 have 64 GB of memory.

Not an LSM tree. An LSM keeps a state larger than memory, and writes a
heavily written one a piece at a time. Ours is in memory by the ask and is
written rarely: volumes made and attached, epochs, copy-ups. Its
write-ahead log (the Raft log) and its compaction (the snapshot) are there
already: it is an LSM of one level, with no memtable to flush, no manifest,
no levels, no compaction policy and no Bloom filters, all of them code to get
right. It is what ZooKeeper, HDFS's NameNode (edit log and fsimage), the
masters of MooseFS and LizardFS, Kafka's KRaft controllers and Consul do.

This is to be looked at again if the state ever outgrows memory, or costs
too much to rewrite -- hundreds of MiB, a hundred TiB of volumes. A state
read from memory and written rarely is then better served by a
copy-on-write B+tree in one file (bbolt's shape, LMDB's) than by an LSM. If
anything in NBS earns an LSM, it is a cs's block-level metadata, should the
cs ever become log-structured (redirect-on-write, snapshots by the block):
Ceph's BlueStore keeps that in RocksDB.

## The cs

- **Its pool**, in `/nbs/nbs.toml`: `devices = ["part:...", "disk:nvme-..."]`,
  each claimed for writing as `nbs-cs` and opened as a `DiskQueue`, with hv's
  refusals. `nbs devices add <id> [force]`, `nbs devices drain <id>`.
- **A format of its own on each device**, no filesystem under it: a
  superblock (cluster, cs, device, slot size and count, version, CRC); a
  table of slots, one record a slot (chunk; the epoch the cs knows it at, and
  the epoch it was last written at; state -- free, active, open, rebuilding;
  the newest generation written; CRC), every sector of it kept twice with a
  sequence number, so that a torn write leaves the older copy good; then the
  slots, 4 MiB each. A slot is taken at its chunk's first write. A record
  changes at the first write of a generation or of an epoch, not at every
  write.
- **Fences**: per volume, the newest attach generation the cs has seen,
  written to its devices and synced before the fence is acknowledged. A
  request of an older generation is refused.
- **Open marks**: before the first write of a generation to a chunk, its
  record says open, synced before that write starts. The client closes a
  chunk once every replica has flushed it and nothing has been written to it
  for a while; a close lost in a crash only costs a needless reconcile. After
  a crash, the open chunks are the only ones whose replicas may differ.
- **A worker task per device**, pinned, asleep on one event for its mailbox
  and its queue (ipc.md, I2). A request is checked (fence; epoch, asked of
  the local mds when it is newer than the slot's), mapped to its slot,
  started on the device and answered. A chunk never written reads as zeros.
- **Reports to the leader**: capacity, the devices' health, which chunks it
  holds and which have been written to.

## The client, in hv

- **vm.toml**: `disks = ["nbs:web-root"]`, beside files and devices.
  `hv create web nbs size=20G` makes a volume, `hv create web
  image=nbs:ubuntu-24.04 size=20G` clones an image, and `hv create web <url>
  nbs size=20G` pours a cloud image into a new volume (diskimage's
  converter, as onto a device).
- **The disk's serial** (virtio-blk's GET_ID) is `nbs-<volume id>`, so that
  a guest finds it as `/dev/disk/by-id/virtio-nbs-<id>` whatever its order
  among the disks.
- **A third `Media`**, beside `ImageQueue` and `DiskQueue`: the disk task's
  loop stays as it is. `start` sends to the replicas, `reap` takes the
  answers off the client's mailbox, `wait` sleeps on its event. No task of
  its own.
- **A start**: attach (read-write, this host), which gives generation G and
  the map from the local mds at the attach's index; fence every cs that holds
  a replica (one that does not answer is taken out of its chunks' sets, as
  for a write); reconcile what an earlier attach left open; serve. **A
  stop**: flush, close, detach.
- **Reads** go to one active replica -- never one still being rebuilt --
  this host's where there is one; **writes** to every replica in the chunk's
  set, a rebuilding one included; **a flush** to every cs written to since
  the last. Two writes that overlap are never out at once -- the second
  waits for the first -- so no two replicas apply them in different orders.

## Replication: the rules

1. Which set a chunk is in, and every set's members and version, are the
   mds's, through Raft. A change of a set's members raises its version, and
   a chunk moved to another set takes that set's. The set and its version
   are the chunk's epoch.
2. A write is answered to the guest only once every replica of the chunk's
   current set has it. A replica that has not answered in 0.5 s is taken out
   of its set (the set's version up, committed) before the write is
   answered: a write is never answered on a set the mds would not vouch for.
   The client asks; the leader takes it out only once the cs does not answer
   the leader either -- a client whose own way to a healthy cs is broken
   waits, rather than strip that cs from sets other volumes share. With one
   replica left, writes go on to it, said loudly in the log and in `nbs`.
3. A cs refuses a request whose epoch is older than its own, answering with
   the newer one and the index it was committed at; a newer epoch it checks
   with its own host's mds at that index before it takes it and writes it
   down. A change that adds a replica reaches the replicas already in the set
   before the new one starts copying, so no write can miss it.
4. A cs refuses a request of an attach generation older than its fence. A
   volume is attached for writing once at a time; attaching it elsewhere
   while its old host lives takes `force`, and fences. Read-only, it may be
   attached any number of times at once, while nobody has it for writing --
   Kubernetes's ReadOnlyMany, below.
5. Durability is the guest's flush: a write answered before a flush that has
   completed is on every replica's medium; one after may be lost by a crash,
   as on a disk with a write cache -- which is what virtio-blk tells the
   guest it has.
6. After an unclean detach, a chunk open on any replica is made the same on
   all -- copied from any one of them, granules compared by checksum first --
   before the volume is served again. Nothing past the last flush was
   promised, so any replica's version will do; every other chunk is the same
   on all of them already.
7. A discard frees whole chunks only: a freed slot reads as zeros on every
   replica alike. The rest of a discard is left as it was -- virtio's discard
   promises nothing of what is read after it -- and a write of zeros
   (`WRITE_ZEROES`) is a write.

## Where replicas go

A placement is made by one pure function of the committed state,
`place(state, what) -> sets`. It is the same function in the apply of a
volume's creation and in the leader's repair plans, so every mds reaches the
same answer and the fuzzer can hold it to its rules:

1. **Three replicas on three hosts.** A host is the failure domain. Later,
   labels in the registry -- a rack, a room -- can make a larger one.
2. **Only cs's that are up and take new data**: not draining, not out, on
   devices with room.
3. **One replica on the volume's own host** where it has room -- the host it
   was made on and is attached from -- so that its reads are local.
4. **The other two on the hosts with the most room left**, ties broken by a
   hash of the chunk's id. A volume's chunks spread over many sets, so a lost
   host's chunks are copied back from many hosts at once, not from one.
5. **Room is counted as reserved, never as used.** A placement never takes
   more than a device has, and a volume that does not fit on three hosts is
   refused whole.
6. **Room is kept to rebuild a host.** With four hosts or more, a new volume
   is refused when what would be left could not take the biggest host's
   replicas. With three hosts there is nowhere to rebuild to, so this does
   not apply.
7. **A copy-up** -- a write to a chunk a snapshot or a clone shares -- goes
   onto the same set as the chunk it copies, so that each cs copies locally.

All of it in integers, in the state's own order: no floats, no clock, and no
randomness but a hash of ids.

## How three are kept

The leader runs a controller. From the committed state and what it hears --
ipc's monitors on every host's `nbs.cs`, and each cs's reports of its
devices and chunks -- it works out what is wrong and proposes the commands
that put it right. It does that at every change it hears of, and every 10 s
besides. It keeps no plan of its own between turns, so a new leader simply
goes on, and nothing heard twice or missed once can mislead it.

| What it sees | What it commits |
|---|---|
| a cs unreachable for 5 s | `cs down`: marked down in every set it is in, each set's version up. Writes go on to the other two, and the chunks are *degraded* |
| down for 10 min (`out_after`), a device reported failed, or `nbs host remove` | its replicas taken out. For each chunk, `place` finds a third on another host, *rebuilding*. Chunks move in batches of 64 (256 MiB), each committed once its copies are done, so chunks get their third copy back one batch at a time, not when the whole host is done |
| a cs back before `out_after` | it reports its chunks and when each was last written. A chunk nobody wrote meanwhile is active again at once; the rest are copied from a replica that has them. A reboot costs what was written during it |
| a device or a host drained | the same moves as for one that is lost, but the old replica stays active until the new one is: four copies for a while, never two |
| no host with room for a third | nothing to commit: the chunks stay at two, and say so |

- **Order**: chunks with one replica left before chunks with two; within
  each, the longest degraded first.
- **Rate**: rebuilds are held to a share of each link (`rebuild_mbps`, a
  third by default), so that a rebuild does not starve the guests.
- **A copy** goes from cs to cs: the new replica's cs pulls from an active
  one, and checks every granule by checksum before its batch is committed.
  Writes reach a rebuilding replica all the while (rule 3 of "Replication").
- **Seen**: `nbs status` counts the chunks at three replicas, at two, at one
  and at none; what is being rebuilt, and how fast; and why anything is stuck
  (no room, too few hosts). A line goes into the log whenever the count of
  chunks at fewer than three changes.
- **Planned reboots**: the registry's maintenance flag on a host (`cluster
  maintenance ax41 on`, cluster.md) holds off `out_after` for
  that host, so a reboot that takes longer than 10 min copies nothing.

What this cannot do: with exactly three hosts, a host lost for good leaves
every chunk at two replicas until it comes back or a fourth host is added,
since there is no third host to put them on. Keeping three on its own takes
four hosts or more.

**Tested**:
- fuzz/nbs's `meta` target holds `place` and the controller to the rules:
  three hosts, room never exceeded, the volume's host taken where it can be,
  chunks spread. It also holds them to progress: after any failures, once
  hosts and room allow, every chunk is back at three within so many turns.
- The `cluster` target loses, returns and drains hosts and devices under
  writes.
- `nbs-test.py`, with `out_after` short, kills one host of four for good and
  waits for every chunk to be back at three. It also reboots one before
  `out_after`, to count that only what was written meanwhile is copied.

## When something fails

| What | What happens |
|---|---|
| a device of a cs | its chunks reported lost there; each rebuilt from another replica |
| a host, or its cs | after 5 s the leader takes it out of the sets it is in (their versions up: a few sets, not every chunk) and writes go on to two; after 10 min its chunks are rebuilt elsewhere, onto three again; a host back before that copies only what was written meanwhile |
| the mds leader | Raft elects another within seconds; I/O goes on meanwhile, since it does not pass through the mds |
| a partition | the minority's mds's commit nothing; a client there goes on while it reaches every replica and needs nothing changed, and waits when it does -- safety before availability |
| the host a VM runs on | the volume stays attached to it; starting the VM on another host is a forced attach: fence, reconcile, serve |
| the cluster's power | each cs comes back from its slot table, each mds from its log; each volume's open chunks are reconciled at its next attach |
| two replicas of a chunk | the third serves alone, and is copied onto two more |
| all three | the chunk's data is gone: its reads fail, loudly -- never zeros for a chunk the cs's reported written |

## Snapshots, images, clones

- **A snapshot** of an attached volume: the mds asks its client, over the
  channel, to stop writing and flush; commits the snapshot (the map copied,
  each chunk's count up); the client goes on. A write to a chunk now shared is
  a copy-up: a new chunk placed on the same three cs's as the old, which each
  copy locally -- some 2 ms for 4 MiB on NVMe -- and then the write. The
  snapshot reserves room for those copy-ups, as much again as the volume, on
  those cs's: no write fails for space. Thin snapshots and clones, which
  reserve nothing and so can run a pool out, are a later option.
- **An image** is a volume frozen read-only. `hv convert <image> nbs:<name>`
  and `hv create ... <url> nbs` pour one in.
- **A clone** is a volume whose chunks are its image's until written: a VM
  from an image at once. Its room is reserved as any volume's: no pool runs
  out of space under a guest.
- **A delete** counts its chunks down; one at zero is freed, its cs's told,
  its slots given back and discarded on the devices.

## Files, commands, ports

- `/nbs/nbs.toml` (the `conf` crate): the cluster, this host's mds role, the
  cs's devices, limits. `/nbs/mds/`: the mds's Raft state. A cs's state is on
  its devices.
- `/etc/rc`: `insmod /lib/ipcnet.ko`, `/lib/nbs-mds.ko`, `/lib/nbs-cs.ko`,
  then `hv`.
- The shell: `nbs init`, `nbs status`, `nbs hosts`, `nbs host
  add|remove|replace`, `nbs cs`, `nbs volumes`, `nbs volume
  create|delete|resize|snapshot|clone|freeze`, `nbs devices add|drain`; and
  `ipc`.
- An `nbs` provider on the bus with its own descriptor, for the API and the
  panel. The `vm` provider's descriptor is at the bus's 16 KiB already, so
  what NBS adds is `nbs`'s methods, not `vm`'s. Its changes can be called
  again with the same result, as an automation that retries needs (a CSI
  driver above all, below). A volume is made by a name its caller chooses:
  made again with the same name and size, the volume is answered as it is;
  made with another size, refused. Deleting one that is gone succeeds. A
  volume's answer says which hosts hold its replicas, so that a scheduler
  can start its VM where the reads stay local ([vmctl.md](vmctl.md)).
- One port, `ipcnet`'s 39240. NBS has none of its own: its services are
  names, `nbs.mds` and `nbs.cs`, on every host. `ipcnet` listens on the
  NIC its config names, so NBS's traffic moves off the public interface
  onto a private network -- a Hetzner vSwitch's VLAN -- once `net` has
  VLANs ([ipc.md](ipc.md), "Later: VM networks across hosts").

## Tested

1. **`fuzz/nbs`** (host, in CI): the crates as they are, several hosts in one
   process, nothing real. A simulated channel (order per pair, delays, what
   was in flight lost when a connection breaks, partitions), simulated
   devices (the fuzzers' common disks: a write cache, a power switch, requests
   that fail, torn writes), a simulated root for each mds (what was not synced
   lost or torn at a crash), and a clock. Every run the same for its seed, as
   in TigerBeetle's VOPR and FoundationDB's simulation. Raft itself is
   `fuzz/raft`'s ([raft.md](raft.md)); here it runs as `nbs-mds` runs it.
   Targets:
   - `meta`: the state machine against a model of its own: volumes, chunks,
     counts, three distinct hosts a chunk, no room reserved twice.
   - `cs`: the device format through power cuts and torn writes: a fence or
     an open mark acknowledged never lost, no data on the medium past an open
     mark that is not.
   - `cluster`: whole hosts -- mds, cs, a client writing and reading random
     blocks -- through crashes, partitions, devices failing, attaches forced
     elsewhere, snapshots and clones. Every read held to a model of the
     volume (with one writer: the last completed write's data, or an
     overlapping one in flight's); flushed data never lost while one host is;
     replicas the same after every reconcile and rebuild; no write of a fenced
     generation on any medium.
   - `wire`: every message read from noise without a panic, and read back as
     written.
2. **On the kernel**: ipc's gates ([ipc.md](ipc.md)); `nbs-test.py` (both
   arches; three to five arm64 nos under HVF are the fast loop) on
   `scripts/hub.py`: a cluster from `/etc/rc`, volumes made, `nbsload`
   writing and verifying, hosts' QEMUs killed and booted again, partitions,
   the leader killed, a device pulled, every rebuild's replicas compared; then
   `hv-nbs-test.py` (x86-64): a guest on an NBS disk, its host killed, the VM
   started on another, its filesystem clean to `e2fsck`.
3. **On the hardware**: the EX44, the AX41 and a third host; numbers against
   a device disk on the same machine.

## Steps

Each a round on a branch of its own, with its gates and a CLAUDE.md row for
each new gate.

- **N0.** hv-local-disks merged: the cs stands on it.
- **N1.** ipc in the image ([ipc.md](ipc.md), I1 and I2).
- **N2.** ipc between hosts ([ipc.md](ipc.md), I3 and I4): TCP's changes
  first ([tcp.md](tcp.md), T1 to T3), then `ipcnet`.
- **N3, Raft.** The `raft` crate, R1 to R3 ([raft.md](raft.md)).
- **N4, the registry.** The cluster's hosts in one place, raft's first user
  ([cluster.md](cluster.md), C1 and C2). From here on, `hosts.toml` is the
  registry's.
- **N5, the mds.** `nbs-mds.ko` running the raft crate's `rsm` over the
  channel with an empty state, its log on the root; `nbs init`, `nbs host
  add`, `nbs status`; nbs-test's first phase: elections, a killed leader,
  partitions, restarts that keep the log, a wiped host refused as the old
  member. Done (docs/nbs.md). N0 to N4 were done before it. As built:
  - **raftmember**: the member engine was taken out of cluster.ko into a
    crate of its own, `src/crates/modules/cluster/raftmember`, which both
    modules link. It holds the task, raft over ipc, commands handed to the
    leader, the membership kept to the state's voters, and the log's files.
    Each group's own part is its `App`. The registry's byte codec became
    `common/codec` the same way.
  - **nbsmeta** for now holds the cluster's identity and its members by
    incarnation, the removed ones, and the voters (the first five). Its
    fuzz target is `fuzz/raft`'s `nbsmeta`, beside the registry's; fuzz/nbs
    comes with N6, which grows the state.
  - **The join is an offer**: the member `nbs host add` is typed at offers
    the cluster to `nbs.mds@<host>`. The host's mds, in no cluster, makes its
    incarnation, its member file and an empty log, and answers. Then `Add`
    commits, and the leader adds it as a learner and makes it a voter once
    it holds the log. While a host is in no cluster, a listener holds
    `nbs.mds` to take the offer.
  - **A wiped host** answers raft's messages for its old member with "no
    such member". The leader says so (`nbs`, and its log), an add of the
    host is declined by the host itself, and `nbs host replace` removes the
    old member for good and offers a new one. `nbs host remove` is there
    too; `nbs hosts` names `nbs status`'s list.
  - **The folder is `/nbs/mds`** (`member`, `raft/`). `/nbs/nbs.toml` waits
    for the cs's devices (N7).
  - **Tests**: `scripts/nbs-test.py`, three arm64 nos on hub.py with the
    registry made among them first.
- **N6, the metadata.** `nbsmeta` and its target: volumes, hosts, devices,
  placement, the snapshot of the state, the digest; the commands and the
  `nbs` provider. Done (docs/nbs.md), with decisions 4, 5 and 11 taken as
  written. As built:
  - **What is stored** (`nbsmeta`'s `storage.rs`) is as "What is stored"
    has it:
    - a cs a host, kept by the host's name, so a member replaced keeps its
      cs and devices;
    - a device by its id, with its slots: 4 MiB each, after a slot for the
      cs's superblock and its table of slots twice (`device_slots`);
    - replica sets shared by every chunk on the same three cs's;
    - a chunk in 8 bytes (its set, its count, its flags), a freed one
      chained onto a free list through its set;
    - a volume with its own host, its writer by generation, its readers,
      and its map.

    Every table is bounded, and every count is held to its rules again
    when a snapshot is read back.
  - **Placement** (`place.rs`) is "Where replicas go", in integers:
    - the volume's own host first, then the most room left, ties broken by
      a hash of the volume, the chunk and the cs;
    - found whole on a copy of the room, then made, so a volume that does
      not fit takes nothing;
    - with four hosts or more, refused where it leaves too little room to
      rebuild the biggest host, counted in sum;
    - refused as too few hosts when fewer than three take new chunks at
      all, as no room when three or more do and are full.
  - **The commands** are a device added and drained; a volume made (made
    again the same is answered as it is), deleted (one gone already
    answered so), grown (never shrunk); an attach (a new generation each
    time, another host's writer refused unless forced) and a detach by
    generation; and the digest. The attach and the detach are in the
    state for N8, and nothing asks for them yet.
  - **The snapshot is a stream.** `FrozenState` writes the tables a piece
    at a time from the copy-on-write handles taken at its index, and a
    restore reads them back through `codec::Stream`, held to every rule
    (`check_storage`: the counts, the free list walked, each cs's room).
    The state is never in one buffer, which the kernel's heap would refuse
    past 512 KiB.
  - **The digest** is the SHA-256 of the snapshot's bytes, taken every 64
    entries. The leader proposes its own, every member holds its own to
    it, and a member whose own differs stops (raftmember's `App::halt`)
    and says where.
  - **Read your writes**: a change is answered once the host it was typed
    at has applied it too (raftmember's `ask`, the leader's answer carrying
    the entry's index). nbs-test's x86-64 run found the gap: the member
    fills in an answer during its turn and publishes its view only at the
    turn's end, so a volume made and then read on one host could be missing.
  - **When the log is compacted**: raft's floor of 1 MiB, not the 64 MiB
    "The mds's state on disk" has. A small state is cheap to write, and a
    start then replays less. `compact=` changes it; nbs-test gives 4096.
  - **The shell and the bus**: `nbs devices add|drain`, `nbs cs`, `nbs
    volume create|delete|resize`, `nbs volumes`, `nbs volume <name>`, and
    the `nbs` provider with its descriptor.
    - Reads are this member's view.
    - `nbs devices add` finds the device on this host by its id or its
      name, and refuses one with no id or one that something holds.
      Nothing is written to it before N7.
  - **Tests**:
    - `fuzz/nbs`'s `meta` target against a model of its own (its own
      placement, an fsck of its own, snapshots read back, spoiled and
      continued, and two members' digests), with fifteen mutants;
    - `fuzz/raft`'s `nbsmeta` target, taken along;
    - nbs-test's N6 phase on three hosts' pools, through restarts, a host
      replaced by a snapshot, and a member removed.
- **N7, the cs.** `nbscs` and its target; `nbs-cs.ko`: its pool (ids,
  claims, `DiskQueue`, refusals), fences, open marks, reads and writes,
  garbage. Done (docs/nbs.md, "The chunk server"), with decision 3 taken as
  written: the client to the cs's, each cs checking with its own host's mds.
  As built:
  - **Its pool is the metadata's**, not a file of the cs's: the devices
    `nbs devices add` put there for this host's cs, asked of the host's mds
    every 5 s. `nbs devices add` refuses a device with anything on it but
    zeros or this cluster's superblock; `wipe` (the plan's `force`) zeroes
    its first 8 KiB once the metadata has it. The cs formats a blank device,
    leaves anything else alone and says why, and serves only once it has
    taken every device in -- with one missing, a chunk on it would read as
    zeros and be written twice.
  - **The format** (`nbscs`'s `format.rs`) is the plan's, with every record
    -- a slot's and a fence's -- its own: a sequence number and a CRC32C
    seeded with the format's 64-bit id. A page is written in copy A, flushed,
    then in copy B; the fences are a table of their own in slot 0, the same on
    every device. A device is blank only where its first two pages are zeros
    past a superblock's 128 bytes too, so that a filesystem whose first bytes
    are zeros is never formatted.
  - **A chunk has an incarnation** (nbsmeta's `Chunk.incarnation`, the
    snapshot's format NBM3): a freed chunk's number is taken again, and a
    slot of the chunk it was is never read or written for the one it is. The
    plan had no such thing; without it, a cs that missed a free would serve a
    new chunk the old one's data.
  - **What the fuzzer found**, through power cuts (`fuzz/nbs`'s `cs`
    target): a slot's zeros not flushed before the record that takes it,
    which let a record kept with its zeros lost read another volume's data; a
    record's first write torn in copy A taken for a lost record; a fence page
    left with one copy stale at the open, which the next cut could leave with
    neither. And in the module: a read not held to the chunk's volume. Each
    has its mutant now.
  - **One task**, not one a device: requests one at a time, each waited for
    on its device. N11 measures what more would take.
  - **No reports to the leader**: the cs asks its mds which of its chunks
    and fences are gone, every 30 s, and gives them back (garbage). What a
    cs holds is to be pulled by N9's repair rather than pushed.
  - **The wire** (`nbswire`): a client's requests and a cs's answers on
    `nbs.cs`, a cs's questions to `nbs.mds` and its answers; every request
    and answer between hosts by ipcnet's bulk connection, so that a client's
    keep their order and do not hold up raft.
  - **`nbs io`** in nbs-mds writes, reads, flushes, closes, attaches and
    detaches a volume from the shell, as N8's client will: fence each cs
    before its first write, write every replica, read one.
  - **Tests**:
    - `fuzz/nbs`'s `cs` target (power cuts with torn sectors and rot, every
      fence, open mark, close and sector held to what it may be) and `wire`
      target, with seventeen mutants of the two crates;
    - nbs-test's N7 phase: devices taken in and formatted, a device with
      something on it refused and wiped, a volume written to three replicas
      and read back, flushed, closed, a writer fenced out by a forced attach,
      a host booted again with its data, fence and open marks, a volume
      deleted with every cs giving its chunks and fence back, and a host's
      nbs-cs unloaded and loaded again.
- **N8, the client.** `nbsclient`; hv's `nbs:` disks; attach, fence,
  reconcile, flush; a replica taken out of its set after 0.5 s, the leader
  looking at the cs first (decision 6); a cloud image poured into a new
  volume (`hv create <name> <url> nbs size=...`, decided 2026-11-07), so that
  a VM boots off NBS; `nbsload.ko`; the `cluster` target; nbs-test's I/O
  phases, `hv-nbs-test.py`. A replica taken out stays out until N9 brings it
  back. As built:
  - **The client** (`nbsclient`, no kernel in it) is a session of a volume's,
    sans I/O: attach, the map, fences, the reconcile, serve, stop. It is
    driven over ipc by `nbsdrive` in hv.ko, nbs-mds.ko's `nbs io` and
    nbsload.ko, and over the fuzzer's own network by the `cluster` target.
  - **The client's asks** of its host's mds (`TAG_ASK` on `nbs.mds`): attach,
    detach, the map in pages, a replica out. A replica out is asked by the
    volume's writer of now, with its generation, of a set of its volume's:
    the metadata refuses a writer fenced since (`NotTheWriter`).
  - **The attach carries the session's token**, so that one asked again --
    its answer lost -- is answered with the generation it gave. Without it a
    retry took a second generation and fenced its own session out. The
    snapshot's format is NBM4.
  - **A replica out** (`ReplicaDown`): silent 0.5 s to the client, then the
    leader's look (`Op::Ping`, 250 ms) -- out only where it does not answer
    the leader either -- or refused as failed by the cs itself, taken as it
    is. The set's version moves on. The last active replica is never taken
    out; writes go on to it alone. Decided 2026-11-07, with decision 6.
  - **What the `cluster` target found**, and each has its mutant now:
    - a fenced writer's write done by taking out, through the leader's look,
      the healthy replicas that refused it;
    - a flush not fenced -- an old writer told its writes were durable after
      a newer writer's reconcile;
    - a cs that restarted with a chunk open refusing that writer's writes but
      not its flushes;
    - a flush done before the one ahead of it, which had taken the dirty
      cs's;
    - a cs counted dirty when sent a write rather than when it acknowledged
      it;
    - a stop that left a write queued behind an overlapping one, or a chunk
      a read held open, for good;
    - the last replica's loss of a writer's unflushed writes, which now
      fails the session (`LOST_LAST`) rather than leaving it waiting.
  - **hv's `nbs:<volume>` disks**: a third media of the disk's task's, the
    session's buffers lent to the device as an image's are; serial
    `nbs-<number>`, 4 KiB blocks; `hv start <name> force`; the pour, `hv
    convert <image> nbs:<volume>` and `hv create ... onto=nbs:<volume>` onto a
    volume made with `nbs volume create` -- a fresh volume's zeros left out.
    There is no `hv create ... nbs size=`: the volume is made first, by
    `nbs`, so that hv asks nbs-mds for nothing a client does not.
  - **Tests**: the `cluster` target (six hosts, crashes, takeovers now and
    at rest), fifty-four mutants in all, twelve of them the client's;
    nbs-test's I/O phase on sessions, and nbsload's verify; hv-nbs-test.py,
    Linux guests on volumes under KVM on the AX41, nested -- a pour read
    elsewhere, a volume moved between hosts, a forced start fencing the old
    guest, a host killed under a write.
- **N9, repair.** A host or a device lost, epochs, a rebuild under writes, a
  host back; a scrub comparing replicas by checksum. The design, as decided
  before it is built:
  - **A set and its joiner.** A set keeps its three members, each Active or
    Down -- the version a member went down at kept with it -- and at most
    one *joiner*: a cs being copied onto, written and not read, which takes
    a member's place once its copy is done. One mechanism for every case:
    a cs back before `out_after` joins in its own place and copies only what
    it missed; a cs out -- `out_after` past, its host removed -- is replaced
    by a new one in the place it leaves; a cs drained is replaced while it
    stays active, four copies for a while and never two. Joining and the
    join's end each raise the set's version.
  - **Rule 3, by the epochs the cs's keep per chunk.** The joiner pulls each
    chunk from an active member with the join's epoch: the source moves the
    chunk's known epoch on before it reads it (checked with its own mds),
    so a write of the epoch before is refused there from then on, and its
    writer, its map read again, writes the joiner too. A write the joiner
    has from a client marks its granules, and the copy leaves those alone:
    the client's are newer.
  - **What a cs back copies**: the chunks its source wrote at or after the
    version the member went down at -- the writes it may have missed -- and
    every chunk it no longer holds (a device lost with them); its other
    chunks are the same on every replica already.
  - **Verified before the end**: every granule copied is held to the
    source's by checksum before the joiner says it is done; one that
    differs and was not written meanwhile is copied again and said.
  - **The controller** is a pure function of the committed state and of
    what the leader sees of each cs -- answering or not, for how long, in
    maintenance or not -- in nbsmeta, the same that the fuzzer runs: a cs
    silent 5 s is down (`CsDown`: every set it is a member of, and every
    join it is the joiner of), one silent `out_after` (10 min, held off by
    the registry's maintenance flag) is out (`CsOut`); a set with a member
    down or out, or drained, is given a joiner (`Join`), those with one
    active member left first; `place` picks a new one -- up, not in the set,
    its host not a member's, the room for every chunk of the set, the most
    room first. A set is rebuilt whole: the batches of 64 the plan began
    with would mean splitting a set, which comes when the numbers ask for
    it. The leader proposes, every 10 s and at every change it hears of;
    the joiner's cs says when it is done (`Joined`), and a join whose joiner
    goes down is dropped with it.
  - **Steps**: N9a, nbsmeta's commands, the joiner, the controller and
    `place` for a joiner, the snapshot NBM5, the `meta` target's model and
    its progress rule (every set back at three active members within so
    many turns once hosts and room allow); N9b, the copy -- nbswire's pulls
    and checksums, nbscs's granule marks, nbs-cs's joiner; N9c, the
    controller in nbs-mds -- monitors of every `nbs.cs`, the timers, `nbs
    status`'s counts; N9d, the client writing the joiner, and the `cluster`
    target losing, bringing back and draining hosts under writes; N9e, the
    scrub; N9f, nbs-test with a short `out_after` -- a host of four lost for
    good and every chunk back at three, a host back that copies only what
    was written meanwhile -- docs and gates.
  - **N9a, done** (2026-11-08): a cs up, down or out (`CsDown`, `CsUp`,
    `CsOut`), a set's members down with the version each was active until,
    its joiner (`Join` at the version it was planned at, `Joined`,
    `Unjoin`), new chunks only into a set every member of which is active
    and with no joiner, and only on cs's that are up, an empty set made
    again before a new one; the snapshot NBM5; `nbsmeta::repair` -- the
    controller and the counts `nbs status` will show; the `meta` target's
    model of all of it, and its progress rule. What came up on the way:
    - **A cs's change and a join in one turn**: a join planned on the state
      before its cs came back up was refused as in place. The controller
      proposes the cs's changes alone, and joins at the next turn.
    - **A placement's count of the sets it takes** (found reading it): the
      trial counted every empty set as one to make again, the real pass
      only those whose version can move on, and the trial did not see that
      making again the empty clean set of three cs's picked later leaves
      those three needing one more -- with the table full, an
      `unreachable!` in nbs-mds. The count and the pass now share one rule
      (`spare`): an empty set is made again only where it is no clean set
      the placement fills. The table is never full in the fuzzer; the model
      holds the order the rule gives, and its own mutant fails the gate.
    - **Mutants**: twenty-two of N9a's and five re-pointed, 75 in all. Two
      survived the first run: `cs_down_keeps_last` took out a check that
      `touches` had made redundant -- the rule is in one place now -- and
      `joiner_checked` was out of reach of spoiling bytes at random, so a
      set's joiner is now found by its values and spoiled on purpose.
      `replica_down_drops_joiner` was caught by luck only, until a writer's
      `ReplicaDown` was drawn of a volume there is, of a set it has chunks
      in, and now and then of the set's joiner. Three mutants wait for N9d's
      `cluster` target with joins: the mds vouching a joiner for writes
      only, the client writing the joiner, the map naming its host.
- **N10.** Snapshots, images and clones.
- **N11, numbers.** On the hardware, and what they ask for: pages that move
  instead of copies (a message's pages lent to a `DiskQueue`), chain
  replication (the writer's NIC sending one copy instead of two), TCP's T4
  (SACK, RACK-TLP) if retransmissions cost,
  a UDP bulk path on a private link only if TCP measures as the limit.

## What to expect

- **Writes over 1 GbE**: two copies leave the writer's host, so some 55 MB/s
  a host; a small write's latency is a round trip and the slower replica's
  device.
- **Reads** from the local replica: the device's speed, less the copies --
  guest to the client's buffer, to a message, to the cs's buffer.
- **The first write to a cold chunk** waits for the open mark's sync, 1 to 4
  ms on these drives.
- **A host lost**: the writes to its chunks stall 0.5 s, the leader's look
  at the cs and a commit, then go on to two replicas.

## Decisions to confirm

1. Built as recommended (I4): ipc across hosts, Erlang's way -- not on one
   host only with NBS carrying its own traffic between hosts.
2. Built as recommended (I3, I4): TCP with TLS 1.3, keys pinned both ways, on
   one port, 39240; no UDP in the first version.
3. Decided (N7): the mds off the data path, the client to the cs's
   directly, each cs checking what it has not checked with its own host's
   mds.
4. Decided: one writer a volume, the attach fenced; a VM moved by a forced
   attach.
5. Decided: 4 MiB chunks; room reserved at create, a clone's too; data on
   write.
6. Decided (2026-11-07): the client writes to all three at once and is
   answered when all three have it; a replica silent for 0.5 s -- not the
   2 s first written -- is taken out of the set first, and only once the
   leader finds the cs silent too (rule 2). Writes answered by two of three
   without waiting for the third, and a quorum with versions on every
   replica, were weighed and left for the numbers to ask for (N11).
7. Built as recommended (N5): up to five voters, the rest learners.
8. Decided (2026-11-07): the runs on the hardware are nos on the EX44's
   bare metal, one of the hosts and the one the VMs on NBS run on, and nos
   VMs under KVM on the AX41 for the others. A fourth host, for the cluster
   to get back to three replicas by itself after a host is lost, is another
   VM there.
9. Built as recommended (N6): the mds's state on disk as the Raft log and
   snapshots of the state held in memory, not an LSM tree. An on-disk
   structure comes in only if the state ever outgrows memory.
10. Decided: the cluster's hosts and keys in a registry of their own, a
    small Raft group joined with a one-time token, built right after the
    raft crate and before NBS ([cluster.md](cluster.md)).
11. Decided: placement and repair as in "Where replicas go" and "How three
    are kept":
    - the volume's own host first, then the hosts with the most room;
    - a cs down after 5 s, out after 10 min;
    - rebuilds at a third of a link;
    - room kept to rebuild the biggest host, with four hosts or more.

## Limits of the first version

- One writer a volume: no shared disks.
- A write is as slow as its slowest replica, until that one is taken out.
- Writes are network-bound on 1 GbE.
- No erasure coding, compression, deduplication, QoS, encryption at rest,
  or second data centre.
- Copy-on-write by whole chunks: 4 MiB copied, locally, at the first write
  to a shared chunk.
- Three hosts at least, and four to get back to three replicas on its own
  after a host is lost for good.
- Only hv reads and writes a volume: an NBS volume is no block device of
  nos's, so nothing mounts one. That waits for a module to be able to
  register a block device.
- No live migration yet. NBS is what it needs: a VM's disk would no longer
  have to move with it.

## Later: Kubernetes on NBS

Asked 2026-11-05: a Kubernetes cluster in VMs on the hosts, its storage
from NBS. It fits. What Kubernetes asks of block storage, NBS has by design:
- volumes made and deleted by an API;
- one writer at a time, with a forced move fenced -- ReadWriteOnce, and the
  force-detach Kubernetes does when a node dies;
- three replicas;
- snapshots and clones -- VolumeSnapshot, a PVC made from another;
- a volume reachable from every host, so a pod's volume follows the pod to
  whichever node it lands on.

What is missing is the way in.

```
  pod --- PVC --- the CSI driver (Go, in the cluster: a controller and a node plugin)
                       |  HTTPS, mTLS: the host API (api.ko)
                       v
          nbs provider: create, delete, snapshot, clone, resize, capacity
          vm provider:  a volume attached to the node's VM, and detached
                       |
          hv on the node's host: the disk hot-plugged into the running VM
          -> the node plugin finds /dev/disk/by-id/virtio-nbs-<id>, formats and mounts it
```

- **The CSI driver**, about 2000 lines of Go with Kubernetes's own sidecars
  (provisioner, attacher, snapshotter, resizer). Its controller's
  `CreateVolume`, `DeleteVolume`, `CreateSnapshot`, `ControllerExpandVolume`
  and `GetCapacity` are `nbs` calls; `ControllerPublishVolume` and
  `ControllerUnpublishVolume` attach to the node's VM and detach. The node
  plugin finds the disk by its serial, formats it, mounts it and grows its
  filesystem. It lives in this tree, as the panel does, under `csi/`.
- **In hv: disks hot-plugged into a running VM.** A guest's disks are
  fixed at its start today, four at most (`run::MAX_DISKS`, `add_disk`
  before `load`), and a Kubernetes node takes dozens of volumes, added and
  removed as pods come and go. The way to it is a virtio-scsi controller --
  one PCI device, hundreds of LUNs, each added or removed with an event the
  guest's driver rescans on, with no PCI hot-plug -- or virtio-blk on PCIe
  ports with hot-plug, which is more of a PCI model. Also: a disk's size
  changed while it runs, with the config-change interrupt the guest grows
  it by.
- **In the API**:
  - roles narrower than admin and read: a CSI driver's key may manage
    volumes and attach them to the cluster's VMs, and nothing else;
  - one host's API reaching another's providers through ipc, so that the
    driver needs one address whatever host a node runs on (the bus over ipc,
    ipc.md's limits);
  - quotas of room for a key.
- **In NBS**: limits per volume (IOPS, bandwidth) for tenants that must not
  starve one another; a volume's home replica moved after its node, so that
  reads stay local (a locality move by the controller).

What NBS does not give, and should not: ReadWriteMany, one filesystem
written from many nodes. That is a file service on top -- an NFS server on
a ReadWriteOnce volume, as Longhorn does it.

And with NBS under it, the Kubernetes cluster runs no storage system of its
own. Rook-Ceph or Longhorn on disks NBS already keeps three times over would
be nine copies.

Taken into the first version now, since each costs little there and much
after:
- creation by a chosen name, and changes that can be called again with the
  same result ("Files, commands, ports");
- the disk's serial as the volume's id ("The client, in hv");
- read-only attaches by many at once (rule 4).
