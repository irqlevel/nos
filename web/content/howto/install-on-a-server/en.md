---
title: "Install nos on a dedicated server"
description: "Pour the nos system image onto a spare partition of a Linux server, boot it once through the server's own GRUB, and log in over SSH."
order: 5
updated: 2026-10-22
---

By the end of this guide a bare-metal x86-64 server runs nos from a partition of its own: the kernel and every module from a release's system image, booted through the server's own GRUB, with nos's SSH server letting in the key you gave it. The server's Linux stays installed and stays the default: nos boots once when you ask for it, and the boot after that is Linux again.

You need:

- an x86-64 server running Linux with GRUB 2, root access to it, and hardware nos has drivers for (see [Check the server first](#check-the-server-first));
- a spare plain partition on it, at least 64 MiB;
- an `ssh-ed25519` key, because nos's sshd takes no other kind;
- a way to reset the server from outside, such as your hosting provider's reset button or API, in case nos does not come up.

The project's own Hetzner servers run nos this way: from a partition of its own, booted once at a time beside the server's Linux. The reference page is [Install: nos on a server](/docs/install).

## How it stays safe

The install is built so that a nos that fails costs you a reset, not a server:

- **nos gets a partition of its own.** It holds an ext2 filesystem that nos mounts as its root. On the server's Linux, `nos-install` adds one file, `/etc/grub.d/42_nos`, and regenerates `grub.cfg`. Nothing else is touched.
- **The server's default boot entry does not change.** A boot of nos is armed by a flag, `nos_next=nos`, in a GRUB environment block on nos's partition. GRUB reads the flag, boots nos once, and **clears the flag before it loads anything**. The server's own first entry stays the fallback.
- **`reboot` inside nos goes back to Linux**, because the flag is already gone.
- **A nos that never comes up is one reset away from Linux.** A panic, a hang or a kernel GRUB cannot load leaves nothing to undo.
- **A nos that panics resets itself.** The image's GRUB entries pass `panic=10`, so the machine resets ten seconds after the panic report, and after a one-shot boot that means Linux.

## Check the server first

nos runs only on hardware it has drivers for, and a server's only console is its network. `nos-install` looks for each of the following and tells you what it found: it refuses an unsuitable partition and warns about the rest. It cannot fix any of them.

| You need | Why |
|---|---|
| An NVMe or virtio disk for nos's partition (virtio-scsi also works) | These are the only disks nos reads. On a SATA disk or behind a RAID controller, GRUB still loads the kernel through the firmware, but nos finds no root on it: no modules, no `/etc/rc`, no sshd. |
| A network card nos drives: Realtek RTL8168 or RTL8125, Intel 82576, I210 or I211 (the `igb` driver), or virtio-net | With any other card, nos boots and nobody can reach it. |
| DHCP on that network | nos has no static IP configuration. |
| A plain partition of at least 64 MiB (256 MiB is comfortable; guest disk images need more) | It can't be an md RAID member, an LVM volume or encrypted, because nos reads none of those. A server mirrored end to end has no such partition, so leave room for one when you install the server (Hetzner's `installimage` takes a partition list) or create one from free space. |
| Linux booting with GRUB 2, BIOS or UEFI, with **Secure Boot off** | nos is a Multiboot2 kernel, and GRUB under Secure Boot's lockdown refuses to load one. |
| e2fsprogs, util-linux, GRUB's tools (`grub-editenv`, plus `update-grub` or `grub-mkconfig`), and squashfs support in the kernel or `unsquashfs` | `nos-install` runs these. Stock Ubuntu and Debian have them all. |

## Get the system image

The unit you install is the **system image**, `nos-x86_64.squashfs`. Every release tag builds one. On the server:

```sh
wget https://github.com/irqlevel/nos/releases/latest/download/nos-x86_64.squashfs
wget https://github.com/irqlevel/nos/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

What is inside:

| Path | What |
|---|---|
| `/boot/kernel64.elf` | the kernel |
| `/boot/grub/nos.cfg` | its GRUB entries, `nos` and `nos (next)`, with the command line the kernel was built to read |
| `/lib/modules/*.ko` | every module built with that kernel, so every one of them loads into it |
| `/etc/rc` | the default boot script: `insmod /lib/modules/sshd.ko`, then `sshd start` |
| `/nos-install` | the installer that matches this image |
| `/VERSION` | `version=`, `revision=`, `arch=`, `module_abi=` |

Nothing in the image belongs to a particular machine: it has no host key, no authorized key and no address. Owners, times and modes are fixed, so a build makes the same bytes every time and the image's sha256 identifies its content.

To build the image yourself instead, run `make image` on Linux with the toolchain installed, or `scripts/build-image.sh` anywhere Docker runs (`--clean` builds from nothing, as a release does). Either leaves `nos-x86_64.squashfs` at the top of the tree. See [Build nos](/howto/build).

## Pour it onto a partition

As root on the server, mount the image and run the `nos-install` inside it. Replace `/dev/nvme0n1p4` with your spare partition:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install install /dev/nvme0n1p4 --ssh-key /root/.ssh/authorized_keys
umount /mnt
```

`--ssh-key` takes a `.pub` file, an `authorized_keys` file or a key line, and you can give it more than once. Passing the server's own `/root/.ssh/authorized_keys` lets into nos the keys that already log into Linux. From a file, `nos-install` takes the `ssh-ed25519` lines and says which lines it left out: keys of another kind, and lines with options in front of the key. A key line of another kind given on its own, or a file with no `ssh-ed25519` key in it, is refused. The option itself is optional, but without a key sshd lets nobody in, and with the UDP shell off as well, a booted nos has no way in at all.

`install` works through these steps and stops at the first thing that is wrong:

1. **Checks the image and the partition.** The image must have its `/VERSION`, kernel and modules, and must be for this machine's architecture. The partition must be a block device, must not be a whole disk with partitions on it, must not be held by md, LVM or the device mapper, and must not be one of the running system's filesystems. `install` warns when the disk is not NVMe or virtio, when no network card nos drives is present, and when Secure Boot is on.
2. **Prepares the filesystem.** An empty partition gets an ext2 filesystem labelled `nos`, in the format nos writes. A partition with an ext2 nos can write keeps everything on it. Anything else is refused, unless you pass `--format`, which makes a new filesystem and loses what was there.
3. **Copies the system in.** It mounts the partition and puts the kernel, the modules, `nos.cfg` and `/VERSION` there. If the partition already had them, the old three move to `.prev` together, for a later `rollback`. `/etc` belongs to the machine: the image's `/etc/rc` goes in only where there is none, and your keys are added to `/etc/ssh/authorized_keys`. On an Intel CPU, if the server has a microcode file for it in `/lib/firmware/intel-ucode`, that file goes to `/boot/ucode`, and GRUB loads it beside the kernel. Without one (an AMD CPU, or no `intel-microcode` package), nos runs the microcode the firmware loaded.
4. **Writes `/grubenv` on the partition.** It holds this machine's kernel parameters and the one-shot flag. `e2fsck` checks the filesystem before the write and after it.
5. **Adds the GRUB entry.** It writes `/etc/grub.d/42_nos`, which finds the partition by its UUID, runs `update-grub` (or `grub-mkconfig`), and checks that `grub.cfg` now contains it.

The options of `install` and `update`:

| Option | What it does |
|---|---|
| `--ssh-key KEY` | Adds a key sshd lets in. |
| `--udpshell[=PORT]` | Turns on the UDP shell, on port 9000 or PORT. **Off unless given**, because it runs any command for anyone who can send the machine a datagram ([UDP shell](/docs/udp-shell)). |
| `--netconsole IP:PORT` | Sends the kernel log over UDP to IP:PORT ([Netconsole](/docs/netconsole)). It is the one way to see a boot that dies after the network is up. |
| `--cmdline "ARGS"` | Adds kernel parameters ([Kernel parameters](/docs/kernel-parameters)). |
| `--format` | Makes a new filesystem on the partition, even over an existing one. |
| `--boot-once`, `--reboot` | Arms one boot of nos for the next boot; `--reboot` also reboots now. |
| `--image FILE` | Names the image, when this `nos-install` is not the one inside it. |

## Boot nos once

Arm one boot and reboot. You can also pass `--reboot` to `install` or `update` instead:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install boot --reboot
```

`boot` sets `nos_next=nos` in the partition's `/grubenv`. At the next boot GRUB reads it through `42_nos`, makes the `nos` entry the default for this one boot with the server's first entry as the fallback, and clears the flag before it loads anything.

The kernel's command line is `root=UUID=<partition-uuid> dhcp=auto dns=on panic=10` (the UUID of nos's partition), plus whatever this machine was given with `--udpshell`, `--netconsole` and `--cmdline`. nos mounts the partition read-write as its root, takes an address by DHCP and runs `/etc/rc`. The image's `/etc/rc` loads sshd and starts it. The first time, sshd makes a host key from the kernel's entropy pool and keeps it in `/etc/ssh/ssh_host_ed25519_key` for every later boot. It lets in the keys in `/etc/ssh/authorized_keys`.

About a minute after the reboot, from your workstation:

```sh
ssh -o UserKnownHostsFile=~/.ssh/known_hosts.nos root@<server> version
```

It prints `nos <version> (<revision>)`. The separate known-hosts file is there because nos has a host key of its own, not the Linux one your `known_hosts` already has for that address. To have one host key under both systems, copy Linux's `/etc/ssh/ssh_host_ed25519_key` to the same path on nos's partition before the first boot; nos's sshd reads OpenSSH's format.

When you are done, `reboot` in nos takes the server back to its Linux.

## Or deploy from your workstation

`scripts/nos-deploy.py` does all of the above from the machine the image is on, over SSH to the server's Linux as root:

```sh
scripts/nos-deploy.py root@<server> install /dev/nvme0n1p4 --ssh-key ~/.ssh/id_ed25519.pub
scripts/nos-deploy.py root@<server>
```

The first command installs. The second, with no command, means `update`, which is what you run every time after. The script:

1. copies the image to the server, checks its sha256 there and mounts it;
2. runs the `nos-install` inside the image with `--boot-once`, then reboots the server;
3. waits for nos's own sshd to answer on port 22 with the banner `SSH-2.0-nos_sshd`, where OpenSSH answered before;
4. logs in and checks that the running kernel is the image's.

Its options:

| Option | What it does |
|---|---|
| `--image FILE` | Uses that image instead of the default, `nos-x86_64.squashfs` at the top of the tree (from `make image` or `scripts/build-image.sh`). |
| `--no-boot` | Puts the image on the server and stops before the reboot. |
| `--wait SECONDS` | Sets how long nos has to come up (600 by default). |
| `--ssh-key`, `--udpshell[=PORT]`, `--netconsole IP:PORT`, `--cmdline ARGS`, `--format` | Pass through to `nos-install`. |

## Check what is installed

`status` works from the mounted image and takes no options:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install status
```

It names the GRUB entry and the partition, what is installed and what was installed before, the kernel parameters, whether a boot is armed, how many SSH keys there are and the microcode's revision. On an AMD server right after the first install it looks like this:

```text
GRUB entry:  /etc/grub.d/42_nos, UUID <uuid>
partition:   /dev/nvme0n1p4
installed:   nos <version> (<revision>)
previous:    none
parameters:  none
next boot:   this machine's default
ssh keys:    1
microcode:   none; nos runs firmware's
```

## Update, roll back, uninstall

To install a newer image over the current one, download it as in [Get the system image](#get-the-system-image), then:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install update --reboot
```

`update` finds the partition through `42_nos`, so you don't name it again. The new kernel, modules and `nos.cfg` go in. The old kernel, modules and `/VERSION` move to `.prev` together, and the microcode is copied again from the server's `/lib/firmware`. `/etc` (`rc`, the keys, the host key) and everything else on the partition stay. The kernel parameters stay too, unless you pass `--udpshell`, `--netconsole` or `--cmdline`. Any of those replaces all the parameters, and `--cmdline ""` clears them.

A kernel and its modules are only ever replaced together, because a kernel refuses a module built against another kernel interface. `/lib/modules` belongs to the image, so put a module of your own somewhere else.

The other commands, each run from the mounted image:

- `/mnt/nos-install rollback` swaps the current kernel, modules and `/VERSION` with the `.prev` ones.
- `/mnt/nos-install uninstall` deletes `42_nos`, regenerates GRUB's configuration so nos is out of the menu, and leaves the partition alone.

### Try a kernel from inside nos

A running nos can try a kernel from a release before it replaces the installed one. Run this in nos's shell:

```sh
wget https://github.com/irqlevel/nos/releases/download/<tag>/kernel-x86_64.elf /boot/kernel64.elf.next
sha256 /boot/kernel64.elf.next
grubenv /grubenv nos_next=nos-next
reboot
```

Compare what `sha256` prints with the release's `SHA256SUMS`. The `nos (next)` entry then boots once. It brings no modules: the ones in `/lib/modules` belong to the running kernel, and the new one refuses them if their kernel interface differs. Update a whole image from Linux.

## Get back to Linux

- **From a running nos:** run `reboot`. The one-shot flag is gone, so GRUB boots the server's default.
- **From a nos that hangs or never answers:** reset the server from outside. GRUB had cleared the flag before loading nos, so Linux comes up.
- **After a panic:** wait. `panic=10` resets the machine ten seconds after the report.
- **To take nos out of the menu:** run `nos-install uninstall` from Linux, as above.

If you want to stay in nos across reboots, you can make it the server's default: set `GRUB_DEFAULT=nos` in `/etc/default/grub` and run `update-grub`.

> Once nos is the default, a nos that does not come up comes up again after every reset, and the only way out is your hosting provider's rescue system.

## Troubleshooting

**`nos-deploy: <server> came back with <banner>, not nos -- GRUB did not boot it`.** The server came back to Linux instead of nos. Check that Secure Boot is off. Also look for an entry of the server's own with the id `nos` or `nos-next`, such as a hand-made one in `/etc/grub.d/40_custom`: it comes first in `grub.cfg` and is the one GRUB boots. `nos-install` warns when it finds one.

**`no nos on <address>:22 after 600 s`.** nos either died or came up unreachable. Reset the server to get Linux back, then find out why:

- Reinstall with `--netconsole IP:PORT` pointing at a machine you control, and run `python3 scripts/netconsole.py -p PORT` there. The kernel log, panic report included, arrives over UDP ([Netconsole](/docs/netconsole)).
- For a boot that dies before its network is up, use `disklog=on`. It needs a disk area prepared with `scripts/disklog.py format` ([Kernel parameters](/docs/kernel-parameters)).

**`ssh` refuses the host key or warns that it changed.** nos's host key is not the server's Linux one. Use a known-hosts file of its own, as in [Boot nos once](#boot-nos-once), or copy the Linux host key onto the partition.

**nos's sshd rejects your key.** It takes `ssh-ed25519` keys only. Add one with `--ssh-key` on the next `update`, or with `sshd allow` from inside nos ([sshd](/docs/sshd)).

**`WARNING: ... is not on an NVMe or a virtio disk`.** GRUB will load the kernel from it, but nos will not find its root there, so there is no sshd and no way in. Put the partition on an NVMe or virtio disk.

**`... is in use by ... (RAID, LVM, device mapper)`.** nos needs a plain partition; it reads none of those.

**`... has a <type> filesystem on it, which nos does not take`.** Pass `--format` only if you mean to lose what is on that partition.

**`/etc/rc on it runs 'insmod ...': that module is not the image's`.** After an update, a module loaded from outside `/lib/modules` is of another build, and the new kernel refuses it. Rebuild it against the new kernel.

**`e2fsck finds ... damaged`.** Run `e2fsck -f <partition>` from Linux to repair it, then run `nos-install` again.

## Next steps

- [Connect to a running nos](/howto/connect): SSH and the UDP shell, and what you can do once you are in.
- [Boot a Linux guest](/howto/linux-guest): on the server, the hypervisor module is already at `/lib/modules/hv.ko`.
- [Write a loadable module](/howto/write-a-module), and keep your own modules outside `/lib/modules`.
- [Install: nos on a server](/docs/install), the full reference, and [Real hardware](/docs/real-hardware), the machines nos runs on.
