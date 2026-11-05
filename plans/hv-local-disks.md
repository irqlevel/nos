# hv: a VM's disk on a partition or a whole disk of nos's

Asked 2026-11-05: a VM created with a partition or a disk nos sees as its
disk, `fs/image` left out of the way entirely. Decided with the user: whole
disks as well as partitions; a device with a filesystem on it given only with
`force`; provisioning a device from a cloud image in the same round; a spare
partition on the EX44 for the tests.

## Where it starts

A guest's disk is a file today: virtio-blk in `hv` -> `FileDisk`
(`modules/hv/src/disk.rs`) -> `kcore::fs::ImageQueue` -> `fs::image` (the
file's extent map, growth, growth ahead, ext4 commits) -> `block::queue` ->
the NVMe driver. What is there to build on:

- `hv::disk::Backend` is narrow (`size`, `read_only`, `with_buffer`,
  `submit`, `take`) and a guest takes any `Box<dyn Backend>` (`guest.rs`).
- `block::queue::Queue` is the asynchronous path `fs::image` itself uses;
  128 queues of 256 I/Os each, where VMs have 16 x 4 disks at most.
- The block table's claims (`block/src/table.rs`) refuse overlapping
  stretches of one disk: a mounted partition, the disk under it and the
  partitions over it exclude one another -- nos's root is safe already.

What is missing: stable names (Linux and nos alike number the EX44's two
NVMe drives differently from boot to boot, and the table keeps no GPT GUID
and no serial number), a claim's holder named by the module that holds it
(it reads "a module writing to it"), discard, and a module's way to the
asynchronous path without `unsafe` of its own.

## What it looks like

- vm.toml: `disks = ["part:3f0c...", "/vms/web/seed.iso:ro"]` -- devices and
  files mixed, `:ro` for either. `part:<PARTUUID>` is a partition: its GPT
  unique GUID, or `<MBR signature>-<number>` as Linux writes them;
  `disk:<id>` is a whole disk by its serial (`nvme-<serial>`). A device's
  name (`nvme14`) is taken where a device is given and written down as its
  id -- never stored as a name.
- `hv devices`: what can be given to a VM -- id, name, size, who holds it,
  which VM's vm.toml names it, what is on it.
- `hv create <name> <url> disk=<device> [force]` pours a cloud image onto the
  device and gives back the rest of it; `hv create <name> disk=<device>` and
  `hv disk attach <vm> <device> [ro] [force]` give a device as it is.
- The `vm` provider and the panel: a device list, create onto a device,
  attach one.

## Safety

1. A VM's start claims each device it has, named `vm <name>`: while it runs
   no mount, disk log, other writer or VM can have it, nor anything that
   overlaps it.
2. Never given: an EFI System partition, a BIOS boot partition.
3. A device with something on it -- an ext2/3/4, XFS, btrfs, swap, FAT,
   NTFS, LVM, md RAID, LUKS, ISO 9660, a partition table -- is given only with
   `force`, the refusal saying what was found. Checked where a device is
   given, not at every start: from then on its contents are the VM's.
4. A device another VM's vm.toml names is given only with `force`.
5. `hv clone` of a VM with a device disk is refused; `hv delete` leaves its
   devices alone and says so.
6. A guest can put an ext4 labelled `nos` on its disk: a machine with device
   disks boots with `root=UUID=` (as nos-install sets it), and `root=auto`
   refuses two candidates rather than taking the first.
7. With a whole disk nos reads the guest's partition table at every boot
   (and its superblocks under `root=auto`): both are fs-fuzz's already.

## Steps

**P1, the block layer.** Stable ids in the device table (GPT unique and type
GUIDs, MBR signature and number, the NVMe serial), in `disks` and the
`block` provider, and a lookup by id; a claim with a holder's name;
`kcore::block::DiskQueue`, `ImageQueue`'s calls over a device -- a request
split at the device's largest I/O and given back whole, done at once on a
device without the asynchronous path, no `unsafe` in the module; discard by
the NVMe's Dataset Management. Gates: parttest, fs-fuzz, netblk-test,
sshd-test (the ffi digest), the smoke boots.

**P2, a VM's disk on a device.** `DeviceDisk`, sharing the disk task's loop
with `FileDisk`; the device's virtio-blk features from its backend (discard
where the device has it, no write of zeros, `BLK_SIZE` for 4 KiB sectors);
vm.toml's words, `hv devices`, the checks above, the provider and the panel.
Gates: hv-test's new phase (a QEMU NVMe with a GPT: a guest's disk on a
partition, the refusals, the partition found by PARTUUID with the drives
swapped), hv-linux-test `--device`, hv-fuzz, api-test, web-test, the panel's
tests.

