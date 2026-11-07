# NBS: its metadata service and its chunk servers

NBS is the replicated block storage for VM disks, images and snapshots
planned in [plans/nbs.md](../plans/nbs.md). As built (steps N5 to N7), it is
two modules on every host:

- `nbs-mds.ko` (`src/crates/modules/storage/nbs-mds`), its metadata
  service: the state every host of an NBS cluster holds alike through Raft --
  - the NBS cluster's identity and its members;
  - each host's chunk server and its devices;
  - the volumes, and on which three hosts each of their chunks is;
- `nbs-cs.ko` (`src/crates/modules/storage/nbs-cs`), its chunk server (cs):
  the host's devices, formatted as NBS's own, and the chunks on them --
  written, read, fenced and freed as the metadata says ("The chunk server",
  below).

The client that gives a VM its disk from them comes with N8; until then
`nbs io` writes and reads a volume from the shell.

The state is `nbsmeta::State` (`src/crates/common/storage/nbsmeta`, no kernel
in it), the state machine of the raft crate's `rsm` ([raft.md](raft.md)). The
member is the one the cluster registry runs too, `raftmember`
(`src/crates/modules/cluster/raftmember`), with NBS's hooks. Its messages go over
the message channel between hosts, ipcnet ([ipc.md](ipc.md)), to `nbs.mds` on
every member.

## Using it

The hosts first trust each other: ipcnet and the cluster registry
([cluster.md](cluster.md)), which pins every host's key on every other. Then
on each host:

```
$ insmod /lib/modules/nbs-mds.ko           # dir=/nbs/mds unless dir= names another
$ insmod /lib/modules/nbs-cs.ko            # after it: the cs asks this host's mds what it is to take in
```

`compact=<bytes>` is how much log the member keeps past its snapshot before
it takes a new one. It is the state's size if that is more, and raft's
1 MiB unless given ("The snapshot", below).

On the first host:

```
a$ nbs init store
nbs: NBS cluster store made (53f0...), a its first member (1730...) -- `nbs host add` the next
```

On any member, a host of the registry made a member:

```
a$ nbs host add b
nbs: b is member 9512... -- a learner until it holds the log, then a voter while fewer than five vote
```

And then:

```
nbs                        this host's member: incarnation, role, term, leader, what it applied, members and voters
nbs hosts                  every member: its incarnation, voter or learner, the leader, a member that lost its state
nbs host remove <host>     <host>'s member out, for good: its incarnation never taken again
nbs host replace <host>    <host>, which lost its state, back as a new member
```

What is stored, on any member:

```
nbs devices add <id|name> [wipe]   a device of this host's for its chunk server, by its id or its name
nbs devices drain <id>     the device takes no new chunks
nbs cs                     every host's chunk server: its devices, its room, what of it is reserved
nbs volume create <name> <size> [host=<host>]   size in MiB, or with M, G or T after it; its own host this one
nbs volume resize <name> <size>                 grown, never made smaller
nbs volume delete <name>
nbs volumes                every volume
nbs volume <name>          one, and the hosts its replicas are on
```

For example:

```
a$ nbs devices add part:4f7661ab-0525-4f56-9d6b-9283d322e47c
nbs: device part:4f7661ab-... of this host's chunk server, 292 MiB -- device 3 in the metadata; its cs takes it in
a$ nbs volume create disk1 64
nbs: volume disk1 (1), 64 MiB
a$ nbs volume disk1
nbs: volume disk1 (1), 64 MiB, 16 chunks of 4 MiB, its own host a, detached
  replicas, chunks a host: a 16, b 16, c 16
```

`nbs devices add` finds the device on this host by its id or by its name. It
records the device's id, never its name: names change between boots, ids do
not. Refused:
- a device with no id;
- one that something holds at the time: a mounted filesystem, the disk log, a
  guest of hv's;
