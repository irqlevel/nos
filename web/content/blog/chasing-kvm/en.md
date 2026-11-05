---
title: "Chasing KVM: the same Ubuntu VM under KVM and under nos"
description: "We ran one Ubuntu VM under KVM and under nos on the same server. The disk started at 1% of KVM's speed. Four days later most numbers match KVM or beat it."
date: 2026-11-01
updated: 2026-11-05
tags: [hypervisor, performance, storage, testing, x86-64]
---

nos has its own hypervisor, and sooner or later someone was going to ask how much slower Linux runs under it than under KVM. On 31 October we finally measured it properly. The first numbers were embarrassing. The guest's disk ran at about 1% of KVM's speed.

We spent the next four days fixing things, in three rounds, and measured again after each one. By 4 November the CPU benchmarks had caught up with KVM, most of the disk tests were faster than under KVM, and a few things were still slower. This post is about what was wrong and how we found it.

## How we measured

Everything ran on the Hetzner EX44 that nos lives on. It has an i5-13500 with six P-cores and eight E-cores, and a Samsung MZVL2512HCJQ NVMe drive. The guest was the Ubuntu 24.04 cloud image with kernel 6.8, four vCPUs, 8 GiB of memory and an 8 GiB disk. We ran it under QEMU 8.2 and KVM on the server's own Ubuntu, then rebooted the server into nos and ran the same image there.

We kept the two setups as close as we could. Both times the guest's disk was a raw file on nos's root partition, a fresh copy of the same image, and for the KVM run Ubuntu simply mounted that partition. nos puts a guest like this on two whole P-cores and serves its disk from an E-core, so we pinned QEMU's vCPU threads and its I/O thread to the same CPUs. Both runs gave the guest 2 MiB pages and a virtio-blk disk, and QEMU ran with `cache=none`, `aio=native` and `-cpu host`.

At boot the guest runs sysbench, 7-Zip, perf's scheduler and syscall benchmarks, cyclictest, fio, a kernel build and pgbench, prints the results to its serial console and shuts down. The kit and the results of every run are in [bench/vm](https://github.com/irqlevel/nos/tree/master/bench/vm).

## Before and after

The chart shows nos as a percentage of KVM on 31 October, before any of this work, and on 4 November, after the third round. Over 100% means nos was faster.

```chart
type: before-after
unit: "%"
max: 160
step: 25
reference: { value: 100, label: "KVM = 100%" }
series: { before: "31 October", after: "4 November" }
caption: "nos as a percentage of KVM, from the table below. Over 100% means nos was faster. cyclictest's average is off the scale: 76 µs before and 4 µs now, against KVM's 31."
groups:
  - rows:
      - { label: "sysbench CPU, 1 thread", before: 94, after: 99 }
      - { label: "sysbench CPU, 4 threads", before: 95, after: 101 }
      - { label: "random memory reads, 1 thread", before: 84, after: 92 }
      - { label: "perf pipe, one round trip", before: 42, after: 86 }
  - rows:
      - { label: "fio: first write to a new file, 1M QD8", before: 1, after: 80 }
      - { label: "fio: 4k random reads, QD1", before: 59, after: 101 }
      - { label: "fio: 4k random reads, QD32", before: 8, after: 150 }
      - { label: "fio: 4k random reads, 4 jobs at QD16", before: 7, after: 155 }
      - { label: "fio: 4k random writes, QD1", before: 42, after: 113 }
      - { label: "fio: 4k random writes, QD32", before: 15, after: 90 }
      - { label: "fio: 1M reads, QD8", before: 12, after: 149 }
      - { label: "fio: 1M writes, QD8", before: 5, after: 127 }
      - { label: "fio: 4k writes with fdatasync", before: 115, after: 117 }
```

<details>
<summary>All the numbers</summary>

