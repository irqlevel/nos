# VM performance: what is left

Where a Linux guest under nos's hypervisor still falls behind the same guest
under QEMU/KVM on the same server, and where to look for each. The numbers are
the last run's (96281a83, `bench/vm/results/nos-96281a83`) against KVM's
(`bench/vm/results/kvm`), on the Hetzner EX44; how they were measured and what
the work so far changed is in [`vm-performance.md`](vm-performance.md), and
the kit that measures them is [`bench/vm`](../bench/vm/README.md).

Two things to keep in mind for any of it. The same code run twice moves some
numbers by 5-10% (4k random I/O at QD32, the fdatasync job's tail), so a
change is read across two or three runs, never off one. And every run stops
the server's VMs for an hour: a run is planned, not tried.

Where 96281a83 stands: 1 MiB reads at 150% of KVM's, 1 MiB writes at 102%, 4k
reads by four jobs of 16 at 171% (372k IOPS against 217k), the fdatasync job
at 115% with its write's p99.9 72 us (KVM's 226), pgbench's writes at 106-156%.
Behind: a new file's first write (67%), 4k I/O from one job (71-83%),
cyclictest's average (43%), perf's pipe (73%), random reads of memory (85%),
CPU-bound work (96-98%).

**Round 12 (a0ae873e, 2026-11-04): the same, measured from inside.** The
tracepoints' build ran the suite with a few seconds of records taken at each
phase that is behind (`bench/vm/results/nos-a0ae873e`: `summary.txt` and the
decoded windows in `traces.md`; the kit's `out/ex44-bench/run2/round-c12.sh`).
Its numbers are round 11's within the noise -- cyclictest's average 66 us,
4k reads at QD1 85%, at QD32 72% -- but one: sysbench's sequential writes of
memory by four threads, 21.6 GB/s where every run before had 45-50 (four
threads writing one shared 512 MiB buffer; no record was being made then,
and that step has next to no exits): to be run again before anything is made
of it. What the windows measured is under each heading below.

## Rounds 13-17 (vm-perf3, 2026-11-04): perf-bench, cyclictest and fio

Short runs of the suite's three steps (`bench/vm/guest/run.sh`'s ONLY, staged
by `bench-host.sh nos-stage <steps>`), each with round 12's trace windows; the
numbers against KVM's (`bench/vm/results/kvm`) and round 12's:

