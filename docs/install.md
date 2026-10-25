# Install: nos on a server

How to put nos on a bare-metal x86-64 server from a release, boot it, update
it and get back out. The unit is the **system image**, `nos-x86_64.squashfs`:
the kernel, every module built with it, the GRUB entries that boot it, a
default `/etc/rc` and `nos-install`, the script that pours it onto a
partition. The server keeps its own Linux and its own GRUB; nos gets a
partition of its own and an entry in that GRUB, and boots **once** when asked,
so a nos that never comes up is a reset away from the server's own system --
and one that panics resets itself, ten seconds after its report (`panic=10`).

That is how the Hetzner boxes in [Real hardware](real-hardware.md) run nos.
They were set up by hand, before this existed; a new machine is set up as
below.

## What the server needs

nos boots on what it has drivers for, and a server's only console is its
network, so check these before anything else. `nos-install` looks for each
and says what it finds -- the partition it refuses, the rest it warns of --
but cannot fix any of them:

- **An NVMe or a virtio disk** for nos's partition. Those (and virtio-scsi)
  are the disks nos reads. On a SATA disk, or behind a RAID controller, GRUB
  still loads the kernel -- it reads disks through the firmware -- but nos
  finds no root there: no modules, no `/etc/rc`, no sshd.
- **A network card nos drives**: Realtek RTL8168 or RTL8125, Intel 82576,
  I210 or I211 (the `igb` driver), or virtio-net. With any other, nos boots and
  nobody can reach it. The network has to hand out addresses by DHCP: nos has
  no static configuration.
- **A plain partition** of at least 64 MiB (256 MiB is comfortable; a guest's
  disk images need more): not a member of an md RAID, not an LVM volume, not
  encrypted -- nos reads none of those. A server installed with its disks
  mirrored end to end has no such partition. Leave space for one when
  installing it (Hetzner's `installimage` takes a partition list) or make one
  from free space.
- **Linux booting with GRUB 2**, BIOS or UEFI, with **Secure Boot off**: nos
  is a Multiboot2 kernel, and a GRUB under Secure Boot's lockdown refuses to
  load one. `nos-install` runs under that Linux, as root, and wants
  e2fsprogs, util-linux, GRUB's tools (`grub-editenv`, `update-grub` or
  `grub-mkconfig`) and either squashfs support in the kernel or
  `unsquashfs` -- all of them are on a stock Ubuntu or Debian.

## The image

From a release -- every tag `v*` builds one (`.github/workflows/release.yml`):

