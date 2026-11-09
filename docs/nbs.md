# NBS: its metadata service, its chunk servers and its client

NBS is the replicated block storage for VM disks, images and snapshots
planned in [plans/nbs.md](../plans/nbs.md). As built (steps N5 to N8), it is
two modules on every host, and a client:

- `nbs-mds.ko` (`src/crates/modules/storage/nbs-mds`), its metadata
  service: the state every host of an NBS cluster holds alike through Raft --
  - the NBS cluster's identity and its members;
  - each host's chunk server and its devices;
  - the volumes, and on which three hosts each of their chunks is;
- `nbs-cs.ko` (`src/crates/modules/storage/nbs-cs`), its chunk server (cs):
  the host's devices, formatted as NBS's own, and the chunks on them --
  written, read, fenced and freed as the metadata says ("The chunk server",
  below).

- the client (`src/crates/common/storage/nbsclient`, no kernel in it): a
  volume's session -- attached, its cs's fenced, its open chunks reconciled,
  read and written through the cs's, flushed, closed, detached -- driven over
  ipc by `nbsdrive` (`src/crates/modules/storage/nbsdrive`) in the modules
  that use a volume: hv.ko's `nbs:` disks, `nbs io`, nbsload.ko ("The
  client", below).

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
1 MiB unless given ("The snapshot", below). `out_after=<seconds>` is how
long a cs is silent before the leader's controller has it out and its
places taken by others: ten minutes unless given ([Repair](#repair)).

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
nbs replica bad <set> <host>   <host>'s cs taken down in <set>, to join its place again and copy it anew --
                               the member the scrub found unlike the two others
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
  for every chunk on it, active or draining, and for every chunk of a set it
  is joining in another's place (N9). A cs is up, down or out as the leader
  has committed it ([Repair](#repair)): only an up one takes new chunks.
- **The volumes**:
  - a number never taken again;
  - a name -- lowercase letters, digits, `.`, `_` and `-`, up to 63;
  - a size in MiB, up to 16 TiB;
  - its own host: where a replica of every chunk goes, and where its VM is
    to run;
  - its attach: a writer and its generation, or readers;
  - its map: a chunk for every 4 MiB;
  - its kind (N10): a volume, an image -- frozen read-only, to be cloned --
    or a snapshot, a volume's map at a moment; and what it was made of: the
    volume a snapshot was taken of, the image or snapshot a clone was made
    of.
- **The replica sets**: three cs's on three hosts, shared by every chunk
  placed on the same three, so that a host lost changes the few sets it is
  in and not every chunk it holds. Each member is active or down -- a down
  one with the last version it was active at -- and a set has at most one
  joiner, a cs being copied onto ([Repair](#repair)). New chunks go only
  into a clean set, every member active and no joiner; an empty set is made
  again for three new cs's before a new one is made -- never the clean set
  of three cs's the same placement fills, so that the sets a placement takes
  are counted before it takes any.
- **The chunks**: the set each is in, how many maps name it, and whether it
  was written. A chunk freed goes onto a free list and is taken again
  first.
- **The copy-ups pending** (N10): by a volume and a place in its map, the
  new chunk a write to a shared chunk is to go to, and the old one the map
  still names.

### Snapshots, images and clones (N10)

A snapshot or a clone shares the chunks of what it was made of: its map a
copy of the other's, every chunk counted once more, and no room reserved --
thin (decision 5). The metadata's commands, as the shell and the client are
to drive them (N10b-N10d):

- `Snapshot { volume, name, generation }` -- of a volume only, at the
  generation of the writer that paused for it, or 0 for one with none: a
  writer not paused could have a write on its way into a chunk the snapshot
  shares from then on.
- `Freeze { name }` -- a volume made an image, refused while attached; its
  copy-ups pending given up.
- `Clone { from, name, host }` -- of an image or a snapshot only (else
  `NotFrozen`).
- `CopyUp { name, place, generation }` -- the writer's, before it writes a
  chunk others share: a new chunk in the same set, so that each of its cs's
  copies it locally; its room taken on each, refused as `NoRoom` where one
  has none left -- the room kept to rebuild a host may go to it. Pending, the
  old chunk named by the map still, so that nothing can free it while the
  cs's copy it (`Op::CopyUp`, below). Asked again, it is answered the
  pending chunk; of a chunk shared no longer, the chunk itself. Refused as
  `Joining` in a set being joined (N10b): a joiner lists what its source
  holds once, and a new chunk behind its listing would never reach it -- so
  a join gives up its set's copy-ups pending as it begins, their writers ask
  again once it is over, and what the cs's copied for them is freed as any
  chunk gone. A join still needs room for every chunk of the set, those
  among it: the controller plans none with less. Refused as `Draining` in a
  set with a member drained (N10c): its devices take no new chunk, and its
  place is to be joined by another cs first. A copy-up pending of a chunk
  named by one map now -- a snapshot dropped meanwhile -- is given up (N10c):
  one left would hand its new chunk, copied as the old one was then, to a
  copy-up asked after the place was written directly.
- `CopyDone { name, place, generation, chunk }` -- the map names the new
  chunk, the old one counted down, freed at none.

A volume deleted, or an image, counts its chunks down: those a snapshot or a
clone still names live on. A chunk's count is a u16; a map names a chunk
once at most, and there are fewer volumes than it holds.

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
2. only on the cs's of members that are up, on their active devices, where
   there is room;
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

## Repair

How three are kept (plans/nbs.md, "How three are kept"). What N9a has of it
is the metadata and the controller; the copies are N9b's, the leader's turns
N9c's.

- **A cs's state.** `CsDown` -- silent 5 s to the leader -- has it down in
  every set it is an active member of, the last active member of one apart,
  and drops every join it is the joiner of: writes go on to the others. Its
  version at the time is kept with each place, as the last it was active
  at. `CsOut` -- silent past `out_after`, 10 minutes held off by the
  registry's maintenance flag, or its host removed -- is down for good: its
  places are to be taken by others. `CsUp` is a cs that answers again; its
  places in its sets stay down until it joins them again.
- **A join** (`Join`) puts a cs into a set as its joiner, to take one place
  once its copy is done: its own place, the member down and back, which
  copies only what it missed; or another's -- a member down and out, or one
  whose cs is drained, which stays active meanwhile -- with room for every
  chunk of the set. It names the set's version it was planned at, and is
  refused once the set has moved on. `Joined` makes the joiner the member in
  that place, active, the room of the one it replaces given back; `Unjoin`
  gives the join up. Each raises the set's version: a chunk's epoch moves on
  with it, so the writes of the epoch before are refused at the cs's that
  know the new one (rule 3 of plans/nbs.md's "Replication: the rules").
- **The joiner is written, not read**: a client writes it with every member
  active, and reads only from active members; the host's mds vouches it for
  writes alone. Silent to the writer, it is asked out as a member is
  (`ReplicaDown`), after the same look by the leader, and its join dropped.
- **The controller** (`nbsmeta::repair::plan`) is a pure function of the
  committed state and what the leader sees of each cs -- answering, for how
  long not, in maintenance -- the same in the member as in the fuzzer. Its
  turn proposes the cs's to go down, up or out; where none is to, joins:
  the sets with one active member first; a member down whose cs is up again
  joins its own place; one gone, or drained, has another join in its place
  -- the most room first, ties by a hash of the set and the cs; two joins a
  joiner and sixteen a turn at most. A set is rebuilt whole.
- **Seen**: `nbsmeta::repair::health` counts the chunks by how many of
  their set's members are active, and those being copied onto a joiner;
  `nbs status` says them, with the joins under way and the cs's down and
  out.
- **In nbs-mds** (N9c) the leader looks at every cs once a second
  (`Op::Ping`) and plans every ten seconds -- sooner, a second after its
  last plan at the soonest, where the state moved on (a cs changed, a join
  done or dropped) or a cs went silent or came back -- proposing what the
  plan gives once what it proposed last is applied. After a proposal of its
  is refused it keeps to the ten seconds, saying each refusal. A cs's silence counts from when this leader began to watch it: a
  new leader takes none for down at once. The registry's maintenance flag
  is not read yet.

### The copy

A joiner's cs copies the set (N9b): nbswire's `copy`, driven by nbs-cs's
task, whose mds tells it its joins (`Question::Joins`) with its devices.

- **From a source**, an active member: the set's chunks it holds a page at
  a time (`Op::Holdings`), each pulled (`Op::Pull`) and summed
  (`Op::Sums`). Each names the joiner and the set's version, which the
  source's mds vouches for (`Question::VerifyJoin`). A pull and a sum move
  the chunk's epoch on at the source, durable before they read: a write of
  the epoch before is refused there from then on, and its writer, its map
  read again, writes the joiner too. An epoch older than the one a chunk is
  known at is refused even where the cs's cache once vouched for it.
- **A chunk found open is no source**: restarted with it open at a writer's
  generation, the cs may have lost what that writer wrote since its last
  flush, which a joiner would take as the set's. Its pulls and sums are
  refused as failed -- the copy takes another source, or waits for the
  writer to take this one out, or for a writer's reconcile to make the
  chunk the same, which ends the mark -- where the cs is its set's last
  active member, its writer's next request there fails as the last replica
  refused (`LOST_LAST`), and the next writer's start reconciles it. Refused,
  they keep the mark too: a pull's moving the epoch on writes the chunk's
  record, which would take it off. (Both found by fuzz/nbs's cluster
  target, N9d: a joiner copied from such a replica, and a pull from one had
  taken the mark off.)
- **The marks**: what clients write to the set from the join on is marked,
  a bit a sector, in memory alone, from the first write the mds vouches for
  as the joiner's (`Told::VerifiedJoining`). The copy writes only where no
  sector is marked: the client's data is newer, and the source has it or
  will. A joiner takes no write of an epoch before its join: one from a view
  that old -- a writer taken over since among them, which the members fenced
  against it refuse -- would be marked, and kept over the set's (found by
  the cluster target, N9d). Its writer reads its map again.
- **A member back** pulls whole what it may have missed -- a chunk last
  written at the version it went down at or after, or one it does not hold
  -- and holds the rest to the source's by sums, copying only what differs:
  one it found open when its devices were taken in among them, a sum
  finding what a power cut lost as it finds a write missed. It trusts no
  chunk it holds: taken out, it kept each at the epoch it last knew, and a
  writer since taken over, its view as old, could write it there unfenced,
  which the members fenced against that writer refused (found by the
  cluster target, N9d; a touch, which moved the epoch on and kept the data,
  did before).
- **The check**: a chunk pulled is summed against the source's. A granule
  that differs is pulled again and held to the source's a sector at a time,
  the marked sectors left out; a sector that does not read back as copied is
  a round, and past three the join is given up and said. The chunk is then
  closed, its data flushed first -- flushed too where a client's write left
  it open at the writer's generation, which leaves it open: the join's end
  makes this cs a member, and a power cut after must not take the copy's
  sectors (found by the cluster target, N9d). A chunk the joiner holds open
  from before its join -- a member then, taken out with the chunk open, its
  writer closing it on the members it had left -- is nobody's but the
  copy's: the copy closes it at its end, whether it wrote the chunk or found
  it the source's already, and leaves a mark open only where a client has
  written the chunk since the join began (found by the cluster target, N9d:
  kept, it made the chunk found open once the joiner crashed, and as its
  set's last member it was no source for the joiner after it).
- **Its end** is asked of the mds (`Question::JoinDone`, proposed as
  `Joined`); a copy that fails gives the join up (`Question::GiveUp`, as
  `Unjoin`). A join found as nbs-cs starts is given up: its marks are gone,
  and a copy without them could write over a client's write. The
  controller joins it again under a new version.
- **A chunk found open** is written by its set's joiner, as a joiner is
  written and not read, and its copy pulls the chunk; refused to its writer
  as `Failed` only where its cs is an active member.

### The scrub

The leader looks at every chunk in turn, a chunk every `scrub=` seconds (ten
unless nbs-mds is given another, 0 for none), and holds its set's active
members alike (`nbswire::scrub`, the one nbs-mds runs and the cluster target
does):

- **The look**: each active member asked for the chunk's record and the
  CRC-32C of each of its 64 KiB granules (`Op::Scrub`) -- read as a joiner's
  sums are, but moving no epoch on, so that a writer is refused nothing. A
  chunk open on any member is in flight and is left for a later round, as is
  a set that moved on between the asking and the answers.
- **A member that holds none of it** says so, and is compared as what such
  a chunk reads as: zeros (`nbscs::zero_sums`, a slot being zeros before a
  chunk takes it). A member that lost a chunk its set's others hold
  differs from them as a rotted one does. A chunk no member holds was never
  written: nothing is read, and the next chunk is looked at a hundredth of
  the pace on -- the pace is the reads', so that a volume mostly unwritten
  takes a round little longer than what is written of it, its empty chunks
  asked about no faster than that.
- **Two looks**: two members' answers are not taken at one instant, and a
  write and a close between them would show a difference that is none. A
  difference is a finding only when a look two seconds on finds every
  member's answer as the first did.
- **What it finds it says**, in the log and in `nbs status` -- the chunk,
  its set and version, and the member unlike the two others where two
  agree, with the command that takes it down; a finding of a set that has
  moved on since is shown as history -- once for the set's version, and
  takes nothing down. A cs keeps no
  checksum of its own data, and two members alike are no proof which one
  holds it: a joiner that copied a source that had rotted makes two alike in
  rot, and a scrub that took the odd one down would take the one good copy
  down, its join back copying the rot over it (found by the cluster target,
  N9e). A person decides: `nbs replica bad <set> <host>` takes that member
  down (`ReplicaBad`, at the set's version now; refused for a member down
  already, a set with a joiner and the set's last active member), and the
  controller has it join its own place again, a member back whose copy
  holds every chunk to a source's by sums and pulls what differs.

`nbs status` on the leader counts what its scrub has done since it began
to lead -- chunks found alike, held by none, let be (open, moved on, an
answer late or refused, written between two looks) and unlike, those with
a member unlike the two others among them -- and lists its sixteen newest
findings.

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
- **Another volume's chunk is read or written only once the host's mds
  vouches** that this volume's map names it (N10b): a snapshot's or a
  clone's map shares the chunks of what it was made of, and a clone whose
  image is deleted writes the image's chunks it alone names. Written, the
  chunk is the writer's volume's from then on -- what its reconcile asks the
  cs for, and what its flush is held to.
- **A copy-up's chunk is durable before it is answered**, its data before
  its record, and never written over once held (below).
- **A chunk the cs does not hold is read as zeros only once the mds vouches**
  that the volume's map names it (N10c): one freed since -- a copy-up done,
  the old chunk named by no map -- is not a chunk never written, and a read
  of it still on its way from before is refused.

### A copy-up's copy (N10b)

Before a writer writes a chunk others share, it asks the metadata
`CopyUp`, then has each active member of the chunk's set copy it into the
new chunk, locally (`Op::CopyUp { from, to, position, index }`), then asks
`CopyDone` (N10c). On a cs (`Store::copy_up`):
- a new chunk the cs does not hold is asked of its mds first
  (`Question::VerifyCopy`), every time -- once a copy-up;
- the copy is of the whole chunk, 256 KiB at a time, into a slot the cs
  takes off its free list only once the chunk's record is written: a copy
  that fails part way leaves the slot free;
- its data is flushed before its record is written -- closed, the copy-up's
  volume's, written at the set's version now, so that a member back copies
  it as any chunk written while it was out;
- asked again, with the new chunk held, it does nothing: what the writer
  wrote to it since is kept;
- the old chunk held by none here -- never written -- the new one is not
  taken either: both read as zeros;
- refused where the old chunk was found open at the cs's start: it may lack
  what its writer wrote since its last flush, which the copy would keep for
  good. The writer takes the member out, and it copies the new chunk when
  it comes back.

A copy-up holds the cs's task for its copy -- 4 MiB read and written, and
three flushes.

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

What the mds vouched for is kept, up to 65536 of them, and taken only for a
chunk the cs holds (N10c), whose record refuses an epoch older than its own
even so. One the cs does not hold -- never written, or freed since -- is
asked about every time: nothing here refuses its epoch but the mds (found by
fuzz/nbs's cluster target: a read of a chunk never written, vouched for at
a set's version, kept; the set joined; the writer's first write of it, by
its view of before, taken as vouched for, and the joiner never written). A
copy-up's copy it
vouches for (N10b) where the copy-up is pending at that place of that
volume, from that chunk to that one at their incarnations, both at the
set's version now, this host's cs an active member of the set, and the
volume's writer of now asking.

### What is freed

Every 30 s the cs asks its mds which of the chunks it holds are gone -- freed,
or their number another chunk's now -- and gives their slots back, and which
of the volumes it fences are gone, and takes their fences off (nbswire's
`collect`, which fuzz/nbs's cluster target's cs's run too).

A client's requests and their answers go between hosts by ipcnet's bulk
connection, all of them, so that they keep their order and their data do not
hold up raft's messages. A request carries 256 KiB at most. One to a host
with no cs -- nbs-cs not loaded there -- comes back from the channel as a
bounce, and the client takes the cs for one that does not answer.

## The client

A volume is used through a session of it (`nbsclient::Session`): a state
machine that is handed what comes -- its host's mds's answers, the cs's --
and the time, and says what to send and which of its driver's operations
are done. No kernel is in it; `nbsdrive::Drive` drives it over a mailbox of
its own in the kernel, and fuzz/nbs's `cluster` target over a network of
its own.

A session's life:
1. **Attach** at the host's mds (`nbs.mds`, `TAG_ASK`): for writing at a new
   generation -- `force` takes the volume from a writer on another host --
   or read-only, which a writer elsewhere refuses. The attach carries the
   session's token: asked again, its answer late or lost, it is answered with
   the generation it gave, not another. One not answered in 10 s fails the
   session.
2. **The map**, read from the mds at the attach's entry or later: each
   place's chunk and incarnation, its set, the set's cs's and their states,
   each cs's host.
3. **Fences**: every cs a set of the volume writes to fenced at the
   generation, from which on an older writer's requests -- writes, reads,
   closes, flushes -- are refused.
4. **Reconcile**: each chunk any replica holds open -- left so by a writer
   that stopped uncleanly -- read from one replica, this host's if it has
   one, and written to every one, 256 KiB at a time, then closed.
5. **Serve**, until **stop**: what was asked before it done, a flush, every
   chunk written closed, detached.

What it holds to (plans/nbs.md, "Replication: the rules"):
- **A write is done once every replica in has it.** One that has not
  answered in 0.5 s is asked out at the mds (decision 6): the leader looks at
  the cs (`Op::Ping`, 250 ms) and takes it out of its set -- the set's
  version up, and with it every chunk's epoch -- only if the cs does not
  answer it either; otherwise the ask is refused for now and asked again. A
  cs that refuses as failed is taken out as it is, with no look. Only the
  volume's writer of now takes a replica out, and only of a set of its
  volume's: a writer fenced since is refused (`NotTheWriter`), its session
  failed -- else a write of its would be done by taking out the replicas
  that refuse it. The set's last active replica is never taken out: writes
  wait for it.
- **Two writes that overlap are never out at once.** A read goes to one
  active replica, this host's where it has one, and to another when it does
  not answer.
- **Its view of a set is whole, or held as behind.** Each change raises a
  set's version by one, so a take-out's answer one past the view is that
  take-out alone; one further is changes the view lacks -- a join begun --
  and the set is read again, its writes held until then. A map read that
  finds a set changed sends its writes out again by the new view, so that a
  write is done only by every replica the view has in, a joiner among them.
  A read that finds no active replica in the view waits for the map: the
  set's last active member is never taken out. A cs that comes into the view
  while the session's start fences is fenced too, and the start waits for
  it. A session detached takes no answer to an ask of before. An ask to
  take a cs out refused as no replica of its set -- its place another's
  since, a joiner's of the controller's -- has the map read again, which
  drops the ask; what waits for the cs is done as it comes due, by the view
  read -- a request to a cs no set writes as a silent one's, a close the cs
  refuses as not in the chunk's set (`NotHere`) at once. (Each found by the
  cluster target with the controller in it, N9d.)
- **A flush** waits for every write before it, then asks every cs that has
  acknowledged a write since the last flush began, and that some set of the
  volume still writes, to make its writes durable. It is done once they have
  and once every flush before it is, since each takes the cs's dirty when it
  begins; one that fails fails those out after it. A close is a flush, then
  every chunk written closed at once. A cs taken out of every set holds
  nothing the volume counts on: a flush, a close or a fence asked of it is
  done, not waited on -- nbs-test's repair phase found a close waiting for
  good on a cs the controller had taken out (`CsDown`) while it still owed
  a flush.
- **A replica that restarted** with a chunk open at the writer's generation
  may have lost what was written since the last flush, which the others
  have: it refuses that writer's requests of the chunk -- and its flushes --
  as failed, which takes it out. Where it is the chunk's last active replica
  the session fails (`LOST_LAST`): the volume is to be attached again, whose
  reconcile takes what the replica holds.
- A chunk written, flushed and idle for 3 s is closed on its replicas, so
  that a crash leaves few to reconcile. An operation not done in a minute
  fails, said.
- A writer that follows another -- by force, or after a crash -- holds what
  that one had done and not flushed as a crash would leave it: there or
  not, as the replica its reconcile read had it.
- **A write to a chunk others share** -- a snapshot's or a clone's map names
  it too, as the map says (`Place.shared`) -- is held while the chunk is
  copied up (N10c): a new chunk asked of the mds (`CopyUp`), each active
  member of the set, as the view has it, asked to copy the old one into it
  (`Op::CopyUp`), and the mds told (`CopyDone`); the map names the new chunk
  then, and the writes held go to it. A member that refuses as failed, or is
  silent 0.5 s, is asked out as for a write, and copies the new chunk when it
  comes back, as any chunk written while it was out. A member's refusal as
  of another epoch or map, a take-out's answer past the view -- the members
  asked may not be the set's: done without the one asked, a copy-up was done
  by none, the new chunk zeros under the writes after (found by the cluster
  target) -- and a copy-up's end refused, the copy-up given up meanwhile,
  have it asked again from its start once the map is read again; each step
  of it does nothing done already. A chunk shared no longer is answered as
  itself, and written as it is. Refused for now -- the set being joined, or
  a member of it drained -- it is asked again, the write waiting; refused
  for room, its writes fail, said.

### `nbs io`

nbs-mds's `nbs io` (`io.rs`) is a session of the volume's for the shell and
the tests:

```
nbs io attach <volume> [force]                  attached for writing here: a new generation; force takes it from another host
nbs io write <volume> <offset> <bytes> <byte>   <bytes> of <byte> at <offset>, to every replica in
nbs io read <volume> <offset> <bytes>           from one replica: this host's where it has one
nbs io flush <volume>                           what was written durable on every replica in
nbs io close <volume>                           that, and every chunk written closed on its replicas
nbs io held <volume>                            the volume's chunks each cs holds open: asked of each, not of a session
nbs io detach <volume>                          flushed, closed, let go of
```

The first write attaches the volume for writing on this host unless
`attach` did; the session is kept for the commands after, until `detach` or
another `attach`, and moves only while a command drives it. A read on a host
with no session of the volume's attaches read-only for itself and lets go
after -- refused while another host writes. Offsets and sizes are whole
4 KiB. For example:

```
a$ nbs io write disk1 0 65536 0x5a
nbs: 65536 bytes of 0x5a written at 0 of disk1, generation 1: 1 chunks, to every replica in
a$ nbs io read disk1 0 65536
nbs: 65536 bytes at 0 of disk1, from a: 0x5ax65536
```

### nbsload

`nbsload.ko` is blkload's for a volume: a load test through a session,
IOPS, bandwidth and latency down to the percentiles (`common/latency`, the
histogram they share).

```
nbsload <volume> [randread|randwrite|read|write|verify] [bs=4k] [qd=8] [secs=5] [span=<MiB>] [force]
```

The command's own task keeps qd operations out at once; a write test
attaches for writing -- `force` takes the volume -- and destroys what is
on it. `verify` writes blocks each of which says which block and which of
its writes it is, holds every read as it goes to the block's last write
done, and after a flush reads every block it wrote back: one that does not
hold its last write is said, with where.

### A guest's disk: `nbs:<volume>`

hv's guests take a volume as a disk: `disk=nbs:<volume>` on `hv start`, or
in a vm.toml's `disk`. The disk is a virtio block device of 4 KiB blocks,
the volume's size, its serial `nbs-<number>`; its task drives the session --
attached as the guest starts, every cs fenced, the open chunks reconciled --
and the guest stopped lets the volume go: flushed, closed, detached. `hv
start <name> force` takes the VM's volumes from a writer elsewhere: a VM
moved off a host that is gone. No vm.toml says force.

An image is poured onto a volume made beforehand (`nbs volume create`):

```
hv convert <image> nbs:<volume> [force]         a qcow2 image or a raw disk, written through a session, flushed, let go of
hv create <name> <url> onto=nbs:<volume>        a cloud's image downloaded and poured onto it, the VM's first disk
```

A volume never written before reads as zeros, so a pour onto one leaves the
image's zeros out.

## Tested

- **`scripts/nbs-fuzz.py`** (host, in CI; [testing.md](testing.md)): `fuzz/nbs`'s
  `meta` target, the state against a model of its own:
  - every command's answer and everything the state shows held to the
    model's, the placement to the model's own; among the commands, cs's
    going down, up and out, and joins begun, ended and given up, from the
    controller's turns and at random;
  - an fsck of its own after every step;
  - the snapshot read back in pieces, spoiled and refused, and continued
    from: bytes spoiled at random, and a set's joiner made one no set can
    have;
  - two members' digests compared, and a divergence said;
  - the controller's progress (N9a): from the state of a step, some hosts
    gone for good and the rest answering, its turns applied until it has
    nothing to propose -- every proposal answered as the model answers it,
    none refused -- and then no member down whose cs answers, no place
    left that a cs had the room to fill, no join under way, every cs that
    answers up and every one gone out.

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

  Its `cluster` target is the whole of it end to end (N8): the client's
  session writing, reading, flushing and closing a volume through six
  hosts' cs's -- nbswire's `serve` over nbscs, as nbs-cs.ko runs it -- asking
  its host's mds, whose answers are nbswire's `mds` over nbsmeta's state,
  over a network that delivers in any order of pairs, with hosts that crash
  -- their disks' caches kept, lost or torn -- and come back, writers lost
  with them, and writers taken over by force: now, or at rest, the old one
  asked on to write and flush. Every write done held on every replica in,
  every read to what a write done or out since may have left, every replica
  that comes back to what a flush made durable, the replicas the same once a
  writer serves and at the end, and no active replica doing an older
  generation's write or flush once a newer writer serves. At the end of
  some runs a host is lost for good, where every chunk can do without it:
  the writer's writes past it, its flush and its close must be done, and no
  flush may go to its cs once no set has it in.

  Its `join` target is one join's copy (N9b) between two engines, the
  source served by nbswire's `serve` as a cs serves it, and the hosts' mds
  played from the set's state the target keeps. The network delays,
  reorders, drops and repeats; up to three writes of the map now are in
  flight at once, each delivery anywhere among the copy's; writers of maps
  before the join write the source alone; the set moves on, and the join is
  given up and made again; a sector being checked rots; and the joiner's
  power fails mid-copy, now and then a few writes before a chunk is closed.
  In a third of the runs a member back joins its own place, with chunks
  found open after a cut; in a tenth each, one race random runs seldom put
  together is set up on purpose (testing.md). At its end every chunk of the
  set on the joiner is the source's, byte for byte, and every one the copy
  closed is still so after a power cut.

  Its `scrub` target is the leader's scrub alone (N9e), over a set of
  three members whose copies are the target's: writes and closes reach each
  member on a delay of its own, the scrub's asks on another way, a member
  holds a chunk once its first write reaches it, granules rot on one member
  or two, or a member loses a chunk; members are taken down and joined back
  under the looks, and the races random runs seldom make -- a write raced
  through a look, a close held back on one member -- set up of a
  randomness of their own. Every finding is held to the set's version now
  and to what each member held when it answered, every rot is found, and a
  chunk held by none takes no turn of the pace.

  The leader's scrub runs in the `cluster` target too, its asks and answers in a queue
  and a randomness of their own so that the run goes as it would without
  them, held to finding nothing the target did not rot; at the end of some
  runs a sector of a closed chunk rots on one member, or on two, the
  finding is held to it, and the members taken down as a person would are
  copied again: every replica alike.

  The writers' copy-ups run there too (N10c): snapshots of the volume
  taken between two writers -- the one stopped, the next started -- two kept
  at most, dropped now and then; the writers after copy up each chunk a
  snapshot shares before they write there, through joins, drains, crashes and
  takeovers; each cs's garbage pass frees what copy-ups and drops leave; and
  every snapshot reads, on every active replica up and at every look, what
  the volume held at it.

  `--mutants` takes a hundred and seventy-one rules out one at a time --
  seventy-one of the metadata's, five of them the controller's; forty-one of
  the cs's, twelve of them the copy's and eleven a copy-up's; thirty-one of
  the wire's, its `serve`'s, the mds's, the copy's and the scrub's;
  twenty-eight of the client's -- each caught.
- **`fuzz/raft`'s `nbsmeta` target**: the members' part of the state, its
  commands from pools of names and incarnations that meet and from wide ones
  that fill the cluster. There are four scenarios and six mutants.
- **`scripts/nbs-test.py`** (three nos on `scripts/hub.py`'s LAN, after the
  cluster registry is made among them, and a fourth booted late; every
  nbs-mds with `out_after=10`, the least it takes, and `scrub=1`). It
  covers:
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
  - the client (N8), through `nbs io`'s sessions: a reader refused while a
    host writes, a forced writer's fences failing the old session, a host's
    closed chunks and data kept over its reboot; nbsload's `verify` holding
    every block to its last write, and a read load on another host;
  - repair (N9): a volume's two chunks written, the sets moved on past
    them (b's cs taken away and back), c killed -- the leader's
    controller has its cs down, then out ten seconds on, its places for
    others' to take, of which there are none with three hosts -- one chunk
    written past it and closed, c booted again: its cs up, joined in its own
    place, every chunk at three replicas again with none being copied; a
    read on c of what was written while it was gone, which its join's copy
    took, and of the chunk not written meanwhile; and what its copies did,
    each join's end in its log (`dmesg`): the chunk written meanwhile pulled
    whole, the other found as its source's, nothing more;
  - a fourth host, d, booted late, invited into the registry and added to
    the NBS cluster, its cs taking its device in; a volume's two chunks
    written on a, b and c; one of the three killed for good -- its cs down,
    then out -- and d joining every set it was in, in its place: every chunk
    at three replicas, none being copied, the volume read off d's own
    replica; the lost host removed, three members again;
  - rot (N9e): a chunk written and closed, its bytes found in a pool image
    on each of its three hosts and a sector of them spoiled on one -- running
    -- through QEMU's monitor (`qemu-io` over QMP), under its cs; the
    leader's scrub saying that host's copy unlike the two others', with the
    command; the command run, the member joined back in its own place and
    its copy done; the chunk read off that host's replica as written, the
    spoiled sector back as it was in the image, and the finding shown as
    history; at the end, a chunk written and found alike, the volumes'
    others held by none, and no finding of now;
  - nothing panicked.
- **`scripts/hv-nbs-test.py`** (x86-64, manual: a guest kernel and an
  initrd): Linux guests of hv's on volumes -- an image poured with `hv
  convert` read by a guest on another host; a guest's writes read back past
  its page cache, and by a guest on another host after it stops; a start
  refused while another host's guest has the volume, then forced, the old
  guest's writes failing; a host killed under a guest's write, the write
  done and read back.

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
- A replica taken out stays out: bringing it back, or a new one in its
  place, is N9's repair. Until then each host lost takes a replica out of
  the sets it is in, and a set whose last active replica is lost waits for
  it.
- A reconcile copies a chunk left open whole, 4 MiB, from one replica --
  this host's, or the first in -- whether the replicas differ or not.
- A guest's disk offers no discards and no writes of zeros, and copies a
  read's data out of the session's buffer into the guest's.
- A cs serves one request at a time, each waited for on its device, and
  copies the data into a message and out of one. Measuring, and pages that
  move instead, are N11's.
- A cs serves only with every device the metadata gives it. A device that
  fails while it serves fails the requests of its chunks; one that is never
  taken in stops the cs serving. Taking a device out, and rebuilding what was
  on it, is N9's.
- A write to a chunk a snapshot or a clone shares, in a set being joined
  or with a member drained, waits for the join (N10c), and fails past the
  operation's deadline: a join of a big set takes minutes. plans/nbs.md has
  what would lift it.
- A chunk's data has no checksum of the cs's: a sector is taken to be
  written whole, as NVMe's atomic write unit promises. The scrub compares the
  replicas and says what differs; which copy is good a person decides. A
  copy takes its source's data as it is: rot on a source the scrub has not
  found yet goes into the joiner.
- A device drained stays draining, and neither a device nor a cs is ever
  removed. A placement never moves: moving chunks is N9's.
- Bounds, each refused past:
  - 4096 volumes;
  - 2^22 chunks, 16 TiB of volumes in all;
  - 64 cs's of 16 devices each;
  - a volume's readers: one a member, 16 at most.

  The state is in memory whole: some 3 MiB for every TiB of volumes.
