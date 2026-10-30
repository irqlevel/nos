# Filesystems

How files work in `nos`: what gets mounted at boot and how the root is
chosen, the file API kernel code uses, what the ext2 driver does and does
not do, and how a root filesystem image is made and checked. The shell side
is in [Shell commands](shell-commands.md).

## What is mounted at boot

`rust_mount_root_fs()` (`src/crates/fs/src/rootfs.rs`) runs late in the boot, after the disks
that appear during `rust_init` (NVMe) have been probed for partitions, and
does what `root=` on the command line asks:

| Command line | What ends up on `/` |
|---|---|
| nothing | nothing; the VFS starts empty and filesystems are mounted from the shell |
| `root=auto` | the ext4 -- or ext2 -- filesystem whose volume label is `nos`, read-write. Without one: a ramfs on `/`, the first of the ext family found read-only on `/boot`, the first nanofs found read-write on `/data` (the layout the ISO had before there was a root on disk) |
| `root=vda1`, `root=nvme01` | that block device, ext4 or ext2 if it carries one, else nanofs |
| `root=LABEL=nos` | the ext4 or ext2 filesystem with that label |
| `root=UUID=…` | the ext4 or ext2 filesystem with that UUID, as `blkid` prints it |

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

Rust reaches files through `crate::vfs_instance()` (`src/crates/fs/src/vfs.rs`),
and C++ through the `kernel_vfs_*` calls it exports -- there is no C++ view
of the layer, so a file that needs one declares the calls it makes where it
makes them (`kernel/module.cpp` reading a `.ko`, `kernel/cmd.cpp` for
`crc32`, `sha256`, `wget` and `/etc/rc`). Paths are absolute; the longest
mount prefix wins; `.` and `..` resolve in the tree, and `..` stops at a
mount root.

```rust
let file = Open::new(vfs, b"/lib/modules/foo.ko", OPEN_READ)?;   // closed when it goes
let got = file.read(&mut buf)?;                                  // 0 at end of file
```

An open file is a `Handle`: a slot in the VFS's table of open files and the
generation of what is in it. That is what crosses the C ABI as well -- the
`RustFile*` the C++ shell holds is that word and not an address -- so a
handle from anywhere is looked up rather than followed, and one that was
closed, or never opened, or whose filesystem has been unmounted at shutdown,
reads as no file instead of as whatever memory is there now.

- `open(path, flags)` with `OPEN_READ`, `OPEN_WRITE`, `OPEN_CREATE`,
  `OPEN_TRUNCATE`, `OPEN_APPEND` (which puts every write at the end and
  implies `OPEN_WRITE`); `close`, `read`, `write`, `seek`, `tell`, `size`. A
  write past the end extends the file; the gap reads as zeros.
- `stat` (type, size, inode), `read_dir(path, index)` to walk a
  directory by index, `rename` (a move within one filesystem; a directory
  cannot move under itself), `truncate` (either way), `sync`, `remove` (a
  directory goes with everything under it), `create(path, directory)`.
- `files.rs` for the whole-file cases: `list_dir` and `read_file` for what
  the shell prints, `replace_file`/`locate` for a configuration file that a
  full disk or a crash midway must not leave empty, and the `kernel_file_*`
  ABI over them that a module -- and the shell's `/etc/rc` -- reads and
  writes by.

An open file pins its vnode: `remove` and `rename` refuse a file (or a
directory containing one) that is open, and `unmount` refuses a filesystem
with open files. `unmount_all` at shutdown does not refuse — it is shutdown —
and the handles it abandons stop naming anything.

