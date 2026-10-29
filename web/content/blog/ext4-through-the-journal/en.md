---
title: "ext4 in nos: a root that survives a power cut"
description: "New nos installs get ext4, written through its own journal: names and data, freed at scale through the orphan list, every checksum checked on every read. How it is built from three readings of one format that share no code, how it is held to e2fsck and Linux, and what the first real server and a review turned up."
date: 2026-10-30
tags: [storage, rust, testing, fuzzing, safety, hardware]
---

Until 29 October every root filesystem nos made was ext2. ext2 is simple, and its simplicity is the problem on a server. It has no journal, so a power cut in the middle of a change can leave the filesystem half changed, and what repairs that is e2fsck, run from another system. It has no checksums, so a damaged block reads as whatever it now happens to say. On a machine whose only console is the network, both mean a trip to the rescue system.

ext4 fixes both. With a journal, a change is on the disk whole or not at all, and the next mount replays what was committed. With `metadata_csum`, every superblock, descriptor, inode, extent block and directory block carries a CRC32C, and a damaged one is refused instead of believed. It is also the filesystem most Linux systems already run on, so e2fsck, debugfs and Linux itself can check a nos root and judge what nos wrote to it.

Since 29 October, `nos-install`, `scripts/mkrootfs.sh` and `scripts/build-disk.sh` make ext4. They name its features one by one from none — has_journal, ext_attr, resize_inode, dir_index, filetype, extent, 64bit, flex_bg, sparse_super, large_file, huge_file, dir_nlink, extra_isize and metadata_csum — which is e2fsprogs 1.46's ext4, the one GRUB 2.06 can boot from. The Hetzner EX44 has run on an ext4 root since that day. ext2's driver stays as it was, for the roots made before.