| | KVM | r12 a0ae873e | r13 7eaedbb5 | r17 (r16 0ca91bcf, the disk's hold off) |
|---|---:|---:|---:|---:|
| cyclictest avg | 31 us | 66.2 | 65.5 | **5.25** |
| perf pipe | 6.0 us | 8.44 | 8.49 | 8.06 |
| 4k read QD1 | 20.1k, 47 us | 17.2k, 52 | 19.4k, 45 | **20.0k, 44** |
| 4k write QD1 | 39.9k, 23 us | 28.9k, 27 | 39.5k, 18 | **39.3k, 18** |
| 4k read QD32 | 195k | 142k | 145k | 135k (112-145k) |
| 4k write QD32 | 178k | 126k | 130k | 104k (99-130k) |
| fdatasync | 620, 139 us | 708, 40 | 749, 23 | 722-749 |

- **C1E auto-promotion** was on (firmware's), and is off now (c4b1af44): it
  moved cyclictest by nothing.
- **The disk's task spins before it sleeps** (8f8d14fa): both its wakes, a
  QD1 request's 10 us -- reads and writes at KVM's and past it.
- **HWP's preference** was what a just-woken CPU's slowness was: with every
  CPU at EPP 0 (round 14, `hwp_epp=0`) the TSC deadline's write right after a
  wake cost 1.2 us of the host's where it had cost 10.1, a timer interrupt
  went in 0.15 us past its edge where it had 4.2, and cyclictest fell to
  5.75 us. Held at EPP 0 now are the vCPUs' CPUs while a guest runs
  (2398e421) -- not the disk's task's E-core: held too, every 4k request took
  3.8 us longer to its completion's interrupt on CPU 1 (round 15-16; why, not
  known), and without it they are back (round 17, 83f6cdbe).
- **No kick while eight requests are out** (0ca91bcf): the notifies that were
  41% of the QD32 job's CPU are gone -- 25 commands in flight at most, where
  10 were. What binds QD32 now is the device's being vCPU 0's: a request
  added waits on the ring for the round a completion's wake brings
  (3.8 us to the disk's task at p50, 0.8 before), and every completion for
  that round too (4.2 us back, 0.7 before); with fio's job on vCPU 0 itself
  each completion's wake is an exit of the job's (round 15: 273 of them, the
  job 36% in its guest). The QD32 numbers move 10-20% with where the guest
  puts the job. The lever: the disk's task serving the device itself, as
  QEMU's iothread with irqfd does -- the used ring written and the MSI posted
  from there, the ring looked at as it polls, vCPU 0 out of it.

## Rounds 18-23 (vm-perf3, 2026-11-04): the disk's task, copies, fast exits, growth ahead

The same short suite (sysbench cpu and memory, perf-bench, a long pipe,
cyclictest, fio, the first write traced), each round's windows and `cpufreq`
samples in `out/ex44-bench/results-nos-<sha>`; against KVM:

| | KVM | r18 007650c6 | r20 6a8ece3c | r21 faa0484b | r23 c0650c51 |
|---|---:|---:|---:|---:|---:|
| 4k read QD32 | 195k | 215k | **290k** | 284k | 288k |
| 4k read 4x16 | 217k, p99 358 us | 224k, 831 | **342k**, 880 | 347k, **268** | 325k, 292 |
| 4k write QD32 | 178k | 130k | **174k** | 172k | 164k |
| first write (2 GiB) | 4643 MiB/s | 3002 | 3351 | 3346 | 3025 |
| 1 MiB write QD8 | 3694 MiB/s | 4281 | 4619 | 4627 | **1454** |
| fdatasync | 620 | 733 | 737 | 734 | **371** |
| perf pipe | 6.0 us | 8.26 | (8.34 long) | **7.39** | 7.35 |
| cyclictest avg | 31 us | 6 | -- | **4** | 4 |
| sysbench cpu t1 | 1589 | -- | -- | 1526 | 1534 |
| memory random read t1 | 949 MiB/s | -- | 804 | 802 | 820 |

- **The disk's task serves its device** (007650c6): QD32 reads past KVM.
- **A guest's memory copied through the huge pool's window** (6a8ece3c): the
  disk's task on its E-core paid ~1 us of tmp_map and invlpg per 4 KiB; the
  4x16 job to 158%, QD32 writes to 98%.
- **The ICR and TSC-deadline writes answered in the world switch** (faa0484b):
  30.7 million of a run's 30.7 million WRMSRs; pipe 8.3 -> 7.4 us, cyclictest
  6 -> 4 us. A TSC-deadline exit had cost 3.4 us of the host's.
- **Growth ahead of a guest's stream of writes** (c0650c51) asked over blocks
  the file already had, 664 times a run: the 1 MiB writes and fdatasync above
  fell by half or more (round 23); aa2b4785 asks only where the image's map
  has a hole, and the boot's fstest holds a second pass over a written file
  to asking none.
- **The turbo**: `cpufreq` (cdc0756f, bf65d154) found the busy vCPU's core at
  4.55-4.58 GHz under sysbench, where Ubuntu on the same box runs it at 4.8
  (round 23's deploy, a busy loop on cpu 8), with idle P-cores' threads
  36-99% of the window in C1 -- nos's idle going deep only after four long
  idles in a row. CPU-bound work's 3-4% to KVM is that size. Next: a core's
  C6 for idles with the next timer 1 ms off, on CPUs no guest holds.
- **perf's pipe** (round 22's window): of a HLT's 2.2 us out of the guest,
  0.84 waiting for the ring, 0.98 from the wake to the next entry and 0.38
  from the exit to the halt. Next: the HLT waited out in the world switch's
  loop for what is posted (VT-x), the round left out.
- **Memory random reads** stay at 83-86% with the SMT sibling idle (not
  halt-polling), the EPT's leaves 2 MiB as KVM's are and the guest with PCID:
  what is left wants a PMU to see.
- **The guest has one virtio-blk queue**, KVM's four (one a vCPU). The 4x16
  job's p99 fell from 880 to 268 us with the exits answered in the world
  switch (r20 -> r21: the IPIs its completions wake the other vCPUs by) and
  stayed there (276, 292): more queues are not a finding yet.

## Rounds 24-27 (vm-perf3, 2026-11-04): turbo, halts, growth ahead tuned

| | KVM | r23 c0650c51 | r24 aa6776a8 | r25 a4548330 |
|---|---:|---:|---:|---:|
| sysbench cpu t1 / t4 | 1589 / 3259 | 1534 / 3192 | **1579 / 3299** | 1579 / 3297 |
| perf pipe | 6.0 us | 7.35 | **6.42** | 6.66 |
| first write (2 GiB) | 4643 MiB/s | 3025 | 3220 | **3785** |
| 1 MiB write QD8 | 3694 MiB/s | 1454 | **4604** | 4648 |
| fdatasync | 620 | 371 | **713** | 729 |
| 4k read QD32, p99 | 195k, 232 us | 288k, 337 | 296k, **154** | 293k, 154 |

- **A core's C6 for idles 1 ms from their next timer** on CPUs no guest holds
  (aa6776a8): the busy vCPU's core at 4763 MHz where it was at 4552-4581,
  the housekeeping P-cores' idles 40-94% in C6 where they were 36-99% in C1;
  CPU-bound work at KVM's.
- **The HLT waited out in the world switch for what is posted** (79e4347f):
  53 million HLTs a run ended there, 1.9 million went the loop's way; the
  pipe's HLT 1.3 us out of the guest with its wait, where it was 2.4.
- **The hole check** (aa2b4785) undid round 23's regression; growth ahead
  four chunks first, to a gigabyte (a4548330): the first write at 82%.
  Round 25's window: 19 commits of 5-33 ms by the growths ahead -- the later
  with the device busy -- their stretches 108 MiB on average, every write of
  the guest's elsewhere having begun the stream again: a stream now outlasts
  STREAM_MISSES of them (dc529775), which moved the first write nowhere
  (80% in rounds 26 and 27). What is left of its gap to KVM's is the commits
  themselves, each two flushes of a device busy with the guest's data; KVM's
  host commits once, at the guest's fsync.
- **Rounds 26 and 27 are one image** (dc529775): 26's 1 MiB reads (97%),
  writes (39%) and fdatasync (97%) were 149%, 127% and 117% in 27, whose
  deploy found the NVMe drives at 38 and 32 C -- a device's state, not the
  code's, and a reason to read any one round's dip twice before acting on
  it. Memory random reads were at 92% (t1) and 97% (t4) in round 26.

## Done since ddcbd4c6

- **The fdatasync job's tail** (p99 0.7-0.9 ms and p999 ~1 ms in every run
  since 311baf25): gone -- 61-62 us at p99 in both runs since the disk's
  requests go to the device as they come (9fde9bd7), 72 us at p999 once the
  idle task's window was closed (below).
- **1 MiB writes and 4k reads by several jobs**: indirect descriptors
  (c88699b5 -- without them a 256 KiB request took 66 of the ring's 256
  descriptors, and Linux had three out at once), each request to the device
  as it comes and back as each is done, from buffers the device reads and
  writes as they are (9fde9bd7), 64 requests with a guest's disk and 256
  commands with the NVMe (16f9eb9d).
- **The machine's own stalls** (about 12 000 vCPU sleeps a run ending 1-3 ms
  late, cyclictest's most 3.1-3.3 ms, the host's wakeups on the guest's CPUs
  3-4.6 ms late at most): not SMIs but the idle task's window -- out of its
  idling it was counted as any task, and an interrupt's decision left it the
  CPU over a task woken then, which waited out its 3 ms slice (d0a4b493,
  d92695bc). With the async disk's wakes it took 4k reads at QD32 to 13k IOPS
  (16f9eb9d's run). Now: 2 timer interrupts more than 1 ms late in a run, no
  sleep ending more than 1 ms past its time, cyclictest's most 205 us (KVM's
  3280). KVM's own 2.2-3.3 ms maxima are its own.