**P3, a device provisioned.** `hv create <url> disk=`: the cloud image
converted straight onto the device, the rest of it discarded; `hv convert`
onto a device.

**P4, speed, each measured.** An asynchronous flush; DMA straight into the
guest's memory rather than through a slot's buffer -- its memory is the huge
pool's 2 MiB pages, a request's segments become PRP lists, and a request
whose segments PRP cannot take goes through the copy as now.

## What to expect

The first write to a new file -- 80% of KVM with a file -- the most: no
growth, no ext4 commits, no flushes of its own. 4k reads and writes a little
(no extent lookup, no image lock). 1 MiB transfers not until P4 (the copy on
the E-core stays). The fair comparison is KVM on the same partition as a raw
drive.

## What was measured (EX44, 5 November)

P1-P3 as 21b67b1e. A 15 GiB partition carved for it (p6, `nos-hv-test`, on
the disk nos's root is not on; the disk log's p5 cut from 16 GiB to 1 GiB
for the room), the bench's golden image written onto it before each run, the
suite cut to fio, the kernel build and pgbench; under KVM the partition the
guest's raw disk (`bench-host.sh kvm-device`), under nos the VM given it by
its id (`hv create benchdev ... disk=part:<guid> force`), the same CPUs.
One run each, KVM's first (`bench/vm/results/comparison-device.md`):

| | KVM, the partition | nos, the partition | nos, a file (`dc529775-fio`) |
|---|---:|---:|---:|
| first write, 1 MiB QD8 | 4762 MiB/s | 4633 (97%) | 3696 (78%) |
| 4k reads QD1 / QD32 / 4x16, IOPS | 20.6k / 220k / 240k | 101% / 147% / 152% | 99% / 134% / 140% |
| 4k writes QD1 / QD32, IOPS | 50.8k / 238k | 90% / 71% | 88% / 67% |
| 1 MiB reads / writes QD8 | 2185 / 4867 MiB/s | 100% / 95% | 99% / 96% |
| 4k writes, an fdatasync each | 634 IOPS | 120% | 115% |
| kernel build, make -j4 | 318 s | 320 s (99%) | -- |
| pgbench rw c1 / c16, tps | 423 / 4679 | 161% / 111% | -- |

KVM on the partition is faster than KVM on a file of nos's root (31
October): 4k writes 128% and 134%, 1 MiB reads 150% and writes 132% -- so the
partition's reference is higher than the file's. Against it nos's first write
is at 97% where a file's was at 78-80%; its 4k writes at QD32 at 71%, where
KVM gained a third from the partition and nos 5% (168.7k IOPS against a
file's 160.1k): the row to look at next, with a trace window, before reading
anything into it from one run. On the real machine `disks` gave both
Samsungs their model and serial and every partition its GUID, `hv devices`
told the EFI System partitions, Ubuntu's md members, nos's root and the disk
log apart, and the VM held p6 for the 36 minutes it ran.

## Limits of the first version

Devices are those nos found at boot; one VM a device, read-only or not; the
old contents of a device given as it is are the guest's to read.
