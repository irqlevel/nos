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
- **R2, the log**: `log` and its target. As built, on branch `raft-log`:
  - **What the user gives it.** A directory of its own, as `Files`: list,
    length, read at, append, truncate, sync a file, sync the directory,
    rename, remove. A POSIX file system needs both syncs; nos's `File::sync`
    flushes the whole file system, so there the two are one.
  - **Segments.** `wal.<seq>`, written only at their end, as etcd's WAL is.
    Each has a header (magic, version, its seq, CRC32C), then records: a
    length, a CRC32C over the length and the payload, and the payload --
    an entry (`wire::encode_entry`), the hard state, or a snapshot's
    marker. An entry that replaces others is written after them, never over
    them: reading the log back replays the records in order, as
    `MemStorage::append` does. So nothing is cut and then written again in
    place, where a crash in between could bring back what was cut.
  - **The hard state is a record of the log, not a file of its own.** The
    plan above had a file, written anew, synced and renamed. That is two
    syncs and a directory's on every `Ready` whose commit moved, which is
    nearly every one. As a record it rides the one sync that makes the
    `Ready`'s entries durable. Every segment begins with the hard state as
    it stood, so the newest is never in a segment that compaction removes.
  - **Snapshots** go to `snap.<index>`: written as `.tmp` in pieces with a
    CRC each, synced, renamed, and the directory synced. Only then is a
    marker record written to the log. A marker says the snapshot was either
    taken (the state machine's, the entries up to it compacted) or
    installed (a leader's, the whole log dropped). Reading the log back,
    the newest marker names the snapshot to load, and a snapshot file with
    no marker -- a crash between the rename and the marker -- is removed.
  - **The cut.** On opening the log, the last segment's tail is cut at the
    first record that does not check: its length, its CRC, its decoding, or
    an index past the last and one. The cut is synced before anything is
    written after it. Any other segment was synced before the next was
    made, so a bad record there is corruption, and the log refuses to open.
  - **Compaction.** After a marker, segments are removed oldest first:
    those that come before the marker's segment and hold no entry past the
    snapshot. Once one would hold a live entry, removal stops, so what
    remains replays to the same log.
  - **Never larger than a block of the kernel's heap.** A record or a piece
    is read at a time. Writes go through a buffer of 128 KiB. The list of
    segments is capped, and past the cap appends are refused until a
    snapshot.
  - **What opening adds**: every segment, the snapshot and the directory
    synced before the log is used -- a program started over after a failure,
    the machine never down, sees what was written and never synced.
  - **The fuzzer** (`fuzz/raft`'s `log` target, on a disk of its own with
    POSIX's promises and no more) found four bugs, each now a scenario:
    1. A leader's snapshot below the commit was taken. The log then
       committed past its own end, and refused to open.
    2. An open that used what was not yet durable. A segment whose
       directory sync had failed, used after a restart without a power cut,
       lost entries that had been synced at the next cut.
    3. An installed snapshot was honoured only while it was the newest
       marker. Once a snapshot had been taken after it, the log before it
       came back from segments whose removal had failed.
    4. An entry below a taken snapshot, written after entries past it, was
       skipped on replay together with the truncation it had made. The
       entries it had replaced came back.
  - **Mutants** (`fuzz/raft/mutants.py`, `log_*`): the CRC check, the seal
    before a new segment, the syncs at open, the snapshot's sync before its
    rename, the compaction's bound, the installed snapshot's drop -- each
    taken out, the gate and its scenario fail.
- **R3, the state machine**: sessions, reads, snapshots; the linearizability
  check through it. As built, on branch `raft-rsm` (from `raft-log`):
  - **`Rsm<S, M>`** drives a `RawNode` over a store `S` (`LogStore`:
    `MemStorage`, or R2's `DiskLog`) and the user's `StateMachine` `M`. Its
    caller hands it ticks, messages and clients' requests; `process()`
    writes and syncs what the node's `Ready` says, applies what is
    committed, serves the reads that may be served, takes the next steps of
    a snapshot, and hands back the messages to send and the events to answer
    clients with. Still no clock and no network.
  - **The state machine**: `apply(index, command)` -- the answer --,
    `query(query)` against the state as it is, `size()`, `freeze()` -- a
    view that applying does not change, written a piece at a time -- and
    `restore` from a snapshot's bytes.
  - **Sessions** (the dissertation's 6.3). A client registers, and the
    entry's index is its id. Each command carries the client and a sequence
    number, one command at a time per client. A command retried -- after a
    leader changed, a timeout -- under the number last applied is answered
    from the session and not applied again. One under that is `Stale`, one
    of a session gone `Expired`. Sessions are part of the replicated state:
    in the snapshot, evicted least recently used past 1024 in log order, so
    every member evicts alike, and an answer longer than 1 KiB is not kept
    (`Lost` on a retry).
  - **Reads**: linearizable by ReadIndex, served once applied to the index
    it confirms, and failed back to the caller if the leadership changes
    first. Or at an index the client names -- say its last write's -- on
    any member, once it has applied that far.
  - **Snapshots**: begun once the log applied since the last is as large as
    the state, with a floor. Frozen at the index applied then, written over
    several calls while applying goes on, and handed to the store
    (`create_snapshot`), which compacts the log. A leader's snapshot
    restores the sessions and the state machine.
  - **Tested**: a register through `Rsm` in a group over a network that
    fails, members crashing on R2's log over the crash disk, clients that
    time out and retry; the history of every client's operations checked
    for linearizability (Wing and Gong's search). Mutants: the session's
    answer for a retry, the read's wait for its index, the freeze at the
    snapshot's index, the sessions in the snapshot. As run:
    - fuzz/raft's `rsm` target: 100 runs of 1500 steps, 300 to 600
      operations each, snapshots every few hundred entries;
    - five scenarios, one of them the check itself held to known
      histories;
    - each mutant caught by its scenario and by the target alone within
      forty runs.
    - What a campaign found: a member restarted named its reads' contexts
      from 1 again. A leader's answer to a read asked before the restart,
      arriving after it, matched a read asked since, and that read was
      served at the old index, older than a write already acknowledged
      (one run in 130,000). `Rsm::new` now takes a nonce, a number of the
      start's own that every context carries; the scenario
      `rsm_read_after_restart` and the mutant `rsm_ctx_reused` hold it.

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
