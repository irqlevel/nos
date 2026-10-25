---
title: "Make a VM from a cloud image"
description: "Give nos a distribution's cloud image by its URL and get a VM that boots its own kernel, is set up by cloud-init with your SSH key, and starts again after nos reboots."
order: 7
updated: 2026-10-25
---

`hv create <name> <url>` makes a VM out of a distribution's cloud image the way a cloud does, and nothing is done on any other machine. nos downloads the image over HTTPS, converts it from qcow2 to a raw disk and grows the disk. It writes a cloud-init seed with your SSH key, then a `vm.toml` that boots the image's own kernel. The VM lives in a folder on nos's disk, so it is still there after nos reboots. By the end of this guide, Debian 13 will be running under nos, nos itself in QEMU, and you will log into the guest over SSH from your machine.

You need:

- **x86-64, and a CPU virtualization extension that nos can see.** Under QEMU that is TCG with `-cpu max`, on any host including a Mac, or KVM with nested virtualization on a Linux host. Every command below was run under TCG on a Mac. Under KVM, nos uses the host's own AMD-V or Intel VT-x. Under VT-x, the way these VMs boot (PVH, below) has not been tried yet.
- **nos built from master** ([Build nos](/howto/build)). Releases up to v0.0.25 predate `hv create <url>`. From one build, you need `nos.iso`, `out/x86_64/modules/hv.ko`, `out/x86_64/modules/sshd.ko` and `out/x86_64/hvboot`.
- **An Ed25519 key** (`ssh-keygen -t ed25519`). nos's sshd accepts no other kind, and the guest gets the same key.
- **Internet access for nos.** QEMU's user network gives it NAT and a DNS server, and the ISO turns DNS on (`dns=on`).

The commands assume a working directory that holds the nos checkout as `nos/`. To see how the pieces work inside, read [A VM from a URL](/blog/vms-from-cloud-images).

## 1. Make nos's root filesystem

Put the modules, hvboot and your key where the system image keeps them, and have `/etc/rc` start sshd at boot:

```sh
mkdir -p rootdir/lib/modules rootdir/etc/ssh
cp nos/out/x86_64/modules/hv.ko nos/out/x86_64/modules/sshd.ko rootdir/lib/modules/
cp nos/out/x86_64/hvboot rootdir/lib/hvboot
cp ~/.ssh/id_ed25519.pub rootdir/etc/ssh/authorized_keys
printf 'insmod /lib/modules/sshd.ko\nsshd start\n' > rootdir/etc/rc
nos/scripts/mkrootfs.sh root.img 8192 rootdir
```

