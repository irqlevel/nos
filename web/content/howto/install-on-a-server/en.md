---
title: "Install nos on a dedicated server"
description: "Put nos on a whole disk of a dedicated server with one command in the provider's rescue system or from the installer ISO, or next to the server's Linux on a spare partition, and log in over SSH."
order: 5
updated: 2026-11-18
---

There are two ways to put nos on a bare-metal x86-64 server.

- **A whole disk of its own.** nos takes one disk: a GPT, GRUB for both BIOS and UEFI, a root of 10 GiB unless you choose another size, and the rest of the disk left empty for virtual machines and storage. You install it with one command in your provider's rescue system, or from the installer ISO. This is the quick way.
- **A partition next to the server's Linux.** The server's Linux stays installed and stays the default. nos boots once when you ask for it, and the boot after that is Linux again. This is the careful way to try nos on a machine you also need for something else.

Either way, nos's SSH server lets you in with your key. The full reference is [Install: nos on a server](/docs/install).

## Check the server first

nos runs only on hardware it has drivers for, and on most servers the network is the only console. The installers check each item below and tell you what they found.

| You need | Why |
|---|---|
| A disk nos reads: NVMe, virtio (virtio-scsi too), or SATA with the controller in AHCI or IDE mode | Behind a RAID controller or on USB, GRUB still loads the kernel through the firmware, but nos then finds no root filesystem. That means no modules, no `/etc/rc` and no sshd. A whole disk is refused there; a partition gets a warning. |
| A network card nos drives: Realtek RTL8168 or RTL8125, Intel 82576, I210, I211 or I350 (the `igb` driver), Intel 82574L (the `e1000e` driver), or virtio-net | With any other card, nos boots, but nobody can reach it. nos puts its address on the first of these cards in PCI order, so the cable should be in that port. |
| An address: DHCP, or a static one | nos asks for an address by DHCP, or takes a static one from `/etc/net.toml` on its root, which the installers write. Many dedicated servers have no DHCP; the installers offer the address the server already has. |
| Secure Boot off | nos is a Multiboot2 kernel. With Secure Boot on, GRUB runs locked down and refuses to load it. |
| An `ssh-ed25519` key | nos's sshd accepts no other key type. |

## A whole disk, from the rescue system

Boot the server into your provider's rescue system: a Linux started from the network, with the server's disks left alone. Log in over SSH as root and run:

```sh
curl -fsSL https://nos-os.tech/install.sh | sh
```

The script downloads the system image of the newest release, checks it against the release's `SHA256SUMS`, mounts it and starts the installer's dialog. The dialog asks:

1. **Which disk.** You see every disk with its model, serial number and size, what it holds, and whether nos can read it. Everything on the disk you choose is erased.
2. **How big the root is.** 10G unless you change it. The rest of the disk becomes an empty partition, `nos-data`, for virtual machines' disks and NBS.
3. **The network.** It offers the rescue system's own address, gateway and DNS server, which on a dedicated server are the server's. You can also take what a Linux already on the disks had, type a static address, or choose DHCP.
4. **The SSH keys.** The keys root logs into the rescue system with, a pasted key, a URL, or a GitHub user's keys.

Then it shows what it will write and asks you to type the disk's name before it writes anything. Under UEFI it puts nos first in the firmware's boot order, right after the network boot entries, so the provider's rescue system keeps working. If the provider's panel still has the server booting into rescue, switch that back, then reboot. About a minute later:

```sh
ssh -o UserKnownHostsFile=~/.ssh/known_hosts.nos root@<server> version
```

It prints nos's version. nos has a host key of its own, which is why the command keeps it in a separate known-hosts file.

### Without the dialog

Put the installer's arguments after `sh -s --`:

```sh
curl -fsSL https://nos-os.tech/install.sh | sh -s -- disk /dev/sdb --wipe --network-here --ssh-key /root/.ssh/authorized_keys
```

`--network-here` gives nos the rescue system's address, gateway and DNS server. `--wipe` is required when the disk holds anything. `--root-size 20G` changes the root's size, and `--dry-run` shows what would be written without writing it. On a server with several disks, name the disk by its `/dev/disk/by-id/` path, so that the one erased is the one you meant.

The same command updates nos on that disk later, keeping its keys, host key and settings:

```sh
curl -fsSL https://nos-os.tech/install.sh | sh -s -- update
```

### By hand

