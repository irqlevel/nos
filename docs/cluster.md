# The cluster's registry

`cluster.ko` (`src/crates/modules/cluster/cluster`) keeps one list of a nos
cluster's hosts -- their names, keys and addresses -- on every host of it,
through Raft, so that a host is let in once and every other host learns of
it, instead of `hosts.toml` written by hand on each ([plans/cluster.md](../plans/cluster.md),
C1). The list is `registry::State` (`src/crates/common/cluster/registry`, no kernel in it),
the state machine of the raft crate's `rsm` ([raft.md](raft.md)); the
messages go over the message channel between hosts, ipcnet
([ipc.md](ipc.md)), and ipcnet is told the peers the list gives it.

## Using it

On the first host, ipcnet and then the cluster:

```
$ insmod /lib/modules/ipcnet.ko
$ ipcnet init a                     # this host's name, in a cluster of its own
$ ipcnet start
$ insmod /lib/modules/cluster.ko
$ cluster init test
cluster: test made, a its first host (node 1) -- `cluster invite` the next
```

On a host to come in, the same up to its key, which the invitation names:

```
b$ insmod /lib/modules/ipcnet.ko; ipcnet init b; ipcnet start; insmod /lib/modules/cluster.ko
b$ ipcnet id
dcbe3e82ded37008fa428641cf4b4bad98fc2fc50177a5082057c11774db8c95
a$ cluster invite b 10.9.0.2 dcbe3e82ded3...
cluster: b invited from 10.9.0.2 for 15 minutes -- on it: cluster join nos1-6f1c...
b$ cluster join nos1-6f1c...
cluster: b joined as node 2, with 2 hosts -- the registry follows from the leader
```

Then, on any member:

```
cluster                         this member: node, role, term, leader, what it applied, hosts and voters
cluster hosts                   every host: node, voter or learner, addresses, key, maintenance -- and the invitations open
cluster remove <host>           a host out, for good: its key refused from then on
cluster revoke <host>           its open invitation taken back, its token good for nothing
cluster maintenance <host> on|off   a host being worked on, or back
cluster address <host> <role> <ip|none>   a host's address for ipc, underlay or public
cluster rekey                   this host moved to a new key, no connection lost
cluster recover [<cluster>]     on the last host standing: what it does, then -- the cluster's name given -- does it
```

