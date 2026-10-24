---
title: "Boot a Linux guest under the nos hypervisor"
description: "Build a small Linux kernel and a BusyBox initramfs, load hv.ko into nos, and run Linux guests: type commands into them, network them, stop them."
order: 6
updated: 2026-10-22
---

nos's hypervisor is a loadable module, `hv.ko`. By the end of this guide, it will run real Linux guests inside nos, with nos itself running in QEMU:

- a kernel you built yourself, booted once for a set time with `hv boot`;
- long-running guests started with `hv start`: you type commands into them, give them a disk and a network, and stop them;
- Alpine Linux, booted as it ships.

You need:

- **x86-64.** Linux guests run under nos on x86-64 only.
- **A CPU virtualization extension that nos can see.** Under QEMU you have two options:
  - **TCG**, QEMU's software emulator, works on any host, a Mac included. With `-cpu max` it emulates AMD-V with nested paging (hardware translation of guest physical addresses). It is slow: the guest is emulated twice, and booting it to a shell takes minutes.
  - **KVM** on a Linux host, with nested virtualization turned on (`nested=1` for `kvm_amd` or `kvm_intel`). `-enable-kvm -cpu host` hands nos the host's own AMD-V or Intel VT-x, which is far faster. TCG has no VT-x, so the hypervisor's Intel backend runs only under KVM or on real hardware.
- **`nos.iso` and `hv.ko` from the same build or the same release.** A kernel refuses a module built against a different kernel interface.
- **A checkout of the nos repository**, for its scripts.
- **An x86-64 Linux machine or container to build the guest**, with the source of Linux 6.6 or later. The project's own gates (its test scripts) use 6.18.
- **QEMU 9.2 or later**, if you want guests with more than one CPU under TCG.

The commands below assume a single working directory that holds the nos checkout as `nos/` and the Linux source as `linux/`. You will create `guest/` and `rootdir/` next to them.

To see how the hypervisor works inside, read the [hypervisor page](/hypervisor) and [The hypervisor](/docs/hypervisor).

## 1. Build a guest kernel

The guest kernel is an unmodified 64-bit Linux `bzImage`. Start from `tinyconfig`, the smallest configuration, and turn on what the hypervisor's virtual machine provides. In `linux/`:

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

The kernel ends up in `linux/arch/x86/boot/bzImage`. Keep `ARCH=x86_64`: on an x86-64 host, a plain `make tinyconfig` configures a 32-bit kernel. On Debian or Ubuntu, the build needs `gcc make bc flex bison libelf-dev libssl-dev python3 xz-utils`.

What the options are for:

| Options | For |
|---|---|
| `64BIT` … `SERIAL_8250_CONSOLE` | a 64-bit kernel that writes to the emulated 8250 serial port, which is the guest's console |
| `BLK_DEV_INITRD` … `DEVTMPFS` | a gzipped initramfs, ELF binaries and scripts, `/proc`, `/sys` and devtmpfs |
| `MULTIUSER` … `FILE_LOCKING` | what BusyBox's applets expect. `POSIX_TIMERS` matters most: BusyBox's `ping` paces itself with `alarm()`, and without timers it waits forever after its first reply |
| `SMP` … `NR_CPUS 16` | guests with more than one CPU (`cpus=`): the x2APIC, which needs `HYPERVISOR_GUEST` or `IRQ_REMAP`, the MP table, and up to 16 CPUs |
| `PCI` … `EXT4_FS` | disks: legacy virtio over PCI, MSI-X for guests with more than one CPU, and ext4 |
| `NET` … `VIRTIO_NET` | the guests' network: TCP/IP, packet sockets, `ip=` autoconfiguration and virtio-net |

These are the same options as in the kernel that the project's Linux-guest gate boots. `hv stop` presses the guest's power button. For the guest to notice it, add `-e ACPI -e ACPI_BUTTON -e X86_PM_TIMER -e INPUT -e INPUT_EVDEV` before `olddefconfig`. For the guest to power off when the button is pressed, run BusyBox's `acpid` in it with a handler that powers off ([ACPI](/docs/hypervisor#acpi)). Without ACPI the guest never notices the button, and `hv stop` stops it at once.

## 2. Build a BusyBox initramfs

The initramfs holds a static BusyBox and an `/init` script. Which BusyBox you use matters in three ways:

- **Its line editor must ask the terminal where the cursor is** (`ESC [ 6 n`). The module types a line into a guest only after it has seen that question at a prompt.
- **It needs `httpd`** for the network example below.
- **Its `exec` must run a script that has no `#!` line through the shell**, which `udhcpc -s` needs.

The static BusyBox 1.36.1 from Ubuntu 24.04 does all three. Alpine's `busybox-static` has no `httpd` and doesn't run such scripts, and the BusyBox in the official `busybox` Docker image never asks where the cursor is.

Get Ubuntu's binary, write the init script and pack the archive:

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

On Ubuntu 24.04 itself you can skip Docker: install `busybox-static` and copy `/usr/bin/busybox`. `gen_init_cpio` is the kernel's own tool for building the archive. It is built along with the `bzImage`, and it creates the device node without needing root.

> Keep `dmesg -n 1` in the init script. If the kernel prints a line after the shell's prompt, that line hides the prompt from the module, and then every `hv exec` fails.

## 3. Make nos's root filesystem

Build nos in the checkout. The build leaves `nos.iso`, `out/x86_64/modules/hv.ko` and `out/x86_64/modules/sshd.ko` there ([Build nos](/howto/build)):

```sh
cd nos && ./scripts/build-iso-docker.sh && cd ..
```

