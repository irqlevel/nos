---
title: "nos on a server with two sockets"
description: "We brought nos up on a Supermicro with two Xeons and 256 GB of RAM and taught it that memory on the other socket is slower. How the kernel finds out where its memory is, what it does about it, what we measured, and what is still missing."
date: 2026-11-19
tags: [memory, hardware, performance, hypervisor, x86-64]
---

Until now nos had only run on machines with one processor. The biggest one it runs on now is a Supermicro X10DRi we call Newton: two Intel Xeon E5-2670 v3 with twelve cores and 24 threads each, 256 GB of RAM, two Intel SSDs and a two-port Intel I350 network card. These processors came out in 2014, so it's not a new machine, but it has something none of our other machines had. It has two sockets, and with two sockets not all memory is the same.

This is what nos says about it now, half a minute after boot:

```
$ numa
node 0 (domain 0): CPUs 0-11,16-27 (24)
  memory 0x0-0x80000000, 2048 MiB
  memory 0x100000000-0x2080000000, 129024 MiB
  free 120731 MiB, 8068 MiB of it zeroed; huge pool 8122 MiB, 8122 MiB of it free
node 1 (domain 1): CPUs 32-43,48-59 (24)
  memory 0x2080000000-0x4080000000, 131072 MiB
  free 122808 MiB, 24259 MiB of it zeroed; huge pool 8262 MiB, 8262 MiB of it free
distances:
        0    1
   0   10   21
   1   21   10
```

## Getting it to boot

We can't reach Newton's serial console, and its network gives out no addresses over DHCP, so the first boot had to work blind. The installer copies the static address Ubuntu uses into nos's kernel command line, nos sends its log over the network to a collector on another server, and before rebooting we armed the board's BMC watchdog from Ubuntu. On this board that watchdog keeps counting through the firmware's POST, so if nos had hung, the machine would have rebooted back into Ubuntu by itself within the hour.

It wasn't needed. nos came up with all 48 threads, all 256 GB, both disks and both network ports, and its SSH server answered 16 seconds after GRUB handed over. One thing was wrong, and it was the one thing that is supposed to save you when everything else goes wrong.

Intel chipsets have a hardware watchdog, the TCO timer, which resets a machine whose kernel stops responding. nos arms it on every boot. Chipsets from the 100 series on keep the timer's I/O address in register 0x50 of their SMBus controller, and for older chipsets our code read register 0x50 of a different device, the LPC bridge. On the C610 chipset in this server that register holds something else entirely (0xF8). So nos armed a timer on I/O ports where nothing was listening, and reported success. On these older chipsets the timer sits at a fixed offset of 0x60 from the power management base, and the bit that stops it from rebooting the machine is kept in another register, inside a memory window of the chipset. nos now looks in the right place, clears that bit and reads it back to check that the change stuck. Because the window's address comes from a chipset register, we also made the kernel refuse to map device memory over ordinary RAM, in case some other chipset hands back a nonsense address.

## Why two sockets change things

Each processor has its own memory controllers and its own DIMMs. A core can still read the other processor's memory, but the request has to cross the link between the sockets (QPI on this generation) and come back, and that trip takes time. The firmware describes the layout in two ACPI tables. The SRAT says which CPUs and which ranges of memory belong together, and each such group is called a NUMA node. The SLIT gives the distance between every pair of nodes, where 10 is a node's distance to itself. On Newton the distance to the other node is 21.

Until now nos read neither table. It kept every free page in one list sorted by address and handed out the lowest pages first. On Newton the lower half of the addresses is the first socket's memory, so a program running on the second socket was given the first socket's memory. Everything worked, just slower than it had to.

To see how much slower, we wrote a small kernel module called numabench. It takes a gigabyte of memory on each node, and then a task on each node reads every node's gigabyte in two ways. First it follows a chain of pointers through the memory in random order, so the CPU can't guess the next address and every load has to wait for the one before. That gives the latency of a single cache line. Then it reads the whole gigabyte in order, which shows how fast one core can stream it.

```
numabench: 1024 MiB a node, 8000000 loads a pair, 2 nodes
  CPUs of    memory of       load ns    read GB/s    its pages
  node 0     node 0            137.6         8.16         100%
  node 0     node 1            193.2         6.37         100%
  node 1     node 0            193.3         6.81         100%
  node 1     node 1            137.9         8.67         100%
```

A cache line from the other socket's memory arrives 55 nanoseconds later, which is about 40 percent more, and one core reads that memory roughly a fifth slower. The latencies include TLB misses, because the module uses ordinary 4 KiB pages and a random walk over a gigabyte misses the TLB almost every time. Every row pays that cost equally, so the gap between the rows is what the other socket costs. The last column checks the allocator: every page of each node's gigabyte really was on that node.

## What the kernel does with it now

nos reads the SRAT and the SLIT at boot, before the second CPU starts, and after that the topology never changes. If the tables contradict each other, for example two nodes claiming the same memory, a CPU listed in two nodes or an entry cut short, the kernel treats the machine as a single node, the way it always did. A broken table should cost speed, never correctness.

Free memory is now kept per node. Every free page sits in its own node's lists, and an allocation takes memory from the node of the CPU it runs on, then from the next closest node. There was one snag here. nos builds its list of free pages before it has read any ACPI table, so at first every page lands on node 0's list. Moving them turned out to be cheap. The list is sorted by address and each node's memory is one or two long stretches of it, so the kernel cuts the list at the node boundaries and moves each stretch in one step.

