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
  registry writes `hosts.toml` anew (new file, synced, renamed over the old)
  and tells `ipc.net` to read it. A host added is pinned at once; a host
  removed has its connections closed at once. After a reboot, ipcnet starts
  from that file, before the registry has caught up.
- **Its members**: every host of the cluster, the first five voters and the
  rest learners, as for NBS's mds.

## Commands

```
cluster init <name>             on the first host: a cluster of one
cluster invite <host> <ip>      on a member: prints a token, once
cluster join <token>            on the new host
cluster hosts                   every host: address, key, roles, connected or not
cluster remove <host>           out, for good: its key refused everywhere
cluster maintenance <host> on|off   a host about to be rebooted or worked on
cluster rekey                   on a host: a new key, without losing a connection
cluster address <host> <role> <ip>   a host that moved, or a new address of one
cluster recover                 on the last host standing (below)
```

A `cluster` provider on the bus, so that the panel lists the hosts and an
admin can invite one -- the token shown once, as the command shows it.

## Joining

The way kubeadm joins a node: a token that carries a secret, and the
member's key, which the joiner pins.

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
- **net-fuzz's `ipcnet` target**, with the join path: clients with no
  token, a wrong secret, a token used again or expired, from another
  address or for another name, and floods of joins. It is held to letting
  in exactly the hosts that hold good tokens, once each, and nothing past
  `cluster.join` before that.
- **`cluster-test.py`** (both arches, on `scripts/hub.py`):
  - a cluster made on one host, two more invited and joined, and every host
    reaching every other;
  - the bad tokens refused;
  - a host removed and dropped by all within seconds;
  - a rekey with no connection lost;
  - every host rebooted, each starting from its `hosts.toml`;
  - two of three killed and `cluster recover` on the last.

## Steps

**C1, the registry.** Built in parts. C1a, done: the state machine as a
crate of its own with no kernel in it, `src/crates/registry` -- the state,
the commands (init, invite, join, remove, revoke) and their answers, every
command carrying the proposer's clock and every invitation past its time let
go of at the entry, the snapshot and the rules one read back is held to, the
voters' policy (the first five to join) -- and `fuzz/raft`'s `registry`
target with its scenarios and six mutants (docs/testing.md). Then:
the state machine's module, `cluster.ko` and its commands,
`hosts.toml` written and `ipc.net` told; ipcnet's join path (the address
admitted while an invitation is open, the client key taken unpinned for
`cluster.join` alone, the proof); the `registry` fuzz target, the join cases
in net-fuzz's `ipcnet`, `cluster-test.py`.

**C2, the rest.** `rekey`, `address`, `maintenance` and `recover`; the
provider and the panel's page.

## Limits

- A host is in one cluster at a time.
- An invitation's expiry is the proposing leader's clock: hosts whose clocks
  differ by minutes see a token's quarter of an hour a little longer or
  shorter.
- `recover` is manual and can lose what the lost hosts alone had committed.
- The bootstrap is by hand: the first host's `init`, and the token carried
  to the new host by whoever runs the join.