These commits did it, merged as [3b00c7f](https://github.com/irqlevel/nos/commit/3b00c7fcd4471bd0e1fe8d19a03b2e1c860b1aec) and, after a review, [a92314e](https://github.com/irqlevel/nos/commit/a92314e0bc2740f587e6e8905e2e7cd04eefb25e):

| Commit | What it added |
|---|---|
| [4a6cc5d](https://github.com/irqlevel/nos/commit/4a6cc5db8c1e79cf9fbab32805bc8c3adcc8b800) | `extfmt`: the ext2/3/4 format and every checksum on it, as a crate with no kernel in it |
| [9443eb4](https://github.com/irqlevel/nos/commit/9443eb4089bd01c5bb99f9335860dd7602d65e53) | The journal's format, and the plan of a replay, held to e2fsck's replay byte for byte |
| [299f063](https://github.com/irqlevel/nos/commit/299f0638d4be415a769c4101c04c6b6a50800e3c), [eea5268](https://github.com/irqlevel/nos/commit/eea5268a4de739ad2569387b7c074be57fb2f859) | The fuzzer's own ext4: a mkfs and a checker that share no code with `extfmt`, with a replay and an orphan release of their own |
| [8cff912](https://github.com/irqlevel/nos/commit/8cff912e27f9c55e082bcba2ebd4f4bb092fce57) | The driver, read-only: every checksum on every read, a journal nobody replayed read through |
| [6993c6e](https://github.com/irqlevel/nos/commit/6993c6e6287e5b79e13a07a4c2ceae489ce496ca) | Names written through the journal: one transaction a call, the journal replayed at a mount, aborted at the first failed write |
| [5ef7e7c](https://github.com/irqlevel/nos/commit/5ef7e7ceab94b3afcc3df27d2e84a246f6640703), [db16c2a](https://github.com/irqlevel/nos/commit/db16c2af722e2d8fb502711fa38e55022dc32f84) | A file's data: ordered, unwritten extents made written, blocks sent in batches |
| [bf9ed7e](https://github.com/irqlevel/nos/commit/bf9ed7ee8dba2d4e2ff7add1efd1669b808b2cb0) | Giving back at scale through the orphan list, and the last free blocks kept for a punch |
| [d87ab13](https://github.com/irqlevel/nos/commit/d87ab1346138adfc012f5fbc8319b63809f8e4db) | New installs are ext4 |
| [cd0758b](https://github.com/irqlevel/nos/commit/cd0758bc854cbee6631c48b93a709545f7c229d0) | A filesystem of any size mounted for writing: what the EX44's root needed |
| [66a6506](https://github.com/irqlevel/nos/commit/66a6506f521b734d161015e56790d40dc7c58a17) | What a review found, and the fuzzers' heap held to the kernel's |

## Three readings of one format

A filesystem driver and its test usually share a parser. Then they agree with each other whether the parser is right or not. ext4 in nos is three separate readings of the format, and each is checked against the others.

**`extfmt`** ([src/crates/extfmt](https://github.com/irqlevel/nos/tree/master/src/crates/extfmt)) is the format and nothing else: the superblock, group descriptors, inodes, extent trees, block maps, directory blocks and attribute blocks, every checksum `metadata_csum` and `gdt_csum` put on them, and the JBD2 journal. It has no kernel in it and no `unsafe` (`#![forbid(unsafe_code)]`), 2,492 lines. It also decides what nos does with a filesystem: refuse it, mount it read-only, or write it. The kernel's driver reads through it, and so does hvboot, the program that boots a Linux guest from the guest's own `/boot`.

`extfmt-test.py` (in CI) holds it to e2fsprogs. mke2fs makes filesystems of several shapes — the default ext4, 1 KiB blocks, 128-byte inodes, `gdt_csum`, ext3, ext2 — with a two-level extent tree, indexed directories and attributes in a block of their own. Every checksum of every metadata block must be the one `extfmt` computes. Then bytes that the checksums cover are changed one at a time, and `extfmt` and `e2fsck -fn` must both notice. Then debugfs writes journals and leaves them unreplayed, and `extfmt`'s replay plan must write the same image as `e2fsck -E journal_only`, byte for byte, including with the log wrapped round the ring.

**The driver** ([src/crates/fs/src/ext4](https://github.com/irqlevel/nos/tree/master/src/crates/fs/src/ext4)) is the kernel's side: 5,139 lines in 14 files, also with no `unsafe`. It mounts, reads through `extfmt`, and writes through its journal.

**The judge** is the fuzzer's own ext4: a mkfs, a checker, a JBD2 replay and an orphan release, written separately in [fuzz/fs/src/image/ext4](https://github.com/irqlevel/nos/tree/master/fuzz/fs/src/image/ext4). `fs-fuzz.py` checks that no judge names `extfmt` before it runs anything. The judge is held to e2fsprogs in turn: `fs-fuzz.py --e2fsck` has e2fsck check the judge's images and its verdicts on damaged ones, and compares the judge's replay with e2fsck's, byte for byte.

## Reading first

The first version of the driver only read. It reads the whole family, ext2 and ext3 included, though a plain ext2 still goes to ext2's own driver unless `mount ext4` asks for this one. What it will not touch is refused outright: inline data, bigalloc, encryption, casefold, verity, an external journal and a few more. What it can read but not write is mounted read-only, with the reason said: no journal, no extents, no `metadata_csum`, `orphan_file`, quotas.

Every checksum is checked on every read: the superblock and the descriptors at the mount, an inode, an extent block or a directory block when it is read. What fails is said and not used. A descriptor that fails refuses the mount, and so does a superblock, with one exception: a superblock torn by a power cut while the journal still holds a copy of it, which is used to find the journal and nothing more. A block that fails fails its read, or leaves an entry out of its directory, with a line in the log saying which.

A journal holding transactions nobody replayed, after a crash under Linux or under nos, is not replayed by a read-only mount: it writes nothing at all. `extfmt` works out what a replay would write, and every read of those blocks is served from the newest copy in the journal. `mounts` says `journal-not-replayed=<blocks>` while that is so.

## Writing: one call, one transaction

Every call that changes metadata is one transaction. What it changes is kept apart as copies of whole blocks until the journal holds all of it, and a call that fails before then leaves nothing changed: not on the disk, not in the descriptors in memory, not in the tree the VFS sees. A block can only be changed as a kind — a bitmap, the descriptors, an inode table block, a directory leaf or index, an extent block, an attribute block — and the checksums its kind carries are put on it when the transaction is sealed. A transaction is bounded: a quarter of the journal, and never more than 512 blocks.

A commit writes the log's start into the journal's superblock if the log was empty, then revoke blocks, descriptors and the copies. It flushes, and then writes the commit block forced to the medium. Only after that are the new blocks what reads see. They stay pinned in memory until a checkpoint writes them home: when the log is half full, when more than 1024 blocks are pinned, when a commit would not fit, and at the unmount. A clean unmount leaves the log empty and the superblock's free counts the groups'.

On the EX44 it looks like this:

```text
$ mounts
ext4 on /  nvme05 label=nos journal=1447/32767 seq=6054  rw
procfs on /proc  ro
```

1,447 of the log's 32,767 blocks are in use, and the next transaction will be number 6054.

A write that fails in a commit or a checkpoint, or a flush that fails, aborts the journal. Nothing more is written: the device itself refuses every write and flush from then on, the call fails, reads go on, and the filesystem is left to the next mount's replay, or to e2fsck's. There is one exception. A block of a file's data that does not write fails that call alone, and its transaction is undone, so nothing maps what failed. One file's bad sector, a guest disk's say, should not take every other file down with it. Linux's ordered mode does the same by default.

A file's data follows Linux's `data=ordered`: a write is cut into transactions of at most 256 blocks, and each one's data is on the device before its commit, so a block the commit maps into a file never shows what it held before. A block in a hole is allocated and written whole, zeros around what goes into it. A block in an unwritten extent is written whole and made written, the extent split around it. The blocks go to the device in batches of up to 64, in flight together on NVMe and virtio-blk.

Giving back is where a bounded transaction meets an unbounded file. A guest's disk image can be millions of blocks, and freeing them is far more than one transaction may hold. So a remove that cannot finish in one transaction takes the name away and puts the inode on the superblock's orphan list in its first transaction. The transactions after it give back a bounded share each — up to 32 extents, what they free kept as runs, so that a file of millions of blocks is a handful of them — and the last takes it off the list. A crash in between leaves an orphan, which the next mount for writing releases after its replay, as Linux and e2fsck release another kernel's. A directory that once held millions of names is given back the same way.

A punch, which is what a guest's discard becomes, may take the last eight free blocks that nothing else is given. A punch inside an extent splits it, and on a full disk the leaf that split needs must not be what stops a guest from giving its blocks back.

## Holding it to account

`fs-fuzz.py` builds the kernel's own `block` and `fs` crates into a host program, over disks whose media are the fuzzer's: each has a volatile write cache, a power switch and requests that can be made to fail. Four targets are ext4's:

- **`ext4`** mounts the judge's images read-only — uninitialised groups full of noise, deep extent trees, unwritten extents over noise, indexed directories, journals whose newest copies stand over stale homes, orphans on the list — and every answer must be the model's, with nothing written.
- **`ext4bad`** damages those images. Each is refused or walked, and never panics, writes or leaks.
- **`ext4rw`** mounts for writing: names and data, open files, disks' images, files of two names, a disk all but full, every answer the model's. It cuts the power at chosen requests, and before every single request of a call. Each picture of the disk is then replayed three ways — by the judge, by the kernel mounted read-only, and by the kernel mounted for writing — and all three must agree, be clean, and hold a tree that the call in flight could have left. For a write, every sector must hold what it held before or what it was given, and nothing of another file's. It also makes requests fail: a file's data block that does not write must fail its call alone, and any other failed write must leave nothing written in that mount after it.
- **`ext4sys`**, new since the review, makes a file whose extent names the filesystem's own blocks. More on it below.

The gate runs 68,601 inputs across every target. `fs-fuzz.py --e2fsck`, also in CI, then holds what `ext4rw` leaves, clean images and power-cut pictures alike, to e2fsck.

The last word is Linux's. `ext4-test.py` boots nos under QEMU, in CI, on images that mke2fs and debugfs made:

- It reads them from nos's shell, every file's CRC-32 the host's.
- It changes names and data from the shell, and e2fsck finds the image clean and debugfs's extents what was written.
- It kills a boot in the middle of its changes, and the image must replay clean under e2fsck and under Linux, every file's MD5 what the model says.
- It reads through a journal that debugfs left unreplayed, with the image's bytes unchanged after.
- It releases orphans that debugfs left, at a mount for writing.
- It mounts a filesystem of a tebibyte with a 1 GiB journal for writing. Why that one is there is the next part.

## The EX44: 520 KiB

On 29 October the EX44's 16 GiB root was made ext4. From Ubuntu, on the same partition, mke2fs made it with nos-install's features, the same UUID and label, and a 128 MiB journal. The tree was copied back from a backup of the ext2 with `cp -a --sparse=always`, and every file's SHA-256 compared with the backup's.

Then nos booted, and did not mount it for writing:

```text
ext4: no memory for the system zone
```

The kernel fell back to its fallback layout: no `/etc/rc`, so no sshd, on a machine whose only console is the network. It took a reset from Hetzner's robot to get back into Ubuntu.

The system zone is the set of blocks no file may own: every group's metadata and the journal. The driver worked it out as four runs a group and one a journal block before joining them, which for a 128 MiB journal of 32,768 blocks came to 520 KiB in one allocation. The kernel's heap gives at most 128 pages, 512 KiB, in one allocation, and past that `kernel_alloc` returns null. Two more allocations grew the same way and would have failed on bigger filesystems: the journal's map, a block number for each of its blocks (2 MiB for a 1 GiB journal), and the descriptors, kept as one allocation (512 KiB, all that one allocation gives, for a tebibyte's 8192 groups). No gate had seen any of them. The fuzzers run on the host, where the allocator gives gigabytes, and every image a test made had a journal of a few MiB.

A replica reproduced it under QEMU: the same mke2fs line, a tree written by Linux, on NVMe. [cd0758b](https://github.com/irqlevel/nos/commit/cd0758bc854cbee6631c48b93a709545f7c229d0) keeps what a mount holds of the geometry in pieces. The journal's map is now the runs its inode maps it in, one run or a few however long the journal is. The system zone is joined as it is worked out, and the descriptors are kept a block each. That is why `ext4-test.py` mounts a tebibyte filesystem for writing: 8192 groups, a 1 GiB journal, and every one of the three allocations past the heap's limit as they were. On the kernel before the fix it says `mount failed`.

After that the EX44 ran its two VMs from that root: the management panel and an Ubuntu. Inside the Ubuntu VM, 1 GiB was written and synced, the caches dropped, and its SHA-256 read back the same. `fstrim` in the guest gave back 4.1 GiB through punches, and after nos's clean reboot e2fsck found the root and both VMs' own filesystems clean. On 30 October, before the next kernel went in, the root had carried both VMs for eight hours. e2fsck found nothing wrong with it:

```text
nos: 86/1048576 files (4.7% non-contiguous), 1000111/4194304 blocks
```

It did say that two extent trees could be narrower, but that is an optimisation it offers, not damage.

## A review, and a heap that never said no

After the merge we reviewed the driver, `extfmt` and the way the VFS hands a disk's image to a guest. The worst finding was the EX44's story again. `extfmt`'s replay planner kept a list of every transaction and every copy in the log. For a log of 8193 transactions that list asked the heap for 917,504 bytes at once, and every mount of such a journal, for writing or read-only, would have panicked. nos's own journal can get there: hours of small commits on a root of 32 GiB or more, whose mke2fs journal is 256 MiB, then a crash. The planner now reads the log in three passes, as Linux's recovery does — the scan and the homes it names, the revokes, the copies — and keeps maps that grow a node at a time.

The rest, in brief:

- **A file could name the filesystem's own blocks.** The system zone held only what the driver allocated, and a file's own extents were trusted as long as they lay inside the filesystem. On an image whose file named an inode table, every checksum right, a write to the file went over the table, and a guest's disk on such a file would have done it from the guest. Every run of a file's data is now held to `Meta::file_may_own`, Linux's `ext4_inode_block_valid`. Every extent node read from the disk is checked before a change as Linux checks it: extents of at least a block, in order, apart, inside the 2^32 blocks a file has.
- **The orphan list** assumed that an inode goes on once and leaves from the head. A call whose giving back stops part way, on a read that fails say, leaves its inode on the list, and a later call may cut the same file again or remove it. Put on twice, the list looped; freed while on it, the list named an inode not in use. An inode is now on the list at most once and leaves from wherever it is. Orphans with no extents — a fifo, a socket, a device node, a fast symlink that Linux left there still open — are released instead of making the mount read-only.
- **An abort was the journal's alone.** The device went on taking writes, the next transaction of a call of several among them. Now the device refuses them.
- **Some walks kept a list of every node to visit,** which passes 512 KiB at 65,537 of them. They now walk the tree by its own links.

The 917,504 bytes and the 65,537 nodes have the same cause as the 520 KiB: the fuzzers' allocator never said no. So now it does. [fuzz/common/machine/heap.rs](https://github.com/irqlevel/nos/blob/master/fuzz/common/machine/heap.rs) refuses every allocation of the kernel's code past 128 pages, exactly as `kernel_alloc` would, and prints the backtrace of what asked. The fuzzer's own big allocations — its models, images and judges — are marked as its own and let through. Every fix has a control that fails on the code before it:

- `extfmt-test.py` now plans a log of 9000 transactions and allows no allocation past 512 KiB; the old planner asks for 917,504 bytes.
- The new `ext4sys` target mounts for writing over a file whose extent names an inode table, the descriptors or the journal's log. It tries a write, a read, a cut, a remove and a disk's image of that file, and every one must be refused, with those blocks unchanged at the unmount. On the old driver, the very first input's write goes into the inode table.
- The judge's mkfs now puts orphans with no extents on the list, and on the old driver `ext4rw` finds its mount read-only.

## What it does not do yet

Each of these waits for a measurement or a need to ask for it. The whole list is in [Filesystems](/docs/filesystems).

- **Group commit.** Every call that changes metadata commits before it returns: a flush, the log, another flush and the commit block. What a small call costs is the device's flushes. Calls that come together could share one commit.
- **Preallocation.** There is no `fallocate`. A disk image that `hv create` makes is a sparse file whose blocks are allocated as the guest first writes them.
- **orphan_file.** Debian 13's mke2fs makes it by default, and nos mounts such a filesystem read-only. The installs leave it out.
- **Indexed directories, written.** nos reads an htree directory as the linear one it also is, and drops its index the first time it adds a name.
- **Discard.** What a remove, a cut or a punch gives back is not discarded on the device.
- **CRC32C by the CPU.** Every checksum is computed a byte at a time from a table. SSE4.2's `crc32` and arm64's CRC32C instructions are not used.
