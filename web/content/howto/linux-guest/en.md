---
title: "Boot a Linux guest under the nos hypervisor"
description: "Build a small Linux kernel and BusyBox initramfs, load hv.ko into nos, then boot, type at, network and stop Linux guests."
order: 6
updated: 2026-10-22
---

nos's hypervisor is a loadable module, `hv.ko`. By the end of this guide it runs real Linux guests inside nos in QEMU:

- a kernel you built yourself, booted once for a set time with `hv boot`;
- guests that stay up with `hv start`, which you type at, give a disk and a network, and stop;
- Alpine Linux, booted as it ships.

You need:

- **x86-64.** Linux guests run under nos on x86-64 only.
- **A virtualization extension that nos can see.** Under QEMU you have two options:
  - **TCG** with `-cpu max` emulates AMD-V with nested paging on any host, a Mac included. It is slow: the guest is emulated twice, and a boot to its shell takes minutes.
  - **KVM** on a Linux host, with nested virtualization on (`nested=1` for `kvm_amd` or `kvm_intel`). `-enable-kvm -cpu host` hands nos the host's own AMD-V or Intel VT-x and is far faster. TCG has no VT-x, so Intel's backend runs only under KVM or on real hardware.
- **`nos.iso` and `hv.ko` from the same build or the same release.** A kernel refuses a module built against another kernel interface.
- **A checkout of the nos repository**, for its scripts.
- **An x86-64 Linux machine or container to build the guest**, with the source of Linux 6.6 or later. The project's own gates use 6.18.
- **QEMU 9.2 or later** for guests of more than one CPU under TCG.

The commands below assume one working directory that holds the nos checkout as `nos/` and the Linux source as `linux/`. You will create `guest/` and `rootdir/` next to them.

How the hypervisor works inside is on the [hypervisor page](/hypervisor) and in [The hypervisor](/docs/hypervisor).

## 1. Build a guest kernel

The guest is an unmodified 64-bit Linux `bzImage`. Start from `tinyconfig` and switch on what the hypervisor's machine provides. In `linux/`:

```sh
make ARCH=x86_64 tinyconfig
./scripts/config \
    -e 64BIT -e PRINTK -e EARLY_PRINTK -e TTY -e SERIAL_8250 -e SERIAL_8250_CONSOLE \
    -e BLK_DEV_INITRD -e RD_GZIP -e BINFMT_ELF -e BINFMT_SCRIPT -e PROC_FS -e SYSFS -e DEVTMPFS \
    -e MULTIUSER -e POSIX_TIMERS -e FUTEX -e EPOLL -e SIGNALFD -e TIMERFD -e EVENTFD -e FILE_LOCKING \
    -e SMP -e HYPERVISOR_GUEST -e X86_X2APIC -e X86_MPPARSE --set-val NR_CPUS 16 \
    -e PCI -e PCI_MSI -e VIRTIO_MENU -e VIRTIO_PCI -e VIRTIO_PCI_LEGACY \
    -e BLOCK -e BLK_DEV -e VIRTIO_BLK -e EXT4_FS \
    -e NET -e INET -e PACKET -e UNIX -e IP_PNP -e NETDEVICES -e NET_CORE -e VIRTIO_NET
make ARCH=x86_64 olddefconfig
make ARCH=x86_64 -j"$(nproc)" bzImage
```

The kernel is `linux/arch/x86/boot/bzImage`. Keep `ARCH=x86_64`: on an x86-64 host a plain `make tinyconfig` configures a 32-bit kernel. On Debian or Ubuntu the build needs `gcc make bc flex bison libelf-dev libssl-dev python3 xz-utils`.

What the options are for:

| Options | For |
|---|---|
| `64BIT` … `SERIAL_8250_CONSOLE` | a 64-bit kernel that writes to the emulated 8250 serial port, the guest's console |
| `BLK_DEV_INITRD` … `DEVTMPFS` | a gzipped initramfs, ELF binaries and scripts, `/proc`, `/sys` and devtmpfs |
| `MULTIUSER` … `FILE_LOCKING` | what BusyBox's applets expect. `POSIX_TIMERS` above all: BusyBox's `ping` paces itself with `alarm()`, and without timers it waits forever after its first reply |
| `SMP` … `NR_CPUS 16` | guests of more than one CPU (`cpus=`): an x2APIC, which needs `HYPERVISOR_GUEST` or `IRQ_REMAP`, the MP table, and up to 16 CPUs |
| `PCI` … `EXT4_FS` | disks: legacy virtio over PCI, MSI-X for a guest of more than one CPU, and ext4 |
| `NET` … `VIRTIO_NET` | the guests' network: TCP/IP, packet sockets, `ip=` autoconfiguration and virtio-net |

This is the option set of the kernel the project's Linux-guest gate boots. For a guest that hears the power button `hv stop` presses, add `-e ACPI -e ACPI_BUTTON -e X86_PM_TIMER -e INPUT -e INPUT_EVDEV` before `olddefconfig`. For it to power off when the button is pressed, run BusyBox's `acpid` in it with a handler that powers off ([ACPI](/docs/hypervisor#acpi)). Without ACPI the guest never hears the button, and `hv stop` stops it at once.

## 2. Build a BusyBox initramfs

The initramfs is a static BusyBox and an `/init` script. Which BusyBox you use matters in three ways:

- **Its line editor must ask the terminal where the cursor is** (`ESC [ 6 n`). The module types a line at a guest only after it has heard that question at a prompt.
- **It needs `httpd`**, for the network example below.
- **Its `exec` must run a script with no `#!` line through the shell**, as `udhcpc -s` needs.

Ubuntu 24.04's static BusyBox 1.36.1 does all three. Alpine's `busybox-static` has no `httpd` and runs no such script, and the official `busybox` Docker image's never asks for the cursor.

Get Ubuntu's binary, write the init and pack the archive:

```sh
mkdir -p guest && cd guest
docker run --rm --platform linux/amd64 -v "$PWD:/out" ubuntu:24.04 sh -c \
    'apt-get update -qq && apt-get install -y -qq busybox-static && cp /usr/bin/busybox /out/'
cat > init <<'EOF'
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
echo "nos-guest: init is up"
dmesg -n 1
exec sh
EOF
cat > initramfs.list <<'EOF'
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
dir /bin 0755 0 0
dir /proc 0755 0 0
dir /sys 0755 0 0
dir /mnt 0755 0 0
file /bin/busybox busybox 0755 0 0
file /init init 0755 0 0
EOF
../linux/usr/gen_init_cpio initramfs.list | gzip -9 > initrd
cd ..
```

On Ubuntu 24.04 itself you can skip Docker: install `busybox-static` and copy `/usr/bin/busybox`. `gen_init_cpio` is the kernel's own archive tool, built along with the `bzImage`, and it writes the device node without needing root.

> Keep `dmesg -n 1` in the init. A kernel line printed after the shell's prompt hides the prompt from the module, and every `hv exec` then fails.

## 3. Make nos's root filesystem

Build nos in the checkout, which leaves `nos.iso`, `out/x86_64/modules/hv.ko` and `out/x86_64/modules/sshd.ko` there ([Build nos](/howto/build)):

```sh
cd nos && ./scripts/build-iso-docker.sh && cd ..
```

You can also take `nos.iso`, `hv-x86_64.ko` and `sshd-x86_64.ko` from one [release](https://github.com/irqlevel/nos/releases) instead. Copy them in as `hv.ko` and `sshd.ko`, and use that `nos.iso` in the QEMU command below.

Collect what nos needs into `rootdir/` and make an ext2 image of it:

```sh
mkdir -p rootdir
cp nos/out/x86_64/modules/hv.ko nos/out/x86_64/modules/sshd.ko rootdir/
cp linux/arch/x86/boot/bzImage guest/initrd rootdir/
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

`mkrootfs.sh` labels the filesystem `nos`, which is what the ISO's `root=auto` mounts read-write on `/`. On a Mac it runs `mkfs.ext2` in the `nos-builder` container that `build-iso-docker.sh` made. Everything the guests use must be in `rootdir/` when you make the image. The disk and Alpine sections below add files: put them in `rootdir/` and run `mkrootfs.sh` again before the next boot. From inside nos you can also fetch a file with `wget <url> <path>`, up to 20 MiB.

## 4. Boot nos

```sh
qemu-system-x86_64 -nographic -m 2G -smp 4 -cpu max \
    -cdrom nos/nos.iso \
    -drive file=root.img,format=raw,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0,hostfwd=tcp::2222-:22,hostfwd=tcp::8080-:8080
```

On a Linux host with KVM and nested virtualization, replace `-cpu max` with `-enable-kvm -cpu host`.

- **`-cpu max`** gives TCG's AMD-V with nested paging. QEMU's default CPU model reports SVM without it, and the hypervisor needs nested paging.
- **`-nographic`** puts nos's serial console, and its shell, on your terminal. `Ctrl-A X` quits QEMU.
- **The two forwards** are host port 2222 to nos's sshd and host port 8080 to nos's port 8080, which `hv forward` uses below.

More on running nos in QEMU: [Run nos in QEMU](/howto/run-in-qemu).

## 5. Load the hypervisor

Type these at nos's shell:

```sh
insmod /hv.ko
hv info
hv on
```

`hv info` lists what the CPU's extension offers, a feature a line. `hv on` turns the extension on for every running CPU; `hv on 2` would turn it on for one. The `hv` line asks the CPUs themselves which of them have it on:

```text
$ insmod /hv.ko
module: hv loaded at 0xFFFF800031000000
$ hv on
hv: turned on for cpu 0-3
hv: AMD-V (SVM) on for cpu 0-3 of 0-3
```

`hv help` lists every subcommand. `hv off` turns the extension off again, but refuses while a started guest runs on a CPU it names. `rmmod hv` stops every guest first, then turns the extension off on every CPU.

## 6. A guest for a minute: `hv boot`

```sh
hv boot /bzImage initrd=/initrd secs=60 input=id\n cmdline=console=ttyS0 nolapic
```

The bzImage and the initrd are streamed from nos's files into guest memory, and the guest runs on a vCPU for `secs` seconds. `input=` is typed at its shell once it has one (`\n` is a newline). Then `hv boot` prints the guest's console and how it ended. Under TCG the report looks like this (from the project's documentation and trimmed; the lines your init prints will differ):

```text
  --- ttyS0 ---
[    0.000000] Linux version 6.18.53 ...
[    0.000000] NX (Execute Disable) protection: active
...
[    0.540000] Run /init as init process
...
BusyBox v1.36.1 built-in shell (ash)
~ # id
uid=0 gid=0
~ #
  --- end ttyS0 ---
```

The options of `hv boot` and `hv start`:

| Option | What it does |
|---|---|
| `mem=MiB` | Guest RAM; 256 by default, 64 at least. |
| `secs=N` | How long `hv boot` runs the guest; 60 by default, 600 at most. |
| `initrd=path` | The initramfs, a file of nos's. |
| `input=...` | Text to type at its shell once it has one. |
| `cmdline=...` | The guest's kernel command line. It takes the rest of the line, so put it last. The default is `console=ttyS0 nolapic` for a guest of one CPU and `console=ttyS0` for more. nos adds the host's TSC rate (`tsc_early_khz=`) unless the line already names one or the rate is not known. |

While `hv boot` runs, the guest's console also goes to nos's kernel log a line at a time, as `hvguest| ...` lines. A started guest's console goes there only if you pass `log` to `hv start`.

## 7. Guests that stay up

`hv start` builds the same guest, hands it to a vCPU task of its own and returns. From then on you reach the guest with the other `hv` commands, from the console, over SSH or over the UDP shell. In this transcript the commands came from the project's gate under TCG, where the guest is at its shell about a second after it starts:

```text
$ hv start /bzImage initrd=/initrd cmdline=console=ttyS0 nolapic
hv: vm 0 started on cpu 3 -- /bzImage, 256 MiB, 1 cpu, cmdline "console=ttyS0 nolapic"
$ hv exec 0 secs=60 id
... the rest of its boot ...
~ # id
uid=0 gid=0
~ #
$ hv exec 0 uname -r
uname -r
6.18.53
~ #
$ hv send 0 echo nos$((6*7))nos\n
hv: vm 0: 20 bytes queued
$ hv wait 0 nos42nos
hv: vm 0 printed "nos42nos", 58 ms in
$ hv stop 0
hv: vm 0 stopped -- on request -- nothing in it listens to its power button
```

| Command | What it does |
|---|---|
| `hv list` | Lists the started guests: running, stopping or how each ended, its host CPUs, memory, uptime and address, restarts, and its exit, interrupt, halt and kick counts. |
| `hv exec <id> [secs=N] <line>` | Types a line and prints what comes back, up to the prompt printed after the line went in (10 s by default). A line typed while the guest still boots waits for its shell. |
| `hv send <id> <text>` | Types now, as at a terminal: `\n` for a newline. It also types at a login prompt, which `exec` would wait at in vain. |
| `hv wait <id> [secs=N] [boot=N] <text>` | Waits until this boot's console shows `text`; with `boot=N`, not before the guest's Nth restart. |
| `hv console <id> [bytes=N]` | Prints the end of its console, escape sequences taken out. |
| `hv restart <id>` | Boots it again from its files, running or stopped. |
| `hv stop <id\|all> [secs=N]` | Presses its power button, gives it `secs` (30 by default, `0` not to ask) to turn itself off, then stops it and says how it ended. |

Use the id that `hv start` printed: ids are not reused while the module is loaded. A guest that resets itself stops, with its reset as the reason, unless you start it with `restart`. Then it is booted again each time, up to five times a minute.

### A live console over SSH: `hv attach`

`hv attach` is the guest's console, live: it shows the last kilobyte, then whatever the guest prints, and types each key as you press it. It needs a session that can type, which only SSH provides. Start nos's sshd with your key, in nos's shell:

```sh
insmod /sshd.ko
sshd allow ssh-ed25519 AAAA... you@host
sshd start
```

Then, from your machine:

```sh
ssh -t -p 2222 root@127.0.0.1 hv attach 0
```

`Ctrl-]` detaches; the guest runs on. More about logging in is in [Connect to a running nos](/howto/connect).

## 8. Give it a disk

`disk=<path>` gives the guest a virtio disk backed by a file of nos's. Make an ext4 image with a file in it, put it in `rootdir/` and remake `root.img`:

```sh
mkdir -p content && echo hello > content/hello.txt
mke2fs -q -F -t ext4 -d content rootdir/disk.img 32M
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

A Mac has no `mke2fs`, so run that line in the `nos-builder` container. Copy the content directory inside the container first, as `scripts/mkrootfs.sh` does, because `mke2fs -d` reads extended attributes that a macOS bind mount does not serve:

```sh
docker run --rm --platform linux/amd64 -v "$PWD:/w" -w /w nos-builder \
    bash -c 'cp -r content /tmp/content && mke2fs -q -F -t ext4 -d /tmp/content rootdir/disk.img 32M'
```

Boot nos again and load the hypervisor (steps 4 and 5), then:

```sh
hv start /bzImage initrd=/initrd disk=/disk.img cmdline=console=ttyS0 nolapic
hv exec 0 secs=60 mount -t devtmpfs devtmpfs /dev
hv exec 0 mount /dev/vda /mnt
hv exec 0 cat /mnt/hello.txt
```

- **Naming and size.** Up to four `disk=` options give a guest `vda`, `vdb` and so on, in order. Each disk is the size of its file: the guest writes within it and never grows it.
- **One writer or many readers.** `disk=path:ro` makes a disk read-only to the guest. An image is either one guest's to write or any number of guests' to read, and a start that would break that is refused, naming the owner: `hv: vm 1 not started -- /disk.img: vm 0's disk writes it`.
- **Flushes reach the disk.** A guest's flush syncs the nos filesystem the image lives on.

## 9. Put it on the network

`net` on `hv start` gives the guest a virtio NIC on a switch it shares with the other guests and with nos. The first port is 10.0.100.2, the next 10.0.100.3, and so on, handed to the guest's kernel as `ip=`. nos is 10.0.100.1. Start two guests:

```sh
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv exec 0 secs=60 ping -c 3 10.0.100.1
hv exec 0 ping -c 3 10.0.100.3
ping 10.0.100.2
```

The first two pings run in guest 0, to nos and to the other guest. The last is nos's own `ping`, out of its virtual NIC `hv0`.

**Reach a guest from outside with `hv forward`.** It relays a port of nos's to a port of a guest's. Serve a page from guest 1 and forward nos's port 8080 to it:

```sh
hv exec 1 mkdir -p /www
hv exec 1 echo hello from a guest > /www/index.html
hv exec 1 httpd -p 80 -h /www
hv forward add 8080 1 80
```

nos answers `hv: port 8080 forwarded to vm 1, 10.0.100.3:80`. QEMU forwards host port 8080 to nos, so from your machine:

```sh
curl http://127.0.0.1:8080/index.html
```

**The guests reach the world through nos's NAT**, out of nos's own address. The switch answers a guest's DHCP itself, with the port's address, nos as the router and nos's DNS server. The BusyBox initramfs has no `/etc/resolv.conf`, so for names to resolve, point it at the DNS server `ip=` gave the kernel:

```sh
hv exec 0 mkdir -p /etc && ln -sf /proc/net/pnp /etc/resolv.conf
```

NAT keeps guests away from private networks, link-local addresses (where a cloud's metadata server answers), carrier-grade NAT's shared space and nos's own uplink subnet. Under QEMU's user network that last one includes the host at 10.0.2.2: `nat allow 10.0.2.2/32` lets a guest reach it, and `nat` shows what NAT is doing. More is in [A network](/docs/hypervisor#a-network).

## 10. More than one CPU

`cpus=N` gives a guest N CPUs, up to 16, each a vCPU task on a host CPU of its own. The guest's kernel needs Linux 6.6 or later with `SMP`, `X86_X2APIC` and `X86_MPPARSE`, which the recipe above has. The default command line drops `nolapic`, and nos adds `noapic` itself, because the machine has no IO-APIC unless you pass `ioapic`. From a run on one of the project's servers, trimmed:

```text
$ hv start /bzImage mem=256 cpus=2 initrd=/initrd cmdline=earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check
hv: vm 0 started on cpus 3,2 -- /bzImage, 256 MiB, 2 cpus, cmdline "earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check"
$ hv list
vm 0  running  cpus 3,2  256 MiB  0 s  exits 0  irq 0  hlt 0  kicks 0  /bzImage
$ hv exec 0 grep -e LOC -e RES -e CAL /proc/interrupts
LOC:        266        223   Local timer interrupts
RES:         11         15   Rescheduling interrupts
CAL:        125        144   Function call interrupts
```

- **Each vCPU needs its own host CPU.** Two CPUs of one guest never share a host CPU, so a guest has at most as many CPUs as `hv on` covers.
- **A guest gets whole cores.** By default (`hv cores own`) its cores are its own, and no other guest has a CPU on them; a guest that would need a core another guest holds is refused, saying so. `hv cores shared` lets guests share cores, for a machine whose guests are all yours, and `hv cores` says who holds which CPUs.
- **Under TCG, use QEMU 9.2 or later.** Before 9.2, TCG does not put the accesses of a guest whose paging is off through the nested page table. A guest's second CPU starts in real mode, so it would run out of nos's own memory and corrupt it. KVM and real hardware have always handled this correctly.

## 11. A distribution: Alpine

Alpine's `virt` ISO boots as it ships. Its kernel and initramfs are taken out of the ISO, and the ISO itself becomes the guest's read-only disk. Download `alpine-virt-<version>-x86_64.iso` from Alpine's site, then extract and stage the files (`xorriso` is in most distributions):

```sh
mkdir -p rootdir/alpine
xorriso -osirrox on -indev alpine-virt-3.24.2-x86_64.iso -extract /boot/vmlinuz-virt rootdir/alpine/vmlinuz-virt
xorriso -osirrox on -indev alpine-virt-3.24.2-x86_64.iso -extract /boot/initramfs-virt rootdir/alpine/initramfs-virt
cp alpine-virt-3.24.2-x86_64.iso rootdir/alpine/alpine.iso
chmod 644 rootdir/alpine/*
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

Boot nos, load the hypervisor, and start it:

```sh
hv start /alpine/vmlinuz-virt mem=512 initrd=/alpine/initramfs-virt disk=/alpine/alpine.iso:ro net restart cmdline=console=ttyS0 nolapic modules=loop,squashfs,sd-mod,usb-storage
hv wait 0 secs=600 login:
hv send 0 root\n
hv exec 0 cat /etc/alpine-release; uname -r
```

Its initramfs loads `virtio_blk`, finds the ISO on `vda`, installs the base system from the ISO's packages into a tmpfs, and OpenRC brings it up to a login prompt on `ttyS0`. A getty asks for no cursor position, so log in with `hv send`, which types right away. After the first `send`, `exec` lines also go in as typed. On the switch, Alpine's initramfs configures `eth0` from `ip=` and writes nos's DNS server to `resolv.conf`, so `apk` reaches its mirrors through NAT. `reboot` resets the guest and `restart` boots it again; to wait for the next boot's prompt, use `hv wait 0 boot=1 login:`. Debian's cloud image boots too: see [A distribution](/docs/hypervisor#a-distribution).

## On a server

On a server installed from the system image ([Install nos on a server](/howto/install-on-a-server)), the module is already at `/lib/modules/hv.ko`, so step 5 starts with `insmod /lib/modules/hv.ko`. Put the guest's files on nos's partition from the server's Linux before you boot nos, or fetch them from inside nos with `wget`. The project's servers run guests under the AMD-V of a Ryzen 5 3600 and the VT-x of an i5-13500 ([On real hardware](/docs/hypervisor#on-real-hardware)).

## Troubleshooting

**`hv: vm 0: the guest has not read the line -- it is not at a prompt; the line stays queued`.** The module never saw the guest's shell ask for the cursor position. Check that you use a BusyBox that asks (step 2) and that the init has `dmesg -n 1`. At a login prompt, use `hv send`.

**`hv exec` gives up while the guest is still booting.** It waits 10 s by default. Under TCG give the first one more time, as in `hv exec 0 secs=60 id`.

**`hv on` fails, or `hv info` shows no nested paging.** Start QEMU with `-cpu max` (TCG) or `-enable-kvm -cpu host` (KVM with nested virtualization on).

**`insmod /hv.ko` says `built against another kernel interface`.** `hv.ko` must come from the same build or the same release as the running kernel.

**A `cpus=2` guest comes up with one CPU.** The guest kernel is older than 6.6, or lacks `SMP`, `X86_X2APIC`, `X86_MPPARSE` or a large enough `NR_CPUS`. An older kernel still boots on one CPU.

**A guest of several CPUs is refused.** The free cores have too few CPUs for it. Stop a guest, turn the extension on for more CPUs, or use `hv cores shared`.

**`ping` in the guest stops after the first reply.** The guest kernel has no `POSIX_TIMERS`.

**A guest cannot reach 10.0.2.2, or another private address.** NAT keeps guests away from private networks and from nos's own subnet. Run `nat allow <net>` for what they may reach.

**A guest that reboots stops instead.** Without `restart`, a reset stops the VM and `hv list` shows why. Start it with `restart`, or run `hv restart <id>`.

**`hv off` refuses.** A started guest still runs on that CPU: `hv stop` it first, or `rmmod hv`, which stops all of them.

## Next steps

- Let the project's gate do all of this: `nos/scripts/hv-linux-test.py --bzimage linux/arch/x86/boot/bzImage --initrd guest/initrd` boots a guest and runs the long-lived VMs, and `--disk`, `--net`, `--cpus N` and `--attach` cover the rest. `nos/scripts/hv-distro-test.py --iso alpine-virt-3.24.2-x86_64.iso` does Alpine. Both use the `nos.iso` and modules built in the tree ([Run the gates](/howto/run-the-gates)).
- [The hypervisor](/hypervisor), and the reference: [A Linux guest](/docs/hypervisor#a-linux-guest), [Guests that stay up](/docs/hypervisor#guests-that-stay-up), [More than one CPU](/docs/hypervisor#more-than-one-cpu).
- [Install nos on a server](/howto/install-on-a-server) to run guests on real hardware.
