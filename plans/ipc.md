# ipc: named mailboxes, one channel for modules, tasks and hosts

Asked 2026-11-05, while planning NBS ([nbs.md](nbs.md)): message queues in the
kernel, named in a namespace public to the host, found by discovery, so that
modules and tasks find each other's ends and talk -- as in Erlang -- and there
is one channel of communication rather than one per service. NBS's services
(`nbs-mds`, `nbs-cs`, the client in `hv`) are its first users, on one host
and between hosts.

## Where it starts

- **Modules cannot call one another**: each is linked on its own against the
  kernel's export table (docs/modules.md). What they share today is the shell
  (`kcore::cmd::dispatch`), files, the log, and the management bus
  (`src/crates/mgmt`): providers by name, JSON calls and jobs, at most 32
  providers and 256 KiB an answer -- made for an administrator's requests,
  not for a service's traffic.
- **No loopback**: the network stack has no `lo` and does not deliver to the
  host's own address, so two modules on one host cannot even reach each other
  by TCP.
- **Between hosts**: TCP has 64 connection slots for the whole kernel
  (`MAX_CONNECTIONS`), 8 KiB windows unless bulk (512 KiB, on a listener's
  accepted connections only -- `kernel_tcp_connect` has no bulk form), an RTO
  that starts at 1 s and is not taken from the measured round trip, a 200 ms
  timer, no SACK and no keepalive. TLS is rustls: a client verifying against
  the Mozilla roots (wget) with no certificate of its own, and the API's
  server pinning client keys (`tls::server`, `Pinned`). No client pins a
  server's key.
- **What a channel is made of, already there**: `kcore::sync::Event` (one
  waiter, signalled from anywhere, hard IRQs included), `kcore::ring::
  LocklessRing`, handle tables looked up and never dereferenced (block
  devices, open files, NICs), the bus's way of unregistering a provider by
  waiting out its calls.

## The model

```rust
// a server
let mb = Mailbox::new(Limits { messages: 256, bytes: 16 << 20 })?;
mb.register("nbs.cs", Scope::Cluster)?;      // this host's namespace; Cluster: its peers' too
loop {
    let Some(msg) = mb.recv(Duration::from_secs(1)) else { continue };
    match msg.tag() { /* the protocol's */ }
    msg.reply(&mb, TAG_DONE, &answer)?;      // to msg.from(), with its ref
}

// a client
let mds = ipc::lookup("nbs.mds")?;            // on this host
let cs = Name::on("ax41", "nbs.cs");          // on another, by name
let answer = ipc::call(&mine, &cs, TAG_READ, &request, Duration::from_secs(5))?;
```

- **Mailbox**: a bounded queue of messages with one owner, who receives from
  it, one task at a time. Made by `Mailbox::new`, closed when dropped. Its
  address is `Addr { host, slot, generation }`: host 0 is this one, and the
  slot and its generation are looked up in the mailbox table, so a closed
  mailbox's address is `Gone` -- never another mailbox's.