- **A disk's pass holding the platform's lock**: a guest writing 1 MiB at a
  time eight deep had 8 MiB copied under it, and its clocksource watchdog
  found a PM timer read taking 800 us; a pass now moves 512 KiB at most
  (96281a83).

## A new file's first write: 67%

3103 MiB/s against KVM's 4643 (1954 before the async disk). Every hole the
guest writes into costs a growth (244 a run, 6.3 ms on average, 18 ms at
most) and every flush after it a conversion (347, 5.7 ms on average, 29 ms at
most), each a commit of nos's ext4 that the guest's write or flush waits for:
its log, a flush and its commit block -- on this Samsung a FUA write costs
what a flush does, so two flushes' worth. Linux's ext4 under KVM does the same
work but off the guest's path: unwritten extents converted by a workqueue at
I/O completion, and many in one journal commit. pgbench's init (67%) is the
same writes.

- Where: `image.rs` (`grow`, `make_written`, `GROW_MAX_CHUNKS`),
  `src/crates/fs/src/ext4/data.rs` (`allocate`, `mark_written`), `journal.rs`.
- Levers: growing ahead of a sequential writer in the background, so that its
  write finds the blocks there; conversions of several guest flushes in one
  commit -- a flush still waits for the commit that makes its blocks written,
  but one commit can serve the flushes that came meanwhile. The image's
  let-go lines time both, and its requests by the batch -- the writes into
  holes, mostly: 284 a run, half of them within 8 ms.