Instead, you can take `nos.iso`, `hv-x86_64.ko` and `sshd-x86_64.ko` from a single [release](https://github.com/irqlevel/nos/releases). Copy the modules in as `hv.ko` and `sshd.ko`, and use that release's `nos.iso` in the QEMU command below.

Collect what nos needs in `rootdir/` and make an ext2 image of it:

```sh
mkdir -p rootdir
cp nos/out/x86_64/modules/hv.ko nos/out/x86_64/modules/sshd.ko rootdir/
cp linux/arch/x86/boot/bzImage guest/initrd rootdir/
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

`mkrootfs.sh` labels the filesystem `nos`. The ISO boots nos with `root=auto`, which mounts the filesystem with that label read-write on `/`. On a Mac, the script runs `mkfs.ext2` in the `nos-builder` container that `build-iso-docker.sh` created. Everything the guests use must be in `rootdir/` when you make the image. The disk and Alpine sections below add files: put them in `rootdir/` and run `mkrootfs.sh` again before the next boot. You can also download a file from inside nos with `wget <url> <path>`, even a disk image of several gigabytes.

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

- **`-cpu max`** gives you TCG's AMD-V with nested paging. QEMU's default CPU model reports SVM (AMD-V) without nested paging, and the hypervisor needs it.
- **`-nographic`** puts nos's serial console, and with it nos's shell, on your terminal. `Ctrl-A X` quits QEMU.
- **The two port forwards** connect host port 2222 to nos's sshd and host port 8080 to nos's port 8080, which `hv forward` uses below.

For more on running nos in QEMU, see [Run nos in QEMU](/howto/run-in-qemu).

## 5. Load the hypervisor

Type these commands in nos's shell:

```sh
insmod /hv.ko
hv info
hv on
```

`hv info` lists what the CPU's virtualization extension offers, one feature per line. `hv on` turns the extension on for every running CPU, while `hv on 2` would turn it on for CPU 2 only. The last line that `hv on` prints comes from asking the CPUs themselves which of them have the extension on:

```text
$ insmod /hv.ko
module: hv loaded at 0xFFFF800031000000
$ hv on
hv: turned on for cpu 0-3
hv: AMD-V (SVM) on for cpu 0-3 of 0-3
```

`hv help` lists every subcommand. `hv off` turns the extension off again, but it refuses while a guest started with `hv start` runs on one of the CPUs you name. `rmmod hv` first stops every guest, then turns the extension off on every CPU.

## 6. A guest for a minute: `hv boot`

```sh
hv boot /bzImage initrd=/initrd secs=60 input=id\n cmdline=console=ttyS0 nolapic
```

`hv boot` streams the bzImage and the initrd from nos's files into guest memory and runs the guest on a vCPU for `secs` seconds. Once the guest has a shell, the `input=` text is typed into it (`\n` is a newline). Then `hv boot` prints the guest's console and how the run ended. Under TCG the report looks like this (an example from the project's documentation, trimmed; the lines your init prints will differ):

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

Options of `hv boot` and `hv start`:

| Option | What it does |
|---|---|
| `mem=MiB` | Guest RAM in MiB: 256 by default, at least 64. |
| `secs=N` | How many seconds `hv boot` runs the guest: 60 by default, at most 600. |
| `initrd=path` | The initramfs, a file on nos's filesystem. |
| `input=...` | Text to type into the guest's shell once it has one. |
| `cmdline=...` | The guest's kernel command line. It takes the rest of the line, so put it last. The default is `console=ttyS0 nolapic` for a guest with one CPU and `console=ttyS0` for more. nos adds the host's TSC rate (`tsc_early_khz=`) unless the line already sets one or the rate is unknown. |

While `hv boot` runs, the guest's console also goes to nos's kernel log line by line, as `hvguest| ...` lines. For a guest started with `hv start`, the console goes there only if you pass `log`.

## 7. Guests that stay up

`hv start` builds the same guest, hands it to a vCPU task of its own and returns. From then on, you work with the guest through the other `hv` commands: from the console, over SSH or over the UDP shell. The commands in this transcript come from a run of the project's gate under TCG, where the guest reaches its shell about a second after it starts:

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
| `hv list` | Lists the started guests: whether each one is running, stopping or how it ended, its host CPUs, memory, uptime and address, its restarts, and its counts of exits, interrupts, halts and kicks. An exit is a VM exit, the moment the CPU leaves the guest and hands control back to the hypervisor. A kick is an interrupt that forces a running vCPU out of its guest. |
| `hv exec <id> [secs=N] <line>` | Types a line and prints what comes back, up to the prompt that follows the line. It waits 10 s by default. A line typed while the guest is still booting waits for its shell. |
| `hv send <id> <text>` | Types the text right away, as at a terminal (`\n` is a newline). It also works at a login prompt, where `exec` would wait in vain. |
| `hv wait <id> [secs=N] [boot=N] <text>` | Waits until the current boot's console shows `text`. With `boot=N`, it first waits for the guest's Nth restart. |
| `hv console <id> [bytes=N]` | Prints the end of the guest's console, with escape sequences removed. |
| `hv restart <id>` | Boots the guest again from its files, whether it is running or stopped. |
| `hv stop <id\|all> [secs=N]` | Presses the guest's power button and gives it `secs` seconds (30 by default; `0` skips asking) to turn itself off. Then it stops the guest and says how it ended. |

Use the id that `hv start` printed. Ids are not reused while the module is loaded. A guest that resets itself stops, with the reset given as the reason. If you started it with `restart`, it is booted again instead, up to five times a minute.

### A live console over SSH: `hv attach`

`hv attach` is the guest's live console. It shows the last kilobyte of output, then whatever the guest prints, and sends each key to the guest as you press it. It needs an interactive terminal session, and only SSH provides one. In nos's shell, start nos's sshd with your key:

```sh
insmod /sshd.ko
sshd allow ssh-ed25519 AAAA... you@host
sshd start
```

Then, from your machine:

```sh
ssh -t -p 2222 root@127.0.0.1 hv attach 0
```

`Ctrl-]` detaches, and the guest keeps running. For more about logging in, see [Connect to a running nos](/howto/connect).

## 8. Give the guest a disk

`disk=<path>` gives the guest a virtio disk backed by a file on nos's filesystem. Make an ext4 image with a file in it, put the image in `rootdir/` and rebuild `root.img`:

```sh
mkdir -p content && echo hello > content/hello.txt
mke2fs -q -F -t ext4 -d content rootdir/disk.img 32M
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

A Mac has no `mke2fs`, so run that line in the `nos-builder` container. `mke2fs -d` reads extended attributes, which a macOS bind mount does not provide, so first copy the content directory inside the container, as `scripts/mkrootfs.sh` does:

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

- **Names and size.** A guest can have up to four `disk=` options. They become `vda`, `vdb` and so on, in order. Each disk is the size of its file: the guest writes within it and never grows it.
- **One writer or many readers.** `disk=path:ro` makes a disk read-only to the guest. An image is either written by one guest or read by any number of guests. A start that would break this is refused, and the message names the owner: `hv: vm 1 not started -- /disk.img: vm 0's disk writes it`.
- **Flushes reach the disk.** When the guest flushes its disk, nos syncs the filesystem that holds the image.

