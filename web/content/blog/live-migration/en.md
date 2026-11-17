---
title: "Live migration: moving a running VM to another host"
description: "A running VM in a nos cluster can now move to another host without stopping. How its memory is copied while the guest keeps writing to it, how two hosts agree on which of them runs it, what happens to its network and its CPU, how we tested it, and what it can't do yet."
date: 2026-11-17
tags: [hypervisor, storage, networking, safety, testing]
---

With [NBS](/blog/nbs-block-storage) a VM's disk stopped belonging to one server, and with [vnet](/blog/vnet-networks-across-hosts) its network did too. One thing still tied a running VM to the host it was started on: its memory and the state of its processors and devices. Now that is gone as well. A running VM can move to another host of the cluster without being stopped. Here a Debian VM with 1 GiB of memory moves from one host of our demo stand to another:

```
> hv migrate mt-deb ax41b
hv: mt-deb offered to ax41b: 1024 MiB, 1 cpu, 3 files of its folder
hv: mt-deb round 0: 279 MiB of its memory (190489 pages of zeros) and
    118 KiB of files in 2820 ms, the pages it wrote meanwhile marked
hv: mt-deb sent in 2 rounds: 281 MiB of its memory, 118 KiB of files,
    its state 2 KiB, 6 flows of its firewall -- paused 508 ms so far
hv: mt-deb runs on ax41b -- paused 860 ms; its folder here taken away
```

The guest doesn't reboot. Its processes keep running, its files stay open, and its TCP connections to other VMs survive the move. It is paused once, at the very end, for somewhere between half a second and two and a half on our stand. In the management panel the same thing is a button on the VM's page.

This post is about how that works, what we got wrong on the way, and what it can't do yet.

## What actually has to move

Less than one might think. The disk stays where it is: it is an NBS volume, already stored on three hosts, and the only thing that changes is which host writes to it. The VM's address stays the same too, because vnet gives a VM its address and MAC for the whole cluster, not for one host. What has to travel is the memory, which is nearly all of the bytes (a guest with 1 GiB of RAM has 262,144 pages), the registers of each virtual CPU and the state of each emulated device, a few kilobytes together, and the VM's folder: its `vm.toml` and its console log.

A move starts with an offer. The old host sends the new one the VM's configuration and a description of the CPU the guest has been shown, and the new host decides whether it can take it: is its CPU the same, does its clock run at the same rate, does the VM's network exist there, are all the disks volumes, is there room. If anything is wrong it says no and why, and the VM keeps running where it was. If it can take the VM, the new host builds it right away, with empty memory and stand-ins where its disks and its network card will be, and only then does the copying start. When the last bytes arrive there is little left to do: put the state in, swap the real disks in, and run.

## Copying memory that keeps changing

The simple way would be to pause the guest, copy its memory and start it on the other side. For a gigabyte over a gigabit link that is about ten seconds of a frozen VM, long enough for its TCP peers to start retransmitting and for anything with a heartbeat to give up on it. So we copy while it runs, and then copy again whatever it changed in the meantime, which is what most hypervisors do.

The first round sends every page. A page of zeros goes as a short note without its bytes, and a guest that hasn't been running long is mostly zeros, so the first round is often much smaller than the guest's memory. While it goes, the guest keeps writing, and we need to know which pages it wrote.

That is the dirty log's job. When a move starts, every page of guest memory is made read-only in the nested page tables, the ones the processor uses to turn the guest's physical addresses into the host's (NPT on AMD, EPT on Intel). Memory mapped in 2 MiB blocks is split into 4 KiB pages first, so that a write marks four kilobytes and not two megabytes. The guest's first write to a page then traps into the hypervisor, which marks the page in a bitmap and makes it writable again, and the guest carries on. The hypervisor's own writes into guest memory don't trap: when a disk read completes or a network frame arrives, the emulated device writes into the guest's buffers itself, so those pages are marked where the write is made.

