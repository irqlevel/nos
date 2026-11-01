# VM performance: what is left

Where a Linux guest under nos's hypervisor still falls behind the same guest
under QEMU/KVM on the same server, and where to look for each. The numbers are
the last run's (ddcbd4c6, `bench/vm/results/nos-ddcbd4c6`) against KVM's
(`bench/vm/results/kvm`), on the Hetzner EX44; how they were measured and what
the work so far changed is in [`vm-performance.md`](vm-performance.md), and
the kit that measures them is [`bench/vm`](../bench/vm/README.md).

Two things to keep in mind for any of it. The same code run twice moves some
numbers by 5-10% (4k random I/O at QD32, the fdatasync job's tail), so a
change is read across two or three runs, never off one. And every run stops
the server's VMs for an hour: a run is planned, not tried.

## The fdatasync job's writes have a tail they did not have

A 4 KiB write at random with an fdatasync after each: the IOPS are KVM's and
more (112-116%), but since 311baf25 a write's p999 is 0.9-1.1 ms in every run
(104-120 us before it) and its p99 0.7-0.9 ms in four runs of six (54-56 us
before; KVM's 166). These are writes over blocks written already -- the job's
file is the one the layout wrote and made written at its end -- so no growth
and no conversion of their own.

The likely reading, to be shown before anything is changed: an image's calls
come one at a time (`&mut self`, and the slot's lock across the C ABI), and
something else holds the image while the write waits -- a growth (6.2 ms on
average, 18 ms at most in a run), a conversion of another file's blocks (5.6
ms, 14-29 ms at most), a map taken again under the mount's lock, or nos's own
journal checkpointing. 311baf25 made growths fewer and bigger (to 256 MiB) and
took the flush out from before a conversion.

- Where: `src/crates/fs/src/image.rs` (`write_vec`, `sync`, `make_written`,
  `grow`, `give`, `retake`), the disk task that serves the guest
  (`src/crates/modules/hv/src/disk.rs`, `src/crates/hv/src/devices/blk.rs`),
  `src/crates/fs/src/ext4/journal.rs` (`commit`, `checkpoint`).
- How: time every call of the image, not only its growths and conversions --
  a histogram of `write_vec` and `sync`, and for the slowest what it waited on
  -- in its let-go lines, as `Timed` does now; one run. 64d61a78's results
  have no tail to compare with (`bench/vm/results/nos-64d61a78`).

## A new file's first write: 42%

1954 MiB/s against KVM's 4643. Every hole the guest writes into costs a
growth (262 a run) and every flush after it a conversion (350), each a commit
of nos's ext4 that the guest's write or flush waits for: its log, a flush and
its commit block -- on this Samsung a FUA write costs what a flush does, so
two flushes' worth. Linux's ext4 under KVM does the same work but off the
guest's path: unwritten extents converted by a workqueue at I/O completion,
and many in one journal commit.

- Where: `image.rs` (`grow`, `make_written`, `GROW_MAX_CHUNKS`),
  `src/crates/fs/src/ext4/data.rs` (`allocate`, `mark_written`), `journal.rs`.
- Levers: growing ahead of a sequential writer in the background, so that its
  write finds the blocks there; conversions of several guest flushes in one
  commit -- a flush still waits for the commit that makes its blocks written,
  but one commit can serve the flushes that came meanwhile.

## 1 MiB writes: 63%; 4k at QD32: 72-84%

1 MiB writes at QD8 2314 MiB/s against 3694 (reads 93%); 4k random reads at
QD32 149-165k IOPS against 195k, writes 123-139k against 178k, 4x16 78%. The
disk's work is one task on one slow core, as KVM's iothread is -- the same
footing -- so it is what each request costs there.

- Every byte goes through the image's own buffer (`Direct::batch`, a
  `DmaBuffer`): the guest's memory copied into it on a write and out of it on
  a read, on the slow core. KVM's O_DIRECT has the device read and write the
  guest's pages. The guest's memory is 2 MiB pages of the huge pool,
  physically contiguous: the NVMe's PRP lists could name them, the image
  checking every range against the guest's memory as it does its own.
- Then what a request costs past the copy: the virtqueue (`blk.rs`), the
  image's resolve, the NVMe submission and its interrupt. nos's profiler on
  the disks' CPU while fio runs shows where (`docs/profiler.md`).

## Waking a guest's CPU: cyclictest 65 us, perf's pipe 9.6 us

cyclictest's average 65 us against KVM's 31, perf's pipe 8.9-9.6 us against
6.0, pgbench's read-only queries with one client 80-83%. The timer itself is
on time now -- 7 us late on average, 3 ms at most and that the machine's own
-- and 91% of halts are caught in the halt poll, as KVM catches 90%. What is
left is the path from an IPI or a timer to the guest running again, and what
the guest does in between.

- The guest's clock: under KVM it runs on `kvm-clock` and its CPUID says
  `tsc_known_freq`; under nos it is told the TSC's frequency on its command
  line (`tsc_early_khz`, which hvboot adds), runs on `tsc`, and its CPUID says
  nothing of it. Give it the frequency in CPUID too (KVM's leaf 0x40000010,
  or leaf 0x15) and see whether its timers -- and cyclictest's measure of
  them -- change (`src/crates/hv/src/policy.rs`).
- An IPI to a halted vCPU: the sender rings the doorbell, which signals an
  event, which sends the host CPU an IPI, which schedules the vCPU's task
  (`hv/src/smp.rs` `Doorbell`, `hv/src/run.rs`'s halted path,
  `kernel/event.cpp`). KVM wakes the vCPU's thread from the posted-interrupt
  wakeup vector. Time the steps with the cycle counter, a few thousand wakes
  at a time.
- What the exit costs past the world switch: 617 ns handled and 321 to the
  next entry on average (`hv`'s report, `loop`); `hv bench profile` splits
  the rest.

## Random reads of memory: 85%

sysbench's random reads of memory with one thread, 803 MiB/s against 949; 92%
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

## The machine's own stalls

About 12 000 of a run's 10 million halted vCPU sleeps end 1-3 ms late, nearly
all ended by a ring rather than their time; cyclictest's most is 3.1-3.3 ms
under KVM as under nos, and the host scheduler's wakeups on the guest's CPUs
come 3-4.6 ms late at most since boot (`sched 10000`). Something stops the whole machine for milliseconds --
SMIs, most likely. Nothing to do in nos, but worth knowing for certain:
MSR_SMI_COUNT (0x34) read at a run's start and end says how many came.

## CPUID the guest does not get

XSAVES, WAITPKG, MOVDIRI/MOVDIR64B and the TSC's frequency, which KVM offers
(`vm-performance.md`, G2). The first three move nothing the suite measures;
the TSC's frequency may (above).
