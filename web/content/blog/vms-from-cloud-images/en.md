---
title: "A VM from a URL: cloud images on nos, kept in folders"
description: "hv create takes a cloud image's URL and makes a VM of it with nothing but nos: the kernel's own HTTPS download, a qcow2 converter with its own DEFLATE decoder, a cloud-init seed, and a folder that outlives a reboot."
date: 2026-10-25
tags: [hypervisor, storage, networking, boot, rust, x86-64]
---

Ask a cloud for a VM and it does four things. It takes a distribution's cloud image and makes it the VM's disk, grows the disk, hands cloud-init a seed with your SSH key, and boots the kernel that the image's own GRUB would boot. Since 25 October nos does the same with one command, by itself, with nothing prepared on another machine:

```text
$ hv create debian https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 size=8G password=nos-howto-1 forward=2201:22
hv: https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 -- into /vms/debian/debian-13-genericcloud-amd64.qcow2
download: 5 MiB of 326 MiB, 1%, 2.6 MiB/s
...
download: 342294528 bytes in 21.3 s, 15.2 MiB/s
hv: /vms/debian/debian-13-genericcloud-amd64.qcow2: qcow2 version 3, a disk of 3072 MiB in clusters of 64 KiB -- into /vms/debian/disk.raw
hv: 266 of 3072 MiB of the disk, 2 s
...
hv: /vms/debian/disk.raw: 907 MiB written of 3072 MiB, the rest holes -- 14487 clusters inflated, 788 copied, 0 zeros, 33877 not there -- in 62.0 s
hv: /vms/debian/vm.toml written -- hv start debian boots it: hvboot, a 8192 MiB disk, cloud-init given 1 key and a password
$ hv start debian
hv: vm 0 (debian) started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3", restarted when it resets
hv: port 2201 forwarded to vm 0, 10.0.100.2:22
```

That nos ran under QEMU's TCG on a Mac. 48.6 s after `hv start`, an `ssh` from the Mac reached the guest's own sshd, through QEMU's port forward and then nos's, and got in with the key that nos's own sshd accepts. cloud-init had found its data on the seed nos wrote: `DataSourceNoCloud [seed=/dev/vdb]`.

Two days earlier none of this existed. A guest lived only as long as the `hv start` line that started it, and the next start needed every word typed again. Its kernel and initrd had to be pulled out of the image on another machine with `debugfs` or `xorriso`, so a kernel that `apt upgrade` installed inside the guest never ran. A disk on nos's ext2 could not grow past 4 GiB. These commits changed that:

| Commit | What it added |
|---|---|
| [36a77e7](https://github.com/irqlevel/nos/commit/36a77e70897b57dafccbde721ffc052af1566203) | A VM kept in a folder: `vm.toml`, `hv create`, `hv vms`, its console in `console.log`, and parameters for modules, so that `insmod` can say where the folders are |
| [fe893b5](https://github.com/irqlevel/nos/commit/fe893b54982526314ed13b9d43b2ec38faed9443) | ext2 files past 4 GiB, so that a guest's disk can be as big as it needs to be |
| [96c4663](https://github.com/irqlevel/nos/commit/96c46634df4e477c4bf998db8dc3d1993e9d30b7) | PVH and hvboot: a guest boots the kernel its own disk's GRUB would |
| [8d5df60](https://github.com/irqlevel/nos/commit/8d5df60c3fb5534becfde26129538e9d73a8e21f) | `hv convert`: a cloud's qcow2 image made a raw disk on nos, by a decoder that gives zlib's answer to every stream |
| [8c525ce](https://github.com/irqlevel/nos/commit/8c525cec7df00b58b8aca223dd409551a7b3c7f5) | `hv create <name> <url>`: the download, the disk, cloud-init's seed and the `vm.toml` in one command |
| [ac09bba](https://github.com/irqlevel/nos/commit/ac09bbadef7a73419ddef5b25df52105b267034e) | Debian 13's cloud image made and booted that way: 12 checks of 12 |

## A VM is a folder

A VM kept in a folder lives in `/vms/<name>/`. The folder holds a `vm.toml` that says how to boot the VM, the files it names (a kernel, an initrd, disks) and a log of the VM's console. `hv start web` boots it, and every command that takes a VM's number takes its name as well ([A VM in a folder](/docs/hypervisor#a-vm-in-a-folder)). `hv create web <words>` writes the file from the same words `hv start` takes. It checks them the way a start would and never writes over a `vm.toml` that is already there. `hv vms` lists the folders, what each one boots and which are running. A `vm.toml` looks like this:

```toml
kernel = "/boot/bzImage"
initrd = "/boot/initrd"
cmdline = "console=ttyS0 root=/dev/vda rw"
memory = 1024
cpus = 2
disks = ["root.img"]
net = true
port = 3
forward = ["2222:22"]
restart = true
```

The format is a strict subset of TOML, read by a parser with no kernel in it ([`config.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/modules/hv/src/config.rs)). Whatever it cannot read it refuses, naming the line. That includes a key it does not know and a key given twice, and the parser's comment gives the reason: `memroy = 4096` would otherwise boot a guest of the default size without a word. `port=` pins the guest to one port of the switch, so its address and MAC are the same at every start. `forward=` relays nos's ports to the guest while it runs. `restart` boots it again when it reboots itself.

The console log is written by a task of the VM's own, once a second, from the console's ring buffer ([`vmlog.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/modules/hv/src/vmlog.rs)). The guest's CPUs never write it, because then a VM exit would wait for the disk. hv's own lines go between the guest's, where they happened: `--- hv: <date> UTC vm 0 started on cpus 2,3 -- ... ---`, and then `booting again`, `stopped` or `off the list`. At 8 MiB the file moves to `console.log.1`. If a guest prints more than 64 KiB in a second, the ring loses what it can no longer hold, and the log says how much was lost.

Where the folders live is hv's one parameter, and modules had no parameters before this commit. Now `insmod /hv.ko vms=/data/vms` hands the rest of the line to the module's init. The module header went to version 2, and a module built for the old one is refused. A parameter hv does not have makes the load fail, the same way a misspelt key makes a `vm.toml` fail. nos's root is a disk, so a folder survives a reboot of nos, and `rc add hv start web` starts the VM at every boot.

## Disks past 4 GiB

A guest's disk is a file on nos's ext2. Until fe893b5, nos's ext2 mapped a file only through its double indirect block and kept the file's size in 32 bits. On 4 KiB blocks a file stopped at 4 GiB, and Ubuntu's 3.5 GiB cloud disk only just fitted. The commit added the triple indirect block, the high word of the size and the `large_file` feature flag. As in Linux, the first file of 2 GiB or more sets the flag, and the flag reaches the disk before the inode that needs it. A file can now be as big as Linux's ext2 allows, almost 2 TiB on 4 KiB blocks. `truncate /vms/web/root.img 20G` makes a blank 20 GiB disk that costs nothing until the guest writes to it. A truncate steps over holes a whole subtree at a time, so cutting a sparse file of terabytes costs only what was written into it.

The filesystem fuzzer learned to make files that big too ([Fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)). Between the fuzzer and a read of the code, three bugs turned up, two of them older than the change. Here is one of those two. A write that ran out of room on the block after the last one wrote the tail of the file's last block past the end of the file, and having allocated nothing, it gave nothing back. A file grown over that tail later read the failed write's bytes where it should have read zeros.

## The guest's own kernel: hvboot

Until 96c4663, hv booted whatever kernel it was handed. A cloud image's kernel and initrd had to be taken out of it first, and the guest's own `/boot` was never read. Now a VM can boot through **hvboot**, a 164 KiB program that runs inside the guest and boots what the guest's GRUB would ([A guest's own kernel](/docs/hypervisor#a-guests-own-kernel-pvh-and-hvboot)). It reads the guest's virtio disks, their GPT or MBR, and the ext2, ext3 or ext4 filesystems on them. Then it reads `grub.cfg` and `grubenv`, picks the entry GRUB would boot, loads that entry's kernel and initrd, and enters the kernel through Linux's boot protocol. After `apt upgrade` and `update-grub`, the next boot runs the new kernel, including a restart, because the VM is built again from its files.

hv starts hvboot by PVH, the direct boot protocol that Xen defined. Linux's `vmlinux`, Cloud Hypervisor and the firmware written for them all speak it. The ELF's segments go to their physical addresses, a start-info structure carries the command line, the memory map and the address of the ACPI tables, and the first CPU starts in 32-bit protected mode with paging off. On the host side this is only an ELF loader, and it checks every header field before using it. hv still has four `unsafe` sites.

Why read `grub.cfg` inside the guest rather than in nos? Because everything it reads is the guest's: a partition table, a filesystem, a GRUB script. A bug in hvboot is then the guest's problem, as a bug in the guest's own GRUB would be, and not nos's. Its parsers are plain Rust over byte slices with no `unsafe`. The 29 sites it does have are its platform's: I/O ports, physical memory, the heap, the virtio rings, the interrupt table and the jump into the kernel. It is not GRUB's interpreter. It reads the script's words the way GRUB does but runs no condition, so it reads every branch of an `if`, and the last `linux` line an entry has wins. Ubuntu 24.04's and Debian 13's cloud images boot this way as they ship, and under TCG hvboot reaches the kernel 0.6 s after the VM starts. It cannot read XFS, btrfs, LVM or LUKS, so a /boot of the RHEL family is out of reach for now. It does not read BLS entries either, and there is no EFI: the kernel is entered through its boot protocol.

## A cloud's qcow2 image, made a raw disk on nos

Clouds ship their images in QEMU's qcow2 format, and hv's disks are raw files. Ubuntu 24.04's server image is qcow2 version 3 with 64 KiB clusters, and 27,971 of the 28,663 clusters that hold data are compressed with zlib. Its L2 tables, which map the disk's clusters to places in the file, are spread through the file, and some data comes before the table that maps it. So the image cannot be converted while it downloads. It is downloaded first and converted after.

`hv convert` is a module command over the `diskimage` crate, which has no kernel in it, no dependency and `forbid(unsafe_code)` ([A disk from a cloud's image](/docs/hypervisor#a-disk-from-a-clouds-image)). It reads qcow2 versions 2 and 3, clusters from 512 bytes to 2 MiB, clusters that are stored, compressed, zero or absent, and extended L2 entries with their subclusters. Whatever it cannot convert faithfully it refuses, and it says why: a backing file, encryption, an external data file, zstd compression, an image QEMU has marked corrupt, a feature it does not know.

The interesting part is the DEFLATE decoder. A compressed cluster is raw DEFLATE, and QEMU reads it through zlib. So a good stream is whatever zlib calls good, including streams that are odd at the edges, and the decoder's header says so ([`inflate.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/diskimage/src/inflate.rs)):

```rust
//! Raw DEFLATE (RFC 1951) -- what a compressed qcow2 cluster holds --
//! decoded as zlib's inflate decodes it. QEMU reads those clusters through
//! zlib, so a cluster this accepts must be one QEMU reads, to the byte, and
//! one QEMU refuses must be refused: every rule here is zlib's, checked in
//! zlib's order (its inflate.c and inftrees.c), and
//! scripts/diskimage-test.py holds the two to the same answer over streams
//! made and broken at random.
```

Here is one such rule. QEMU asks zlib for exactly a cluster's worth of bytes. Once zlib's buffer is full, it still reads on until the next symbol would produce a byte, and if it finds a fault on the way, it fails. So this decoder reads on "dry" as well. The gate, [`diskimage-test.py`](/docs/testing), holds it to zlib over 6,634 streams: zlib's own, the same streams broken, streams written block by block, each rule's own stream cut at every byte, and noise. Every verdict and every byte must be zlib's. When we planted a mistake that let an incomplete code-length code through, the random streams missed it, and only the streams cut at every byte caught it. That is why those streams are part of the gate. The same gate converts images that `qemu-img` and `qemu-io` write (every version, cluster size and kind of cluster) and compares the result with what `qemu-img` reads, byte for byte. It also fuzzes the images' metadata.

The image is a download, so the converter trusts none of it. Every offset is held to the format's alignment and to the file's size. Nothing the image says can make the converter read, write or hold more than 64 KiB at a time, whatever the cluster size, so a 2 MiB cluster is never one allocation. One rule is stricter than QEMU's. QEMU reads past the end of a file as zeros, but here a table or a cluster that starts past the end is refused:

```rust
const WHOLE: &str = "past the image's end -- is the image whole?";
```

A file like that is a download cut short. Turning it into a disk full of zeros where its data should be is exactly the kind of failure nobody would hear about.

The disk keeps its holes. It is a new file, grown to the image's size with a hole (`File::set_len`, new in the module interface), and only the 4 KiB blocks that are not all zeros are written. Ubuntu's 3.5 GiB disk costs the 1.7 GiB it actually holds. nos converted it itself, on its ext2 root on arm64 under HVF, in 25.8 s, writing 1,785 MiB of 3,584, and the SHA-256 that nos computed of the result matched the host's. On the host, `qemu-img compare` called the same conversion identical.

## One command

`hv create <name> <url>` puts the pieces together ([A VM of a cloud's image](/docs/hypervisor#a-vm-of-a-clouds-image), [`cloud.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/modules/hv/src/cloud.rs)):

- **The download.** A module cannot link the network layer, and the TCP connection a module can open has a small receive window and no TLS. So the kernel now exports its own HTTP client to modules, `kernel_http_fetch`. It is the same code as the shell's `wget` ([HTTPS from inside the kernel](/blog/https-from-the-kernel)): TLS with the certificate checked, redirects followed, and a receive window big enough for a download of gigabytes. Only a 2xx answer counts as a body, a file that is already there is never written over, and a failed download leaves no file behind.
- **The disk.** A qcow2 image is converted and the download removed. A raw image is taken as it is. Anything else is refused by name: an ISO, a gzip or xz stream, a VMDK or a VHD, each with a word on what to do with it. `size=` grows the disk with a hole, never below what the image holds, and on the first boot cloud-init's growpart grows the root into it.
- **The seed.** cloud-init's NoCloud data source looks for a disk labelled `cidata`, and nos writes that disk itself: `seed.iso`, an ISO 9660 image with Rock Ridge names, made by the same crate as the converter. Its `meta-data` names the machine after the VM and gives it an instance ID of its own (`nos-<name>-<time>`), so that cloud-init treats it as a new machine. Its `user-data` gives the image's default user (`debian` or `ubuntu`) the keys nos's own sshd accepts, from `/etc/ssh/authorized_keys`, and with `password=` a password for the console. Every string goes in as a YAML double-quoted string, so a key's comment can never be read as YAML. libarchive reads 24 sectors before it agrees that a file is ISO 9660, so the seed is padded to 64 KiB.
- **The `vm.toml`**, written last. It names hvboot as the kernel (`/lib/hvboot`, where the system image keeps it), then the disk and the read-only seed, a NIC on the switch and `restart`, with 1 GiB of memory unless `mem=` says otherwise. The rest of `hv start`'s words go in as given: `cpus=`, `port=`, `forward=2201:22` for the guest's own sshd. If any step fails, the command removes what it made (the download, the disk, the seed, and the folder if it made the folder too) and says why.

The gate for the command is `hv-test.py` on x86-64. It runs a web server of its own and makes VMs from it: a compressed qcow2 image downloaded, converted and grown, the disk held to its SHA-256; a raw image taken byte for byte; and a 404, a file that is no image and a `size=` below the image's, each refused with nothing left behind.

## Debian 13 from a URL

`hv-distro-test.py --create <url>` does the whole thing in a nos under QEMU's TCG on a Mac, then checks the result from inside the guest. We gave it Debian 13's genericcloud image. nos fetched it over HTTPS through Debian's 302 redirect, 342,294,528 bytes in 35.5 s (9.1 MiB/s), and converted it in 51.5 s, writing 907 MiB of 3,072. `hv start` booted it through hvboot, off the image's own `/boot`. cloud-init finished 49.9 s after the VM started, with its data from `DataSourceNoCloud [seed=/dev/vdb]`. The `debian` user logged in on the console with the seed's password, nos's key was in its `authorized_keys`, the machine had the VM's name, the root had grown into the 4 GiB disk and `sudo` worked. Finally `hv stop` pressed the power button and the guest shut itself down. That is 12 checks of 12.

The first run lost its login, and the console showed why. Debian's serial getty prints its first prompt after cloud-init's last line, and agetty throws away whatever was typed before it opened the port. The test had typed the user name the moment cloud-init said it was done. The name was thrown away, and the password, typed next, became the user name at the following prompt. The test now waits for `login:` before it types. The run before that had stopped at `hv wait`'s limit of 600 s per wait, so the test now waits in pieces.

## Across a reboot of nos

A folder is meant to outlive nos. To check that the way a user would meet it, we ran the steps of the [how-to](/howto/cloud-vm) as a script. nos ran under QEMU's TCG on a Mac with an 8 GiB ext2 root, and the commands went to nos's shell through its own sshd. That run is the session at the top of this post. With the guest up, a file was written inside it, and `rc add` put `insmod`, `hv on` and `hv start debian` into `/etc/rc`. `hv stop` pressed the power button, the guest was off 7.4 s later, and nos powered off.

On the next boot nothing was typed: `/etc/rc` started the VM. 34 s after nos's sshd came up, the guest's sshd answered with the same host key as before, and `ssh` had been told to refuse any other. cloud-init had taken it for the machine it had already set up, the file was still there, and `console.log` held both boots, each between hv's lines:

```text
--- hv: 2026-10-25 12:45:38 UTC vm 0 started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3", restarted when it resets ---
--- hv: 2026-10-25 12:46:37 UTC vm 0 stopped -- the guest powered itself off, S5 by its ACPI PM1 control register, at 0xffffffff96e1d0a3 ---
--- hv: 2026-10-25 12:46:37 UTC vm 0 off the list ---
--- hv: 2026-10-25 12:46:44 UTC vm 0 started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3", restarted when it resets ---
```

Both boots, the download included, took three and a half minutes.

## What it does not do yet

- It does not check the image against the distribution's `SHA256SUMS`. For now the TLS certificate is what vouches for the bytes.
- A download that breaks off is not resumed. The client gives up after ten seconds of silence, and the command leaves nothing behind.
- nos's root has to hold the download and the disk at the same time while the image converts.
- VMs made this way have run only under QEMU's TCG on a Mac, which emulates AMD-V. PVH has not yet run under VT-x, and no distribution has been booted by hvboot on bare metal. The servers are next.

Stage 4 of the [roadmap](https://github.com/irqlevel/nos/blob/master/plans/04-control-plane.md) puts the VM lifecycle behind an HTTP API. The folder is already the VM definition that API needs, kept across reboots. The steps are in the how-to [Make a VM from a cloud image](/howto/cloud-vm), and the rest of the hypervisor is on [its page](/hypervisor).