| | KVM | 31 October | 4 November |
|---|---:|---:|---:|
| sysbench CPU, 1 thread | 1589 events/s | 1489 (94%) | 1579 (99%) |
| sysbench CPU, 4 threads | 3259 events/s | 3112 (95%) | 3294 (101%) |
| random memory reads, 1 thread | 949 MiB/s | 793 (84%) | 875 (92%) |
| perf pipe, one round trip | 6.0 µs | 14.3 (42%) | 7.0 (86%) |
| perf syscall, one call | 87.8 ns | 70.5 (125%) | 66.6 (132%) |
| cyclictest, average | 31 µs | 76 (41%) | 4 (775%) |
| cyclictest, worst case | 3280 µs | 3200 (102%) | 29 (11,310%) |
| fio: first write to a new file, 1M QD8 | 4643 MiB/s | 28 (1%) | 3696 (80%) |
| fio: 4k random reads, QD1 | 20.1k IOPS | 11.9k (59%) | 20.3k (101%) |
| fio: 4k random reads, QD32 | 195k IOPS | 15.5k (8%) | 294k (150%) |
| fio: 4k random reads, 4 jobs at QD16 | 217k IOPS | 15.6k (7%) | 337k (155%) |
| fio: 4k random writes, QD1 | 39.9k IOPS | 16.9k (42%) | 44.9k (113%) |
| fio: 4k random writes, QD32 | 178k IOPS | 26.9k (15%) | 160k (90%) |
| fio: 1M reads, QD8 | 1456 MiB/s | 169 (12%) | 2172 (149%) |
| fio: 1M writes, QD8 | 3694 MiB/s | 173 (5%) | 4695 (127%) |
| fio: 4k writes with fdatasync | 620 IOPS | 711 (115%) | 726 (117%) |
| the same, 99.9th percentile of a write | 226 µs | 1089 (21%) | 33 (685%) |
| kernel build, `make -j4` | 318 s | not run | 330 (96%) |
| pgbench init | 6.5 s | not run | 9.8 (67%) |
| pgbench read-only, 1 client | 61.6k tps | not run | 53.8k (87%) |
| pgbench TPC-B, 1 client | 411 tps | not run | 641 (156%) |
| pgbench TPC-B, 16 clients | 4655 tps | not run | 4928 (106%) |

In brackets is nos as a percentage of KVM, counted so that over 100% means nos was faster. For times that is KVM's time divided by nos's. 31 October is master before the work (bced1539), and 4 November is the build after the third round (dc529775), with fio from its second run. We stopped the first run before the kernel build, and the 4 November build ran a shorter suite without it, so the last five rows come from 2 November (96281a83).

</details>

Two runs of the same build can differ by 5 to 10% on some of these rows, so we only believed a change once it showed up in several runs. The last build's first run also had 1 MiB reads at 97% of KVM and writes at 39%. A second run of the same build had them at 149% and 127%, so the fio numbers come from that second run.

A few results beat KVM, though not because nos's hypervisor does anything clever. Work that flushes a lot is ahead: fio's test with an fdatasync after every write is at 117%, and pgbench's TPC-B with one client was at 156% the last time we ran it. Under nos a flush from the guest goes straight to the NVMe, while under QEMU it becomes an fdatasync on the image file and goes through the host's ext4 and its journal first. perf's syscall benchmark is at 132% because the guest under nos protects itself from branch history injection with the CPU's own BHI_DIS_S, while under QEMU 8.2 it runs a software mitigation on every system call.

## The disk

A guest's disk under nos is a file on nos's own ext4 root. Before this work every read and write from the guest went through the VFS and took the root filesystem's lock, and the guest could only have one request in flight. Writing into a part of the file that had no blocks yet cost an ext4 commit for every batch, about 9 ms per 256 KiB, and the NVMe driver sent 8 KiB per command and waited for each one.

The first round fixed most of that. The disk image now reads the file's block map from ext4 once and keeps it, so reads and writes go to the NVMe without going through the VFS. A write into a hole grows the file by 16 MiB in one commit, NVMe commands are as large as the controller allows, and the guest can have 32 requests in flight.

Then the first test with a real disk image showed that the 16 MiB growth never happened. ext4 refused every chunk after the first hole in a file and said nothing, so the writes took the old slow path. The data was still correct, which is why no other test had noticed. A transaction was checking an extent block it had just changed against a checksum it hadn't updated yet ([f4a1a68f](https://github.com/irqlevel/nos/commit/f4a1a68f4bd53198f471f8437e4a3d4d3aee0a99)). A bug like this only ever shows up as slowness, so fs-fuzz now fails on any refused chunk it can't explain by a full disk or a failed request.

