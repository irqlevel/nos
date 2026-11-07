# NBS's metadata service

`nbs-mds.ko` (`src/crates/modules/storage/nbs-mds`) is the first piece of NBS,
the replicated block storage for VM disks, images and snapshots planned in
[plans/nbs.md](../plans/nbs.md): its metadata service, the state every host of
an NBS cluster holds alike through Raft. As built (step N5), that state is the
NBS cluster's identity and its members. Volumes, devices and the placement of
their chunks come with N6 (`nbsmeta`), the chunk servers with N7.

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

On the host, in `/nbs/mds`:

- `member` -- which member of which NBS cluster this host is: its
  incarnation, the cluster's id, its own name and the cluster's on the first
  line; for a host that took an offer, a line for each member it was offered
  by, which it knows the members by until its log has caught up. Written once
  and never written over: the mark that this host is a member.
- `raft/` -- the member's log (`raft::log::DiskLog`).

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
  leader is being chosen;
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

- **`fuzz/raft`'s `nbsmeta` target** ([testing.md](testing.md)): the state
  machine against a model of its own. Its commands come from pools of names
  and incarnations that meet, and from wide ones that fill the cluster. The
  snapshot is checked read back, changed and refused. There are four
  scenarios and six mutants.
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
