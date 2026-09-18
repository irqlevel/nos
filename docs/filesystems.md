# Filesystems

How files work in `nos`: what gets mounted at boot and how the root is
chosen, the file API kernel code uses, what the ext2 driver does and does
not do, and how a root filesystem image is made and checked. The shell side
is in [Shell commands](shell-commands.md#filesystem).

## What is mounted at boot

`MountRootFs()` (`fs/rootfs.cpp`) runs late in the boot, after the disks
that appear during `rust_init` (NVMe) have been probed for partitions, and
does what `root=` on the command line asks:

| Command line | What ends up on `/` |
|---|---|
| nothing | nothing; the VFS starts empty and filesystems are mounted from the shell |
| `root=auto` | the ext2 filesystem whose volume label is `nos`, read-write. Without one: a ramfs on `/`, the first ext2 found read-only on `/boot`, the first nanofs found read-write on `/data` (the layout the ISO had before there was a root on disk) |
| `root=vda1`, `root=nvme01` | that block device, ext2 if it carries one, else nanofs |
| `root=LABEL=nos` | the ext2 filesystem with that label |
| `root=UUID=…` | the ext2 filesystem with that UUID, as `blkid` prints it |

`ro` mounts the root read-only. A procfs goes on `/proc` in every case (the
directory is created on the root if it is missing). If the named root is not
found the fallback layout is mounted and the boot continues, so a wrong
`root=` costs a shell without persistence rather than a box without a shell.

Both shipped configs say `root=auto`: the ISO (`build/grub.cfg`) so that any
disk labelled `nos` attached to QEMU becomes the root, and the disk image
(`build/grub-disk.cfg`), whose one partition carries that label. The label is
what makes the same command line work whatever bus the disk is on — virtio
on QEMU and the clouds, NVMe on the Hetzner boxes.

`fstest=on` runs the filesystem self-test (below) on `/` right after the
mount; the smoke tests boot with it and assert `fstest: passed`.

## The file API

Kernel code reaches files through `Vfs::GetInstance()` (`fs/vfs.h`). Paths
are absolute; the longest mount prefix wins; `.` and `..` resolve in the
tree, and `..` stops at a mount root.

```cpp
File* f = vfs.Open("/lib/modules/foo.ko", Vfs::OpenRead);
ulong size = vfs.GetSize(f);
ulong got;
vfs.Read(f, buf, size, got);    // got == 0 at end of file
vfs.Close(f);
```

- `Open(path, flags)` with `OpenRead`, `OpenWrite`, `OpenCreate`,
  `OpenTruncate`, `OpenAppend` (which puts every write at the end and implies
  `OpenWrite`); `Close`, `Read`, `Write`, `Seek`, `Tell`, `GetSize`. A write
  past the end extends the file; the gap reads as zeros.
- `Stat` (type, size, inode), `ReadDir(path, index, entry)` to walk a
  directory by index, `Rename` (a move within one filesystem; a directory
  cannot move under itself), `Truncate` (either way), `Sync`, `Remove` (a
  directory goes with everything under it), `CreateDir`, `CreateFile`.
- `ReadFile(path, printer)` and `WriteFile(path, data, len)` for the
  whole-file cases the shell has.

An open file pins its vnode: `Remove` and `Rename` refuse a file (or a
directory containing one) that is open, and `Unmount` refuses a filesystem
with open files. `UnmountAll` at shutdown does not refuse — it is shutdown.

A mounted filesystem claims its block device (`BlockDeviceTable::Claim`) until
it is unmounted, and so do the disk log and a module writing to a device
direct (`blkload`'s write tests) -- and the shell's `format` and `diskwrite`,
for as long as they write. A claim is refused while another overlaps
it — the same device, the disk a partition is on, or a partition of that
disk — so a mount fails (`Vfs::Mount: vdb is in use by a mounted filesystem`
in dmesg) on the disk under a mounted partition, on the disk log's area, or
on a device a write test is running on; and those, in turn, keep off a
mounted one. Reads need no claim.

One mutex serialises every VFS call, so a multi-megabyte read holds up an
`ls` from the UDP shell for its duration. Filesystems see one call at a time
and do no locking of their own; that is the contract in `fs/filesystem.h`.

## ext2

`src/rust/fs/src/ext2.rs` reads and writes ext2 rev 1 with 1, 2 or 4 KiB blocks:
direct, indirect and doubly-indirect blocks (a file can reach 4 GiB at 4 KiB
blocks; the triple-indirect block is not implemented and a file needing it is
refused), sparse files, directories of any size. Directories are read from
disk the first time a path walk enters them, not at mount, so the memory
cost is what is used, not what is on the disk.

What it refuses, and why:

- any `incompat` feature other than `filetype` — ext3 with journal recovery
  pending, ext4 extents, `64bit`, `meta_bg`: same magic, different format.
- an image without `filetype` (rev 0): directory entries do not say what
  they name.
- any `ro_compat` feature other than `sparse_super` and `large_file` is
  mounted **read-only**: `gdt_csum` above all, whose group checksums a write
  would silently break.
- symlinks and device nodes are left out of the tree; a file over 4 GiB is
  skipped with a message.

Writing keeps the on-disk structures consistent for e2fsck and for Linux,
which can mount the same partition (that is how modules get onto a Hetzner
box: copy them in under Ubuntu). The rules the driver follows, in place of
the journal ext2 does not have:

- **Order.** Allocation bitmaps go down first (FUA), then the data and
  indirect blocks (plain writes) followed by a device flush, then the inode
  (FUA), then the free counts in the group descriptors and superblock. A
  crash at any point leaves at worst blocks marked used that nothing
  references — e2fsck reclaims them — and never a block both referenced and
  free.
- **Frees run the other way.** A truncate cuts the tail off the block tree,
  commits the inode without it, and only then clears the bits, in batches of
  512 blocks so a large file does not hold a huge list in memory. A remove
  takes the name out of its directory first, so a crash leaves an orphan for
  `lost+found`, never a name leading nowhere.
- **Renames add the new name before dropping the old one**, so a crash
  leaves the file reachable twice, which e2fsck reduces to once.
- **The `valid` state bit is cleared at a read-write mount and set again
  at a clean unmount**, as Linux does. A boot after a crash prints
  `was not cleanly unmounted` and mounts anyway; run `e2fsck` under Linux
  when convenient. The unmount happens on `poweroff` and `reboot`, before the
  soft IRQs stop (a block request completes through one).
- **Directories are modified linearly** and the htree `index` flag is
  dropped from any directory the driver touches; images for `nos` are made
  with `-O ^dir_index` so Linux never builds an index in the first place.
- Only the primary superblock and group descriptors are updated; the
  backups drift, which e2fsck tolerates (`-b` uses them for recovery only).

Every read and write is a synchronous request to the block device — there
is no cache. A 3 MiB module read is about 770 block reads, a few tens of
milliseconds on NVMe. The indirect and doubly-indirect blocks last used are
kept in memory so a sequential pass does not re-read them per data block.

## nanofs, ramfs, procfs

nanofs (`fs/nanofs.cpp`) is the kernel's own small checksummed filesystem:
1024 inodes, 64 MiB of data, files up to 1 MiB, every write copy-on-write
with the inode committed last. It predates ext2 write support and remains
for what it is good at — a small, self-verifying store (`format nanofs`,
`mount nanofs`, `scripts/mkfs_nanofs.py`) — and takes the whole file API,
including writes at an offset and truncates, by rewriting the file.

ramfs (`src/rust/fs/src/ramfs.rs`) is the in-memory filesystem the fallback
layout puts on `/`, and what `TestVfs` exercises at every boot: a file is
one buffer that doubles as it grows. procfs (`procfs.rs`) is a ramfs whose
`/proc/interrupts` is regenerated on each lookup -- the VFS looks a file up
before it reads it or reports its size, so refreshing there keeps the two
consistent; `/proc/version` and `/proc/cmdline` are written once at mount.

## Making a root filesystem

```sh
scripts/mkrootfs.sh root.img 256 rootdir        # 256 MiB, populated from rootdir/
scripts/mkrootfs.sh root.img 64 "" 1024         # empty, 1 KiB blocks (what the smoke tests use)
```

This is `mke2fs -L nos -O ^dir_index -d rootdir`: the label `root=auto`
looks for, no directory index, populated from a directory without loop
devices or root privileges (inside the `nos-builder` Docker image on macOS,
where the copy goes through a directory in the container because a macOS
bind mount serves no extended attributes). Attach it to QEMU as a raw disk
and the ISO boot mounts it:

```sh
qemu-system-x86_64 ... -cdrom nos.iso -drive file=root.img,format=raw,if=virtio
```

`scripts/build-disk.sh` makes `nos.qcow2` the same way — one MBR partition,
ext2 labelled `nos`, `/boot/kernel64.elf` and `/boot/grub` inside it, GRUB's
boot code in the boot sector, `ROOTFS=dir` to add content (modules under
`lib/modules`) — and runs `e2fsck` over the result. Nothing in it needs
`--privileged`.

On a machine that dual-boots Ubuntu (the Hetzner boxes) the root is an ext2
partition made under Ubuntu, `mkfs.ext2 -L nos -O ^dir_index /dev/nvme0n1pN`,
filled by mounting it there, and named on the `nos` command line by label or
UUID. After a `nos` session, `e2fsck -f` under Ubuntu is the check that the
driver left it as it should.

## Checking it

`fstest [dir] [size]` from the shell, or `fstest=on` at boot, runs
`FsSelfTest` (`fs/fstest.cpp`) in a directory it makes and removes: write
and read back, append, write at an offset, truncate both ways with the gap
checked for zeros, rename, move into a subdirectory, `readdir`, the
operations that must fail, a file of `size` bytes (300 KiB by default)
written in 64 KiB pieces and read back in 12345-byte ones, patched in the
middle, cut short, grown back, and a recursive remove. `fstest / 5M` at
4 KiB blocks reaches the doubly-indirect blocks; the smoke tests get there at
300 KiB by using 1 KiB blocks. `crc32 <path>` checks a file against its
copy on the host.

`TestVfs` in `kernel/test.cpp` runs the same test on a ramfs at every boot,
before any disk is up.