```sh
wget https://github.com/irqlevel/nos/releases/latest/download/nos-x86_64.squashfs
wget https://github.com/irqlevel/nos/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

Or built from the tree: `make image` on Linux with the toolchain, or
`scripts/build-image.sh` anywhere Docker runs (`--clean` builds from nothing,
as a release does). Either leaves `nos-x86_64.squashfs` at the top of the
tree.

What is in it:

| Path | What |
|---|---|
| `/boot/kernel64.elf` | the kernel |
| `/boot/grub/nos.cfg` | its GRUB entries, `nos` and `nos (next)`, with the command line the kernel was built to read |
| `/lib/modules/*.ko` | every module built with that kernel, so every one of them loads into it ([Modules](modules.md)) |
| `/lib/hvboot` | the loader `hv` boots a guest's own kernel with, of the same build as `hv.ko`: the kernel of every VM `hv create <name> <url>` makes ([The hypervisor](hypervisor.md#a-guests-own-kernel-pvh-and-hvboot)) |
| `/etc/rc` | the default boot script: `insmod /lib/modules/sshd.ko`, `sshd start` |
| `/nos-install` | the installer, the one that knows this image |
| `/VERSION` | `version=`, `revision=` (what `version` prints in nos), `arch=`, `module_abi=` |

Nothing in it belongs to a machine: no host key, no authorized key, no
address. Owners, times and modes are fixed, so a build makes the same bytes
every time, and the image's sha256 names what is in it.

## Pouring it onto a partition

As root, on the server:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install install /dev/nvme0n1p4 --ssh-key /root/.ssh/authorized_keys
umount /mnt
```

`--ssh-key` takes a `.pub` file, an `authorized_keys` file or the key line
itself, and may be given more than once. nos's sshd takes `ssh-ed25519` keys
only: of a file, `nos-install` takes those and says which lines it left out
(another kind of key, or options in front of one); a key given as a line of
another kind, or a file with no `ssh-ed25519` key in it, it refuses. It is
optional -- but without one sshd lets nobody in, and with the UDP shell off as
well, a booted nos has no way in at all.

What `install` does, in order, stopping at the first thing that is wrong:

1. Checks the image (`/VERSION`, the kernel, the modules; for this machine's
   architecture; nothing in it this `nos-install` does not know where to
   put, which it would leave out -- an image newer than it, whose own
   `nos-install` does) and the partition: a block device, not a disk with
   partitions on it, not held by md, LVM or the device mapper, not one of the
   running system's filesystems. Warns when the disk is not NVMe or virtio,
   when no network card nos drives is there, when Secure Boot is on.
2. A partition with nothing on it gets an ext2 filesystem labelled `nos`
   (`mkfs.ext2 -O ^dir_index`, the format nos writes, docs/filesystems.md).
   One that has an ext2 nos can write keeps everything on it; one with
   anything else is refused, unless `--format` says to make a new
   filesystem in its place -- which loses what was there.
3. Mounts it and puts the kernel, the modules, hvboot, `nos.cfg` and
   `/VERSION` there. On a partition that had them, the old ones but
   `nos.cfg` go to `.prev`, together, for `rollback`. `/etc` is the
   machine's: the image's `/etc/rc` goes in only where there is none, and
   the keys are added to `/etc/ssh/authorized_keys`. The machine's
   microcode -- Intel's file for
   its CPU from `/lib/firmware/intel-ucode`, which the server's own system
   keeps current -- goes to `/boot/ucode`, which GRUB loads beside the
   kernel; with none there (an AMD CPU, no `intel-microcode` package), nos
   runs firmware's.
4. Writes `/grubenv` on it -- this machine's kernel parameters, and the
   one-shot flag -- and checks the filesystem with `e2fsck`, before the
   write and after it.
5. Writes `/etc/grub.d/42_nos`, which finds the partition by its UUID, and
   runs `update-grub` (or `grub-mkconfig`); then checks `grub.cfg` has it.

The server's default boot entry does not change.

| Option | |
|---|---|
| `--ssh-key KEY` | a key sshd lets in (above) |
| `--udpshell[=PORT]` | the UDP shell, on 9000 or PORT. **Off unless given**: it runs any command for anyone who can send the machine a datagram ([UDP shell](udp-shell.md)) |
| `--netconsole IP:PORT` | the kernel log, over UDP, to IP:PORT ([Netconsole](netconsole.md)) -- the one way to see a boot that dies after the network is up |
| `--cmdline "ARGS"` | more kernel parameters ([Kernel parameters](kernel-parameters.md)) |
| `--format` | a new filesystem on the partition, even over one there |
| `--boot-once`, `--reboot` | boot nos once, the next time -- and reboot now |
| `--image FILE` | the image, when this `nos-install` is not the one inside it |

## Booting nos

```sh
nos-install boot --reboot      # or --reboot on install or update
```

`boot` sets `nos_next=nos` in the partition's `/grubenv`. At the next boot
GRUB reads it in `42_nos`, makes the `nos` entry the default for this one
boot, and **clears it before loading anything**; the server's own first
entry stays the fallback. So:

- a nos that comes up stays up until it reboots, and `reboot` in it goes back
  to the server's own system;
- a nos that does not come up -- a panic, a hang, a kernel GRUB cannot load
  -- is followed by the server's own system after a reset (Hetzner's robot
  can do one), with nothing left to undo.

The kernel's command line is `root=UUID=<the partition> dhcp=auto dns=on
panic=10` and whatever this machine was given (`--udpshell`, `--netconsole`,
`--cmdline`) -- `panic=10` resets a nos that panics ten seconds after its
report, to the server's own system after a one-shot boot. nos mounts the partition read-write as its root, takes an
address by DHCP and runs `/etc/rc`, whose `sshd start` makes the host key the
first time -- from the kernel's entropy pool, into
`/etc/ssh/ssh_host_ed25519_key`, kept there for every boot after -- and lets
in the keys of `/etc/ssh/authorized_keys` ([sshd](sshd.md)). About a minute
after the reset:

```sh
ssh -o UserKnownHostsFile=~/.ssh/known_hosts.nos root@<server> version
```

nos has a host key of its own, not the Linux one `known_hosts` knows for the
address, hence a file of its own for it. To have one key under both, copy the
Linux one in before the first boot: `/etc/ssh/ssh_host_ed25519_key` to the
same path on the partition (sshd reads OpenSSH's format).

To stay in nos across reboots, make it the server's default: `GRUB_DEFAULT=nos`
in `/etc/default/grub`, then `update-grub`. Then a nos that does not come up
comes up again after every reset, and the way out is the hosting provider's
rescue system.

## From a workstation

`scripts/nos-deploy.py` does the above from the machine the image was built
on, over SSH to the server's Linux as root:

```sh
scripts/nos-deploy.py root@<server> install /dev/nvme0n1p4 --ssh-key ~/.ssh/id_ed25519.pub
scripts/nos-deploy.py root@<server>            # every time after: update
```

It copies the image there, checks its sha256 there, mounts it and runs the
`nos-install` inside it with `--boot-once`, reboots the server, waits for
nos's own sshd to answer on port 22 (`SSH-2.0-nos_sshd`, where OpenSSH
answered before), and logs in to check the kernel running is the image's.
`--no-boot` stops before the reboot, `--wait` gives nos longer than ten
minutes, and nos-install's options pass through.

## Updating, and going back

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install update --reboot
```

`update` finds the partition through `42_nos` and puts the new kernel,
modules, hvboot and `nos.cfg` there; the old kernel, modules, hvboot and
`/VERSION` go to `.prev` together, and the microcode is taken again from the server's
`/lib/firmware`. `/etc` -- `rc`, the keys, the host key -- and everything
else on the partition stay. The kernel parameters stay too, unless
`--udpshell`, `--netconsole` or `--cmdline` is given: then those replace them
all (`--cmdline ""` clears them). A kernel and its modules are only ever
replaced together, since a kernel refuses a module built against another
kernel interface. `/lib/modules` is the image's: a module of your own goes
somewhere else.

`nos-install rollback` swaps the current kernel, modules, hvboot and
`/VERSION` with the `.prev` ones -- back to an image older than hvboot,
none. `nos-install status` says what is installed and what was,
the kernel parameters, the microcode's revision and whether a boot is armed. `nos-install uninstall`
takes `42_nos` out and nos out of the menu, and leaves the partition alone.

From inside a running nos, without Linux, a kernel can be tried before it
replaces the running one, as [Real hardware](real-hardware.md#updating-the-kernel-from-inside-nos)
describes -- here into `/boot`:

```sh
wget https://github.com/irqlevel/nos/releases/download/<tag>/kernel-x86_64.elf /boot/kernel64.elf.next
grubenv /grubenv nos_next=nos-next
reboot
```

That boots the `nos (next)` entry once. It brings no modules: the ones in
`/lib/modules` are the running kernel's, and the new one refuses them if
their kernel interface differs. A whole image is updated from Linux.

## What goes where

On the partition, which is nos's root:

| Path | From | |
|---|---|---|
| `/boot/kernel64.elf`, `.prev` | the image | the kernel GRUB boots, and the one before it |
| `/boot/kernel64.elf.next` | you, from inside nos | a kernel tried once |
| `/boot/grub/nos.cfg` | the image | the entries `nos` and `nos-next` |
| `/lib/modules/`, `/lib/modules.prev/` | the image | the kernel's modules, and the ones before them |
| `/lib/hvboot`, `.prev` | the image | the loader of a guest's own kernel, and the one before it |
| `/VERSION`, `.prev` | the image | what is installed, and what was |
| `/boot/ucode` | the server's `/lib/firmware/intel-ucode`, at every install and update | this CPU's microcode, which nos loads on every CPU as it boots ([Boot](boot.md#x86-64-the-application-processors)) |
| `/grubenv` | nos-install | `nos_cmdline`, this machine's kernel parameters; `nos_next`, the one-shot flag |
| `/etc/rc` | the image, the first time | the boot script: the machine's after that (`rc` edits it) |
| `/etc/ssh/authorized_keys` | `--ssh-key` | the keys sshd lets in (`sshd allow` adds one from inside) |
| `/etc/ssh/ssh_host_ed25519_key` | sshd, at its first start | nos's host key |

On the server's Linux there is one file, `/etc/grub.d/42_nos`, which holds
the partition's UUID and nothing else of the machine's. It is run by
`update-grub` like the rest of `/etc/grub.d`, and at every boot it loads
`nos_next` and `nos_cmdline` from the partition's `/grubenv` -- only those --
and the entries from the partition's `/boot/grub/nos.cfg`. A new image brings
new entries without `update-grub`.

An entry of the server's own with the id `nos` or `nos-next` -- a hand-made
one in `/etc/grub.d/40_custom`, as on the boxes set up before this -- comes
first in `grub.cfg` and would be the one booted; `nos-install` says so when
it finds one. So does an `/etc/rc` that loads a module from outside
`/lib/modules`: after an update that module is of another build.

## Limits

- x86-64 only. The arm64 kernel runs on QEMU `virt`; there is no arm64
  server to pour it onto.
- One partition per server: `42_nos` names one. Installing onto another moves
  the entry to it.
- nos's ext2 has no symlinks or device files, and no file over 4 GiB at 4 KiB
  blocks ([Filesystems](filesystems.md)).
- A server whose disks or network card nos has no driver for is not one this
  works on: see the first section.

`scripts/image-test.py` is the test of all of this: it pours the image onto
a partition with `nos-install` under Linux, boots the disk through a GRUB
standing in for a server's, logs in with the key it gave, checks that no UDP
shell answers and that the next boot is the server's own, then updates,
rolls back and uninstalls ([Testing](testing.md)).
