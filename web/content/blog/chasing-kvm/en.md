---
title: "Chasing KVM: one Ubuntu guest, two hypervisors, one server"
description: "The same Ubuntu VM on the same server, under KVM and under nos's hypervisor: from 1% of KVM's disk to 42-98%, the bugs met on the way, and what is still behind."
date: 2026-11-01
tags: [hypervisor, performance, storage, testing, x86-64]
---

nos runs Linux guests under its own hypervisor, and the question that matters for that is a plain one: how much slower is a guest under nos than the same guest under KVM, the hypervisor everyone else uses? On 31 October we measured it, on the Hetzner EX44 that nos runs on (an i5-13500, six P-cores and eight E-cores, a Samsung MZVL2512HCJQ NVMe). The first answer was bad. Two days later most of it is close, a few things are ahead of KVM, and what is still behind has a name and a place to look.

## The same guest, twice

One disk image: Ubuntu 24.04's cloud image with kernel 6.8, 4 vCPUs, 8 GiB of memory, an 8 GiB disk. It ran under QEMU 8.2 and KVM on the server's own Ubuntu 24.04, and after a reboot under nos's hypervisor, with the same benchmark suite running inside it at boot. Everything that could be made the same was:

- **The disk.** In both runs the guest's disk is a raw file on nos's own root partition, a fresh copy of the same golden image each time. Ubuntu mounts the partition for the KVM run; nos holds the file as a disk's image in its run.
- **The CPUs.** nos puts a lone 4-CPU guest on two whole P-cores and its disk's task on an E-core of its own. KVM's vCPU threads were pinned to the same four CPUs and its iothread to the same E-core, so the guest saw two cores of two threads under both.
- **The memory.** 8 GiB of 2 MiB pages under both, hugetlbfs preallocated for QEMU.
- **The devices.** virtio-blk with an iothread, `cache=none`, `aio=native`, `discard=unmap`, virtio-net, `-cpu host`, and the same CPU frequency policy.

