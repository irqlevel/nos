---
title: "Chasing KVM: the same Ubuntu VM under KVM and under nos"
description: "We ran one Ubuntu VM under KVM and under nos on the same server. The disk started at 1% of KVM. Here is what we fixed in two days and what is still slower."
date: 2026-11-01
tags: [hypervisor, performance, storage, testing, x86-64]
---

nos has its own hypervisor, and sooner or later someone was going to ask how much slower a Linux guest is under it than under KVM. On 31 October we finally measured it properly. It was worse than we expected: the guest's disk ran at about 1% of KVM's speed. We spent the next two days fixing things and measuring again, and this post is about how that went.

## The setup

Everything ran on the Hetzner EX44 that nos lives on: an i5-13500 with six P-cores and eight E-cores, and a Samsung MZVL2512HCJQ NVMe. The guest was Ubuntu 24.04's cloud image with kernel 6.8, 4 vCPUs, 8 GiB of RAM and an 8 GiB disk. We ran it under QEMU 8.2 and KVM on the server's own Ubuntu, then rebooted into nos and ran the same image under nos's hypervisor.

We tried to keep the two runs as close as we could. Both times the guest's disk was a raw file on nos's root partition, a fresh copy of the same golden image; for the KVM run, Ubuntu simply mounted that partition. nos places a 4-CPU guest on two whole P-cores and runs its disk on a separate E-core, so we pinned QEMU's vCPU threads to the same four CPUs and its iothread to the same E-core. Both runs used 8 GiB of 2 MiB pages, virtio-blk with `cache=none` and `aio=native`, virtio-net, `-cpu host` and the same CPU frequency settings.

Inside the guest, a script runs at boot: sysbench (CPU and memory), 7-Zip, openssl, perf's scheduler and syscall benchmarks, cyclictest, fio, a kernel build and pgbench. It prints results to the serial console as it goes, packs everything into an archive at the end and shuts the guest down. The whole kit, along with the results of every run, now lives in the repository under [bench/vm](https://github.com/irqlevel/nos/tree/master/bench/vm).

## The numbers

nos as a percentage of KVM (over 100% means nos was faster), before the work (master at bced1539) and after it (ddcbd4c6):

| | KVM | before | after |
|---|---:|---:|---:|
| sysbench CPU, 1 / 4 threads | 1589 / 3259 | 94% / 95% | 96% / 98% |
| 7-Zip, 4 threads | 21,530 MIPS | 95% | 96% |
| random memory reads, 1 thread | 949 MiB/s | 84% | 85% |
| perf pipe, one round trip | 6.0 µs | 14.3 µs (42%) | 9.6 µs (63%) |
| cyclictest, average | 31 µs | 76 µs (41%) | 65 µs (48%) |
| fio: first write to a new file, 1M QD8 | 4643 MiB/s | 28 MiB/s (1%) | 1954 MiB/s (42%) |
| fio: 4k random reads, QD1 / QD32 | 20.1k / 195k IOPS | 59% / 8% | 91% / 76% |
| fio: 4k random writes, QD1 / QD32 | 39.9k / 178k IOPS | 42% / 15% | 81% / 72% |
| fio: 1M reads / writes, QD8 | 1456 / 3694 MiB/s | 12% / 5% | 93% / 63% |
| fio: 4k writes with fdatasync | 620 IOPS | 115% | 112% |
| kernel build, `make -j4` | 318 s | -- | 331 s (96%) |
| pgbench init | 6.5 s | -- | 10.0 s (65%) |
| pgbench read-only, 1 / 4 clients | 61.6k / 272k tps | -- | 80% / 101% |
| pgbench TPC-B, 1 / 16 clients | 411 / 4655 tps | -- | 154% / 103% |
| guest boot | 5.24 s | 7.20 s | 4.57 s |

We stopped the first run before the kernel build. With the disk at 1% of KVM there was no point in waiting for the rest.

## The disk