Each round takes the marks and makes those pages read-only again, in one step under the table's lock, and sends them. The part we thought about longest is the TLB. A processor caches translations, and a virtual CPU that wrote to a page recently may still hold a writable translation for it after the table says read-only. Its next write would go through without a trap and never reach the log, and the copy on the other host would quietly differ from the original. Nothing would crash. The moved guest would just have one stale page somewhere, which is the worst kind of bug to look for later. So every round gives the nested table a new identity and waits until each virtual CPU of the guest has passed the top of its run loop, where it drops what its TLB held of the old one: under AMD-V it takes a fresh ASID, under VT-x it runs INVEPT. Only after that are the round's pages read.

Rounds go on until what is left would take less than 300 ms to send at the speed the last round went. A guest that writes faster than the link can carry never gets there, so after five rounds we start slowing it down: its virtual CPUs lose 30% of their time, and 10% more each round after that, up to 90%. After thirty rounds the move is given up and the VM stays where it was.

Then the last round. The guest is paused, its disk requests in flight are allowed to finish, and its state is taken at once. After that go the pages it wrote since the previous round, along with the pages of its virtio rings, which the devices write through mappings of their own that the log can't see. We got the order wrong the first time. The state used to be taken after the last pages had been sent, and the guest woke up on the new host with its clock pushed forward by however long those pages took. Linux noticed and said so in its log ("clocksource: Long readout interval"). Now the state is taken the moment the guest stops.

## The state

The state is everything that isn't memory. For each virtual CPU that means its registers in a form both AMD-V and VT-x can produce: the general and control registers, the segments, the system MSRs, the debug registers, the XSAVE area with the vector registers, an event waiting to be delivered, the interrupt shadow. Then each local APIC, and every emulated device: the timers, the serial port, the interrupt controllers, the virtio queues. Times are kept relative to the moment of the pause, and the guest's TSC goes as an offset, so the guest's clocks carry on from where they stopped and the pause itself is left out.

The new host treats the state as input it doesn't trust, since it came from another machine. Every field is checked against its limits as it is read, and the XSAVE area is checked once more when it is loaded into the CPU. `hv save` and `hv restore` use the same format to write a VM's state and memory into a file on one host and start it again from there, and that is how most of the state code was tested before a second host was involved at all.

## Which of the two hosts runs it

This was the question we spent the most time on. If messages between the hosts get lost at the wrong moment, or one of them dies halfway through, both might decide to run the VM, and then two copies of one guest write to one disk. Or neither runs it. The second is bad. The first is much worse.

So the decision is made in exactly one place, NBS's metadata, which is a Raft group of its own. Every volume has a writer, and every writer has a generation. When the old host has sent everything, it tells the new one which generation of the first volume is its own. The new host commits by attaching that volume for writing, on the condition that the writer is still exactly that generation, which is a compare-and-set in NBS's log. The old host, if it hears nothing back within a minute, takes the volume back itself, from the same generation. Whichever of the two lands first wins, and the other is refused with `NotTheWriter`. That refusal is told apart from no answer at all: a request that got no answer is sent again with the same token, and NBS answers a repeated request the way it answered the first one, so a retry can never win a second time.

A host that can't find out how it ended runs nothing. The VM is left stopped, its volume belongs to whichever side took it, and someone runs `hv start <vm> force` on the host where it should run. We'd rather have a stopped VM that a person has to start than a VM running on two hosts.

## Its network

This part is shorter because vnet had done most of the work already. A VM's address and MAC come from vnet and are the same on every host, so the guest's ARP cache stays correct, and its gateway exists on every host as well. When the new host commits, it tells vnet that the VM's port is its own now. Before this change, each host's hypervisor asked vnet for its table once a second. Now vnet tells the hypervisor on each host about every change as soon as it applies it, without waiting to be asked.

Until a host has heard, it still sends the VM's packets to the old host, and the old host passes them on to the new one for ten seconds. That took one exception in the tunnel. Normally a host accepts a packet only if its sender's address is a port on the host that sealed it, so that no host can speak for another host's VMs. The new host now also accepts packets from the old one for the port that has just arrived from it, for those ten seconds, even when their sender sits on a third host. The old host had the whole VM in its memory a moment earlier, so this gives it nothing it didn't already have.

