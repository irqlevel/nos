# Raft

`src/crates/raft` is Raft, the consensus protocol, as a crate of its own: a
group of members that agree on one log, so that every member applies the same
entries in the same order and none applied is lost while a majority of them
lives. It is what the cluster's registry and NBS's metadata service are to
run on ([plans/raft.md](../plans/raft.md)); nothing of nos is in it -- no
kcore, no `unsafe`, no dependency -- so a module links it as it is and
`fuzz/raft` runs it on the host.

## Driving a member

A member is a `RawNode`: a state machine with no I/O, in etcd's shape. Its
caller gives it the time, what arrives and what its users ask, and does what
it says:

```
loop {
    every tick:          node.tick()
    a message arrives:   node.step(m)
    a user asks:         node.propose(data), propose_change(cc), read_index(ctx), transfer_leader(to)
    if node.has_ready() {
        let rd = node.ready();
        write rd.snapshot, rd.entries and rd.hard_state, and sync
        send rd.messages              -- only now: a vote is on the disk before it is answered
        apply rd.committed; serve rd.read_states once applied that far
        node.advance();
    }
}
```

Nothing steps the node between `ready` and `advance`. A tick is whatever
the caller says -- a hundred milliseconds, say -- and the election timeout
is `election_ticks` of them and up to as many again at random, from the
caller's seed. Messages become bytes and back with `raft::wire`, which
checks everything it reads and never panics.

A group starts with `MemStorage::bootstrap(voters, learners)` on each of its
first members: their logs begin with the membership changes that made the
group, committed, so that a member added later, sent the log from its first
entry, makes the same membership of it. A member added later starts with
nothing (`MemStorage::new()`) and is sent the group by its leader.

## What it does

- Elections with randomised timeouts; PreVote, so that a member cut off and
  back does not depose a leader that is fine; CheckQuorum, so that a leader
  that hears from no majority steps down and a member that hears from a
  leader votes against no one.
- Replication pipelined, held to a count and a number of bytes in flight to
  each follower, a conflict found in few round trips by the follower's hint;
  a window of appends lost for good freed a heartbeat at a time.
- A commit counted only on an entry of the leader's own term (the paper's
  figure 8); a leader's own entries counted once they are on its disk.
- Snapshots: the caller's bytes, sent to a follower the log no longer
  reaches in pieces of `snapshot_piece_bytes`, each acknowledged.
- Learners; membership changed one server at a time, each change in force as
  it is appended, never before the leader has committed an entry of its term
  (the 2015 erratum) and never two not yet committed.
- Leadership transfer (TimeoutNow) and linearizable reads (ReadIndex).
- Every request of an older term answered with ours -- an append, a vote --
  as the paper has it.

What it does not do: there is no clock, network, thread or lock in it, no
leader leases (a read costs a round of heartbeats), no joint consensus (one
server at a time), and no machinery for thousands of groups at once.

## Tested

`scripts/raft-test.py` ([Tests and gates](testing.md)) builds `fuzz/raft`
and holds the crate to known answers, the known traps played out, groups
fuzzed over a network that fails in every way, checked at every step against
Raft's properties, and a member fed messages no correct member sends --
every allocation of it to the 512 KiB one block of the kernel's heap is;
`fuzz/raft/mutants.py` holds the gate to catching each rule taken out.

## Limits

Nothing the crate allocates is larger than a block of the kernel's heap,
however long the log or busy the group: a message is at most 512 KiB
(`wire::MAX_MESSAGE_BYTES`), an entry 256 KiB, an append's entries 256 KiB
of data and 4096 of them, the entries waiting to be written 4096, a batch
handed out to apply 4096 entries, a snapshot 1 GiB in 64 KiB pieces, the
membership changes in the log kept in pieces of 1024; a `Ready` hands out
at most 2048 messages, and past them a message is dropped and counted
(`Stats::unsent`) as a network would drop it. A term or index of 2^62 or
past, which no group reaches, is refused in a message, so nothing a peer
says overflows. Raft trusts its members: one that lies can make the group
agree to anything, and the crate's promise to it is only that it does not
panic, keeps its own term, vote and commit, and stays within those limits.

## Next

The durable log over a caller's files (`raft::log`, R2) and the replicated
state machine with client sessions and reads at an index (`raft::rsm`, R3),
as [plans/raft.md](../plans/raft.md) has them.