Writing a new file is the hard case, because the guest writes to blocks nos has never given to the file. Our first version filled each new chunk with zeros before the guest's data went on top. This Samsung has no Write Zeroes command, so every block was written twice and first writes ran at 19% of KVM. Unwritten extents, which read as zeros until the guest flushes, made it worse at first. We had also switched the journal to FUA writes, and on this drive a FUA write costs about as much as a whole flush, with six to eight of them in every commit. With ordinary commits back and a growth that doubles while the guest keeps writing in order, first writes reached 42%.

The second round found a limit in the ring itself. Unless the device offers indirect descriptors, Linux's virtio-blk driver gives every page of a request its own slot in the ring. A 256 KiB request took 66 of the ring's 256 slots, so the guest could never have more than three of them in flight. The disk offers indirect descriptors now. Each request also goes to the NVMe as soon as the disk's task sees it and back to the guest as soon as it's done, where before a whole batch waited for its slowest request. 1 MiB reads went to 150% of KVM and writes to 102%. On the way we found that the NVMe driver had quietly gone back to 8 KiB commands, because its PRP lists asked the page allocator for more contiguous pages than it hands out.

In the third round we went after 4k requests. At QD32 all of the device's work happened on the guest's first vCPU, which put finished requests on the ring and sent their interrupts. Now the disk's task does that itself, the way QEMU's I/O thread does with irqfd ([007650c6](https://github.com/irqlevel/nos/commit/007650c6f094b10abb4519dc4925a33e38a5ace5)), and the guest doesn't notify the disk while eight of its requests are in flight. The disk's task also spins for a short while before it goes to sleep, so at QD1 it's still awake when the next request comes. It runs on an E-core, and it used to map each page of guest memory it copied and flush it from the TLB afterwards, about a microsecond per 4 KiB. Guest memory comes from a pool of 2 MiB pages, and that pool is now mapped once at boot. 4k reads went from 83% of KVM to 101% at QD1 and from 76% to 150% at QD32.

The same round gave the disk image a task of its own that allocates space ahead of a guest writing its disk from one end, so the guest's writes no longer stop at every hole to wait for a commit. The first version also asked for space the file already had, and in one run 1 MiB writes to an existing file fell to 39% of KVM. Now it only asks where the image's block map has a hole, and a test at boot catches that mistake. First writes went from 67% to 80%.

## Waking up a guest CPU

When a guest CPU executed HLT, nos used to put its thread to sleep right away, so every wakeup cost a sleep, a reschedule and a context switch. KVM spins for a little while first, and on the EX44 that spin catches about 36 million of 40 million halts. nos now does the same and catches 89 to 91% of them, and perf's pipe benchmark went from 14.3 µs to about 9.

nos's report for each guest tracks how late its timer interrupts arrive, and every run had a worst case of 10,009 µs, exactly one host timer tick. We fixed an obvious race and the number stayed. Then we noticed that the loop armed the host timer for the guest's deadline with interrupts enabled, so any other timer interrupt that came before the VM entry could set the timer again for the next host tick. We moved the arming into the VM entry, where interrupts stay off until the guest runs. The worst case was still 10,009 µs.

So we taught the report to print the latest interrupt of a run and what happened just before it. The guest had been entered right at its deadline, with the host timer set for exactly that moment, and it left one tick later. Our new entry code checked whether the deadline had passed and only then armed the timer, and arming a timer for a moment that has already gone by does nothing. [Arming first and checking after](https://github.com/irqlevel/nos/commit/ddcbd4c6d07486d3fd8f91f0435b14211bf9ef12) fixed it. hv-fuzz couldn't have caught this. It ran its own copy of the entry code, which happened to have the right order, and its clock never moved between two reads. Now it runs the real code with a clock that sometimes jumps to the deadline in the middle of arming, and the wrong order fails on input 43.

That still left about 12,000 timer interrupts a run that were more than 1 ms late, nearly all of them after a halted vCPU overslept by 1 to 3 ms. cyclictest's worst case under KVM is over 3 ms as well, so we blamed the machine, most likely SMIs, and the first version of this post said so. We were wrong. The second round's disk changes made it much worse, with 21,112 late interrupts in one run, and the scheduler's statistics on the vCPUs' CPUs showed woken tasks waiting up to 4 ms with nothing else on the run queue. When the idle task woke up from MWAIT, it ran a bit of its loop before going back to sleep, and while it did, the scheduler counted it as an ordinary task. A vCPU woken at that moment stayed on the run queue while the idle task went back into MWAIT, and nothing woke the CPU again until the idle task's 3 ms time slice ran out. Now the idle task [looks at its run queue before it sleeps](https://github.com/irqlevel/nos/commit/d0a4b493c93fb76ed05e5f52376d7ca9089319b3), a run has 2 late interrupts instead of about 12,000, and cyclictest's worst case fell to 205 µs. KVM's is 3280 µs on the same machine, so whatever causes its spikes, it isn't the hardware.

cyclictest's average was still 72 µs against KVM's 31, even though the host now delivered the timer interrupt 3 µs after its deadline on average. The missing time was in the host CPU's clock. A guest's CPUs idle between its timer ticks, and at HWP's default balanced setting the CPU dropped its clock in every idle, so the first microseconds after each wake ran several times slower. Right after a wake, the guest's write of its next deadline took the host 10.1 µs to handle. nos now holds the CPUs a guest runs on at HWP's performance setting while it runs, and cyclictest's average fell to about 6 µs.

The last microseconds were in the exits themselves. A guest's writes to the ICR, which send IPIs, and to its TSC deadline are now handled inside the loop that enters the guest, without the rest of the exit path. So is a HLT that an IPI from another vCPU is about to end. In the last run about 54 million HLTs ended that way. cyclictest's average went down to 4 µs, and perf's pipe from 8.2 µs to between 6.4 and 7.0.

## The clock

The CPU benchmarks started at 94 and 95% of KVM, and the reason was the turbo clock. nos used HLT for its own idle CPUs, and to the CPU a core halted that way still looks busy, so the cores doing the work got a lower turbo clock. 7-Zip ran at 4460 MHz under nos and at 4768 under KVM. Once idle CPUs waited in MWAIT and went into a deep C-state after several long idles in a row, the clock went up to as much as 4715 MHz and the benchmarks to 96 and 98%.

Closing the rest of the gap took a new shell command, `cpufreq`, which prints each CPU's clock while it was busy and how deep its idles went. It showed the core with the busy vCPU at 4.55 GHz, where Ubuntu on the same machine ran it at 4.8, and several idle P-cores spending most of their idle time in C1. nos only went deeper after four long idles in a row. Now an idle CPU that no guest is using goes to C6 whenever its next timer is at least 1 ms away. The busy core runs at 4.76 GHz, and sysbench's CPU test is at 99 and 101% of KVM.

## What is still slower

The full list, with where to look and how to measure each item, is in [plans/vm-perf-todo.md](https://github.com/irqlevel/nos/blob/master/plans/vm-perf-todo.md). The main items:

- First writes are at 80%. Each growth of the file is still an ext4 commit with two flushes, made while the NVMe is busy with the guest's data. Under KVM the host's ext4 commits once, when the guest calls fsync.
- Random memory reads are at 92%. The other thread of the busy core is idle, the nested page tables use 2 MiB pages like KVM's and the guest has PCID, so the next step is the CPU's performance counters.
- perf's pipe is at 86 to 93% of KVM, and 4k writes at QD32 at 90%.
- We haven't run the kernel build or pgbench since 2 November. Then the build was at 96% of KVM, pgbench's initial load at 67% and its read-only test with one client at 87%.

## The code

The work went into master in three merges, [vm-perf](https://github.com/irqlevel/nos/commit/df0bf875b6854faec76edf3d459d74cf1aa212b9), [vm-perf2](https://github.com/irqlevel/nos/commit/342b2888a70a8f83fb0bbd0e4af20bf22ed0f4af) and [vm-perf3](https://github.com/irqlevel/nos/commit/9dbbad3842a06d61ef7723419a47966cec1329e4). What each run changed and measured is written down in the to-do list above, and the raw results of every run are in [bench/vm/results](https://github.com/irqlevel/nos/tree/master/bench/vm/results).