A guest's disk under nos is a file on nos's own ext4 root. Before this work, every read and write from the guest went through the VFS and took the root filesystem's lock. Writing into a part of the file that had no blocks yet cost an ext4 commit for every batch, about 9 ms per 256 KiB. On top of that, the NVMe driver sent 8 KiB per command and waited for each one, and the guest's disk had only one request in flight at a time.

Most of this was fixed by [a80b3436](https://github.com/irqlevel/nos/commit/a80b3436f74015aa5c3395534cc2a52369886e05). The disk image now reads the file's block map from ext4 once and keeps it, so reads and writes go straight to the NVMe without touching the VFS lock. When the guest writes into a hole, the file grows by 16 MiB in a single commit. NVMe commands are now as large as the controller allows, using PRP lists. And the guest can have several requests in flight: 8 at first, then 32 in [4f206bb1](https://github.com/irqlevel/nos/commit/4f206bb1bc585a19e4269d4bfc3af10d0658e3c1), once we saw that QD32 reads were stuck at 8 on the device. That took QD32 reads from 41% of KVM to somewhere between 76% and 84%.

The first test against a real disk image, `hv-linux-test.py --disk`, showed that the 16 MiB chunks didn't work at all. ext4 refused every chunk after the first hole in a file, without any error, and the writes quietly took the old slow path. All the data was still correct, which is why no other test had noticed. A transaction was re-reading an extent block it had just changed and checking it against a checksum that hadn't been updated yet ([f4a1a68f](https://github.com/irqlevel/nos/commit/f4a1a68f4bd53198f471f8437e4a3d4d3aee0a99)). A bug like that only ever shows up as slowness, so fs-fuzz now fails on any refused chunk it can't explain by a full disk or a failed request.

## The first write

Writing a new file for the first time is the one case where the guest writes to blocks nos has never handed to the file. Our first working version allocated each 16 MiB chunk and filled it with zeros before the guest's data went on top. This Samsung has no Write Zeroes command, so every block was written twice. That gave 883 MiB/s, 19% of KVM.

The obvious fix was ext4's unwritten extents: allocate the blocks without writing them, have them read back as zeros, and mark them written once the guest flushes ([39151a0c](https://github.com/irqlevel/nos/commit/39151a0c4d080feab00e496a095868653af9966d)). Those commits don't order any data, so we also let them skip the flush before the commit block and write the journal with FUA instead. It got worse: 422 MiB/s. On this drive a FUA write costs about as much as a full flush, and each commit was now doing six to eight of them instead of one flush. The image was also flushing the device before every conversion.

[311baf25](https://github.com/irqlevel/nos/commit/311baf25e60eb853e4d19555081efaedaf304271) undid both. Commits went back to journal, flush, commit block. The conversion's own commit already flushes the guest's data before any extent says it's there, so the extra flush wasn't needed. We also made the growth adaptive: when the guest keeps writing right where the last chunk ended, the next allocation doubles, up to 256 MiB, so a guest filling its disk sequentially pays for a commit every 256 MiB instead of every 16. fs-fuzz's random writes never triggered the doubling, so we added a step that fills an image from one end; a bug we planted in the new allocator on purpose was caught on input 166. First writes are now at 1954 MiB/s, 42% of KVM. Untarring a kernel went from 75% to 92% of KVM, and pgbench's init from 42% to 65%.

One thing got worse, and we don't understand it yet. In the fio job that writes random 4 KiB blocks with an fdatasync after each, throughput didn't change (112% of KVM). But since 311baf25 about one write in a thousand takes a millisecond, and in four runs out of six one in a hundred takes 0.7–0.9 ms. Before, the 99th percentile was 56 µs, and KVM's is 166 µs. These writes go to blocks that already exist, so they shouldn't run into any allocation or conversion of their own. It's at the top of the to-do list.

## The CPU

When a guest CPU executes HLT, nos used to put its thread to sleep right away, so every wakeup meant a sleep, a reschedule and a context switch. KVM spins for a little while first, and on the EX44 that spin catches about 36 million out of 40 million halts. [5f8ac3ea](https://github.com/irqlevel/nos/commit/5f8ac3ea2830062245320cc6b245823511192289) does the same in nos: the spin gets longer after short halts and goes away after long ones. It now catches 89–91% of halts, perf's pipe benchmark dropped from 14.3 µs to 8.5–9.6 µs, and the guest boots 1.75 s faster because it no longer sits waiting on IPIs during boot. The same commit lets the guest keep PCID and tells it the host's microcode revision, as KVM does.

nos also used HLT for its own idle CPUs, so as far as turbo was concerned, every idle core looked busy. 7-Zip ran at 4460 MHz under nos and at 4768 under KVM. Now idle CPUs wait in MWAIT with interrupts masked ([40f31a68](https://github.com/irqlevel/nos/commit/40f31a683b32eb08a7b3c663db23c05d5d5d63c8), the default since [5aba2825](https://github.com/irqlevel/nos/commit/5aba28258283c60c29517c15417d678f4b82f12d)), and they only go into a deep C-state after several long idle periods in a row. Without that check, waking up from a deep C-state, which takes hundreds of microseconds, would land on disk completions and IPIs. The clock went from 4460 MHz to as much as 4715, and CPU-bound benchmarks from 94–95% of KVM to 96–98%.

About half of a Linux guest's VM exits are writes to its timer deadline or IPIs. After either, nos now goes straight back into the guest without running its whole loop, as KVM does ([64d61a78](https://github.com/irqlevel/nos/commit/64d61a78ba5d0e6eb5136b94237e0568737aabf6)); 53 million of 125 million exits in a run took that path. We couldn't measure any effect on perf's pipe or on single-client pgbench, so their bottleneck must be somewhere else.

## The timer that was 10 ms late

nos's report for each guest tracks how late its timer interrupts arrive. On average it was 7–10 µs, but the worst case was 10,009 µs in every single run. 10 ms is exactly one host timer tick, so somewhere a guest's timer deadline was being missed and only picked up on the host's next tick.

There was an obvious race in the code. The loop checked the guest's timers and armed the host timer a few hundred nanoseconds later, so a deadline that fell in between got missed. We fixed it by checking again after arming. The 10 ms stayed.

So we taught the report to say where these late interrupts came from ([28af927d](https://github.com/irqlevel/nos/commit/28af927da63b78817c5feee8af32417d6cbb1237), [a68aeb3a](https://github.com/irqlevel/nos/commit/a68aeb3aeb41e9bc94a2c6d92a958c0c1848c346)). Of 12,388 interrupts that were more than 1 ms late, 12,078 came after a halted vCPU overslept by 1–3 ms, and it was nearly always another CPU that woke it, not its own timer. That turned out to be the machine itself. cyclictest's worst case under KVM is 3.1–3.3 ms too, and the host scheduler's wakeups on those CPUs run up to 3–4.6 ms late. Something pauses the whole machine for a few milliseconds every now and then, most likely SMIs, and there's nothing nos can do about it. Another 241 were the real problem: a guest kept running past its timer deadline until a host interrupt finally kicked it out, 10 ms later.

It turned out the loop armed the host timer for the guest's deadline with interrupts enabled. If any other timer interrupt fired before the VM entry, for another task's deadline or the end of a time slice, its handler re-armed the timer for the next host tick, and the guest's deadline was gone. [7ae441c4](https://github.com/irqlevel/nos/commit/7ae441c443c739414532d6b7e70f6a9c08ec7f5c) moved the arming into the VM entry itself, where interrupts stay disabled until the guest runs, and cancels the entry if the deadline has already passed. The next run cancelled 38,943 entries that way, and the worst case was still 10,009 µs.

So we added one more thing to the report: the single latest interrupt of the run and what happened right before it ([dc8bbb5b](https://github.com/irqlevel/nos/commit/dc8bbb5b349a8cc59adea9822c70dc9dccf5f6ee)). After the next run it said:

```text
worst      10009 us past its edge, after an interrupt of the host's took the guest out: the guest entered 0 us from the edge, the host's timer asked for 0 us from it, and left 10009 us from it
```

The guest had been entered right at its deadline, with the host timer set for exactly that deadline, and it left one tick later. The bug was in our own fix. The new entry code first checked whether the deadline had passed, saw that it hadn't, and only then armed the timer, and arming a timer for a moment that has already gone by does nothing. If the deadline passed in the few hundred nanoseconds between the check and the arming, the guest ran with no timer at all. [ddcbd4c6](https://github.com/irqlevel/nos/commit/ddcbd4c6d07486d3fd8f91f0435b14211bf9ef12) arms first and checks afterwards. In the next run the worst case was 3026 µs, and that one was the machine's own stall.

Our fuzzer could not have caught this. It used its own copy of the entry code, which happened to have the right order, and in its model time never moved between two reads of the clock. The entry code now lives in a separate file that hv-fuzz runs as is, and the fuzzer's clock sometimes jumps to the deadline right in the middle of arming it. With that, the wrong order is caught on input 43, and arming in the loop on input 167.

## Where nos is faster

A few results beat KVM, though not because the hypervisor does anything clever.

On fdatasync-heavy work nos is ahead: 112–116% on the fio fdatasync job and 103–154% on pgbench's TPC-B. A flush from the guest under nos goes straight to the NVMe, while under QEMU it turns into an fdatasync on the image file, which goes through the host's ext4 and its journal first. perf's syscall benchmark is at 129%, because the guest under nos protects itself against branch history injection with the CPU's own BHI_DIS_S, while the guest under QEMU 8.2 runs a software mitigation on every system call. That says more about mitigations than about either hypervisor. And the guest boots in 4.57 s instead of 5.24.

Mitigations also cut against nos. Since nos tells the guest the host's microcode revision, the guest turns on its register file data sampling mitigation and clears its register file every time it returns to user space. Under KVM the guest reports itself as vulnerable and doesn't bother.

## What's left

The full list, with the files to look at and how to measure each item, is in [plans/vm-perf-todo.md](https://github.com/irqlevel/nos/blob/master/plans/vm-perf-todo.md). In short:

- The new fdatasync tail. Our best guess is that the write waits behind something else that holds the disk image: calls into the image are serialized, a growth takes 6 ms on average (up to 18), and a conversion 5.6 ms (up to 29). The next step is to time every call into the image, not just growths and conversions.
- First writes at 42% of KVM. Every growth and every conversion is a commit that the guest's write or flush has to wait for. Linux's ext4 under KVM converts extents in a background workqueue and batches many conversions into one commit.
- 1 MiB writes at 63% and 4k QD32 at 72–84%. Every byte gets copied through the image's own buffer on the slow core, while under KVM the NVMe reads and writes the guest's pages directly. The guest's memory is made of physically contiguous 2 MiB pages, so the NVMe's PRP lists could point at it too.
- Waking up a guest CPU: cyclictest 65 µs against 31, perf's pipe 9.6 µs against 6.0. Timers are on time now, so the cost is in getting from an IPI or a timer to the guest actually running again. There's also the guest's clock: under KVM its CPUID reports `tsc_known_freq` and it runs on `kvm-clock`, under nos it does neither.
- Random memory reads at 85%. It's not the clock, so probably the nested page walk or the TLB.
- CPU-bound work at 96–98%. Idle CPUs under nos still get a timer tick 100 times a second, and under Linux they don't.

We also learned something about benchmarking itself. Running the same code twice can move some numbers by 5–10%: QD32 reads went from 84% to 76% between two runs of almost the same code, and the fdatasync tail comes and goes. So we only believe a change if it shows up across several runs. Each run also takes the server's VMs down for about an hour and reboots it twice, so runs have to be planned. The KVM run and all ten nos runs are in [bench/vm/results](https://github.com/irqlevel/nos/tree/master/bench/vm/results).

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
