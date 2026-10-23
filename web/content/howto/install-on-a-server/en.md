---
title: "Install nos on a dedicated server"
description: "Install the nos system image on a spare partition of a Linux server, boot it once through the server's own GRUB, and log in over SSH."
order: 5
updated: 2026-10-22
---

By the end of this guide, a bare-metal x86-64 server runs nos from its own partition. The kernel and all the modules come from a release's system image, the server's own GRUB boots them, and nos's SSH server lets you in with the key you gave it. The server's Linux stays installed and remains the default. nos boots once when you ask for it, and the boot after that is Linux again.

You need:

- an x86-64 server running Linux with GRUB 2, root access to it, and hardware that nos has drivers for (see [Check the server first](#check-the-server-first));
- a spare plain partition on it, at least 64 MiB;
- an `ssh-ed25519` key, because nos's sshd accepts no other type;
- a way to reset the server from outside, such as your hosting provider's reset button or API, in case nos does not come up.

We run nos this way on the project's own Hetzner servers: from its own partition, next to the server's Linux, booted once at a time. The full reference is [Install: nos on a server](/docs/install).

## How it stays safe

The install is designed so that a failed nos costs you a reset, not the server:

- **nos gets its own partition.** The partition holds an ext2 filesystem that nos mounts as its root. On the server's Linux, `nos-install` adds a single file, `/etc/grub.d/42_nos`, and regenerates `grub.cfg`. It touches nothing else.
- **The server's default boot entry does not change.** A flag, `nos_next=nos`, arms a boot of nos, that is, schedules it for the next start. The flag lives in a GRUB environment block on nos's partition, a small file of variables that GRUB can read and write. GRUB reads the flag, boots nos once, and **clears the flag before it loads anything**. The server's own first entry stays the fallback.
- **`reboot` inside nos goes back to Linux**, because the flag is already gone.
- **If nos never comes up, one reset brings Linux back.** A panic, a hang or a kernel that GRUB cannot load leaves nothing to undo.
- **If nos panics, it resets itself.** The image's GRUB entries pass `panic=10`, so the machine resets ten seconds after the panic report. After a one-shot boot, that reset brings up Linux.

## Check the server first

nos runs only on hardware it has drivers for, and on a server the network is the only console. `nos-install` checks each item below and tells you what it found. It refuses an unsuitable partition and warns about the rest, but it cannot fix any of them.

| You need | Why |
|---|---|
| An NVMe or virtio disk for nos's partition (virtio-scsi also works) | These are the only disks nos can read. On a SATA disk or behind a RAID controller, GRUB still loads the kernel through the firmware, but nos then finds no root filesystem. That means no modules, no `/etc/rc` and no sshd. |
| A network card that nos has a driver for: Realtek RTL8168 or RTL8125, Intel 82576, I210 or I211 (the `igb` driver), or virtio-net | With any other card, nos boots, but nobody can reach it. |
| DHCP on that network | nos has no static IP configuration. |
| A plain partition of at least 64 MiB (256 MiB is comfortable; guest disk images need more) | It can't be a member of an md RAID, an LVM volume or encrypted, because nos reads none of those. A server whose disks are mirrored end to end has no such partition. Leave room for one when you install the server (Hetzner's `installimage` takes a partition list), or create one from free space. |
| Linux that boots with GRUB 2, under BIOS or UEFI, with **Secure Boot off** | nos is a Multiboot2 kernel. With Secure Boot on, GRUB runs locked down and refuses to load such a kernel. |
| e2fsprogs, util-linux, GRUB's tools (`grub-editenv`, plus `update-grub` or `grub-mkconfig`), and squashfs support in the kernel or `unsquashfs` | `nos-install` runs these. Stock Ubuntu and Debian have them all. |

## Get the system image

What you install is the **system image**, `nos-x86_64.squashfs`. Every release tag builds one. Download it on the server and check it:

```sh
wget https://github.com/irqlevel/nos/releases/latest/download/nos-x86_64.squashfs
wget https://github.com/irqlevel/nos/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

What is inside:

| Path | What |
|---|---|
| `/boot/kernel64.elf` | the kernel |
| `/boot/grub/nos.cfg` | the kernel's GRUB entries, `nos` and `nos (next)`, with the command line the kernel was built to read |
| `/lib/modules/*.ko` | every module built together with that kernel, so each of them loads into it |
| `/etc/rc` | the default boot script: `insmod /lib/modules/sshd.ko`, then `sshd start` |
| `/nos-install` | the installer that matches this image |
| `/VERSION` | `version=`, `revision=`, `arch=`, `module_abi=` |

Nothing in the image is tied to a particular machine: it has no host key, no authorized key and no address. File owners, timestamps and modes are fixed, so a build produces the same bytes every time, and the image's sha256 identifies its content.

You can also build the image yourself. Run `make image` on Linux with the toolchain installed, or `scripts/build-image.sh` anywhere Docker runs (`--clean` builds everything from scratch, as a release does). Either way, `nos-x86_64.squashfs` ends up at the top of the tree. See [Build nos](/howto/build).

## Install it on a partition

As root on the server, mount the image and run the `nos-install` that comes inside it. Replace `/dev/nvme0n1p4` with your spare partition:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install install /dev/nvme0n1p4 --ssh-key /root/.ssh/authorized_keys
umount /mnt
```

`--ssh-key` takes a `.pub` file, an `authorized_keys` file or a key line, and you can give it more than once. If you pass the server's own `/root/.ssh/authorized_keys`, the keys that already log into Linux will log into nos too. From a file, `nos-install` takes the `ssh-ed25519` lines and tells you which lines it skipped: keys of other types, and lines with options in front of the key. It refuses a key line of another type given on its own, and a file with no `ssh-ed25519` key in it. The option itself is optional. But without a key, sshd lets nobody in, and if the UDP shell is off as well, there is no way into a booted nos at all.

`install` goes through these steps and stops at the first problem:

1. **Checks the image and the partition.** The image must contain its `/VERSION`, kernel and modules, and must be built for this machine's architecture. The partition must be a block device. It must not be a whole disk with partitions on it, must not be held by md, LVM or the device mapper, and must not be one of the running system's filesystems. `install` warns if the disk is not NVMe or virtio, if there is no network card that nos has a driver for, and if Secure Boot is on.
2. **Prepares the filesystem.** On an empty partition, it creates an ext2 filesystem labelled `nos`, in the format that nos writes. If the partition already has an ext2 filesystem that nos can write, everything on it is kept. Anything else is refused, unless you pass `--format`, which creates a new filesystem and erases what was there.
3. **Copies the system in.** It mounts the partition and puts the kernel, the modules, `nos.cfg` and `/VERSION` there. If the partition already had a kernel, modules and `/VERSION`, the old ones move to `.prev` together, so that a later `rollback` can bring them back. `/etc` belongs to the machine: the image's `/etc/rc` is copied only if there is none yet, and your keys are added to `/etc/ssh/authorized_keys`. On an Intel CPU, if the server has a microcode file for it in `/lib/firmware/intel-ucode`, that file goes to `/boot/ucode`, and GRUB loads it alongside the kernel. Without such a file (an AMD CPU, or no `intel-microcode` package), nos runs with the microcode that the firmware loaded.
4. **Writes `/grubenv` on the partition.** This file holds the kernel parameters for this machine and the one-shot flag. `e2fsck` checks the filesystem before and after the write.
5. **Adds the GRUB entry.** It writes `/etc/grub.d/42_nos`, which finds the partition by its UUID. Then it runs `update-grub` (or `grub-mkconfig`) and checks that `grub.cfg` now contains the new entry.

Options of `install` and `update`:

| Option | What it does |
|---|---|
| `--ssh-key KEY` | Adds a key that sshd lets in. |
| `--udpshell[=PORT]` | Turns on the UDP shell, on port 9000 or on PORT. **It stays off unless you pass this**, because the UDP shell runs any command for anyone who can send the machine a datagram ([UDP shell](/docs/udp-shell)). |
| `--netconsole IP:PORT` | Sends the kernel log over UDP to IP:PORT ([Netconsole](/docs/netconsole)). This is the only way to see a boot that dies after the network is up. |
| `--cmdline "ARGS"` | Adds kernel parameters ([Kernel parameters](/docs/kernel-parameters)). |
| `--format` | Creates a new filesystem on the partition, even over an existing one. |
| `--boot-once`, `--reboot` | Arms one boot of nos for the next start. `--reboot` also reboots right away. |
| `--image FILE` | Names the image, for when the `nos-install` you run is not the one inside it. |

## Boot nos once

Arm one boot of nos and reboot the server:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install boot --reboot
```

Instead of running `boot`, you can also pass `--reboot` to `install` or `update`.

`boot` sets `nos_next=nos` in the partition's `/grubenv`. At the next boot, GRUB reads the flag through `42_nos`. It makes the `nos` entry the default for this one boot, keeps the server's first entry as the fallback, and clears the flag before it loads anything.

The kernel command line is `root=UUID=<partition-uuid> dhcp=auto dns=on panic=10`, with the UUID of nos's partition, plus whatever this machine was given with `--udpshell`, `--netconsole` and `--cmdline`. nos mounts the partition read-write as its root, gets an address over DHCP and runs `/etc/rc`. The image's `/etc/rc` loads sshd and starts it. On the first boot, sshd generates a host key from the kernel's entropy pool and saves it in `/etc/ssh/ssh_host_ed25519_key` for all later boots. It lets in the keys listed in `/etc/ssh/authorized_keys`.

About a minute after the reboot, run this from your workstation:

```sh
ssh -o UserKnownHostsFile=~/.ssh/known_hosts.nos root@<server> version
```

It prints `nos <version> (<revision>)`. The command uses a separate known-hosts file because nos has its own host key. Your `known_hosts` already holds a different key for that address, the Linux one. If you want one host key for both systems, copy Linux's `/etc/ssh/ssh_host_ed25519_key` to the same path on nos's partition before the first boot. nos's sshd reads OpenSSH's key format.

When you are done, run `reboot` in nos to take the server back to its Linux.

## Or deploy from your workstation

`scripts/nos-deploy.py` does all of the above from the machine that has the image. It logs into the server's Linux as root over SSH:

```sh
scripts/nos-deploy.py root@<server> install /dev/nvme0n1p4 --ssh-key ~/.ssh/id_ed25519.pub
scripts/nos-deploy.py root@<server>
```

The first command installs nos. The second has no command, which means `update`; that is what you run every time after. The script:

1. copies the image to the server, checks its sha256 there and mounts it;
2. runs the `nos-install` inside the image with `--boot-once`, then reboots the server;
3. waits until nos's own sshd answers on port 22 with the banner `SSH-2.0-nos_sshd`, where OpenSSH answered before;
4. logs in and checks that the running kernel is the one from the image.

Its options:

| Option | What it does |
|---|---|
| `--image FILE` | Uses that image instead of the default, `nos-x86_64.squashfs` at the top of the tree (built by `make image` or `scripts/build-image.sh`). |
| `--no-boot` | Installs the image on the server, but does not reboot into it. |
| `--wait SECONDS` | How long nos has to come up (600 by default). |
| `--ssh-key`, `--udpshell[=PORT]`, `--netconsole IP:PORT`, `--cmdline ARGS`, `--format` | Passed through to `nos-install`. |

## Check what is installed

Run `status` from the mounted image. It takes no options:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install status
```

It shows the GRUB entry and the partition, what is installed now and what was installed before, the kernel parameters, whether a boot is armed, how many SSH keys there are, and the microcode revision. On an AMD server right after the first install, the output looks like this:

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

To install a newer image over the current one, download it as in [Get the system image](#get-the-system-image), then run:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install update --reboot
```

`update` finds the partition through `42_nos`, so you don't need to name it again. It copies in the new kernel, modules and `nos.cfg`. The old kernel, modules and `/VERSION` move to `.prev` together, and the microcode is copied again from the server's `/lib/firmware`. `/etc` (`rc`, the keys, the host key) and everything else on the partition stay as they are. The kernel parameters stay too, unless you pass `--udpshell`, `--netconsole` or `--cmdline`. Any of those replaces all the parameters, and `--cmdline ""` clears them.

A kernel and its modules are always replaced together, because a kernel refuses a module built against a different kernel interface. `/lib/modules` belongs to the image, so keep your own modules somewhere else.

Two more commands, also run from the mounted image:

- `/mnt/nos-install rollback` swaps the current kernel, modules and `/VERSION` with the `.prev` ones.
- `/mnt/nos-install uninstall` deletes `42_nos` and regenerates GRUB's configuration, which takes nos out of the menu. It leaves the partition alone.

### Try a kernel from inside nos

From a running nos, you can try a kernel from a release before it replaces the installed one. Run this in nos's shell:

```sh
wget https://github.com/irqlevel/nos/releases/download/<tag>/kernel-x86_64.elf /boot/kernel64.elf.next
sha256 /boot/kernel64.elf.next
grubenv /grubenv nos_next=nos-next
reboot
```

Compare what `sha256` prints with the release's `SHA256SUMS`. After the reboot, the `nos (next)` entry boots once. It brings no modules of its own. The ones in `/lib/modules` belong to the running kernel, and the new kernel refuses them if their kernel interface differs. A whole image is updated from Linux.

## Get back to Linux

- **From a running nos:** run `reboot`. The one-shot flag is gone, so GRUB boots the server's default.
- **If nos hangs or never answers:** reset the server from outside. GRUB cleared the flag before it loaded nos, so Linux comes up.
- **After a panic:** wait. `panic=10` resets the machine ten seconds after the report.
- **To take nos out of the menu:** run `nos-install uninstall` from Linux, as above.

If you want to stay in nos across reboots, make it the server's default: set `GRUB_DEFAULT=nos` in `/etc/default/grub` and run `update-grub`.

> Once nos is the default, every reset boots nos again, even a nos that never comes up. Then the only way out is your hosting provider's rescue system.

## Troubleshooting

**`nos-deploy: <server> came back with <banner>, not nos -- GRUB did not boot it`.** The server came back to Linux instead of nos. Check that Secure Boot is off. Also look for an entry of the server's own with the id `nos` or `nos-next`, such as a hand-written one in `/etc/grub.d/40_custom`. Such an entry comes first in `grub.cfg`, so it is the one GRUB boots. `nos-install` warns when it finds one.

**`no nos on <address>:22 after 600 s`.** Either nos died, or it came up but can't be reached. Reset the server to get Linux back, then find out why:

- Reinstall with `--netconsole IP:PORT` pointing at a machine you control, and run `python3 scripts/netconsole.py -p PORT` there. The kernel log, including any panic report, arrives over UDP ([Netconsole](/docs/netconsole)).
- If the boot dies before its network is up, use `disklog=on`, which writes the kernel log to an area on a disk. That area has to be prepared with `scripts/disklog.py format` ([Kernel parameters](/docs/kernel-parameters)).

**`ssh` refuses the host key or warns that it changed.** nos's host key is not the one the server's Linux uses. Use a separate known-hosts file for nos, as in [Boot nos once](#boot-nos-once), or copy the Linux host key onto the partition.

**nos's sshd rejects your key.** It accepts only `ssh-ed25519` keys. Add one with `--ssh-key` on the next `update`, or with `sshd allow` from inside nos ([sshd](/docs/sshd)).

**`WARNING: ... is not on an NVMe or a virtio disk`.** GRUB will load the kernel from that disk, but nos will not find its root there. That means no sshd and no way in. Put the partition on an NVMe or virtio disk.

**`... is in use by ... (RAID, LVM, device mapper)`.** nos needs a plain partition. It can read none of these.

**`... has a <type> filesystem on it, which nos does not take`.** Pass `--format` only if you are ready to lose what is on that partition.

**`/etc/rc on it runs 'insmod ...': that module is not the image's`.** After an update, a module loaded from outside `/lib/modules` comes from a different build, and the new kernel refuses it. Rebuild it against the new kernel.

**`e2fsck finds ... damaged`.** Run `e2fsck -f <partition>` from Linux to repair the filesystem, then run `nos-install` again.

## Next steps

- [Connect to a running nos](/howto/connect): SSH, the UDP shell, and what you can do once you are in.
- [Boot a Linux guest](/howto/linux-guest): on the server, the hypervisor module is already at `/lib/modules/hv.ko`.
- [Write a loadable module](/howto/write-a-module), and keep your own modules outside `/lib/modules`.
- [Install: nos on a server](/docs/install) is the full reference, and [Real hardware](/docs/real-hardware) describes the machines nos runs on.
