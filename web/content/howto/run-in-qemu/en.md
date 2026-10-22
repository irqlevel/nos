---
title: "Run nos in QEMU"
description: "Boot nos on x86-64 from the ISO or a disk image, under KVM on Linux, and on arm64 with HVF on Apple Silicon, then tell a good boot from a bad one."
order: 2
updated: 2026-10-22
---

By the end of this guide nos will be running in QEMU, with its shell in front of you and its serial log in a file. You will also know which log lines mean the boot went well, and which kernel parameters to change when it didn't.

You need:

- the files from [Build nos](/howto/build): `nos.iso` to boot x86-64 from the ISO, `nos.qcow2` to boot it from the disk image, and `nos-arm64.img` for arm64;
- QEMU on the host: `qemu-system-x86_64`, `qemu-system-aarch64` and `qemu-img`;
- Linux with KVM for `scripts/run.sh`, and an Apple Silicon Mac for HVF. Every other setup runs under TCG anywhere, only slower;
- host ports 2222 and 9000 free: the scripts forward them into the guest.

## The scripts

| Script | Boots | Machine | Console |
|---|---|---|---|
| `scripts/qemu.sh` | `nos.iso` | 2 CPUs, virtio-net | the QEMU window; serial port to `nos.log` |
| `scripts/qemu-disk.sh` | `nos.qcow2` | 4 CPUs, 8 GB, virtio-blk, virtio-scsi with three disks, NVMe, virtio-net, virtio-rng | serial port on your terminal |
| `scripts/run.sh` | `nos.iso` under KVM | every host CPU, virtio-scsi, `isa-debug-exit` | VNC display `:0`; serial port to `nos.log` |
| `scripts/qemu-arm64.sh` | `nos-arm64.img` on `virt` | 4 CPUs, 1 GB, GICv3, virtio-mmio blk, net and rng | serial port to `nos-arm64.log` |

`qemu.sh`, `qemu-disk.sh` and `qemu-arm64.sh` forward host TCP port 2222 to the guest's port 22, for [SSH](/howto/connect). `qemu-disk.sh` and `qemu-arm64.sh` also forward UDP port 9000, for the UDP shell. `qemu.sh`, `qemu-disk.sh` and `run.sh` start QEMU's GDB stub on port 1234 (`-s`). `qemu-arm64.sh` passes any extra arguments on to QEMU, so `./scripts/qemu-arm64.sh -s` does the same there.

## Boot x86-64 from the ISO

```sh
./scripts/qemu.sh
```