If you would rather not pipe a script into a shell, this is what it does:

```sh
wget https://github.com/irqlevel/nos/releases/latest/download/nos-x86_64.squashfs
wget https://github.com/irqlevel/nos/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-setup
umount /mnt
```

`/mnt/nos-setup` is the dialog. `/mnt/nos-install disk ...` with the arguments above does the same without it.

## A whole disk, from the installer ISO

Many providers install dedicated servers from an ISO. Download [nos-installer-x86_64.iso](https://github.com/irqlevel/nos/releases/latest/download/nos-installer-x86_64.iso), mount it as virtual media through the server's IPMI, iDRAC or iLO, or give that link to the provider's install-from-ISO, and boot the server from it. Its menu:

| Entry | What it does |
|---|---|
| Install nos | the dialog on the screen |
| Install nos on ttyS0 or ttyS1 | the dialog on a serial port; IPMI Serial over LAN is often ttyS1 |
| Try nos | boots nos straight from the ISO and writes nothing; the screen shows which disks and network cards nos found |

The dialog is the same as in the rescue system. When it is done, take the ISO out of the virtual media and reboot.

Some providers run the ISO in a virtual machine on top of the server's disks; Selectel's install-from-image does. The installer notices and says so. Such a virtual machine can see neither the server's network nor its firmware, so the installer offers the address a Linux already on the disks had, and at the end asks you to set the server's boot order from the provider's rescue system. This command does it:

```sh
curl -fsSL https://nos-os.tech/install.sh | sh -s -- boot --default
```

## Next to Linux, on a spare partition

This way, nos needs a plain partition of at least 64 MiB (256 MiB is comfortable; a VM made from a cloud image takes one to two GiB to begin with). It can't be a member of an md RAID, an LVM volume or encrypted, because nos reads none of those. The server's Linux must boot with GRUB 2 and have e2fsprogs, util-linux, GRUB's tools (`grub-editenv`, plus `update-grub` or `grub-mkconfig`) and squashfs support. Stock Ubuntu and Debian have them all.

### How it stays safe

The install is designed so that a failed nos costs you a reset, not the server:

- **nos gets its own partition.** It holds an ext4 filesystem that nos mounts as its root. On the server's Linux, `nos-install` adds a single file, `/etc/grub.d/42_nos`, and regenerates `grub.cfg`. It touches nothing else.
- **The server's default boot entry does not change.** A flag, `nos_next=nos`, schedules one boot of nos. GRUB reads it, boots nos once, and **clears the flag before it loads anything**, so the server's own first entry stays the fallback.
- **`reboot` inside nos goes back to Linux**, and if nos never comes up, one reset brings Linux back.
- **If nos panics, it resets itself** ten seconds after the panic report (`panic=10`), and that reset brings up Linux.

### Install and boot it once

Download and check the image as in [By hand](#by-hand), then, as root, with your spare partition in place of `/dev/nvme0n1p4`:

```sh
mount -o loop,ro nos-x86_64.squashfs /mnt
/mnt/nos-install install /dev/nvme0n1p4 --ssh-key /root/.ssh/authorized_keys
/mnt/nos-install boot --reboot
```

`--ssh-key` takes a `.pub` file, an `authorized_keys` file or a key line, and can be given more than once. From a file, `nos-install` takes the `ssh-ed25519` lines and tells you which lines it skipped. Without a key, sshd lets nobody in.

`install` checks the image and the partition, makes an ext4 filesystem on an empty partition (one with an ext4 or ext2 nos can write keeps everything on it, anything else is refused unless you pass `--format`), copies in the kernel, the modules and the rest of the system, adds your keys, writes the network to `/etc/net.toml`, and adds the GRUB entry. It stops at the first problem. `boot --reboot` arms the one-shot boot and reboots. About a minute later, log in as in the rescue section above.

Options of `install` and `update`:

| Option | What it does |
|---|---|
| `--ssh-key KEY` | Adds a key that sshd lets in. |
| `--network-here` | Gives nos the address, gateway and DNS server this Linux has, for a network with no DHCP. |
| `--ip A.B.C.D/N`, `--gw A.B.C.D`, `--dns A.B.C.D` | Gives nos a static address, its gateway and its DNS server, for a network with no DHCP. |
| `--dhcp` | Has nos ask DHCP for its address at every boot. Without any of the network's options, `install` writes this. |
| `--cmdline "ARGS"` | Adds kernel parameters, such as `deadman=600` ([Kernel parameters](/docs/kernel-parameters)). The network is not among them: it is the options above. |
| `--netconsole IP:PORT` | Sends the kernel log over UDP to IP:PORT ([Netconsole](/docs/netconsole)), the only way to see a boot that dies after the network is up. |
| `--udpshell[=PORT]` | Turns on the UDP shell. **It stays off unless you pass this**, because it runs any command for anyone who can send the machine a datagram ([UDP shell](/docs/udp-shell)). |
| `--format` | Creates a new filesystem on the partition, even over an existing one. |
| `--boot-once`, `--reboot` | Arms one boot of nos for the next start; `--reboot` also reboots right away. |

### Or deploy from your workstation

`scripts/nos-deploy.py` does all of the above from the machine that has the image, over SSH to the server's Linux as root:

```sh
scripts/nos-deploy.py root@<server> install /dev/nvme0n1p4 --ssh-key ~/.ssh/id_ed25519.pub
scripts/nos-deploy.py root@<server>
```

The first command installs nos. The second, with no command, runs `update`; that is what you run every time after. The script copies the image over, checks its sha256 there, installs it with `--boot-once`, reboots the server, waits until nos's own sshd answers on port 22, and checks that the running kernel is the image's.

### Status, update, rollback, uninstall

All of these run from the mounted image on the server's Linux:

- `/mnt/nos-install status` shows the partition, what is installed and what was before, the kernel parameters, whether a boot is armed, the number of SSH keys, and the microcode.
- `/mnt/nos-install update --reboot` installs a newer image. The old kernel, modules and `/VERSION` move to `.prev` together; `/etc` (the keys, the host key, `rc`) and the kernel parameters stay, unless you pass new ones.
- `/mnt/nos-install rollback` swaps the current system with the `.prev` one.
- `/mnt/nos-install uninstall` takes nos out of GRUB's menu and leaves the partition alone.

To stay in nos across reboots, set `GRUB_DEFAULT=nos` in `/etc/default/grub` and run `update-grub`.

> Once nos is the default, every reset boots nos again, even a nos that never comes up. Then the only way out is your hosting provider's rescue system.

### Get back to Linux

- **From a running nos:** run `reboot`. The one-shot flag is gone, so GRUB boots the server's default.
- **If nos hangs or never answers:** reset the server from outside. GRUB cleared the flag before it loaded nos, so Linux comes up.
- **After a panic:** wait. `panic=10` resets the machine ten seconds after the report.

## Troubleshooting

**No answer on port 22.** nos either died or came up but can't be reached. Reinstall with `--netconsole IP:PORT` pointing at a machine you control and run `python3 scripts/netconsole.py -p PORT` there: the kernel log, including any panic report, arrives over UDP ([Netconsole](/docs/netconsole)). On a network with no DHCP, check that nos was given the server's address (`status` shows the parameters), and that the cable is in the first port of a card nos drives.

**`ssh` refuses the host key or warns that it changed.** nos has its own host key. Use a separate known-hosts file for it, as above.

**nos's sshd rejects your key.** It accepts only `ssh-ed25519` keys. Add one with `--ssh-key` on the next `update`, or with `sshd allow` from inside nos ([sshd](/docs/sshd)).

**`... is not on an NVMe, a virtio or a SATA disk`, or `... on a SATA controller nos does not drive`.** GRUB would load the kernel from that disk, but nos would not find its root there. Choose another disk, or switch the controller from RAID to AHCI in the firmware's setup.

**`... is in use by ... (RAID, LVM, device mapper)`.** nos needs a plain partition or a whole disk. It can read none of these.

**`nos-deploy: <server> came back with <banner>, not nos -- GRUB did not boot it`.** Check that Secure Boot is off, and look for an entry of the server's own with the id `nos` or `nos-next`, such as one in `/etc/grub.d/40_custom`: such an entry comes first in `grub.cfg`, so GRUB boots it instead.

## Next steps

- [Connect to a running nos](/howto/connect): SSH, the UDP shell, and what you can do once you are in.
- [Boot a Linux guest](/howto/linux-guest): the hypervisor module is already at `/lib/modules/hv.ko`.
- [Make a VM from a cloud image](/howto/cloud-vm): `hv create <name> <url>`.
- [Install: nos on a server](/docs/install) is the full reference, and [Real hardware](/docs/real-hardware) describes the machines nos runs on.