- **`/lib/hvboot`** is the kernel that `hv create` writes into the VM's `vm.toml`. hvboot is a small program that runs inside the guest and boots the kernel that the guest's own GRUB would boot ([A guest's own kernel](/docs/hypervisor#a-guests-own-kernel-pvh-and-hvboot)).
- **`/etc/ssh/authorized_keys`** does two jobs. nos's sshd lets these keys in, and `hv create` gives them to the guest's default user through cloud-init.
- **The size.** While `hv create` runs, the root holds the download and the converted disk at the same time. Debian's image is 326 MiB and its disk takes 907 MiB, so 8 GiB leaves room for the guest to grow. `mkrootfs.sh` makes a sparse file, which takes up only what is written into it. On a Mac it runs `mkfs.ext2` in the `nos-builder` container that the build created.

## 2. Boot nos

```sh
qemu-system-x86_64 -nographic -m 3G -smp 4 -cpu max \
    -cdrom nos/nos.iso \
    -drive file=root.img,format=raw,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0,hostfwd=tcp::2222-:22,hostfwd=tcp::2201-:2201
```

On a Linux host with KVM and nested virtualization, replace `-cpu max` with `-enable-kvm -cpu host`.

- **`-m 3G`** leaves room for the guest's 1 GiB. If you give the guest more with `mem=`, give nos more too.
- **Port 2222** on your machine reaches nos's sshd. **Port 2201** reaches nos's port 2201, which `forward=2201:22` below relays to the guest's own sshd.
- **`-nographic`** puts nos's console on your terminal. `Ctrl-A X` quits QEMU.

When the console prints `sshd: listening on port 22`, nos is ready. You can type the commands below at its console, or over SSH from another terminal: `ssh -p 2222 root@127.0.0.1`.

## 3. Make the VM

Load the hypervisor, turn it on and make the VM:

```sh
insmod /lib/modules/hv.ko
hv on
hv create debian https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 size=8G password=<a password> forward=2201:22
```

Under TCG on a Mac this took 84 s, the download 21 s of it:

```text
hv: https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 -- into /vms/debian/debian-13-genericcloud-amd64.qcow2
download: 5 MiB of 326 MiB, 1%, 2.6 MiB/s
...
download: 342294528 bytes in 21.3 s, 15.2 MiB/s
hv: /vms/debian/debian-13-genericcloud-amd64.qcow2: qcow2 version 3, a disk of 3072 MiB in clusters of 64 KiB -- into /vms/debian/disk.raw
...
hv: /vms/debian/disk.raw: 907 MiB written of 3072 MiB, the rest holes -- 14487 clusters inflated, 788 copied, 0 zeros, 33877 not there -- in 62.0 s
hv: /vms/debian/vm.toml written -- hv start debian boots it: hvboot, a 8192 MiB disk, cloud-init given 1 key and a password
```

What the words mean:

- **`debian`** is the VM's name, and its folder is `/vms/debian`.
- **The URL** is Debian 13's generic cloud image. The kernel's own HTTPS client fetches it, checking the server's certificate and following Debian's redirect to a mirror.
- **`size=8G`** grows the disk, with a hole that costs nothing until the guest writes into it. On the first boot, cloud-init grows the root partition to fill it.
- **`password=`** gives the image's default user, `debian`, a password for the console. Without it, the user can log in only with your key.
- **`forward=2201:22`** relays nos's port 2201 to the guest's port 22 while the guest runs.
- **The rest of `hv start`'s words** go into the file too: `mem=2048`, `cpus=2`, `port=3`, `cmdline=...` ([A VM of a cloud's image](/docs/hypervisor#a-vm-of-a-clouds-image)).

See what it made:

```text
$ hv vms
debian  not running  hvboot  1024 MiB  1 cpu  disks disk.raw, seed.iso:ro  net  forward 2201:22  restart
$ ls /vms/debian
f 8589934592 disk.raw
f 65536 seed.iso
f 228 vm.toml
$ cat /vms/debian/vm.toml
# A VM of nos's hypervisor: hv start <this folder's name> boots it (docs/hypervisor.md)
kernel = "/lib/hvboot"
memory = 1024
disks = ["disk.raw", "seed.iso:ro"]
net = true
forward = ["2201:22"]
restart = true
console_log = true
```

`disk.raw` is the disk: 8 GiB long, with 907 MiB written. `seed.iso` is cloud-init's seed, which the guest sees as a read-only disk labelled `cidata`. `hv start` reads `vm.toml` every time it starts the VM, and it refuses a key it does not know, naming the line. The keys are listed in [Where the folders are](/docs/hypervisor#where-the-folders-are).

## 4. Start it and log in

```text
$ hv start debian
hv: vm 0 (debian) started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3", restarted when it resets
hv: port 2201 forwarded to vm 0, 10.0.100.2:22
```

hvboot finds the guest's own `/boot`, reads its `grub.cfg` and boots the default entry's kernel. Then cloud-init sets the guest up from the seed: the machine's name, your key, the password and the bigger root. `hv console debian` shows what the guest has printed so far. `ssh -t -p 2222 root@127.0.0.1 hv attach debian` follows its console live and sends it your keys, and `Ctrl-]` detaches. Under TCG on a Mac, the guest's sshd answered 49 s after `hv start`. Log in from your machine:

```sh
ssh -p 2201 debian@127.0.0.1
```

The connection goes through QEMU's port forward to nos, then through nos's forward to the guest. Inside, check what cloud-init did:

```text
debian@debian:~$ cloud-init status --long
status: done
...
detail: DataSourceNoCloud [seed=/dev/vdb]
errors: []
debian@debian:~$ df -h /
Filesystem      Size  Used Avail Use% Mounted on
/dev/vda1       7.7G  596M  6.8G   8% /
```

The `debian` user can use `sudo`. On the guest's console (`hv attach debian`), the same user logs in with the password you gave `hv create`.

## 5. Keep it across reboots

The folder is on nos's disk, so it outlives nos. To start the VM every time nos boots, add three lines to `/etc/rc`:

```text
$ rc add insmod /lib/modules/hv.ko
$ rc add hv on
$ rc add hv start debian
$ rc
1  insmod /lib/modules/sshd.ko
2  sshd start
3  insmod /lib/modules/hv.ko
4  hv on
5  hv start debian
```

Then stop the guest and nos:

```sh
hv stop debian
poweroff
```

`hv stop` presses the guest's power button and gives it 30 s to shut itself down; Debian took 7 s under TCG. `secs=N` changes the wait. Boot nos again with the same QEMU command and type nothing: `/etc/rc` starts the VM. `ssh -p 2201 debian@127.0.0.1` reaches the same machine, with the same host key and the same files. cloud-init does not set the guest up a second time, because the seed's instance ID has not changed. `/vms/debian/console.log` keeps the console of every boot, with hv's own lines between them:

```text
--- hv: 2026-10-25 12:46:37 UTC vm 0 stopped -- the guest powered itself off, S5 by its ACPI PM1 control register, at 0xffffffff96e1d0a3 ---
--- hv: 2026-10-25 12:46:37 UTC vm 0 off the list ---
--- hv: 2026-10-25 12:46:44 UTC vm 0 started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "...", restarted when it resets ---
```

At 8 MiB the log moves to `console.log.1`. To keep the folders somewhere other than `/vms`, load the module with `insmod /lib/modules/hv.ko vms=<folder>`.

To remove the VM, stop it, remove its folder and its line in `/etc/rc`:

```sh
hv stop debian
rm /vms/debian
rc del 5
```

## Other images

The same command takes other distributions' cloud images, as long as hvboot can read their `/boot`: ext2, ext3 or ext4, on a GPT or MBR disk, with a GRUB config. Ubuntu 24.04's server image converts in nos and boots through hvboot, though `hv create` has not yet made one end to end:

```sh
hv create noble https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img size=20G
```

Its download is 592 MiB, and its disk takes 1.7 GiB once converted, so give nos's root room for both. Not yet supported: a `/boot` on XFS or btrfs (the RHEL family) or on LVM or LUKS, GRUB's BLS entries, and anything that needs EFI to start its kernel. A file that is neither qcow2 nor a raw disk is refused, and the message names what it is.

## On a server

On a server installed from a system image built from master ([Install nos on a server](/howto/install-on-a-server); v0.0.25's image predates hvboot), `hv.ko` is already in `/lib/modules`, hvboot is at `/lib/hvboot`, `/etc/rc` starts sshd, and `nos-install --ssh-key` has put your key in `/etc/ssh/authorized_keys`. Steps 3 to 5 are the same there, and `forward=` makes the guest reachable at the server's own address. The partition nos runs from has to hold the images and the guests' disks. `hv create` has not yet been run on the project's servers.

## Troubleshooting

**`the server answered 404 -- there is nothing at that URL`.** The image has moved. Distributions rename and retire their images, so take the URL from the distribution's download page.

**`its host has no address the resolver found -- is DNS on (dns=on)?`.** nos has no DNS server. The ISO's command line turns DNS on, so check that DHCP gave nos an address (`net`). On a command line of your own, add `dns=on`.

**`the TLS handshake was refused -- a certificate that did not verify (is the clock right?), or no protocol in common`.** A certificate checks out only if nos's clock is right, and nos reads its clock from the RTC.

**`writing the file failed -- is the filesystem full?`.** nos's root is too small for the download and the disk together. Make `root.img` bigger. A failed `hv create` removes everything it made, so you can simply run it again.

**`/vms/debian/vm.toml is there already -- not written over`.** A VM of that name exists. Pick another name, or remove the old VM first.

**`hv: no key in /etc/ssh/authorized_keys and no password= -- nobody can log in to it`.** The VM was made, but nobody can log into it. Remove it, add a key with `sshd allow <the line from your .pub file>` or pass `password=`, and make it again.

**`ssh -p 2201` is refused or hangs.** The guest is still booting. `hv console debian` shows how far it got. Under TCG the first boot takes about a minute.

## Next steps

- [A VM from a URL](/blog/vms-from-cloud-images): how each piece works and how it was tested.
- In the reference: [A VM in a folder](/docs/hypervisor#a-vm-in-a-folder), [A VM of a cloud's image](/docs/hypervisor#a-vm-of-a-clouds-image), [A disk from a cloud's image](/docs/hypervisor#a-disk-from-a-clouds-image).
- [Boot a Linux guest](/howto/linux-guest): guests from a kernel you build yourself, with disks, a network and several CPUs.
- `nos/scripts/hv-distro-test.py --create <url>` runs this guide as a test and checks the guest from the inside ([Run the gates](/howto/run-the-gates)).
