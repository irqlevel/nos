# Raft

`src/crates/common/cluster/raft` is Raft, the consensus protocol, as a crate of its own: a
group of members that agree on one log, so that every member applies the same
entries in the same order and none applied is lost while a majority of them
lives. It is what the cluster's registry and NBS's metadata service are to
run on ([plans/raft.md](../plans/raft.md)); nothing of nos is in it -- no
kcore, no `unsafe`, no dependency but the tree's own `crc` (the log's
CRC32C) -- so a module links it as it is and `fuzz/raft` runs it on the
host.

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
    if let Some(e) = node.fault() { report e; start the member again over its storage }
}
```

A read the storage fails -- an entry's term, entries, the snapshot -- stops
the member (`fault`): what it read cannot be believed, so whatever it
decided on it, a vote given on a last term it could not read among them, is
never written or sent; every call is refused with the failure and no
`Ready` is handed out, so the caller's loop does not spin on nothing. It is
started again with `RawNode::new` over what its storage holds.

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
- A read's round asked again by every heartbeat, as etcd's are, so that one
  whose own heartbeats were lost is answered by the next; a round a change
  of voters has confirmed answered then.
- A follower takes as much of an append as there is room for and says how
  far; a leader whose follower is behind sends it an empty append with each
  heartbeat's answer, which it refuses if it has not everything -- lost, or
  not taken -- and is probed from what it has.

What it does not do: there is no clock, network, thread or lock in it, no
leader leases (a read costs a round of heartbeats), no joint consensus (one
server at a time), and no machinery for thousands of groups at once.

## The log on disk

`raft::log::DiskLog` keeps what a member must not lose -- its entries, its
hard state and its newest snapshot -- in a directory its user hands it as
`Files`: list, length, read at, append, truncate, sync a file, sync the
directory, rename, remove. The promises it is held to are POSIX's: a file's
writes are on the medium once that file is synced, a name made, renamed or
removed once the directory is. In nos, `kcore::fs` gives every call, and a
`File::sync` flushes the whole file system, so there the two syncs are one.

```
let mut log = DiskLog::open(files, LogConfig::default())?;        // or DiskLog::bootstrap(files, config, voters, learners)
let mut node = RawNode::new(config, log ...)                      // DiskLog is a Storage
...
let rd = node.ready();
if let Some(s) = rd.snapshot { log.apply_snapshot(s)? }           // durable when it returns
log.append(&rd.entries)?;
if let Some(hs) = rd.hard_state { log.set_hard_state(hs)? }
log.sync()?;                                                      // one sync for all of it
send rd.messages ...
```

A snapshot of the state machine's own goes in with `create_snapshot(index,
conf, data)`, durable when it returns; the log then drops what it made
needless. An error stops the log -- every call after says `Stopped` -- and
`DiskLog::open` over the same files starts it again from what they hold.

How it is laid out:
- **Segments** (`wal.<seq>`) are written only at their end, as etcd's WAL
  is. Each is a header, then records: a length, a CRC32C over the length and
  the payload, and the payload. A payload is an entry, the hard state, or a
  snapshot's marker.
- **Entries that replace others** are written after them, not over them.
  Reading the log back replays the records in order, so nothing is ever cut
  and then written again in place.
- **The hard state rides the `Ready`'s one sync.** It is a record, and every
  segment begins with it as it stood.
- **A snapshot** is a file of its own (`snap.<index>`), in pieces with a CRC
  each. It is written as `.tmp`, synced and renamed, and the directory
  synced, before a marker in the log names it, as taken or as installed.

Opening the log reads it twice:
1. **The first pass checks every record.** It cuts the last segment's tail
   at the first record that does not check -- a crash's torn write is never
   read as an entry -- and finds the newest marker and hard state. Any other
   segment was synced before the next was begun, so a bad record there is
   corruption, and the log refuses to open.
2. **Everything read is then made the medium's.** Every segment, the
   snapshot and the directory are synced before anything is told of them. A
   program started over after a failure, with no power cut, sees what was
   written and never synced, and a member that answered for it would lose it
   at the next cut.
3. **The second pass replays the entries past the snapshot.** It skips
   everything before the newest installed snapshot, which dropped it. An
   entry in the snapshot that comes after entries past it drops them again,
   as it did when it was written.

As etcd does, the log since the snapshot is held in memory (`MemStorage`,
in pieces), and the files are for its durability. After a snapshot, segments
are removed oldest first while none holds an entry past it.

## The replicated state machine

`raft::rsm::Rsm` is what every user of the crate needs on top of a node:
- the node itself, over a store (`MemStorage`, or `DiskLog`);
- the user's `StateMachine`;
- clients' sessions, reads, and snapshots taken as the log grows.

It is still driven, with no clock and no network:

```
let mut rsm = Rsm::new(config, store, State::new(), nonce)?;   // the store's snapshot restored, the log after it applied again
every tick:          rsm.tick()
a message arrives:   rsm.step(m)
a client asks:       rsm.register(request), rsm.submit(client, seq, &command),
                     rsm.read(request, query), rsm.read_at(request, index, query)