## 9. Connect the guest to the network

The `net` option of `hv start` gives the guest a virtio NIC on a switch that it shares with the other guests and with nos. The first port gets 10.0.100.2, the next 10.0.100.3, and so on. The guest's kernel receives its address through `ip=`. nos itself is 10.0.100.1. Start two guests:

```sh
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv exec 0 secs=60 ping -c 3 10.0.100.1
hv exec 0 ping -c 3 10.0.100.3
ping 10.0.100.2
```

The first two pings run in guest 0: one to nos, one to the other guest. The last is nos's own `ping`, sent out of its virtual NIC `hv0`.

**Reach a guest from outside with `hv forward`.** It relays a port on nos to a port on a guest. Serve a page from guest 1 and forward nos's port 8080 to it:

```sh
hv exec 1 mkdir -p /www
hv exec 1 echo hello from a guest > /www/index.html
hv exec 1 httpd -p 80 -h /www
hv forward add 8080 1 80
```

nos answers `hv: port 8080 forwarded to vm 1, 10.0.100.3:80`. QEMU forwards host port 8080 to nos, so you can fetch the page from your machine:

```sh
curl http://127.0.0.1:8080/index.html
```

**The guests reach the outside world through NAT in nos**, using nos's own address. The switch answers a guest's DHCP requests itself. It hands out the port's address, with nos as the router and nos's DNS server. The BusyBox initramfs has no `/etc/resolv.conf`. For name lookups to work, point it at the DNS server that `ip=` gave the kernel:

```sh
hv exec 0 mkdir -p /etc && ln -sf /proc/net/pnp /etc/resolv.conf
```