## 4k I/O from one job: 71-83%

Reads at QD32 149k IOPS against 195k, writes 126k against 178k; at QD1 reads
53 us against 47, writes 26 against 23. The same disk does 372k with four
jobs, so it is not the device's side: it is what a request costs the CPU
that makes it. fio's submission latency says so -- 6-7 us against KVM's 1-2,
at every depth: the notify, a port write that leaves the guest, has its pass
run on that CPU under the platform's lock (`Blk::fill`: the chain taken, its
header read, a write's data copied, the request handed to the disk's task and
the task woken), where KVM's ioeventfd only signals its iothread.

- The guest's disk has one queue; under KVM it has four, one a vCPU
  (`virtio_blk virtio0: 4/0/0 default/read/poll queues`), each with its own
  vector, so a request comes back on the CPU that made it. Under nos every
  completion interrupts the one CPU the queue's vector is on, which wakes
  the job's CPU with an IPI when it is another. virtio-blk's multiqueue
  (`VIRTIO_BLK_F_MQ`) is what the NIC's queue pairs already are
  (`devices/net.rs`).
- The notify's own cost: profile the vCPU's host CPU during the QD32 job
  (`docs/profiler.md`) to see where its 4-5 us go before moving any of it --
  the pass could hand the chain over and leave the rest to the disk's task.
- At QD1 the requests sent as they come lost about 4 us each against the
  batches they replaced (reads 91% -> 83% of KVM, writes 81% -> 74%): the
  disk's task sleeps on its queue's event between a request and its
  completion (`block::queue::wait`), and the NVMe's interrupt wakes it, where
  a batch waited on the NVMe's `WaitGroup`, which spins on `schedule()` and
  never sleeps. A short poll of the completion ring before the sleep, grown
  and dropped as the halt poll is -- QEMU's iothread polls its AioContext so.
- Then a completion goes from the disk's task to the first vCPU, whose
  round gives it to the guest (`run.rs`: the devices' rounds are the first
  CPU's): a wake more than KVM's irqfd, when the job is on another CPU.

**What round 12's windows measured** (p50 of about 680 requests each, the
stretches `trace-decode.py --hv` pairs a request through):

| | in all | to the disk's task | served by the image | of it, the device | back to the vCPU |
|---|---:|---:|---:|---:|---:|
| 4k read, QD1 | 49.2 us | 4.9 | 43.3 | 37.9 | 1.3 |
| 4k write, QD1 | 20.6 us | 5.0 | 14.8 | 9.4 | 0.8 |
| 4k read, QD32 | 50.6 us | 4.1 | 44.2 | 38.2 | 1.8 |

and the notify's exit costs the host 3.0 us after it, every time. So at QD1
the host's part of a read is about 11 us of 49 and of a write 11 of 21 --
the disk's task waking for the request (4.9-5.0), waking again for the NVMe's
completion (most of the image's 5.4 beyond the device) and the notify (3.0);
the vCPU's own pick-up is cheap, its halt's poll catching the ring (862 of
963 halts ended in the spin; a ring to a polling vCPU reaches its entry in
0.85 us, one to a sleeping vCPU in 2.4). At QD32 the device never had more
than **9 commands at once** though the guest keeps 32 out: the job's vCPU
made 1136 notify exits for 674 requests, 3.0 us of the host's each, and was
in its guest 51% of the time -- the submission is that CPU's, one request at
a time, which is fio's 6-7 us of submission latency.

The levers, in the order the numbers put them:

1. **The disk's task polls before it sleeps** -- for a request from the
   device and for its image's completions, a spin grown and dropped as the
   halt poll's (`modules/hv/src/disk.rs`'s `serve`, `ImageQueue::wait`):
   both its wakes, about 10 us of a QD1 request. QEMU's iothread polls.