then:                let out = rsm.process()?;
                     send out.messages; answer clients with out.events
```

`process` writes and syncs what the node's `Ready` says before it hands out
a message. It applies what is committed, serves the reads that may be
served, and takes the next steps of a snapshot. Every member applies every
entry and says so: the caller answers the clients that are its own, by the
request ids it chose and by the client and sequence number of a command.

- **The state machine.** `apply(index, command)` gives the answer, and must
  give the same on every member: nothing in it may depend on a clock or the
  member's id. `query` reads the state as it is, `size` says about how
  large a snapshot of it is, `freeze` gives a view of it that applying does
  not change (copy-on-write, cheap), written a piece at a time, and
  `restore` reads one back.
- **Sessions.** A client registers -- an entry, whose index is its id --
  and numbers its commands, one at a time. A command retried under the
  number last applied is answered from the session (`Applied`) and not
  applied again; one under that is `Stale`; one of a session that is gone,
  `Expired`, after which the client registers again. Sessions are part of
  the state, carried by the snapshot.
- **Reads.** `read` is linearizable: the leader confirms it with a round of
  heartbeats (ReadIndex), and it is served once applied as far, or failed
  back (`ReadFailed`) if the leadership changes first. Its context carries
  `nonce`, a number of this start's own -- random, or a count of starts --
  so that a leader's answer to a read the member asked before it was
  started again, arriving after, is never taken for a later read's. `read_at` is served
  on any member once it has applied as far as the index the client names --
  its own last write's, so that it reads its writes.
- **Snapshots** are taken once the entries applied since the last are as
  large as the state, with a floor (`SNAPSHOT_FLOOR`, 1 MiB). The state is
  frozen at the index applied then and written a few pieces a call while
  applying goes on; the store then compacts the log. A leader's snapshot
  restores the sessions and the state.

## A member's own asks

`raft::asks` is what a member does with commands asked of it -- a shell's,
on whichever member it runs -- so that each is applied once, whoever
leads. Each ask gets a session of its own: a registration
(`Rsm::register_for`, which names the asking member, so that its
`Registered` is told from another member's of the same request number),
then its one command under that session's number 1, then the session
ended. Until the asking member's own log says what became of the command,
it is sent again -- the same session, the same number -- to whichever
member leads: at once when the leader changes, and after a while
otherwise. A command whose first try is in a log as its leader steps down
is answered from the session when it is asked again, not applied twice.
The asking member learns what became of it as it applies the log itself;
and the leader that applies a registration or a command it took for
another member tells that member too (`asks::Tell`) -- at once, where the
member may be far behind in the log (a host just booted, catching up by a
snapshot that skips the entry's own event), or never apply the entry at
all (a host the command removes from the group). Either word is the same
entry's, so whichever comes first is taken, and a word lost is no matter.

What became of an ask: `Applied`, with the entry and the answer; `Lost`
(applied once, its answer too long to keep and its application not seen
here, a snapshot taken in over it); `NotAsked` (its time up with its
command never sent -- not applied, ever); `Unknown` (its time up with its
command sent and not heard of -- it may yet be applied, once; its session
is ended then, so a try still on its way is not applied where the end
comes first). A refusal of the leader's before it submits is never final:
an earlier try may be in a log already, so the command is sent again in a
moment, the refusal kept for what the ask is told if its time runs out.

raftmember (`src/crates/modules/cluster/raftmember`) runs it for the
shell of every group's module: the leader does what is to be sent itself;
another member hands it to the leader as `TAG_SESSION`, and the leader's
word -- a refusal, a session made, a command applied -- comes back as
`TAG_TOLD`. Its first version had no word from the leader: a member far
behind waited for its own log at each step, and the leader of two asked to
remove itself, the lead handed on, never heard that it had been removed --
its shell told after 30 s that the command may yet be applied
(cluster-test). Before it, raftmember submitted a
shell's command under the leader's own session and asked it again under a
new number when the leader changed: a `cluster invite` whose first try
was in the log as the leader stepped down was applied twice, and its
second application refused it -- the invitation made, the shell told it
failed.

The one gap: a session evicted (past `MAX_SESSIONS` newer ones) after its
command was applied, where the asking member did not see the application,
makes the next try `Expired` and the command asked again under a new
session -- which takes a thousand registrations within one ask's life.

## A group of one, by force

`raft::recover::force_alone(store, me)` makes a member's store -- while no
member runs over it -- the log of a group of `me` alone, for a group whose
majority is gone for good: a change taking out every other member, voters
then learners, appended to it one a member, and the log committed to its
end there and then. The member started again over the store is its group's
one voter and elects itself. It is unsafe by design, as etcd's
`--force-new-cluster` is: what the group had committed and this member had
not is gone, and what this member had appended and the group never
committed is committed now -- so the members taken out never come back with
what they hold; the cluster's `recover` removes them from its registry for
good (docs/cluster.md). A learner, or a member never let in, is refused
(`NotAVoter`); nothing is written before a refusal.

The scenario `force_alone_after_loss` holds it to all of that: a group of
three voters and a learner, an entry appended to the third member alone,
everyone else lost; the third forced alone takes out the other three,
commits that entry, elects itself and commits on its own -- and two mutants
(the changes left uncommitted, the learners left in) each fail it.

## Tested

`scripts/raft-test.py` ([Tests and gates](testing.md)) builds `fuzz/raft`
and holds the crate to known answers, the known traps played out, groups
fuzzed over a network that fails in every way, checked at every step against
Raft's properties, a member fed messages no correct member sends, the log
on disk through power cuts, torn writes and lost ones, and a register
through the replicated state machine whose clients' histories are checked
for linearizability, and every member's own asks under losses, doubles,
cuts, leaders' refusals and snapshots taken in -- none applied twice, each
told truly what became of it -- every allocation of it to the 512 KiB one
block of the kernel's heap is;
`fuzz/raft/mutants.py` holds the gate to catching each rule taken out.

## Limits

Nothing the crate allocates is larger than a block of the kernel's heap,
however long the log or busy the group: a message is at most 512 KiB
(`wire::MAX_MESSAGE_BYTES`), an entry 256 KiB, an append's entries 256 KiB
of data and 4096 of them, the entries waiting to be written 4096, a batch
handed out to apply 4096 entries, a snapshot 1 GiB in 64 KiB pieces, the
membership changes in the log kept in pieces of 1024, `MemStorage`'s log
in pieces of 1024 that a snapshot drops whole rather than copying what
comes after it; a `Ready` hands out
at most 2048 messages, and past them a message is dropped and counted
(`Stats::unsent`) as a network would drop it. A term or index of 2^62 or
past, which no group reaches, is refused in a message and in what a
storage starts a member from, and a member does not stand or write past
it, so nothing a peer or a disk says overflows; a configuration's election
timeout is at most 2^24 ticks, and an append's entries no more than may
wait to be written. A snapshot taken in part and then given up -- no piece
for two election timeouts -- is dropped. Raft trusts its members: one that lies can make the group
agree to anything, and the crate's promise to it is only that it does not
panic, keeps its own term, vote and commit, and stays within those limits.

The log on disk reads at most a record or a piece at a time and gathers its
writes in 128 KiB; it keeps up to 8192 segments (`MAX_SEGMENTS`, 8 MiB each
by default), and past them it stops (`Full`) until a snapshot compacts it.
It refuses a leader's snapshot below what is committed, as a node never
restores one there. It cannot tell a byte of the last segment that went
bad after it was synced from a torn write: either is cut there, so a disk
that corrupts what it has written can cost the newest entries. In any other
segment, or in the snapshot, the log refuses to open. A sync that fails is
taken as having written nothing -- the log stops -- but a file system that
then marks the pages clean and lets a later sync succeed (Linux's "fsync
gate") is outside what it can know.

The replicated state machine keeps at most 1024 sessions, evicting the
least recently used in the log's order. It keeps an answer for a retry only
up to 1 KiB: a retry of a longer one is told `Lost`. It keeps at most 4096
reads waiting. A client has one command at a time; a second before the
first is answered is the first's retry if it has its number, and otherwise
moves the session on, after which the first's answer is `Stale`. A member
has at most 64 asks in flight (`asks::MAX_ASKS`); one more is refused at
once.

## Next

The crate's users: the cluster's registry ([plans/cluster.md](../plans/cluster.md),
C1 and C2) -- its state machine is `src/crates/common/cluster/registry`, a `StateMachine`
of its own, held by fuzz/raft's `registry` target -- then NBS's metadata
service ([plans/nbs.md](../plans/nbs.md)).
