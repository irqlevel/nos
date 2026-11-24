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

A server can also be nos's alone: a whole disk made nos's own, with a GRUB
of its own that boots nos and nothing else -- from the provider's rescue
system, with one command,

```sh
curl -fsSL https://nos-os.tech/install.sh | sh
```

or from the **installer ISO** a provider's virtual media boots
([A disk of its own](#a-disk-of-its-own),
[From a provider's rescue system](#from-a-providers-rescue-system),
[From an ISO](#from-an-iso)).
There is no system of the server's to fall back to then: the way back in
is the provider's rescue system or its console.

## What the server needs

nos boots on what it has drivers for, and a server's only console is its
network, so check these before anything else. `nos-install` looks for each
and says what it finds -- the partition it refuses, the rest it warns of --
but cannot fix any of them:

- **An NVMe, a virtio or a SATA disk** for nos's partition. Those (and
  virtio-scsi) are the disks nos reads; a SATA disk behind an AHCI HBA (PCI
  class 0106 -- Linux binds `ahci`) or a controller the firmware leaves in
  IDE mode (0101 -- `ata_piix`). Behind a RAID controller, GRUB still loads
  the kernel -- it reads disks through the firmware -- but nos finds no root
  there: no modules, no `/etc/rc`, no sshd.
- **A network card nos drives**: Realtek RTL8168 or RTL8125, Intel 82576,
  I210, I211 or I350 (the `igb` driver), Intel 82574L (the `e1000e`
  driver), or virtio-net. With any other, nos boots and nobody can reach it. A network
  that hands out no address by DHCP is given one with
  `--ip A.B.C.D/N --gw A.B.C.D --dns A.B.C.D` -- what its Linux has
  statically -- or `--network-here`, which takes this Linux's: written to
  nos's `/etc/net.toml` ([The network](#the-network)).
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
  `unsquashfs` -- all of them are on a stock Ubuntu or Debian. A disk of
  nos's own needs no Linux of the server's: a rescue system's will do, or
  the installer's own, and Secure Boot off all the same.

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
| `/lib/CLOUDHV.fd` | OVMF's CloudHv UEFI guest firmware, from the pinned EDK2 source ([Build](build.md#cloudhv-guest-firmware)); the platform it needs is [`plans/windows.md`](../plans/windows.md) |
| `/lib/licenses/CLOUDHV` | the firmware's EDK2, Xen and LZMA notices |
| `/lib/licenses/CRYPTOGAMS` | the license of the CRYPTOGAMS assembly the kernel and its modules run -- ChaCha20-Poly1305's ([HTTPS](tls.md#the-assembly)) -- which asks a binary handed out to carry it |
| `/etc/rc` | the default boot script: `insmod /lib/modules/sshd.ko`, `sshd start` |
| `/nos-install` | the installer, the one that knows this image |
| `/nos-setup` | the installer's dialog, the ISO's, run in a provider's rescue system from the image mounted ([From a provider's rescue system](#from-a-providers-rescue-system)); like `nos-install`, it stays in the image |
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
2. A partition with nothing on it gets an ext4 filesystem labelled `nos`:
   its features named one by one from none, as nos's ext4 writes them and
   GRUB 2.06 reads them, nothing this machine's `mke2fs.conf` would add, its
   inode tables and journal written whole (docs/filesystems.md). One that
   has an ext4 or an ext2 nos can write -- an install before ext4 made an
   ext2 -- keeps everything on it, its journal replayed first where nos
   left it without unmounting; one with anything else is refused, unless
   `--format` says to make a new filesystem in its place -- which loses
   what was there.
3. Mounts it and puts the kernel, the modules, hvboot, CloudHv, the licenses,
   `nos.cfg` and `/VERSION` there. On a partition that had them, the old
   ones but
   `nos.cfg` go to `.prev`, together, for `rollback`. `/etc` is the
   machine's: the image's `/etc/rc` goes in only where there is none, and
   the keys are added to `/etc/ssh/authorized_keys`. The machine's
   microcode -- Intel's file for
   its CPU from `/lib/firmware/intel-ucode`, which the server's own system
   keeps current -- goes to `/boot/ucode`, which GRUB loads beside the
   kernel; with none there (an AMD CPU, no `intel-microcode` package), nos
   runs firmware's.
   A seed for the random pool goes to `/etc/random-seed`, 64 bytes from
   this machine's `/dev/urandom`: on a CPU with no random instruction it is
   what sshd's keys come from ([Randomness](random.md)).
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
| `--cmdline "ARGS"` | more kernel parameters ([Kernel parameters](kernel-parameters.md)) -- not `ip=`, `gw=`, `dns=` or `dhcp=`, which are the options below |
| `--ip A.B.C.D/N`, `--gw A.B.C.D`, `--dns A.B.C.D` | nos's address on a network with no DHCP server, its gateway and its DNS server, in `/etc/net.toml` ([The network](#the-network)); `--dns` alone, or with `--dhcp`, is the DNS server asked in place of the lease's |
| `--dhcp` | nos's address by DHCP, at every boot: what `/etc/net.toml` says when no option of the network is given |
| `--network-here` | the address, gateway and DNS server this Linux has (below) |
| `--panel` | the management panel (below); `--no-panel` takes `api panel create` out of `/etc/rc` again |
| `--format` | a new filesystem on the partition, even over one there |
| `--boot-once`, `--reboot` | boot nos once, the next time -- and reboot now |
| `--image FILE` | the image, when this `nos-install` is not the one inside it |

### The management panel

The interactive installer, both from `curl ... | sh` and from the ISO, asks
**Install the management panel?** The default is **No**; choosing **Yes** has
the same effect as `--panel` below. An ISO built with answers takes
`panel=yes`; left out or `panel=no`, no panel is installed.

`--panel` gives the machine the management panel: a VM of nos's own, which
manages it in a browser ([The management panel](api.md#the-management-panel)).
nos-install makes root's password for it, prints it once with where the panel
will be, and keeps only its scrypt hash, in `/etc/api/panel.pass` on the
partition; and it gives `/etc/rc` the lines that load hv and the host API
and run `api panel create`, each added only where the file does not have it:

```
nos-install: the panel: https://203.0.113.5:39233 once nos has made it, minutes after its first boot
nos-install: the panel's user is root, its password qD1Rf...
nos-install: that password is printed this once: /etc/api/panel.pass on /dev/nvme0n1p4 has its hash alone
```

At nos's first boot `api panel create` downloads Debian's cloud image and the
panel's bundle -- the release's, which the image names in `/lib/panel/bundle`
-- and makes the VM; the panel is up once its VM has installed it, minutes
later, its certificate the one `api panel` prints the fingerprint of. An
update with `--panel` keeps the password there is. `python3` is needed for
the hash (`hashlib.scrypt`): the ISO carries it, and the one-line installer
installs it with the other missing packages where apt-get is available.

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

On a server with nobody to reset it -- no robot, no BMC -- give a boot that
tries something `--cmdline deadman=N` as well: a nos that has not been told
`deadman off` N seconds into its boot panics, and `panic=10` resets it, so one
that never reached the network is back on the server's own system by itself
([Kernel parameters](kernel-parameters.md)). Not on a machine that boots nos
every time, unless its `/etc/rc` calls it off.

The kernel's command line is `root=UUID=<the partition> dhcp=auto dns=on
panic=10` and whatever this machine was given (`--udpshell`, `--netconsole`,
`--cmdline`) -- `panic=10` resets a nos that panics ten seconds after its
report, to the server's own system after a one-shot boot. nos mounts the partition read-write as its root, takes
eth0's settings from `/etc/net.toml` there -- an address by DHCP, or the
static one the install was given ([The network](#the-network)) -- and runs `/etc/rc`, whose `sshd start` makes the host key the
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

## A disk of its own

`nos-install disk` makes a whole disk nos's -- what the installer ISO runs,
and what a provider's rescue system, a Linux booted from the network with
the server's disks idle, runs as well:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install disk /dev/disk/by-id/nvme-SAMSUNG_MZQL2960HCJR-00A07_S64FNE0R123456 \
    --ssh-key /root/.ssh/authorized_keys --root-size 10G
umount /mnt
```

The disk gets a GPT of nos's own:

| Partition | Size | |
|---|---|---|
| `BIOS boot` | 1 MiB, from 1 MiB | GRUB's core image, for a BIOS's boot |
| `EFI System` | 512 MiB, FAT32 | GRUB for UEFI at `\EFI\BOOT\BOOTX64.EFI`: the path firmware boots a disk by when it has no boot entry for it |
| `nos` | `--root-size`, 10G unless given; `all` for the rest of the disk | nos's root: an ext4 labelled `nos`, the image on it as `install` puts it there |
| `nos-data` | the rest, where it is 1 GiB or more | nothing on it -- its first and last MiB zeros: a VM's disk, by the GUID `disks` shows it with ([A device as a guest's disk](hypervisor.md#a-device-as-a-guests-disk)), or NBS's device (`nbs devices add`) |

GRUB goes on for BIOS and for UEFI both, where this Linux has both
(`grub-pc-bin`, `grub-efi-amd64-bin`; the one the machine boots by it must
have), so the disk boots whichever way the firmware is set, and the root's
`/boot/grub/grub.cfg` is the disk's own: what `42_nos` does in a server's
GRUB, with nothing else to fall back to -- it finds the root by its UUID,
takes `nos_cmdline` and `nos_next` from `/grubenv`, and the entries from the
image's `nos.cfg`, `nos` the default.

Under UEFI the firmware is then told to boot the disk: its entry for the
disk's `\EFI\BOOT\BOOTX64.EFI` -- one the firmware made by itself (AMI's
"UEFI OS"), or one `nos-install` makes, `nos` -- goes right after the
network's entries that lead the boot order, and nothing else moves. An
entry of the firmware's that is off -- which firmware passes over, even as
the next boot -- is turned on; where efibootmgr cannot turn it on (version
18, Ubuntu 24.04's and Debian 12's, turns on only the last entry it lists),
an entry of nos's own is made beside it, and that is the one used. A
provider's servers try the network first, for its rescue system, and that
stays so; what booted after it before -- the system the server came with,
on another disk -- boots after nos now, if nos does not come up.
`--keep-boot-order` leaves the order as it is. A BIOS has no order a running
system can set: its setup, or the provider's panel, puts the disk first.

Refused, before anything is written: a partition (`install` takes those);
a disk any of which is in use -- mounted, swap, held by md, LVM or the
device mapper; a disk nos does not read -- behind a RAID controller, on USB
(it would boot, and find no root); one too small for the root; and a disk
that holds anything -- a partition table, a filesystem, any byte that is not
zero in its first or last MiB -- unless `--wipe` says to erase it.
`--dry-run` checks all of that and says what it would write.
`nos-install disks` lists the disks here as an installer offers them.

`update`, `rollback`, `boot` and `status` then find the root of a disk of
nos's own by themselves, from any Linux booted on the machine -- the
rescue system, the installer's shell -- by its label and its grub.cfg (or
take it named), and change nothing of that Linux's GRUB. `boot` has the
firmware boot the disk next, once (UEFI's `BootNext`), the order as it was
after it; `boot --default` puts it first after the network's entries, for
good; `uninstall` has nothing to take out.

`--network-here` gives nos the network this Linux has: the address of the
interface its default route leaves by, the route's gateway, its first DNS
server (`resolv.conf`, or the servers systemd-resolved asks) -- written to
nos's `/etc/net.toml`. A provider's rescue system is up on the
server's own address, so its network is the one nos needs on a network with
no DHCP. nos puts the address on `eth0`, the first network card it drives
in the bus's order, and `nos-install` says so when that is not the card
this Linux reaches the network by.

### From a provider's rescue system

A provider's rescue system -- a Linux booted from the network, the server's
disks idle, reached by SSH on the server's address (Selectel's: its panel's
boot template "Boot to Rescue") -- is the shortest way onto a server's
disk: the network and the firmware there are the server's own. One
command, as root there:

```sh
curl -fsSL https://nos-os.tech/install.sh | sh
```

`https://nos-os.tech/install.sh` runs the newest release's `install.sh`
(`scripts/installer/install.sh`, its release's tag written in by
`release.yml`), which fetches that release's system image and its
`SHA256SUMS`, checks the one against the other, mounts the image and runs
the dialog in it -- the ISO's ([From an ISO](#from-an-iso)), on the rescue
system's own terminal, with two differences:

- **The network** is offered as the rescue system has it -- the address its
  default route leaves by, the gateway, the DNS server (`nos-install
  network`) -- since a rescue system is up on the server's own address; or
  as a Linux on the disks has it, or as given. Nothing of the rescue
  system's own network is touched: the session came in by it.
- **The keys** root logs in with there (`/root/.ssh/authorized_keys`, its
  `ssh-ed25519` ones) are offered, beside the ISO's paste, URL and GitHub.

What the install runs and the rescue system lacks -- GRUB for BIOS and UEFI
(`grub-pc-bin`, `grub-efi-amd64-bin`), mkfs, `efibootmgr`, `whiptail` -- is
installed with apt-get first, where it has one; elsewhere it is named, and
`nos-install` refuses what it cannot do without. Arguments after `sh -s --`
are `nos-install`'s, and there is no dialog:

```sh
curl -fsSL https://nos-os.tech/install.sh | sh -s -- disk /dev/disk/by-id/ata-INTEL_SSDSC2KB240G7_BTYS81950A94240AGN \
    --wipe --network-here --ssh-key /root/.ssh/authorized_keys --root-size 10G
```

`NOS_VERSION=v0.0.29` before `sh` takes that release's instead,
`NOS_RELEASE_URL` a directory holding an image and its `SHA256SUMS`
(https://, file://), and `NOS_NO_PACKAGES=1` installs nothing. By hand, the
same:

```sh
wget https://github.com/irqlevel/nos/releases/latest/download/nos-x86_64.squashfs
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-setup        # the dialog -- or nos-install disk with its arguments
umount /mnt
```

Then the provider's boot back to the disks -- Selectel's boot template to
what it was -- and a reboot: the firmware boots nos after its PXE, which no
longer sends the server to the rescue.

Selectel's "install from an image" runs an ISO in QEMU in that same rescue
system (its `qemu-iso`), over the server's disks: the installer ISO below
works so, but from inside a virtual machine, whose network is QEMU's and
whose firmware is QEMU's -- it can neither see the server's address nor set
the server's boot order. The installer says so, looks for the address on a
Linux already on the server's disks, and ends with the line to run in the
rescue system before the reboot:

```sh
curl -fsSL https://nos-os.tech/install.sh | sh -s -- boot --default
```

## From an ISO

Many providers install a dedicated server from an ISO: mounted through the
server's BMC as a virtual CD (IPMI, iDRAC, iLO), or from their panel, and
seen on its console (KVM). `nos-installer-x86_64.iso` is that for nos: a
small Linux that runs from memory -- Alpine, of one pinned release --
carrying the system image as it was built, which it pours with that
image's `nos-install disk`. It boots by BIOS or UEFI; its menu:

| Entry | |
|---|---|
| Install nos | the dialog on the screen; the kernel's lines on the first serial port as well |
| ... on ttyS0, ... on ttyS1 | the dialog on a serial port: an IPMI Serial over LAN is often the second |
| Try nos | the image's own kernel, booted from the ISO with no root: nothing mounted, nothing written, and the screen says which disks and network cards nos finds -- whether this machine is one nos runs on, before anything is erased |

The dialog asks which disk -- every one Linux sees, each said with its
model, serial and size, what it holds and whether nos can read it -- how big
nos's root is to be, the network, the keys sshd is to let in (pasted, or
fetched by URL or as a GitHub user's `https://github.com/<user>.keys`, their
fingerprints shown), and whether to install the management panel. Then what
`nos-install disk --dry-run` would write, and the disk's name typed to erase
it. When it is done: take the ISO out of the virtual media and reboot --
under UEFI the firmware is told to boot the
disk (above).

The network first offers what a Linux already on the server's disks is set
up with -- the system the server came with knows its address on a network
with no DHCP: netplan, ifupdown, NetworkManager, systemd-networkd or ifcfg,
each root mounted read-only, its journal not replayed. Else a static
address, given: the gateway offered is the network's first address, where
most networks have it, and the DNS server the first of the gateway,
1.1.1.1 and 8.8.8.8 that answers from it; the installer then tries the
gateway. DHCP is said for what it is -- nos asks for its address at every
boot, and on a network with no DHCP server it boots unreachable. In a
virtual machine nothing is tried: what answers there is QEMU's.

Every release carries one, made of the release's system image and tried
before it is published (`.github/workflows/release.yml`); the newest is
always at the same link, for a provider's "install from ISO by URL":

```sh
wget https://github.com/irqlevel/nos/releases/latest/download/nos-installer-x86_64.iso
wget https://github.com/irqlevel/nos/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

Or built from the tree with Docker:

```sh
scripts/installer/build.sh                      # nos-installer-x86_64.iso, of nos-x86_64.squashfs
scripts/installer/build.sh --answers answers    # one that asks nothing
```

An ISO built with answers ([`answers.example`](../scripts/installer/answers.example))
installs with no questions, as a fleet's is: the disk by its
`/dev/disk/by-id` name, `wipe=yes`, the root's size, the keys or their URLs,
a static address, netconsole, kernel parameters, `panel=yes` where wanted,
and what to do after --
`poweroff` unless told, `reboot` (the virtual CD ejected first where the BMC
lets it be), or stay. What it does goes to the screen and, at the level
every console shows, to the kernel log: a `netconsole=` given to the
installer's kernel (`e` in its menu) carries it off a machine nobody
watches. A word `nos.KEY=VALUE` on that command line answers one question,
and wins over the file. Booted again on a disk that has the image on it as
it would put it there, it installs nothing -- so a firmware that boots the
ISO before the disk does not erase what it just made.

It carries Intel's microcode, a file a CPU as Debian's package has them
(split out of Alpine's `intel-ucode` with `iucode_tool`), so a disk it makes
has the machine's CPU's at `/boot/ucode` as an install from the server's
own Linux would; and the random pool's first seed is the installer's own
`/dev/urandom`, which Linux seeds where nos's jitter finds nothing.

The installer's Linux is GPL software among others: the packages it is made
of are listed in the ISO's `/etc/nos-installer/packages`, by version -- and
beside the ISO in the release, as `nos-installer-packages.txt` -- for their
sources at Alpine's.

## Updating, and going back

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install update --reboot
```

`update` finds the partition through `42_nos` and puts the new kernel,
modules, hvboot, CloudHv, licenses and `nos.cfg` there; the old kernel, modules,
hvboot, CloudHv, licenses and `/VERSION` go to `.prev` together, and the microcode is taken again from the server's
`/lib/firmware`. `/etc` -- `rc`, the keys, the host key -- and everything
else on the partition stay. The kernel parameters stay too, unless
`--udpshell`, `--netconsole` or `--cmdline` is given: then those replace them
all (`--cmdline ""` clears them). So does `/etc/net.toml`, unless `--ip`,
`--gw`, `--dns`, `--dhcp` or `--network-here` is given; an install from
before the file gets one written from its kernel parameters' `ip=`, `gw=`,
`dns=` and `dhcp=` ([The network](#the-network)). A kernel and its modules are only ever
replaced together, since a kernel refuses a module built against another
kernel interface. `/lib/modules` is the image's: a module of your own goes
somewhere else.

`nos-install rollback` swaps the current kernel, modules, hvboot, CloudHv,
licenses and `/VERSION` with the `.prev` ones -- back to an image older
than hvboot, CloudHv or the licenses, none of that. `nos-install status` says what is installed and what was,
eth0's settings, the kernel parameters, the microcode's revision and whether a boot is armed. `nos-install uninstall`
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

## The network

nos's network is `/etc/net.toml` on its root: eth0's address, its gateway,
its DNS server, or DHCP. `nos-install` writes it -- `install` and `disk`
whatever they are given, `update` when given `--ip`, `--gw`, `--dns`,
`--dhcp` or `--network-here` -- and nos reads it at every boot, once the
root is mounted and before anything goes out: DHCP, the netconsole, sshd.
It is the strict TOML of a VM's `vm.toml`:

```toml
# A network with no DHCP server
ip = "203.0.113.5/24"
gw = "203.0.113.1"
dns = "203.0.113.53"
```

or `dhcp = true`, which is also what a file without `ip` means: an address
asked of DHCP at every boot, and the lease's DNS server -- or, with `dns`
beside it, that server in its place. `dhcp = false` without an `ip` leaves
eth0 with no address until nos's `dhcp` command is run. Edited from inside
nos (`edit /etc/net.toml`, over SSH), it is taken at the next boot. `net`
says what eth0's settings are, and where they came from.

The file is taken whole or not at all. A key it does not have, a key given
twice, an `ip` that is not an address a host may have with its prefix
length -- or is its subnet's network or broadcast address -- a `gw` that is
not another address of that subnet, a `gw` with no `ip`, `dhcp = true`
beside an `ip`: each is refused, the boot says which and on which line,
and the kernel command line's `ip=`, `gw=`, `dns=` and `dhcp=` are taken in
its place -- `nos.cfg`'s own `dhcp=auto dns=on`, DHCP, on an installed
server, which on a network with no DHCP server leaves it unreachable. So
`nos-install` checks what it writes by the same rules and refuses what nos
would, before it writes anything.

The command line's words are what a root with no `/etc/net.toml` goes by:
an ISO's, QEMU's, a test's, a root that was not found
([Kernel parameters](kernel-parameters.md#networking)) -- and a kernel from
before the file. An install from before it had its network as `ip= gw=
dns=` in the grubenv's `nos_cmdline`; `update` writes the file from them,
and keeps them there for as long as a kernel on the partition -- the one
kept for a rollback, above all -- does not read the file: it looks for
`/etc/net.toml` in each kernel's image. The update after that, with both
kernels reading it, takes them out.

## What goes where

On the partition, which is nos's root:

| Path | From | |
|---|---|---|
| `/boot/kernel64.elf`, `.prev` | the image | the kernel GRUB boots, and the one before it |
| `/boot/kernel64.elf.next` | you, from inside nos | a kernel tried once |
| `/boot/grub/nos.cfg` | the image | the entries `nos` and `nos-next` |
| `/lib/modules/`, `/lib/modules.prev/` | the image | the kernel's modules, and the ones before them |
| `/lib/hvboot`, `.prev` | the image | the loader of a guest's own kernel, and the one before it |
| `/lib/CLOUDHV.fd`, `.prev` | the image | the CloudHv UEFI guest firmware, and the one before it |
| `/lib/licenses/`, `/lib/licenses.prev/` | the image | the licenses of the code the kernel, its modules and the firmware carry, and the ones before them |
| `/lib/panel/bundle`, `/lib/panel.prev` | the image, when released with a panel | the panel's bundle: its URL and SHA-256, which `api panel create` fetches it by |
| `/VERSION`, `.prev` | the image | what is installed, and what was |
| `/boot/ucode` | the server's `/lib/firmware/intel-ucode`, at every install and update | this CPU's microcode, which nos loads on every CPU as it boots ([Boot](boot.md#x86-64-the-application-processors)) |
| `/grubenv` | nos-install | `nos_cmdline`, this machine's kernel parameters; `nos_next`, the one-shot flag |
| `/etc/net.toml` | nos-install (`--ip`, `--gw`, `--dns`, `--dhcp`, `--network-here`; DHCP where none is given) | eth0's settings, read at every boot ([The network](#the-network)) |
| `/etc/rc` | the image, the first time | the boot script: the machine's after that (`rc` edits it) |
| `/etc/ssh/authorized_keys` | `--ssh-key` | the keys sshd lets in (`sshd allow` adds one from inside) |
| `/etc/ssh/ssh_host_ed25519_key` | sshd, at its first start | nos's host key |
| `/etc/api/` | `--panel`, and the host API at its first start | `panel.pass`, root's password's hash for the panel; the host API's key, its clients, api.toml ([The host API](api.md#the-folder)) |

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
- nos makes no symlinks or device files on its root, and leaves out of its
  tree the ones another system made there ([Filesystems](filesystems.md)).
- A server whose disks or network card nos has no driver for is not one this
  works on: see the first section.
- A disk of nos's own is one disk: nos reads no md mirror, so its root is
  not mirrored onto a second. Its sectors are 512 bytes, as BIOS boots them
  (a 4Kn disk is refused). Under UEFI the firmware's order is set, its
  network entries first still; under BIOS it is the firmware's setup's.
- The installer's dialog takes keys, not a password: nos's sshd lets in
  ssh-ed25519 keys alone.

`tests/image-test.py` is the test of all of this: it pours the image onto
a partition with `nos-install` under Linux, boots the disk through a GRUB
standing in for a server's, logs in with the key it gave, checks that no UDP
shell answers and that the next boot is the server's own, then updates,
rolls back and uninstalls; and it makes a whole disk nos's own and boots it
under SeaBIOS and OVMF. `tests/installer-test.py` boots the installer ISO,
built with answers, and installs with it ([Testing](testing.md)).
