# NBS's metadata service

`nbs-mds.ko` (`src/crates/modules/storage/nbs-mds`) is the first piece of NBS,
the replicated block storage for VM disks, images and snapshots planned in
[plans/nbs.md](../plans/nbs.md): its metadata service, the state every host of
an NBS cluster holds alike through Raft. As built (steps N5 and N6), that
state is:
- the NBS cluster's identity and its members;
- each host's chunk server and its devices;
- the volumes, and on which three hosts each of their chunks is.

It is recorded and not yet written: the chunk servers that store the chunks
come with N7, the client with N8.

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
nbs devices add <id|name>  a device of this host's for its chunk server, by its id or its name
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
nbs: device part:4f7661ab-... of this host's chunk server, 292 MiB -- device 3 in the metadata; nothing is written to it before the cs (N7) takes it
a$ nbs volume create disk1 64
nbs: volume disk1 (1), 64 MiB
a$ nbs volume disk1
nbs: volume disk1 (1), 64 MiB, 16 chunks of 4 MiB, its own host a, detached
  replicas, chunks a host: a 16, b 16, c 16
```

`nbs devices add` finds the device on this host by its id or by its name. It
records the device's id, never its name: names change between boots, ids do
not. A device with no id is refused, as is one that something holds at the
time: a mounted filesystem, the disk log, a guest of hv's.

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

## Tested

- **`scripts/nbs-fuzz.py`** (host, in CI; [testing.md](testing.md)): `fuzz/nbs`'s
  `meta` target, the state against a model of its own:
  - every command's answer and everything the state shows held to the
    model's, the placement to the model's own;
  - an fsck of its own after every step;
  - the snapshot read back in pieces, spoiled and refused, and continued
    from;
  - two members' digests compared, and a divergence said.

  `--mutants` takes fifteen rules out one at a time, each caught.
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
  metadata, that will need three hosts (N7).
- What is stored is recorded, not written. N7's cs claims and formats the
  devices: until then, `nbs devices add` refuses only a device that
  something holds when it is asked.
- No command attaches a volume yet. The attach and the detach are in the
  state for the client (N8).
- A device drained stays draining, and neither a device nor a cs is ever
  removed. A placement never moves: moving chunks is N9's.
- Bounds, each refused past:
  - 4096 volumes;
  - 2^22 chunks, 16 TiB of volumes in all;
  - 64 cs's of 16 devices each;
  - a volume's readers: one a member, 16 at most.

  The state is in memory whole: some 3 MiB for every TiB of volumes.
