# A VM under nos against the same VM under KVM

One disk image, one server: an Ubuntu 24.04 guest of 4 CPUs, 8 GiB of memory
and an 8 GiB disk, run once under QEMU/KVM on the server's own Linux and once
under nos's hypervisor after the next boot, the same benchmark suite running
inside it at boot. What it found, and what the work on it changed, is in
[`plans/vm-performance.md`](../../plans/vm-performance.md); what is left, in
[`plans/vm-perf-todo.md`](../../plans/vm-perf-todo.md).

## What makes the two runs the same

- **The disk.** The guest's disk is a raw file on nos's own root partition in
  both runs: Linux mounts it for the KVM run (the file sparse, as nos's copy
  is), and nos holds it as a disk's image in its run. The same NVMe, the same
  ext4, a fresh copy of the same golden image each time.
- **The CPUs.** nos's hv puts a lone 4-CPU guest on two whole fast cores and
  its disks' task on a slow core of its own (`hv/src/place.rs`). KVM's vCPU
  threads are pinned to the same four CPUs, its iothread to the same slow
  core, and the rest of QEMU to the other slow cores (`host/bench-host.sh`).
  The guest sees two cores of two threads under both.
- **The memory.** 8 GiB of 2 MiB pages, as nos's huge pool gives a guest:
  hugetlbfs, preallocated, for QEMU.
- **The devices.** virtio-blk with an iothread, `cache=none`, `aio=native`,
  `discard=unmap`; virtio-net; `-cpu host`. The same HWP policy on both
  (the whole range of clocks, turbo included).

What is not the same is said by the suite itself, at its start: the guest's
kernel line, the clock source, the vulnerabilities its kernel reports and the
CPU flags it sees (`BENCH vuln`, `BENCH flags`).

## The suite

`guest/run.sh`, run once by `bench.service` when `/opt/bench/AUTORUN` exists,
each step's numbers printed on the serial console as `BENCH ...` lines as they
come and everything at the end as a base64 tar.xz between `BENCH-ARCHIVE-BEGIN`
and `-END` (`decode.py` takes it out), then the guest powers itself off -- which
is how either host knows it is done:

| step | what |
|---|---|
| sysbench cpu | primes, 1 and 4 threads, three runs each |
| 7-Zip | `7z b`, 1 and 4 threads |
| openssl | aes-256-gcm, chacha20-poly1305, sha256 at 16 KiB |
| sysbench memory | sequential reads and writes, random reads, 1 and 4 threads |
| perf bench | sched pipe, sched messaging, syscall basic |
| cyclictest | 4 threads, 1 ms interval, a minute |
| fio | a new 2 GiB file's first write (1 MiB, QD8), then over it: 4k random reads at QD1, QD32 and 4x16, 4k random writes at QD1 and QD32, 1 MiB reads and writes at QD8, 4k writes with an fdatasync each |
| kernel | a Linux tree untarred, `make -j4 defconfig` built, removed |
| pgbench | init at scale 100, read-only and TPC-B at 1, 4, 8 and 16 clients |

`/opt/bench/QUICK` beside it cuts every step short, to check the suite itself.
`/opt/bench/ONLY` cuts the run to the steps it names (`sysbench-cpu`, `7zip`,
`openssl`, `sysbench-memory`, `perf-bench`, `cyclictest`, `fio`, `kbuild`,
`pgbench`) -- `bench-host.sh nos-stage <step>...` writes it -- and takes two
words that no whole run has: `pipe-long`, perf's pipe alone for about a
minute, and `layout-trace`, fio's first write marked by a `layout-ready`
step 15 s before it and a `layout-done` 10 s after it -- each for a trace
window to land in.
The image is Ubuntu's cloud image, provisioned once by cloud-init under KVM
(`guest/provision.sh`: the tools, PostgreSQL, nothing left that wakes by
itself).

## Running it

It needs a server that boots both: nos installed by `nos-install` with its
GRUB entry, the server's Linux the default, and nos's root an ext4 partition
that Linux can mount. Both reachable by ssh at the same address, each with a
host key of its own. A run of either half takes about an hour; preparing the
golden image once, another.

```sh
mkdir -p out/vm-bench && cp bench/vm/bench.conf.example out/vm-bench/bench.conf
$EDITOR out/vm-bench/bench.conf            # the ssh config, nos's root partition, the CPUs

bench/vm/kvm.sh                            # nos's VMs stopped, the server on Linux, the KVM run
make image                                 # nos-x86_64.squashfs
bench/vm/nos-deploy.sh nos-x86_64.squashfs # installed for one boot, a fresh disk, back into nos
bench/vm/nos-run.sh mytry 1                # the nos run, the VMs that ran before started again

python3 bench/vm/compare.py out/vm-bench/results-kvm/archive/run/summary.txt \
    out/vm-bench/results-nos-mytry/archive/run/summary.txt
python3 bench/vm/compare-n.py kvm=bench/vm/results/kvm/summary.txt \
    ddcbd4c6=bench/vm/results/nos-ddcbd4c6/summary.txt mytry=out/vm-bench/results-nos-mytry/archive/run/summary.txt
```