2. **The guest kicks less**: notifications off while the disk has requests
   out -- EVENT_IDX's `avail_event` left behind, the ring looked at again
   at each completion (`Blk::poll`'s `fill`) -- so that a QD32 job does not
   exit per request; then the notify's own 3 us.
3. Multiqueue, as above.

## Waking a guest's CPU: cyclictest 72 us, perf's pipe 8.2 us

cyclictest's threads average 60, 90, 59 and 78 us against KVM's 27-35; perf's
pipe 8.2 us against 6.0; pgbench's read-only queries with one client 87%. The
host is not where it goes: hv's report says a timer interrupt reaches the
guest 3 us after its edge on average, 4.2 ms once at most (a host interrupt
that took the guest out), and no vCPU sleep ended more than 1 ms late. So the
rest is the guest's own path from the interrupt to cyclictest's thread
reading the clock, under nos 50-80 us and under KVM 25-30 -- the same kernel.

- Count what the guest does on that path: the exits a cyclictest cycle makes
  (`hv`'s report over a run of cyclictest alone, against its cycles), and
  which -- the tick restarted on idle's exit is a write of TSC_DEADLINE, an
  IPI a write of the ICR.
- Not the paravirtual features KVM's guest has and nos's has not. nos offers
  KVM's signature, NOP_IO_DELAY and PV EOI (03e3ede8) and leaves the rest
  out on purpose (`src/crates/hv/src/policy.rs`): PV TLB flushes, PV sched
  yield and PV spinlocks stand on steal time and help a guest whose CPUs get
  preempted, which a guest on cores of its own is not; kvmclock does what
  the invariant TSC does. KVM's PV IPIs spare a guest in physical x2APIC mode
  -- as KVM's is -- an ICR write a destination; nos's guest is in cluster
  mode, one write for all four of its CPUs.
- The guest's clock: under KVM it runs on `kvm-clock` and its CPUID says
  `tsc_known_freq`; under nos it is told the TSC's frequency on its command
  line (`tsc_early_khz`, which hvboot adds), runs on `tsc`, and its CPUID says
  nothing of it (KVM's leaf 0x40000010, or leaf 0x15;
  `src/crates/hv/src/policy.rs`).
- What the exit costs past the world switch: 1011 ns handled and 545 to the
  next entry on average (`hv`'s report, `loop`); `hv bench profile` splits
  the rest.
- The one timer interrupt 4.2 ms late (`worst`): the guest was entered 995 us
  before its edge with the host's timer armed for the edge, and ran on until
  a host interrupt took it out 4068 us after it -- an arming lost, as T1's
  were before ddcbd4c6, once in a run.

**What round 12's windows measured.** In cyclictest's: the vCPUs' sleeps to
their timers' edges ended **17.3 us past their time** at p50 (p99 21.8) --
`TIMER_ADVANCE_NS`, 10 us, covers part of it -- so the guest's timer
interrupt was asked for 8.7 us past its edge at p50, 13.1 at p99; and the
exits right after such a wake cost the host **11.0 us** each (the TSC
deadline's write, 887 of them) where on a busy core, in the pipe window, the
same write costs 1.3. The host is slow for the first microseconds after an
idle: its clock coming back up, or the idle itself deeper than nos asks --
nos's MWAIT asks for C1, and nothing in it clears MSR_POWER_CTL's C1E
auto-promotion, which Linux's intel_idle turns off on these CPUs. Read that
MSR first; then spin the last stretch before an edge rather than sleep it
(the advance as long as the wake costs, measured). In the pipe window a ring
reaches a sleeping vCPU's entry in 3.7 us, a polling one's in 1.1.

The same slowness shows on a mostly idle guest of one CPU (the panel's,
Debian on the PIT and the 8259): 11-12 us of the host's after each of its
port exits, 2.8 of them before the exit's record -- where in the busy pipe
window the bench's first CPU, which runs the same devices' round after each
exit, pays 1.4 us a port exit. So it is the idle core, not the round; the
round is still worth leaving out after a PIT or 8259 write, as the TSC
deadline's and the ICR's writes leave it out (W2), once the idle's cost is
known.

## Random reads of memory: 85%

sysbench's random reads of memory with one thread, 804 MiB/s against 949; 92%
with four. Not the clock (MWAIT idle moved it by nothing). The nested walk:
what the EPT's leaves say (2 MiB, their memory type, accessed and dirty
flags), and what the EPT pointer asks of the walk (`src/crates/hv/src/npt.rs`,
`src/crates/hvarch/src/x86/vmx.rs`). `perf stat -e dTLB-load-misses` in the
guest under each would say whether it is the TLB.

## CPU-bound work: 96-98%

7-Zip, openssl and sysbench's CPU test at 96-98% of KVM's, the clock 4460-4715
MHz against 4768: the cores nos leaves idle wake too often to sleep deep --
every idle CPU's tick, 100 a second, which Linux stops. A tickless idle in nos
is the lever (`kernel/cpu.cpp`: `Cpu::idle_mwait`, the tick).

## CPUID the guest does not get

XSAVES, WAITPKG, MOVDIRI/MOVDIR64B, TSC_ADJUST, self-snoop, CPUID faulting,
the L1D flush and the TSC's frequency, which KVM's guest has
(`vm-performance.md`, G2). None of them moves what the suite measures but
perhaps the TSC's frequency (above).
