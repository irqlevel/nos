---
title: "Chasing KVM: the same Ubuntu VM under KVM and under nos"
description: "We ran one Ubuntu VM under KVM and under nos on the same server. The disk started at 1% of KVM. What we fixed in three rounds of work, and what is still slower."
date: 2026-11-01
updated: 2026-11-04
tags: [hypervisor, performance, storage, testing, x86-64]
---

nos has its own hypervisor, and sooner or later someone was going to ask how much slower a Linux guest is under it than under KVM. On 31 October we finally measured it properly. It was worse than we expected: the guest's disk ran at about 1% of KVM's speed. We spent the next two days fixing things and measuring again, and this post is about how that went. A second round followed on 2 November and a third on 4 November; both are [at the end](#update-2-november-2026-the-second-round).

## The setup

Everything ran on the Hetzner EX44 that nos lives on: an i5-13500 with six P-cores and eight E-cores, and a Samsung MZVL2512HCJQ NVMe. The guest was Ubuntu 24.04's cloud image with kernel 6.8, 4 vCPUs, 8 GiB of RAM and an 8 GiB disk. We ran it under QEMU 8.2 and KVM on the server's own Ubuntu, then rebooted into nos and ran the same image under nos's hypervisor.

We tried to keep the two runs as close as we could. Both times the guest's disk was a raw file on nos's root partition, a fresh copy of the same golden image; for the KVM run, Ubuntu simply mounted that partition. nos places a 4-CPU guest on two whole P-cores and runs its disk on a separate E-core, so we pinned QEMU's vCPU threads to the same four CPUs and its iothread to the same E-core. Both runs used 8 GiB of 2 MiB pages, virtio-blk with `cache=none` and `aio=native`, virtio-net, `-cpu host` and the same CPU frequency settings.

Inside the guest, a script runs at boot: sysbench (CPU and memory), 7-Zip, openssl, perf's scheduler and syscall benchmarks, cyclictest, fio, a kernel build and pgbench. It prints results to the serial console as it goes, packs everything into an archive at the end and shuts the guest down. The whole kit, along with the results of every run, now lives in the repository under [bench/vm](https://github.com/irqlevel/nos/tree/master/bench/vm).

## The numbers

The table has the results under KVM and under nos, before the work (master at bced1539) and after it (ddcbd4c6). In brackets is nos as a percentage of KVM, always counted so that over 100% means nos was faster; for times, that is KVM's time divided by nos's.

| | KVM | before | after |
|---|---:|---:|---:|
| sysbench CPU, 1 thread | 1589 events/s | 1489 (94%) | 1528 (96%) |
| sysbench CPU, 4 threads | 3259 events/s | 3112 (95%) | 3188 (98%) |
| 7-Zip, 4 threads | 21,530 MIPS | 20,545 (95%) | 20,632 (96%) |
| random memory reads, 1 thread | 949 MiB/s | 793 (84%) | 803 (85%) |
| perf pipe, one round trip | 6.0 µs | 14.3 (42%) | 9.6 (63%) |
| cyclictest, average | 31 µs | 76 (41%) | 65 (48%) |
| fio: first write to a new file, 1M QD8 | 4643 MiB/s | 28 (1%) | 1954 (42%) |
| fio: 4k random reads, QD1 | 20.1k IOPS | 11.9k (59%) | 18.3k (91%) |
| fio: 4k random reads, QD32 | 195k IOPS | 15.5k (8%) | 149k (76%) |
| fio: 4k random writes, QD1 | 39.9k IOPS | 16.9k (42%) | 32.2k (81%) |
| fio: 4k random writes, QD32 | 178k IOPS | 26.9k (15%) | 128k (72%) |
| fio: 1M reads, QD8 | 1456 MiB/s | 169 (12%) | 1357 (93%) |
| fio: 1M writes, QD8 | 3694 MiB/s | 173 (5%) | 2314 (63%) |
| fio: 4k writes with fdatasync | 620 IOPS | 711 (115%) | 695 (112%) |
| kernel build, `make -j4` | 318 s | -- | 331 (96%) |
| pgbench init | 6.5 s | -- | 10.0 (65%) |
| pgbench read-only, 1 client | 61.6k tps | -- | 49.0k (80%) |
| pgbench read-only, 4 clients | 272k tps | -- | 274k (101%) |
| pgbench TPC-B, 1 client | 411 tps | -- | 635 (154%) |
| pgbench TPC-B, 16 clients | 4655 tps | -- | 4777 (103%) |
| guest boot | 5.24 s | 7.20 (73%) | 4.57 (115%) |

We stopped the first run before the kernel build. With the disk at 1% of KVM there was no point in waiting for the rest.

```chart
type: before-after
unit: "%"
max: 160
step: 25
reference: { value: 100, label: "KVM = 100%" }
series: { before: before, after: after }
caption: "nos as a percentage of KVM, from the table above. Over 100% means nos was faster; a row with one dot has no measurement from before the work."
groups:
  - rows:
      - { label: "sysbench CPU, 1 thread", before: 94, after: 96 }
      - { label: "sysbench CPU, 4 threads", before: 95, after: 98 }
      - { label: "7-Zip, 4 threads", before: 95, after: 96 }
      - { label: "random memory reads, 1 thread", before: 84, after: 85 }
  - rows:
      - { label: "perf pipe, one round trip", before: 42, after: 63 }
      - { label: "cyclictest, average", before: 41, after: 48 }
  - rows:
      - { label: "fio: first write to a new file, 1M QD8", before: 1, after: 42 }
      - { label: "fio: 4k random reads, QD1", before: 59, after: 91 }
      - { label: "fio: 4k random reads, QD32", before: 8, after: 76 }
      - { label: "fio: 4k random writes, QD1", before: 42, after: 81 }
      - { label: "fio: 4k random writes, QD32", before: 15, after: 72 }
      - { label: "fio: 1M reads, QD8", before: 12, after: 93 }
      - { label: "fio: 1M writes, QD8", before: 5, after: 63 }
      - { label: "fio: 4k writes with fdatasync", before: 115, after: 112 }
  - rows:
      - { label: "kernel build, make -j4", after: 96 }
      - { label: "pgbench init", after: 65 }
      - { label: "pgbench read-only, 1 client", after: 80 }
      - { label: "pgbench read-only, 4 clients", after: 101 }
      - { label: "pgbench TPC-B, 1 client", after: 154 }
      - { label: "pgbench TPC-B, 16 clients", after: 103 }
  - rows:
      - { label: "guest boot", before: 73, after: 115 }
```

## The disk

A guest's disk under nos is a file on nos's own ext4 root. Before this work, every read and write from the guest went through the VFS and took the root filesystem's lock. Writing into a part of the file that had no blocks yet cost an ext4 commit for every batch, about 9 ms per 256 KiB. On top of that, the NVMe driver sent 8 KiB per command and waited for each one, and the guest's disk had only one request in flight at a time.

Most of this was fixed by [a80b3436](https://github.com/irqlevel/nos/commit/a80b3436f74015aa5c3395534cc2a52369886e05). The disk image now reads the file's block map from ext4 once and keeps it, so reads and writes go straight to the NVMe without touching the VFS lock. When the guest writes into a hole, the file grows by 16 MiB in a single commit. NVMe commands are now as large as the controller allows, using PRP lists. And the guest can have several requests in flight: 8 at first, then 32 in [4f206bb1](https://github.com/irqlevel/nos/commit/4f206bb1bc585a19e4269d4bfc3af10d0658e3c1), once we saw that QD32 reads were stuck at 8 on the device. That took QD32 reads from 41% of KVM to somewhere between 76% and 84%.

The first test against a real disk image, `hv-linux-test.py --disk`, showed that the 16 MiB chunks didn't work at all. ext4 refused every chunk after the first hole in a file, without any error, and the writes quietly took the old slow path. All the data was still correct, which is why no other test had noticed. A transaction was re-reading an extent block it had just changed and checking it against a checksum that hadn't been updated yet ([f4a1a68f](https://github.com/irqlevel/nos/commit/f4a1a68f4bd53198f471f8437e4a3d4d3aee0a99)). A bug like that only ever shows up as slowness, so fs-fuzz now fails on any refused chunk it can't explain by a full disk or a failed request.

## The first write

Writing a new file for the first time is the one case where the guest writes to blocks nos has never handed to the file. Our first working version allocated each 16 MiB chunk and filled it with zeros before the guest's data went on top. This Samsung has no Write Zeroes command, so every block was written twice. That gave 883 MiB/s, 19% of KVM.

The obvious fix was ext4's unwritten extents: allocate the blocks without writing them, have them read back as zeros, and mark them written once the guest flushes ([39151a0c](https://github.com/irqlevel/nos/commit/39151a0c4d080feab00e496a095868653af9966d)). Those commits don't order any data, so we also let them skip the flush before the commit block and write the journal with FUA instead. It got worse: 422 MiB/s. On this drive a FUA write costs about as much as a full flush, and each commit was now doing six to eight of them instead of one flush. The image was also flushing the device before every conversion.

[311baf25](https://github.com/irqlevel/nos/commit/311baf25e60eb853e4d19555081efaedaf304271) undid both. Commits went back to journal, flush, commit block. The conversion's own commit already flushes the guest's data before any extent says it's there, so the extra flush wasn't needed. We also made the growth adaptive: when the guest keeps writing right where the last chunk ended, the next allocation doubles, up to 256 MiB, so a guest filling its disk sequentially pays for a commit every 256 MiB instead of every 16. fs-fuzz's random writes never triggered the doubling, so we added a step that fills an image from one end; a bug we planted in the new allocator on purpose was caught on input 166. First writes are now at 1954 MiB/s, 42% of KVM. Untarring a kernel went from 75% to 92% of KVM, and pgbench's init from 42% to 65%.

One thing got worse, and we don't understand it yet. In the fio job that writes random 4 KiB blocks with an fdatasync after each, throughput didn't change (112% of KVM). But since 311baf25 about one write in a thousand takes a millisecond, and in four runs out of six one in a hundred takes 0.7–0.9 ms. Before, the 99th percentile was 56 µs, and KVM's is 166 µs. These writes go to blocks that already exist, so they shouldn't run into any allocation or conversion of their own. It's at the top of the to-do list. (It was gone after the second round; see the update below.)

## The CPU

When a guest CPU executes HLT, nos used to put its thread to sleep right away, so every wakeup meant a sleep, a reschedule and a context switch. KVM spins for a little while first, and on the EX44 that spin catches about 36 million out of 40 million halts. [5f8ac3ea](https://github.com/irqlevel/nos/commit/5f8ac3ea2830062245320cc6b245823511192289) does the same in nos: the spin gets longer after short halts and goes away after long ones. It now catches 89–91% of halts, perf's pipe benchmark dropped from 14.3 µs to 8.5–9.6 µs, and the guest boots 1.75 s faster because it no longer sits waiting on IPIs during boot. The same commit lets the guest keep PCID and tells it the host's microcode revision, as KVM does.

nos also used HLT for its own idle CPUs, so as far as turbo was concerned, every idle core looked busy. 7-Zip ran at 4460 MHz under nos and at 4768 under KVM. Now idle CPUs wait in MWAIT with interrupts masked ([40f31a68](https://github.com/irqlevel/nos/commit/40f31a683b32eb08a7b3c663db23c05d5d5d63c8), the default since [5aba2825](https://github.com/irqlevel/nos/commit/5aba28258283c60c29517c15417d678f4b82f12d)), and they only go into a deep C-state after several long idle periods in a row. Without that check, waking up from a deep C-state, which takes hundreds of microseconds, would land on disk completions and IPIs. The clock went from 4460 MHz to as much as 4715, and CPU-bound benchmarks from 94–95% of KVM to 96–98%.

About half of a Linux guest's VM exits are writes to its timer deadline or IPIs. After either, nos now goes straight back into the guest without running its whole loop, as KVM does ([64d61a78](https://github.com/irqlevel/nos/commit/64d61a78ba5d0e6eb5136b94237e0568737aabf6)); 53 million of 125 million exits in a run took that path. We couldn't measure any effect on perf's pipe or on single-client pgbench, so their bottleneck must be somewhere else.

## The timer that was 10 ms late

nos's report for each guest tracks how late its timer interrupts arrive. On average it was 7–10 µs, but the worst case was 10,009 µs in every single run. 10 ms is exactly one host timer tick, so somewhere a guest's timer deadline was being missed and only picked up on the host's next tick.

There was an obvious race in the code. The loop checked the guest's timers and armed the host timer a few hundred nanoseconds later, so a deadline that fell in between got missed. We fixed it by checking again after arming. The 10 ms stayed.

So we taught the report to say where these late interrupts came from ([28af927d](https://github.com/irqlevel/nos/commit/28af927da63b78817c5feee8af32417d6cbb1237), [a68aeb3a](https://github.com/irqlevel/nos/commit/a68aeb3aeb41e9bc94a2c6d92a958c0c1848c346)). Of 12,388 interrupts that were more than 1 ms late, 12,078 came after a halted vCPU overslept by 1–3 ms, and it was nearly always another CPU that woke it, not its own timer. We put that down to the machine itself. cyclictest's worst case under KVM is 3.1–3.3 ms too, and the host scheduler's wakeups on those CPUs ran up to 3–4.6 ms late, so it looked as if something, most likely SMIs, paused the whole machine for a few milliseconds now and then. That was wrong: it was nos's own scheduler all along, and the update at the end explains how. The other 241 were a separate bug: a guest kept running past its timer deadline until a host interrupt finally kicked it out, 10 ms later.

It turned out the loop armed the host timer for the guest's deadline with interrupts enabled. If any other timer interrupt fired before the VM entry, for another task's deadline or the end of a time slice, its handler re-armed the timer for the next host tick, and the guest's deadline was gone. [7ae441c4](https://github.com/irqlevel/nos/commit/7ae441c443c739414532d6b7e70f6a9c08ec7f5c) moved the arming into the VM entry itself, where interrupts stay disabled until the guest runs, and cancels the entry if the deadline has already passed. The next run cancelled 38,943 entries that way, and the worst case was still 10,009 µs.

So we added one more thing to the report: the single latest interrupt of the run and what happened right before it ([dc8bbb5b](https://github.com/irqlevel/nos/commit/dc8bbb5b349a8cc59adea9822c70dc9dccf5f6ee)). After the next run it said:

```text
worst      10009 us past its edge, after an interrupt of the host's took the guest out: the guest entered 0 us from the edge, the host's timer asked for 0 us from it, and left 10009 us from it
```

The guest had been entered right at its deadline, with the host timer set for exactly that deadline, and it left one tick later. The bug was in our own fix. The new entry code first checked whether the deadline had passed, saw that it hadn't, and only then armed the timer, and arming a timer for a moment that has already gone by does nothing. If the deadline passed in the few hundred nanoseconds between the check and the arming, the guest ran with no timer at all. [ddcbd4c6](https://github.com/irqlevel/nos/commit/ddcbd4c6d07486d3fd8f91f0435b14211bf9ef12) arms first and checks afterwards. In the next run the worst case was 3026 µs, which we also put down to the machine. (It wasn't; see the update below.)

Our fuzzer could not have caught this. It used its own copy of the entry code, which happened to have the right order, and in its model time never moved between two reads of the clock. The entry code now lives in a separate file that hv-fuzz runs as is, and the fuzzer's clock sometimes jumps to the deadline right in the middle of arming it. With that, the wrong order is caught on input 43, and arming in the loop on input 167.

## Where nos is faster

A few results beat KVM, though not because the hypervisor does anything clever.

On fdatasync-heavy work nos is ahead: 112–116% on the fio fdatasync job and 103–154% on pgbench's TPC-B. A flush from the guest under nos goes straight to the NVMe, while under QEMU it turns into an fdatasync on the image file, which goes through the host's ext4 and its journal first. perf's syscall benchmark is at 129%, because the guest under nos protects itself against branch history injection with the CPU's own BHI_DIS_S, while the guest under QEMU 8.2 runs a software mitigation on every system call. That says more about mitigations than about either hypervisor. And the guest boots in 4.57 s instead of 5.24.

Mitigations also cut against nos. Since nos tells the guest the host's microcode revision, the guest turns on its register file data sampling mitigation and clears its register file every time it returns to user space. Under KVM the guest reports itself as vulnerable and doesn't bother.

## What's left

The full list, with the files to look at and how to measure each item, is in [plans/vm-perf-todo.md](https://github.com/irqlevel/nos/blob/master/plans/vm-perf-todo.md). This was the list on 1 November; the update below says what has changed since. In short:

- The new fdatasync tail. Our best guess is that the write waits behind something else that holds the disk image: calls into the image are serialized, a growth takes 6 ms on average (up to 18), and a conversion 5.6 ms (up to 29). The next step is to time every call into the image, not just growths and conversions. (Gone after the second round.)
- First writes at 42% of KVM. Every growth and every conversion is a commit that the guest's write or flush has to wait for. Linux's ext4 under KVM converts extents in a background workqueue and batches many conversions into one commit. (67% after the second round.)
- 1 MiB writes at 63% and 4k QD32 at 72–84%. Every byte gets copied through the image's own buffer on the slow core, while under KVM the NVMe reads and writes the guest's pages directly. The guest's memory is made of physically contiguous 2 MiB pages, so the NVMe's PRP lists could point at it too. (1 MiB writes are at KVM's speed now; 4k from a single job is still behind, for a different reason.)
- Waking up a guest CPU: cyclictest 65 µs against 31, perf's pipe 9.6 µs against 6.0. Timers are on time now, so the cost is in getting from an IPI or a timer to the guest actually running again. There's also the guest's clock: under KVM its CPUID reports `tsc_known_freq` and it runs on `kvm-clock`, under nos it does neither. (After the third round: 4 µs and 7.0.)
- Random memory reads at 85%. It's not the clock, so probably the nested page walk or the TLB. (92% after the third round.)
- CPU-bound work at 96–98%. Idle CPUs under nos still get a timer tick 100 times a second, and under Linux they don't. (That guess was wrong: it was the idle cores' C-states, see the third round. sysbench CPU is at 99–101% of KVM now.)

We also learned something about benchmarking itself. Running the same code twice can move some numbers by 5–10%: QD32 reads went from 84% to 76% between two runs of almost the same code, and the fdatasync tail comes and goes. So we only believe a change if it shows up across several runs. Each run also takes the server's VMs down for about an hour and reboots it twice, so runs have to be planned. The KVM run and every nos run are in [bench/vm/results](https://github.com/irqlevel/nos/tree/master/bench/vm/results).

## Update, 2 November 2026: the second round

After the first round, two items on that list looked related: 1 MiB writes were stuck at 63%, and 4k reads at QD32 at about 150k IOPS, while the NVMe could clearly do more. So the second round went after the disk again, and on the way it found what had really been behind the machine's "stalls".

The 1 MiB writes had a limit we hadn't noticed. Unless the device offers indirect descriptors, Linux's virtio-blk driver puts every page of a request into its own entry of the ring. A 256 KiB request took 66 of the ring's 256 entries, so the guest could never have more than three of them in flight, whatever its queue depth said. The disk now offers indirect descriptors ([c88699b5](https://github.com/irqlevel/nos/commit/c88699b5ab8c9bc9420d4c4ad0f5c1ff7a7fab4b)): a request takes one entry, and its pages sit in a table of their own.

Then we changed how the disk's task serves requests ([9fde9bd7](https://github.com/irqlevel/nos/commit/9fde9bd75b1f535c3a331e934e5a689577631f9f)). Before, it took a batch off the ring, ran the whole batch through the disk image and handed it all back, so every request waited for the slowest one in its batch. Now each read or write goes to the NVMe as soon as the task sees it, straight from a buffer the NVMe reads and writes itself, and goes back to the guest as soon as it is done. The guest can have 64 requests in flight, and the NVMe driver 256 commands ([16f9eb9d](https://github.com/irqlevel/nos/commit/16f9eb9d0b5164408d1f326d458ca42c5bb121ab)). There was one trap on the way. nos's page allocator hands out at most 128 physically contiguous pages, and the PRP lists for 256 commands asked for 256. The allocation failed without a word, and the driver quietly went back to 8 KiB commands. The lists now come in runs of 64 pages.

The first run with all of this was a mixed bag. 1 MiB reads went up to 135% of KVM and writes to 74%, but 4k reads at QD32 fell from 149k IOPS to 13k, and cyclictest's first thread averaged 853 µs. The report counted 21,112 timer interrupts more than 1 ms late.

The scheduler's statistics on the vCPUs' CPUs showed woken tasks waiting up to 4 ms at the 99th percentile, with nothing else on the run queue. Here is what was going on. When nos's idle task wakes up from MWAIT, it runs a bit of its own loop before going back to sleep, and while it does, the scheduler counts it as an ordinary task. If a vCPU was woken at that moment, the scheduler compared the two deadlines, left the CPU to the idle task, and the idle task went back into MWAIT with the vCPU still waiting on the run queue. Nothing woke the CPU again until the idle task's 3 ms time slice ran out. The window is short, but with the new disk path every finished request wakes a vCPU, often right after its CPU has taken some other interrupt, so the window was hit all the time. [d0a4b493](https://github.com/irqlevel/nos/commit/d0a4b493c93fb76ed05e5f52376d7ca9089319b3) makes the idle task check its run queue before it goes back to sleep, and [d92695bc](https://github.com/irqlevel/nos/commit/d92695bc030ece6526849e454ac0acc35c2c753e) closes what was left of the window: the idle task now marks itself idle first and looks at the queue after that.

That also settled an old question. Earlier in this post we put about 12,000 late wakeups a run down to the machine, most likely SMIs. It was this same window, and it had been there all along. With it closed, a whole run had 2 timer interrupts more than 1 ms late instead of about 12,000, no vCPU slept more than 1 ms past its time, and cyclictest's worst case was 205 µs. Under KVM, on the same machine, it is 3280 µs, so whatever causes KVM's 2–3 ms spikes, it isn't the hardware.

One more fix came from the guest's own log. During the 1 MiB write test, Linux's clocksource watchdog complained that a read of the PM timer had taken 800 µs. All of a guest's devices share one lock in nos, and a single notify from the guest could copy all of its queued writes, 8 MiB, while holding it, so every other vCPU that needed a device waited. A pass over the queue now copies at most 512 KiB before it lets go ([96281a83](https://github.com/irqlevel/nos/commit/96281a83467640194aa376942c57a0106baed710)). hv-fuzz checks every call into the disk against that limit; with the limit taken out, it fails on input 6517.

Here is the second round next to the first, against the same KVM run. The percentages are counted as before.

| | KVM | first round | second round |
|---|---:|---:|---:|
| perf pipe, one round trip | 6.0 µs | 9.6 (63%) | 8.2 (73%) |
| cyclictest, average | 31 µs | 65 (48%) | 72 (43%) |
| cyclictest, worst case | 3280 µs | 3091 (106%) | 205 (1600%) |
| fio: first write to a new file, 1M QD8 | 4643 MiB/s | 1954 (42%) | 3103 (67%) |
| fio: 4k random reads, QD1 | 20.1k IOPS | 18.3k (91%) | 16.7k (83%) |
| fio: 4k random reads, QD32 | 195k IOPS | 149k (76%) | 149k (76%) |
| fio: 4k random reads, 4 jobs at QD16 | 217k IOPS | 169k (78%) | 372k (171%) |
| fio: 4k random writes, QD1 | 39.9k IOPS | 32.2k (81%) | 29.4k (74%) |
| fio: 4k random writes, QD32 | 178k IOPS | 128k (72%) | 126k (71%) |
| fio: 1M reads, QD8 | 1456 MiB/s | 1357 (93%) | 2187 (150%) |
| fio: 1M writes, QD8 | 3694 MiB/s | 2314 (63%) | 3763 (102%) |
| fio: 4k writes with fdatasync | 620 IOPS | 695 (112%) | 710 (115%) |
| the same, 99.9th percentile of a write | 226 µs | 1090 (21%) | 72 (314%) |
| kernel build, `make -j4` | 318 s | 331 (96%) | 330 (96%) |
| pgbench init | 6.5 s | 10.0 (65%) | 9.8 (67%) |
| pgbench read-only, 1 client | 61.6k tps | 49.0k (80%) | 53.8k (87%) |
| pgbench TPC-B, 16 clients | 4655 tps | 4777 (103%) | 4928 (106%) |
| guest boot | 5.24 s | 4.57 (115%) | 5.29 (99%) |

```chart
type: before-after
unit: "%"
max: 180
step: 25
reference: { value: 100, label: "KVM = 100%" }
series: { before: "first round", after: "second round" }
caption: "nos as a percentage of KVM after each round, from the table above. Over 100% means nos was faster."
groups:
  - rows:
      - { label: "perf pipe, one round trip", before: 63, after: 73 }
      - { label: "cyclictest, average", before: 48, after: 43 }
  - rows:
      - { label: "fio: first write to a new file, 1M QD8", before: 42, after: 67 }
      - { label: "fio: 4k random reads, QD1", before: 91, after: 83 }
      - { label: "fio: 4k random reads, QD32", before: 76, after: 76 }
      - { label: "fio: 4k random reads, 4 jobs at QD16", before: 78, after: 171 }
      - { label: "fio: 4k random writes, QD1", before: 81, after: 74 }
      - { label: "fio: 4k random writes, QD32", before: 72, after: 71 }
      - { label: "fio: 1M reads, QD8", before: 93, after: 150 }
      - { label: "fio: 1M writes, QD8", before: 63, after: 102 }
      - { label: "fio: 4k writes with fdatasync", before: 112, after: 115 }
  - rows:
      - { label: "kernel build, make -j4", before: 96, after: 96 }
      - { label: "pgbench init", before: 65, after: 67 }
      - { label: "pgbench read-only, 1 client", before: 80, after: 87 }
      - { label: "pgbench TPC-B, 16 clients", before: 103, after: 106 }
```

1 MiB reads and writes are now at KVM's speed or above it, and so are 4k reads from four jobs at once: 372k IOPS against 217k. The fdatasync tail from the first round is gone: one write in a thousand takes up to 72 µs, against 226 under KVM. The CPU-bound benchmarks didn't move (96–98%).

Two numbers went down. 4k at QD1 lost about 4 µs a request, reads from 91% of KVM to 83% and writes from 81% to 74%. The old path waited for the NVMe in a loop that never went to sleep. The new one puts the disk's task to sleep until the NVMe's interrupt wakes it, and that wakeup is what costs the time. And the guest took 5.29 s to boot instead of about 4.6 s in every earlier run, because four systemd services that start together each took about 0.7 s longer. That was a single run, so we'll see whether it repeats.

What's left now, in short (the full list is in [plans/vm-perf-todo.md](https://github.com/irqlevel/nos/blob/master/plans/vm-perf-todo.md), as before):

- 4k I/O from a single job, at 71–83% of KVM (90–150% after the third round). According to fio, submitting a request takes 6–7 µs under nos and 1–2 µs under KVM. Under nos the guest's notify exits to the host, and the whole request is handled right there on the guest's CPU: it is taken off the ring, a write's data is copied, and the disk's task is woken. Under KVM the notify only wakes QEMU's I/O thread, and a busy thread polls the ring itself with notifications switched off, so the guest doesn't have to notify it. A short spin before the disk's task goes to sleep should also win back the 4 µs at QD1.
- cyclictest averages 72 µs against 31. The host now delivers the guest's timer interrupt 3 µs after its deadline on average, so the rest of the time is spent inside the guest, and we don't know yet where. (It wasn't inside the guest: it was the host CPU's clock after an idle. 4 µs after the third round.)
- First writes at 67%, still waiting on growths and conversions. (80% after the third round.)
- Random memory reads at 85% and CPU-bound work at 96–98%, as before. (92% and 99–101% after the third round.)

Both new runs are in [bench/vm/results](https://github.com/irqlevel/nos/tree/master/bench/vm/results) next to the others.

## Update, 4 November 2026: the third round

The third round went after what the second one left: how long it takes to get a guest CPU back into the guest after an IPI or a timer, 4k I/O from a single job, and the CPU-bound gap. Every run of it is in [plans/vm-perf-todo.md](https://github.com/irqlevel/nos/blob/master/plans/vm-perf-todo.md). Here is what had an effect.

cyclictest was slow because of the host CPU's clock, not the guest. A guest's CPUs idle between its timer ticks, and at the default balanced HWP preference the host CPU dropped its clock in every idle, so the first microseconds after each wake ran several times slower. Right after a wake, the guest's write of its next timer deadline took the host 10.1 µs to handle. The CPUs a guest runs on are now held at the performance preference while it runs ([2398e421](https://github.com/irqlevel/nos/commit/2398e421f767ca01bb0cd9e49e27e566f2fff297)), and cyclictest's average went from 72 µs to about 5.

For 4k at QD1, the disk's task now spins for a short while before it goes to sleep, so it is still awake when the next request or the NVMe's completion comes ([8f8d14fa](https://github.com/irqlevel/nos/commit/8f8d14fa68362585e6b11bb5d5b2148a84175ec3)). At QD32 the problem was that all of the device's work happened on the guest's first vCPU. The disk's task now gives finished requests back to the guest and sends their interrupts itself, the way QEMU's I/O thread does with irqfd ([007650c6](https://github.com/irqlevel/nos/commit/007650c6f094b10abb4519dc4925a33e38a5ace5)), and the guest doesn't notify the disk while eight of its requests are in flight ([0ca91bcf](https://github.com/irqlevel/nos/commit/0ca91bcf3238b8409bb83c59fc99f88076196dd0)). The disk's task runs on an E-core, and each copy to or from the guest's memory used to map a page for that one copy and flush it from the TLB afterwards, about a microsecond per 4 KiB. A guest's memory comes from a pool of 2 MiB pages, and that pool is now mapped once at boot ([6a8ece3c](https://github.com/irqlevel/nos/commit/6a8ece3c7da9a4991770b2667b4cc85084fe45f0)). 4k reads at QD32 went from 76% of KVM to 150%, and 4k writes at QD32 from 71% to 90%.

Then the exits. A guest's writes to the ICR, which send IPIs, and to the TSC-deadline MSR are now handled inside the loop that enters the guest, without the rest of the exit path ([faa0484b](https://github.com/irqlevel/nos/commit/faa0484b99a52af2e3c58d8e3a8b660e13e67269)). A deadline write used to cost the host 3.4 µs. A HLT that an IPI from another vCPU will end soon gets the same treatment: the loop waits for the IPI there and goes straight back into the guest ([79e4347f](https://github.com/irqlevel/nos/commit/79e4347f4d6a3e33778dab7b03148d71e3650eba)). In perf's pipe test, getting from such a wakeup to the next entry used to take almost a microsecond by itself. In the last run, about 54 million HLTs ended this way. The pipe's round trip went from 8.2 µs to 6.4–7.0 µs over the last three runs.

The CPU-bound gap turned out to be the turbo clock. A new shell command, `cpufreq` ([cdc0756f](https://github.com/irqlevel/nos/commit/cdc0756f721562927dfe8437ff139e7e1552426e)), showed the core with the busy vCPU running at 4.55 GHz. Ubuntu on the same machine runs it at 4.8. Several idle P-cores spent most of their idle time in C1, not in a deep C-state, and a core in C1 still counts as busy when the CPU picks the turbo clock for the busy one. nos only went deep after four long idles in a row. Now an idle CPU that no guest is using goes to C6 if its next timer is at least 1 ms away ([aa6776a8](https://github.com/irqlevel/nos/commit/aa6776a8b617b6edd8d73897dd3e7581b8ab29ca)). The busy core runs at 4.76 GHz, and sysbench CPU is at 99–101% of KVM.

Last, the first write to a new file. When the guest writes its disk from one end, a task of the disk image's own now allocates the space ahead of it, so the guest's writes don't stop at every hole to wait for an ext4 commit ([c0650c51](https://github.com/irqlevel/nos/commit/c0650c517a6f8a8ce84c1ded2132188ec4722ace), [a4548330](https://github.com/irqlevel/nos/commit/a45483306f7a16f279745dfc135ddf257da020a2)). The first version also asked for space where the file already had it, and in one run 1 MiB writes to an existing file fell to 39% of KVM. It now asks only where the image's block map has a hole ([aa2b4785](https://github.com/irqlevel/nos/commit/aa2b478510d3d1d01dee28d7d6d8447a85702a76)), and a boot test catches that mistake. First writes went from 67% to 80%.

Here is the third round next to the second. We didn't run the kernel build or pgbench this time. The last build's first run had 1 MiB reads at 97% and writes at 39%; a second run of the same build had them back at 149% and 127%, so the fio numbers below come from the second run.

| | KVM | second round | third round |
|---|---:|---:|---:|
| perf pipe, one round trip | 6.0 µs | 8.2 (73%) | 7.0 (86%) |
| cyclictest, average | 31 µs | 72 (43%) | 4 (775%) |
| cyclictest, worst case | 3280 µs | 205 (1600%) | 29 (11,310%) |
| sysbench CPU, 1 thread | 1589 events/s | 1530 (96%) | 1579 (99%) |
| sysbench CPU, 4 threads | 3259 events/s | 3188 (98%) | 3294 (101%) |
| random memory reads, 1 thread | 949 MiB/s | 804 (85%) | 875 (92%) |
| fio: first write to a new file, 1M QD8 | 4643 MiB/s | 3103 (67%) | 3696 (80%) |
| fio: 4k random reads, QD1 | 20.1k IOPS | 16.7k (83%) | 20.3k (101%) |
| fio: 4k random reads, QD32 | 195k IOPS | 149k (76%) | 294k (150%) |
| fio: 4k random reads, 4 jobs at QD16 | 217k IOPS | 372k (171%) | 337k (155%) |
| fio: 4k random writes, QD1 | 39.9k IOPS | 29.4k (74%) | 44.9k (113%) |
| fio: 4k random writes, QD32 | 178k IOPS | 126k (71%) | 160k (90%) |
| fio: 1M reads, QD8 | 1456 MiB/s | 2187 (150%) | 2172 (149%) |
| fio: 1M writes, QD8 | 3694 MiB/s | 3763 (102%) | 4695 (127%) |
| fio: 4k writes with fdatasync | 620 IOPS | 710 (115%) | 726 (117%) |
| the same, 99.9th percentile of a write | 226 µs | 72 (314%) | 33 (685%) |

```chart
type: before-after
unit: "%"
max: 180
step: 25
reference: { value: 100, label: "KVM = 100%" }
series: { before: "second round", after: "third round" }
caption: "nos as a percentage of KVM after the second and third rounds, from the table above. Over 100% means nos was faster. cyclictest is off the scale: 43% and 775%."
groups:
  - rows:
      - { label: "perf pipe, one round trip", before: 73, after: 86 }
      - { label: "sysbench CPU, 1 thread", before: 96, after: 99 }
      - { label: "sysbench CPU, 4 threads", before: 98, after: 101 }
      - { label: "random memory reads, 1 thread", before: 85, after: 92 }
  - rows:
      - { label: "fio: first write to a new file, 1M QD8", before: 67, after: 80 }
      - { label: "fio: 4k random reads, QD1", before: 83, after: 101 }
      - { label: "fio: 4k random reads, QD32", before: 76, after: 150 }
      - { label: "fio: 4k random reads, 4 jobs at QD16", before: 171, after: 155 }
      - { label: "fio: 4k random writes, QD1", before: 74, after: 113 }
      - { label: "fio: 4k random writes, QD32", before: 71, after: 90 }
      - { label: "fio: 1M reads, QD8", before: 150, after: 149 }
      - { label: "fio: 1M writes, QD8", before: 102, after: 127 }
      - { label: "fio: 4k writes with fdatasync", before: 115, after: 117 }
```

One number went down: 4k reads from four jobs, from 372k IOPS to 337k. It is still 155% of KVM.

What's left now:

- First writes at 80%. Each growth is still an ext4 commit with two flushes, and it happens while the NVMe is busy with the guest's data. Under KVM the host's ext4 commits once, when the guest calls fsync.
- Random memory reads at 92%. The other thread of the busy core is idle, the nested page tables use 2 MiB pages just like KVM's, and the guest has PCID. To go further we need the CPU's performance counters.
- perf's pipe at 86–93%, and 4k writes at QD32 at 90%.

Both runs of the last build are in [bench/vm/results](https://github.com/irqlevel/nos/tree/master/bench/vm/results) next to the others.

## Commits

| Commit | What it did |
|---|---|
| [a80b3436](https://github.com/irqlevel/nos/commit/a80b3436f74015aa5c3395534cc2a52369886e05) | The disk image keeps its own block map, grows 16 MiB at a time, and sends large NVMe commands |
| [f4a1a68f](https://github.com/irqlevel/nos/commit/f4a1a68f4bd53198f471f8437e4a3d4d3aee0a99) | ext4 stops refusing every chunk after a file's first hole |
| [5f8ac3ea](https://github.com/irqlevel/nos/commit/5f8ac3ea2830062245320cc6b245823511192289) | Halt polling; the guest keeps PCID and sees the host's microcode revision |
| [40f31a68](https://github.com/irqlevel/nos/commit/40f31a683b32eb08a7b3c663db23c05d5d5d63c8), [5aba2825](https://github.com/irqlevel/nos/commit/5aba28258283c60c29517c15417d678f4b82f12d) | MWAIT for idle CPUs, deep C-states only after long idle periods; on by default |
| [4f206bb1](https://github.com/irqlevel/nos/commit/4f206bb1bc585a19e4269d4bfc3af10d0658e3c1) | 32 disk requests in flight instead of 8 |
| [39151a0c](https://github.com/irqlevel/nos/commit/39151a0c4d080feab00e496a095868653af9966d), [311baf25](https://github.com/irqlevel/nos/commit/311baf25e60eb853e4d19555081efaedaf304271) | Chunks as unwritten extents, marked written on the guest's flush; normal commits again; growth that doubles up to 256 MiB |
| [64d61a78](https://github.com/irqlevel/nos/commit/64d61a78ba5d0e6eb5136b94237e0568737aabf6) | Fast path for timer deadline and IPI writes; the first timer race fix |
| [7ae441c4](https://github.com/irqlevel/nos/commit/7ae441c443c739414532d6b7e70f6a9c08ec7f5c), [ddcbd4c6](https://github.com/irqlevel/nos/commit/ddcbd4c6d07486d3fd8f91f0435b14211bf9ef12) | The host timer is armed during VM entry, then checked; the fuzzer runs the real entry code |
| [1ade6604](https://github.com/irqlevel/nos/commit/1ade6604cf1f8b309d686d3a8f1bee923418457e) | The benchmark kit and all results in the repository, plus the to-do list |
| [c88699b5](https://github.com/irqlevel/nos/commit/c88699b5ab8c9bc9420d4c4ad0f5c1ff7a7fab4b) | Indirect descriptors: a disk request takes one entry of the ring |
| [9fde9bd7](https://github.com/irqlevel/nos/commit/9fde9bd75b1f535c3a331e934e5a689577631f9f) | Each disk request goes to the NVMe as it comes and back as soon as it is done |
| [16f9eb9d](https://github.com/irqlevel/nos/commit/16f9eb9d0b5164408d1f326d458ca42c5bb121ab) | 64 requests in flight, 256 NVMe commands, PRP lists in runs of 64 pages |
| [ef2638b7](https://github.com/irqlevel/nos/commit/ef2638b743db0dcbb4285dd63d22cb76064b2f2f) | `sched` shows each CPU's longest wakeup since boot |
| [d0a4b493](https://github.com/irqlevel/nos/commit/d0a4b493c93fb76ed05e5f52376d7ca9089319b3), [d92695bc](https://github.com/irqlevel/nos/commit/d92695bc030ece6526849e454ac0acc35c2c753e) | The idle task no longer leaves a woken task waiting out its time slice |
| [96281a83](https://github.com/irqlevel/nos/commit/96281a83467640194aa376942c57a0106baed710) | One pass over the disk's queue copies at most 512 KiB under the devices' lock |
| [1b791bf2](https://github.com/irqlevel/nos/commit/1b791bf2cdd4182648fce4eecca6bea33ef33e86) | The second round's results and the updated to-do list |
| [2398e421](https://github.com/irqlevel/nos/commit/2398e421f767ca01bb0cd9e49e27e566f2fff297) | A guest's CPUs are held at HWP's performance preference while it runs |
| [8f8d14fa](https://github.com/irqlevel/nos/commit/8f8d14fa68362585e6b11bb5d5b2148a84175ec3) | The disk's task spins for a short while before it sleeps |
| [0ca91bcf](https://github.com/irqlevel/nos/commit/0ca91bcf3238b8409bb83c59fc99f88076196dd0) | The guest doesn't notify the disk while eight requests are in flight |
| [007650c6](https://github.com/irqlevel/nos/commit/007650c6f094b10abb4519dc4925a33e38a5ace5) | The disk's task gives finished requests back to the guest and sends their interrupts itself |
| [6a8ece3c](https://github.com/irqlevel/nos/commit/6a8ece3c7da9a4991770b2667b4cc85084fe45f0) | Guest memory is copied through the pool of 2 MiB pages, mapped once at boot |
| [faa0484b](https://github.com/irqlevel/nos/commit/faa0484b99a52af2e3c58d8e3a8b660e13e67269) | ICR and TSC-deadline writes are handled in the loop that enters the guest |
| [79e4347f](https://github.com/irqlevel/nos/commit/79e4347f4d6a3e33778dab7b03148d71e3650eba) | A HLT that an IPI ends soon is waited out in the same loop |
| [cdc0756f](https://github.com/irqlevel/nos/commit/cdc0756f721562927dfe8437ff139e7e1552426e), [bf65d154](https://github.com/irqlevel/nos/commit/bf65d1543e7f1153557db2fb4b42f05ee917dc1b) | `cpufreq`: each CPU's clock, and how deep its idles were |
| [aa6776a8](https://github.com/irqlevel/nos/commit/aa6776a8b617b6edd8d73897dd3e7581b8ab29ca) | C6 for idle CPUs no guest is using, when their next timer is at least 1 ms away |
| [c0650c51](https://github.com/irqlevel/nos/commit/c0650c517a6f8a8ce84c1ded2132188ec4722ace), [aa2b4785](https://github.com/irqlevel/nos/commit/aa2b478510d3d1d01dee28d7d6d8447a85702a76), [a4548330](https://github.com/irqlevel/nos/commit/a45483306f7a16f279745dfc135ddf257da020a2), [dc529775](https://github.com/irqlevel/nos/commit/dc52977544a4267376f9943f16984b578cc198b5) | The disk image grows ahead of a guest writing from one end, only where it has holes |
| [49ece047](https://github.com/irqlevel/nos/commit/49ece0479663e418df6ba863199f1bf17abe122b), [cddea62b](https://github.com/irqlevel/nos/commit/cddea62b8ef62e6e20cd032cc62b7f08e8c587a3) | The third round's to-do list and results |
