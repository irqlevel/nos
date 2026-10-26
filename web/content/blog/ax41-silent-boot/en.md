---
title: "The AX41 boot that said nothing"
description: "A kernel died before it could say a word, on a server whose only console is the network. Two counters on its disks and a QEMU replica showed why."
date: 2026-09-10
tags:
  - debugging
  - hardware
  - boot
  - qemu
  - x86-64
---

The AX41 is one of the three real machines nos boots on: a Hetzner dedicated server with an AMD Ryzen 5 3600 (Zen 2, 6 cores / 12 threads), two NVMe drives, an Intel I210 network card, and Ubuntu next to nos in the GRUB menu ([Real hardware](/docs/real-hardware#hetzner-ax41-1-ltd-dedicated-server)). It has no serial port and no IPMI. Whatever nos has to say there goes over the network: netconsole (the kernel log sent to a UDP collector), the UDP shell or SSH. All of these need a network card with an address, so a kernel that dies before that dies in silence.

On 10 September 2026 we changed the AX41's GRUB entries. GRUB would now load nos from `nosenv`, a plain ext2 partition, and nos would mount the same partition read-write as its root, `root=LABEL=nosenv`. That way a running nos could download and install its next kernel by itself ([6d3ffd5](https://github.com/irqlevel/nos/commit/6d3ffd5c61d6e8867d347015c2aeead7f2f046a4)). The first boot from the new entries produced nothing at all: no reply to ping, nothing at the netconsole collector, no new boot in the disk log (which nos writes to a raw partition).

On this machine that is survivable by design. The `nosboot` script arms exactly one boot of nos, GRUB clears the flag before it loads anything, and a hardware reset brings the server back to Ubuntu. What the design does not give you is a reason.

## What silence still tells you

"Nothing" still tells you something about time. Each channel starts working at a known point of the boot, so silence on all of them limits how far the kernel can have got. Here is the boot order at the time, from [`kernel/main.cpp`](https://github.com/irqlevel/nos/blob/master/src/base/kernel/main.cpp):

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

Netconsole buffers from early on, but sends nothing until the device has an address. The disk log keeps every line in memory from the first one, but it cannot write until the soft IRQ layer exists. That layer runs the deferred half of interrupt handling, and block requests complete through it, so the disk log is set up right after the root mount. Silence on both meant the kernel died somewhere between GRUB and `DiskLog::Setup`, which is most of the boot. To narrow it down, we read the disks from Ubuntu, read-only.

## Two counters on the disks

The disk log's header holds a boot number, which the kernel increments in `DiskLog::Setup`. [`scripts/disklog.py`](https://github.com/irqlevel/nos/blob/master/scripts/disklog.py) prints it:

```sh
sudo ./scripts/disklog.py read /dev/nvme0n1p1      # read the last boot
```

It had not moved. (At the time the log ran on every boot of a machine with a prepared area. Since [0d70452](https://github.com/irqlevel/nos/commit/0d704526d5d421c69576d2e79a1af453a4e666df) it needs `disklog=on`, and without it an unchanged number says nothing, so check the entry's command line first.)

The second counter is ext2's own. When nos's ext2 driver (C++ then, Rust now) mounts a filesystem read-write, it does what Linux does ([`ext2.rs`](https://github.com/irqlevel/nos/blob/master/src/crates/fs/src/ext2.rs)):

```rust
            self.sb.state &= !STATE_VALID;
            self.sb.mount_count = self.sb.mount_count.wrapping_add(1);
            self.sb.mount_time = wall_clock_secs() as u32;
```

Ubuntu mounts the same partition at `/nosenv` on every boot. `dumpe2fs -h` gave the mount count, Ubuntu's journal gave the number of Ubuntu boots that had mounted the partition, and the two matched exactly. So nos had never written its superblock: it died before it mounted its root.

That still left everything from GRUB to the root mount: the memory map, ACPI, the NVMe driver, the partition probe. Each of them reads something specific to this machine and its disks.

## A replica instead of another reboot

Every guess tried on the real server costs a boot, a hardware reset and a boot back into Ubuntu. So we brought the server to QEMU instead:

1. We took the exact kernel binary from `/nosenv`, not a rebuild.
2. We copied the real beginnings of both NVMe drives over ssh: the first megabyte of each (the MBR and the gap after it where GRUB lives), the disk log's area, the first 32 MiB of nosenv (superblock, group descriptors, the first group's bitmaps and inode table, the root directory), and the first 64 KiB of each RAID member.
3. We made two sparse raw images the size of the real drives, with the same partition table. At the partition's offset we created an ext2 with nosenv's geometry, label and UUID, then wrote the real beginnings over it. The result passed `e2fsck`.
4. We built an ISO with `grub-mkrescue` and the server's command line. We booted it with the two NVMe drives in the server's order and with QEMU's `igb`, which emulates the 82576, the closest model to the I210.

```sh
mkfs.ext2 -q -F -b 4096 -I 256 -L nosenv -U <uuid of nosenv> -E offset=1048576 nvme1.img 8388608
e2fsck -fn "nvme1.img?offset=1048576"
qemu-system-x86_64 -smp 4 -cpu max -m 4096 -cdrom replica.iso \
    -drive file=nvme0.img,format=raw,id=n0,if=none -device nvme,serial=n0,drive=n0 \
    -drive file=nvme1.img,format=raw,id=n1,if=none -device nvme,serial=n1,drive=n1 \
    -device igb,netdev=net0 -netdev user,id=net0 \
    -serial file:replica.log
```

The replica has what the server lacks, a serial port. It died at once, before touching a disk:

```text
Key netconsole value 10.0.2.2:6666
PANIC:ParseMultiBootInfo():grub.cpp,148: Can't parse command line
```

## Two equals signs

`Parameters::ParseParameter` split each token into a key and a value with `StrChrOnce(param, '=')`. That helper returns `nullptr` when the separator occurs more than once, and `root=LABEL=nosenv` has two. With no separator found, the parser took the token for a bare word, a parameter with no value. The only bare word it accepts is `ro`, so the parse failed. On x86, a command line that failed to parse meant a panic ([`grub.cpp`](https://github.com/irqlevel/nos/blob/master/src/base/arch/x86_64/grub.cpp)):

```cpp
            Trace(0, "Cmdline %s", cmdLine->String);
            if (!Kernel::Parameters::GetInstance().Parse(cmdLine->String)) {
                Trace(0, "Can't parse command line");
                Panic("Can't parse command line");
            }
```

`ParseMultiBootInfo` runs early in `Main2`: before the netconsole ring starts capturing, before the disk log exists, before any network card. Even the address the log was to be sent to was a parameter on the line that killed the boot. The replica's log shows it parsed just before the panic. `root=LABEL=` and `root=UUID=` had been documented since they were added the day before, but the smoke test boots with `root=auto` and had tried neither.

The fix, [259bb5c](https://github.com/irqlevel/nos/commit/259bb5c81a168f16979726d2ac8718fc2cee5c72), changes one call:

```diff
-    const char* sep = Stdlib::StrChrOnce(param, '=');
+    /* The key ends at the first '=': the value may hold one of its own, as
+       root=LABEL=<label> and root=UUID=<uuid> do */
+    const char* sep = Stdlib::StrChr(param, '=');
```

The boot self-test written with it, `TestParameters`, parses `root=LABEL=`, `root=UUID=` with `ro`, and `root=<device>` into scratch instances. It at once found a second bug of the same kind. Tokens shorter than three characters were rejected before the parser looked at them, and `ro` has two, so its branch could never run. The minimum is two now.

## Nothing on the command line stops the boot

That fixed this line, but left a question: why could any line kill the boot at all? On x86, one token the parser could not take failed the whole parse. That could be a bare word such as `quiet` or `splash` (a Linux habit), a parameter with nothing before or after its `=`, one longer than 48 characters, or a line longer than 255. On this server each of those was a silent death. On arm64 it failed the other way: the result was ignored, and every parameter after the first bad one was quietly dropped.

The same evening, [ade11b8](https://github.com/irqlevel/nos/commit/ade11b8b0a1c7fdd35b7517c1a4903635a749f0b) made `Parse` behave like Linux. It takes what it can and names what it cannot:

```text
Cmdline: ignoring 'quiet'
```

`grub.cpp` no longer panics, and the warnings go to dmesg, which netconsole replays from the start of the boot. A line longer than the buffer is read up to its last whole parameter, because half a parameter is a different one: `root=LABEL=nosenv` cut short is `root=LABEL=nos`. The copy uses `StrnCpy`, because at the time `SnPrintf` kept nothing at all when its output did not fit. That is also why the old parser had read no part of an over-long line. On the replica we tried a 269-character line with junk around the server's own parameters. Each bad token was named and skipped, nosenv was mounted read-write, and the boot reached `boot: complete` with a DHCP lease.

At the end of the month the command line got a fuzz target of its own ([Fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)). It builds lines from every parameter the kernel takes and words it does not know, cuts them anywhere, and checks every getter against a reader written from `parameters.h`. A parameter that the end of the buffer cuts through must be left out whole, never read as the shorter one it would be. The fuzzer also found that the `Parameters` constructor left two members unset. They were zero only because the object lives in static storage.

## Consoles of last resort

Here is what a machine like this can fall back on:

- **netconsole**, `netconsole=ip:port` ([Netconsole](/docs/netconsole)). From the moment the command line is known, every traced line goes into a 128 KiB ring. Once the device has an address, the backlog goes out in datagrams with sequence numbers, so you can tell a gap from a hang. If the link may not live long, `nctail=N` spends it on the newest lines. [`scripts/netconsole.py`](https://github.com/irqlevel/nos/blob/master/scripts/netconsole.py) receives it.
- **The disk log**, `disklog=on`. It writes every line, from the first, to a raw partition prepared from the other OS with `scripts/disklog.py format`, and is read back from there. Since 0d70452 it is off unless asked for, because a forced write had cost 25–33 ms a line on real hardware. A line is now queued without a lock and written by a task of its own, and a panic flushes what is queued.
- **`panic=N`** ([b32a3b5](https://github.com/irqlevel/nos/commit/b32a3b58478a05f1b4b0c6db3af54d2a6d9e2865)) resets the machine N seconds after the panic report has gone out, so a one-shot boot that dies brings the server back to its own OS by itself. The system image passes `panic=10` ([installing on a server](/howto/install-on-a-server)).

Both log channels now have gates, `netconsole-test.py` and `disklog-test.py`. A gate is a test that every change to its code must pass, and these two exist because on some machine each channel is the only console ([Tests and gates](/docs/testing)).

The gap this boot fell into is still there, and a comment in `main.cpp` names it. The disk log can write nothing before the soft IRQ layer and the root mount, so a machine that stops earlier leaves nothing on disk. Closing the gap needs a polled completion path in the NVMe driver, one that does not wait for the soft IRQ layer. Until then we read a silent boot as we read this one: from the order of the boot, two counters on the disks, and a replica with a serial port.