**A device's comparison.** The same guest on a partition of its own instead
of a file -- under KVM the partition as the guest's raw disk, under nos given
to the VM by its id ([A device as a guest's
disk](../../docs/hypervisor.md#a-device-as-a-guests-disk)) -- with the two
steps on the server's Linux that stage it and run it:

```sh
bench-host.sh device-stage /dev/disk/by-partuuid/<guid> fio kbuild pgbench
bench-host.sh kvm-device /dev/disk/by-partuuid/<guid>     # results-kvm-device
```

`device-stage` refuses a device that is mounted, swap, held by an array or a
mapping, or smaller than `golden.raw`; gives the whole of it back
(`blkdiscard`), writes `golden.raw` over it byte for byte and reads it back,
and cuts the suite in it as `nos-stage` does. Staged again before nos's run,
the device is given there by `hv create benchdev /lib/hvboot mem=8192 cpus=4
net disk=part:<guid> force` -- `force`, since what it holds is the image's
partition table.

Every run stops the VMs nos runs (by their power buttons, as `hv stop` does)
and reboots the server twice; `nos-run.sh ... 1` starts the stopped VMs again.
`nos-install update --boot-once` keeps the server's kernel line, `/etc/rc` and
the panel's password, and leaves its GRUB's default as it was: a nos that does
not come up is left by the next power cycle. Each step polls rather than holds
a quiet ssh session, which a slow path between the workstation and the server
cuts.

## The results here

`results/` keeps every run's `BENCH` lines (`summary.txt`) and, for nos, hv's
report of the guest as it stopped (`hv-report.txt`: its exits, halts, timer
lateness, disks) -- all on a Hetzner EX44 (an i5-13500, six P-cores and eight
E-cores, a Samsung MZVL2512HCJQ NVMe), October and November 2026:

| directory | what it ran |
|---|---|
| `kvm` | QEMU 8.2 under Ubuntu 24.04's 6.8 kernel, the reference |
| `nos-bced1539` | master before the work, stopped at the kernel build |
| `nos-40f31a68-halt`, `nos-40f31a68-mwait` | the disk path (D1-D5), PCID, halt polling, MWAIT idle off and on |
| `nos-64d61a78` | 32 requests in flight, chunks as unwritten extents, the exit fast path, MWAIT idle the default |
| `nos-311baf25` | the chunks' commits flushing as before, growth doubling to 256 MiB |
| `nos-ea1dc0fe` ... `nos-dc8bbb5b` | the same, with the report telling where late timer interrupts were waited for |
| `nos-ddcbd4c6` | the host's timer armed for a guest's edge as the entry is made, and only then looked at |
| `nos-16f9eb9d` | indirect descriptors, every request to the device as it comes and back as each is done, 64 requests out -- and the idle task's window, which left a woken vCPU waiting out a 3 ms slice: 4k reads at QD32 13k IOPS |
| `nos-96281a83` | the same, the idle task looking at its queue with its flag up, and a disk's pass bound by the data it moves |
| `nos-dc529775` | the third round (vm-perf3), the short suite -- sysbench, perf-bench, cyclictest, fio: a guest's CPUs at EPP 0, the disk's task serving its device, guest memory copied through the huge pool's window, the ICR, TSC-deadline and HLT exits answered in the world switch, C6 for the idle CPUs no guest holds, growth ahead of a guest's stream of writes |
| `nos-dc529775-fio` | the same build again, fio alone: the first run's 1 MiB reads and writes (97%, 39%) were the device's state, back at 149% and 127% here |
| `kvm-device` | the same QEMU with the guest's disk a partition of its own, raw (`kvm-device`: p6 of the disk nos's root is not on, 5 November) -- the reference for a device |
| `nos-21b67b1e-device` | the guest's disk that partition, given to the VM by its id (`hv-local-disks`): fio, the kernel build and pgbench |

`results/comparison.md` is `compare-n.py` over the reference and the runs
that changed something; `results/comparison-device.md` the two on a device,
beside the file runs before them. The same code run twice moves some numbers by 5-10%
(4k random I/O at QD32, the fdatasync job's tail), so a change is to be read
across runs, not off one.