The suite: sysbench's CPU and memory tests, 7-Zip, openssl, perf's scheduler and syscall benchmarks, cyclictest, fio (a new file's first write, then random and sequential I/O over it, and 4 KiB writes each followed by an fdatasync), a kernel tree untarred, built and removed, and pgbench. The guest prints each number on its serial console as it goes, packs everything into an archive at the end and powers itself off, which is how either host knows it is done. The whole kit is in the tree now, as [bench/vm](https://github.com/irqlevel/nos/tree/master/bench/vm), with every run's results.

## Where it started, and where it is

nos's share of KVM's result (above 100%: nos is better), on master as it was (bced1539) and after the work (ddcbd4c6):

| | KVM | before | after |
|---|---:|---:|---:|
| sysbench CPU, 1 / 4 threads | 1589 / 3259 | 94% / 95% | 96% / 98% |
| 7-Zip, 4 threads | 21,530 MIPS | 95% | 96% |
| random reads of memory, 1 thread | 949 MiB/s | 84% | 85% |
| perf's pipe, a round trip | 6.0 us | 14.3 us (42%) | 9.6 us (63%) |
| cyclictest, average | 31 us | 76 us (41%) | 65 us (48%) |
| fio: a new file's first write, 1M QD8 | 4643 MiB/s | 28 MiB/s (1%) | 1954 MiB/s (42%) |
| fio: 4k random reads QD1 / QD32 | 20.1k / 195k IOPS | 59% / 8% | 91% / 76% |
| fio: 4k random writes QD1 / QD32 | 39.9k / 178k IOPS | 42% / 15% | 81% / 72% |
| fio: 1M reads / writes QD8 | 1456 / 3694 MiB/s | 12% / 5% | 93% / 63% |
| fio: 4k writes with fdatasync | 620 IOPS | 115% | 112% |
| kernel build, `make -j4` | 318 s | -- | 331 s (96%) |
| pgbench init | 6.5 s | -- | 10.0 s (65%) |
| pgbench read-only, 1 / 4 clients | 61.6k / 272k tps | -- | 80% / 101% |
| pgbench TPC-B, 1 / 16 clients | 411 / 4655 tps | -- | 154% / 103% |
| the guest's boot | 5.24 s | 7.20 s | 4.57 s |

The first run never got to the kernel build: we stopped it there, because a disk at 1% of KVM's made the rest pointless until it was fixed.

## The disk: from a request at a time to KVM's neighbourhood

A guest's disk under nos is a file on nos's ext4 root, held open as a disk's image. Before this work, a guest's read or write went through the VFS, under the lock of nos's root, for every batch, and a write into a part of the file that had no blocks yet was a write through the VFS with an ext4 commit of its own: about 9 ms for each 256 KiB. The NVMe driver sent 8 KiB a command and waited for each, and the guest's disk had one request out at a time.

[a80b3436](https://github.com/irqlevel/nos/commit/a80b3436f74015aa5c3395534cc2a52369886e05) changed all of that at once. The image takes the file's map from ext4 once and holds it, so a read or a write is resolved there and goes to the NVMe without taking the VFS's lock. A write into a hole grows the file by the 16 MiB around it in one commit, rather than a commit for every batch the guest writes there. NVMe commands go up to the controller's limit with PRP lists. And the guest's disk keeps several requests in flight: eight at first, 32 since [4f206bb1](https://github.com/irqlevel/nos/commit/4f206bb1bc585a19e4269d4bfc3af10d0658e3c1), when the EX44 showed QD32 reads stuck at eight at the device (41% of KVM's, now 76-84%).

The first test of it on a real disk image, `hv-linux-test.py --disk`, found that the chunks never worked. ext4 refused every chunk past the first hole of each file, silently, and every such write still went through the VFS, the slow way, with every answer correct. A transaction read back the extent block it had just changed, and held it to a checksum the change had not put on it yet ([f4a1a68f](https://github.com/irqlevel/nos/commit/f4a1a68f4bd53198f471f8437e4a3d4d3aee0a99)). A correct but slow path is the kind of failure no other test notices, so fs-fuzz now counts every refused chunk and calls one a finding unless the disk was full or a request had failed.

## The first write, three ways

A new file's first write is the one place where the guest writes into blocks that nos has never given the file. The first working version gave each 16 MiB chunk its blocks and wrote zeros to them first: the EX44's Samsung has no Write Zeroes, so the zeros went down as data, every block twice. That made 883 MiB/s, 19% of KVM's.

The obvious fix was ext4's unwritten extents: give the chunk its blocks without writing them, read them as zeros until the guest writes them, and mark what it wrote as written at its next flush ([39151a0c](https://github.com/irqlevel/nos/commit/39151a0c4d080feab00e496a095868653af9966d)). Those two commits order no data, so we let them skip the flush before the commit block and write their journal blocks with FUA instead. On the EX44 the first write fell to 422 MiB/s. On this Samsung a FUA write costs about what a flush costs, and a commit wrote six to eight of them where it used to need one flush. The image also flushed the device before every conversion.

[311baf25](https://github.com/irqlevel/nos/commit/311baf25e60eb853e4d19555081efaedaf304271) took both out. A commit is its log, a flush and its commit block again, whatever it orders; the conversion's own commit flushes the guest's data to the medium before any extent says it is there. And a hole that begins where the last growth ended gets twice the last growth's chunks, up to 256 MiB, so a guest filling its disk from one end costs a commit for every 256 MiB rather than every 16. fs-fuzz's random writes never reached that doubling, so the fuzzer gained a step that fills an image from one end; a planted bug in the new run allocator was found at its 166th input. The first write is at 1954 MiB/s now, 42% of KVM's, and unpacking a kernel tree and pgbench's init went from 75% and 42% to 92% and 65%.

It also brought something we have not explained. The fio job that writes 4 KiB at random with an fdatasync after each keeps its throughput (112%), but since 311baf25 one write in a thousand takes about a millisecond, and in four runs of six one in a hundred takes 0.7-0.9 ms (it was 56 us; KVM's is 166). These are writes over blocks the guest wrote already, so they should not meet a growth or a conversion of their own. It is the first item on the list of what is left.

## The CPU side

**Halt polling** ([5f8ac3ea](https://github.com/irqlevel/nos/commit/5f8ac3ea2830062245320cc6b245823511192289)). When a guest's CPU halts, nos used to put its task to sleep at once, and every wakeup cost a sleep, a schedule and a switch. KVM spins for a while first, and on the EX44 its spin caught 36 million of 40 million halts. nos now does the same, growing the spin after short halts and dropping it after long ones: 89-91% of halts are caught in the spin, perf's pipe went from 14.3 us to 8.5-9.6, and the guest's boot lost 1.75 s it had spent waiting on IPIs. The same commit gives the guest PCID and tells it the host's microcode revision, as KVM does.

**MWAIT for idle** ([40f31a68](https://github.com/irqlevel/nos/commit/40f31a683b32eb08a7b3c663db23c05d5d5d63c8), the default since [5aba2825](https://github.com/irqlevel/nos/commit/5aba28258283c60c29517c15417d678f4b82f12d)). nos idled with HLT, so every idle core still counted as busy for turbo, and 7-Zip ran at 4460 MHz where it ran at 4768 under KVM. An idle CPU now waits in MWAIT with interrupts masked, and goes deep only after a run of long idles; a deep C-state's exit, hundreds of microseconds, would otherwise land on disk completions and IPIs. The clock rose to 4460-4715 MHz, and CPU-bound work went from 94-95% of KVM's to 96-98%.

**A fast path for two exits** ([64d61a78](https://github.com/irqlevel/nos/commit/64d61a78ba5d0e6eb5136b94237e0568737aabf6)). Half of a Linux guest's exits are writes of its timer's deadline or of an IPI. After either, nos now goes straight back into the guest without its loop's whole round, as KVM does: 53 million of 125 million exits in a run. It made no measurable difference to perf's pipe or to pgbench with one client. Whatever those wait on, it is not that microsecond.

## A timer that came 10 ms late

nos's report of a guest counts how late its timer interrupts are, past the moment they were due: 7-10 us on average, and at most 10,009 us. Every run, the most was 10 ms and a few microseconds, which is the length of one tick of the host. Somewhere an edge of the guest's timer was missed and picked up by the host's next tick.

The first fix was for a race we could see in the code: the loop looked at the guest's timers and armed the host's timer for their next edge a few hundred nanoseconds later, so an edge that fell between the two was left for the next tick. The loop now looks again after arming. The 10 ms stayed.

So the report learned to say where such interrupts were waited for ([28af927d](https://github.com/irqlevel/nos/commit/28af927da63b78817c5feee8af32417d6cbb1237), [a68aeb3a](https://github.com/irqlevel/nos/commit/a68aeb3aeb41e9bc94a2c6d92a958c0c1848c346)). Of 12,388 interrupts more than 1 ms late in a run, 12,078 came after a sleep of a halted vCPU that had the edge in it and ended 1-3 ms past its time, nearly always ended by another CPU ringing it rather than by its own deadline. That is the machine's own doing: cyclictest's worst case is 3.1-3.3 ms under KVM as well, and the host scheduler's wakeups on those CPUs are 3-4.6 ms late at most. Something stops the whole machine for milliseconds now and then, most likely SMIs, and nothing in nos can help that. Another 241 came after an interrupt of the host's took out a guest that had computed past its edge: those were the 10 ms.

The loop armed the host's timer for the guest's edge in its round, with interrupts on, and any interrupt of that timer that arrived before the entry, for another task's deadline or the end of a time slice, re-armed it for the host's next tick in its handler. [7ae441c4](https://github.com/irqlevel/nos/commit/7ae441c443c739414532d6b7e70f6a9c08ec7f5c) moved the arming into the entry itself, where interrupts are off until the guest runs, and turns the entry back if the edge has already come. The next run turned back 38,943 entries for an edge that came on the way in, and the worst case was still 10,009 us.

One more counter settled it. The report keeps the run's latest interrupt and what came before it ([dc8bbb5b](https://github.com/irqlevel/nos/commit/dc8bbb5b349a8cc59adea9822c70dc9dccf5f6ee)):

```text
worst      10009 us past its edge, after an interrupt of the host's took the guest out: the guest entered 0 us from the edge, the host's timer asked for 0 us from it, and left 10009 us from it
```

The guest was entered at its edge, with the host's timer asked for that edge, and left a tick later. The new entry code looked whether the edge had come, saw that it had not, and then armed the timer, and the timer arms nothing for a deadline that has passed. An edge that went by in the few hundred nanoseconds between the look and the arming left the guest running unarmed. [ddcbd4c6](https://github.com/irqlevel/nos/commit/ddcbd4c6d07486d3fd8f91f0435b14211bf9ef12) arms first and looks after, and the worst case in the next run was 3026 us: a sleep woken late by the machine's own stall.

The fuzzer could not have caught the second mistake: it modelled the entry with its own copy of the code, which had the order right, and in its model the clock never moved between two reads. The entry's code now lives in a file of its own that hv-fuzz runs as it is, and its clock now and then reaches a deadline in the middle of arming it. The look before the arming is found at its 43rd input, and the arming in the round at its 167th.

## Where nos is ahead

A few numbers are better than KVM's, for reasons that are not about the hypervisor.

- **fdatasync and pgbench's TPC-B**: 112-116% and 103-154%. A guest's flush under nos becomes a flush of the NVMe; under QEMU it is an fdatasync of the image file, through the host's ext4 and its journal first.
- **perf's syscall benchmark**: 129%. The guest under nos protects itself against branch history injection with the CPU's own `BHI_DIS_S`; the one under QEMU 8.2 runs the software sequence on every syscall. It is a difference in mitigations, not in virtualization.
- **The guest's boot**: 4.57 s against 5.24.

It also cuts the other way. The guest under nos is told the host's microcode revision, so it clears its register file on every return to user space against register file data sampling. The guest under KVM reports itself vulnerable and does nothing.

## What is left

[plans/vm-perf-todo.md](https://github.com/irqlevel/nos/blob/master/plans/vm-perf-todo.md) has each of these with the files to look in and how to measure it:

- **The fdatasync job's new tail**. Most likely a write waiting behind something else that holds the disk's image, whose calls come one at a time: a growth (6 ms on average, up to 18), a conversion (5.6 ms, up to 29), or nos's own journal. The next step is to time every call of the image, not only its growths and conversions.
- **The first write at 42%**. Each growth and each conversion is a commit the guest's write or flush waits for. Linux's ext4 under KVM converts on a workqueue and puts many conversions into one commit.
- **1 MiB writes at 63% and 4k at QD32 at 72-84%**. Every byte goes through the image's own buffer: copied in on a write and out on a read, on the slow core, where KVM has the NVMe read and write the guest's own pages. The guest's memory is 2 MiB pages, physically contiguous, so the NVMe's PRP lists could name them directly.
- **Waking a guest's CPU**: cyclictest's 65 us against 31, perf's pipe 9.6 us against 6.0. The timer is on time now, so this is the path from an IPI or a timer to the guest running again. There is also the guest's clock: under KVM its CPUID says `tsc_known_freq` and it runs on `kvm-clock`, under nos it does not.
- **Random reads of memory at 85%**, which is not the clock: the nested page walk, or the TLB.
- **CPU-bound work at 96-98%**: nos's idle CPUs still take a tick 100 times a second, and Linux's do not.

Two things we learned about the measurement itself. The same code run twice can move some numbers by 5-10%: QD32 went from 84% to 76% between two runs of nearly the same code, and the fdatasync tail comes and goes. So a change is judged across runs, never on one. And every run stops the server's VMs for an hour and reboots it twice, so it is planned rather than tried. KVM's run and all ten of nos's are in [bench/vm/results](https://github.com/irqlevel/nos/tree/master/bench/vm/results).

## The commits

| Commit | What it did |
|---|---|
| [a80b3436](https://github.com/irqlevel/nos/commit/a80b3436f74015aa5c3395534cc2a52369886e05) | The disk image holds its own map, grows a chunk at a time and goes to the NVMe a vector at a time |
| [f4a1a68f](https://github.com/irqlevel/nos/commit/f4a1a68f4bd53198f471f8437e4a3d4d3aee0a99) | ext4 no longer refuses every chunk past a file's first hole |
| [5f8ac3ea](https://github.com/irqlevel/nos/commit/5f8ac3ea2830062245320cc6b245823511192289) | Halt polling; the guest keeps PCID and sees the host's microcode revision |
| [40f31a68](https://github.com/irqlevel/nos/commit/40f31a683b32eb08a7b3c663db23c05d5d5d63c8), [5aba2825](https://github.com/irqlevel/nos/commit/5aba28258283c60c29517c15417d678f4b82f12d) | MWAIT for idle, masked, deep only after long idles; the default |
| [4f206bb1](https://github.com/irqlevel/nos/commit/4f206bb1bc585a19e4269d4bfc3af10d0658e3c1) | 32 disk requests in flight instead of 8 |
| [39151a0c](https://github.com/irqlevel/nos/commit/39151a0c4d080feab00e496a095868653af9966d), [311baf25](https://github.com/irqlevel/nos/commit/311baf25e60eb853e4d19555081efaedaf304271) | Chunks as unwritten extents, made written at the guest's flush; commits that flush as before; growth doubling to 256 MiB |
| [64d61a78](https://github.com/irqlevel/nos/commit/64d61a78ba5d0e6eb5136b94237e0568737aabf6) | The fast path for timer-deadline and IPI writes; the timer edge that came on the way in |
| [7ae441c4](https://github.com/irqlevel/nos/commit/7ae441c443c739414532d6b7e70f6a9c08ec7f5c), [ddcbd4c6](https://github.com/irqlevel/nos/commit/ddcbd4c6d07486d3fd8f91f0435b14211bf9ef12) | The host's timer armed in the entry, then looked at; the entry's code run by the fuzzer as it is |
| [1ade6604](https://github.com/irqlevel/nos/commit/1ade6604cf1f8b309d686d3a8f1bee923418457e) | The benchmark kit and every run's results in the tree, and the list of what is left |
