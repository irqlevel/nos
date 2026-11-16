---
title: "vnet: one private network for VMs on different servers"
description: "VMs on different hosts of a nos cluster can now share private networks. How vnet keeps its state, how packets cross between servers in a sealed UDP tunnel, public addresses, firewalls, what happens when a host fails, and how to use it."
date: 2026-11-16
tags: [networking, hypervisor, safety, testing, fuzzing]
---

Until a few days ago every host in nos had its own little network for its VMs. The switch inside the hypervisor handed out the same subnet everywhere, 10.0.100.0/24, with room for sixteen ports. A VM went out to the internet through a masquerade on the host, and the only way in was `hv forward`, which relays one TCP port. So two VMs on two different hosts couldn't talk to each other, their addresses were the same on every host, and there was no way to give a VM a public address.

Once [NBS](/blog/nbs-block-storage) let a VM's disk outlive its host, the network was the obvious next problem. On 15 November we wrote down what we wanted: VMs on different hosts of a cluster on one private network, private addresses, NAT to the internet, and a public address for a VM that needs one, such as an additional IP or a small subnet rented from Hetzner. A few decisions were made right away. A public address is one-to-one NAT for now, and putting the address inside the guest itself can come later. The networks carry IPv4 and ARP and nothing else. A cluster can have as many networks as it likes, and each VM is on one of them, chosen when the VM is created. And every VM has a firewall: everything coming in is closed by default, even from its own network, everything going out is open, and the firewall keeps state.

The result is called vnet. It is three pieces: a module that keeps the networks' state on every host of the cluster, the switch in each host's hypervisor that the VMs plug into, and a sealed UDP tunnel between the hosts.

## How it fits together

```
           host A                                       host B
  VM web, 10.10.0.50                           VM db, 10.10.0.51
          |                                            |
  switch: a port on lan                        switch: a port on lan
          |   \                                        ^
          |    `-- tunnel ==== UDP 39241, sealed ===== tunnel
          |
  hv0, the gateway 10.10.0.1 -- NAT -- uplink -- the internet
```

The networks themselves live in `vnet.ko`. It is a Raft group of its own on top of the cluster's registry, the same way NBS's metadata service is, so every host keeps the whole state and they all agree on it. The state is the networks (each with a private subnet from /16 to /28, an MTU, and a way out to the internet or none), the ports (one per VM's network card, with its address, its host and its firewall), the public addresses and the cluster's firewalls. Raft's apply step reads no clock and no registry, so every member computes the same state, and every 64 entries the members compare a digest of it. A member whose digest differs from the leader's stops serving rather than hand out a different picture of the network.

The hypervisor on each host doesn't talk Raft. A small task in it, the keeper, asks vnet for this host's table once a second, and gets the table back in full only when something in it has changed. When a VM starts, its port is attached with a read the Raft leader confirms, waiting up to five seconds for one.

## A VM on a network

A VM is put on a network when it is created, with `network=vnet:lan`. Its port, address and firewall come from vnet. The guest learns its address, gateway and MTU three ways: on its kernel command line (`ip=`), from DHCP, which the switch answers itself, and from the virtio NIC, which advertises the MTU. Its MAC address is `02:00` followed by the four bytes of its IP address.

The switch is strict about what it carries. On a cluster network it passes IPv4 and ARP and drops everything else, and nothing is broadcast. It answers ARP itself: for the gateway with the host's MAC, for a VM of the same network with that VM's MAC, wherever the VM is running. Every network's gateway is the network's first address, and it lives on every host, so a VM always goes out to the internet from the host it runs on. There's no central router whose loss would cut everyone off. A packet larger than the network's MTU gets "fragmentation needed" back from the gateway, and two networks never see each other's packets.

## Across servers: the tunnel

When the destination VM is on another host, the switch hands the packet to the tunnel. Each packet crosses the wire as one UDP datagram:

```
Ethernet | IPv4, DF | UDP 39241 to 39241, checksum 0 |
  kind 1, network 3, key 4, counter 8 | the IPv4 packet, sealed | tag 16