The same are the `cluster` provider's on the management bus -- reads
`status` and `hosts`, actions `invite` (the token answered once, kept
nowhere), `revoke`, `remove` (its host's name typed again), `maintenance`,
`address` and `rekey` -- so that the panel lists the hosts and an admin
invites one from its page (`cluster.descriptor.json`). `join` and `recover`
are typed at the host's own shell: a token comes from another host, and a
recover is unsafe by design.

At boot, /etc/rc loads both and starts ipcnet; the member starts by itself
over its log:

```
insmod /lib/modules/ipcnet.ko
ipcnet start
insmod /lib/modules/cluster.ko
```

## What it keeps

- **The registry**: the cluster's id and name; its hosts, each with its
  node, its key's fingerprint (two while a key changes), its addresses and
  labels; the hosts removed, kept so that a removed key is never taken
  back; and the invitations open, each until its time. Every command
  carries the proposing member's clock, and every invitation past its time
  by an entry's clock is let go of as that entry is applied, on every member
  alike. At most 16 hosts and open invitations together; the first five to
  join vote, the rest are learners.
- **Its folder**, /etc/cluster unless `insmod cluster.ko conf=` names
  another:
  - `member` -- which member of which cluster this host is: its node, the
    cluster's id and its name; for a host that joined, also every host its
    join was answered with -- name, node, address, keys -- which it reaches
    the cluster by until its log has caught up, after a reboot too. Written
    once, synced, never written over: the mark that this host is a member.
    A log in `raft/` with no `member` beside it is what an `init` or a
    `join` that did not finish left, and the next one clears it.
  - `raft/` -- the member's log, `raft::log::DiskLog`: segments of records
    each with a CRC32C, snapshots by rename ([raft.md](raft.md)).
  - `.sync`, an empty file: a sync of the directory is a sync of it, nos's
    filesystem making everything durable at any file's sync.

## How a host joins

1. **Its key first**: `ipcnet id` on the new host prints its key's
   fingerprint, the key made at ipcnet's first start.
2. **`cluster invite <host> <ip> <key>`** on any member commits an
   invitation: the host's name, the address it will come from, its key, the
   hash of a secret, and when it ends -- fifteen minutes on by the leader's
   clock. The command prints the token, once: the cluster's id, this
   member's name, address, port and key, the invitation's number and the
   secret, as `nos1-` and hex. The registry keeps the secret's hash, never
   the secret. Every member's ipcnet is told the invited host as a peer as
   it applies the entry, pinned by the key given -- a host invited, which
   reaches the member's `cluster.raft` by that name and nothing else: no
   other mailbox, no mailbox by its address, no monitor. ipcnet holds it to
   that (`only=cluster.raft` on its line of hosts.toml) until the join makes
   it a host, and lifts it in place, its connections kept.
3. **`cluster join <token>`** on the new host: ipcnet takes the token's
   cluster as its own (only a host with no peers yet may), pins the
   inviting member by the token's key, and the request -- the invitation's
   number and the secret -- goes to that member's `cluster.raft` over the
   connection, pinned both ways. The member takes it only from the host
   the invitation names, which is ipcnet's word -- the name its key is
   pinned under -- and only with the secret whose hash the invitation
   keeps; the leader commits `Join`, which makes the host with the
   invitation's key and closes the invitation in one entry. A token is good
   once. The joiner asks again while nobody can take the join yet -- no
   leader, or one not ready -- and the member takes an invitation's join
   once while it is under way, so a request asked again meanwhile never
   finds the invitation used by its first.
4. **The answer** is the joiner's node and every host. The joiner writes its
   `member` with them, tells ipcnet every host as a peer and starts its
   member over an empty log. The leader adds it to Raft as a learner, sends
   it the log, and makes it a voter once it holds it -- within eight entries
   of what is committed -- if it is among the first five.

Refused: a token for another host (by the joiner itself), used again, past
its time or taken back; a wrong secret; at the invitation, a name, an
address or a key that is a host's or an open invitation's already, and a
removed host's key.

**Why the key at the invitation.** plans/cluster.md first had ipcnet take a
TLS client whose key nobody pinned, while an invitation was open, for one
mailbox, `cluster.join`, and the secret proved by an HMAC over both keys.
As built, ipcnet has no such path: every connection it takes or makes is
pinned both ways, so no handshake from a key nobody named is ever answered,
there is no state before authentication for anyone to flood, and the secret
needs no HMAC -- it only ever goes inside a TLS session bound to both keys.
What it costs is one value carried to the inviting member before the
invitation instead of after: the new host's key. What the first design
promised still holds: before its join, the host reaches the join and
nothing past it.

## A host's key, moved

`cluster rekey` on a host moves it to a new key with no connection lost:

1. ipcnet makes a key and stages it -- `tls/next.pem`, not yet used
   (`ipcnet.ctl`'s `TAG_KEY_STAGE`; `ipcnet id` says it);
2. the registry is told of it (`AddKey`): every member's ipcnet pins the
   host by both keys as it applies the entry;
3. ipcnet switches to it (`TAG_KEY_USE`): `tls/identity.pem` written whole
   with it -- key and certificate in one file, so that the two never go
   apart -- the TLS server and every peer's clients made anew with it, and
   the connections up kept;
4. the old key is dropped (`DropKey`) and retired: refused from then on, by
   every member and in every invitation, as a removed host's is.

A peer whose keys change keeps its connections: a connection keeps the key
it was made with, and the next one is made with a key the peer has then --
in ipcnet a peer is its name and its address, and its keys change in place
(`PeerConf::same_link`). So a rekey is a rotation, not a revocation: a key
thought stolen is answered by removing the host, which cuts its
connections at once.

Each step is asked again harmlessly, and a rekey cut short anywhere --
a reboot between two of them -- goes on from where it stopped at the next
`cluster rekey`: it reads where it is from the registry's keys for the host
(the newer one last), ipcnet's key and its staged one.

## When the majority is gone

`cluster recover` on the last host standing makes it its cluster's one
voter. It is refused while the member follows a leader, or while any other
voter's member answers a probe within 3 s; without the cluster's name it
says what it is about to do, and with it, it does it:

1. the member stopped, and its log made the log of a group of itself alone
   (`raft::recover::force_alone`, docs/raft.md): every other member taken
   out by changes appended to it, and the log committed to its end;
2. the member started again over it, electing itself at once;
3. every other host removed from the registry, for good -- their keys
   refused with them.

Unsafe by design, as etcd's `--force-new-cluster` is: what the lost hosts
had committed and this one had not is gone, and what this one had appended
and the group had not committed is committed. That is why the others are
removed: none of them comes back with a history that went another way; a
host lost and found again is installed anew and invited. A minority cut
off from a live majority cannot tell it from a majority gone -- the probe
asks, and the typed name is the admin's word that they are gone.

## How a member runs

- **One task**, `cluster/raft`: raft's `Rsm` over the log in `raft/`, a tick
  every 100 ms, its mailbox `cluster.raft` -- a `Cluster` name -- and its
  messages to `cluster.raft@<host>`, those over 4 KiB on ipcnet's bulk
  connection. A message is taken only from the host whose node it says it
  is from: which host sent it is ipcnet's word (`kcore::ipc::peer_name`),
  looked up in the registry.
- **Commands**: the shell hands its command to its member's task; a member
  that does not lead hands it to the leader and passes the answer back --
  taken from that leader alone, and, should another be elected meanwhile,
  given up at once to be asked of the new one. What could not be taken yet
  -- no leader, a leader without its session yet -- the shell asks again,
  for up to 30 s. A leader submits under a session of its own, asked as it
  comes to lead and again whenever it goes; its requests are numbered from a
  random start each time the member starts, so that a registration of an
  earlier start, applied again as the member starts over its log, is never
  taken for this start's (recover, which elects its member at once over the
  whole log, found it).
- **Raft's membership** follows the registry a change at a time, made by the
  leader: a removed host taken out, a new one in as a learner, a learner
  made a voter once it holds the log, a voter past the first five made a
  learner again. Asked to remove itself, the leader first hands the lead to
  another voter while every host still reaches it -- the removal drops the
  removed host's connections as each member applies it, and a group of two
  would lose the very connection the hand-over needs -- and the command,
  asked again, reaches the new leader.
- **ipcnet's peers**: the member tells ipcnet's control mailbox,
  `ipcnet.ctl` (`ipcwire::control`, a `Host` name), the peers the registry
  gives this host -- every other host and every open invitation -- whenever
  they change; ipcnet writes hosts.toml (a new file, synced, renamed over
  the old) and gives the running server the list: one writer of the file.
  A host added is pinned at once, a host removed has its connections closed
  at once. A list ipcnet refuses is said in the log and told again ten
  seconds later.
- **`init`** starts the member over a new log with this host its one voter,
  commits `Init`, and only then writes `member`; should any of it fail, the
  member is stopped and its log and `member` removed, for an `init` again --
  nobody else knows of the cluster yet. A `join` that fails after the leader
  committed it says what to do: a `member` that could not be written leaves
  the host in the registry and not running, to be removed and invited again;
  a member that did not start starts at the next `insmod`.

## Tested

- **`fuzz/raft`'s `registry` target** (docs/testing.md): the state machine
  against a model of its own, alone and as a group through `raft::rsm` over
  a lossy network, with scenarios and six mutants.
- **net-fuzz's `ipcnet` target**: the control mailbox's requests among its
  other steps -- who this host is, lists of peers, another cluster's, this
  host among them, noise, and the world's host made a host invited and a
  host again -- held to hosts.toml saying what was asked, a refusal leaving
  it as it was, the peer kept with its connections either way, and, while
  invited, nothing of its reaching the sink but by the sink's name and no
  monitor of its taken. Four bugs planted in ipcnet each fail it: a message
  delivered past the restriction, a monitor taken, a restriction lifted by
  dropping the peer, a peer made anew.
- **`scripts/cluster-test.py`** (both arches, three nos on
  `scripts/hub.py`'s LAN): a cluster made on one host; two invited and
  joined, one through a member that does not lead, after a join with a wrong
  secret refused, and while invited its pings to a member's echo refused;
  the provider answering through the bus; maintenance and an address seen
  by every host; a rekey under 6000 pings, every one answered and no
  connection lost, the old key refused after; every host rebooted at once;
  recover on the last of three, refused first, and a host installed anew
  joined after; every host up to every other and pings answered over the
  connections the registry made; a second `init`, another host's token and
  a token used again refused; a host removed and dropped by the others
  within seconds; a member's VM restarted, back from its folder; the leader
  of two removed -- the lead handed on first -- and its key refused when
  invited again.

## Limits

- One cluster a host; at most 16 hosts and open invitations, five voters.
- A removed host may not hear of its removal: the others drop its
  connections as they apply it, maybe before it has. Its `cluster` then
  says it has no leader -- or that it is removed, if it applied the entry.
  `rmmod cluster` there.
- A command whose answer was lost, asked again, may be refused for what its
  first try did -- an invitation there already, a host gone already:
  `cluster hosts` says what is.
- A join whose answer was lost -- a leader changing as it was taken, a
  connection cut -- says so: it may have been made, which `cluster hosts`
  on a member says; if the host is there, `cluster remove` it and invite it
  again. A host is never answered its node a second time: one whose folder
  was wiped would come back under its old node with nothing of what it had
  voted for.
- An invitation's fifteen minutes are the leader's clock.
- A list of peers that drops one waits, in ipcnet, for that peer's dialer
  to end -- up to an ARP and a connect's 5 s.
- A rekey is a rotation, not a revocation: connections made with the old
  key go on until they end.
- `recover` cannot tell a majority gone from one cut off: it is the admin's
  to know.
- An address for the `ipc` role is never taken away, only changed: a host
  is always somewhere for the others to call.