- one with something on it that is not this NBS cluster's -- anything at all
  in its first two pages but zeros or a cs's superblock of this cluster: a
  filesystem, a partition table, a boot sector, another cluster's cs. `wipe`
  takes it all the same: its first 8 KiB are zeroed once the metadata has
  it, and its cs formats it.

A device added again as it is -- the one its cs holds -- is answered with its
number, nothing read or written.

The same is on the management bus as the `nbs` provider (`provider.rs`,
`nbs.descriptor.json`), which the panel shows:
- to read: `status`, `hosts`, `cs`, `volumes`, `volume`;
- to change: `volume_create`, `volume_delete`, `volume_resize`.

Reads are this member's view, at the entry it has applied. Every change is a
command through the leader, answered once this member has applied it too:
what is read next on the same host holds it.

## What it keeps

- **The NBS cluster**: its id, 128 random bits made at `nbs init`, and a
  name. Every message of the member's carries the id before its bytes, and a
  member takes none of another cluster's: dropped, counted, said in the log
  once in ten seconds at most.
- **Its members**, in the order they joined: a host's ipc name and the
  incarnation its mds made when it was offered the cluster -- 64 random bits,
  never 0, the member's raft node. The first five to join vote; the rest are
  learners, which hold everything and do not vote.
- **The members removed**: an incarnation removed is never taken again,
  whatever an old copy of the list says.
- **Each host's chunk server (cs)**, kept by the host's name. A member
  replaced keeps its host's cs, and a member removed leaves its cs taking no
  new chunks. Its devices:
  - each by its id -- a GPT partition's GUID, an MBR partition's signature
    and number, an NVMe's serial;
  - with its size in 4 MiB slots, after what the cs keeps of its own at the
    device's start: a slot for its superblock, then its table of slots,
    twice;
  - active, or draining: taking no new chunks. What a drained device holds
    is to move with N9.

  A cs's room is the slots of its active devices. Reserved of it is a slot
  for every chunk on it, active or draining.
- **The volumes**:
  - a number never taken again;
  - a name -- lowercase letters, digits, `.`, `_` and `-`, up to 63;
  - a size in MiB, up to 16 TiB;
  - its own host: where a replica of every chunk goes, and where its VM is
    to run;
  - its attach: a writer and its generation, or readers;
  - its map: a chunk for every 4 MiB.
- **The replica sets**: three cs's on three hosts, shared by every chunk
  placed on the same three, so that a host lost changes the few sets it is
  in and not every chunk it holds.
- **The chunks**: the set each is in, how many maps name it, and whether it
  was written. A chunk freed goes onto a free list and is taken again
  first.

On the host, in `/nbs/mds`:

- `member` -- which member of which NBS cluster this host is: its
  incarnation, the cluster's id, its own name and the cluster's on the first
  line; for a host that took an offer, a line for each member it was offered
  by, which it knows the members by until its log has caught up. Written once
  and never written over: the mark that this host is a member.
- `raft/` -- the member's log (`raft::log::DiskLog`): its segments, its
  hard state, and its newest snapshot, `snap.<index>`.

## Where replicas go