```

The tunnel carries the IP packet, not the whole Ethernet frame. The receiving host builds the Ethernet header itself, from its own table. Our first sketch looked like Geneve; what we built is closer to WireGuard's data message, and there is less to parse. We chose UDP over the TCP links the hosts already share because inside TCP, one lost segment on the link between hosts would stall every guest connection behind it, and the guests' retransmission timers would fight TCP's own.

Every datagram is sealed with ChaCha20-Poly1305. The counter is the nonce and the header is the associated data, so a datagram whose network, key or counter was changed on the way doesn't open. Keys are handed out in a way that needs no agreement: each host gives every other host the key to seal datagrams *to it* with. The key travels over the cluster's message channel, which is TLS with both sides' keys pinned. A new key is made for every message, given out again every hour (`rekey=3600`), and the previous one is kept for two minutes (`keylife=120`), so that packets sealed just before a change still open. There is no race over who goes first, a reboot simply means new keys, and a host removed from the registry loses its keys with it.

On the receiving side the tag is checked before a single byte is decrypted, and a window of the last 2,048 counters drops anything replayed or older. The sender is identified by the key number, not by the source address. That matters because two of our three hosts sit behind one NAT, and the address and port a datagram arrives from say nothing reliable about who sent it.

The tunnel also keeps an eye on the hosts. Every five seconds it sends an echo through the tunnel to each host it has a key for, and every third echo is as large as the largest MTU of any network. A host that hasn't answered for 20 seconds gets a line in the log, and its key is asked for again. A link that carries small echoes but not big ones gets a line too, because that means the network underneath can't carry the MTU the networks were given. The tunnel adds 60 bytes to each packet, so on an ordinary 1,500-byte link a network's MTU can be at most 1,440. The default is 1,420.

Packets are sealed on the CPU of the guest that sent them and opened in the receive soft IRQ. Since 16 November both go through CRYPTOGAMS assembly instead of portable Rust. On the Apple M-series core where we measured it, a 1,400-byte datagram now takes about 1.2 µs to seal instead of 2.7. We haven't measured the servers' x86 yet.

## Public addresses

A public address is routed by the provider to one host of the cluster, and can be put on the port of a VM on any host. The host it is routed to serves it: its uplink answers ARP for the address and announces it, NAT maps it one to one to the VM's private address, and the packets travel through the tunnel to the host where the VM actually runs. The replies come back the same way and leave from the host the address is routed to, because Hetzner won't let a server send from an address that isn't routed to it:

```
in:   the world -> ex44's uplink -> NAT one to one -> hv0 -> the switch
        -> the tunnel (kind 3) -> ax41 -> the port
out:  the port -> its gateway -> the tunnel (kind 2) -> ex44 -> hv0
        -> NAT one to one -> the uplink -> the world
