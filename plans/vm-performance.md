# A VM as fast under nos as under KVM

**Goal:** the same Linux guest, on the same machine, as fast under nos's
hypervisor as under QEMU/KVM on Linux -- and where it is not, a measured
reason that is a choice, not an accident.

**Where it comes from:** the comparison on the EX44 of 2026-10-31. One disk
image -- Ubuntu 24.04's cloud image (serial 20261017, kernel 6.8.0-147), 4
vCPUs, 8 GiB, an 8 GiB disk -- run under QEMU 8.2 / KVM on the machine's
Ubuntu 24.04.4 and under nos (master bced1539), with the same suite at boot.
Both on the same CPUs (two P-cores' four threads, APIC 41, 40, 33, 32, the
disk's work on an E-core, APIC 62), both from 2 MiB pages, both disks a raw
file on the same ext4 partition of the same NVMe, the same HWP request. KVM
was set up as it is for speed: vCPU threads pinned, `-cpu host`, an iothread,
`cache=none,aio=native`, hugetlbfs. The kit is `out/ex44-bench` (`guest/run.sh`
the suite; `host/bench-host.sh` the Ubuntu side; `compare.py` the table).

## What was measured

nos's result as a share of KVM's, over 100% where nos did better. The nos
run was stopped at its kernel build, the disk writes below being what they
are; the kernel build and pgbench have KVM's numbers only.

| | KVM | nos | |
|---|---:|---:|---:|
| sysbench cpu, 1 / 4 threads, events/s | 1589 / 3259 | 1489 / 3112 | 94% / 95% |
| 7-Zip, 4 threads, MIPS | 21 530 | 20 545 | 95% |
| the clock a busy core ran at (7-Zip's) | 4768 MHz | 4460 MHz | 94% |
| random reads of memory, 1 thread, MiB/s | 949 | 793 | 84% |
| perf sched pipe, us an op | 6.0 | 14.3 | 42% |
| hackbench, s | 5.60 | 5.43 | 103% |
| a syscall (getppid), us | 0.088 | 0.071 | 125% |
| cyclictest, average us | 31 | 76 | 41% |
| fio: a new file's first write, 1M QD8, MiB/s | 4643 | 28 | 1% |
| fio: 4k random reads QD1 / QD32, IOPS | 20.1k / 195k | 11.9k / 15.5k | 59% / 8% |
| fio: 4k random writes QD1 / QD32, IOPS | 39.9k / 178k | 16.9k / 26.9k | 42% / 15% |
| fio: 1M reads / writes QD8, MiB/s | 1456 / 3694 | 169 / 173 | 12% / 5% |
| fio: 4k writes with fdatasync, IOPS | 620 | 711 | 115% |
| kernel build (`make -j4` defconfig), s | 318 | -- | |
| pgbench -S, 1 / 4 clients, tps | 61.6k / 272k | -- | |
| pgbench (TPC-B), 1 / 16 clients, tps | 411 / 4655 | -- | |

What nos does better, and keeps: the guest is given `BHI_DIS_S` (QEMU 8.2's
guest clears the branch history in software at every syscall), told of
RFDS's clearing (KVM's guest says "Vulnerable: No microcode"), and a guest's
flush goes to the NVMe with nothing in between.

## The plan

In order; each item names the cause it removes, what must stay true, and its
gate. The comparison is run again after each group, and its numbers go here.

### Disk

The guest's disk is a file on nos's ext4 (`fs::image`): a batch of blocks the
file has is read and written on the device off the VFS's lock, where the
VFS's map says (`Vfs::map`, under the mount's lock, every request); a batch
with a hole goes through the VFS as a write (`write_within`), and ext4
commits it then and there -- a transaction of the bitmaps, the tree, the
inode, its log written, flushed, its commit block FUA, the checkpoint inline:
about 9 ms for 256 KiB, the mount's lock held throughout. Under it, a batch
is a 4 KiB piece a command (the NVMe driver takes 8 KiB at most, two PRP
entries) waited for one by one, and a disk serves one guest request at a time.