NAT keeps guests away from private networks, link-local addresses (where a cloud's metadata server answers), the shared address space of carrier-grade NAT, and the subnet of nos's own uplink. Under QEMU's user networking, that last one includes the host at 10.0.2.2. `nat allow 10.0.2.2/32` lets guests reach it, and `nat` shows what NAT is doing. More is in [A network](/docs/hypervisor#a-network).

## 10. Guests with more than one CPU

`cpus=N` gives a guest N CPUs, up to 16. Each one is a vCPU task on a host CPU of its own. The guest's kernel must be Linux 6.6 or later with `SMP`, `X86_X2APIC` and `X86_MPPARSE`, which the recipe above has. For such a guest, the default command line leaves out `nolapic`. nos adds `noapic` itself, because the virtual machine has no IO-APIC unless you pass `ioapic`. Here is a trimmed run on one of the project's servers:

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

- **Each vCPU needs its own host CPU.** Two CPUs of one guest never share a host CPU, so a guest can't have more CPUs than `hv on` has turned the extension on for.
- **A guest gets whole cores.** By default (`hv cores own`), a guest's cores are its own, and no other guest has a CPU on them. A guest that would need a core another guest holds is refused, with a message saying so. `hv cores shared` lets guests share cores, for a machine whose guests are all yours. `hv cores` shows who holds which CPUs.
- **Under TCG, use QEMU 9.2 or later.** Before 9.2, when a guest has paging off, TCG does not send its memory accesses through the nested page table. A guest's second CPU starts in real mode, where paging is off, so it would run directly in nos's own memory and corrupt it. KVM and real hardware have always handled this correctly.

## 11. A distribution: Alpine

Alpine's `virt` ISO boots as it ships. You take its kernel and initramfs out of the ISO, and the ISO itself becomes the guest's read-only disk. Download `alpine-virt-<version>-x86_64.iso` from Alpine's site, then extract the files and put them in place (`xorriso` is packaged in most distributions):

```sh
mkdir -p rootdir/alpine
xorriso -osirrox on -indev alpine-virt-3.24.2-x86_64.iso -extract /boot/vmlinuz-virt rootdir/alpine/vmlinuz-virt
xorriso -osirrox on -indev alpine-virt-3.24.2-x86_64.iso -extract /boot/initramfs-virt rootdir/alpine/initramfs-virt
cp alpine-virt-3.24.2-x86_64.iso rootdir/alpine/alpine.iso
chmod 644 rootdir/alpine/*
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

Boot nos, load the hypervisor and start the guest:

```sh
hv start /alpine/vmlinuz-virt mem=512 initrd=/alpine/initramfs-virt disk=/alpine/alpine.iso:ro net restart cmdline=console=ttyS0 nolapic modules=loop,squashfs,sd-mod,usb-storage
hv wait 0 secs=600 login:
hv send 0 root\n
hv exec 0 cat /etc/alpine-release; uname -r
```

Alpine's initramfs loads `virtio_blk`, finds the ISO on `vda` and installs the base system from the ISO's packages into a tmpfs. Then OpenRC, Alpine's init system, brings it up to a login prompt on `ttyS0`. A getty (the program behind a login prompt) does not ask for the cursor position, so log in with `hv send`, which types right away. After the first `send`, `exec` also types its lines right away.

On the switch, Alpine's initramfs configures `eth0` from `ip=` and writes nos's DNS server to `resolv.conf`, so `apk` reaches its mirrors through NAT. `reboot` in the guest resets it, and the `restart` option boots it again. To wait for the next boot's login prompt, use `hv wait 0 boot=1 login:`. Debian's cloud image boots too; see [A distribution](/docs/hypervisor#a-distribution).

## On a server

On a server installed from the system image ([Install nos on a server](/howto/install-on-a-server)), the module is already at `/lib/modules/hv.ko`, so step 5 starts with `insmod /lib/modules/hv.ko`. Copy the guest's files to nos's partition from the server's Linux before you boot nos, or download them from inside nos with `wget`. The project's servers run guests under AMD-V on a Ryzen 5 3600 and under VT-x on an i5-13500 ([On real hardware](/docs/hypervisor#on-real-hardware)).

## Troubleshooting

**`hv: vm 0: the guest has not read the line -- it is not at a prompt; the line stays queued`.** The module never saw the guest's shell ask for the cursor position. Check that your BusyBox asks for it (step 2) and that the init script has `dmesg -n 1`. At a login prompt, use `hv send`.

**`hv exec` gives up while the guest is still booting.** By default it waits 10 s. Under TCG, give the first `hv exec` more time, as in `hv exec 0 secs=60 id`.

**`hv on` fails, or `hv info` shows no nested paging.** Start QEMU with `-cpu max` (TCG) or `-enable-kvm -cpu host` (KVM with nested virtualization on).

**`insmod /hv.ko` says `built against another kernel interface`.** `hv.ko` must come from the same build or the same release as the running kernel.

**A `cpus=2` guest comes up with one CPU.** The guest kernel is older than 6.6, or it lacks `SMP`, `X86_X2APIC`, `X86_MPPARSE` or a large enough `NR_CPUS`. An older kernel still boots, on one CPU.

**A guest with several CPUs is refused.** The free cores have too few CPUs for it. Stop a guest, turn the extension on for more CPUs, or use `hv cores shared`.

**`ping` in the guest stops after the first reply.** The guest kernel has no `POSIX_TIMERS`.

**A guest cannot reach 10.0.2.2 or another private address.** NAT keeps guests away from private networks and from nos's own subnet. Use `nat allow <net>` for the networks they may reach.

**A guest stops instead of rebooting.** Without `restart`, a reset stops the VM, and `hv list` shows why. Start the guest with `restart`, or run `hv restart <id>`.

**`hv off` refuses.** A started guest is still running on that CPU. Stop it with `hv stop` first, or run `rmmod hv`, which stops all of them.

## Next steps

- Let the project's gates do all of this. `nos/scripts/hv-linux-test.py --bzimage linux/arch/x86/boot/bzImage --initrd guest/initrd` boots a guest and runs the long-lived VMs, and `--disk`, `--net`, `--cpus N` and `--attach` cover the rest. `nos/scripts/hv-distro-test.py --iso alpine-virt-3.24.2-x86_64.iso` covers Alpine. Both use the `nos.iso` and the modules built in the tree ([Run the gates](/howto/run-the-gates)).
- [The hypervisor](/hypervisor), and in the reference: [A Linux guest](/docs/hypervisor#a-linux-guest), [Guests that stay up](/docs/hypervisor#guests-that-stay-up), [More than one CPU](/docs/hypervisor#more-than-one-cpu).
- [Install nos on a server](/howto/install-on-a-server) to run guests on real hardware.
