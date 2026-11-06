# cluster: the registry of a nos cluster's hosts

Asked 2026-11-05 and decided by the user: right after the raft crate (R3)
and before NBS, one place for a cluster's hosts -- their keys, addresses and
roles -- so that a host is let in once and every other host learns of it,
instead of `hosts.toml` pinned by hand on every host ([ipc.md](ipc.md)). It
is the raft crate's first user ([raft.md](raft.md)): a small state, on which
Raft first meets the kernel, the files and ipc before NBS's metadata does.

## Where it starts

- `ipcnet` (ipc.md, I4) pins its peers from `/etc/ipc/hosts.toml` -- name,
  address, key fingerprint, the cluster's id -- written by hand
  (`ipc host add`) on every host. A cluster of n hosts takes n - 1 of them on
  each; a host removed or a key changed has to be told to every one.
- The raft crate (R1 to R3): the protocol, its wire, a durable log, and a
  replicated state machine with sessions and reads.
- The host API's keys (`/etc/api/`, its clients and roles) are another
  matter: people and tools, not hosts. They stay as they are.

## What it is

- `cluster.ko`, a module on every host of the cluster, running the raft
  crate's `rsm` with a state machine of its own. Its log lives under
  `/etc/cluster/` and its messages go over ipc as `cluster.raft`, a
  `Cluster` name.
- **Its state**: the cluster's id and name; the hosts, each with its key's
  fingerprint (two while a key changes), its addresses, its labels and
  whether it is a member of this group; the hosts removed, kept as tombstones
  so that a removed key is never taken back by an old copy of the list; and
  the invitations still open.