- **Name**: one mailbox's, unique on its host, registered by its owner and
  taken away when the mailbox closes; how names look and whose they are is
  below. The namespace is public: `lookup(name)`, `list(prefix)`, and
  `watch(prefix)` -- a `NameUp` or `NameDown` message for every change, so a
  client that starts before its server learns when it comes and when it
  goes. A name's scope is `Host` (this host's alone) or `Cluster` (reachable
  from the cluster's hosts too, below).
- **Message**: `from`, a `tag` (u32: what it is, the protocol's to define), a
  `ref` (u64: which request a reply answers), and up to 1 MiB of payload --
  a guest's largest request is 256 KiB (`blk::REQUEST_BYTES`) -- held in
  pages of the channel's pool, a page a piece, so no allocation is ever larger
  than a page and the heap's 512 KiB ceiling never comes into it. Copied in
  at `send` and out at `recv` in the first version.
- **Send**: to an address or a name, this host's or `Name::on(host, name)`.
  `Ok` once it is in the mailbox, or handed to `ipcnet` for another host;
  `Full` when the mailbox is at its limit -- backpressure, try again;
  `Gone`; `NoRoute` (no such host, or `ipcnet` not loaded); `TooLarge`;
  `NoMemory` (the pool at its cap). Never blocks unless asked to
  (`send_wait(timeout)`).
- **Receive**: `recv(timeout)`, `try_recv()`. A task that waits for several
  things waits for one `Event` they all signal: a mailbox can be made on an
  event of the task's, and a block or image queue opened on the same event
  (one change to `kernel_blockq_open` and the image queue's open), so that a
  chunk server's worker sleeps on one thing and wakes for a request or a
  completion alike.
- **Monitor**: `mb.monitor(addr)` puts exactly one `Down { addr, reason }`
  into `mb` when `addr` closes -- `Closed`, `Unloaded` (its module went),
  `HostLost` (its host stopped answering), `NoSuch` (gone already when
  asked). A monitor is how a caller never waits forever for a peer that is no
  more.
- **Call**: kcore's helper over the rest: a fresh `ref`, a monitor on the
  target, the send, and a wait for the reply with that `ref`, the `Down`, or
  the timeout -- the three ways a call ends, each said.
- **System messages** have tags of their own: `Down`, `NameUp`, `NameDown`,
  `HostUp`, `HostDown`, and `Bounced` -- another host's refusal of a message
  (`Full`, `Gone`, a name it does not export), sent back to its sender.
- **Order**: messages from one mailbox to another arrive in the order they
  were sent. Between hosts that holds while a connection lasts; one that
  breaks loses what was in flight -- never reorders, never duplicates -- and
  every monitor across it fires.

What is Erlang's and what is not: mailboxes, registered names, monitors and
location-transparent sends are its; links, selective receive and unbounded
mailboxes are not here. A kernel cannot let one slow receiver take the
memory, so a full mailbox is an answer; a task that wants to wait for one
reply among others gives the reply a mailbox of its own.

## Names

```
nbs.mds            this host's metadata service                  Cluster
nbs.cs             this host's chunk service                     Cluster
nbs.cs.0           the worker of the first device in its pool    Cluster
hv.vm.web          vm web's control, on the host it runs on      Host
kernel.echo        the image's echo, for the tests               Host
ipc.net            ipcnet, the router to other hosts             Host

nbs.cs@ax41        a name on another host
ax41:17.3          an address there -- slot 17, generation 3: a client's
                   mailbox, or a reply's, has an address and no name
```

- **A name** is parts joined by dots, each a lowercase letter and then
  `a-z 0-9 _ -`, 48 bytes in all; its first part is its namespace.
- **A host's name** (`ax41`) is unique in its cluster: the name its key is
  pinned under. It is the registry's name for the host, not a DNS name.
  `nbs.cs@ax41` is looked up in the registry, which gives the key the host
  must show and the address to call; DNS is never asked. Its letters are a
  DNS label's (`a-z 0-9 -`, a letter first, up to 32), so that by convention
  it can be the machine's own hostname.
- **Namespaces are owned.** A module registers names under one namespace:
  its own name up to its first `-` -- `nbs-mds`'s and `nbs-cs`'s both `nbs`.
  The channel knows which module a caller is by an address in its code
  (`ModuleTable::name_at`), so nothing in the module's header says it, and
  the loader is not asked; `kernel`, `ipc` and `sys` are the image's. Two modules may share a namespace -- `nbs-mds` and
  `nbs-cs` both register under `nbs` -- but a name is one mailbox's, and a
  second registration of it is refused (`Taken`), loudly. That is against
  mistakes, not malice: a module is kernel code and can do anything
  (docs/modules.md).
- **A name is `Host`** unless its owner says `Cluster`.
- **Discovery of another host**: `list` sent to its `ipc.net` answers its
  `Cluster` names, at a rate a peer is held to; `watch_on("ax41", "nbs.")`
  gives a `NameUp` and a `NameDown` while connected, and a `NameDown` for
  each name when the host is lost. (A device's worker is named by its place
  in the pool, not by the device's name: `nvme0` can be another disk at the
  next boot.)

## Safe by design

- **Pull, never push.** The channel runs nobody's code: a message's arrival
  calls no handler, its receiver's own task takes it. No message can land in
  a module that is gone, nothing waits on a module's code, and unloading one
  is its mailboxes' drops.
- **Addresses are looked up**, slot and generation, never pointers; a stale
  one is `Gone`.
- **Everything bounded**: a mailbox's messages and bytes (its owner's
  choice, within the host's caps), the pool (a share of memory, a boot
  parameter), the names, the monitors and watches a mailbox may hold. `Full`
  and `NoMemory` are counted and visible in `ipc`.
- **No allocation under a lock**: a message's pages are taken before the
  mailbox's lock and given back after it; a queue is a list of messages
  already built.
- **Task context** in the first version: no send from an interrupt or a soft
  IRQ (a `try_send` of preallocated messages if a user ever needs one).
- **What a module leaves open is closed, loudly**: every mailbox records the
  module that made it, and an unload closes any it left and logs each as that
  module's bug.
- **No `unsafe` in a user**: `kcore::ipc` holds it at the C ABI, as
  `kcore::block` and `kcore::fs` do.

## In the kernel

- `src/crates/ipc`, in the image as the bus is: the mailbox table, the names,
  monitors and watches, the pool. The C ABI in `ipc/src/abi.rs`, declared in
  `ffi/src/ipc.rs`, wrapped in `kcore::ipc` (`Mailbox`, `Message`, `Addr`,
  `Name`, `call`); Rust in the image uses the crate itself. The C++ half
  needs none of it.
- `ipc` in the shell: names (and whose), mailboxes (owner, depth, limits,
  counters), and with `ipcnet` the hosts (connected, round trip, bytes,
  bounces). An `ipc` provider on the bus, read only, for the panel.
  Tracepoints `ipc_send`, `ipc_recv`, `ipc_down` (group `ipc`).
- At boot: `ipc::selftest` in `rust_test` (order, `Full`, `Gone`, a closed
  address never taken for a new mailbox, monitors, watches, a call), and
  `test_module_ipc` after it: modtest talks to an echo of the kernel's and is
  unloaded under traffic -- the kernel's side must see its `Down`, the pool
  must be back where it was.

## Between hosts: ipcnet

A module, `ipcnet.ko`: a host that is no cluster's needs none of it.

- **A host** has a name, unique in its cluster (`ex44`), and a key; its peers
  are in `/etc/ipc/hosts.toml`, pinned both ways (below).
- **One fixed port, 39240** (beside the API's 39234), on the NIC the config
  names -- eth0 by default, and never hv0: no guest reaches it.
- **TLS 1.3, pinned both ways.** Both ends are `tls::pinned`: the server the
  API's (`Server`), the client new in I3 (`Client`) -- the server's key
  pinned in place of the Mozilla roots, and a certificate of the client's
  own.
- **Two TCP connections a pair of hosts**: control (small messages -- Raft,
  monitors, metadata) and bulk (data), chosen by a message's class, so that a
  megabyte of data or a lost segment of it never stands in front of a
  heartbeat. The host whose name sorts first connects, the other accepts:
  never two of a kind.
- **The wire** is a crate with no kernel and no `unsafe`, `ipcwire`: a
  message's head, then its payload in frames of up to 16 KiB -- a TLS record
  -- so that a big message is cut up and other messages' frames go between
  its frames. Every length checked; fuzzed.
- **Routing.** The image's channel hands a message for another host to
  `ipcnet`'s mailbox, registered as the router (one at a time); `ipcnet`
  hands what arrives to the channel (`deliver_remote`, the router's call
  alone), with `from` the sending host's address as its TLS key proves it.
  Delivered only to a `Cluster` name, or to a mailbox that has sent to that
  host -- a reply's way back. Nothing else on a host is reachable from
  another.
- **Liveness.** Heartbeats on the control connection every 500 ms. Nothing
  for 3 s, or either connection broken, and the host is lost -- even if it is
  back a moment later, since what was in flight is gone: `HostLost` to every
  monitor of an address there, `HostDown` to every watcher, and a reconnect
  with backoff, after which it is `HostUp`.
- **Backpressure.** A mailbox full at the receiving host is a `Bounced` back
  to the sender, never a connection that stops reading; what waits to go to
  a host has its cap (`Full` at the send).
- **Measured before it carries data**: TLS 1.3 in software -- the kernel has
  no SSE, so ChaCha20-Poly1305 and AES-GCM run on their portable backends --
  on the EX44's and the AX41's cores, against the 117 MB/s of their 1 GbE
  links. An NBS write sends two copies between hosts. The AX41's, measured
  (`tlsbench` under KVM, 2026-11-05): ChaCha20-Poly1305 203 MB/s a way with
  sender and receiver on two CPUs, AES-128-GCM 92, AES-256-GCM 82;
  handshakes 1.2-2.4 ms. ipcnet's links ask for ChaCha20-Poly1305 first.
  The EX44's waits for it to run Ubuntu again.

### Who may send to whom

| Who | Reaches | Held to it by |
|---|---|---|
| the image's tasks, the modules | every name and address on the host, `Host` or `Cluster` | nothing but the checks against mistakes (namespaces, limits): they are kernel code |
| a guest | nothing | there is no way in: `ipcnet` listens on its NIC alone, never hv0, and a listener takes only what comes in by its NIC |
| another host | the `Cluster` names, and the mailboxes that have sent to it -- a reply's way back | hosts.toml: the SYN's address, then TLS 1.3 with both keys pinned, then the name the key is pinned under; and the service's own check of who it is |
| a person, a tool | nothing of ipc's | they come through the host API (api.ko) and its roles: ipc carries the services' traffic, not people's |

- **A host's key** is Ed25519, made at its first start from the kernel's
  random pool and kept in `/etc/ipc/host.key`, with a self-signed
  certificate (the `web` crate writes one). Its fingerprint, the SHA-256 of
  the key as the API's `Pinned` takes it, is the host: `ipc host id` prints
  it.
- **Peers** are in `/etc/ipc/hosts.toml`: each one's name, address and
  fingerprint (two while its key changes), and the cluster's id. The
  registry keeps it (below); without one, a peer is added on both hosts by
  hand (`ipc host add <name> <ip> <fingerprint>`), and `ipc host remove`
  takes it away and closes its connections. `ipc host
  rekey` makes a new key, and the old one is taken beside it until every
  peer has the new one.
- **The handshake** is TLS 1.3 alone, ALPN `nos-ipc/1`. The server takes a
  client whose key is pinned; the client takes a server whose key is pinned
  under the name it called. Then comes the first frame: both names and the
  cluster's id. A host of another cluster, or a key under another name, is
  refused there.
- **After it**, a message's `from` is the peer's host and mailbox, as the
  connection proves it -- never what the message says. A message to a `Host`
  name, or to a mailbox that never sent there, is dropped and counted.
- **The services check too**: `nbs-mds` takes Raft's messages only from the
  hosts of its Raft configuration, and `nbs-cs` takes I/O only from the NBS
  cluster's hosts, with the right attach generation. The transport proves
  who; the service decides what that one may do.
- **Everything is encrypted**: no plaintext mode in the first version.

**The cluster's hosts in one place: the registry** ([cluster.md](cluster.md),
decided: right after the raft crate, before NBS). Pinned by hand, a cluster
of n hosts takes n - 1 `ipc host add`s on each, and a host removed or a key
changed has to be told to every one of them. The registry does it once.

- Its hosts, their keys, addresses and roles are a small Raft group's state.
- A host joins with a one-time token from `cluster invite`: the joiner pins
  the member's key from the token and proves the token's secret inside TLS.
- Every member's ipcnet pins the new host as it applies the entry; a host
  removed is refused everywhere the same way.
- `hosts.toml` becomes the registry's copy, written on every change and
  read by ipcnet after a reboot. `ipc host add` by hand stays for a host
  with no registry, and for I4's tests before the registry exists.

### Floods

No host stops a flood that fills its link: a 1 Gbit line is full before a
packet reaches nos. That is stopped upstream. Hetzner filters attacks on its
network, and its firewall in front of each server -- 39240 open to the
cluster's hosts alone -- is the first thing to set. What nos does is make a
flood cost no more than its packets, and keep it from anything else:

1. **No slot before the address is checked.** `ipcnet`'s listener answers
   the addresses in hosts.toml alone: a SYN from any other is dropped
   without a slot, a SYN-ACK or an RST -- silently. Today a listener takes
   the connection and resets it after (the API's `admit`), a slot taken for
   a moment each time; the check moves into TCP's listen path, between
   `find_listener` and the backlog ([tcp.md](tcp.md), T3), and the API can
   take it too.
2. **Spoofed SYNs from a peer's address** never finish the handshake. A
   backlog of 2 and a short SYN-RECEIVED timeout bound them, and the
   connections already up go on: a flood can only delay a reconnect.
3. **The crypto rationed.** At most two TLS handshakes at once, one a
   source, each given 3 s. A refused one ends with an RST, so no TIME-WAIT
   holds a slot (the API's lesson). A handshake is a few ms of CPU, and no
   more than two are ever being paid for.
4. **A peer's share.** What a peer may have waiting -- to send, and received
   but not yet delivered, in bytes and in messages -- the monitors and
   watches it may hold, and its lists and lookups a second are all held to
   limits. Frames are held to 16 KiB and messages to 1 MiB; a kind nobody
   knows is dropped and counted. Peers are served in turn and control before
   bulk, so no peer starves another and no data starves a heartbeat. A full
   mailbox is a `Bounced`, never a connection that stops reading.
5. **Slow and silent.** Nothing for 3 s and a peer is lost: a connection
   held open and quiet holds nothing for long.
6. **No reflection.** There is a TCP handshake before anything, so `ipcnet`
   cannot be used to amplify -- unlike a UDP request answered with more than
   it brought (netblk's READ: 70 bytes in, 16 KiB out).
7. **Locally**: mailboxes and the pool are bounded, and what a module's
   mailboxes hold is charged to that module, up to a cap: a runaway module
   fills its own share, not the host's.
8. **Seen**: a counter for each -- SYNs dropped by source, handshakes
   refused, bounces, limits met -- in `ipc hosts`, and a line in the log about
   them at most every few seconds, so that a flood does not flood the log.

Left to the network layer: what the stack sends a stranger anyway. Every
segment for a port nobody listens on is answered with an RST, one each,
with no limit; limiting their rate is `net`'s item, not ipc's.

### TCP, not UDP

The question was asked for NBS's traffic. TCP with TLS, for everything:

- What it brings that is in the tree and fuzzed already: delivery in order
  with nothing lost or doubled while a connection lives, congestion control
  (RFC 5681/6582), window scaling, messages of a megabyte without a
  fragmentation of our own, and rustls running over it.
- What UDP would bring is netblk's numbers -- the 1 GbE line rate, zero copy
  (docs/netblk.md) -- and no head-of-line blocking. What it would take: a
  retransmission of our own, and a way to recognise a duplicate (a delayed
  copy of an old write that lands after a newer one is silent corruption),
  congestion control, cutting a megabyte into datagrams and putting it back
  together, and crypto (there is no DTLS or QUIC in the tree). All of it new
  code facing the network, the most dangerous kind there is.
- So TCP now, with the wire independent of what carries it. A UDP path for
  the bulk class, on a private link, is worth it only if the numbers show TCP
  as the limit.

What TCP has to get first is a plan of its own, [tcp.md](tcp.md):

- bulk buffers both ways on an active open, a module's included (T1);
- a timeout from the measured round trip, a timer armed for the earliest
  deadline, and a connection given up by time rather than by count (T2) --
  with the floor Linux has, 200 ms, for any peer, and ipcnet's links, both
  of whose ends are nos and acknowledge every segment at once, given 20 ms;
- strangers' SYNs dropped before they take a slot, half-open connections
  timed out, and the resets to closed ports held to a rate (T3).

## Steps

**I1, the channel in the image** -- done on branch `ipc-raft`
(docs/ipc.md). The `ipc` crate, the ABI and `kcore::ipc`; the `ipc` command
and kernel.echo; the selftest and `test_module_ipc`. Its tracepoints and its
provider on the bus wait for I4, where there are hosts to show. `fuzz/ipc` (host, in CI): the crate as it is, over the
fuzzers' common machine (`fuzz/common/`, tasks one at a time, the lock rules
checked), held to a model -- every message sent `Ok` received once, in order,
unless its mailbox closed first; every monitor firing exactly once; names
unique; the pool back to empty when every mailbox has closed, and never past
its caps. Gates: the smoke boots on both arches, UB builds too; `fuzz/ipc`;
`sshd-test.py` (the ffi digest); `cpp-fuzz.py --target module`.

**I2, one event to wait on.** A mailbox, a `DiskQueue` and an `ImageQueue`
made on a task's event. Gates: `netblk-test.py`, `hv-test.py`,
`hv-device-test.py`, `fs-fuzz.py`.

**I3, TCP and TLS for hosts.** TCP's T1 to T3 ([tcp.md](tcp.md)), each
measured before and after on the AX41 and the EX44; `tls`'s pinned client
with a certificate. Done on branch `ipc-raft` but for the measurements on
the boxes: `tls::server` became `tls::pinned` (feature `pinned`), one
session for both ends, and `Client` beside `Server`.
Gates: `net-fuzz.py` (`tcp`, `tls_server`, `tls_client`), `tcp-test.py`, the
`tls --features pinned` build for both kernel targets, `api-test.py`.

**I4, ipcnet.** The module and `ipcwire`; net-fuzz's `ipcnet` target (the
module's source against hosts with the right key, a stranger's and none,
breaking the wire and TLS -- held to no delivery to a `Host` name from
outside, nothing delivered twice or after its `Down`, memory bounded);
`ipc-test.py` (both arches), the first user of a harness NBS reuses:
`scripts/hub.py`, a learning Ethernet switch over QEMU's UDP socket netdevs
that drops, delays and cuts, and two or three nos on it. It checks keys
pinned both ways; a stranger's SYNs dropped with no slot taken, and the
other services still answering; a stranger with an address on the list
refused in the handshake, two handshakes at a time at most; a key pinned
under another name refused; a `Host` name unreachable from a peer; order;
`Bounced`; a peer at its limits slowed while another goes on; `HostLost`
within 3 s of a host's QEMU killed; `rmmod ipcnet` under traffic. TLS's cost
measured on the EX44 and the AX41.

### How I4 is built (a design to confirm before it is)

Written after I1 and I3, from what the channel turned out to be. The
pieces, in the order they are to be built and gated:

- **A peer's number.** ipcnet gives each peer it pins a number of this
  host's own, 1 and up (`Addr.host`); 0 is this host, as in I1. An address
  with a peer's number is a mailbox there, as this host knows it. On the
  wire an address is always the host's that owns it -- host 0 -- and
  ipcnet rewrites: a `to` going out loses its number, a `from` coming in
  gets the sender's. So no host's numbering ever leaves it, and a peer
  renumbered after a reboot breaks nothing but the addresses held of it,
  which its `HostLost` ended anyway.
- **The router** (ipc crate). `ipc::route(&mailbox)` makes one mailbox --
  ipcnet's -- the router; there is one or none. A send to an address with a
  peer's number, or to `name@host` (the host's name as ipcnet pinned it),
  goes into the router's mailbox as a message of the channel's own tag
  (`TAG_ROUTE`) whose payload begins with where it is going and who sent it.
  The router's limits are the host's cap on what waits to go out: `Full` at
  the send, as the plan says. With no router, `NoRoute`, as now.
- **What comes in** (ipc crate). `ipc::deliver_remote(&router, from, to,
  tag, reference, payload)`, the router's call alone, delivers to a
  `Cluster` name, or to a mailbox that has sent to `from`'s host: the
  channel keeps, for each peer, the mailboxes that sent there -- bounded,
  dropped when a mailbox closes or the peer is lost. Anything else is
  refused and counted, never delivered: a `Host` name is unreachable from
  outside. A full mailbox is `Full` to the router, which sends a `Bounced`
  back.
- **Monitors across hosts** (ipc crate and ipcnet). A monitor of an
  address on a peer is kept, with its `Down` made and its place kept, on
  the watcher's host, as every monitor is; the channel tells the router
  (`TAG_ROUTE` again), which asks the peer to watch. The peer's router
  monitors the mailbox with a monitor of its own and sends `Down` back when
  it goes; `ipc::remote_down(&router, host, id, reason)` hands over the
  kept `Down`. `ipc::host_lost(&router, host)` ends every monitor of an
  address there with `HostLost` -- on the watcher's host alone, so it needs
  nothing from a host that is gone -- clears that peer's sent-to set, and
  tells the watchers of `host.<name>` (`HostDown`; `HostUp` when it is back).
- **The wire** (`ipcwire`, no kernel, no `unsafe`): frames of an 8-byte
  head -- kind, flags, length -- and at most 16 KiB in all. `Hello` (the
  version, the cluster's id, both names; first, after TLS), `Ping`/`Pong`
  (500 ms), `Head` (a message: its number on this connection, `to` -- an
  address or a name -- `from`, tag, reference, length up to 1 MiB) and
  `Body` (its bytes, a frame at a time, other messages' frames between),
  `Bounced`, `Monitor`, `Demonitor`, `Down`. The decoder takes bytes as they
  come and gives frames whole, every length checked before anything is
  kept; the assembler puts messages back together, a bounded number at once
  and a bounded number of bytes, and refuses a frame of a message it has
  not been told the head of. Fuzzed on the host: anything decodes or is
  refused, never a panic; whatever is encoded decodes to itself.
- **The module** (`modules/ipcnet`): the host key and certificate made at
  its first start (`web::x509`), hosts.toml read with `conf`, a listener on
  39240 with the peers' addresses as its allowlist (T3's `listen_allowed`,
  given a C ABI then), a task a connection, the host whose name sorts first
  connecting -- with the 20 ms RTO floor, the other half of T2, given to its
  connections alone -- two connections a peer, the `Hello`, heartbeats and
  the 3 s loss, `Bounced`, the shell's `ipc host` commands and `ipc hosts`.
- **Its gates**: net-fuzz's `ipcnet` target and `ipc-test.py` on
  `scripts/hub.py`, as the step says.

Confirmed by the user (2026-11-05). Written on branch `ipc-raft`: the wire
(`ipcwire`, fuzzed by `ipc-fuzz.py --target wire`), and the channel's side
of the router -- `remote.rs`: the peers' numbers and epochs, what is routed
and its head (`ipcwire::route`), `deliver_remote`'s answer rule, monitors of
peers' mailboxes and `HostLost`, the router's close -- for the image's tasks
and through the C ABI for a module (`kcore::ipc`), held by the fuzz model
and a self-test at every boot; and the TCP ABI a module needs for it
(`kcore::tcp`: `TcpListener::bind_allowed` and `set_allowed`, T3's
allowlist, and `TcpStream::set_rto_floor`, the link's 20 ms, T2).

And then the module, `modules/ipcnet` (docs/ipc.md, "Between hosts:
ipcnet"), with what it needed of the channel: a bounce told back to a
sender (`TAG_BOUNCED`), the tag's bulk bit, a message received in pieces --
a module's heap gives 512 KiB at once, and the router is handed messages of
a megabyte -- and `deliver_remote`'s payload in pieces; a router's close no
longer heard by itself, which the fuzz model found. `ipcwire`'s Hello
carries the session, its assembler a bound on its pieces. `ipc-test.py` on
`scripts/hub.py`'s LAN passes on arm64 under HVF; what it found was the
sender's own, two messages of one pair begun at once and delivered out of
order, which it now never does. The links' polling is gone
too: TCP's receive waits on an event of its connection's, which the
receive path signals, and which a `TcpWaker` -- what ipcnet's queues hold
-- ends early; a small message's round trip went from 7 ms at the median to
0.4 ms on hub.py's LAN. net-fuzz's `ipcnet` target (docs/testing.md)
holds the module's source against a world that is the other host -- right
keys and wrong, Hellos right and wrong, messages, bursts into a full
mailbox, monitors, silence, a broken wire -- and catches nine of ten bugs
planted in it. `ipc-test.py --arch x86_64` passes under TCG, from an ISO
whose command line has the UDP shell (`make nos-test.iso
TEST_ISO_ARGS=udpshell=9100`). Left of I4: the measurements on the boxes.

## Limits of the first version

- Copies: a payload is copied in and out, and into TLS records between
  hosts. Pages that move instead -- to a block queue's DMA, to a NIC -- come
  when a number asks for them.
- No send from interrupt context.
- Names are per host. A registry for the whole cluster (Erlang's `global`)
  only if something needs one; NBS does not, since its mds knows where
  everything is.
- Hosts are pinned by hand until the registry keeps the list, with its
  join tokens ([cluster.md](cluster.md)).
- The management bus stays as it is. It could become a protocol over the
  channel later -- a panel calling every host's providers through `ipcnet`
  -- but not in this plan.

## Later: VM networks across hosts

Asked 2026-11-05: VMs on different hosts on one private network, some of
them with a public address. That is a task of its own, after these plans.
What it will stand on, and what is kept ready for it now:

- **Its packets do not go over ipc.** A guest's packets inside ipc's TCP
  would be TCP inside TCP: a loss on the link between hosts stalls every
  guest connection behind it, and two retransmission timers fight. The data
  goes as UDP instead, one encapsulated frame a datagram (Geneve's shape),
  each sealed with an AEAD (ChaCha20-Poly1305), on a port of its own:
  39241/udp, kept for it now. Storage's RPCs want what TCP gives; a tunnel
  of packets must not have it, since the guest's own TCP does that work.
- **Its keys come over ipc.** One of two hosts makes their data keys and
  hands them to the other over their ipc connection, which TLS has proved
  already. So the tunnel needs no handshake protocol of its own; a host the
  registry removes loses its keys with its connections; keys are changed
  by time and by bytes.
- **Its state is the raft crate's third user** ([raft.md](raft.md)):
  networks, subnets, ports (a VM's NIC, its addresses, the host it is on),
  routes and public addresses, in a Raft group of its own. Where every VM
  runs is wanted both here and by a cluster-wide register of VMs, so the two
  may well be one.
- **Its addresses**: each host has an `underlay` address in the registry
  beside its `ipc` one (cluster.md), so the tunnel can run on a private
  network while ipc runs where it likes.
- **What `net` needs first**: VLANs (802.1Q), since a Hetzner vSwitch is a
  tagged VLAN on the server's NIC and the stack sends and takes no tag today;
  and an MTU per device, since a vSwitch's is 1400 and TCP's MSS is fixed at
  1460. Both help NBS as well: its traffic off the public interface.
- **Public addresses**: a public IP lives on a gateway host, wherever
  Hetzner routes it -- an additional IP or subnet, or a failover IP, which
  Hetzner's API moves between servers and nos's HTTPS client can call.
  There it is NATed one to one onto the VM's private address and tunnelled
  to the VM's host. NAT today masquerades port by port and keeps guests off
  the private ranges (`nat.rs`'s `kept`). A one-to-one mapping, and the
  overlay's ranges let through to the tunnel instead of the world, are what
  it gains.
- **The switch** (`modules/hv/src/net.rs`) gets ports that live on other
  hosts: a frame for one goes into the tunnel. ARP and ND for them are
  answered locally, and its DHCP server gives the network's MTU (option 26),
  which it gives none of today.
- **And live migration** later: a VM's port moving to another host is one
  change in that state, and its disk, on NBS, does not move at all.
