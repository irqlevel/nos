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
