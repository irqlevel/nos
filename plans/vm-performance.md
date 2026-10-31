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

## Status

| item | state |
|---|---|
| D1-D5 | done 2026-10-31 on branch `vm-perf`: smoke both arches, `fs-fuzz.py` (the image's map held to the filesystem's after every change, chunks of 1 to 64 blocks, vectors with sectors of one block, disks whose write of zeros is there, missing or a lie), `hv-test.py`; to measure on the EX44 |
| G1 | done: PCID was offered all along, and Ubuntu's 6.8 turned it off itself on reading microcode revision 0 -- the guest is told the host's (`hvarch::x86::cpu::microcode_revision`), as KVM tells it; `hv-fuzz.py`, `hv-test.py` |
| W1 | done: halt polling, KVM's growth and drop, never spun to a timer's edge (a first version that was found a spin by `hv-fuzz.py`'s `platform` target); `hv-fuzz.py`, `hv-test.py` |
| C1, W2, T1, G2, E1, E2 | to do |
