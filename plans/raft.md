# raft: consensus as a crate of its own

Asked 2026-11-05, while planning NBS ([nbs.md](nbs.md)): the Raft its mds
runs on written once, generic, as a crate of its own -- for whatever else
needs a replicated state later.

## What it is

- `src/crates/raft`: `#![no_std]` with `alloc`, `#![forbid(unsafe_code)]`,
  and no dependency on any crate of nos's -- not kcore, not ipc -- or on any
  outside it. It knows nothing of NBS, of files, of the network or of the
  clock: it is the algorithm, driven by whoever uses it.
- Linked into what uses it (`nbs-mds.ko` first), never into the kernel image
  for its own sake; built on the host by its fuzzer. Since it has nothing of
  nos in it, it could be published as it is.
- Why not one that exists: raft-rs (TiKV's, the Rust port of etcd's) needs
  `std` and brings protobuf, and openraft needs `std` and an async runtime;
  neither builds for a kernel target. The design to follow is etcd's -- a
  state machine with no I/O -- which etcd, raft-rs, CockroachDB and TiKV have
  run in production for years.

## Its layers

1. **`raft::node`, the protocol, with no I/O.** `tick()`, `step(message)`,
   `propose(bytes)`, `propose_change(change)`, `read_index(ctx)`,
   `transfer_leader(to)`; then `ready()`, which says what to do --
   the hard state and the entries to persist, a snapshot to install, the
   messages to send, the committed entries to apply, the reads that may now be
   served, a change of role -- and `advance()` once it is done. Entries are
   bytes the crate does not read. A group is a `Node`: many groups on a host
   are many `Node`s, and the user's envelope says which a message is for.
   What it does: elections with randomised timeouts (the randomness the
   user's, seeded), PreVote, CheckQuorum (a leader that hears from no
   majority steps down), replication pipelined and held to a count and a
   number of bytes in flight, fast backtracking over a conflict, a commit
   counted only on an entry of the leader's own term (the paper's figure 8),
   snapshots sent in pieces, learners, membership changed one server at a
   time with the 2015 fix (no change before the leader has committed an entry
   of its term), leadership transfer, ReadIndex.
2. **`raft::wire`, the messages as bytes**: versioned, every length checked,
   nothing trusted. A message from the network that does not parse, or names
   a term or an index that cannot be, is dropped and counted -- never a
   panic.
3. **`raft::log`, a durable log over the user's files**: a trait of a few
   calls (append, read at, length, truncate, sync, rename, remove, list).
   Segments of records, each with its length and a CRC32C; at a start, the
   tail cut at the first record that does not check, so that a torn write is
   never read as an entry; the hard state written to a new file, synced and
   renamed over the old; snapshots written in pieces with a CRC, synced and
   renamed. No allocation larger than a record or a piece -- the kernel's
   heap gives 512 KiB at most.
4. **`raft::rsm`, a replicated state machine over the three**: the user's
   `StateMachine` (apply an entry; write a snapshot in pieces and read one
   back); client sessions, so that a command retried after a leader change is
   applied once (a client's id and its sequence number, the dissertation's
   section 6.3); linearizable reads (ReadIndex at the leader) and reads at an
   index on any member (served once it has applied that far); the leader's
   name, for a follower to forward to. A snapshot is taken once the log
   since the last is as large as the state (with a floor), so that a byte
   of log costs at most a byte of snapshot. It is written from a frozen view
   of the state -- the state machine's `freeze` -- while applying goes on.
   Still no I/O: inputs in, outputs out.

## What is not in it

- **A clock.** A tick is whatever its user says: 100 ms in `nbs-mds`.
- **A transport.** The user sends what `ready()` hands it (over ipc, in nos)
  and hands what arrives to `step`.
- **Threads, locks, async.** One owner drives it.
- **Machinery for groups by the thousand** -- shared ticks, heartbeats
  coalesced per host. A user that needs that adds it above.

## Who uses it

- **The cluster's registry**, the first, right after R3: the hosts, their
  keys, addresses and roles, so that a host is let in once rather than by
  hand on every host ([cluster.md](cluster.md)).
- **`nbs-mds`**: `rsm`, with NBS's metadata as the state machine, the log on
  the root through `kcore::fs`, messages over ipc ([nbs.md](nbs.md)).
- **Later candidates, not planned here:**
  - the cluster's VMs and the host each runs on, with the scheduler that
    places them and restarts them where their host died
    ([vmctl.md](vmctl.md));
  - the state of VM networks across hosts -- networks, subnets, ports and
    the hosts they are on, routes, public addresses -- which wants to know
    where every VM runs, as vmctl does, so the two may share a group
    ([ipc.md](ipc.md), "Later: VM networks across hosts");
  - the host API's clients and audit log for the whole cluster;
  - leases and locks;
  - NBS's data in Raft groups (Multi-Raft), should writing to all replicas
    ever give way.

## Tested

`fuzz/raft` (host, in CI, through `scripts/raft-test.py`). Fixed inputs are
the gate; `--seed N --seconds S` runs a campaign.

- **`node`**: three to seven members in one process over a simulated network
  -- delays, losses, duplicates, reordering, partitions, members crashing and
  coming back with what they had synced -- through membership changes,
  leadership transfers and snapshots. It is held at every step to Raft's
  properties:
  - at most one leader a term;
  - logs that match from where they agree back;
  - no committed entry lost or changed;
  - every member applying the same entries in the same order;
  - no read served stale (a test register through `rsm`, checked for
    linearizability);
  - progress: a leader, and commits, within so many ticks once a majority
    hears one another.
- **`log`**: the file format through crashes, torn writes and lost ones.
  What was synced is there after; a torn tail is cut and never read as an
  entry; noise is refused.
- **`wire`**: every message read from noise without a panic, and read back
  as written.
- **The known traps, as fixed inputs**: the paper's figure 8, a partitioned
  old leader asked to read, a membership change before the leader's first
  commit, a snapshot overtaking the log, a member wiped and back under the
  same id.
- **Mutants it must catch**, as the API's and the relay's were caught: the
  figure 8 check taken out, a vote not synced before it is answered, the
  election timer not reset on granting a vote, the commit index moved past
  what the leader has.

## Steps

- **R1, the core and its wire**: `node`, `wire`, a storage in memory; the
  `node` and `wire` targets, the traps, the mutants.
- **R2, the log**: `log` and its target.
- **R3, the state machine**: sessions, reads, snapshots; the linearizability
  check through it.

Then the cluster's registry is its first user ([cluster.md](cluster.md)),
and `nbs-mds` its second ([nbs.md](nbs.md)).

## Limits

- One group a `Node`; groups by the thousand need a layer above.
- Membership changed one server at a time, without joint consensus: enough
  for three to five voters added and removed in turn.
- No leader leases: a read is by ReadIndex or at an index, never by the
  clock, so a read at the leader costs a round of heartbeats.
- Snapshots are the user's bytes: the crate moves them; the state machine
  makes them and reads them.
