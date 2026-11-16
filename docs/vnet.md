# vnet: the cluster's networks for VMs

`vnet.ko` (`src/crates/modules/net/vnet`) keeps the networks a cluster's VMs
share across its hosts -- each a private subnet of its own, its ports, the
public addresses routed to the hosts, and the cluster's firewalls -- on
every host of the cluster, through Raft. hv (`hv.ko`) carries a VM's
packets on one: through the switch on its own host, through a sealed tunnel
to a VM of the same network on another host, out to the world masqueraded
from its own host or from a public address one to one. The design, the
decisions behind it and what each stage built are in
[plans/vnet.md](../plans/vnet.md); this page is how it is used and what it
does.

```
 guest - virtio-net - port -+                      +- port - guest
                            the switch (host A)    the switch (host B)
 vnet.ko == the bus ==> hv: where each port is,         ^
 (Raft; the registry)        the public addresses       |
                          hv0      tunnel === UDP 39241, sealed === tunnel
                            |     (keys over ipc: hv.tunnel@B)
                 NAT: masquerade | one to one -- eth0 -- the world
```

## Making them

vnet stands on the cluster's registry ([cluster.md](cluster.md)): its hosts
are the registry's, by node. On one host of it, the group and its members,
as NBS's are made:

```
$ insmod /lib/modules/vnet.ko                # on every host, after cluster.ko
$ vnet init lab
vnet: lab made
$ vnet host add b
vnet: b is member 2
$ vnet host add c
```

A network -- a private subnet of /16 to /28, apart from every other network
of the cluster's and holding no host's own address, its first address the
gateway on every host:

```
$ vnet create lan 10.10.0.0/24 mtu=1400      # 1440 at most: the tunnel adds 60 bytes to a packet
$ vnet create dev 10.20.0.0/24 out=no        # no way out to the world
$ vnet list
```

## A VM on one

A VM is on one network, chosen as it is made, written `network =
"vnet:<name>"` in its vm.toml -- and nothing else about it: its port, its
address and its firewall are vnet's.

```
$ hv create web /bzImage net network=vnet:lan address=10.10.0.50 firewall=web
hv: web given port 4 on vnet:lan at 10.10.0.50, judged by its firewall web
$ hv start web
$ vnet ports                                 # every port, its network, address and host
$ hv delete web                              # its port deleted with its folder
```

- **The port is made at `hv create`**, or at the VM's first start, and
  attached at each start with a read the Raft leader vouches for -- up to
  five seconds for one; with none, a port the host's own state binds here
  starts unconfirmed, said in `hv list`, and is asked again until it is
  confirmed.
- **The guest is told** its address, its gateway and the MTU on its command
  line (`ip=`), by DHCP (option 26 for the MTU) and by its NIC
  (`VIRTIO_NET_F_MTU`). Its MAC is `02:00` and its address's four bytes.
- **On a network the switch carries IPv4 and ARP alone**: ARP and DHCP
  answered by the switch, nothing broadcast, another network never reached,
  a packet past the MTU answered "fragmentation needed" from the gateway.
  `hv list` counts what each port's VM sent that went nowhere.
- `hv probe add <name> network=vnet:lan` makes a port with no VM behind it,
  which answers ARP, echo and UDP to port 7 and pings from where a guest
  would be: what a network is checked with before any guest boots.

## Firewalls

Every VM's NIC is judged at its port by one firewall: what comes in denied
unless a rule lets it, what goes out allowed, the answers to a VM's own
flows let back. A VM on a cluster network is judged by a firewall of the
cluster's, kept in vnet's state -- `default` unless it names another --
written and read as hv's own are ([The firewall](hypervisor.md#the-firewall)):

```
$ vnet fw default add allow in from net      # a network's VMs reach each other
$ vnet fw create web
$ vnet fw web add allow in tcp 80,443
$ vnet port firewall 4 web                   # port 4 judged by web from the next table on
```

A change of a firewall's rules, and a port moved to another firewall, reach
every host within a second of being made -- hv's keeper asks vnet's table
every second -- and the flows the new rules deny are cut.

## Across hosts