```

The guest itself only ever sees its private address. A VM without a public address goes out masqueraded from its own host, as before.

## Firewalls

Every VM's network card is judged at its port by exactly one firewall. Incoming packets are denied unless a rule lets them in, outgoing packets are allowed, and replies to the VM's own connections are let back in. Rules are checked in order and the first match decides, and up to 2,048 flows are tracked per VM. A firewall is a named set of rules such as `allow in tcp 80,443`, and any number of VMs can share one. Cluster networks use the cluster's firewalls, which live in vnet's state. A VM that names none gets `default`. A change to a firewall reaches every host within a second, and flows the new rules deny are cut. An outgoing packet that a rule denies gets an answer ("administratively prohibited") instead of silence, so a program inside the VM fails fast instead of timing out.

## When something fails

The design rule was that the data path shouldn't depend on Raft being healthy. If vnet has no leader, packets keep flowing by the last table. A VM that starts in the meantime, with its port already on this host, gets it "unconfirmed" until the leader is back, while a brand-new port has to wait for one. If `vnet.ko` is unloaded on a host, VMs that are already running keep working, and new ones on cluster networks are refused until it is back. If another host reboots or reloads its hypervisor, both sides make new keys, and there are a few seconds of loss. A host that goes silent is noticed by its echoes. A port that is moved or deleted while its VM is running gets shut: the VM keeps running, but nothing reaches it or leaves it.

Two bugs from the development are worth telling. The first one was caught by the fuzzer. Its `tunnel` target runs three hosts' real switches and tunnels over a simulated wire that loses, repeats, damages, reorders and forges datagrams, and on a third of its runs two of the hosts sit behind a NAT. When we taught it to move a host's port under traffic, it found a nonce used twice. When a host's address changed, its entry in the tunnel was rebuilt, and the list of key numbers already used went with the old entry. A key message that arrived a second time was then accepted as new, and its counter started again from zero. With a stream cipher that is about as bad as it gets. The bug had been there since the first version of the tunnel; nothing had reached it because addresses never moved in the earlier tests.

The second one only showed up on the real servers. A VM on a cluster network couldn't start at all if it had been made the usual way, with a folder and a `vm.toml`. A helper that remembers a VM's switch port at its first start wrote `port=` into its config, and VMs on cluster networks refuse `port=`, since vnet owns their ports. It was one of four problems the stand found that no gate had.

Altogether the hypervisor's fuzzer has 99 mutants for the firewalls, the NAT, the cluster networks, the tunnel and the public addresses. Each one removes a single rule, and each one is caught. vnet's state has its own fuzzer target and 18 mutants of its own, and `tests/vnet-test.py` runs the whole thing on three nos with probes standing in for VMs.

## Numbers

On 15 November we measured it on the demo stand: the EX44 on its own hardware, and ax41a, a nos running under KVM on the AX41, behind the AX41's single address and its NAT. Each ran an Alpine VM whose disk was a clone of an NBS image, so the VM on ax41a was a guest inside a guest. iperf3 for 20 seconds gave 861 Mbit/s of TCP from the EX44's VM in a single stream, about what a gigabit link carries once each packet is 60 bytes bigger, and 833 Mbit/s with four streams. The other way it gave 470 Mbit/s, with the nested VM sending. Ping was 0.52/0.66/1.02 ms min/avg/max. 11.4 million datagrams went one way and 4.2 million the other, and none were lost or refused in the tunnel. 109 weren't sent because the EX44's network card had a full ring at that moment.

That was still with the cipher in portable Rust. After moving it to CRYPTOGAMS today, we ran a quick check on the same stand: a probe on ax41a pinged a VM on the EX44 20 times out of 20, with 32 datagrams each way and none forged or replayed. We haven't run iperf3 again yet.

## Using it from the panel

In the [management panel](/blog/management-panel) every host has a page called VM networks. It starts with Make a vnet and Add a host. After that, New network asks for a name, a subnet, an MTU between 576 and 1440 (1,420 by default) and whether the network has a way out to the internet. The page lists what needs attention, the networks, the ports, the public addresses, the firewalls with their rules, and vnet's members. A port can be moved, deleted or given another firewall. A public address can be put on a port or pointed at another host when the provider routes it elsewhere. A firewall gets rules with Add a rule, and its two policies, for packets no rule matches, are set with Policies. Each rule can be taken out or moved up and down from its own table.

[![The VM networks page on the EX44: nothing to look at, the network lan with its subnet, gateway, MTU 1,400 and a way out, two ports on it, one on ax41a and one on ex44, and no public addresses](/images/vnet-networks-across-hosts/en-vnet-networks.webp)](/images/vnet-networks-across-hosts/en-vnet-networks.webp)

*The VM networks page on the EX44. The network lan has two ports, one on ax41a and one on the EX44, both judged by the firewall bench. No public addresses yet.*

[![Further down the page: the firewalls default and bench, inbound denied and outbound allowed, their rules, and vnet's three members with ax41a leading](/images/vnet-networks-across-hosts/en-vnet-firewalls.webp)](/images/vnet-networks-across-hosts/en-vnet-firewalls.webp)

*Further down: the cluster's two firewalls, their rules, and vnet's three members, with ax41a leading.*

When you create a VM, the network list offers the host's own networks and the cluster's, written `vnet:<name>`. On a cluster network the firewall list switches to the cluster's firewalls, and the public address field disappears, because public addresses belong to vnet and are put on ports from the VM networks page.

[![The bottom of the Create a VM form: the network vnet:lan chosen, and the cluster's firewall default](/images/vnet-networks-across-hosts/en-create-network.webp)](/images/vnet-networks-across-hosts/en-create-network.webp)

*The VM form with a cluster network chosen. The firewall list is the cluster's now, and the public address field is gone.*

## Using it from the shell

`vnet.ko` needs the cluster's registry first ([the cluster](/docs/cluster)). On every host:

```
$ insmod /lib/modules/vnet.ko
```

On one of them, make the group and add the other hosts:

```
$ vnet init lab
vnet: lab made
$ vnet host add b
vnet: b is member 2
```

Networks, one with a way out and one without:

```
$ vnet create lan 10.10.0.0/24 mtu=1400
$ vnet create dev 10.20.0.0/24 out=no
$ vnet list
```

A VM on one of them:

```
$ hv create web /bzImage net network=vnet:lan address=10.10.0.50 firewall=web
hv: web given port 4 on vnet:lan at 10.10.0.50, judged by its firewall web
$ hv start web
$ vnet ports
```

Firewalls:

```
$ vnet fw default add allow in from net
$ vnet fw create web
$ vnet fw web add allow in tcp 80,443
$ vnet port firewall 4 web
```

The first line is the one you almost always want: it lets a network's VMs reach each other. Without it, inbound traffic is closed even inside the network.

A public address routed to host `a`, put on port 4 wherever its VM runs, and taken off again:

```
$ vnet ip add 203.0.113.9 a
vnet: public address 203.0.113.9, routed to a
$ vnet ip set 203.0.113.9 4
vnet: 203.0.113.9 on port 4
$ vnet ip set 203.0.113.9 none
```

`hv tunnel` shows each host, whether its keys are in place, when its echo was last answered, and what was sent, received and dropped, with the reason for each drop:

```
hv tunnel: instance 5a1c..., node 1, at 10.9.0.1:39241
  b                10.9.0.2:39241        hop known  key to it had  keys given it 2  echo answered 3 s ago, a full one 13 s ago
  sent 1873, received 1869; not sent: no key 2, no room 0, no hop 0; not taken: unknown 0, replayed 0, forged 0, stale 0; echoes answered 41
```

To check a network before any guest boots, `hv probe add p1 network=vnet:lan` makes a port with no VM behind it. It answers ARP, ping and UDP to port 7, and `hv probe ping p1 <address>` pings from where a guest would be.

## What it doesn't do yet

Cluster networks carry IPv4 and ARP only, so no IPv6 and no broadcast or multicast between VMs. A VM has one network and one network card. A packet goes only to the port it is addressed to, so a VM that routes traffic for its neighbours, a VPN gateway for example, won't work. A network's MTU can't be changed once it is made. TCP crosses the tunnel one MTU-sized packet at a time, where on a single host a large send is carried whole. A cluster has at most 16 hosts, and a host's switch at most 16 ports. On the list for later are a public address inside the guest itself, several networks per VM, IPv6, failover addresses moved through Hetzner's API, and live migration of VMs between hosts, which will need both vnet and NBS.

The full description is in [the vnet documentation](/docs/vnet), the design and its history in [plans/vnet.md](https://github.com/irqlevel/nos/blob/master/plans/vnet.md).
