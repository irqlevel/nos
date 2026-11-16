# vnet: VM networks across the hosts of a cluster

Asked 2026-11-15: VMs on different hosts of a nos cluster on one private
network, with private addresses, NAT to the public internet, and a public
address for a VM that wants one -- an additional IP or a subnet rented from
Hetzner, say. [ipc.md](ipc.md) sketched it ("Later: VM networks across
hosts", 2026-11-05) and kept things ready: the `underlay` role of a host's
address in the registry ([cluster.md](cluster.md)), 39241/udp, a place for a
third user of the raft crate ([raft.md](raft.md)). This is that sketch made a
design; where it leaves the sketch, "Decisions" says so.

Decided by the user: a public address is **one to one NAT** in the first
version, an address inside the guest itself later; the first version
carries **IPv4 and ARP only** on these networks; and a cluster has as many
private networks as are wanted, **a VM on one of them, chosen as it is
made**. And **every VM has a firewall**: its inbound closed by default from
everywhere, its own network too, its outbound open, with state; rules that
allow and deny, kept in **named firewalls** that as many VMs are given as
are wanted ("The firewall", below).

`vnet` is the working name: the module `vnet.ko`, the command `vnet`, the
provider `vnet`.

## Where it starts

- **A VM's network belongs to its host today.** The switch
  (`modules/hypervisor/hv/src/net.rs`) hands out the same subnet on every
  host, 10.0.100.0/24: a port's number is its address and its MAC, sixteen
  ports. `network=` is a group of ports on that one switch that reach each
  other, nothing more.
- **The way out is a masquerade** through `hv0`, the stack's end of the
  switch (`layers/net/src/nat.rs`), and the way in is `hv forward`: one TCP
  port of nos's, relayed.
- So VMs on two hosts cannot reach each other, their addresses are the same
  on every host, and a VM cannot be given a public address.
- **What it stands on**: the raft crate and `raftmember`; the registry, with
  each host's `underlay` address; ipc between hosts, TLS with both keys
  pinned; `kcore::net`'s UDP listener and frames; ChaCha20-Poly1305, vendored
  with its portable backend, since the kernel keeps SSE off.

## What it is

```
 guest - virtio-net - port -+                      +- port - guest
                            the switch (host A)    the switch (host B)
 vnet.ko == ipc ==>  where a port is, ARP, DHCP, MTU     ^
 (raft; the registry)       |         |                  |
                          hv0      tunnel === UDP 39241, sealed === tunnel
                            |     (keys over ipc: hv.tunnel@B)
                 NAT: masquerade | one to one -- eth0 -- the internet
```

- **`vnet.ko` on every host**: a Raft group of its own (`raftmember`'s
  engine, as `nbs-mds` is). Its state is the networks, the ports and the
  public addresses.
- **The switch** gets ports of these networks beside its own: a table of
  where every address is, kept to the group's state, and a tunnel to every
  other host.
- **NAT** gets the networks' subnets to masquerade, and one to one mappings.

What a user has:

```
vnet init                                 on a member of the cluster: a group of one
vnet host add|remove|replace <host>       the group's members, as NBS's are
vnet recover                              on the last host standing, as cluster recover
vnet create lan 10.10.0.0/24 [mtu=1420]   a network: its subnet; .1 is its gateway, on every host
vnet delete lan                           once no port is on it
vnet list | vnet ports [lan] | vnet alerts
vnet port <id|uuid|name@host|address>     whose it is, where it is, whether that is confirmed
vnet port move <port> <host> [force]
vnet port delete <port>
vnet ip add 203.0.113.9 host=ex44         a public address, and the host it is routed to
vnet ip set 203.0.113.9 web@ax41|none     onto a VM's port, or off it
vnet ip move 203.0.113.9 ax41             the provider routes it elsewhere now
vnet ip remove 203.0.113.9 | vnet ip
hv create web <url> network=vnet:lan [address=10.10.0.5] [public=203.0.113.9]
hv tunnel                                 the other hosts: keys, counts, when each was heard, whether the MTU fits
hv probe add p1 network=vnet:lan          an end with no VM behind it
hv probe ping p1 10.10.0.5
hv fw create edge [in=deny] [out=allow]   a firewall of the host's; vnet fw ..., the cluster's
hv fw edge add allow in tcp 22 from 198.51.100.0/24 [at=N]   the first rule a flow matches decides
hv fw edge add allow in from net          its VM's own network in
hv fw edge add deny out tcp 25
hv fw edge del 2 | hv fw edge move 3 1 | hv fw edge policy out=deny | hv fw edge
hv fw set web edge | hv fw delete edge | hv fw
hv create web <url> firewall=edge         firewall=vnet:edge for a VM on a cluster's network
```

- **As many private networks as are wanted**, 256 at most: each `vnet
  create` a subnet of its own, apart from the others, made and deleted from
  the shell and from the panel's page of them.
- **A VM's network is chosen as the VM is made, one a VM**: `hv create ...
  network=vnet:<name>`, and in the panel's form for a new VM a list of the
  cluster's networks -- name, subnet, how many addresses are free -- beside
  the host's own.
- The VMs of a network reach each other at their private addresses wherever
  they run; two networks do not reach each other, as two groups do not today.
- Every VM goes out to the internet masqueraded **from its own host**: no
  detour through a gateway, and no host whose loss takes every VM's way out.
- A public address is one to one: the guest sees its private address alone,
  the world reaches it at the public one -- at the ports its firewall opens,
  TCP, UDP and ICMP -- and it goes out from it. The VM may run on any host,
  not only the one the address is routed to.
- **A firewall on every VM**: inbound closed by default from everywhere --
  the internet, a public address, the VMs of its own network -- outbound
  open, and the answers to a VM's own flows let in. The rules that allow and
  deny are kept in named firewalls, each given to as many VMs as are wanted;
  a VM given none has `default`.
- A host's own networks -- `network = "lab"`, 10.0.100.0/24, `port=`,
  `forward=` -- stay as they are. One of the cluster's is written `network =
  "vnet:<name>"`, as a disk of NBS's is `nbs:<volume>`; an older hv refuses
  the line, its `valid_network` knowing no `:`. A host in no cluster loses
  nothing, and `public = "..."` in a vm.toml gives a VM of its own a public
  address (V0b).

## Decisions

| What | Decided | Why |
|---|---|---|
| Where the state is | A Raft group of its own, `vnet`, as `nbs-mds` is | The sketch's; packets go on without a leader, by the last table |
| What a host is in the state | The registry's **node**, not its name | A removed host's name may be a new host's, its node never (`registry/src/lib.rs`, `Removed`): by name, another machine would become the gateway of the old one's addresses |
| A port's attach at a VM's start | A read the leader confirmed; with no leader, only a port bound here, and **said** to be unconfirmed | The view after `catch_up` is what the log knew at shutdown; `nbs-mds` met this already (`member.rs`, a member started behind what it had applied) |
| An address's MAC | `02:00` and the IPv4 address's four bytes; the gateway `HOST_MAC` everywhere | The switch learns nothing, a MAC does not change when a VM moves, NAT needs no ARP |
| Subnets | Apart from one another in the cluster, private ranges only, /16 to /28, not 10.0.100.0/24, covering no host's address | An address is one port's in the whole cluster: one table by address, and one `hv0` for every network; NAT lets a private range neither out nor in |
| A network's gateway | `hv0`, one a host, for every network; the switch answers ARP and echo for the gateway's address | There are four virtual NICs and none is given back; the stack does not change; `hv forward` works as it is |
| What a network carries | An IPv4 packet to the port **it is addressed to**; the switch answers ARP and DHCP; no broadcast between hosts | The same on one host as across two; no flooding in the tunnel; a cloud's VPC is so |
| What the tunnel carries | UDP 39241, **the packet, not the frame**: sixteen bytes of header (kind, network, key, counter), ChaCha20-Poly1305 | The receiver makes the Ethernet header itself, from the addresses: no MAC crosses to be forged or mistaken; not TCP in TCP; the sketch had Geneve's shape, this is WireGuard's data message, less to parse |
| Keys | Each host gives each other host the key to seal **to it** by, over ipc, and changes it by time | No agreement to reach and no race for who goes first; a reboot is new keys; a host the registry removes loses them. The sketch had one of the two make both |
| hv and vnet | ipc to the group's member on the same host, as hv's disk reaches `nbs-mds` (`on_client`) | The read and the command run on the member's task; a deadline and a cancel of hv's own; changes pushed, not asked for; the bus's JSON has a ceiling (256 KiB an answer, 64 KiB parsed) |
| The way out | A masquerade on the VM's own host; NAT is told the networks' subnets | No gateway to go through; NAT's test `on_subnet(inner, src)` is what changes |
| A public address | One to one in the gateway host's NAT, with no table of flows; a VM on another host through the tunnel both ways | Hetzner lets no address out of a server it is not routed to; with no flows to lose, a gateway restarted goes on |
| MTU | 1420 -- 1440 is the most, 1500 less 20, 8, 16 and 16 -- given when the network is made and not changed; the guest is told by `VIRTIO_NET_F_MTU` and DHCP's option 26; the switch holds it and answers ICMP 3/4 | `ip=` carries no MTU; a guest reads it once, at start, so one changed under it is packets lost without a word |
| VLANs and a device's MTU in `net` | A later step of its own | The tunnel is sealed and runs between public addresses; a vSwitch (a VLAN, MTU 1400) is for a private underlay. The sketch had them first |
| A VM's firewall | Named, one a VM; inbound closed from everywhere, its own network too; outbound open; with state; at the switch's port | The user's; one place a VM, wherever it runs, which sees the true source -- past one to one and past the tunnel |
| Where firewalls are kept | Where the VM's network is: a host's with hv, the cluster's in vnet's state (`vnet:<name>`) | A VM on a cluster's network moves and a host's firewall does not; networks are named so too |

## The state

`common/net/vnetmeta`, no kernel in it, and `modules/net/vnet`; both
architectures.

- **A network**, 256 at most in a cluster: its name (26 bytes at most), its
  number (the VNI, 24 bits, from a counter, never used again), its subnet,
  its MTU (576 to 1440),
  whether it has a way out, and where its next address is given from -- the
  next free one round the subnet, so that an address let go is not the next
  one given. An address ending in 0 or 255 is not given: the stack's ARP on
  `hv0` judges a sender by `hv0`'s own /24.
- **A port**: its id, from a counter; its key, a VM's UUID and the NIC (0);
  its network; its address; the registry's node it is on; a generation, from
  one counter for the cluster.
- **A public address**: the address, the node that is its gateway, the id of
  the port it is on or none. Sixty-four a gateway at most, refused when the
  sixty-fifth is added rather than left out of the gateway's NAT in silence.
- **Commands**: `Init{group}`; `NetworkCreate`, `NetworkDelete`;
  `PortCreate` -- asked again for the same UUID and NIC it answers the port
  there is, and what differs from the one asked for, and never changes or
  rebinds it; `PortMove{port, expect, to}`, a compare and set whose refusal
  says where the port is; `PortDelete`, which lets its public address go and
  says so; `PublicAdd`, `PublicRemove` (refused while it is on a port),
  `PublicSet{address, gateway, port, expect}`; `Digest`. The group's members
  are a crate both it and NBS link (below).
- **The rules a third group keeps** (docs/raft.md, docs/nbs.md): `apply`
  with no clock, no member's id and **no registry** in it; a state cheap to
  clone, since the engine copies it into the view at every turn -- tables
  that copy on write (`Pieces`, moved out of nbsmeta into a crate both link)
  and indexes behind an `Arc`; a snapshot written and read as a stream, and
  one read back held to every rule `apply` keeps; a SHA-256 of it proposed
  by the leader, and a member whose own differs stopped (`App::halt`); the
  first five to join vote; the ports bounded in `apply`.
- **What `apply` cannot see** the proposing member checks: a subnet or a
  public address covers no address of any host the registry has. hv, as it
  installs a subnet or a mapping, refuses one that covers an address of a
  device of its own, and says so; an overlap that comes later (`cluster
  address`) is an alert.
- **The decisions are pure functions** in `vnetmeta` -- what an attach is
  answered, a host's table, the table's revision -- so that `raft-test.py`,
  which CI runs, holds them: no gate on `tests/hub.py` runs in CI, nor does
  `hv-test.py`.
- **The other hosts' addresses** -- `underlay`, or `ipc` where a host has
  none -- the member reads by asking the `cluster` provider for its hosts,
  there being no other way for a module. An ask that fails keeps the last
  answer and raises an alert; it never means there are no hosts.
- **Its members** are as NBS's: `vnet init`, `vnet host add`, an offer to a
  host the registry has. Not written a second time: the module's half -- the
  listener of a host in no group yet, the offer, start and stop, which
  `cluster` and `nbs-mds` each have already -- moves into `raftmember`, and
  the state's half -- `Init`, `Add`, `Remove`, who votes, the check -- into
  `common/cluster/members`. With them a `recover` for any group
  (`raft::recover::force_alone`): with two hosts, one lost for good is the
  first failure an operator meets.

## hv and vnet

ipc with the group's member on the same host; the wire is `vnetmeta`'s.

- **An attach** (`Attach{uuid, network, node}`), at `hv create` and at `hv
  start`:
  1. a read the leader confirms (`Core::read`), waiting some five seconds
     for a leader to be known;
  2. the port bound here is answered -- address, mask, gateway, MTU, VNI,
     generation, public address; one bound to another host is refused, the
     host named; none there is made (`PortCreate`);
  3. with no leader, only a port the local state binds here: the attach is
     **unconfirmed**, which `hv start` and `hv list` say and an alert keeps
     saying, and it is read again once there is a leader;
  4. a generation that changed revokes the port, even one that came back to
     this host.
- **The table** (`Watch`, answered `Table{instance, revision, hosts,
  networks, ports, public addresses}`) comes at once and at every change;
  only a command that changes it, or a new answer of the registry's, moves
  its revision. Each watches the other's mailbox: with vnet.ko gone hv goes
  on by the last table, says so once, raises an alert and refuses attaches;
  with a new instance there it confirms every port it has attached again.
- **The switch, the tunnel and NAT are brought up by hv's task, by the
  table** -- as soon as it gives this host a port or a public address -- and
  not by the first guest with a NIC, as today: a gateway host with no VM of
  its own would otherwise serve nothing, in silence.
- **A network chosen as a VM is made.** The `vnet` provider lists the
  networks (`networks`: name, subnet, MTU, ports, addresses free). A
  descriptor gets a way to say that a parameter's values come from a read of
  another provider's on the same host (`choices`: provider, method, field),
  and the panel's `MethodForm` draws a list by it: `vm.create`'s `network`
  is the cluster's networks and the host's own. A network there is none of
  is refused by `hv create`, which names those there are.
- **A VM's start and stop write nothing to the log.** The port is made at
  `hv create`, where a conflict shows at once, or at the first start, and
  deleted at `hv delete`. **The state alone holds** a port's address and its
  public address: a vm.toml has `network = "vnet:lan"` and no more, and
  `address=` and `public=` on `hv create` are carried out once. A VM with no
  folder -- a new UUID at every start -- is not taken on such a network.
- **A move** (`vnet port move`): the proposer asks the port's host first.
  While its VM runs there it is refused; a host that does not answer is
  moved from only by `force`, which says what that risks. The host revokes
  the port when it has caught up.

## The switch

`modules/hypervisor/hv/src/net.rs`; x86-64, as all of hv is.

- **A port's slot gets an identity** -- its address, and `VNET | vni` as its
  group -- changed under the inbox's lock with its owner. `port_ip`,
  `port_mac` and `port_of` stop being functions of the slot's number. A
  slot's owner is a VM or a probe, and the two attach, release and are
  revoked by one path.
- **Every delivery says whom it is for**: under the inbox's lock the owner
  and the slot's address (and network) are compared -- for a frame from a
  guest, from `hv0` and from the tunnel alike. Today the group alone is
  compared, which is enough only because a slot's address is its number: with
  an identity of its own, a frame for a VM gone would be the next VM's.
- **Where every address is**: a table taken whole beforehand, address to
  network, where -- a slot, another host, nowhere -- generation and public
  gateway. An entry is copied out under a lock held for the lookup; a new
  table takes the old one's place whole.
- **A frame from a guest on such a network**:
  1. checked as it comes in (`ingress`): its own MAC and address; IPv4 and
     ARP; IPv6 is not carried in the first version, and counted;
  2. DHCP is answered in place: the port's address, the network's mask, the
     gateway, the DNS server, the MTU;
  3. ARP: for the gateway, `HOST_MAC`; for a port of its network, that
     port's MAC; for its own address -- a probe for duplicates -- nothing;
     and ARP goes no further;
  4. to `HOST_MAC`, by where the packet is for: `hv0`'s own address is the
     stack's; the gateway's address has its echo answered by the switch, and
     nothing else is carried there, which the stack would drop without a
     word; an address of any network of the cluster is counted
     (`isolated`); anything else goes into the tunnel to the gateway of the
     port's public address when that is another host, and to the stack and
     NAT when not;
  5. to a port's MAC: when the packet is for that port and the network is
     the same, into its inbox or into the tunnel; counted otherwise
     (`isolated`, `misdirected`);
  6. any other broadcast is not carried, and counted;
  7. past the network's MTU it is not carried; with DF set it is answered
     ICMP 3/4 from the gateway's address, once a send -- a send of TCP whole
     is judged before it is cut -- and no more often than a rate.
- **A frame from `hv0`** -- NAT's answers, one to one inwards, a relay's
  segments: to a port here, into its inbox, up to 1500 bytes, since those
  are a masquerade's answers and the stack's own; to a port on another host
  only what is forwarded, not what `hv0` itself sends, and within the MTU.
  The stack's ARP on `hv0` for a port here is answered by the switch, and
  not sent to the ports.
- **`hv0`'s sink seals nothing and allocates nothing**: it is called under
  the device's transmit lock, interrupts off. What cannot be done there --
  a send into the tunnel, the switch's own answer to `hv0` -- goes on one
  ring taken beforehand, which the tunnel's task empties; a destination has
  its share of the ring, so that a flood at one public address does not put
  the others' packets out.
- **A port revoked, and every port of a host whose member stopped** for a
  digest that differs -- its table is then known to be wrong -- are shut:
  nothing from them or to them goes on, and `hv list` says why.
- **A probe** (`hv probe`): a port with no VM, its identity kept; it answers
  echo, and sends echo and UDP the way a guest's NIC does. The gates begin
  with it -- in V0a, on a port of the host's own network -- and it is what an
  operator checks a network with on a host that runs.

## The tunnel

`common/net/vnetwire`, no kernel and no `unsafe` in it, and
`modules/hypervisor/hv/src/tunnel.rs`.

```
Ethernet | IPv4, DF | UDP 39241 to 39241, checksum 0 | kind 1, network 3, key 4, counter 8 | the IPv4 packet, sealed | tag 16
```

- **`vnetwire`**: the datagram read and written, sealed and opened
  (ChaCha20-Poly1305, the counter its nonce, the header its associated
  data), the window of counters seen (2048), the messages keys go in. hv.ko
  and the fuzzer link it.
- **The kinds**, and what the receiver holds each to against its table
  before it makes the Ethernet header:
  1. port to port: from a port of that network which is on the host that
     sent it, to a port here of the same network;
  2. port to gateway: from a port on that host whose public address is
     routed here **and mapped already**; not to `hv0`'s own address;
  3. gateway to port: to a port here whose public address is on that host;
     from an address outside every network of the cluster, or from the
     network's gateway, which NAT's refusals come from;
  4. and 5., an echo and its answer.
  Anything else is counted (`stale`). Nothing that came out of the tunnel
  goes back into it.
- **Sending** is in the context of what sends -- a guest's CPU, or the
  tunnel's task: a frame from the pool (`NetFrame::alloc_tx`), the headers,
  the seal, `Nic::transmit`.
  - A guest does not have the uplink's transmit queue to itself: a send into
    the tunnel goes only while the device keeps room for the stack
    (`Nic::tx_room`), and is dropped otherwise, counted against the port.
    ipc, raft and NBS live in the same 256 frames.
  - A guest NIC's transmit pass runs under its VM's platform lock: a frame
    sealed counts as several against the pass's bound.
  - The next hop's MAC is had beforehand, in the task (`Nic::resolve`);
    nothing on the way waits for ARP. A datagram's source is the address the
    registry has for this host: where the device it leaves by does not have
    that address, the other host is not brought up, and an alert says so.
    One new call of the kernel's: the device an address leaves by
    (`kernel_net_route`), a virtual NIC never.
- **Receiving** is a `UdpHandler` in the receive soft IRQ: from port 39241
  -- which NAT never gives a guest's flow -- from the other host's address,
  under a key that is known, with a counter the window has not seen; only
  then is the frame kept (`Lent::retain`) and opened where it lies, the tag
  checked before a byte is deciphered, and the window moved after it.
- **Keys**: a mailbox, `hv.tunnel`, a `Cluster` name. A message is from
  whom `kcore::ipc::peer_name` says, who has to be in the table. The rules
  without which a nonce could be used twice:
  - a key is made anew for every `Key` message, from `kcore::random` --
    refused while the pool has no seed -- and installed to receive by before
    it is sent; its number is random;
  - `Key` carries the instance that made it and the instance it is for; it
    is taken only by that instance, and a number had before is not taken;
  - another host's new instance has both its keys to receive by and the key
    it was given dropped;
  - a key is changed by time, the old one good for an overlap; none is
    written to a disk; the times are the module's parameters (`rekey=`,
    `keylife=`), or no gate could wait for them.
- **An echo** through the tunnel, sealed, every few seconds, every so
  many-th of them a datagram of full size, its answer no longer than what
  asked for it: a host that is silent, and an underlay that does not carry
  the MTU, are lines in the log and alerts.
- **Locks**: the table, the ring and the keys are each under a lock held
  for a lookup, never with an inbox's lock, never across a seal, a transmit
  or `vnic.receive`. A key is copied out from under its lock, and the open
  runs without it.

## NAT

`layers/net/src/nat.rs` and `arp.rs`; in the image, both architectures.

- **The subnets behind `hv0`** besides its own, sixteen at most -- network,
  mask, gateway -- which hv adds while a port of the network is on this
  host. `outbound` takes a source on `hv0`'s subnet or on one of them; a
  destination on any of them is not NAT's; `reject` answers from the
  network's gateway.
- **One to one**, sixty-four at most -- the public address, the inner one,
  its MAC, its gateway, its MTU -- with no table of flows, and looked at
  before the masquerade both ways:
  - inwards, a packet for the public address has its destination rewritten
    and goes out of `hv0`. **Its source has to be an address of the world's**:
    not one the deny list has (`DENIED`, less what `nat allow` let through),
    not this machine's. Without that, any neighbour on the link could send a
    VM a packet "from" its private neighbour;
  - outwards, a packet from the inner address has its source rewritten. Its
    destination is held to the same list, but not to the uplink's own
    subnet, or a neighbour on Hetzner's subnet could not be answered;
  - ICMP: echo both ways, and errors translated both ways, PMTU discovery
    being nothing without them; a packet inwards past the MTU is answered
    ICMP 3/4 from the public address; whatever NAT makes up itself, today's
    `reject` among it, is held to a rate;
  - an address of this machine's is not mapped; `forget(address)` at a
    mapping made, at a revoke and at a move; mappings and subnets live with
    NAT on -- `disable_owned_in` takes them -- and are made again.
- **ARP** on the outer device answers for the public addresses mapped there
  -- **as the public address**, where today's `send` writes the device's --
  and only while mapped; a mapping made or moved is announced. Hetzner
  routes a subnet to the server's main address; a single additional address
  its router may ask for by ARP, as a plain LAN does -- and the gate on
  `tests/hub.py` -- and answering covers either. An address served so is
  ordered with no virtual MAC: that is for a guest whose own MAC is on the
  wire, and here none is. The ARP cache, sixteen entries for the machine,
  gets more.
- **The ABI**: `kernel_net_nat_inside`, `kernel_net_nat_map`,
  `kernel_net_nat_unmap`, and in `kcore::net` the values whose drop takes
  them back; `nat` lists both.

## The firewall

Decided by the user: a VM's inbound is closed by default -- from the
internet, through a public address, and from the VMs of its own network --
and its outbound open; the firewall keeps state; its rules allow and deny,
either way; and firewalls are named, one given to as many VMs as are
wanted.

- **A firewall** is named, and holds a policy for each direction and rules
  in order, sixty-four at most: the first rule a flow matches decides it,
  and the policy when none does. A VM has exactly one; a firewall is as many
  VMs' as are given it, and a change to it is theirs at once. A VM given
  none has `default` -- inbound denied, outbound allowed, no rules -- which
  can be changed and not deleted. A firewall a VM has is not deleted: the
  refusal names the VMs.
- **Where firewalls are kept** is where the VM's network is: a host's in
  hv's `/etc/hv/firewalls/<name>.toml` -- through `conf`, a new file synced
  and renamed over the old -- for the VMs on the host's networks; the
  cluster's in vnet's state (`vnet fw`), for the VMs on the cluster's
  networks, named as networks are: `firewall=edge`, `firewall=vnet:edge`. A
  VM takes no firewall of the other kind: a VM on a cluster's network moves,
  a host's firewall does not.
- **A rule**: `allow|deny in|out [tcp|udp|icmp] [22,80,8000-8100]
  [from|to <net/mask,...>|net|any]`, `net` being the VM's own network -- its
  subnet, a host network's group -- so that one firewall serves VMs of
  several networks; eight ports or ranges and eight networks a rule at most.
  Read, checked and written by one crate, `common/net/firewall`, with no
  kernel and no `unsafe` in it, which hv.ko, vnetmeta's `apply` and the
  fuzzers link.
- **Where it acts**: at the VM's port of the switch, both ways, on whatever
  host the VM runs -- a cluster firewall's rules come to that host with the
  table. It sees the true addresses: the source a public address's packet
  came from, and the source past the tunnel.
- **With state**: the rules judge a flow's first packet. A flow let through
  goes into the port's table of flows -- room taken when the port is
  claimed, 2048 flows; a full table gives up the oldest flow of the same VM,
  never another's -- and from then on its packets go both ways without the
  rules, and so do ICMP errors about it, PMTU discovery being nothing
  without them. Its times and TCP's states are NAT's (RFCs 5382 and 4787),
  written once: they move from nat.rs into `netwire`, which both link.
- **What it does not see**: DHCP and ARP, which the switch answers itself;
  and the host's own traffic, from `hv0`'s address -- the host is the VM's
  hypervisor, with its memory in reach.
- **`hv forward`**: its relay reaches the VM as the host, so it holds each
  connection to the VM's rules itself, by the client's address
  (`TcpStream::peer`), as it accepts it: a client the rules deny is closed
  on, counted as refused. `hv forward add` says so when the VM's firewall
  lets nobody in at that port.
- **A deny**: a packet inwards is dropped without a word; one outwards is
  answered to the VM itself -- ICMP communication administratively
  prohibited, from its gateway, no more often than a rate -- so that its
  connect fails at once rather than waiting out its retries.
- **A change** is built beside the rules in force and takes their place
  under a short lock: nothing is allocated or freed on the way. Then the
  open flows of the firewall's VMs are judged again -- by a task, a part at
  a time -- and those it denies now are cut.
- **A port that changes hands** has its table emptied as it is claimed, and
  a delivery is checked against the owner's generation under the inbox's
  lock: a VM coming onto a port is never handed what the flows of the one
  before let in.
- **IPv6**, which only a host network's guests carry, between themselves:
  no rule of addresses; it reaches a VM whose firewall lets its network in
  (`allow in from net`, of no protocol and no port) or whose inbound policy
  allows, and neighbour discovery always.
- **IPv4 fragments** to and from a VM are dropped, counted: they carry no
  ports.
- **VMs made before it**: at its first start under the new hv, a VM with no
  `firewall` gets `default`, and one with forwards a firewall of its own name
  allowing in at each forwarded port -- said by `hv start` and the log. The
  panel on the EX44 and every VM reached through `hv forward` stay reached,
  and the rest is closed. `api panel create` gives the panel's VM a firewall
  with its port.
- **What is seen**: the flows each rule and each policy let through or
  denied; the packets denied at each port; each one denied as a tracepoint
  of the `fw` group (`fw_deny`), not a line of the log. The panel has a
  page of firewalls -- the host's from a provider of hv's own, `fw`, its
  page made from its descriptor alone as any provider's is, `vnet`'s the
  cluster's -- a firewall chosen in the form a VM is made by (`choices`, as a
  network is), and a VM's on its page, changed there. (`fw` and not the `vm`
  provider's methods, as built: the vm provider's page is the panel's own,
  where a provider's own descriptor makes a page of its own for nothing;
  and the vm descriptor, twenty of its 32 KiB taken, keeps its room.)

## A packet's way

- **VM to VM on one host**: port, table, inbox -- as today, a send of TCP
  whole too.
- **VM to VM on two hosts**: port, table: another host; sealed; out of eth0;
  B's soft IRQ; opened; held to the table; an Ethernet header made; inbox.
- **VM to the internet**: port, `HOST_MAC`, `hv0`, NAT -- masqueraded as the
  host -- eth0.
- **The internet to the public address of a VM on another host**: H's eth0,
  NAT one to one, `hv0`, the ring, the tunnel's task, B, inbox. Back: B's
  port, `HOST_MAC`, the port's gateway being H, the tunnel, H's `hv0`, NAT
  one to one, eth0.

## When things fail

| What happened | The packets | What is seen |
|---|---|---|
| vnet has no leader | Go on by the last table; a port bound here attaches unconfirmed | "unconfirmed" in `hv start` and `hv list`, an alert; new networks, ports and moves refused, saying so |
| vnet.ko not loaded yet -- rc past a slow `insmod` -- or unloaded | `hv start` waits for the member's mailbox and says what it waits for, then refuses; VMs running go on by the last table | "no vnet on this host"; an alert that the table is not kept up |
| The member stopped for a digest that differs | The host's ports of the cluster's networks are shut | An alert, `vnet alerts`, `hv list` |
| A port moved or deleted while its host was down | A start there is refused, the read being confirmed; with no leader it starts unconfirmed, the other hosts drop its packets, and the host revokes the port once it has caught up | `stale` on the other hosts; "the port is ax41's now" |
| Another host rebooted, or its hv was loaded anew | New keys both ways; seconds of loss | `hv tunnel`: the keys' ages |
| ipc between two hosts is down | Their keys live out their time, then the pair's tunnel stops | An alert that a key is running out |
| The underlay between two hosts is down, or does not carry the MTU | Loss between their VMs | Echoes unanswered: the log, an alert |
| A public address's gateway host is down | The VM with that address has no internet | An alert on the VM; `vnet ip set ... none` puts it back on its own host's masquerade, `vnet ip move` after the provider routes the address elsewhere |
| A host removed from the registry with ports left | Its ports are nowhere; the host and its keys are dropped at once | An alert of ports on a host the registry has not; `vnet port delete` |
| The registry could not be asked | The other hosts' addresses as they were | An alert |
| A firewall a vm.toml names is not on this host | The VM does not start -- neither open nor with `default` | The refusal names the firewall |
| A firewall's rules changed | Theirs at once on every VM it is; the flows they deny now cut | Each rule's counts |
| A VM's table of flows is full | A new flow puts out the oldest of the same VM's | A count at the port |
| No room | An inbox, the ring, a share of `hv0`, the uplink's reserve: dropped | A count for each, against the port or the other host |

## Tested

Each rule is held by a model, and by a mutant that takes it out.

- **`hv-fuzz.py`** (host, in CI): the `switch` target with ports of the
  cluster's networks and a slot handed over mid-send; `nat` with the subnets
  and the mappings; a new `tunnel` target -- two switches over a hostile
  wire: loss, repeats, damage, reordering, another's keys, restarts, the
  table changed mid-send. Held: networks apart; a guest sends as itself
  alone; a delivery only to the slot with that address; a datagram only
  under a key given, and once; no key and counter sealed twice, across
  restarts too; a packet out of the tunnel only from the host the table has
  its port on; nothing from the tunnel into the tunnel; one to one lets no
  private source in and gives no other's address out; no frame out of the
  uplink from a MAC that is not the device's. A new `firewall` target -- the
  rules and the table of flows against a model of their own: the first match
  decides, the policy where none does, a flow's answers and its ICMP errors
  let through and nothing else unasked, a change cutting what it denies now,
  a full table giving up the VM's own oldest -- and `switch`, `guestnet` and
  `relay` with the firewall on the way: nothing reaches a VM that the host
  did not send, a flow of its did not carry, or its rules (its policy) did
  not allow; a VM coming onto a port handed nothing the flows of the one
  before let in; a forward's client held to the rules.
- **`raft-test.py`** (host, in CI): a `vnetmeta` target, its scenarios and
  mutants. Held: a port on one host; a move a compare and set; ids and
  network numbers never used again; a public address on one port; an attach
  answered by a pure function of the state, and an unconfirmed one never
  silent; `default` never deleted, a firewall a port has never deleted, a
  firewall's rules changed by a compare and set of their revision.
- **`vnet-test.py`** on `tests/hub.py`, by hand: the group on both
  architectures -- init, hosts added, networks, ports attached by a word of
  the shell's as `nbs io` does I/O, a reboot, the leader killed, a host cut
  off and its port moved and started there, one UUID twice, the table at its
  bound, vnet.ko unloaded and loaded under it, a halt in both orders of
  loading, `cluster remove` of a host with ports. And the packets, on
  x86-64, three nos with hv.ko under TCG and no guest: probes that reach
  each other across hosts and not across networks, NAT out to a host of the
  world's behind the hub from every host, a public address answering it --
  on its gateway and on another host -- a host killed, keys changed and run
  out, a port moved, a narrow link. The firewall, with probes on one host:
  closed by default, a rule opening it, a deny outwards answered, a change
  cutting a flow, a forward held to its client's address. The hub gets that
  host of the world's --
  a public address behind its router's MAC that answers echo, remembers the
  source it saw and calls a public address itself -- and a link's bound on a
  frame's size.
- **By hand with Linux** (`hv-linux-test.py --net`, and `hv-vnet-test.py` on
  the AX41, the guests nested): the MTU a guest reads from the device and
  from DHCP, ping and TCP across three hosts, `apt-get update` through NAT,
  a login at a public address, iperf through the tunnel against one host's
  switch; two guests of one network that reach each other only once their
  firewall lets the network in.

## Steps

A new gate gets its section in docs/testing.md and its row in CLAUDE.md's
table in the step it comes with.

- **V0a. The firewall on the host's networks**, before any public address,
  so that none is ever open: `common/net/firewall`, the switch's table of
  flows, the host's firewalls and `hv fw`, the relay's check, the VMs made
  before it carried over, `api panel create`'s; the probe; the world behind
  the hub. Two VMs of a host's network that do not reach each other until a
  rule lets them, and a forward that lets in only the addresses its rules
  name. As built: all of it but the world behind the hub, which goes with
  V0b -- the first step whose packets leave the host -- and a forward held
  to two clients' addresses in a boot, which the hub's world gives two of;
  the relay's check is the hv fuzzer's `fwswitch` target's (and a mutant's)
  meanwhile, and `hv-linux-test.py --net`'s for a client let in. Found on the way: a provider's call
  runs on its caller's stack, and a firewall's seven kilobytes of rules
  held by value in a few frames ran `api panel create`'s shell out -- they
  live on the heap now, and a size is asserted of what lives on a stack.
- **V0b. NAT: the subnets behind `hv0`, one to one; `public=` for a VM on
  its own host.** A VM on a host in no cluster reached at a rented address,
  at the ports its firewall opens. As built: one to one and the networks
  behind `hv0` in `nat.rs` (the C ABI `kernel_net_nat_map`, `_unmap`,
  `_inside`, `_outside`; `kcore::net::OneToOne` and `Inside`), ARP answered
  and announced as a public address (`arp.rs`), what NAT makes up held to a
  rate; `public=` on a VM and a probe, mapped by the switch with the port and
  unmapped with it, a start refused where it cannot have one; the panel's
  field; the hub's world (`Hub.world`, `world_call`); the hv fuzzer's
  `onetoone` target and nine mutants; `vnet-test.py`'s V0b phase. The
  networks behind `hv0` have no user before V2's cluster networks: their
  rules are the fuzzer's alone until then. The link's bound on a frame's
  size goes to the hub with V2, where an MTU below 1500 first matters.
- **V1a. A group's members, written once**, with nothing changed in what
  NBS and the registry do: `raftmember`, `common/cluster/members`. With it
  the `begun` mark (below) and `recover` for any group. As built:
  `common/cluster/members` -- the group, its members by incarnation, those
  removed, `init`, `add` and `remove` as applied, who votes, the rules a
  snapshot read back is held to -- which nbsmeta embeds with its commands'
  and snapshot's bytes as they were, and by whose cut the registry votes;
  raftmember's `host.rs` -- the member's runner (its task, the one start at
  a time, why it did not start, said), its log opened or made with
  `begun`, its probe, and `recover` -- which cluster.ko and nbs-mds.ko both
  run; and `offered.rs` -- the offer, the listener of a host in no group,
  the member file, the member's half of its membership, `init`, `host add`,
  `remove`, `replace` and `recover` -- which nbs-mds.ko is now no more than
  the hooks of (`Group`, `Starter`), and vnet.ko will be. New to NBS: `nbs
  recover`, its probe `TAG_PROBE` past the offer's tags. Held: raft-test,
  its six members' mutants; nbs-fuzz; cluster-test's and nbs-test's new
  phases (a log gone beside `begun`, refused; `nbs recover`).
- **V1b. The state and the module**: `vnetmeta`, `vnet.ko`, its provider
  and alerts; the panel's page of networks, ports and public addresses, from
  its descriptor. The cluster's firewalls in the state -- `FirewallCreate`,
  `FirewallSet` (a compare and set of the rules' revision),
  `FirewallDelete`, `PortSetFirewall`, `default` made by `Init` -- and `vnet
  fw`. As built: `common/net/vnetmeta` -- networks, ports, public
  addresses and firewalls, each table behind an `Arc` and copied whole by a
  change while a copy holds it (4096 ports at most: no `Pieces` needed, so
  `Pieces` stays nbsmeta's), the rules in `apply.rs`, an attach and an
  address given as pure functions (`decide.rs`), the snapshot as a stream
  and its check; `common/cluster/rsmkit` -- the digests and a snapshot read
  as a stream, nbsmeta's and vnetmeta's; `firewall::edit` -- the words a
  change of a firewall's rules is asked in, `hv fw`'s and `vnet fw`'s;
  `modules/net/vnet` -- its member over raftmember's offered group, `vnet`,
  its operations shared by the shell and the provider (`ops.rs`), the
  registry asked through the `cluster` provider for its hosts' nodes and
  addresses, the `vnet` provider and its descriptor, `vnet alerts`; the
  panel's page from the descriptor (the mock's answers, a Playwright test).
  A port is made from the shell (`vnet port create`) until hv makes them in
  V2. Held: raft-test's `vnetmeta` target, ten scenarios and eighteen
  mutants -- its first run found a network made around an address made
  public before, which the state's own check of a snapshot then refused --
  web-test, the panel's tests, and vnet-test's V1b phase on both
  architectures. Left for V2, where hv is its user: the table a host is
  pushed (`table()`), the attach over ipc with a confirmed read.
- **V2. The cluster's networks on one host**: a slot's identity, the table,
  ARP, the gateway's echo, the MTU, DHCP's mask, router and option 26,
  `F_MTU`, hv's side of the ipc; a network chosen in the form a VM is made
  by (`choices` in a descriptor, `MethodForm`, made in V0a for a
  firewall); the cluster's firewalls at its ports, their rules coming with
  the table. As built: the switch's slots have an identity of their own --
  `Identity`, claimed by `claim_vnet` under the inbox's lock, the group
  `VNET | vni`, the MAC `02:00` and the address -- and every delivery says
  whom it is for (`For`: group, address, claim), checked under that lock;
  the rules of "The switch" for a frame from a guest and from `hv0` as
  written, `hv0`'s ARP answered through a ring of the switch's that the
  keeper hands in; a port shut (`Switch::shut`) rather than released under
  a running VM. hv reaches vnet through the management bus, as it reaches
  NBS -- a module reaches another's only so -- and not over a mailbox:
  `attach` (raft's ReadIndex through `raftmember::read`, up to five seconds
  for a leader, the host's own state answered unconfirmed without one),
  `detach`, and `table`, which hv's keeper (`hv/vnet`) polls every second
  by vnet's instance and the state's revision while a VM here is on a
  cluster network: the networks to the switch, the cluster's firewalls
  re-placed as their revisions move (`vnet:<name>` among `hv fw`'s, which
  `hv fw` does not change), a port deleted, moved or of another generation
  shut, an unconfirmed attach asked again; vnet gone, said once, the ports
  going on by what hv had. A VM's port is made at `hv create` (`address=`,
  `firewall=` carried out once, kept in the state alone; the vm.toml has
  `network = "vnet:lan"`) or its first start, deleted with its folder; the
  panel's create path the same; a probe on a cluster network is a port of
  vnet's under a UUID of its own. The vm provider's `networks` lists the
  cluster's networks among the host's, which `vm.create`'s `network`
  takes its choices from. Held: the hv fuzzer's `vnetswitch` target --
  its own model, every outcome reached -- with twenty-eight mutants, the
  `dhcp` target's MTU, the `net` target's `F_MTU`; vnet-test's V2 phase
  (one host, probes, `hv create`/`delete`, vnet reloaded). Left: the hv0
  answers' ring carries ARP alone until V3's tunnel shares it; a move
  refused while the port's VM runs -- the port's host asked first -- is
  V3's, the ask crossing hosts: until then a move under a running VM
  shuts its port where it was; a VM's public address on a cluster network,
  said and not mapped, is V4's.
- **V3. The tunnel**: `vnetwire`, keys over ipc, the echo, tracepoints.
  As built: `common/net/vnetwire` -- the header, `seal` and `open` (the
  counter the nonce, the header the associated data, through one helper
  for both sides), the window of 2048 counters, the messages keys go in --
  and hv's `tunnel.rs`, compiled as it is into the hv fuzzer: keys over
  ipc's `hv.tunnel` by the rules of "The tunnel" -- a key made anew for
  every message, taken by the instance it was made for and by number once,
  the numbers taken kept across a host's keys being dropped, so that no
  message seen twice starts a key's counter again -- the window asked
  before the open and moved once after it, a packet out only from a port
  the receiver's table has on the host its address is, nothing from the
  tunnel into it. The switch carries by a `Carrier` the tunnel is (ARP for
  a port elsewhere answered, the packet judged going out and cut at its
  length) and takes from it by `Switch::from_tunnel`. Beyond the plan: a
  host whose echoes stop for twenty seconds has its key asked for anew, so
  a key's message lost costs that long and not until the next `rekey`;
  the echo's every third is as large as the networks' largest MTU and
  answered as large, the way that loses those said once; the uplink's NAT
  put on by the tunnel as soon as there is another host to reach;
  `tunnel_drop` and `tunnel_key` (group `tunnel`), on no packet's way
  through. Held: the hv fuzzer's `tunnel` target -- three hosts' real
  switches and tunnels over a wire that loses, repeats, damages, reorders,
  misdelivers and forges, keys' messages lost and repeated later, tunnels
  started anew with and without their goodbyes, tables wrong, ports moved,
  a host sealing for another's port, uplinks full, the pool dry, hops
  unknown: no key and counter sealed twice, nothing delivered that a guest
  did not send, twice, or from a port the receiver's table has elsewhere,
  and once the wire is kind every port reaching its network's others --
  its `vnetwire` target, the `vnetswitch` model given a carrier, and
  twenty-four mutants, each caught; vnet-test's V3 phase (three nos, probes
  on each, keys changed under traffic, a narrow link, a host killed, hv
  reloaded, a port moved). Left: `kernel_net_route` -- the tunnel goes out
  of NAT's uplink, the default route's device, and says once where the
  registry has another address for this host; a refused move while the
  port's VM runs, which asks the port's host across the cluster as V4's
  gateway does, goes with V4; the tunnel's word to `vnet alerts` -- a host
  silent, a narrow way -- is the log's alone for now; a send's cost and a
  datagram's per packet, measured, with V5's numbers.
- **V4. Public addresses across hosts**: the kinds to and from a gateway,
  the other hosts' share of `hv0`, `vnet ip`. As built: vnet's table says
  every public address on a port -- the address, the host it is routed to,
  the port's network, address and host -- and the keeper, which now asks
  for it on every host whose hv is loaded (a host may be the way in for a
  VM elsewhere), serves those routed here (`Switch::serve`: NAT one to one
  to the port's address and MAC, the network behind `hv0` for its public
  addresses alone where no port of it is here; a moved port keeps the
  mapping, its host updated; what goes is unmapped first) and tells each
  port here where its own is routed (`public_way`). The world's packet NAT
  made the port's leaves `hv0`'s sink for a ring of the tunnel's, which
  wakes its task -- the mailbox it waits on is made on a shared event --
  and goes as kind 3; the port's for the world goes as kind 2 from the
  guest's CPU. Each end holds what it takes: kind 3 only to a port whose
  address is routed to the sender, from the world or the network's gateway,
  judged by the port's firewall; kind 2 only for the world, from a port the
  receiver serves an address for on the host that sealed it, into `hv0` as
  one more port's share of its backlog. A public address is served only on
  a network that goes out, said otherwise. Held: the hv fuzzer's `tunnel`
  target with two public addresses routed to one host -- one on its own
  port, one elsewhere -- through the real NAT one to one, the stand-ins
  noting whose `hv0` each frame went into (one `hv0` stands for every
  host's there): a world's packet only to the port its address is on and
  only from its address's host, a port's out of the right host's `hv0`
  once and from its address, and every address both ways once the wire is
  kind; its first runs found `serve` mapping a moved port's address again
  before unmapping it ("mapped already") and a test's own off-by-one; eight
  mutants, each caught -- ninety-two in all; vnet-test's V4 phase, whose
  first run found V2's keeper leaving a running port on the firewall it was
  attached with after `vnet port firewall` gave it another: the keeper now
  moves each port here to the one the table gives it (`Switch::assign`,
  its flows judged again). Left: a
  public address's alerts (its host down, the VM with no way out), and the
  move refused while the port's VM runs, both needing word across hosts
  that vnet's alerts do not carry yet.
- **V5. Numbers and documents**: docs/vnet.md as built; iperf through the
  tunnel; a tracepoint's cost when off; the EX44 and the AX41 -- the user's
  word. Done so far: docs/vnet.md. The tunnel's tracepoints sit on no
  packet's way through -- a drop, a key -- so there is no cost when off to
  measure on the data path. Left, each on real hosts and on the user's word:
  iperf between VMs of one network on the EX44 and the AX41 through the
  tunnel, and a public address routed to one and on the other's VM.

  The stand first wanted **a port per host**: its neighbours, ax41a and
  ax41b, are behind the AX41's one address, where V3's tunnel had every
  host at UDP 39241 and knew a datagram's host by its source address. Now a
  host's tunnel is at the registry's address for it and that address's own
  port -- the one its ipcnet is forwarded at already, in UDP too -- else
  39241; it sends from the uplink's own address and its port; and a
  datagram is known by its key's number, each one a host gives unique among
  its keys, whatever address and port a NAT gave it (as WireGuard knows a
  peer by its index). vnet's table carries each host's port, and a digest
  of the hosts the keeper gives back: a host's address changed in the
  registry is a table changed, which before it was not until vnet's own
  state moved. The fuzzer's `tunnel` target has two hosts behind a NAT on a
  third of its inputs, their source given the NAT's address or another of
  its own and any port, and moves a host's port under traffic: its first
  run with the moves found **a nonce used twice** -- set_table made a host
  whose address moved anew, the numbers of the keys taken from it lost with
  its old entry, so a key's message come again was taken anew from counter
  0. A V3 bug no input reached, addresses never moving there: a host is kept
  now, its keys and numbers with it, its next hop looked up anew. Seven
  mutants more, each caught; thirty-three of the tunnel's and its
  datagram's among ninety-nine.

  **On the stand** (2026-11-15, the user's word): the EX44 (i5-13500 on its
  own silicon, RTL8125, 1 GbE) and ax41a and ax41b (nos under KVM on the
  AX41's Zen 2, behind its one address and NAT), vnet `stand` of the three,
  lan at MTU 1400. The tunnels keyed in 5 s, ax41a at 65.109.93.213:39241
  and ax41b at :39242, their datagrams through the AX41's NAT both ways and
  between the two through it back to itself; 60 probe echoes of 60. An
  Alpine VM on each of the EX44 and ax41a -- ax41a's a guest of a guest --
  each a clone of NBS's image, by iperf3 for 20 s: **861 Mbit/s** of TCP
  from the EX44's, one stream, what a 1 GbE link carries with 60 bytes
  more a packet; 833 with four; **470** the other way, its sender the
  nested one; ping 0.52/0.66/1.02 ms. 11.4 million datagrams one way and
  4.2 million the other, none lost in the tunnel and none refused; 109 not
  sent, the RTL8125's ring full. Where the time goes is the profiler's to
  say and it says it badly: it prints a task's eight hottest chains,
  and the seal's are spread over many -- a flat profile is what it lacks.
  The update found four things no gate had: master's `store::keep_port`,
  which keeps a folder VM's switch port in its vm.toml at its first start,
  against V2's refusal of `port=` on a network of the cluster's -- no VM of
  a folder on vnet could start (fixed: no port kept there; vnet-test's V2
  phase starts one now); the image path of `hv create` refusing
  `address=`; V0a's firewall refusing the panel's update its ssh (the
  script lets it in for the while); and the neighbours with no hvboot and
  no `hv on`. And one left: the EX44's /etc/rc loads hv before vnet, so a
  probe's port outlives a reboot there (vnet goes first), to delete by
  hand.

## Limits

- IPv4 and ARP only on the cluster's networks: no IPv6, no broadcast or
  multicast between ports. A host's own networks are as they were.
- A packet goes to the port it is addressed to, and to no other: **a VM
  that routes for its neighbours -- a VPN gateway -- does not work**. No
  routing between networks. A VM has one network and one NIC: chosen as the
  VM is made, and changed only by deleting its port.
- TCP crosses the tunnel a packet of the MTU at a time: a send of 64 KiB is
  cut as it comes in, where on one host it is carried whole as before. The
  opening is in the receive soft IRQ, one for the machine, which NAT and
  ipc's and NBS's TCP share. ChaCha20-Poly1305 without SSE on the AX41's
  Zen 2 is 203 MB/s a core (docs/tls.md): a gigabit goes through for about
  half a core receiving. The tunnel itself is to be measured.
- A public address: no fragments; no reaching a VM of the same gateway at
  its public address; the guest does not know its public address; a wrong
  `host=` is packets out from an address the provider does not route there,
  which nothing here can see.
- A firewall: one a VM; sixty-four rules, eight ports and eight networks a
  rule; sixty-four firewalls a host and 256 a cluster; 2048 flows a VM;
  IPv6 by its network and the policy alone; fragments dropped; ICMP of every
  type at once; a forward judged as its connection is accepted, not a packet
  at a time; no log of packets, tracepoints instead.
- A host reaches only its own VMs, the gateway's address being every
  host's: `hv forward` is on the VM's host.
- A port is moved by hand; a VM restarted on another host is vmctl's
  ([vmctl.md](vmctl.md)).
- A network's MTU is not changed once made.
- Sixteen hosts, the registry's bound, and sixteen ports a host, the
  switch's, as today.
- A group of two builds: an older member answers a new command as
  malformed, differs, and is stopped by the digest. The commands carry a
  level, and every host is updated.

## Later

- **A public address inside the guest**: the way one to one goes, with
  nothing rewritten -- the port may send from a second address, and the
  table has it as the port's.
- **Several networks a VM**, a NIC for each, the first its main one: a port
  is keyed by its VM's UUID and the NIC already, and the hypervisor gives a
  guest two NICs today (`run.rs`, `MAX_NICS`); one NIC is the module's bound.
- A network's routes through a port: a VM that routes, a VPN gateway.
- IPv6, and broadcast, on the cluster's networks.
- VLANs and a device's MTU in `net`: a vSwitch as the underlay.
- A send carried whole through the tunnel; a VM reaching its gateway's
  other VM at a public address; VMs' names in DNS.
- Several firewalls a VM; rules of IPv6 addresses; ICMP by its types; a
  rule that holds a rate.
- A failover address moved through Hetzner's API, 90 to 110 seconds a
  move; and a check that an address is routed to the host named.
- vmctl and live migration. Kept ready: a port by its VM's UUID, a host by
  its node, a move a compare and set, the port's host asked before a move --
  the place for a fence.

## Found on the way

Not vnet's, and to be closed or decided apart from it:

- A `member` file beside an empty `raft/` started as the incarnation it
  was: a vote forgotten. `nbs-mds` and `cluster` had it; V1a closed it,
  with a `begun` mark written before the first segment.
- [cluster.md](cluster.md) has `cluster remove` refused while a service's
  data is on the host; the registry's `Remove` refuses nothing of the kind.
- To be checked on the boxes: Hetzner has a server reach its subnet's
  neighbours through the gateway, and `Device::route_ip` sends to an address
  on the device's subnet directly. Two hosts on one subnet may never resolve
  each other -- ipcnet's too.