The firewall's state moves too. A VM's firewall remembers its connections so that their replies get in, and those flows go along with the state and are put into the VM's new port before it runs. Without them, replies to the guest's own connections would be dropped on the new host until it sent something again.

What doesn't survive is a connection masqueraded through the old host. A VM without a public address reaches the internet through NAT on the host it runs on, and that NAT's table stays behind. The guest's long-lived connections to the outside world are cut and have to be opened again.

## Its CPU

When Linux boots, it asks the processor what it can do (the CPUID instruction) and decides once what to use: which instructions, which vector registers, how large a save area. If the new host gave different answers, the guest would keep relying on what the old one had promised and could crash on an instruction the new one doesn't have. So the offer carries the old host's answer to every CPUID question a guest of that shape can ask, the new host compares them with its own, and refuses the VM if any differs, naming the first one that does. AMD and Intel never mix, and the two hosts' TSCs have to run at the same rate, within 250 parts per million.

The EX44, our only Intel server, taught us something here. Its Core i5-13500 has six fast cores and eight efficient ones, and our hypervisor answers a guest's CPUID from whichever core the guest happens to ask on. The two kinds of core describe their caches differently and report different clock frequencies in leaf 0x16. So the first time we tried this on the EX44, a VM saved on a fast core was refused its restore on an efficient one, on the very same machine. Those leaves are now left out of the comparison. They describe the processor and don't enable anything, a guest reads them once at boot, and its timekeeping comes from leaf 0x15, which is the same on every core. `hv info` now checks that all of a host's CPUs agree:

```
cpu model  c99c33c85afad94a -- what a guest moving here is held to,
           alike on the 20 CPUs
```

## How the bytes travel

Everything goes over the cluster's own message channel, the one NBS and vnet already use: TCP between the hosts with TLS on top and each side's key pinned, so a VM's memory is encrypted on the wire with no extra work. One limit of that channel shaped the protocol. A mailbox drops any message over 512 KiB, so a migration message is at most 496 KiB, and a message of pages carries the bytes of at most 112 of them. The old host keeps at most 2 MiB or 32 messages in flight and waits for the new host to acknowledge them. Our first version counted only bytes, and a burst of small messages could overflow the new host's mailbox, which also limits how many messages it holds.

## In the panel

A running VM's page now has a "Move to another host" button. It asks for the host, picked from the cluster's hosts, and whether to pause the VM for the whole copy instead of copying it live. The move runs as a job: its log shows the lines the command prints, its progress is the share of memory sent so far, and cancelling it before the last round is over leaves the VM running where it was. After the last round it can no longer be cancelled, because from then on the decision is NBS's.

## How we tested it

Most of the logic was tested on the host machine before it ever ran on a server. The fuzzer has targets for the state format (hostile bytes are refused without a panic, and every state reads back as it was written), for the messages, and for the handover, which plays both sides over a model of NBS's writer with every message lost, delayed or repeated and either host dying at any step, and checks that the two never both run the VM. The dirty log has a target of its own with simulated TLBs on several CPUs writing while rounds are being copied: once the guest is paused and the last round sent, the copy has to equal the memory, page for page. The network got two more. The tunnel target, which runs three hosts' real switches and tunnels over a wire that loses and reorders datagrams, now also moves ports between hosts while the others' tables are out of date. Another target moves a port's firewall flows to a second switch and checks that the new port refuses nothing the old one would have let in. For the rules we cared most about, we also took each one out of the code in turn and made sure the fuzzer noticed.

Then the end-to-end test: three nos instances under QEMU on a Mac, with a Linux guest on an NBS volume and a vnet network. The guest counts in its memory and writes files of random bytes with their checksums the whole time, while a probe on the third host pings it every 0.3 seconds. It is moved from a to b live, back to a with the whole copy paused, and to b live again. After each move every file has to be whole, the counter has to have carried on, the guest's uptime has to have gone up, its kernel must not have complained about its clock, the probe's pings have to be answered again, and NBS has to name the new host as the volume's writer.