- **A host's addresses** are several, each saying what it is for. `ipc` is
  the one ipcnet calls. `underlay` is where a network of VMs across hosts
  will tunnel its packets ([ipc.md](ipc.md), "Later: VM networks across
  hosts"), and it may be a private network while `ipc` is not, or the other
  way round. `public` is an address the world reaches it at. A host's
  **labels** say what it is: `gateway` for one with public addresses to give
  VMs, `storage`, a rack. Kept from the first version, so that the state's
  shape need not change later.
- **A host's name** is the registry's own -- whatever `cluster invite` was
  given, the machine's hostname by convention -- and not a DNS name. Its
  addresses are IPv4 addresses, since nos's TCP has no other, and `cluster
  address` changes them. Nothing in ipc asks DNS, so the
  cluster keeps working when DNS is down or wrong. A DNS name as an address
  could come later: with the key pinned, a forged answer would be a refused
  handshake, not a way in, but the cluster would then depend on DNS being
  there.
- **The list it keeps for ipcnet**: on every change committed, each host's
  registry tells ipcnet's control mailbox, `ipcnet.ctl`, the peers it is to
  have, and ipcnet writes `hosts.toml` anew (new file, synced, renamed over
  the old) and gives its server the list -- one writer of the file. A host
  added is pinned at once; a host removed has its connections closed at
  once. After a reboot, ipcnet starts from that file, before the registry
  has caught up.
- **Its members**: every host of the cluster, the first five voters and the
  rest learners, as for NBS's mds.

## Commands

```
cluster init <name>             on the first host: a cluster of one
cluster invite <host> <ip> <key>   on a member: prints a token, once -- <key> is
                                what `ipcnet id` says on <host> (built so: "Joining")
cluster join <token>            on the new host
cluster hosts                   every host: address, key, roles, connected or not
cluster remove <host>           out, for good: its key refused everywhere
cluster revoke <host>           its open invitation taken back
cluster maintenance <host> on|off   a host about to be rebooted or worked on
cluster rekey                   on a host: a new key, without losing a connection
cluster address <host> <role> <ip|none>   a host that moved, or a new address of one
cluster recover [<cluster>]     on the last host standing (below): what it does, then -- the name given -- does it
```

A `cluster` provider on the bus, so that the panel lists the hosts and an
admin can invite one -- the token shown once, as the command shows it.

## Joining

The way kubeadm joins a node: a token that carries a secret, and the
member's key, which the joiner pins.

**As built (C1c), the joiner's key is given at the invitation**: `cluster
invite <host> <ip> <key>`, the key read with `ipcnet id` on the new host,
and every member's ipcnet pins it while the invitation is open. Steps 2 and
3 below were the first design and are not what was built: ipcnet takes no
client whose key nobody pinned, for any mailbox -- every connection is
pinned both ways, so there is no state before authentication to flood and no
handshake from an unknown key is ever answered -- and the secret, sent only
inside a TLS session bound to both keys, needs no HMAC over them. The member
takes the join request only from the host the invitation names, as ipcnet
authenticated it (`kernel_ipc_peer_name`), and only with the secret whose
hash the invitation keeps. What step 2 promised is kept: until it has
joined, the host invited reaches one mailbox of a member's, its
`cluster.raft`, by that name -- no other, none by address, no monitor --
held so by ipcnet (`only=` on its peer's line of hosts.toml), and lifted in
place, no connection lost, when the join makes it a host. The cost: the new
host's key carried to the inviting member, before the invitation rather
than after. docs/cluster.md has the join as it runs.

1. **`cluster invite ax41 203.0.113.7`** on any member commits an
   invitation: the host's name, the address it will come from, the hash of a
   secret, and when it expires -- 15 minutes on, by the proposing leader's
   clock, written into the entry so that every member decides alike. It
   prints the token: the cluster's id, this member's address and key
   fingerprint, the invitation's id, and the secret. The secret is never
   stored, only its hash, so neither the log nor a snapshot holds a token
   anyone could use.
2. **While it is open**, the members' ipcnet take SYNs from that address --
   from nowhere else that is not on the list -- and from it a TLS client
   whose key is not pinned yet. That client reaches one mailbox,
   `cluster.join`, one connection at a time, and nothing else.
3. **`cluster join <token>`** on ax41 makes its key, connects to the member
   with the member's key pinned from the token, and in TLS proves the secret:
   an HMAC over both keys' fingerprints and a nonce of the member's, so that
   it holds for this connection alone. The member hands it to the leader,
   which commits the host -- with the key the connection showed -- and the
   invitation used, in one entry. A token is good once.
4. **Every member's ipcnet pins ax41** as it applies that entry. ax41 is sent
   the registry's state, pins every host, and becomes a learner of the
   registry, then a voter if there are fewer than five.

A token used again, expired, presented from another address or for another
host's name, or with a wrong proof, is refused and counted. So are joins
past the rate a pre-auth path is held to (ipc.md, "Floods"): one at a time
an invitation, three tries.

## Leaving, keys, addresses

- **`cluster remove ax41`** commits a tombstone: every member drops ax41's
  connections as it applies the entry, and refuses its key from then on, even
  if an old `hosts.toml` somewhere still names it. Services hear of it as
  `HostDown`. NBS refuses a remove of a host its data is on until that host
  is drained (nbs.md).
- **`cluster maintenance ax41 on`** says a host is being worked on, for
  every service to read alike: NBS holds off counting it out, and the VM
  scheduler places nothing new there ([vmctl.md](vmctl.md)). Off again
  when it is back.
- **`cluster rekey`** on a host commits its new key beside the old one, then
  switches to it, then commits the old one's removal: no connection is lost.
- **`cluster address`** for a host that moved, or that gets an address for
  another role: committed, and every member uses the new one.

## When the majority is gone

The registry commits nothing without a majority of its voters: a minority
keeps the last list it had and goes on with it, and adds or removes no one.
If the majority is gone for good -- two of three hosts dead -- `cluster
recover` on a survivor makes a new configuration of itself alone. That is
unsafe by design, as etcd's `--force-new-cluster` is: anything the lost
hosts had committed and it had not is gone. It is refused while any other
voter answers, and says what it is about to do.

## How NBS uses it

- `nbs host add ax41` takes a host the registry has, with the key the
  registry has for it: NBS needs no invitation of its own, and joining NBS is
  that one command on a member.
- ax41's mds, with no state yet, takes a first contact from an NBS leader on
  a host the registry has, and from nowhere else. A wiped host still comes
  back as a new member with a new incarnation (nbs.md, "Who is in the
  cluster").

## Tested

- **`fuzz/raft`'s `registry` target**: the state machine against a model of
  its own -- hosts, tombstones, invitations and their expiry, rekeys,
  addresses -- run on several members over the simulated network that the
  `node` target uses.
- **net-fuzz's `ipcnet` target**: as built there is no join path in ipcnet
  to fuzz -- a key nobody pinned is never let in -- so the target holds the
  control mailbox instead: who this host is, lists of peers, another
  cluster's, this host among them, noise; hosts.toml saying what was asked,
  a refusal leaving it as it was. The join's own refusals -- a wrong secret,
  another host's token, a token used again or expired -- are the registry
  target's and `cluster-test.py`'s.
- **`cluster-test.py`** (both arches, on `scripts/hub.py`), C1 as built:
  - a cluster made on one host, two more invited and joined -- one through a
    member that does not lead -- and every host reaching every other;
  - a wrong secret, another host's token, a token used again and a second
    `init` refused;
  - a host removed and dropped by all within seconds;
  - a host rebooted, starting from its folder and its `hosts.toml`;
  - the leader of two removed, the lead handed on first, its key refused
    when invited again.
  And, with C2: the provider through the bus; maintenance and an address
  seen by every host; a rekey under 6000 pings with every one answered and
  no connection lost, the old key refused after; every host rebooted at
  once; two of three killed and `cluster recover` on the last -- refused
  while the majority answered -- and a host installed anew joined after.

## Steps

**C1, the registry.** Done, in parts. C1a: the state machine as a crate
of its own with no kernel in it, `src/crates/registry` -- the state, the
commands (init, invite, join, remove, revoke) and their answers, every
command carrying the proposer's clock and every invitation past its time let
go of at the entry, the snapshot and the rules one read back is held to, the
voters' policy (the first five to join) -- and `fuzz/raft`'s `registry`
target with its scenarios and six mutants (docs/testing.md). C1b:
`cluster.ko` -- the member as a task over raft's `rsm` and a `DiskLog` in
/etc/cluster, raft's messages over ipc, commands handed to the leader,
raft's membership kept to the registry's -- and ipcnet's control mailbox,
`ipcnet.ctl`, through which the registry gives ipcnet its peers and ipcnet
alone writes hosts.toml. C1c: the join, with the joiner's key given at the
invitation (above), and the channel's word on which host a message came
from, `kernel_ipc_peer_name`. C1d: `cluster-test.py`, docs/cluster.md.

**C2, the rest.** Done. The registry's `Maintenance`, `Address`, `AddKey`
and `DropKey` -- a key dropped retired for good, as a removed host's is --
with the registry target's model, three scenarios and three mutants;
`raft::recover::force_alone` and its scenario and two mutants; ipcnet's
peers changed in place by their keys, its own key staged and switched
(`tls/identity.pem`, `tls/next.pem`, `TAG_KEY_STAGE`, `TAG_KEY_USE`);
`cluster maintenance`, `address`, `revoke`, `rekey` and `recover`; the
`cluster` provider and its descriptor, which the panel builds its page of.
As built, `recover` also removes every other host from the registry --
their keys refused for good -- since what it forced may differ from what
they hold (docs/cluster.md, "When the majority is gone"); and a rekey keeps
every connection, those made with the old key going on until they end: a
rotation, not a revocation, a key thought stolen being answered by
`cluster remove`.

## Limits

- A host is in one cluster at a time.
- An invitation's expiry is the proposing leader's clock: hosts whose clocks
  differ by minutes see a token's quarter of an hour a little longer or
  shorter.
- `recover` is manual and can lose what the lost hosts alone had committed.
- The bootstrap is by hand: the first host's `init`, the new host's key
  carried to the inviting member, and the token carried back to the new host
  by whoever runs the join.
- A removed host may not hear of its removal: the others drop its
  connections as they apply it, maybe before it has applied it itself.
