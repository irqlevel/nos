---
title: "The AX41 boot that said nothing"
description: "A kernel that died before any console existed, on a server whose only console is the network: two counters on its disks and a QEMU replica found why."
date: 2026-09-10
tags:
  - debugging
  - hardware
  - boot
  - qemu
  - x86-64
---

The AX41 is one of the three real machines nos boots on: a Hetzner dedicated server with an AMD Ryzen 5 3600 (Zen 2, 6 cores / 12 threads), two NVMe drives, an Intel I210 NIC and Ubuntu beside nos under GRUB ([Real hardware](/docs/real-hardware#hetzner-ax41-1-ltd-dedicated-server)). It has no serial port and no IPMI. Whatever nos has to say there, it says over the network — netconsole, the UDP shell, SSH — and all of that needs a NIC with an address. A kernel that dies before then dies in silence.

On 10 September 2026 the AX41's GRUB entries changed. nos would now be loaded from `nosenv`, a plain ext2 partition, and mount that same partition read-write as its root, `root=LABEL=nosenv`, so that a running nos could download and install its next kernel by itself ([6d3ffd5](https://github.com/irqlevel/nos/commit/6d3ffd5c61d6e8867d347015c2aeead7f2f046a4)). The first boot of the new entries produced nothing at all: no ping, nothing at the netconsole collector, no new boot in the disk log.

On this box that much is survivable by design. `nosboot` arms exactly one boot of nos, GRUB clears the flag before it loads anything, and a hardware reset comes back to Ubuntu. What the design does not give you is a reason.

## What silence still tells you

"Nothing" is a statement about time. Each channel opens at a known point of the boot, so silence on all of them bounds where the kernel died. The order then, from [`kernel/main.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/main.cpp):

```text
Main2
  ...
  ParseMultiBootInfo          the command line is read
  Netconsole::Setup           the netconsole ring starts capturing
  ...
BpStartup
  SoftIrq::Init               block requests can complete from here on
  PartitionDevice::ProbeNew   the disks' partition tables are read
  MountRootFs                 a read-write ext2 mount bumps the superblock's mount count
  DiskLog::Setup              the disk log's boot number + 1; the log so far goes to disk
  ...
  Netconsole::Start           datagrams leave once eth0 has an address
  boot: complete
```

The netconsole buffers from early on, but it sends only once the device has an address. The disk log holds every line from the first one in memory, but it can write only once the soft IRQ layer exists — a block request completes through it — so its setup sits right after the root mount. Silence on both says the kernel died somewhere between GRUB and `DiskLog::Setup`. That is most of the boot. To narrow it, we read the disks from Ubuntu, read-only.

## Two counters on the disks

The disk log's header carries a boot number, which the kernel bumps in `DiskLog::Setup`. [`scripts/disklog.py`](https://github.com/irqlevel/nos/blob/master/scripts/disklog.py) prints it:

```sh
sudo ./scripts/disklog.py read /dev/nvme0n1p1      # read the last boot
```

It had not moved. (At the time the log ran on every boot of a machine with a prepared area; since [0d70452](https://github.com/irqlevel/nos/commit/0d704526d5d421c69576d2e79a1af453a4e666df) it needs `disklog=on`, and without the parameter an unchanged number says nothing — check the entry's command line first.)

The second counter is ext2's own. On a read-write mount, nos's ext2 driver — C++ then, Rust now — does what Linux does ([`ext2.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/fs/src/ext2.rs)):

```rust
            self.sb.state &= !STATE_VALID;
            self.sb.mount_count = self.sb.mount_count.wrapping_add(1);
            self.sb.mount_time = wall_clock_secs() as u32;
```

Ubuntu mounts the same partition at `/nosenv` on every boot. So `dumpe2fs -h` gave the mount count, the journal gave the number of Ubuntu boots that had mounted the partition, and the two matched exactly. nos had never written its superblock: it died before it mounted its root.

That still left everything from GRUB to the root mount — the memory map, ACPI, the NVMe driver, the partition probe — each of them reading something particular to this machine and its disks.

## A replica instead of another reboot

Every guess tried on the real box costs a boot, a hardware reset and a boot back into Ubuntu. So we brought the box to QEMU instead:

1. The exact kernel binary from `/nosenv`, not a rebuild.
2. The real heads of both NVMe drives, copied over ssh: the first megabyte of each (the MBR and the GRUB gap), the disk log's area, the first 32 MiB of nosenv — superblock, group descriptors, the first group's bitmaps and inode table, the root directory — and the first 64 KiB of each RAID member.
3. Two sparse raw images the size of the real drives, with the same partition table; an ext2 made with nosenv's geometry, label and UUID at the partition's offset; and the real heads written over it. `e2fsck` passed the result.
4. An ISO from `grub-mkrescue` with the box's command line, booted with the two NVMe drives in the box's order and QEMU's `igb`, the 82576 — the closest model to the I210.

```sh
mkfs.ext2 -q -F -b 4096 -I 256 -L nosenv -U <uuid of nosenv> -E offset=1048576 nvme1.img 8388608
e2fsck -fn "nvme1.img?offset=1048576"
qemu-system-x86_64 -smp 4 -cpu max -m 4096 -cdrom replica.iso \
    -drive file=nvme0.img,format=raw,id=n0,if=none -device nvme,serial=n0,drive=n0 \
    -drive file=nvme1.img,format=raw,id=n1,if=none -device nvme,serial=n1,drive=n1 \
    -device igb,netdev=net0 -netdev user,id=net0 \
    -serial file:replica.log
```

The replica has what the box lacks, a serial port. It died at once, before it touched a disk:

```text
Key netconsole value 10.0.2.2:6666
PANIC:ParseMultiBootInfo():grub.cpp,148: Can't parse command line
```

## Two equals signs

`Parameters::ParseParameter` split a token into key and value with `StrChrOnce(param, '=')`, a helper that returns `nullptr` when the separator occurs more than once. `root=LABEL=nosenv` has two. With no separator found, the token was taken for a bare word; the one bare word the parser takes is `ro`; so the parse failed, and on x86 a command line that failed to parse was a panic ([`grub.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/x86_64/grub.cpp)):

```cpp
            Trace(0, "Cmdline %s", cmdLine->String);
            if (!Kernel::Parameters::GetInstance().Parse(cmdLine->String)) {
                Trace(0, "Can't parse command line");
                Panic("Can't parse command line");
            }
```

`ParseMultiBootInfo` runs early in `Main2`: before the netconsole ring is armed, before the disk log exists, before any NIC. The address the log was to be sent to was itself a parameter on the line that killed the boot — the replica's log shows it parsed just before the panic. `root=LABEL=` and `root=UUID=` had been documented since they were added the day before; the smoke test boots `root=auto` and had met neither.

The fix, [259bb5c](https://github.com/irqlevel/nos/commit/259bb5c81a168f16979726d2ac8718fc2cee5c72), is one call:

```diff
-    const char* sep = Stdlib::StrChrOnce(param, '=');
+    /* The key ends at the first '=': the value may hold one of its own, as
+       root=LABEL=<label> and root=UUID=<uuid> do */
+    const char* sep = Stdlib::StrChr(param, '=');
```

The boot self-test written with it — `TestParameters` parses `root=LABEL=`, `root=UUID=` with `ro`, and `root=<device>` into scratch instances — at once found a second case of the same thing: tokens shorter than three characters were refused before the parser looked at them, and `ro` has two. Its branch was unreachable. The floor is two now.

## Nothing on the command line stops the boot

That fixed this line. The question it left was why any line could kill the boot at all. On x86 the parse failed on any one token it could not take — a bare word such as `quiet` or `splash`, a Linux habit; a parameter with nothing before or after its `=`; one longer than 48 characters; a line longer than 255 — and on this box each of those was a silent death. On arm64 it failed the other way: the result was ignored, and every parameter after the first bad one was quietly dropped.

The same evening, [ade11b8](https://github.com/irqlevel/nos/commit/ade11b8b0a1c7fdd35b7517c1a4903635a749f0b) made `Parse` take what it can and name what it cannot, as Linux does:

```text
Cmdline: ignoring 'quiet'
```

`grub.cpp` no longer panics, and the warnings go to dmesg, which the netconsole replays from the start of the boot. A line longer than the buffer is read up to its last whole parameter, because half a parameter is a different one: `root=LABEL=nosenv` cut short is `root=LABEL=nos`. That copy is a `StrnCpy`, because at the time `SnPrintf` kept nothing at all when its output did not fit — which is also why the old parser had read no part of an over-long line. On the replica, a 269-character line of junk around the box's own parameters had each bad token named and skipped, nosenv mounted read-write, and the boot reached `boot: complete` with a DHCP lease.

At the end of the month the command line got a fuzz target of its own ([Fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)): lines of every parameter the kernel takes, words it does not know, lines cut anywhere, every getter held to a reader written from `parameters.h` — a parameter the buffer's end cuts through left out whole, never read as the shorter one it would be. It also found `Parameters`'s constructor leaving two members unset, zeros only because the object's storage is static.

## Consoles of last resort

What a machine like this has:

- **netconsole**, `netconsole=ip:port` ([Netconsole](/docs/netconsole)). Every traced line goes into a 128 KiB ring from the moment the command line is known; the backlog goes out once the device has an address, in datagrams with sequence numbers, so a gap can be told from a wedge; `nctail=N` spends a short-lived link on the newest lines. [`scripts/netconsole.py`](https://github.com/irqlevel/nos/blob/master/scripts/netconsole.py) receives it.
- **The disk log**, `disklog=on`. Every line from the first, onto a raw partition prepared from the other OS with `scripts/disklog.py format`, and read back from there. Since 0d70452 it is off unless asked for — a forced write had cost 25–33 ms a line on real hardware — a line is queued without a lock and written by a task of its own, and a panic pushes out what is queued.
- **`panic=N`** ([b32a3b5](https://github.com/irqlevel/nos/commit/b32a3b58478a05f1b4b0c6db3af54d2a6d9e2865)): a reset N seconds after a panic's report has gone out, so a one-shot boot that dies brings the server back to its own OS by itself. The system image passes `panic=10` ([installing on a server](/howto/install-on-a-server)).

Both log channels have gates now, `netconsole-test.py` and `disklog-test.py`, because each is the only console somewhere ([Tests and gates](/docs/testing)).

And the gap this boot fell into is still there, named in a comment in `main.cpp`: the disk log can write nothing before the soft IRQ layer and the root mount, so a machine that stops before that point leaves nothing on disk, and closing it needs a polled completion path in the NVMe driver. Until then a silent boot is read the way this one was — from the order of the boot, two counters on the disks, and a replica with a serial port.