- **D1. The image holds its own map.** At open the image takes the file's
  runs from the filesystem -- logical block, device sector, length, each
  held to the blocks a file may own (`Meta::file_may_own`, as every way to a
  file's data is) -- and resolves every batch from them, no lock taken. Only
  the image changes them while it is held (`OPEN_HOLD`: nobody else writes,
  cuts, grows, renames or removes the file; ext4 moves no file's blocks), so
  it changes its copy with each allocation and punch of its own. Gate:
  `fs-fuzz.py` (the image targets, a model of the file's data over power cuts),
  `hv-linux-test.py --disk` and `--disk --nvme-root` (e2fsck on nos's root
  and on the guest's disk).
- **D2. A thin image grows a chunk at a time.** A write into a hole first has
  the filesystem give the file every block of the 16 MiB chunk around it that
  it lacks, as one transaction: the blocks allocated, made zeros on the
  device, and only then committed -- so that after any crash a block is the
  file's (zeros, or the guest's data) or free, never what it held before --
  and then the guest's data goes to the device as an overwrite. A commit a
  16 MiB rather than a 256 KiB. A guest's discard gives back whole chunks;
  what is left of its range is deallocated on the device (D3), the blocks
  kept -- what reads back is zeros or the guest's own old data, which
  virtio's discard allows; a write of zeros writes zeros. New:
  `FileSystem::allocate` (ext4; ext2 and the rest keep the write through the
  VFS). Gate: as D1, and `ext4rw`'s power cuts held to the rule above.
- **D3. Write Zeroes and Deallocate.** The block layer's driver trait gains a
  write of zeros and a deallocation; NVMe implements them by its Write Zeroes
  and Dataset Management commands where the controller says it has them
  (ONCS), within its limits; anything else is zeros written. Gate: smoke (the
  x86 boot attaches an NVMe), `parttest.py`, D2's.
- **D4. A command as big as the controller takes.** PRP lists, a page of
  them a command ID, made when the queue is; a batch's contiguous blocks one
  command, up to MDTS; the batch waited for as one. Gate: smoke, `parttest.py`,
  `netblk-test.py` (the asynchronous path shares the queue), `hv-linux-test.py
  --disk --nvme-root`.
- **D5. More than one request in flight.** The disk's task takes every
  request the ring has and submits them through the asynchronous path
  (`Disk::submit`), gives each back as it completes, and holds to a budget
  of command IDs so that no disk starves another. Later, virtio-blk's queues,
  one a vCPU. Gate: as D4, and `hv-fuzz.py` (`virtq`: every request served
  and heard of).

### Waking up

- **C1. Idle cores sleep deep.** nos idles with HLT, so every core counts as
  busy for the turbo bins and a busy one runs at 4.46 GHz where Linux's,
  its neighbours in C6 and deeper, runs at 4.77. The idle task takes MWAIT
  with the deepest hint CPUID leaf 5 and the ACPI tables offer, where the
  CPU has them, HLT where not; a wake is an interrupt either way. Gate:
  smoke (`test_sleep_prompt`, `test_wake_storm`), `idle-wait-test.py`,
  `panic-test.sh` (the ticking of a panic), the EX44's clock measured again.
- **W1. Halt polling.** A vCPU that halts looks for its next interrupt for a
  while before its task blocks -- KVM's 200 us, grown while a poll finds one
  and shrunk while it does not -- since a wake from blocking costs the host a
  wakeup, a schedule and an entry: KVM's polls caught 36.3 million of 40.3
  million halts in the run. Gate: `hv-test.py`, `hv-fuzz.py` (`platform`),
  `hv-linux-test.py --cpus 2`.
- **W2. Cheaper IPIs and deadlines.** The guest's x2APIC ICR and
  TSC_DEADLINE writes -- 12 million exits in the run's fifteen minutes --
  handled at the exit before anything else is looked at. Gate: as W1.
- **T1. A timer's latency.** cyclictest's 76 us against KVM's 31, measured
  again after C1 and W1, and what is left found: the host timer's arming,
  the task's wakeup, the entry.

### What the guest is shown

- **G1. PCID.** Without it Linux flushes the whole TLB at every switch of
  process -- pgbench's clients and server, a build's compilers. Offered with
  CR4.PCIDE the guest's under both backends, and INVPCID's types as they are.
  Gate: `hv-test.py`, `hv-fuzz.py` (`policy`, `platform`), `insn-test.py`.
- **G2. The rest `-cpu host` gives.** XSAVES (IA32_XSS), WAITPKG (UMWAIT,
  TPAUSE and their control MSR), the TSC's frequency in KVM's timing leaf
  (`tsc_known_freq`), MOVDIRI and MOVDIR64B: each with its policy, its
  fuzz target's model, and a guest of `hv-test.py` where it can be seen.

### Still to explain

- **E1.** Random reads of memory 16% slower, more than the clock explains:
  the nested pages' leaves, the uncore's clock.
- **E2.** The guest's boot, 7.2 s against 5.2 (kernel and userspace).

## Measured after the first group

The same comparison on 2026-10-31 evening: branch `vm-perf` at 40f31a68
(D1-D5, G1, W1, C1, the exit counters), the guest's disk a fresh copy of
the same image each time, run twice -- once with the kernel line the
machine has (`hugepages=32G`, HLT idle), once with `idle=mwait`. nos's
results as a share of KVM's, as above; `out/ex44-bench/run2` has the kit's
second half and every log.

| | KVM | bced1539 | vm-perf | + idle=mwait | 64d61a78 | 311baf25 |
|---|---:|---:|---:|---:|---:|---:|
| sysbench cpu, 1 / 4 threads | 1589 / 3259 | 94% / 95% | 94% / 95% | 96% / 98% | 96% / 98% | 97% / 98% |
| 7-Zip, 4 threads; its clock | 21 530; 4768 MHz | 95% | 95%; 4460 | 95%; 4460-4715 | 96% | 96% |
| openssl, aes-256-gcm / chacha20 / sha256 | | 94% | 94% | 96-97% | 96-97% | 96-97% |
| random reads of memory, 1 thread | 949 MiB/s | 84% | 84% | 85% | 85% | 84% |
| perf sched pipe, us an op | 6.0 | 14.3 (42%) | 9.3 (65%) | 8.5 (71%) | 9.1 (66%) | 8.95 (67%) |
| cyclictest, average us | 31 | 76 (41%) | 73 (42%) | 75.5 (41%) | 66.2 (47%) | 67.0 (46%) |
| fio: a new file's first write, 1M QD8 | 4643 MiB/s | 1% | 885 (19%) | 883 (19%) | 422 (9%) | 1954 (42%) |
| fio: 4k random reads QD1 / QD32 | 20.1k / 195k | 59% / 8% | 89% / 41% | 92% / 41% | 92% / 84% | 90% / 84% |
| fio: 4k random writes QD1 / QD32 | 39.9k / 178k | 42% / 15% | 84% / 72% | 86% / 74% | 81% / 75% | 80% / 78% |
| fio: 1M reads / writes QD8 | 1456 / 3694 MiB/s | 12% / 5% | 93% / 63% | 92% / 63% | 93% / 62% | 93% / 62% |
| fio: 4k writes with fdatasync; a write's p99 | 620; 166 us | 115% | 116%; 61 us | 116%; 54 us | 115%; 56 us | 114%; 716 us |
| kernel untar | 6.14 s | -- | 7.07 s (87%) | 7.09 s (87%) | 8.22 s (75%) | 6.96 s (88%) |
| kernel build (`make -j4` defconfig) | 318 s | -- | 333 s (95%) | 333 s (96%) | 332 s (96%) | 332 s (96%) |
| pgbench init | 6.5 s | -- | 10.9 s (60%) | 10.7 s (61%) | 15.7 s (42%) | 10.2 s (64%) |
| pgbench -S, 1 / 4 / 16 clients | 61.6k / 272k / 238k | -- | 79% / 100% / 99% | 82% / 100% / 99% | 80% / 101% / 100% | 83% / 101% / 100% |
| pgbench (TPC-B), 1 / 4 / 16 clients | 411 / 1211 / 4655 | -- | 156% / 112% / 105% | 156% / 113% / 105% | 156% / 111% / 103% | 156% / 111% / 104% |
| the guest's boot (kernel + userspace) | 5.24 s | 7.2 s | -- | 4.56 s | 4.86 s | 4.62 s |

64d61a78 is items 1-4 of the status below with MWAIT idle the default
(32 requests in flight, chunks as unwritten extents committed with their
logs written with FUA, the exit fast path, the timer's edge taken a round
later); 311baf25 is 64d61a78 with the second of them revised: a commit is
its log, a flush and its commit block whatever it orders, the image no
longer flushes before a conversion, and a growth that follows the last
doubles, to 256 MiB.

What the numbers say, item by item, is in the status below. In short: the
disk is near KVM wherever one request is in flight at a time, and ahead of
it wherever a guest flushes; it is held back where many requests are out at
once (8 in flight per disk) and where a guest writes where the file has
none (this NVMe has no Write Zeroes, so a chunk's zeros are written, every
block twice). The guest boots faster than under KVM. What is left is the
latency of a wakeup -- one guest CPU waking another, a timer -- and the
clock a busy core runs at.

## Status

| item | state |
|---|---|
| D1-D5 | measured on the EX44: reads and writes of 1M 93% and 63% of KVM (12% and 5% before), 4k at QD1 84-92% (latency 48-50 us, KVM's 47), fdatasync 116%, pgbench's TPC-B 105-156%; at QD32 41% -- the device has 8 requests out per disk (`blk::IN_FLIGHT`), and 8 at ~100 us is the 80k IOPS measured: next, 32; a new file's first write 19% (1% before) -- this NVMe (Samsung MZVL2512HCJQ, `oncs=0x57`) has no Write Zeroes, so each chunk's zeros are written before the guest's data goes over them, and a commit a chunk: next, zeros from one page in commands of MDTS with many out, or unwritten extents converted at the guest's flush. Found by `hv-linux-test.py --disk` first: ext4 had refused every chunk past its first hole (f4a1a68f); then items 1 and 2 (64d61a78, 311baf25): 32 requests out per disk -- QD32 84% (164k IOPS), 4x16 78% -- and a chunk given as unwritten extents, made written at the guest's flush, nothing written to it, its growth doubling to 256 MiB where a guest writes from one end: a new file's first write 1954 MiB/s, 42% (9% while the commits' logs were written with FUA -- on this Samsung a FUA write costs what a flush does), untar 88%, pgbench init 64%. Left: in the fdatasync job a write's p99 716 us where it was 56 (KVM's 166) -- the image's growths and conversions timed in its report (`Timed`), to find; 1M writes 62%. In the runs since 311baf25 the fdatasync job's writes have a tail they did not have: p999 0.9-1.1 ms in all six (104-120 us before), p99 0.7-0.9 ms in four of them (54-56 before), the IOPS the same -- writes over blocks written already, so neither a growth nor a conversion of their own: what else holds the image -- its calls come one at a time -- to find |
| G1 | done and measured: the guest keeps PCID and INVPCID (`hvarch::x86::cpu::microcode_revision`, as KVM tells it) |
| W1 | measured: 89% of halts caught in the spin (KVM's 90%), pipe 14.3 us -> 9.3 (KVM 6.0), and the guest's boot lost its 1.75 s (E2) |
| C1 | measured: a busy core's clock 4460-4715 MHz with `idle=mwait`, 4460 without, KVM's 4768 -- CPU-bound work 96-98% of KVM's against 94-95%, and nothing slower for it: 4k QD1 48 us against 50, pipe 8.5 us against 9.3, cyclictest within its noise. The default since 64d61a78 (`idle=halt` for HLT alone): 96-98% in both runs since. What is left of the clock is cores woken too often to sleep deep: every idle CPU's 100 Hz tick among them, which Linux stops |
| T1 | measured: the guests' timer interrupts requested 9-10 us past their edges on average -- and 10 ms at most, an edge left for the host's next tick: a wakeup lost somewhere, to find. cyclictest 73-76 us against KVM's 31, unchanged by W1 and C1. With the edge between the loop's look and the host timer's arming taken a round later (64d61a78: 48926-51809 such a run) the average is 7 us and cyclictest 66-67 us -- and the most still 10009-10010 us, in both runs: another path, 10 ms and a wake's cost, as long as `MAX_HALT_WAIT_NS` and the host's tick. The report's `late` line (28af927d) says where edges more than 1 ms late were waited for. Found (a68aeb3a's report): 241 of a run's ~12.4k came after a host's interrupt took out a guest that had computed past its edge -- the 10 ms, a host tick: the round armed the host's timer with interrupts on, and any other interrupt of that timer before the entry re-armed it for the next tick (`Cpu::local_timer_fired`); the rest after sleeps that had the edge and ended 1-3 ms late, nearly all by a ring -- the machine's own stalls, 3.1 ms at most under KVM's cyclictest too. Fixed: the edge armed in `Kick::entering`, interrupts off until the guest runs (7ae441c4), and armed before it is looked at (ddcbd4c6: 7ae441c4's look first left it unarmed for an edge that came between, its `worst` line said) -- the most 10009 us -> 3026 us, a sleep woken late by the machine's own stall; 50823 entries of a run turned back for an edge come on the way in; average 7 us, cyclictest 65 us (KVM 31) |
| W2 | measured: past the world switch an exit costs 545-559 ns handled and 479-496 ns to the next entry -- about a microsecond of the host's a guest exit, of 122 million in 43 minutes, 54 million of them writes of an MSR (an IPI, a TSC deadline: 466-479 ns handled), 48 million halts; `hv bench profile` 1092-1127 ns an exit on a P-core, 1692 on an E-core. The fast path's target is the microsecond: what pipe (65-71%) and pgbench -S with one client (79-82%) wait on. The fast path for the two writes (64d61a78): 52.8-54.2 million of 125 million exits re-entered without the loop's round, the loop 601-609 ns handled and 406-417 ns to the next entry; pipe 8.95-9.1 us (66-67%) and pgbench -S with one client 80-83% as before -- not what they wait on |
| G2 | left: XSAVES, WAITPKG, MOVDIRI/MOVDIR64B and the TSC's frequency in KVM's leaf move nothing the suite measures -- the guest's XSAVEC and XSAVEOPT do what XSAVES would for it, and the rest is a delay loop's TPAUSE, an accelerator's stores and a calibration at boot |
| E1 | measured: random reads 84-85% with the clock 4% higher under `idle=mwait` as without -- not the clock: the nested walk, the TLB, to look at |
| E2 | solved: the guest's kernel up in 1.64 s (KVM 2.35 s), its boot 4.56 s against 5.24 -- the 1.75 s of IPIs waited on after `async_tx` gone with W1 |