A volume's chunks are placed in the apply of the command that makes or grows
it, from the committed state alone, so every member places them alike
(plans/nbs.md, "Where replicas go"; `nbsmeta`'s `place.rs`):

1. three replicas on three hosts;
2. only on the cs's of members, on their active devices, where there is
   room;
3. one on the volume's own host, where it has room;
4. the other two on the hosts with the most room left, ties broken by a
   hash of the volume, the chunk and the cs;
5. room reserved at create: a volume takes its whole size, written or not,
   and one that does not fit takes nothing;
6. with four hosts or more, a volume refused where it would leave the
   others less free room than the biggest host's replicas take, so that a
   host lost can be rebuilt on the rest.

A volume that cannot be placed is refused, and why:
- fewer than three hosts take new chunks at all: `fewer than three hosts take
  new chunks`;
- three or more do, but are too full: `not room for its chunks on three
  hosts`;
- rule 6: `it would leave too little room to rebuild the biggest host`.

## The digest

A member that applies an entry differently from the others is a fork that
nothing else would notice. So:
- every 64 entries, each member takes the SHA-256 of its snapshot's bytes:
  the state's tables, in a fixed order;
- the leader proposes its own, as an entry like any other, and every member
  holds its own to it;
- a member whose own differs stops, and says at which entry and what the two
  digests were.

`nbs` shows the newest:

```
nbs: the state's digest at entry 64: 694d3a0bd0f289ec... -- 1 of the leader's held to it, 0 too old to
```

A digest is the member's own, never in a snapshot. A member started again
from a snapshot has none until the next 64th entry.

## The snapshot

- **When**: once the log past the last snapshot is as large as the state,
  1 MiB at least -- raft's floor, which `compact=` changes.
- **Written as a stream.** The tables are kept in pieces of 1024 records,
  each shared copy-on-write. A snapshot takes the pieces' handles at its
  index and writes them out a piece at a time; the apply goes on meanwhile.
  The state is never in one buffer, which the kernel's heap would refuse
  past 512 KiB.
- **Read back as a stream too**, and held to every rule applying keeps:
  every count, the free list walked, each cs's reserved room. A snapshot
  that breaks one is refused.
- **Sent whole** to a member added after the log it needs was compacted, as
  raft sends any snapshot.

## How a host becomes a member

The registry says which hosts may talk at all. ipcnet delivers `nbs.mds` --
a `Cluster` name -- only between hosts it has pinned, so a host the registry
has is all `nbs host add` needs:

1. The member the command is typed at offers the cluster to `nbs.mds@<host>`.
   The offer carries the cluster's id and name and its members.
2. On that host the mds, in no cluster, takes it. It makes an incarnation no
   member of the offer has, clears its log folder, writes its member file and
   answers with the incarnation. Then it starts its member over the empty
   log.
3. The command commits `Add { host, incarnation }`. The leader puts the
   incarnation into raft's membership as a learner, raft brings it the log,
   and once it holds the log the leader makes it a voter, if fewer than five
   vote. Until its log has the members, the new member checks senders by the
   members the offer named.

An add asked again is harmless. A host that is a member already answers the
offer with the incarnation it is, and `Add` of the same incarnation is done
again. A host that is a member of another NBS cluster declines.

## A host that lost its state

A host whose `/nbs/mds` was lost -- a disk replaced, an install wiped --
comes back with an mds in no cluster, and must never be counted as the member
it was: Raft would count the vote of a member that forgot what it voted for.
So:

- Its mds, in no cluster, answers every raft message for a member it does not
  have with "no such member" (at most every 5 s a host). The leader takes
  that from the host the member is on alone. It says so in its log once a
  minute and in `nbs` until the member is replaced: `a says its mds has no
  member 1730...: its state is gone -- nbs host replace a takes it back as a
  new member`.
- `nbs host add` of it is declined by the host itself, which sees in the
  offer that the cluster has it as a member still.
- `nbs host replace <host>` removes the old member -- for good, its
  incarnation never taken again -- and offers the host the cluster as a new
  one.

A member removed that did not lose its state is still the incarnation it
was. Added again, it is refused (`that incarnation was removed: never taken
again`). To take it back, `rmmod nbs-mds` there, `rm /nbs/mds`, `insmod`,
and `nbs host add` it.

## How a member runs

raftmember's, as the registry's member runs ([cluster.md](cluster.md), "How a
member runs"):
- one task, `nbs/mds`, over the log, ticking every 100 ms;
- a message taken only from the host its node is on;
- commands handed to the leader, and asked again for up to 30 s while a
  leader is being chosen; answered once this host's member has applied them
  too, so that what is read next on the host holds the change;
- raft's membership kept to the state's voters and learners, a change at a
  time, a removed leader handing the lead on first.

What is NBS's own:
- the cluster's id on every message;
- the offer;
- the listener that holds `nbs.mds` while this host is in no cluster, taking
  an offer and answering raft messages for members it does not have;
- the lost members kept for `nbs` to say.

`nbs init` starts the member over a new log with this host its one voter,
commits `Init`, and only then writes `member`. Should any of it fail, the
member is stopped, its log and `member` removed, and the listener started
again.

## The chunk server

`nbs-cs.ko` is a host's cs: one task, `nbs/cs`, that holds the host's devices
and serves `nbs.cs`, a `Cluster` name, to NBS's clients on any host of the
registry. What it does to its devices is `nbscs`
(`src/crates/common/storage/nbscs`, no kernel in it), which `fuzz/nbs` runs
over disks that lose power. What a client says to a cs, and a cs to its mds,
is `nbswire` (`src/crates/common/storage/nbswire`).

```
nbscs              the cs: what it is doing, its devices, the requests it served
nbscs chunks       the chunks it holds: each one's state, volume and generation
nbscs fences       the volumes it fences, each at its generation
```

For example:

```
a$ nbscs
nbscs: cs 0 -- serving: 2 chunks held, 2 open, 1 volumes fenced, 9 requests served, 0 refused
  device 3    part:4f7661ab-0525-4f56-9d6b-9283d322e47c 292 MiB, 284 MiB free, 0 slots lost, active
```

What `nbscs` shows is what the cs's task last made of itself: at the end of
a turn that fenced, closed, freed or took a device in, and once a second
otherwise -- the shell never waits on the cs's devices.

### Its devices

Every 5 s the cs asks this host's mds which cs it is and which devices it
has. The mds answers from its applied state; no other host's is asked. The
cs takes each device in:
- held with a queue of its own (`DiskQueue`) and claimed as `nbs-cs`, so
  that nothing else writes it while the cs has it;
- opened where it is this cs's: its records and fences read back, and a torn
  copy of either written again from the other;
- formatted where it is blank;
- left as it is where it is anything else -- NBS's of another cluster, cs or
  device number (a disk moved here), not NBS's at all, or another size than
  the metadata's. Said in the log once and in `nbscs`, and tried again every
  5 s.

It serves only once every device the metadata gives it is taken in. With one
missing, a chunk on that device would read as nothing, and its first write
would put it in a second slot, which the device coming back would contend
with. Until then every request is refused `NotReady`, and the client asks
again.

### A cs's device

No filesystem: a format of NBS's own (`nbscs`'s `format.rs`).

```
slot 0     page 0, 1    the superblock, twice: the cluster, cs and device it is, its size, its format's id
           page 2..34   the volumes' fences, copy A
           page 34..66  the volumes' fences, copy B
slot 1..   the table of slots, copy A, then copy B: a record a slot
then       the slots, 4 MiB each: the chunks' data
```

- Every record, a fence's or a slot's, has a sequence number and a CRC32C
  seeded with the format's id -- 64 random bits made at the format -- so that
  a record of an earlier format, or of another device, never passes.
- A record is changed in copy A, which is flushed, then in copy B, flushed
  too. One copy is ever being written, so a power cut leaves the other good.
  Of two good copies the newer is the record; zeros in both, it was never
  written.
- A record neither of whose copies is good is lost: its slot is never used
  again, and that is said.
- A device is blank where its first two pages are zeros. Anything past a
  superblock's 128 bytes in them is not a cs's, since a format never writes
  there: a filesystem whose first bytes are zeros is never taken for blank.

### What a request leans on

- **A fence is durable before it is answered**, on every device of the cs. A
  volume's fence is the newest attach generation the cs has been told. A
  request of an older one is refused (`fenced`), after a crash too.
- **An open mark is durable before the first write of a generation** to a
  chunk. After a crash, every chunk whose replicas may differ says it is
  open, at the generation that wrote it last: what N8's reconcile goes by.
- **A chunk is held by its number and its incarnation.** The metadata gives a
  freed chunk's number to the next chunk made, at the next incarnation. A
  slot of the chunk a number was is never read or written for the one it is
  now, and is freed at the new one's first write; a request of the older
  incarnation is refused.
- **A slot is zeros before a chunk takes it**, the zeros flushed before the
  record that gives it the slot: what was freed is never read through another
  chunk.
- **A close is durable only after the chunk's data is**: it flushes first.
- **Nothing of one volume's is read or written through another's.**

### Checked with the host's mds

A request names the volume, the attach's generation and the chunk: its
number, its incarnation, its epoch -- its replica set and the set's version
-- its place in the volume's map, and the entry of the metadata the client
read the map at. What its engine has checked already, the cs does. A chunk
it does not hold yet, or at an epoch other than the one it knows, it asks its
own host's mds about first -- the mds off the data path, and a cs that trusts
no other host's (decision 3 of plans/nbs.md). The mds answers from its applied
state once it has applied as far as the request's entry, `Behind` until then,
which the client asks again. It vouches that:
- the chunk is in that place of that volume's map, at that incarnation;
- this host's cs is in the chunk's set, at the set's version;
- a write is the volume's writer's, at its attach's generation; a read of
  generation 0 is a read-only one's.

What the mds vouched for is kept, up to 65536 of them.

### What is freed

Every 30 s the cs asks its mds which of the chunks it holds are gone -- freed,
or their number another chunk's now -- and gives their slots back, and which
of the volumes it fences are gone, and takes their fences off.

### `nbs io`

nbs-mds's `nbs io` (`io.rs`) is a client of the cs's, for the shell and the
tests until N8's:

```
nbs io attach <volume> [force]                  attached for writing here, a new generation; force takes it from another host
nbs io write <volume> <offset> <bytes> <byte>   <bytes> of <byte> at <offset>, to every replica
nbs io read <volume> <offset> <bytes>           from one replica: this host's where it has one
nbs io flush <volume>                           what was written durable on every replica
nbs io close <volume>                           every chunk written here closed on every replica
nbs io held <volume>                            the volume's chunks each cs holds open
nbs io detach <volume>
```

The first write attaches the volume for writing on this host, unless
`attach` did, and fences each cs at the generation before its first write
there. A read on a host that has not attached the volume is a read-only one,
of generation 0. Offsets and sizes are whole 4 KiB. For example:

```
a$ nbs io write disk1 0 65536 0x5a
nbs: 65536 bytes of 0x5a written at 0 of disk1, generation 1: 1 chunks, each to its 3 replicas
b$ nbs io read disk1 0 65536
nbs: 65536 bytes at 0 of disk1, from b: 0x5ax65536
```

A client's requests and their answers go between hosts by ipcnet's bulk
connection, all of them, so that they keep their order and their data do not
hold up raft's messages. A request carries 256 KiB at most. It is asked again
for up to 30 s while the answer is that it cannot be done yet; one to a host
with no cs -- nbs-cs not loaded there -- comes back from the channel as a
bounce, and is said at once.

## Tested

- **`scripts/nbs-fuzz.py`** (host, in CI; [testing.md](testing.md)): `fuzz/nbs`'s
  `meta` target, the state against a model of its own:
  - every command's answer and everything the state shows held to the
    model's, the placement to the model's own;
  - an fsck of its own after every step;
  - the snapshot read back in pieces, spoiled and refused, and continued
    from;
  - two members' digests compared, and a divergence said.

  Its `cs` target is the chunk server's engine over disks with a write
  cache, through histories of fences, writes, reads, flushes, closes and
  frees, and power cuts: each write not yet flushed kept, lost or torn at a
  sector's edge, and now and then a record's sector torn inside or a bit
  rotted. After every cut the cs is opened again and held to every fence and
  open mark answered, every chunk closed reading what it held, every sector
  one of the values it may have, no record lost; before each run, a disk with
  something else on it never taken for blank, and another cs's told apart.
  Its `wire` target holds nbswire's messages to their encoding, and reads
  noise as none.

  `--mutants` takes thirty-five rules out one at a time -- eighteen of the
  metadata's, fifteen of the cs's, two of the wire's -- each caught.
- **`fuzz/raft`'s `nbsmeta` target**: the members' part of the state, its
  commands from pools of names and incarnations that meet and from wide ones
  that fill the cluster. There are four scenarios and six mutants.
- **`scripts/nbs-test.py`** (three nos on `scripts/hub.py`'s LAN, after the
  cluster registry is made among them). It covers:
  - an NBS cluster made and two hosts added, one through a member that does
    not lead;
  - an add asked again answered with the same incarnation, and a second init
    refused;
  - a commit through the majority;
  - the leader killed: the other two elect one of themselves and commit, and
    the killed host comes back from its log;
  - the leader cut off by the hub: the other two elect and commit while it
    commits nothing, then it follows once healed;
  - a host's folder removed: the leader says its state is gone, an add of it
    is refused, and `replace` takes it back as a new member;
  - a member removed, and its old incarnation refused when added again;
  - what is stored, on a pool disk of three partitions a host (N6):
    - no volume before three hosts have a device;
    - devices added by id and by name, and the pool's disk itself refused
      for having no id;
    - c's copy of a's partition refused, as a disk moved between servers;
    - volumes made, made again, refused for another size, grown, refused
      smaller, refused past the room;
    - the room filled to its last slot, then given back by a delete;
    - a device drained;
    - the provider answering through the bus as the command does;
    - every host's digest at the same entry and the same;
    - every log compacted into a snapshot;
    - the killed host back from its snapshot and log, the replaced one sent
      the snapshot, and a removed host's cs taking no new chunks;
  - the chunk servers (N7):
    - every host's cs taking its devices in, formatting them;
    - a device with a filesystem's magic on it refused, and taken with
      `wipe`;
    - a volume written to its three replicas and read back, across a
      chunk's end, from the writer's host and read-only from the others;
      flushed, its chunks open on every cs, then closed;
    - a writer on another host refused, then forced: its fences refuse the
      old writer's writes and reads;
    - a host booted again, its cs back with its data, its fence and its open
      marks;
    - the volume deleted, and every cs giving its chunks and its fence back;
    - a host's nbs-cs unloaded: a request to it said at once, the others
      answering; loaded again, serving its devices;
  - nothing panicked.

## Limits

- One NBS cluster a host. At most 16 members, five voters.
- An offer is trusted from any host of the registry: the registry is what
  says who is trusted.
- A host that took an offer knows the members by the offer until its log has
  caught up. A leader elected among members added after the offer is not
  known to it until then.
- Membership changes are one server at a time, without joint consensus.
- A removed member may not hear of its removal, and goes on asking for
  votes, which PreVote keeps from disturbing the others. `nbs` on it says it
  is removed once it applied the entry.
- `nbs host remove` refuses only the last member. It is NBS's data, not its
  metadata, that needs three hosts.
- Only `nbs io` attaches a volume, for the shell. Its reads at generation 0
  are not readers in the metadata, and nothing reconciles the replicas of a
  chunk left open: N8's client attaches, reconciles and flushes.
- A cs serves one request at a time, each waited for on its device, and
  copies the data into a message and out of one. Measuring, and pages that
  move instead, are N11's.
- A cs serves only with every device the metadata gives it. A device that
  fails while it serves fails the requests of its chunks; one that is never
  taken in stops the cs serving. Taking a device out, and rebuilding what was
  on it, is N9's.
- A chunk's data has no checksum of the cs's: a sector is taken to be
  written whole, as NVMe's atomic write unit promises. A scrub comparing the
  replicas is N9's.
- A device drained stays draining, and neither a device nor a cs is ever
  removed. A placement never moves: moving chunks is N9's.
- Bounds, each refused past:
  - 4096 volumes;
  - 2^22 chunks, 16 TiB of volumes in all;
  - 64 cs's of 16 devices each;
  - a volume's readers: one a member, 16 at most.

  The state is in memory whole: some 3 MiB for every TiB of volumes.