nos also zeroes free pages in the background while the CPUs are idle, so that memory given to a VM is already clean. That used to be two tasks for the whole machine. Now there is one per node, running on that node's own CPUs, so the zeroing never crosses between sockets.

VMs get their memory from a pool of 2 MiB pages that is set aside at boot with the `hugepages=` parameter. That pool is now split between the nodes in proportion to their free memory. With `hugepages=16G` Newton put 8,122 MiB on the first node and 8,262 MiB on the second. The first node gets a little less because the kernel keeps its own bookkeeping there, about 2 GiB of page descriptors. We also tried `hugepages=200G`. All 102,400 blocks were reserved, 101,536 MiB on one node and 103,264 MiB on the other, the kernel kept 55,152 MiB (about 54 GiB) for itself, and the boot was no slower.

The scheduler has one new rule. A task moves to a CPU on the other node only if the imbalance is bigger than what it takes to move it within its own node, because its memory stays where it was.

VMs are placed by node too. By default a VM's memory comes from the node where its first virtual CPU landed. You can also choose the node yourself, with `node=1` on the `hv start` command line, `node = 1` in the VM's `vm.toml`, or a drop-down in the management panel. Then the VM's virtual CPUs and the task that serves its disks all go on that node's cores, and its memory comes from that node. If the node doesn't exist, if the `cpu=` you also gave belongs to the other node, or if the node has no free cores left, nos says so instead of quietly putting the VM somewhere else. When a VM starts, nos checks where its memory really ended up and adds it to the end of the start line: `its memory node 1's (100%)`.

The panel shows the nodes as a table on the host's system page: each node's CPUs, memory, free memory, share of the 2 MiB page pool and distances. The form for a new VM lists the nodes along with their CPUs and how much they have free for a guest, and a VM's page shows its node and how much of its memory is there. In the shell, `numa` prints the topology, and `memusage`, `meminfo` and `memcheck` now split their numbers by node.

## How we tested it

Most of it was tested away from the real machine. nos has fuzzers that build the kernel's own C++ for the host and feed it random input. The ACPI fuzzer now generates random and broken SRAT and SLIT tables, plus Newton's real ones, and compares what the kernel makes of them with an independent reading. The page allocator's fuzzer runs on made-up machines with two and three nodes, listed in odd orders, some with CPUs and no memory, with distance tables that make sense and ones that don't. After every step it checks that each free page is on its own node's list and that each allocation got the closest node it could. Every boot runs a self-test that takes a page on each node and checks where it came from. CI boots the kernel in QEMU with two emulated nodes. We also booted it with the undefined behaviour sanitizer and Rust's extra runtime checks turned on, and ran numabench under QEMU, where the timings mean nothing but the last column still has to say 100 percent. After all that came Newton.

## What went wrong along the way

The first version of numabench could never have worked. It asked the kernel heap for 8 MiB pieces and a 4 MiB table to shuffle its random chain, but the largest block the heap hands out is 512 KiB. We noticed it while reading the allocator's code, before the first run. Now the module takes 512 KiB blocks and builds the random chain inside the memory it measures, in the first word of each cache line, using Sattolo's shuffle, which works in place, so no table is needed. The same review found a division in the pointer-chasing loop that would have added a few dozen cycles to every load we were trying to time.

Then the hypervisor's test suite failed in a place that had nothing to do with NUMA. One check reads a VM folder's listing with a regular expression that expected exactly one space between a file's size and its name. A while ago `ls` started lining up its columns, so now there were two spaces. That test doesn't run in CI, so it had been failing on master ever since without anyone noticing. It now compares words instead of spaces.

And Newton's CPU numbers have gaps: 0-11, 16-27, 32-43 and 48-59. That looked like a bug at first, but it isn't one. nos numbers a CPU by its APIC ID, and these processors number their twelve cores 0 to 5 and 8 to 13.

## What's still missing

- nos supports at most 64 CPUs, and it counts them by APIC ID. A two-socket server with more than 16 cores per socket starts its second socket at ID 64 and loses that whole socket. There's no x2APIC support either. Both are [written up as a plan](https://github.com/irqlevel/nos/blob/master/plans/many-cpus.md) and not started yet.
- Physical memory above 512 GiB isn't used.
- Some memory still isn't placed by node. The page descriptors, about 2 GiB on Newton, all live on node 0. The heap's caches of freed blocks are shared by everyone, page tables are allocated wherever a mapping is made, and the 2 MiB page pool is zeroed by whichever task gets to it first.
- Device interrupts aren't sent to the node the device is attached to.
- A task that moves to the other node leaves its memory behind, because nos doesn't migrate pages.
- A VM bigger than one node gets memory from both nodes, but its own kernel thinks it's all one node, because nos doesn't give guests a SRAT yet.
- Placing VMs by node hasn't been tried on Newton itself. VT-x is turned off in its firmware, so no VM can run there until someone switches it on. We tested it under QEMU with two emulated nodes.
- ARM machines are always one node, because nos doesn't read NUMA information from the device tree yet.

The topology code is in [numa.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/numa.cpp), the per-node free lists are in [page_table.cpp](https://github.com/irqlevel/nos/blob/master/src/base/mm/page_table.cpp), and the benchmark is [numabench](https://github.com/irqlevel/nos/blob/master/src/crates/modules/test/numabench/src/lib.rs). The details are in the [NUMA docs](/docs/numa) and in Newton's section of [Real hardware](/docs/real-hardware#newton-a-supermicro-x10dri-of-two-sockets). It all came in with [one merge](https://github.com/irqlevel/nos/commit/0f7d4248).