`hv logcheck` tests the dirty log on a real machine without moving anything. It runs the copy rounds against a running guest on one host, then pauses the guest and compares every copied page with what is in its memory. On the EX44 it passed with one virtual CPU and with two, on VT-x, which our emulated tests can't reach, and so did saving and restoring a VM three times in a row.

The tests found real bugs, and a fake one. The first "corruption" the end-to-end test reported came from the test: it read a file twice while the guest was renaming another file over it. When we added the network to that test, it found that a VM with a network card arrived on the new host without its network, because the network's name lost its `vnet:` prefix twice on the way. And the tunnel target, once it could move ports, found an older bug that had nothing to do with migration: when the message carrying a key got lost, the tunnel reported the link as unable to carry the networks' MTU, and afterwards stayed silent about a link that really couldn't.

## On the demo stand

The numbers at the top of this post come from our demo stand, and the stand is a little unusual. Of its three hosts only the EX44 runs nos directly on its own hardware. The other two, ax41a and ax41b, are nos VMs under Linux KVM on a single AMD Ryzen 5 3600 machine, so a guest on them runs under our AMD-V inside KVM's, and the bytes between them cross the cluster's TLS connections between two VMs on one box. The EX44 is Intel, so nothing moves between it and the other two. What moves is between ax41a and ax41b, and it talks to VMs on the EX44 while it moves.

The Debian VM above, a clone of an NBS image with 1 GiB of memory and one CPU, moved four times. The two moves we timed took seven and eight seconds from the offer to running on the other side. The first round always sent about 279 MiB in 2.4 to 2.8 seconds, and the rest of its memory was zeros. A second round of a few hundred pages was enough every time. The pause was 860, 2418, 1736 and 939 ms. A VM on the EX44 pinged it every 0.2 seconds during two of the moves: 5 of 100 pings went unanswered during the first, and 12 during the second, the one with the long pause.

We turned the migration's tracepoints on for the last move, on both hosts, to see where the 939 ms went. Pausing the guest, sending the last 409 pages and its state took about 15 ms. The other 920 ms were spent waiting for messages: about a third of a second passed between the last page reaching the new host and the old host's final message, more than half a second went to NBS handing the volume over, and starting the guest took 50 ms. So the pause is no longer about memory at all, and the handover's round trips are where it can get shorter.

TCP was the other thing to check. A small test VM on ax41a sent a line with a number every 50 ms to a VM on the EX44 over one TCP connection, and we moved it to ax41b. Its firewall let nothing in except the flows of its own connections, so on the new host the replies could only get in through the flows that had moved with it. All 3,869 lines arrived, in order, none missing, and on ax41b it was still the same connection. For the move back we had the receiver note when each line arrived: the longest it waited for one was 1.25 seconds, and the guest had been paused for 1.2.

The stand also found a bug no test had. ax41b had never had a VM, so it had no folder for VMs at all, and the first move there was refused because the new host made the VM's folder without making the folder above it first. In the emulated test every host had made a VM before.

## What it can't do yet

Both hosts need the same processor, as far as the guest can tell, from the same vendor. Every disk the VM writes to has to be an NBS volume; a file or a disk of the host's own can't follow it. Its network card has to be on one of the cluster's networks, not on the host's own. A VM with a port forwarded to it from its host (`forward=` or `hv forward`) or with a public address mapped by its host's NAT (`public=`) stays where it is. A host takes in one VM at a time. Connections masqueraded through the old host are cut. The pause grows with how fast the guest writes and how slow the link is, and a guest that outruns the link even when slowed down to a tenth of its time doesn't move at all.

We have one Intel server, so a live move between two VT-x hosts hasn't happened yet: VT-x has been tested on its own, by saving and restoring and by checking the dirty log, and the moves themselves ran between AMD hosts. And nothing moves VMs by itself yet. Emptying a host before maintenance and spreading VMs across hosts are the next things to build, on top of this.