A mounted filesystem claims its block device (`kernel_blockdev_claim_as`) until
it is unmounted, and so do the disk log and a module writing to a device
direct (`blkload`'s write tests) -- and the shell's `format` and `diskwrite`,
for as long as they write. A claim is refused while another overlaps
it — a stretch of the same disk: the same device, the disk a partition is on,
a partition of that disk, or another partition of it that the disk's table
has overlap (the table is whoever wrote the disk's to choose) — so a mount fails (`vfs: the device is in use by a mounted filesystem`
in dmesg) on the disk under a mounted partition, on the disk log's area, or
on a device a write test is running on; and those, in turn, keep off a
mounted one. Reads need no claim.

Each mount has a mutex of its own, held across every call on its
filesystem down to the device and back, so a filesystem sees one call at a
time -- and calls on two mounts go on side by side: a multi-megabyte read
off one disk holds up an `ls` of the same filesystem for its duration, and
nothing on any other. Everything a mount's lock guards is inside it: the
filesystem, the files open on it and what is counted of them. The mount
points are a table of their own, under a mutex held only to look a path's
mount up or to change the table, never across a call into a filesystem;
and a mount, an unmount and the halt's unmount of everything go one at a
time, each for the whole of it -- a journal replayed or written home
included -- while calls on the mounts there are go on. The locks are taken
in that order -- the mounting, a mount's, the table -- never two mounts' at
once, and an open file's handle names its mount, so a read or a write
through one takes that mount's lock and no other (`fs/src/vfs.rs`).

The mutex is the kernel's: a task that finds it held gives its CPU to any
other task that can run there, and spins only when none can -- which a
long call on a busy mount now costs the callers of that mount alone.
`fstest` holds every boot to the promise: it mounts a filesystem of its
own whose lookup it holds, and while that call waits a file must be
written, read back and removed on the filesystem under test, and a second
call on the held mount must not get in.

### A disk's image

A guest's disk that is a file of nos's (`hv start ... disk=/vms/a/disk.img`)
is read and written for as long as the guest runs, a request at a time,
and through its mount's mutex it would wait behind every other guest's disk
on that filesystem and behind nos's own files there. So it is opened as an
image (`fs::image::Image`, `kcore::fs::Image` for a module): the file is
**held** (`OPEN_HOLD`), and an image takes only what it must under the
lock -- where the blocks of the
range it is asked for are on the device (`Vfs::map`, the filesystem's
`map_blocks`: ext2 walks the file's pointers, through its caches, and
allocates nothing) -- and reads and writes those blocks itself with the lock
let go, through a buffer of its own, in batches the driver has in flight
together (`Disk::read_pieces`, `write_pieces`).

The hold is what makes that safe: while it lasts nothing else may write the
file -- open it for writing, appending or truncating, cut it by its path,
`write_file` it, grow it through a handle -- and, open, it cannot be
renamed or removed, nor its filesystem unmounted; so the blocks a map named
stay the file's, and nobody else writes them, until the image lets go. A
file someone else has open for writing cannot be held, and one held for
writing cannot be held again; any number may hold one to read while nobody
writes it. Only its image lets go of it: the VFS handle under it is closed
by the image's going, and refused to the file ABI's close, which a word
guessed or kept too long could otherwise reach. What only the filesystem
can do is left to it: a batch with a hole in it -- blocks to allocate and
point at, in order -- is written through the VFS, and the next flush is then
the filesystem's sync (its metadata, then the device); otherwise a flush is
the device's alone. A part of a block -- a guest's sector in a block of 4
KiB -- is read, changed and written back whole, which an image may do
because its calls come one at a time and nobody else writes the file. A
filesystem that keeps no blocks of its own on a device (`data_block_size`
0: ramfs, and nanofs for now) has its images read and written through the
VFS whole.

A guest's discard gives whole blocks back (`Image::discard`, `Vfs::punch`):
the filesystem's `punch` -- ext2's frees the data blocks of the range in
truncate's batches, pointers cleared and on the disk and the inode
committed before the bitmap frees them, and keeps the indirect blocks,
which map what is past the range too -- leaves holes that read as zeros,
the file's size as it was; what the range covers of a block only in part
is left as it was. A write of zeros gives back the whole blocks it covers
with its unmap flag and writes zeros over the rest. That the next file may
have a block the moment the punch is done is safe because an image's calls
come one at a time: none of its reads is under way off the lock on a block
it mapped before. A filesystem with no blocks of its own gives nothing
back, and its discard does nothing.

`fstest` holds one at every smoke boot -- holes read as zeros, a write into
one, into part of a block, across both, a write of zeros given back and one
written, a discard, everything else refused while it is held, the file as
the image left it once it lets go -- and `fs-fuzz`'s targets hold, read,
write, discard, zero, sync and let go of images among every other call,
each held to the model's rules, ext2's images judged by its e2fsck after a
clean unmount and after every power cut.

## What a filesystem is

A filesystem is a type that implements `vfs::FileSystem`, mounted as a
`Box<dyn FileSystem>` the mount owns; unmounting drops it. The trait is the
whole seam: `mount`/`unmount`, `root`, `lookup`, `load_dir`, `create_file`,
`create_dir`, `read`, `write`, `truncate`, `rename`, `remove`, `sync`, and
`name`/`info`/`device` for what `mounts` prints and the device it claims.
Every call arrives with its mount's lock held, so a filesystem sees one
call at a time and does no locking of its own; calls on two filesystems run
side by side, so what two mounts would share needs a lock of its own --
none of the filesystems here shares anything.

What a filesystem holds is a `vnode::Tree`: an arena of nodes -- a name, a
kind, a size, an inode number, the children of a directory in order -- named
by `NodeId`s, a slot and the generation of what is in it. Parents, children
and open files all refer to a node by that number, never by an address, so
the tree is ordinary owned data: a filesystem hands the VFS an id and the
VFS asks the filesystem's tree what it is. An id of a node that has been
freed finds nothing. The slots are taken from the allocator in chunks and
fallibly: an image says how many nodes there will be, and an image is not
to be trusted with that.

There is no `unsafe` in the VFS, the tree or any of the four filesystems.
ext2's on-disk structures are read and written whole through `kcore::pod`,
which is where the two pointer casts that takes are; what is left in the
crate is the C ABI at its edge, where a path or a buffer arrives as a
pointer and a length.

## ext2

`src/crates/fs/src/ext2.rs` reads and writes ext2 rev 1 with 1, 2 or 4 KiB blocks:
direct, single-, double- and triple-indirect blocks, sparse files, files past
4 GiB (`large_file`, a regular file's size in both of its inode's words),
directories of any size. A file is as big as Linux's ext2 lets one be
(`ext2_max_size`): all its block tree maps -- 16 GiB at 1 KiB blocks, 256 GiB
at 2 KiB -- unless a file that size, written whole, would count more blocks
than its 32-bit `i_blocks` holds, as at 4 KiB blocks, where it ends just short
of 2 TiB. Directories are read from
disk the first time a path walk enters them, not at mount, so the memory
cost is what is used, not what is on the disk -- all of a directory or none
of it: one whose read fails part way keeps nothing it read, and is read
again whole at the next look.

What it refuses, and why:

- a geometry that does not hold together: more blocks than the device has, an
  inode count that is not its groups' inodes, a group whose bitmaps or inode
  table lie outside the filesystem. Every number the driver works out from
  the superblock is then one that cannot overflow or lead off the device.
- any `incompat` feature other than `filetype` — ext3 with journal recovery
  pending, ext4 extents, `64bit`, `meta_bg`: same magic, different format.
- an image without `filetype` (rev 0): directory entries do not say what
  they name.
- any `ro_compat` feature other than `sparse_super` and `large_file` is
  mounted **read-only**: `gdt_csum` above all, whose group checksums a write
  would silently break.
- symlinks and device nodes are left out of the tree; a file whose size is
  past what a file can be is skipped with a message.
- a write or a truncate past that size is refused whole, before anything is
  allocated. A write that fails part way (the disk full, a block that would
  not read) gives back what it allocated past the end of the file, and
  zeroes what it wrote past the end into the last block, so that the file
  growing over it later shows zeros; the file keeps the size it had, and
  what it wrote inside that size stays.
- a block the filesystem keeps for itself -- the boot block, a copy of the
  superblock and the descriptors, a bitmap, the inode table -- that a
  damaged bitmap has free is never handed out: it is marked in use, said
  so, and the next free one taken, as Linux's ext2 does a block "in system
  zone". Written over, the filesystem would be gone.
- a name with a NUL in it: the VFS refuses one for every filesystem.

Writing keeps the on-disk structures consistent for e2fsck and for Linux,
which can mount the same partition (that is how modules get onto a Hetzner
box: copy them in under Ubuntu). The rules the driver follows, in place of
the journal ext2 does not have:

- **Order.** Allocation bitmaps go down first, then the data and indirect
  blocks (plain writes) followed by a device flush, then the inode, then
  the free counts in the group descriptors and superblock. A crash at any
  point leaves at worst blocks marked used that nothing references —
  e2fsck reclaims them — and never a block both referenced and free. A
  create is the same: the bits, then the inode, then the name; and one
  undone after an error writes its inode deleted before freeing the bits,
  or, when that write fails, leaves them taken.
- **A file's data pays for what it changes.** A write that allocated
  nothing — a disk image having its blocks written over, a guest's — moved
  no pointer and changed no bit, so nothing is ordered against it: the
  blocks go down plainly, and so does the inode, and only when the size or
  the second of the mtime changed; `sync` puts them on the medium, and a
  guest's flush is a `sync`. A write that allocated flushes the device
  once — the bitmap and the data it wrote then on the medium — and writes
  what points at them after that, plainly: each indirect block it filled in
  once, however many pointers of it the write set (they wait in its buffer
  for the flush; one written as it was set would have reached the disk
  before the bitmap), the inode, the counts. So what a write leaves in the
  device's cache reaches the medium at the next flush -- another write's
  that allocates, a `sync`, the unmount -- and a caller that needs it there
  sooner syncs, as the two-step replace, `grubenv` and a guest's flush do.
  Creates, renames and removes keep the FUA writes of their inodes and
  bitmaps.
- **Frees run the other way.** A truncate cuts the tail off the block tree,
  commits the inode without it, and only then clears the bits, in batches of
  512 blocks so a large file does not hold a huge list in memory. It walks
  down from the end of what the tree maps, not of the size -- whatever the
  tree has past the size goes too, as Linux's truncate has it -- and steps
  over a missing indirect block of any level whole, as the run of holes it
  is: a sparse file of terabytes costs what it has, not what it could have.
  An indirect block goes once the walk has come down to the first block it
  maps. A remove takes the name out of its directory first, so a crash
  leaves an orphan for `lost+found`, never a name leading nowhere; a
  directory goes deepest first, by a walk and not a recursion, as deep as
  the paths reach. A file of more than one name -- a hard link, which nos
  makes none of and Linux does -- loses one link to a remove, the others
  keeping it, and every one of its names shows the size a write or a cut
  gave it.
- **`large_file`** is set in the superblock, on the medium, before the
  inode of the first file of 2 GiB or more is -- what Linux does, and what
  e2fsck wants of a filesystem that has one.
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

There is no cache of data: a file's blocks go to and from the device as
the call asks for them, a batch at a time -- up to 64 blocks, a page of a
256 KiB buffer each (`Disk::write_pieces`, `read_pieces`), in flight
together on NVMe and virtio-blk, where they used to go one synchronous
request after another. A batch of a write stays under one pointer block --
the inode's, the indirect block's, one of those under the double and the
triple indirect block --
because moving on to the next puts the last one down after a flush, and the
blocks it points at have to be on the disk by then, not waiting in the
batch. A partial block is read in first, and a block the batch allocated is
written even when the batch stops short, so it never holds what the disk
had there before. Metadata -- inodes, bitmaps, directories, indirect
blocks -- still goes a block at a time. The inode table block and the
indirect block of each level last used are kept in memory, so a file read or
written a piece at a time does not read its inode back for every piece, and
a sequential pass does not re-read its indirect blocks per data block.

## ext4

`src/crates/fs/src/ext4/` reads ext4 -- and ext3, and an ext2 with a journal
-- through `src/crates/extfmt`, the on-disk format and its checksums as a
crate with no kernel in it, and writes it through its journal: names and
data, and what a remove, a cut or a punch gives back, however much -- in
transactions of a bounded share, through the orphan list, which a mount for
writing releases, its own crash's or another kernel's. It is what new roots
are: `nos-install`, `scripts/mkrootfs.sh` and `scripts/build-disk.sh` make
an ext4 of the features named one by one from none -- has_journal, ext_attr,
resize_inode, dir_index, filetype, extent, 64bit, flex_bg, sparse_super,
large_file, huge_file, dir_nlink, extra_isize, metadata_csum: e2fsprogs
1.46's ext4, which GRUB 2.06 reads -- so that what a newer mke2fs.conf adds,
orphan_file and metadata_csum_seed among them, is not there; and with its
inode tables and its journal written whole (`lazy_itable_init=0`,
`lazy_journal_init=0`), so that nothing an earlier use of the blocks left is
ever read as either. ext2's driver stays as it is, for the roots made
before ext4.

Which driver a filesystem gets is `extfs::driver_for`'s to say, for
`root=`, the fallback layout and `mount`: ext2's takes what it always
took -- revision 1, no incompatible feature but filetype -- but a
filesystem with a journal, which it would write past; ext4's takes the
rest of the family. `mount ext4 <disk> <path>` names the driver outright.

What is refused, mounted read-only or written is
`extfmt::features::classify`'s: refused are inline_data, bigalloc, meta_bg,
encryption, casefold, verity, ea_inode, largedir, mmp, an external journal
and a filesystem with no filetype; read-only, a filesystem without a
journal, without extents (ext3), without metadata_csum, with orphan_file,
quota or project quotas. A mount asked to write is read-only too -- the
reason said -- for a filesystem marked with errors or not marked valid, a
journal that recorded an error, is of the first version or holds a log the
superblock does not say needs replaying, a transaction whose commit fails
its checksum (e2fsck should look), and an orphan list that is not sound --
an inode on it no file may be, or not in use, or failing its checksum, or
mapped by blocks, or named twice; and refused for a journal whose blocks are
the filesystem's own metadata. A superblock that fails only its checksum,
on a filesystem with a journal, is the one a power cut tears as a
checkpoint or a replay writes it home -- with 512-byte sectors its 1024
bytes are two, half the old and half the new, which agree on where
everything is -- while the journal still holds its copy, the log's start
moving past one only once its home is on the medium: it is used to find the
journal and nothing more, and a replay must then write a sound one home, or
a read-only mount find one in the log, or the mount is refused. What a mount
holds of a filesystem's geometry is kept in pieces, none of them growing with
it past what one allocation of the kernel's heap holds (512 KiB): the
descriptors a block each, the journal's map as the runs its inode maps it in,
the system zone -- the blocks no file may own -- as runs joined as they are
worked out. Kept whole, a 16 GiB root's 128 MiB journal made the system zone
520 KiB, and its mount for writing was refused. Past what extfmt
checks of a superblock, the driver refuses a block it cannot read in a page,
more blocks than the device has, groups more than their bitmaps cover, an
inode count that is not what the groups hold, a first inode files may have
below 11, descriptors of more than 1024 bytes, and descriptors whose bitmaps
or tables lie outside the filesystem or on its superblock, its descriptors
and the blocks kept for them to grow into; a journal written through must
be in the filesystem's blocks and of 1024 blocks at least, as Linux has it,
or the mount only reads. The root inode is read before anything is
written: a filesystem whose root does not read is refused as it was found.

Every checksum on what is read is held to the one it should be, and what
fails is said and not used: the superblock and every descriptor at the
mount -- either refuses it -- and an inode, an extent block, a directory's
leaf (by its tail) or index block (by its own) when they are read, which
fails the read, or leaves an entry out of its directory, with a line saying
which. An inode in a part of its group's table never written -- an
uninitialised group's, or past `itable_unused` -- is not read at all.
Unwritten extents read as zeros, whatever their blocks hold. No file may
own a block of the filesystem's own -- the superblock, the descriptors, the
bitmaps, the inode tables, the journal: an extent that names one is
refused wherever it is met, as Linux refuses it (`Meta::file_may_own`, the
kernel's `ext4_inode_block_valid`) -- a read, a write in place, a disk's
image mapped off the VFS's lock, a cut's tail made zeros -- and every node
of a tree a call is about to change is held, as the kernel holds every node
it reads, to extents in order and apart, none of no blocks or past the
2^32 a file has. An entry naming an inode of the filesystem's own (below
the first a file may have, but the root), one deleted, or a file bigger than
a file here can be, is left out of the tree, said. A file is read
a run of an extent at a time: as many of its blocks as the read reaches
and a batch holds -- 64, a page each -- in one call (`Disk::read_pieces`),
in flight together where the device takes several requests at once.
Directories
are read as ext2's are, the first time a walk enters them and all of one or
nothing; an htree directory as the linear one it also is. Names of 64
bytes or more, symlinks and device nodes are left out of the tree.

A journal holding transactions nobody replayed -- a crash under Linux, or
under nos once it writes -- is not replayed by a read-only mount, which
writes nothing: extfmt's recovery planner works out what a replay would
write (`extfmt::recovery`, held to e2fsck's replay byte for byte by
`extfmt-test.py`) -- in three passes over the log, as the kernel's
recovery reads it: the scan to its end, noting every home a copy names;
the revokes of the transactions found whole, of those homes; the copies,
the newest of each home kept. What it holds grows with the log's blocks and
no more, in maps a node at a time: a list of every transaction once asked
the kernel's heap for 917504 bytes at once for a log of 8193 of them --
which nos's own journal reaches after hours of small commits on a root of
32 GiB or more, its journal mke2fs's 256 MiB -- and every mount after the
crash that left it panicked. Every read of a block the plan names is served from
its newest copy in the journal -- a file's data among them, as Linux's
data=journal leaves it, the rest of the batch off the device. The
superblock and the descriptors are read
again through it if it has newer copies of them, as a replay would have
left them. `mounts` says so (`journal-not-replayed=<blocks>`), and while it
is so a disk's image of a file here is read through the VFS, not off the
VFS's lock from where the file's blocks are on the device -- whose copies
are older. A journal the planner refuses -- a descriptor damaged under a
sound commit, a copy failing its checksum, features it does not know -- has
the mount refused, as a filesystem read around it would be neither as it
was nor as it was to be; a log the superblock does not say needs replaying
is read past, as Linux discards it.

### Writing, through the journal

A mount for writing replays the journal if it holds a log -- the planner's
copies written home, flushed, the log marked empty and the superblock's
error fields kept as they were before the replay -- sets the features its
own commits use (v3 checksums, revokes, 64-bit tags on a 64bit filesystem),
and says the filesystem needs recovery, forced to the medium, before
anything else is written. A clean unmount takes that back last, once a
checkpoint has put everything home and the log is marked empty, the
superblock's free counts made the groups'.

Every call that changes metadata is one transaction (`ext4/txn.rs`): what
it changes is kept apart, as copies of whole blocks, until the journal
holds all of it, and a call that fails before then leaves nothing changed
-- on the disk, in the descriptors and superblock in memory, in the tree.
A block is changed only as a kind -- a bitmap, the descriptors, an inode
table block, a directory's leaf or index block, an extent block, an
attribute block -- and every checksum its kind carries is put on it as the
transaction is sealed; an inode's as it is stored. The transaction holds
the device, the cache and the journal for as long as it lives, so that no
checkpoint can write anything home meanwhile, and it is bounded: a quarter
of the journal, and never more than 512 blocks.

A commit (`ext4/journal.rs`) writes the log's start into the journal's
superblock first if the log was empty, then revoke blocks, descriptors and
copies -- a copy whose first word is the journal's magic escaped -- flushes,
and writes the commit block forced to the medium, its time never earlier
than the last. Only then are the blocks what reads see -- pinned in memory
until a checkpoint writes them home, the only copy of each meanwhile. A
checkpoint -- when the log is half full, when more than 1024 blocks are
pinned, when a commit would not fit, at the unmount -- writes every pinned
block home,
flushes, and moves the log's start past them. A block freed is not handed
out again by its own transaction, is never written home by a checkpoint
after, and is revoked when the live log holds a copy of it, so that no
replay writes that copy over what the block becomes. A write that fails in
a commit or a checkpoint -- or a flush, `sync`'s among them, whose order
nothing after could count on -- aborts the journal: nothing more is written --
the device itself refuses every write and flush from there on (`Io`), and
no call begins another transaction -- the
call fails, reads go on, `sync` fails, `mounts` says `journal-aborted`, and
the unmount writes nothing, leaving the filesystem to the next mount's
replay -- or e2fsck's. A block of a file's data that does not write fails
its call alone: its transaction is undone, so nothing maps what failed, and
the filesystem stays writable -- one file's bad sector, a guest disk's, is
not every file's, as Linux's ordered mode by default (`data_err=ignore`)
has it too.

Names: a new inode is the lowest free in the first group from its parent's
that has one -- so that the part of a table never written shrinks by one --
written whole, its extra fields among them, a random generation; a group
never initialised has its bitmaps made as the kernel makes them, its block
bitmap from the system zone (every group's metadata and the journal, which
no file is given whatever a bitmap says) with the bits past its end set. An
entry goes into the first entry with room after it, else into a new block at
the directory's end, the extent tree grown to take it -- the last extent
stretched, a new leaf and levels down to it under the lowest node with
room, a level more at the root. An indexed directory has its index dropped
in the same transaction before an entry goes into it: its root made the `.`
and `..` it also is, its other index blocks empty leaves, the flag cleared
-- linear, which is all a reader without htree reads; an entry taken out of
one leaves the index as it is, true still. A name is looked for on the disk,
not in the tree, which leaves some out; a directory is removed only once
the disk shows nothing in it, and a recursive remove starts only once every
directory under it is known to hold nothing the tree does not show -- a
node at a time after that, deepest first, each its own transaction. A file
loses a link at a time; its last one gives back its blocks and its tree's,
and its share of an attribute block, which goes with its last reference. A
directory's link count is kept as dir_nlink keeps it: 1 past 65000, and 1
kept.

Data (`ext4/data.rs`): a write is cut into transactions of at most 256
blocks, and each one's data is on the device before its commit -- whose
flush comes first -- as Linux's data=ordered has it, so that no block a
commit maps into a file can show what it held before. A block mapped and
written is written over in place, read first where the write covers only
part of it; one in a hole is allocated and written whole, zeros around what
goes into it; one in an unwritten extent is written whole and made
written -- the extent split around it. The blocks go to the device in
batches, each block made whole in a page of the mount's buffer of 64 and
as many as it holds sent together (`Disk::write_pieces`: in flight at once
on NVMe and virtio-blk), the last before the commit; a batch that fails is
the write's failure there, as one block's would be. A
write that fails part way leaves the
transactions before it committed and the file the size they gave it --
and writes zeros again over what it put past the old end into the last
block, in place, which the size that would have covered it never did. Every
write commits, so a write that returns is on the medium. A truncate gives
back everything mapped past the new end -- unwritten extents past the old
one among them -- and writes zeros over what of the new last block lies
past it, so that a file reads as zeros wherever it grows over; one that
reaches 2 GiB sets large_file in the superblock, in the same transaction. A
punch -- a guest's discard of its disk's image -- gives back the blocks of a
range inside the file, its size as it was. The extent tree is changed a
leaf at a time (`ext4/extents.rs`): the leaf's entries edited as a list --
an extent put in where nothing maps, run on into its neighbours; a range
taken out, an extent trimmed or split around it; part of an unwritten
extent made written -- and then the path to it settled: a node that
overflows split, past the change where it is at the end as files grow; the
root a level deeper when it fills; a node left empty freed and taken out of
its parent; every index entry's key its child's first block; an emptied
tree a leaf again. A disk's image maps its file's blocks and reads and
writes them off the VFS's lock (`image.rs`): a hole -- or an unwritten
extent, which maps as one -- goes through the VFS, as it must for blocks to
be allocated or made written, and a discard makes the next sync the
filesystem's. What holds a file as an image is counted by the file -- its
mount and inode -- and not by the name it was opened by (`vfs.rs`): a
second name, a hard link, does not get past the hold. A file of more than
one name has every name's vnode given its size.

Giving back (`ext4/orphans.rs`): a file's blocks go back from its end, an
extent at a time, a transaction taking a bounded share -- as many extents
as a sixteenth of the blocks one may hold, four to thirty-two, and none
past the first once it holds half what one may -- and what it frees kept
as runs, so that a file of millions of blocks is a handful of them. A remove of a file's last
name, or a cut, that one transaction cannot finish puts the inode on the
superblock's orphan list in its first -- the name gone, or the size cut,
with it -- gives back the rest in the ones after, and takes it off the
list in its last; a crash between them leaves an inode of no links named
nowhere, or a file whose extents run past its size, which a mount for
writing releases after its replay -- as Linux and e2fsck release another
kernel's -- writing zeros over what a cut file's last block holds past its
size, as Linux does. An inode is on the list once at most and leaves it
wherever on it it is, found by the list's own links: a call whose giving
back does not finish -- a read that fails part way -- leaves its inode on
the list with the filesystem still written, and a later cut of the same
file leaves it where it is, a remove of it takes it off the list before
the inode is freed, and an inode put on the list above it does not stop it
leaving. An inode with no extents on the list -- a fifo, a socket, a device
node, a fast symlink Linux left there still open -- has nothing to give back
but itself and its attribute block. A directory gives its blocks back as a
file does, a bounded share a transaction, through the orphan list: an empty
one that once held millions of names keeps every block they took. A punch goes on in transactions of its own, each a
range given back whole, and may take the last few free blocks
(`RESERVE`), which nothing else is given: a punch inside an extent splits
it, and on a full disk the leaf the split needs must not be what stops a
guest giving its disk's blocks back. A disk another system filled past them
may still refuse one, leaving the range as it was but for the end of it the
transactions before gave back. A write that fails part way
forces what it wrote in place to the medium before it returns, as every
call leaves what it did: no commit came to flush it.

### What is not done yet

Each of these is left until a measurement or a need asks for it:

- **Group commit.** Every call that changes metadata commits before it
  returns -- a flush, the log, a flush and the commit block written
  through -- so what a small call costs is the device's flushes, not its
  data: 0.58 ms a small file copied under QEMU, more on an NVMe whose flush
  is slow. Calls that come together could share one commit.
- **Preallocation.** There is no `fallocate`: a disk's image `hv create`
  makes is a sparse file, its blocks allocated as the guest first writes
  them -- through the VFS, not off its lock -- and nothing keeps the room
  for them. Unwritten extents are read and made written, never made.
- **orphan_file.** A filesystem with it -- Debian 13's mke2fs makes one by
  default -- mounts read-only; the installs leave it out.
- **Indexed directories, written.** A directory is read whole whether it
  is indexed or not, and loses its index to the first name nos adds, so
  Linux looks names up in a big one a block at a time from then on.
- **Discard.** What a remove, a cut or a punch gives back is not discarded
  on the device: the block layer has no discard, and an SSD under a
  filesystem that grows and shrinks never hears of the free space.
- **CRC32C by the CPU.** Every checksum is computed a byte at a time from
  a table (`extfmt::crc`); arm64's CRC32C instructions and x86's SSE4.2
  `crc32` are not used.
- **Writing what the installs do not make.** A filesystem without a
  journal, without extents or without metadata_csum, or with quotas, stays
  read-only, and the refused features stay refused (above).

## nanofs, ramfs, procfs

nanofs (`src/crates/fs/src/nanofs.rs`) is the kernel's own small checksummed filesystem:
1024 inodes, 64 MiB of data, files up to 1 MiB, every write copy-on-write
with the inode committed last. It predates ext2 write support and remains
for what it is good at — a small, self-verifying store (`format nanofs`,
`mount nanofs`, `scripts/mkfs_nanofs.py`) — and takes the whole file API,
including writes at an offset and truncates, by rewriting the file. The
mount reads the whole tree, by a walk with a stack of its own, so a tree as
deep as the inodes allow reads back whole; a read checks the file's data
against its checksum the first time after a mount or a write, not on every
piece. Mounted read-only it writes nothing, not even the bitmap repair the
mount makes of a reachable inode or block marked free. `format` and `mount`
refuse a device smaller than the layout (69 MB) before touching it.

ramfs (`src/crates/fs/src/ramfs.rs`) is the in-memory filesystem the fallback
layout puts on `/`, and what `test_vfs` exercises at every boot: a file is
one buffer that doubles as it grows. procfs (`procfs.rs`) is a ramfs whose
`/proc/interrupts` is regenerated on each lookup -- the VFS looks a file up
before it reads it or reports its size, so refreshing there keeps the two
consistent; `/proc/version` and `/proc/cmdline` are written once at mount.

## Making a root filesystem

```sh
scripts/mkrootfs.sh root.img 256 rootdir        # 256 MiB, populated from rootdir/
scripts/mkrootfs.sh root.img 64 "" 1024         # empty, 1 KiB blocks (what the smoke tests use)
FSTYPE=ext2 scripts/mkrootfs.sh root.img 64     # ext2, as the roots made before ext4 (ext2-test.py's)
```

This is `mke2fs -t ext4 -L nos -O none,<the features above>
-E lazy_itable_init=0,lazy_journal_init=0 -d rootdir` -- for ext2,
`mke2fs -t ext2 -L nos -O ^dir_index`, no directory index, which ext2's
driver does not write: the label `root=auto` looks for, populated from a
directory without loop devices or root privileges (inside the `nos-builder` Docker image on macOS,
where the copy goes through a directory in the container because a macOS
bind mount serves no extended attributes). Attach it to QEMU as a raw disk
and the ISO boot mounts it:

```sh
qemu-system-x86_64 ... -cdrom nos.iso -drive file=root.img,format=raw,if=virtio
```

`scripts/build-disk.sh` makes `nos.qcow2` the same way — one MBR partition,
ext4 labelled `nos`, `/boot/kernel64.elf` and `/boot/grub` inside it, GRUB's
boot code in the boot sector, `ROOTFS=dir` to add content (modules under
`lib/modules`) — and fails on anything `e2fsck -fn` finds in the result
(given the partition as `image?offset=`; it has no `-E offset=`, which it
refused while its exit code was ignored). Nothing in it needs
`--privileged`.

On a machine that dual-boots Ubuntu (the Hetzner boxes) the root is a
partition `nos-install` made an ext4 under Ubuntu ([Install](install.md)),
or the ext2 one an install before ext4 made, which keeps it; named on the
`nos` command line by UUID. After a `nos` session, `e2fsck -f` under Ubuntu
is the check that the driver left it as it should -- for an ext4 nos left
without unmounting it, after Ubuntu's mount or `e2fsck`'s replay of its
journal.

## Checking it

`fstest [dir] [size]` from the shell (`size` at least 8 KiB), or `fstest=on`
at boot, runs
The self-test (`src/crates/fs/src/selftest.rs`) runs in a directory it makes and removes: write
and read back, append, write at an offset, truncate both ways with the gap
checked for zeros, rename, move into a subdirectory, `readdir`, the
operations that must fail, a file of `size` bytes (300 KiB by default)
written in 64 KiB pieces and read back in 12345-byte ones, patched in the
middle, cut short, grown back, and a recursive remove. `fstest / 5M` at
4 KiB blocks reaches the doubly-indirect blocks; the smoke tests get there at
300 KiB by using 1 KiB blocks. And where the filesystem holds a sparse file
that big at no cost but what is written (`FileSystem::sparse_max`: ext2,
and not ramfs or nanofs, which pay for a file's whole size), a far file:
grown past 5 GiB with holes, patches written across 2 and 4 GiB and twice
past them -- under the triple indirect block, of 1 KiB blocks all of them --
read back with the holes around them, cut in the middle of one and grown
again, what was cut reading as zeros; the smoke tests require its line on
both architectures. `crc32 <path>` checks a file against its copy on the
host, and `truncate <path> <size>` cuts a file, or grows it with a hole -- a
guest's blank disk image of any size the filesystem holds.

`test_vfs` in `kernel/test.cpp` runs the same test on a ramfs at every boot,
before any disk is up.