GRUB boots the kernel with the command line in [`build/grub.cfg`](https://github.com/irqlevel/nos/blob/master/build/grub.cfg): `dhcp=auto dns=on root=auto fstest=on`. No disk is attached, so `/` is a ramfs, and nothing you write survives a reboot. `fstest=on` runs the filesystem self-test on it. The script doesn't enable KVM, so QEMU emulates the CPU with TCG and the boot is slow: the smoke test allows it up to 300 seconds. For KVM on Linux, use `run.sh` (below).

To use the shell, click into the QEMU window and type. Everything the shell prints also goes to the serial port, which this script writes to `nos.log` along with the boot log. To follow the log from another terminal:

```sh
tail -f nos.log
```

To keep files across reboots, give the ISO boot a root filesystem: an ext2 image labelled `nos`, which `root=auto` finds and mounts read-write (see [Making a root filesystem](/docs/filesystems#making-a-root-filesystem)). The following is the `qemu.sh` machine with that disk added:

```sh
./scripts/mkrootfs.sh root.img 256
qemu-system-x86_64 -smp 2 -cdrom nos.iso -serial file:nos.log -vga std \
    -drive file=root.img,format=raw,if=virtio \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0,hostfwd=tcp::2222-:22
```

`mkrootfs.sh` runs `mkfs.ext2` directly when e2fsprogs is installed, and inside the `nos-builder` image otherwise.

### Boot under UEFI

The same ISO boots under OVMF. GRUB then hands the kernel a pixel framebuffer, which nos draws with its 8x16 font console. Adjust the firmware paths below to where your system keeps OVMF:

```sh
cp /usr/share/OVMF/OVMF_VARS_4M.fd /tmp/ovmf_vars.fd
qemu-system-x86_64 -smp 2 -m 1G -cdrom nos.iso -serial file:nos.log \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,unit=1,file=/tmp/ovmf_vars.fd
```

## Boot x86-64 from the disk image

```sh
./scripts/build-disk.sh
./scripts/qemu-disk.sh
```

`qemu-disk.sh` boots `nos.qcow2` from virtio-blk and attaches the other devices the drivers cover: a legacy virtio-scsi controller with three disks, an NVMe disk, virtio-net and virtio-rng. The guest gets 4 CPUs and 8 GB of memory. On the first run the script creates the scratch disks `scsi-disk0.qcow2`, `scsi-disk1.qcow2`, `scsi-disk2.qcow2` and `nvme-disk0.qcow2` in the repository root, and later runs reuse them.

The disk's GRUB entry ([`build/grub-disk.cfg`](https://github.com/irqlevel/nos/blob/master/build/grub-disk.cfg)) boots with `console=serial dhcp=auto dns=on root=auto`, and the script runs QEMU with `-nographic`, so GRUB and then the shell appear on your terminal. The root is the disk's ext2 partition, mounted read-write, so whatever you write from the shell is still there after a reboot. To end the session, run `poweroff` in the shell, or press QEMU's escape sequence, Ctrl-A then X.

## Boot x86-64 under KVM

On Linux with KVM:

```sh
./scripts/run.sh
```

The script gives the guest every host CPU, KVM, and a small virtio-scsi scratch disk (`scsi-disk.img`, zeroed on every run). It writes the serial port to `nos.log` and puts the screen on VNC display `:0`: connect a VNC viewer to type at the shell. The script prints each command as it runs it. It adds no virtio-net device, so in this boot nos has no network card it can drive. It does attach QEMU's `isa-debug-exit` device, which `poweroff` falls back to when ACPI doesn't turn the machine off. The shortest equivalent, from [Run](/docs/run):

```sh
qemu-system-x86_64 -enable-kvm -smp 8 -cdrom nos.iso -serial file:nos.log \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
```

## Boot arm64 on QEMU `virt`

```sh
./scripts/qemu-arm64.sh
```

On an Apple Silicon Mac the script uses HVF (`-accel hvf -cpu host`), about four times as fast as TCG. Anywhere else it uses TCG with a Cortex-A72. To force TCG on a Mac:

```sh
NOS_TCG=1 ./scripts/qemu-arm64.sh
```

QEMU loads the kernel with `-kernel nos-arm64.img`, the command line is `dhcp=auto dns=on root=auto`, and the serial port goes to `nos-arm64.log`; there is no display. You can read the log, but you can't type at the kernel through it. To use the shell, turn on the UDP shell (`NOS_UDPSHELL=9000 ./scripts/qemu-arm64.sh`) or log in over SSH, as described in [Connect to a running nos](/howto/connect). The script `exec`s QEMU, so Ctrl-C in its terminal stops the machine.

The guest's virtio disk is `nos-arm64.qcow2`, which the script creates empty (256 MB) on the first run. With no filesystem on that disk, `/` is a ramfs. For a root that keeps what you write, replace the disk with an ext2 image:

```sh
./scripts/mkrootfs.sh root-arm64.img 256
qemu-img convert -f raw -O qcow2 root-arm64.img nos-arm64.qcow2
```

## What a good boot looks like

Each line of the boot log has the form `level:seconds.microseconds:function(),file,line: message`. Three markers divide the boot, and the smoke tests wait for all of them:

- `After test`: the boot self-tests passed;
- `Preempt is now on`: the scheduler is running and the other CPUs are up;
- `boot: complete`: the shell and the network services are running.

On x86-64 they appear in that order. On arm64 the order is different: `Self test passed` and `Cpus started, N running` come first, then `Preempt is now on`, then `After test` (there it follows the multitasking test), and finally `boot: complete`. The `MountRootFs:` lines say what was mounted as `/`, and a boot with `fstest=on` prints `fstest: passed`. A `PANIC:` anywhere in the log, including `RUST PANIC:`, means the boot failed; see [Debug nos with GDB](/howto/debug-with-gdb) for how to read one.

```sh
grep -e 'After test' -e 'Preempt is now on' -e 'boot: complete' -e 'PANIC:' nos.log
```

After `boot: complete` the shell prints its banner, gets a DHCP lease (QEMU's user network hands out `10.0.2.15`) and shows its prompt:

```text
  _   _  ___  ____
 | \ | |/ _ \/ ___|
 |  \| | | | \___ \
 | |\  | |_| |___) |
 |_| \_|\___/|____/

DHCP auto on eth0...
DHCP ip: 10.0.2.15
DNS resolver started, server: 10.0.2.3
$
```

To check a build without watching the boot, run it headless and read the exit code: 0 means every marker appeared, and 1 means the boot failed, for example with a panic or a timeout. The third command runs the arm64 test under HVF instead of TCG:

```sh
./scripts/smoke-test.sh --skip-build
./scripts/smoke-arm64.sh --skip-build
SMOKE_HVF=1 ./scripts/smoke-arm64.sh --skip-build
```

The smoke scripts create their own scratch disks and write the log to a temporary directory. `SMOKE_TIMEOUT` (300 seconds by default) sets how long to wait, and `SMOKE_LOG` sets the log's path. Without `--skip-build` they first build in the `nos-builder` container.

## First commands to try

| Command | What it shows |
|---|---|
| `help` | every command, including the ones modules add |
| `version` | the release and the commit the kernel was built from |
| `uptime`, `date` | time since boot; wall-clock time |
| `lscpu` | the CPU and the features the kernel depends on |
| `ps`, `top` | the tasks; CPU use per task over a sampling window |
| `meminfo`, `memusage` | the firmware memory map and how much of it the kernel uses |
| `pci`, `disks`, `mounts`, `ls /` | PCI devices, block devices, mount points, files |
| `net`, `arp`, `nslookup example.com` | network devices and counters, the ARP table, a DNS lookup |
| `dmesg 40`, `dmesg 100 virtio` | the newest kernel log lines, optionally filtered |
| `lsmod` | the loaded modules |
| `profile` | where the kernel spends its time, as whole call chains |
| `poweroff`, `reboot` | power off (ACPI S5), reset |

While the shell is running, trace output no longer goes to the console. It still goes to the kernel log, which `dmesg` reads. The full list of commands is in [Shell commands](/docs/shell-commands).

## Kernel parameters

The kernel command line is a list of `key=value` pairs separated by spaces. Where you set it depends on how you boot:

| Boot | Where the command line is | After a change |
|---|---|---|
| x86-64 ISO (`qemu.sh`, `run.sh`) | the `multiboot2` line in `build/grub.cfg` | rebuild `nos.iso` (`make nocheck`) |
| x86-64 disk (`qemu-disk.sh`) | the `multiboot2` line in `build/grub-disk.cfg` | rerun `./scripts/build-disk.sh` |
| arm64 (`qemu-arm64.sh`) | `APPEND=` in `scripts/qemu-arm64.sh` | nothing; the next boot reads it |

Nothing on the command line stops a boot: the kernel logs an unknown key or value and skips it. The line is limited to 255 characters. When a boot fails, these parameters take one subsystem at a time out of the picture:

| Parameter | Effect |
|---|---|
| `smp=off` | runs on the boot CPU only |
| `maxcpus=N` | brings up at most N CPUs, the boot CPU included |
| `loglevel=N` | sets the trace level to boot with, 0–5 (default 1) |
| `usb=off` | x86-64: skips xHCI bring-up |
| `its=off` | arm64: turns off the GICv3 ITS; PCIe MSI degrades, virtio-mmio is unaffected |
| `hwrng=off` | ignores the CPU's random-number instruction |
| `rc=off` | skips `/etc/rc` |
| `dhcp=off` | turns DHCP off entirely |
| `console=serial`, `console=vga` | puts the shell on one console only |
| `trace=vga` | x86-64: shows the boot log on the screen as well as the serial port |

Try them in this order: `smp=off`, then `maxcpus=2`, then `loglevel=4`, then the subsystems one by one. [Debug nos with GDB](/howto/debug-with-gdb) covers the rest of the method, and [Kernel parameters](/docs/kernel-parameters) lists every parameter.

## Other ways to run nos

- `scripts/qemu-igb.sh` boots the ISO with QEMU's Intel 82576 (`igb`) NIC instead of virtio-net. It's the only way to run a hardware NIC driver without real hardware, and it needs QEMU 8.0 or later. `NOS_PCAP=1` also writes a packet capture to `nos-igb.pcap`, and `NOS_TRACE=1` writes the NIC model's trace to `nos-igb-trace.log`.
- `nos.qcow2` boots on Google Cloud and other KVM clouds; see [Run: Google Cloud](/docs/run#google-cloud).

## Troubleshooting

- **QEMU exits at once with an error about a host forwarding rule.** Host port 2222 or 9000 is already in use, often by another QEMU that is still running.
- **HVF isn't used.** The script uses HVF only on an Apple Silicon Mac (when `uname -sm` prints `Darwin arm64`). Everywhere else it falls back to TCG.
- **The boot is slow.** TCG emulates every instruction. On a Mac, x86-64 always runs under TCG, and arm64 does with `NOS_TCG=1`.
- **Nothing you type reaches the arm64 kernel.** Its serial port is a file. Use the UDP shell or SSH instead.
- **Files are gone after a reboot.** `/` was a ramfs; the log says `MountRootFs: mounted ramfs on / (rw)`. Boot the disk image, or attach a root image as shown above.
- **The shell prints `DHCP auto timeout`.** nos found its network card but got no DHCP lease within ten seconds: check the `-netdev` part of the QEMU command line. With no network card it can drive, the shell prints no DHCP line at all.

## Next steps

- [Connect to a running nos](/howto/connect): the UDP shell, SSH and netconsole
- [Debug nos with GDB](/howto/debug-with-gdb)
- [Run the gates](/howto/run-the-gates)
- [Boot a Linux guest under the nos hypervisor](/howto/linux-guest)
- [Boot](/docs/boot): what happens between GRUB and `boot: complete`