A VM reaches a VM of its network on another host as it reaches one on its
own: the switch carries the packet into the tunnel, sealed into a UDP
datagram to the other host's address, opened there into the port's inbox
([Across hosts: the tunnel](hypervisor.md#across-hosts-the-tunnel)).

- **Where a host is**: the registry's address for it -- its `underlay`
  one, or its `ipc` one where it has none -- at that address's port where
  it has one of its own, else UDP 39241. A host behind a NAT is reached at
  the NAT's address and the port the NAT forwards to it; two behind one
  address have ports of their own, the ones their ipcnet has already, and
  the NAT forwards each in UDP as it does in TCP:

  ```
  $ cluster invite b 203.0.113.7:39241 <key>    # b and c behind 203.0.113.7: 39241 -> b, 39242 -> c
  $ cluster invite c 203.0.113.7:39242 <key>    #   TCP for ipcnet, UDP for the tunnel
  $ cluster address b underlay 198.51.100.4:40000   # or the tunnel elsewhere than ipcnet
  ```

  A datagram is known by its key, never by where it came from: whatever
  address and port the NAT gives it, it is its sealer's. A host's address
  changed in the registry reaches every tunnel with vnet's next table.

- **Sealed** with ChaCha20-Poly1305, each host giving every other the key to
  seal to it with, over the message channel (`hv.tunnel`), made anew for
  every message, given anew every `rekey=` seconds (3600) and the one
  before kept `keylife=` (120): `insmod /hv.ko rekey=S keylife=S`. A
  counter seen before, a key not given, a tag that does not hold, a packet
  from a port the receiver's table has elsewhere: dropped, counted.
- **An echo** every five seconds to each host, every third one as large as
  the networks' largest MTU: a host that stops answering is said in the log
  and its key asked for anew; a way that loses the large echoes and not the
  small ones is said too -- an underlay short of the networks' MTU.
- **`hv tunnel`** says each host -- its next hop, its keys, when it last
  answered -- what this host serves, and what was sent, taken and dropped,
  and why; `trace on tunnel` records each drop and each key
  ([tracing.md](tracing.md#the-tunnels-events)).

## Public addresses

A public address is routed by the provider to one host of the cluster's,
and may be put on the port of a VM on any host: the world reaches the VM at
it, and the VM goes out from it, one to one -- the guest sees its private
address alone.

```
$ vnet ip add 203.0.113.9 a                  # routed to a by the provider
vnet: public address 203.0.113.9, routed to a
$ vnet ip set 203.0.113.9 4                  # onto port 4, wherever its VM runs
vnet: 203.0.113.9 on port 4
$ vnet ip move 203.0.113.9 b                 # the provider routes it to b now
$ vnet ip set 203.0.113.9 none               # off the port: it goes out from its own host again
```

The host it is routed to serves it: NAT maps it one to one to the port's
address, ARP on its uplink answers for it, and the world's packets go
through the tunnel to the VM's host; the VM's packets for the world come
back the same way and leave from the address
([A public address across hosts](hypervisor.md#a-public-address-across-hosts)).
A VM with no public address goes out masqueraded from its own host. A
public address on a network that does not go out is not served.

## When things fail

| What happened | The packets | What is seen |
|---|---|---|
| vnet has no leader | Go on by the last table; a port bound here attaches unconfirmed | "unconfirmed" in `hv list`, asked again until vouched for |
| vnet.ko not loaded, or unloaded | VMs running go on by the last table; a new one on a cluster network refused | "vnet is gone from this host" once, in the log |
| A port moved, deleted, or of another generation | The VM keeps running, its port shut: nothing to it or from it | `hv list` says why |
| Another host rebooted, or its hv loaded anew | New keys both ways; seconds of loss | `hv tunnel`: the keys, the echoes |
| A host silent | Nothing to its VMs | "answered no echo", in the log, and its key asked for anew |
| The underlay does not carry the networks' MTU | The large packets lost | "does not carry a packet of N bytes", in the log |
| Two hosts at one address and port -- behind one NAT, with no port of their own | What is sent to one reaches whichever the NAT forwards it to | "both at ...: two hosts behind one address need ports of their own", in the log |
| A public address's host down | The world does not reach the VM at it | `vnet ip set ... none`, or `vnet ip move` once the provider routes it elsewhere |

## Limits

IPv4 and ARP alone on the cluster's networks; no broadcast between ports;
one network and one NIC a VM; a packet goes to the port it is addressed to
and no other, so a VM that routes for its neighbours does not work. TCP
crosses the tunnel a packet of the MTU at a time. What is left -- a move
refused while its VM runs, the alerts of a public address's host -- is in
[plans/vnet.md](../plans/vnet.md).

## The gates

- `tests/hv-fuzz.py` (host, in CI): `vnetswitch` (the switch's rules for
  the cluster's networks, held to a model of its own), `vnetwire` (the
  datagram, its window, the keys' messages), `tunnel` (three hosts' real
  switches and tunnels over a wire that loses, repeats, damages, reorders
  and forges, public addresses through the real NAT); `--mutants`: ninety-nine
  rules taken out, each to be found.
- `tests/raft-test.py` (host, in CI): vnet's state (`vnetmeta`).
- `tests/vnet-test.py` (by hand, on `tests/hub.py`): `--phase v1b` the group
  and its state on three hosts, `v2` one host's networks with probes, `v3`
  the tunnel between three, `v4` public addresses across them
  ([testing.md](testing.md)).
