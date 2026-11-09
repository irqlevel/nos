# Tests and gates

There is no test runner and no separate test binary. What checks the kernel
is, from the most often run to the least: the self-tests every boot runs,
the smoke boots CI runs on both architectures, and a gate per subsystem for
what a smoke boot cannot notice. Each of them is judged by its **exit code**,
never by grepping what it printed: a build that stops on a stale dependency
file says "No rule to make target", not "error", and a log that was never
written contains no `PANIC:` either.

The short form of this page -- which gate to run after touching what -- is
the table in [`CLAUDE.md`](../CLAUDE.md#gates). This is the long one: what
each gate does, and the failure it is there for.

## Self-tests at boot

Self-tests live in `src/base/kernel/test.cpp`; each is a
`stdlib::Error TestXxx()` function. They run early in boot, from
`test::test()` (called in `main.cpp:main2`, and from `main_arm64.cpp`) and
`test::test_multi_tasking()`, and a failing one returns a non-success
`stdlib::Error`. The Rust side's run from the same place, through
`rust_test` (`src/crates/kernel/src/lib.rs`) -- among them the frame test
(`kernel/src/frames.rs`): a page mapped nowhere, written a piece at a time at
every alignment and read back on every running CPU, each through its own
slot of the temporary window, which is how every copy of a guest's memory is
made. A slot that maps the wrong page fails it (`frame selftest: ...`), and
so the smoke boot, on both architectures. The ring test
(`kernel/src/ring.rs`) takes `kcore::ring::LocklessRing` -- what the frame
pool and netblk pass frames and requests through -- full and empty lap after
lap, then runs one producer and one consumer through two cells, and two of
each through sixteen, each task on a CPU of its own where there are four:
nothing lost, nothing twice, each producer's values in its order, and what a
producer wrote before a push seen by whoever pops it. That last is what a
memory ordering too weak would break on arm64 alone, and the one-to-one run
is what catches it: a ring whose sequences were relaxed failed eight boots
of eight under HVF, while TCG on an x86 host may never show it. Its line
says what a push and a pop cost on one CPU and how long each run took
(`ring selftest: passed ...`): a number every boot prints is noticed when it
moves. The [management bus](mgmt.md)'s (`layers/mgmt/src/selftest.rs`) uses a
provider of its own:

- a descriptor that does not say what the provider has refused: an empty
  one, one with a method of the wrong kind;
- calls answered, and refused as they should be;
- an answer held to its limit;
- a job run to its end, its progress, log and result kept;
- a job told to stop by its provider's unregister, which waits for it;
- the name free again after.

`system::selftest` (`kernel/src/system.rs`) then calls every read of every
provider in the image -- `system`'s, `net`'s, `block`'s, `fs`'s -- as a
client would, and holds each answer to being JSON as the bus reads it: a
provider that writes past its own object, leaves a list open or puts a
fraction in one fails the boot here, not in the panel. And it reads the log
by number: a line logged just before must come back after the number the log
had, and nothing at or before the number asked after.

Then `test_module_bus` does the same for a module, once there are tasks to
run its job on:

- modtest is loaded, and its provider is called through `mgmt call`;
- a job of it is started and seen running;
- the module's unload must stop that job and wait it out: its record says
  `cancelled`, and the provider goes with its module.

An unregister that does not tell its jobs to stop, or does not wait for its
calls, fails the boot (`mgmt selftest: ...`). After it `test_module_unload`
holds an unload to its order ([Unloading](modules.md#unloading)): a call of
`modtest hold` is running on a task of its own as modtest is unloaded, and
returns only once the module's stop has run. The stop must find the
module's command and provider closed already, the call says so as it
returns, and modtest's drop panics if a call of its command is still running
-- so a loader that drops a module beside a call of its own, that never
calls the stop, or calls it before the close, fails the boot. And on every CPU `test_sleep_prompt` has a task of its own sleep a
millisecond twenty-one times while the CPU halts between interrupts: the
median sleep must be under 5 ms -- 1.0-2.4 under HVF and TCG, 10.0 when the
CPU's timer is not armed for a sleeper's deadline and the tick wakes it
([the local timer](scheduler.md#the-local-timer)).
Once, from the boot CPU with the others up, `test_task_locate` finds tasks
where they are ([waiting for one to end](scheduler.md#waiting-for-one-to-end)):
one blocked on an event, read off its stack; one on another CPU with
interrupts off, which must be named as that CPU's after the tenth of a
second the test waits for it; one spinning on that CPU, caught there by the
IPI -- through the slot the deaf CPU answered late, which has to be
reusable -- each held to frames naming the function it is in; and a join
of 50 ms reported at a report interval of 20. After it `test_wake_storm`
passes a token round a ring of tasks, two a CPU, each pinned to its own and
each pass a wakeup on another CPU -- half the waits with a deadline, a sleep
now and then racing the signal that comes for it, a task yielding on every
other CPU -- and fails the boot when the token stops for two seconds, naming
the task it was sent to and where that task is: a wakeup lost is a task
asleep for good, which nothing else would report
([blocking and waking](scheduler.md#blocking-and-waking)). Its line says how
many passes in how long; a control run that dropped one wakeup in 1500
failed the boot at the 1633rd. `test_idle_takes_turns` has the boot, which runs
in the boot CPU's idle task, sleep a millisecond beside a task pinned to that
CPU that only yields, and fails it if the sleep takes 200 ms: the idle task is
the last resort only while it idles ([the idle task](scheduler.md#the-idle-task)).
And `test_eevdf`, on a CPU other than the boot's, holds EEVDF's two promises
([who runs next](scheduler.md#who-runs-next-eevdf)): a task woken beside one
task that never blocks, and beside two, runs within half a millisecond and a
millisecond at the median -- signalled at random points of the spinners'
slices, since at a fixed rhythm a lag reckoned wrong slipped under the bound
-- and a spinner of nice 0 has 2.4 to 3.9 times the CPU of one of nice 5,
for 3.06. Its line gives both medians and the share; a control run with the
lag reckoned wrong waited 1256 us beside one spinner and failed the boot.
Then `test_affinity`, on two CPUs other than the boot's, gives a task that
spins and one that sleeps a millisecond at a time another CPU for its
affinity and fails the boot if either is not on it within 100 ms, or is
seen back on the first in the 20 ms after; and holds an idle task, a task
bound to its CPU and a mask of no CPU that runs to being refused
([changing a task's affinity](scheduler.md#changing-a-tasks-affinity)). Its
lines say how long each move took -- 108 us and 1014 us in a smoke boot.
The sleeper is to move at its next wakeup (`leave_if_outbound`), so it is
also failed for looking on the old CPU more than twice after the change: a
control run without that move had it look there 30 times, woken there,
until a decision happened to find it running -- a time bound alone let
another such run pass, its sleeper moving after 9 ms.
`test_line_editor` types at the console's line editor
([line editing](shell-commands.md#line-editing)) on a screen of text and
Backspace, as the console is: lines run and kept, Up and Down through them
to the line being typed, ^R's search -- found, found again, failed, made
shorter, given up with ^G, kept to be edited -- ^C and ^U, a line past 80
characters and a history past 32 lines, each step held to what the screen
must show; `cpp-fuzz.py`'s `lineedit` target holds it to a model over random
keys.

- To add a test, write a `TestXxx()` and register it in the `Test()`
  dispatcher.
- To run a single test, edit `Test()` to call only that function, rebuild,
  boot, and watch `nos.log`.

Two more run at boot when there is something to run them on: `test_modules`
loads the `modtest` module embedded in the image, with a parameter and
without, and then feeds the loader damaged copies of it (see [Loadable modules](modules.md#testing)), and
`fstest=on` runs the filesystem self-test on `/` once it is mounted
(`fstest: passed` or `fstest: FAILED`; see [Filesystems](filesystems.md)).
Part of it is the VFS's promise of a lock a mount: a filesystem of the
test's own mounted under its directory, a call held in it, and meanwhile a
file written, read back and removed on the filesystem under test, and a
second call on the held mount kept waiting -- `two mounts at once: a call
on the other went through`, a marker of both smoke boots. On the VFS of one
lock the control fails it (`a call waited behind one held on another
mount`). The C++ `test_vfs` runs the same self-test over a ramfs before
there are tasks, and that part says it is not tried there.

## Smoke boots

`./scripts/smoke-test.sh` (x86-64) and `./scripts/smoke-arm64.sh` (arm64)
build in Docker, boot the kernel headless and assert the serial markers
`After test` → `Preempt is now on` → `boot: complete`, failing fast on
`PANIC:`. What each marker means is in [Boot](boot.md). Both take
`--skip-build`; `SMOKE_TIMEOUT` is the seconds to wait (300 by default) and
`SMOKE_LOG` where the serial log goes. The arm64 one runs under TCG so that
it runs anywhere, CI included; `SMOKE_HVF=1` runs it under HVF on Apple
Silicon, about four times as fast.

The x86-64 smoke boot attaches virtio-blk (modern), virtio-scsi (legacy),
NVMe, virtio-net and virtio-rng, so it covers the Rust drivers and the MSI-X
path as well as the C++ underneath them. The arm64 one attaches virtio-mmio
blk, net and rng. Both boot with `fstest=on`, on an ext4 root of 1 KiB
blocks, as new installs get one (`scripts/mkrootfs.sh`): ext2's root is
`ext2-test.py`'s. The arm64 one boots with
`hugepages=64M` too: the huge pool reserved right after the free list, and a
block of it handed out, written through its pages and given back by the
frames' selftest (`huge frame selftest: passed`) -- x86's boots nos.iso,
whose command line the hv gates share, and their guests are 4 KiB frames.

**Every refactor step must keep both green**, and CI
(`.github/workflows/ci.yml`) runs `make check`, a build *and* a smoke boot
for `x86_64` and for `aarch64`. A change to common code that only builds on
x86 is a broken change.

## The gates

What a smoke boot cannot notice has a test of its own. Every one of them is
there for a failure that looks like success -- a driver that agrees with
itself, a log that ends because the network ate the rest of it, a slot that
leaks once per connection -- and most were written the day after meeting
one. When you meet another, write another.

All of them are in `scripts/`, want the kernel built first (`make`, or
`make ARCH=aarch64`), take `--keep` to leave their scratch directory behind,
and exit 0 only if every check passed. Most are arm64 only, because they
drive the shell over UDP and the arm64 boot is the one whose command line
carries `udpshell=`; the code they test is the same on either architecture.

Every one of them is also a check for undefined behaviour in the code it
drives, when the kernel it boots is built with the checks on: `UBSAN=1` for
the C++ ([Build](build.md#ubsan)) and `RUSTUB=1` for the Rust
([Build](build.md#rust-ub-checks)), separately or together. The first
undefined operation, and the first unsafe precondition `core` finds
violated, is a panic naming its site, which every gate fails on. CI builds
with both and boots it on both architectures after the rest; after touching
C++ or Rust, run the gate that covers it on such a build too. A report is
nothing a plain build would show -- the first UBSan boots found a null
member access in `CONTAINING_RECORD`, stacks 8 off the ABI's alignment and
an arm64 boot stack that overran the page tables below it, all under gates
that passed, and the first `RUSTUB=1` run found the kernel dropping a log
line too long for its buffer instead of truncating it, which had been
swallowing panic messages whole.

| Gate | Arch | Run it after touching |
|---|---|---|
| `wx-test.sh [--arch x86_64\|aarch64]` | both | a mapping path |
| `panic-test.sh [--arch x86_64\|aarch64]` | both | the panic path, `panic=N`, a reset; a panic off a task's stack (x86's double fault), its report through the netconsole |
| `parttest.py [--arch aarch64\|x86_64]` | both | the block layer, the partition tables |
| `ext2-test.py` | arm64 | ext2, the halt |
| `ext4-test.py` | arm64 | ext4 as mke2fs makes it, read by the driver from the shell -- every file's CRC-32 the host's, `find` listing what the host made and nothing the tree leaves out; names and data changed from the shell -- made, moved, an indexed directory written into, files and a tree removed, `fstest`, a copy, a file grown past 4 GiB and cut back -- `find` and `crc32` showing what they make, then `e2fsck -fn` finding nothing in the image and debugfs's extents what was written; a second boot killed under such changes, its image replayed clean by e2fsck and by Linux's loop mount, every file's MD5 the model's; a journal debugfs left unreplayed, read through without a byte written; and orphans debugfs left on the list -- a file deleted, one of a thousand extents cut inside a block -- released by the mount for writing, the cut file zeros past its cut when grown again, the image clean to e2fsck after, its list empty; and a filesystem of a TiB -- 8192 groups, a journal of a GiB -- mounted for writing, a file written and read back, the image clean to e2fsck: what a mount holds of a filesystem's geometry is in pieces no bigger than the kernel's heap gives at once, which the fuzzers' hosts would never refuse; and two disks of noise given filesystems by the shell's `format` -- refused without `force` and while mounted, made ext4 and ext2, written and put through `fstest` -- clean to e2fsck after, of the label, features and block size asked for, their files read by debugfs and the ext4 by Linux |
| `disklog-test.py` | arm64 | the disk log |
| `netconsole-test.py` | arm64 | the log path (`klog`), a net device, netconsole, what reaches the network as the machine halts |
| `tcp-test.py` | arm64 | TCP, the HTTP client |
| `sshd-test.py [--arch aarch64\|x86_64]` | both | TCP's listening side, the module loader, the `ffi` declarations, the terminal a session lends a command, `edit` in nos; on arm64 the halt |
| `netblk-test.py [--arch x86_64\|aarch64]` | both | the block layer's asynchronous path, the C ABI a module reaches `block` and `net` through; the asynchronous path as the image's queue uses it (`block::queue`) is fs-fuzz's too, whose disks take it |
| `netload-test.py [--arch aarch64\|x86_64]` | both | the receive path, the frame pool, `modules/net/netload`, `kcore::net`'s listener, the tick's receive poll (`rxpoll`) |
| `ipc-test.py [--arch aarch64\|x86_64] [--keep]` | both | the channel between hosts: `modules/cluster/ipcnet`, the router's side of `ipc` (`remote.rs`, bounces, a message received in pieces), `ipcwire` as two kernels speak it, TCP's allowlist and a link's floor as ipcnet uses them, `tls::pinned` both ways -- three nos on `scripts/hub.py`'s LAN, which answers their DHCP, relays the test's UDP shell to each, and drops, delays and cuts between them |
| `cluster-test.py [--arch aarch64\|x86_64] [--keep]` | both | the cluster's registry between kernels: `modules/cluster/cluster` (the member, raft over ipc, its folder, init, invite, join, remove), ipcnet's control mailbox (`ipcnet.ctl`) and hosts.toml as the registry writes it, the channel's word on which host sent a message (`kernel_ipc_peer_name`) -- three nos on `scripts/hub.py`'s LAN, as `ipc-test.py` runs them |
| `usb-test.py` | x86-64 | `drivers/usb/`; what a keyboard sends the shell's line editor |
| `hv-device-test.py [--tcg] [--keep]` | x86-64 | a guest's disk on a device of nos's: the devices' ids (`layers/block/src/table.rs`, the NVMe's identity), the claims and their holders, `layers/block/src/devq.rs` and `kcore::block::DiskQueue`, `diskimage::contents`, `modules/hypervisor/hv/src/devices.rs`, a device disk in `modules/hypervisor/hv/src/disk.rs`, the pour onto a device |
| `hv-test.py [--arch x86_64\|aarch64]` | both | `hv`, `hvarch`, `modules/hypervisor/hv` -- the hypervisor; on x86-64 a PVH image too, hvboot started at its 32-bit entry, `hv create` of VMs from a web server's images, `hv clone` and `hv delete`, and the `vm` provider's actions through `mgmt`; on both, `hv convert` |
| `edit-test.py [--seed N] [--sessions N] [--seconds S] [--session N]` | host (CI) | the editor (`src/crates/common/text/edit`), from its own sources: key sessions held to what they leave in the file and on the screen, files read and saved back byte for byte, random sessions on a terminal of the test's own |
| `diskimage-test.py [--image <qcow2>] [--iterations N]` | host (CI) | the disk image converter (`src/crates/common/storage/diskimage`, `hv convert`), from its own sources: qcow2 images qemu-img and qemu-io write -- versions 2 and 3, clusters of 512 bytes to 2 MiB, stored, compressed, extended, zero, discarded, preallocated, snapshotted, dirty -- converted and held byte for byte to what qemu-img reads, its holes kept; what it must refuse; its DEFLATE decoder against zlib; each image's metadata fuzzed; and the cloud-init seed's ISO 9660, read back by xorriso or bsdtar, blkid and Linux's isofs |
| `hvboot-test.py [--image <disk.raw>] [--iterations N]` | host (CI) | hvboot's partition, ext2/3/4 and GRUB readers and its bzImage header, from their own sources: on disks it makes -- GPT, MBR and none, ext4, ext3 and 1 KiB-block ext2, a grub-mkconfig grub.cfg and grubenv -- the entry GRUB would boot and its files byte for byte, then each disk's metadata fuzzed |
| `extfmt-test.py [--iterations N] [--trials N]` | host (CI) | the ext2/3/4 format crate (`src/crates/common/storage/extfmt`) hvboot reads through and the kernel's ext4 driver is to: every checksum of filesystems mke2fs makes -- ext4 by default, with metadata_csum_seed, with 1 KiB blocks and 128-byte inodes and no 64bit, with gdt_csum, ext3, ext2 -- held to extfmt's, bytes they cover changed one at a time with extfmt and `e2fsck -fn` held to both saying so, and each image fuzzed; then journals debugfs leaves unreplayed, replayed by extfmt's planner and by `e2fsck -E journal_only` to the same image byte for byte, their ring turned so the log wraps, damaged and refused, and fuzzed; and the filesystems `extfmt::mkfs` makes for the shell's `format` -- ext4 and ext2, every block size, up to 3 GiB and 128 groups, the small ones made over noise -- clean to `e2fsck -fn`, their features and label dumpe2fs's, their lost+found debugfs's, and the devices it refuses refused |
| `insn-test.py` | host (CI) | the hypervisor's MMIO decoder, its decoder of a `mov` of a control register, and its guest page walker (`hypervisor/hv/src/{insn,walk}.rs`), against the encodings clang gives |
| `hv-fuzz.py [--seed N --seconds S]` | host (CI) | anything a guest reaches in the hypervisor -- its devices, local APIC and IO-APIC, the MMIO path, the Linux loader and its ACPI and SMBIOS tables, the CPUID and MSR policy, the run loop that dispatches its exits, on every CPU of a guest of several, and the guests' network: the switch, its DHCP server and NAT -- fuzzed with overflow checks on; and the virtio notifications, kicks and interrupts with EVENT_IDX and without, held to a Linux driver's every request served (`virtq`) |
| `net-fuzz.py [--seed N --seconds S]` | host (CI) | anything the network hands the kernel -- the receive path, ARP, ICMP, UDP and TCP; the DHCP client and the DNS resolver; the HTTP client over TCP and over TLS; the UDP shell; netconsole; `sshd` over the `ssh` crate, and the session's line editor -- fuzzed with overflow checks on, each protocol against a model of it |
| `fs-fuzz.py [--seed N --seconds S]` | host (CI) | anything a disk hands the kernel -- the partition tables, the disk log's area, ext2 and ext4 images sound and damaged -- and the storage layers above it: the block table's bounds and claims, the VFS and its C ABI, the file ABI, the shell's storage commands and its `format`, `root=`; fuzzed with overflow checks on, the filesystems against a model of the tree, e2fsck's judgement and power cuts |
| `cpp-fuzz.py [--seed N --seconds S]` | host (CI) | what firmware, a bootloader or a disk hands the kernel's C++ -- the device tree and what `Board` takes of it, the memory map, the GRUB environment block, a module's `.ko`, the x86 boot's Multiboot2 tags, microcode file, ACPI tables and command line, and the printf every line is formatted by, and what a terminal types at the console's line editor, and the tracepoints' ring against a model and a writer thread, and the window `trace latency` reads of the rings against the truth of a schedule played out on them -- and the memory management under everything: the page tables and the physical page allocator past `setup`, the kernel heap; the kernel's own sources under the address and undefined-behaviour sanitizers, each against a model of what its header promises |
| `hv-linux-test.py --bzimage <img> [--initrd <cpio>]` | x86-64, by hand | the Linux loader, the CPUID/MSR policy, the emulated devices -- a real kernel to its shell; with an initrd, guests that stay up and the commands that reach them, and a VM kept in a folder (`hv create`, `hv start <name>`, its `console.log`), in `/vms` and in a folder `insmod /hv.ko vms=` names; `--net`, the guests' switch, NAT, its DHCP server and the DNS server they are given; `--cpus N`, guests of N CPUs, their local APICs and IPIs; `--xapic`, those APICs in xAPIC mode, every access of theirs by MMIO; `--ioapic`, an IO-APIC routing the timer, the serial port, the SCI and -- with `pci=nomsi` -- virtio's INTx; `--acpi`, a guest kernel with ACPI: the tables it is given, the PM timer and the SCI, the reset register, and `hv stop`'s power button -- with `--disk`, the one nos's own poweroff presses, the guest's disk left mounted coming back clean; `--qemu-cpu MODEL`, nos on a named TCG CPU -- `EPYC-Rome-v1`, `Skylake-Client-v1,+svm,+npt,+nrip-save` -- whose family and model put the guest's kernel on that CPU's paths through its MSRs; `--mem` past 4064 with `--nos-mem`, a guest with RAM from 4 GiB; `--nos-iso`, nos booted from `make nos-test.iso TEST_ISO_ARGS=hugepages=...`, every guest's memory the huge pool's, 2 MiB an entry of its nested table |
| `hv-distro-test.py --iso <alpine-virt.iso> [--debian <nocloud.raw>] [--ubuntu <cloudimg.raw>] [--internet] [--cpus N [--ioapic]] [--hvboot]` | x86-64, by hand | a distribution as it ships -- Alpine's kernel, initramfs and packages, its ISO a read-only disk: login, clock, reboot, network and the way out through NAT, and its own sshd reached from outside; Debian's cloud image, systemd provisioned by credentials, networkd by DHCP, its root written to and kept across a reboot -- both on their ACPI (`--acpi-off`: without), Debian shut down by `hv stop`'s power button; with `--hvboot`, Debian's and Ubuntu's booted by hvboot off their own /boot, nothing taken out of them; with `--create <url>`, a VM made by `hv create` of a cloud image nos fetches itself, booted and provisioned by cloud-init from nos's seed |
| `idle-wait-test.py [--smp N]` | x86-64 | a wait primitive, the scheduler's choice of the idle task |
| `trace-test.py [--arch aarch64\|x86_64]` | arm64 (x86-64 where grub-mkrescue is) | the tracepoints and the `trace` command: a boot's `tracepoints=`, records shown merged and in order, the rings made again, wrapped, cleared, frozen, freed; the scheduler's events, and the interrupts' and soft IRQs' -- [Tracepoints](tracing.md) |
| `image-test.py [--image FILE]` | x86-64 (CI) | the system image and what pours and boots it: `make image`, `build/image/`, `nos-install` and its `--panel`, `root=UUID=` |
| `web-test.py` | host (CI) | the host API's wire: `src/crates/common/net/web`, `src/crates/common/ipc/mgmtwire`, a provider's descriptor |
| `crc-test.py [--seed N] [--cases N]` | host (CI) | the tree's CRCs (`src/crates/common/crc`) -- CRC-32, CRC-32C and CRC-16, which GPT, the disk log, ext4 and raft's log are checked by -- from the crate's own sources: the known answers, every form held to the CRC computed from its definition a bit at a time, and CRC-32 to zlib's |
| `raft-test.py [--seed N --seconds S] [--mutants]` | host (CI) | the raft crate (`src/crates/common/cluster/raft`): known answers, the known traps played out, groups fuzzed over a network that fails in every way and held to Raft's properties at every step, the wire fed noise, a member fed what no correct member sends, the log on disk through power cuts, torn writes and lost ones, a register through the replicated state machine checked for linearizability, every allocation held to a block of the kernel's heap -- [Raft](raft.md) |
| `nbs-fuzz.py [--seed N --seconds S] [--target meta\|cs\|wire\|cluster\|join] [--mutants]` | host (CI) | NBS's metadata (`src/crates/common/storage/nbsmeta`): what is stored, placement, the snapshot and the digest; the chunk server's engine (`nbscs`) through power cuts; NBS's messages (`nbswire`); the client (`nbsclient`) end to end through six hosts' cs's and mds, crashes and takeovers; and a join's copy (`nbswire::copy`) from a source onto its joiner, under writes, rejoins, rot and power cuts -- each held to a model of its own -- [`nbs-fuzz.py`](#nbs-fuzzpy----nbss-metadata-decided-on-the-host) |
| `nbs-test.py [--arch aarch64\|x86_64] [--keep]` | both | NBS between kernels: `modules/storage/nbs-mds` (the member, its commands, the `nbs` provider, `nbs io`'s sessions, the controller, the scrub) and `modules/storage/nbs-cs` (the chunk server, a join's copy), raftmember as it runs them, nbsload -- three nos on `scripts/hub.py`'s LAN, as `cluster-test.py` runs them, each with a pool disk, and a fourth booted late: a host back copying only what it missed, one of four lost for good and replaced, rot spoiled under a running cs through QEMU's monitor and mended by `nbs replica bad` |
| `hv-nbs-test.py --bzimage <bzImage> --initrd <cpio> [--keep]` | x86-64, **manual** | hv's guests on volumes of NBS's (`disk=nbs:<volume>`): a pour read by a guest elsewhere, a guest's writes read back and moved with the volume, a forced start fencing the old guest, a host killed under a write -- three nos under KVM where the host has it, the guests nested |
| `api-test.py [--arch aarch64\|x86_64]` | both (arm64 in CI) | the host API module, `nosctl`, the bus's caller side for modules; on x86-64 the `vm` provider through it and `api panel create` |
| `panel/`: `npx playwright test` | host (CI) | the management panel against the mock API, both themes and both languages |
| `panel-appliance-test.py <bundle>` | host (CI), Docker | the panel's bundle (`scripts/panel-bundle.py`, `panel/appliance/`) run as its VM runs it |

### By what you touched, at length

The table `CLAUDE.md` gives the short form of: for each part of the tree, the
gates that hold it and what each holds it to. A gate's own section below says
how it works and what it found.

| After touching | Run (`scripts/`) |
|---|---|
| a mapping path | `wx-test.sh [--arch x86_64\|aarch64]` |
| the panic path, `panic=N`, a reset (`hal::reset_now`); what a panic does off a task's stack -- the lock path's `preempt_current_task`, x86's double-fault stack, a read the report cannot be sure of (`Panicker::recover_fault`) | `panic-test.sh [--arch x86_64\|aarch64]`: its third boot panics from a stack that is no task's -- a real double fault on x86 -- and the report must reach a netconsole collector before `panic=1` resets the machine |
| the halt: what `poweroff` stops and in which order, `Cpu::park`, the static destructors | `ext2-test.py`, `sshd-test.py` (arm64): each powers off, and the machine must switch itself off with nothing panicked on the way down. On x86-64 only `image-test.py`'s power-off, which a panic would pass -- `panic=10` and `-no-reboot` end QEMU too. `netconsole-test.py` (arm64): the halt's own lines must reach the collector, the netconsole stopping after the modules and the filesystems. What the halt does to running guests -- the hv module's unload presses their power buttons and gives them `stop_secs=` -- `hv-linux-test.py --disk --acpi` (x86-64, **manual**): nos powered off under a guest whose disk is mounted, which must come back clean |
| the block layer, the partition tables | `parttest.py [--arch aarch64\|x86_64]`; a batch (`Disk::write_pieces`, a driver's `write_pieces`/`read_pieces` -- NVMe's PRP lists among them): the smoke boots, `fs-fuzz.py`, `hv-linux-test.py --disk` and `--disk --nvme-root` |
| the block layer's asynchronous path, the C ABI a module reaches `block` and `net` through | `netblk-test.py [--arch x86_64\|aarch64]` (`--arch aarch64` on a Mac); its queue of buffers its own (`block::queue`) and an image's served through it (`fs::image`'s `start`, `reap`, `drain`; `kcore::fs::ImageQueue`): `fs-fuzz.py`, whose disks take asynchronous I/O -- done when the input says, carried out then, its memory held to being DMA pages still the kernel's -- and whose `ext4rw` and `ext2` targets send a guest's batches through an image's queue, every read held to the model as it comes back; then `hv-linux-test.py --disk --nvme-root` (x86-64), where a guest's disk goes that way |
| ext2 | `ext2-test.py` (arm64): lets `e2fsck` judge the image; its file data under a guest's disk -- holes filled, blocks written over, in batches, given back by the guest's `fstrim` -- `hv-linux-test.py --disk [--nvme-root]` (x86-64), which judges nos's root the same way, on virtio-blk and on NVMe |
| ext4: `src/crates/layers/fs/src/ext4/` -- its mount, the checksums it holds every read to, the journal it reads through without replaying when mounted read-only; and what it writes through the journal: the transaction (`txn.rs`), the commit, checkpoint and abort and the replay at a mount for writing (`journal.rs`), allocation (`bitmaps.rs`), directories and their index dropped (`dirs.rs`), the extent tree edited a leaf at a time (`extents.rs`), create, rename and remove (`ops.rs`), a file's data, truncate and punch (`data.rs`), giving back at scale through the orphan list and its release at a mount (`orphans.rs`), freed runs and the punch's reserve (`bitmaps.rs`, `txn.rs`), a torn superblock (`mod.rs`); what holds a disk's image counted by the file in `vfs.rs` -- and the driver choice (`layers/fs/src/extfs.rs`: `root=`, the fallback layout, `mount`) | `fs-fuzz.py`'s `ext4` target (the driver read-only on the fuzzer's own images -- uninitialised groups with noise, deep extent trees, unwritten extents over noise, htree directories, entries the tree leaves out, journals holding the newest copies of blocks whose homes are noise, orphans on the list -- every answer the model's, nothing written), `ext4bad` (the images damaged: refused or walked, never a panic, a write or a leak), `ext4sys` (mounted for writing over a file whose extent names the filesystem's own blocks -- an inode table, the descriptors, the journal's log -- refused by every way to its data, a write, a read, a cut, a remove, a disk's image, those blocks left as they were) and `ext4rw` (mounted for writing, the journal replayed and the orphans released: names and data, files open, disks' images, files of two names, a disk all but full, every answer the model's -- a punch never refused for room on a disk that had ext4's reserve free -- the image a clean unmount leaves clean to the judge, no orphan left; the power cut at chosen requests and before every request of a call, each picture replayed by the judge and by the kernel, read-only and for writing, all three the same and clean, its tree what the call in flight may leave -- for a write every sector what it held or was given, nothing of another file's; requests that fail: a file's data block that does not write failing its call alone, any other write's failure and nothing written in that mount after it); `fs-fuzz.py --e2fsck` holds ext4rw's images, clean ones and pictures, to e2fsck. Then `ext4-test.py` (arm64, in CI): mke2fs's ext4 read from the shell, every file's CRC-32 the host's, `find` listing what the host made; names and data changed from the shell -- `fstest` among them -- and the image e2fsck -fn's clean, debugfs's extents what was written; a boot killed under its changes, its image replayed clean by e2fsck and by Linux, every file's MD5 the model's; a journal debugfs left unreplayed read through with the image's bytes unchanged; and orphans debugfs left released by a mount for writing, the image clean to e2fsck after; and a TiB filesystem with a 1 GiB journal mounted for writing -- what the mount holds of the geometry in pieces the kernel's heap gives, which no host-side fuzzer can refuse -- written and clean to e2fsck. After touching the driver choice, the smoke boots (their roots are ext4, ext2-test's ext2) and `fs-fuzz.py`'s `rootfs` |
| the disk log | `disklog-test.py` (arm64): nothing else gives `disklog=on` |
| the log path (`klog`), a net device, netconsole | `netconsole-test.py` (arm64): on the Hetzner boxes it is the only console there is -- what a boot logs, a panic's report, and the halt's lines down to its own last |
| TCP, the HTTP client; TCP's loss recovery -- SACK and D-SACK, RACK-TLP, PRR, the undos, the table of what is in flight (`Sent` in `layers/net/src/tcp.rs`); the window it offers and takes in by (`offer_window`, `acceptable`) and silly window avoidance | `net-fuzz.py --target tcp` (host, in CI): a peer that takes SACK or not, says four blocks, fewer or none, and reneges now and then -- the machine held to SACKing only what reached it, D-SACKing only what reached it twice, and never sending again what an acknowledgement it had taken in told it was held; every connection's table, and the furthest edge its window offered -- within the receive buffer, no byte inside a window offered ever finding no room -- audited after every step. Then `tcp-test.py` (arm64); for the numbers, `scripts/tcp-loss-bench.py` (arm64, HVF): two nos over hub.py, MB/s and each end's counters under loss, delay and reordering, against the tree before -- `--path` for the frames each way by size, `--trace`/`--show` for TCP's tracepoints, `--top` for the CPUs |
| TCP's listening side, the module loader, the `ffi` declarations, `ssh`, `modules/net/sshd` | `sshd-test.py` (arm64); the loader's unload order, the smoke boots (`test_module_unload`) and `cpp-fuzz.py --target module`. 59 checks pass; `connections past the four logging in are refused` flakes under load: stragglers of the probe burst land after its quiet window |
| the editor: `src/crates/common/text/edit`, `modules/tools/edit`; the terminal a session lends the command it runs -- the pty-req's and window-changes' size in `ssh`, `Output::terminal`, `kernel_printer_terminal` | `edit-test.py` (host, in CI): the crate run as the module runs it, on a terminal of the test's own that does what xterm does -- key sessions held to what they leave in the file, at the cursor and on the screen; files read and saved back byte for byte; random sessions after every key of which the editor keeps its promises and the terminal has had nothing it would wrap, scroll or not know, the screen drawn a row at a time what a full redraw draws, and undoing everything the file as it was read. Then `sshd-test.py` (arm64), whose editor phase types at `edit` through `ssh -t` on a pty of its own: drawn at the pty's size, saved, a file changed meanwhile asked about, a resize drawn again whole, `<file>.save` when the connection goes; and `net-fuzz.py`, whose `term` command is told the size its client gave and changed |
| the receive path, the frame pool, `modules/net/netload`, a module's UDP listener in `kcore::net`, the tick's receive poll (`rxpoll`) | `netload-test.py` (arm64; about seven minutes, by design), and `--arch x86_64`, its short form, after touching the module ABI |
| `drivers/net/igb`, the driver seam in `layers/net/src/device.rs` | `tcp-test.py --nic igb` and `netload-test.py --nic igb`: QEMU's 82576, the only way a hardware NIC driver runs anywhere but on a Hetzner box (QEMU 8.0 or later; CI's does not have it, so by hand) |
| `drivers/usb/` | `usb-test.py` (x86-64): types at the kernel through QEMU's monitor |
| the shell's line editors -- the console's (`kernel/line_editor.cpp`), an SSH session's (`common/net/ssh/src/terminal.rs`) -- and what the keyboards send them (`drivers/8042.cpp`, `drivers/usb/src/hid.rs`: an arrow as ESC [ A, Ctrl and a letter as its control byte) | `cpp-fuzz.py --target lineedit` and `net-fuzz.py --target terminal` (host, in CI): each editor held to a model over random keys -- the line Enter runs, and what its screen shows, to the byte; the smoke boots, whose `test_line_editor` types at the console's; `usb-test.py` (x86-64), whose USB keyboard presses Up and ^R. Nothing but QEMU's monitor types at the 8042, by hand |
| `hv`, `hvarch`, `modules/hypervisor/hv` | `hv-test.py [--arch x86_64\|aarch64]`: loads, turns the CPU's virtualization extension on for every CPU, runs every built-in guest on the first and the last CPU and checks what each did, unloads **without** turning it off, and loads again to ask every CPU what it has -- an `rmmod` that left it on is a freed page the CPU is still saving host state into. x86-64 uses KVM `-cpu host` where the host has an extension -- **AMD-V** on AMD, **Intel VT-x (nested)** on Intel, so both backends are covered -- and TCG `-cpu max` (AMD-V) otherwise, since TCG has no VMX |
| the Linux loader, the CPUID/MSR policy, the emulated devices (`hypervisor/hv/src/{linux,policy,run}.rs`, `hypervisor/hv/src/devices/`), the Linux guest commands (`modules/hypervisor/hv/src/{boot,guest,vms}.rs`), a VM kept in its folder (`modules/hypervisor/hv/src/{store,config,vmlog}.rs`, vm.toml read by `src/crates/common/text/conf`) | `hv-linux-test.py --bzimage <bzImage> --initrd <cpio>` (x86-64, **manual** -- a bzImage is megabytes, so not in CI), and for a VM's `vm.toml` `hv-fuzz.py`'s `config` target: `hv boot`s a real 64-bit Linux and checks its early console reaches the banner, the command line, NX and `console [ttyS0] enabled`; then `hv start`s two, types at one (`exec`, `send`, `wait`), checks a guest that reboots itself stops saying so, restarts (by hand, and by itself up to its limit), and that `hv off` refuses and `rmmod` stops a running one; makes a VM's folder with `hv create`, boots it by its name, and finds its boot in its `console.log` -- in `/vms`, and in a folder `insmod /hv.ko vms=` names; `--attach` drives `hv attach` through a real `ssh -tt`; `--disk` gives the guest a virtio disk that must come back out of nos's root passing `e2fsck`, and nos's root must pass it too; `--net` puts two guests on the switch with nos, pinging each other, and fetches a guest's page from outside through `hv forward`. A guest's memory past 4 GiB and from the huge pool (`linux::Ram`, `hugepages=`, `GuestMemory::add_huge`, the nested tables' 2 MiB entries): `--mem 4608 --nos-mem 8G`, and with `--nos-iso` of `make nos-test.iso TEST_ISO_ARGS=hugepages=5G`, under both backends. Every boot's console must say nothing of an unchecked MSR access or an MCA violation: after touching the policy, boot a guest kernel with `X86_MCE`, `MTRR` and `X86_PAT` -- as a distribution's has -- and with `--qemu-cpu EPYC-Rome-v1` and `--qemu-cpu Skylake-Client-v1,+svm,+npt,+nrip-save` as well, a Zen 2's paths through Linux's MSRs and an Intel CPU's. Slow: the guest is twice emulated under TCG |
| the disk image converter: `src/crates/common/storage/diskimage` (qcow2, its DEFLATE decoder, what a file's first bytes say it is) and `hv convert` (`modules/hypervisor/hv/src/image.rs`), `File::set_len` | `diskimage-test.py` (host, in CI): qcow2 images qemu-img and qemu-io write -- versions 2 and 3, clusters of 512 bytes to 2 MiB, stored, compressed and extended, zero, discarded, preallocated, snapshotted, dirty -- converted and held byte for byte to what qemu-img reads, no more written than the blocks that are not zeros; the refusals, a refusal leaving no disk; the decoder against zlib, every answer and every byte, over streams made and broken at random; each image's metadata fuzzed, and its compressed data changed and converted by qemu-img too, the two agreeing; `--image` for a distribution's. Then `hv-test.py [--arch x86_64\|aarch64]`, whose last phase converts in nos, on its ext2, and holds the disk to its SHA-256; `File::set_len` is `fs-fuzz.py`'s too |
| the management bus: `src/crates/layers/mgmt` (its registry, calls, jobs, the `mgmt` command, `rust_mgmt_stop` in both halts, the descriptor check at registration), its module side (`base/ffi/src/mgmt.rs`, `kcore::mgmt`), a provider's unregister; the image's providers -- `system` (`kernel/src/system.rs`, over `kcore::system`, `base/ffi/src/system.rs` and the C++ it copies from: `TaskTable::snapshot`, `ModuleTable::snapshot`, `Dmesg::read_since` and the log's numbers, `Task::get_runtime_ns`, `hal::cpu_model`, `start_load`'s `JobOutcome`), `net`, `block` and `fs` (each layer's `provider.rs`), a device's byte counters, `FileSystem::space` | the smoke boots, both arches: `mgmt::selftest` in `rust_test` (descriptors that do not say what the provider has refused, calls answered and refused, an answer held to its limit, a job run to its end and one stopped and waited out by its provider's unregister), `system::selftest` (every read of every provider in the image answered as JSON, the log read by number) and `test_module_bus` after them (modtest's provider called through `mgmt call`, a job of it the module's unload must stop and wait out, the provider gone with its module); `web-test.py` for a descriptor; then `sshd-test.py` (arm64: the ffi digest changes with the module side), and for the halt's step `ext2-test.py` and `netconsole-test.py` (arm64). After touching the counters on the receive and transmit paths, `netload-test.py` (arm64), its pps against the tree before; `FileSystem::space` and the VFS's mounts, `fs-fuzz.py`; the log's records, `netconsole-test.py`; the module loader, `cpp-fuzz.py --target module` |
| the message channel: `src/crates/layers/ipc` (mailboxes, names and their namespaces, the router and its peers -- `remote.rs`: numbers and epochs, what is routed, what a peer may deliver to, monitors of peers' mailboxes and HostLost, messages, monitors and the place kept for each Down, watches, kernel.echo, the `ipc` command), its C ABI (`base/ffi/src/ipc.rs`, `layers/ipc/src/abi.rs`) and `kcore::ipc`, what the loader tells it (`ModuleTable::name_at`, `rust_ipc_module_gone` after a module's exit, `rust_ipc_stop` in both halts) | the smoke boots, both arches: `ipc::selftest` in `rust_test` and `test_module_ipc` after it (modtest calling kernel.echo with payloads up to 200 KB, its names held to its namespace, a Down, a full mailbox, then unloaded under the echo's traffic -- the echo told, a mailbox it left open closed by the sweep, the host's queues empty after); `ipc-fuzz.py` (host, in CI): the crate over the fuzzers' common machine against a model of its own -- every answer, every queue in order, every count, each Down once in its kept place, names and watches, receivers asleep until a message, a wake or a close, modules unloaded with mailboxes open, the halt -- ten mutants of the crate each failing its gate; after touching `ModuleTable::name_at` or the unload path, `cpp-fuzz.py --target module`; after touching the ABI, `sshd-test.py` (arm64: the ffi digest) |
| the events a task waits on (plans/ipc.md, I2): `src/crates/base/events` (`SharedEvent`, held by its task and by everything made on it, one waiter at a time, the modules' table of them by slot and generation -- `kernel_shared_event_*`), and what is made on one: a mailbox (`Mailbox::new_on`, `look_and_wait` in `layers/ipc/src/lib.rs`), a device's queue and an image's (`Disk::into_queue_on`, `Image::into_queue_on`, the queue entry's task event in `layers/block/src/queue.rs`, signalled from the completion's interrupt under its interrupt-safe lock and taken out under it by the queue's drop) | the smoke boots, both arches: `one_event::selftest` in `rust_test` (a mailbox and a queue on one event, a task asleep on it woken by a message, by a completion made in another CPU's interrupt and by a wake, a second waiter refused, the event outliving its owner and freed with the last of what holds it) and `modtest ipc`'s first part (the same through the C ABI, on a device's queue); `ipc-fuzz.py` (mailboxes on events of the image's and of made-up modules', every event there exactly as long as something holds it) and `fs-fuzz.py` (images' queues made on events and let go of first); after touching the ABI, `sshd-test.py` (arm64: the ffi digest) and `netblk-test.py` (the block ABI) |
| ipcnet's wire: `src/crates/common/ipc/ipcwire` (its frames, the decoder that takes bytes as they come, the assembler that puts messages back together from frames between other messages' -- plans/ipc.md, I4) | `ipc-fuzz.py --target wire` (host, in CI, with the channel's own target): bytes of no frame and frames broken -- a bit flipped, a length changed, bytes added or cut -- refused or decoded to a frame that encodes to them, a head claiming more than a frame refused at once; frames encoded, cut anywhere and decoded as they come back to themselves; messages cut into frames and interleaved, with mistakes among them, against a model of the assembler's limits and of every byte -- seven mutants of the crate each failing it; the kernel's own build compiles the crate, through `ipc` |
| the channel between hosts: `src/crates/modules/cluster/ipcnet` -- its hosts.toml and key (`files.rs`), a peer's session and its queues (`peer.rs`), a connection's handshake, Hello and frames, and its loop's turn (`link.rs`), the router's task (`router.rs`), the listener and the dialers (`server.rs`) -- `scripts/hub.py`, and what the module drives of the channel: `kernel_ipc_bounced`, `kernel_ipc_recv_part` (`Mailbox::recv_pieces`), `deliver_remote`'s pieces, `ipc ping` | `ipc-test.py` (arm64, and `--arch x86_64`): three nos on hub.py's LAN -- keys pinned both ways, pings small and of a megabyte held to every byte and to their order, bounces of a name nobody has and of a full mailbox, 5% loss and jitter, a cut seen within 3 s on both sides and its monitors told, a host with the wrong key never up, a stranger's SYNs dropped, `rmmod ipcnet` under traffic and loaded again; then `net-fuzz.py`'s `ipcnet` target (host, in CI: the module's source against a world that is the other host -- keys and Hellos right and wrong, messages, a full mailbox's bounces, monitors and their Downs, Pings, silence, a broken wire -- each delivery held to the rules, once and in order) and `ipc-fuzz.py` (both targets); after touching the ABI, `sshd-test.py` (the ffi digest); after touching the loop's turn, `tcp-loss-bench.py`'s numbers against the tree before |
| the host API module (`src/crates/modules/mgmt/api`: its listeners, limits and allowlists -- `api reload` reads them again -- the TLS it serves, its routes and roles, the stream and its console channel, its folder -- the host key, the clients, the audit log -- and its own provider), `scripts/nosctl`; the bus's caller side for modules (`kernel_mgmt_call` and the rest in `layers/mgmt/src/abi.rs`, `layers/mgmt/src/view.rs`, `kcore::mgmt`'s calls) | `api-test.py` (arm64, and `--arch x86_64` its short form): the host's key pinned both ways; a stranger and a client with no certificate refused in the handshake with the count of requests parsed unmoved; a reader refused every change and shown the reads alone, an admin's changes in the audit log; jobs run, read back and cancelled; the HTTP edges; the stream; the limits of a key, of all, of handshakes and of idleness; sshd answering while the API is full; a client taken off the list and put back; no TIME-WAIT on the API's port; rmmod under a stream, a connection and a handshake, at once -- and the cost of a handshake printed. Then `net-fuzz.py`'s `api` target (host, in CI): the module's source against clients of each role and of none that ask for every route and break HTTP, the stream and TLS, while the console takes keys off the list, writes one it must refuse and stops the server under them -- every answer held to a model of the routes, the roles and the limits, what ran to what an admin asked for and the audit log, the server's own count of its connections to api.toml's, a VM's console paged in order and typed at by admins alone, once (after touching `web::http` or `web::ws`, `web-test.py` too). Every boot's `test_module_bus` asks the bus as a module does (`modtest bus`); after touching the ABI, `sshd-test.py` too (the ffi digest) |
| the host API's wire: `src/crates/common/net/web` (HTTP/1.1, WebSocket, SHA-1, IPv4 networks, the self-signed certificate) and `src/crates/common/ipc/mgmtwire` (the bus's JSON, its parameters and statuses, the descriptor format's check); a provider's descriptor (`*.descriptor.json`) | `web-test.py` (host, in CI): known answers -- SHA-1's FIPS vectors, RFC 6455's accept key and frames, requests and frames each with what it must make or the status it must be refused with, JSON texts, parameters, networks, a descriptor with every member the format has taken and each of its defects refused; every parser fed what a generator makes and then breaks, held to no panic, to reading back what its writer wrote, to answering a prefix of anything whole "not yet" and to its limits; the certificates it makes judged by OpenSSL -- every extension, date and fingerprint -- and by Python's ssl, serving TLS with them and taking a client's; and every descriptor in the tree checked as the bus checks it, and its texts by Python: in English and Russian, none empty, none left untranslated |
| `hv create <name> <url>` (`modules/hypervisor/hv/src/cloud.rs`), the cloud-init seed's ISO 9660 (`common/storage/diskimage/src/iso.rs`), a module's HTTP download (`kernel_http_fetch` in `layers/net/src/abi.rs` over `net::wget::fetch`, `kcore::http`: its progress handed to the module, which may stop it) | `hv-test.py` (x86-64): VMs made from a web server of the test's own -- a qcow2 image downloaded, converted and grown to `size=`, its disk held by SHA-256, its seed and vm.toml; a raw one taken; a name that has a vm.toml, a 404, a file that is no image and a `size=` under the image's refused, nothing left behind. `diskimage-test.py` for the ISO: read back by Rock Ridge's names, blkid's label, Linux's isofs. Then `hv-distro-test.py --create <url>` (x86-64, **manual**): a cloud image fetched by nos itself and booted, cloud-init provisioned by nos's seed -- the user logs in with its password, NoCloud the datasource, the key nos's, the root grown. After touching the HTTP client (`layers/net/src/{http,wget}.rs`), `tcp-test.py` and `net-fuzz.py` too |
| what is done to a VM's folder after it is made -- `hv clone` (`modules/hypervisor/hv/src/clone.rs`: its disks copied by their extents, its seed made anew from the `user-data` kept, its identity made its own), `hv delete`, a vm.toml changed key by key (`config::rewrite`, `conf::edit`, `vm.toml.prev`), a forward kept in it, `protected`, a name held while its folder is made, cloned or deleted (`Vms::reserve`) -- and the `vm` provider (`modules/hypervisor/hv/src/provider.rs`, `vm.descriptor.json`), the `Work` a long command and a job share (`work.rs`: lines, progress, a stop) | `hv-test.py` (x86-64): a VM made from a URL cloned -- the clone's disk held to the source's by SHA-256 and mostly a hole, its seed new, its UUID its own -- a clone to a name taken refused, a forward added through the provider and kept in the vm.toml with the rest of the file as it was, a protected VM's delete refused without `force`, a delete keeping the disks; `api-test.py --arch x86_64`: the provider through the host API -- start, stop, console read and streamed, a reader's typing refused, create, clone, delete, protected, and a create cancelled midway with nothing left; and `api panel create`'s create: a port of its own, a forward, `protected`, started by the same job; `hv-fuzz.py`'s `config` target (a commented vm.toml rewritten to another configuration read back as it, every comment kept in order); `fs-fuzz.py` (the extents a clone copies by); `web-test.py` (the descriptor) |
| a VM's disk on a device of nos's -- a partition or a whole disk, the filesystem out of the way: the devices' ids (`layers/block/src/table.rs`'s `part_ident` and `disk_ident`, `BlockDriver::identity`, the NVMe's model and serial), the claims' holders and `in_the_way`, the device's queue (`layers/block/src/devq.rs`, `kcore::block::DiskQueue`, the buffers it shares with the image's queue in `base/kcore/src/lent.rs`), the NVMe's Dataset Management, what a device holds (`common/storage/diskimage/src/contents.rs`), `modules/hypervisor/hv/src/devices.rs` and a device's disk in `disk.rs`, the pour onto a device (`image.rs`'s `pour`, `cloud.rs`'s `onto=`); `root=` refusing a label or a UUID two devices carry (`layers/fs/src/rootfs.rs`) | `hv-device-test.py` (x86-64): two NVMe drives beside the root, one partitioned by the test -- every device's id from `disks`, what `hv devices` says each holds, an EFI System partition refused outright and an ext4 without `force`, a partition and a whole disk given by their ids and the disk under the partition refused, a qcow2 poured onto a partition and held to the image byte for byte, a running VM's partition held against the shell's write, a clone refused; then the drives swapped, every id found under the other name and the VM started by its own. Then `parttest.py` (the ids), `diskimage-test.py` (what a device holds -- against mke2fs, mkswap, mkfs.vfat, mkfs.xfs and mkfs.btrfs where the host has them), `fs-fuzz.py`'s `rootfs` (the two-candidate rule), `netblk-test.py` and `sshd-test.py` (the block ABI, the ffi digest), `hv-test.py` and `hv-fuzz.py` (the disk task, the virtio disk's features); for the provider `api-test.py --arch x86_64`, `web-test.py` (its descriptor, which the bus holds to 16 KiB) and the panel's tests |
| PVH (`hypervisor/hv/src/pvh.rs`), the protected-mode start a PVH image is entered in (`svm::protected_mode`, both backends), hvboot (`src/crates/hypervisor/hvboot`) | `hv-fuzz.py` (the `pvh` target: ELFs of either class, the start info and the entry state held to what the guest was given); `hv-test.py`, whose last boot is hvboot started by PVH with no disk, on whichever backend the machine has; `hvboot-test.py` (host, in CI): hvboot's partition, ext2/3/4 and GRUB readers on disks it makes -- the entry GRUB would boot, and its kernel and initrds byte for byte -- then each disk's metadata fuzzed, and `--image` for a distribution's; then `hv-distro-test.py --ubuntu <cloudimg.raw> --debian <nocloud.raw> --hvboot` (x86-64, **manual**): the images booted off their own /boot by their own grub.cfg, nothing taken out of them, through the same checks as without. hvboot runs in the guest, and its parsers have no `unsafe`: keep them so, its platform's sites in its own few files |
| the ext2/3/4 format: `src/crates/common/storage/extfmt` -- its views of the superblock, descriptors, inodes, extent trees, block maps, directory and attribute blocks, every checksum metadata_csum and gdt_csum put on them, `features::classify`, the journal's format (`jbd2`) and the plan of its replay (`recovery`) -- and hvboot's reader over it | `extfmt-test.py` (host, in CI): filesystems mke2fs makes -- ext4 as it makes one by default, with metadata_csum_seed and the UUID changed after, with 1 KiB blocks, 128-byte inodes and 32-byte descriptors, with gdt_csum, ext3, ext2 -- holding a two-level extent tree, directories indexed by `e2fsck -D` and attributes in a block of their own, every checksum of every metadata block held to extfmt's; then bytes the checksums cover changed one at a time, each kind in turn, and extfmt and `e2fsck -fn` held to both saying so; then each image fuzzed. Then journals debugfs writes and leaves unreplayed (`jo -c`, `jw -b`/`-r`/`-c`) -- v3 checksums and none, 64-bit tags and 32-bit, an escaped block, revokes before and after a write, a transaction of more blocks than a descriptor holds, one uncommitted -- replayed by the planner and by `e2fsck -E journal_only` to the same image byte for byte (but for the superblock fields e2fsck keeps for itself), and again with the ring turned so the log wraps; a torn commit ending the log where e2fsck ends it, a descriptor damaged under a sound commit and a copy failing its checksum refused; the planner fuzzed, and a log of 9000 transactions planned without asking the heap for more than the 512 KiB the kernel's gives at once. Then `hvboot-test.py`, hvboot's ext reader being extfmt's. The kernel's ext4 driver's judge in the fuzzer is not to share a line with extfmt |
| the shell's `format` (`src/crates/layers/fs/src/format.rs`: the table of `Maker`s, the claim, the refusals, the batched writes through the device's queue) and what lays a filesystem out for it (`src/crates/common/storage/extfmt/src/mkfs.rs`: the geometry, the order of the writes -- the old superblock's place zeroed and flushed first, the new one written last) | `extfmt-test.py` (host, in CI): mkfs's filesystems -- ext4 and ext2, 1, 2 and 4 KiB blocks, from the smallest it makes to 3 GiB and 128 groups, those to 64 MiB made over noise -- clean to `e2fsck -fn`, dumpe2fs naming their features and label, debugfs their lost+found, and what it refuses refused. Then `fs-fuzz.py`'s `format` target (host, in CI): the command on the fuzzer's disks holding nothing, noise, an ext2 or ext4 of the fuzzer's own or a partition table, with options right and wrong, `force` or not, mounted or not, requests failing and the power going at any of them -- a refusal writing nothing; what it says it made the fuzzer's own judges' clean, of the geometry, label and UUID it said, mounted by the kernel's drivers, `fstest` passing and the unmount clean; a partition's making nothing outside it; any power cut's picture holding the old superblock, none, or the whole new filesystem -- and `fs-fuzz.py --e2fsck` holding its images, made and used, to e2fsck. Then `ext4-test.py` (arm64, in CI): two disks of noise formatted in a boot, written, and read by e2fsck, dumpe2fs, debugfs and Linux after. A new filesystem `format` makes is a `Maker`, and a target of the fuzzer's that judges it by a reader of its own |
| the CRCs: `src/crates/common/crc` (CRC-32, CRC-32C and CRC-16, each as a message has it and as the bare register, slicing by eight) and what is checked by them -- GPT, the disk log, ext4's metadata_csum, gdt_csum and journal, raft's log -- and the `crc32` command (`kernel/src/crc32.rs`) | `crc-test.py` (host, in CI): each CRC's known answers, random messages of every length and alignment held to the CRC computed from its definition a bit at a time -- every form, from any register, whole and across a split, a checksum with a hole -- and CRC-32 to zlib's over messages past 64 KiB, whole and in pieces; every boot's `rust_test` holds the kernel's own build to the check values (`crc32::selftest`). Then the gates of the formats checked by what changed: `parttest.py` and `fs-fuzz.py` (GPT, the disk log), `extfmt-test.py` and `hvboot-test.py` (ext4's checksums and journal, hvboot's GPT), `raft-test.py` (the log), and `ext4-test.py`, whose files the `crc32` command sums against the host's |
| NBS's metadata service: `src/crates/modules/storage/nbs-mds` -- the member over raftmember with NBS's hooks (the cluster's id before every message and held to, the offer a host is made a member by and the listener that takes it while the host is in no cluster, raft's messages for a member a host does not have answered so, a host's lost state said by the leader, the state's digest proposed as leader and a member whose own differs stopped -- raftmember's `App::halt`), its commands (`nbs init`, `nbs host add`, `remove` and `replace`, `nbs`, `nbs hosts`; `nbs devices add` and `drain`, `nbs cs`, `nbs volume create`, `delete` and `resize`, `nbs volumes`, `nbs volume`, in `store.rs`; `nbs io`, below), the `nbs` provider (`provider.rs`, `nbs.descriptor.json`), `compact=`, and its folder; `src/crates/modules/cluster/raftmember`, the member it and cluster.ko run | `nbs-test.py` (arm64, and `--arch x86_64`): three nos on hub.py, the registry made among them first, each with a pool disk of three partitions -- an NBS cluster made and two hosts added, one through a member that does not lead; an add asked again answered with the same incarnation, a second init refused; a commit through the majority; what is stored: no volume before three hosts have a device, the pools' partitions found by their GUIDs, devices added by id and by name, the pool's disk refused for having no id, a device added again the same, c's copy of a's partition refused as a disk moved between servers, every host saying the same of the chunk servers; volumes made, made again the same, refused for another size, grown, refused smaller, past the room and for a name that is none, made for another host; a change read at once on the host it was typed at, and on the others once they have applied it; the room filled to its last slot and given back by a delete; a device drained and what it holds counted against its host's room; the provider answering through `mgmt call` as the command does; every host's digest at the same entry, the same, the leader's held to it; every log compacted into a snapshot (`compact=4096`); the leader killed, the other two electing one of themselves and committing, the killed host back from its snapshot and log with the others' state; the leader cut off by the hub, the other two electing and committing while it commits nothing, and following once healed; a host's folder removed, the leader saying its state is gone, an add of it refused, and `replace` taking it back as a new member, sent the snapshot; a member removed, its old incarnation refused added again, its chunk server kept taking no new chunks; nothing panicked. After touching raftmember, `cluster-test.py` too; the descriptor, `web-test.py` and the panel's tests |
| NBS's metadata: `src/crates/common/storage/nbsmeta` -- the NBS cluster's id and name, its members (a host and the incarnation its mds made) in the order they joined, the members removed and never taken again, the voters' policy (the first five); what is stored (`storage.rs`: each host's cs and its devices in slots, the volumes, their chunks, the replica sets the chunks share, the free list); placement (`place.rs`); the tables in copy-on-write pieces (`pieces.rs`); the snapshot written and read as a stream, the rules one read back is held to, and the digest every 64 entries (`snapshot.rs`); the commands and their answers; and `codec::Stream`, what the snapshot is read through | `nbs-fuzz.py` (host, in CI), whose `meta` target holds it to a model of its own (below), then `nbs-fuzz.py --mutants`, forty-four of them the metadata's (five the repair controller's), each caught; `raft-test.py` (host, in CI), whose `nbsmeta` target holds the members' part to a model of its own -- alone, under commands from pools that meet and from wide ones that fill the cluster, every answer and everything it shows; its snapshot read back, changed and refused, and a replica from a snapshot part way -- and its `nbsmeta_*` scenarios; `fuzz/raft/mutants.py`'s six `nbsmeta_*` mutants, each failing the gate and its own scenario |
| NBS's chunk server, its format and engine: `src/crates/common/storage/nbscs` -- the device's layout (`format.rs`: the superblock twice, the fences and the table of slots in two copies, every record's sequence number and CRC seeded with the format's id, the good copy chosen, a device's first pages told apart -- blank, NBS's, something else's -- by `holds`), the engine (`store.rs`: format and open, a torn copy written again, fences durable on every device before they are answered, open marks before a generation's first write, a chunk held by number and incarnation, a slot zeroed and flushed before its record, a close after its data, the volume a request names held to the chunk's, a free); and NBS's messages, `src/crates/common/storage/nbswire` -- a client's requests and a cs's answers, a cs's questions and its mds's | `nbs-fuzz.py` (host, in CI), whose `cs` target runs the engine over disks with a write cache through power cuts and holds it to what each fence, open mark, close and sector may be (below), and whose `wire` target reads every message back and noise as none; then `nbs-fuzz.py --mutants`, twenty-seven of the cs's and twenty of the wire's, its `serve`'s, the mds's, the copy's and the scrub's, each caught; then `nbs-test.py` (arm64, and `--arch x86_64`) for the cs over a real device |
| NBS's chunk server on a host: `src/crates/modules/storage/nbs-cs` (the devices the host's mds gives it taken in -- held with a `DiskQueue` and claimed, opened, formatted where blank, refused and said otherwise, and tried again -- served only with every one taken in; `nbs.cs` served, a request the engine has not checked asked of the host's mds first, what it vouched for kept; chunks and fences the metadata no longer has given back; `nbscs`, `nbscs chunks`, `nbscs fences`; `disk.rs`, the engine's device over a queue); what nbs-mds tells a cs (`member.rs`'s `on_cs_query`, and the listener's `NoMds`); `nbs io` (`nbs-mds/src/io.rs`); `nbs devices add`'s look at a device's first pages and `wipe` (`store.rs`); kcore's `DiskQueue::read_wait` and `write_wait` | `nbs-test.py` (arm64, and `--arch x86_64`), its N7 phase: every host's cs taking its devices in and formatting them; a device with a filesystem's magic on it refused, and taken with `wipe`; a device its cs holds added again, the same; a volume written to its three replicas and read back across a chunk's end, from the writer's host and read-only from the others; flushed, its two chunks open on every cs at the writer's generation, then closed; a writer on another host refused, then forced, and its fences refusing the old writer's write and read; a host booted again, its cs back with its fence, its open mark and closed chunk, and their data; the volume deleted, refused first while attached, and every cs giving its chunks and its fence back; a host's nbs-cs unloaded, a request to it bounced and said at once while the others answer, and loaded again, serving. Then `nbs-fuzz.py` |
| the cluster's registry: `src/crates/common/cluster/registry` -- its state (the cluster, its hosts by node with their keys, addresses, labels and maintenance, the hosts removed, the keys retired, the invitations open), its commands and their answers (maintenance, an address by role, a key added and dropped among them), applying them under the proposer's clock, its snapshot and the rules a snapshot read back is held to, the voters' policy, a host's peers | `raft-test.py` (host, in CI), whose `registry` target holds it to a model of its own -- alone, under commands from pools that meet and from wide ones that fill the cluster, every answer and everything it shows; its snapshot read back, changed and refused, and a replica from a snapshot part way; and as a group through `raft::rsm` over a lossy network, every member the same and the model's in the log's order -- and its `registry_*` scenarios; `fuzz/raft/mutants.py`'s nine `registry_*` mutants, each failing the gate and its own scenario; a kernel's build compiles the crate for its target, through cluster.ko |
| the cluster's module: `src/crates/modules/cluster/cluster` -- its member over raft's `rsm` and a `DiskLog` (`member.rs`: raft's messages taken only from the host whose node they claim, commands handed to the leader, raft's membership kept to the registry's, the leader's own removal handing the lead on first, the peers told to ipcnet), its folder (`files.rs`: `member` written last and once, the joined hosts in it, a log with no `member` cleared), `init`, `invite`, `join`, `remove`, `revoke`, `maintenance`, `address`, `rekey` (resumable, from the registry's keys and ipcnet's), `recover` (the probe, the typed name, the lost hosts removed) (`lib.rs`), the token (`token.rs`), the `cluster` provider and its descriptor (`provider.rs`, `cluster.descriptor.json`) -- ipcnet's control mailbox (`ipcnet.ctl`: `ipcwire::control`, `modules/cluster/ipcnet/src/control.rs`), a host invited held to the member's mailbox alone (`only=` in hosts.toml, `PeerConf::same_link`, `Peer::may_reach`, `monitor_for`), a peer's keys changed in place and the host's own key staged and switched (`files.rs`'s identity.pem and next.pem, `Peer::live`, `Server::use_identity`), and the channel's word on which host sent a message (`kernel_ipc_peer_name`, `kcore::ipc::peer_name`) | `cluster-test.py` (arm64, and `--arch x86_64`): three nos on hub.py's LAN, no key pinned by hand -- a cluster made, two hosts invited by their keys and joined (one through a member that does not lead), a wrong secret, another host's token, a token used again and a second `init` refused, an invited host's pings to a member's echo refused, every host up to every other and pinging over what the registry made, the provider through the bus, maintenance and an address seen by every host, a rekey under 6000 pings with every one answered and no connection lost, every host rebooted at once, `recover` on the last of three and a host installed anew joined after, the leader of two removed with the lead handed on first and its key refused when invited again; then `net-fuzz.py`'s `ipcnet` target (the control mailbox's requests, hosts.toml held to each; the world's host made a host invited and a host again -- nothing past its one name, no monitor, never made anew -- and pinned by a second key, kept; a key staged) and `raft-test.py` (the registry, `force_alone`); for the provider `web-test.py` (its descriptor) and the panel's tests (`cd panel && npx playwright test`: the cluster's page, an invitation made and taken back); after touching the ABI, `sshd-test.py` (arm64: the ffi digest) and `ipc-fuzz.py` |
| the raft crate: `src/crates/common/cluster/raft` -- the protocol with no I/O (`node`: elections, PreVote, CheckQuorum, replication and its flow control, the commit rule, snapshots sent in pieces, learners and one-server membership changes, leadership transfer, ReadIndex), its wire (`wire`), the storage in memory and the bootstrap (`storage`), the log on disk (`log`: segments of records each with a CRC32C, the hard state a record of it, snapshots by rename and named by a marker, the torn tail cut, what an open reads synced before it is used, compaction; its CRC32C the `crc` crate's), the replicated state machine (`rsm`: the `Ready` loop over a store, clients' sessions, linearizable reads and reads at an index, snapshots frozen and written while applying goes on), a member made a group of itself alone (`recover`: `force_alone`) | `raft-test.py` (host, in CI): known answers -- CRC-32C's among them -- -- every message written and read back, bytes that are not one refused each for its reason, the storage's log across its pieces, a lone voter, the membership change rules; the known traps played out step by step (the paper's figure 8, a deposed leader asked to read, a change before the leader's first commit, a snapshot overtaken while sent, a wiped member back under its old id -- which the checks must catch -- a vote not on the disk before it is answered, the election timer and a vote, a follower's commit past what its append showed, a PreVote taken for a vote, an append after an entry of another term); groups of one to seven over a network that loses, doubles, reorders and cuts, members crashing and back with what they wrote, held at every step to one leader a term, leader completeness, log matching, committed entries never changed and applied alike, reads never stale, elections only once a timer runs out, and progress once healed; the wire fed noise; disks that fail reads, a member stopping on the first and started again; once healed, every read asked of a leader answered; `MemStorage` against a model over logs of tens of thousands of entries; the review's controls (`review.rs`); a member fed messages no correct member sends -- of every kind and value, answers true and false -- never panicking, its term, vote and commit never going back; every allocation held to the 512 KiB one block of the kernel's heap gives (`long_log`: a log of 42,000 entries started over). the log on disk through power cuts on a disk that keeps POSIX's promises and no more -- every write since a file's sync kept, lost or torn to zeros, noise or its end, a run of the directory's changes kept -- read back after each as the state at the last sync with some first records written after it, at a record's boundary, or as a snapshot that failed half way would have left it: nothing synced lost, nothing torn read, nothing cut read again; calls of the disk that fail, the log stopping on the first; a byte of a sealed segment or of the snapshot changed, and the log refusing to open; noise named as its files refused or read as nothing; a scenario for each rule of the log's; a register through `rsm` in groups on the log on disk over the crash disk, members crashed and started again, clients with sessions retrying after timeouts, every client answered once calm and the history of every operation checked for linearizability. After touching `node.rs`, `log.rs`, `rsm.rs` or `recover.rs`, `fuzz/raft/mutants.py` too: each rule it takes out must fail the gate, and the rule's own scenario |
| a guest of more than one CPU: its local APIC (`hypervisor/hv/src/lapic.rs`, its TSC-deadline timer and `IA32_TSC_DEADLINE` among it), KVM's PV EOI (`MSR_KVM_PV_EOI_EN`, `pv_eoi_arm` and `pv_eoi_sync` in `run.rs`, `Lapic::eoi_may_be_quiet`) and what its CPUID tells it of where its CPUs sit (`place::Shape`, the topology leaves in `hypervisor/hv/src/policy.rs`) -- under VT-x its virtual APIC too, and the CPU's own delivery of its interrupts and EOIs, by default where it has posted interrupts (`apicv`, `noapicv`; `load_apic` and `take_apic` in `hypervisor/hv/src/vmx.rs`, the page, the controls and the MSR bitmap's three writes in `hypervisor/hvarch/src/x86/vmx.rs`, `Lapic::from_cpu`), and posted interrupts (`PostedInterrupts`, `Kick::notify`, the host interrupts an exit acknowledges and `deliver_host_interrupt` hands over, `post_edge` and `take_posted` in `run.rs`, the notification vector 0xFC) -- the mailboxes and doorbells between its CPUs (`hypervisor/hv/src/smp.rs`), the MP table (`hypervisor/hv/src/linux.rs`), its devices' MSI-X (the capability and table page in `hypervisor/hv/src/devices/pci.rs`, the vector registers in `devices/virtio.rs`, delivery in `run.rs`), the CPU loop in `hypervisor/hv/src/run.rs`, `cpus=`, `run_cpus` in `modules/hypervisor/hv/src/guest.rs` and where a guest's CPUs and its disks' task are placed (`hypervisor/hv/src/place.rs`, held in `modules/hypervisor/hv/src/cores.rs`, `hv cores`), and in either backend INIT, the start-up IPI, CR0, NMIs and PAUSE | `hv-test.py`, whose `pveoi` guest ends interrupts through its PV EOI word and by the register, alone and nested, and whose `smp` guest starts a second CPU by INIT and a start-up IPI into real mode, has it IPI the first, and runs the first's APIC timer out (under TCG it needs QEMU 9.2 or later, and refuses an older one), and whose `apicv` guest -- x2APIC self-IPIs held by CR8 and taken by priority, one nested, IRR and ISR read, three IO-APIC levels ended by EOI -- must see the same whoever delivers its interrupts: the host under AMD-V, the CPU under VT-x with APIC virtualization (`hv info`), which hv-test then holds it to; and whose `posted` guest's first CPU must take a thousand IPIs from its second while it spins -- in its guest, where the CPU has posted interrupts, leaving it a handful of times -- run it under nested VT-x and on the EX44's silicon; `hv-fuzz.py`, whose `platform` target models the CPU's side and holds the run loop to losing nothing the CPU left in the page, to never waiting with something posted untaken, and -- running hvarch's `Kick` as it is, with the host's tick landing on the way in and an edge coming as the host's timer is armed -- to never entering with that timer armed past the edge its loop asked for; then `hv-linux-test.py --bzimage <bzImage> --initrd <cpio> --cpus 2` -- and with `--kvm` for a kernel built with `KVM_GUEST`, which must find KVM's interface, make EOIs through its word and keep its spinlocks native -- and with `--apicv` under VT-x, whose report must then say the CPU delivered every one of the APICs' interrupts (x86-64, **manual**; Linux 6.6 or later with `SMP`, `X86_X2APIC` and `X86_MPPARSE`): every CPU brought up and online, each on its own APIC timer in TSC-deadline mode, the two one core's two threads sharing a cache as the guest's sysfs says -- on QEMU's AMD `max` and with `--qemu-cpu Skylake-Client-v1,+svm,+npt,+nrip-save` too, Intel's leaves -- IPIs between them, a guest of two on one core's two threads of nos's, a core of its own (the gate boots nos with two cores of two threads), a second guest on the other core and a third refused, through the whole VM phase -- and with `--disk`, `--net` and `--attach` too, where the change reaches them: there the guest's virtio devices must interrupt it by MSI-X, and a guest of one CPU's through the 8259. A guest of one CPU keeps the machine it always had, so run the gate without `--cpus` as well -- under both backends, since INIT, CR0, NMIs and PAUSE are each backend's own. Where a guest's CPUs are placed: `hv-fuzz.py` too, whose `place` target holds every placement and refusal to a model of its own |
| MMIO: the guest page walk (`hypervisor/hv/src/walk.rs`), the decoder (`hypervisor/hv/src/insn.rs`), the emulator (`hypervisor/hv/src/mmio.rs`), the local APIC's xAPIC mode (`hypervisor/hv/src/lapic.rs`) | `insn-test.py` (host, in CI): the decoder against the encodings clang gives for every form Linux's MMIO accessors are, each cut short, and random bytes; the walker against hand-built page tables. `hv-test.py`, whose `mmio` guest has the host perform fourteen stores and loads of a device on each backend, and whose `fault` guest's report must show the state it faulted in (under VT-x an EPT violation reads only what an access takes, and the rest for a report). Then `hv-linux-test.py --bzimage <bzImage> --initrd <cpio> --cpus 2 --xapic`, and with a `--cmdline` of `nox2apic` (x86-64, **manual**): SMP through the xAPIC page, through the VM phase and with `--disk`, `--net` and `--attach` -- under both backends, since the guest now goes on after a nested fault on each -- and without `--xapic`, the x2APIC default |
| a guest's CR4: what AMD-V and VT-x stop it at and what `hvarch` keeps there (`hypervisor/hv/src/cr.rs`, `insn::decode_mov_cr`, the CR4 intercepts in `hypervisor/hv/src/svm.rs` and `hypervisor/hvarch/src/x86/svm.rs`, the CR4 guest/host mask in `hypervisor/hvarch/src/x86/vmx.rs` and the bits `hypervisor/hv/src/vmx.rs` lets a guest own), the bits its CPUID lets it set (`policy::cr4_allowed`) | `hv-test.py`, whose `cr4` guest clears MCE and sets PGE, has a bit its CPUID does not offer (PKE, or CET where PKU is offered), PAE cleared and bit 32 refused, and is held to the CPU's CR4 keeping MCE (QEMU's TCG stops no read of CR4 -- it checks every control register's read against CR0's intercept -- so there its reads are the register's, and the guest's check knows it by the hypervisor's signature); `hv-fuzz.py` (the platform target holds every read and write to what it should have done); `insn-test.py`; then `hv-linux-test.py --bzimage <bzImage> --initrd <cpio>`, and with `--cpus 2`, whose second CPU writes CR4 from 32-bit code (x86-64, **manual**) -- under VT-x too, which nested KVM on an Intel host gives both gates |
| a guest's debug registers and its #DB: what AMD-V and VT-x stop it at and keep apart (`hypervisor/hv/src/dr.rs`, `insn::decode_mov_dr`, the DR intercepts and `LoadedDebugAddrs` in `hypervisor/hvarch/src/x86/svm.rs`, MOV-DR exiting and DR7's switch in `hypervisor/hvarch/src/x86/vmx.rs`, the #DB's DR6 in `hypervisor/hv/src/vmx.rs`) | `hv-test.py`, whose `dr` guest runs two VMs on one CPU, one after the other: the second must read 0 where the first left its own, and each one's breakpoint, on an instruction of its own, fire -- which only its own DR0 made the CPU's before its entry can do; `hv-fuzz.py` (the platform target holds every `mov` of them, at CPL 3 and under general detect too, to a model of its own); `insn-test.py`; then `hv-linux-test.py --bzimage <bzImage> --initrd <cpio>`, and with `--cpus 2` (x86-64, **manual**): Linux writes all four at every CPU's bring-up, which the boot's report counts. `hv-test.py`'s `debug` guest too, for a #DB given back |
| a guest's MSRs that neither extension switches -- TSC_AUX (`hypervisor/hvarch/src/x86/msrs.rs`, `GuestMsrs`, `policy::Own`, `Backend::set_own_msrs`) -- and the instructions a guest runs as the CPU has them that its CPUID offers only where they run: RDTSCP, RDPID and INVPCID (`hvarch::x86::offers`, VT-x's `enable RDTSCP` and `enable INVPCID`, AMD-V's RDTSCP intercept in `hypervisor/hv/src/svm.rs`) | `hv-test.py`, whose `tscaux` guest runs two VMs on one CPU, one after the other: the second must read TSC_AUX as 0 where the first left its own, by RDTSCP, RDPID and RDMSR, each instruction run where its CPUID offers it and a #UD where not -- as `hv info` says they run, which hv-test holds it to -- and INVPCID's types run; `hv-fuzz.py` (the policy target holds TSC_AUX and the three bits to a model of its own, the platform target every write the run loop hands a backend to what the MSR can hold); then on the EX44's silicon, whose INVPCID is the only one a gate reaches run |
| a guest's own speculation controls -- its IA32_SPEC_CTRL over the host's for its run and the host's put back after (`hypervisor/hvarch/src/x86/spec.rs`: `guest_controls`, `State::load_guest`, `State::exited`, the VMCB's `spec_ctrl` for virtual SPEC_CTRL), the IBPB between two guest CPUs and those a guest asks for (`State::entering`, `GuestMsrs::set`), the CPUID bits and MSRs the policy gives them | `hv-test.py`, whose `spec` guest runs two VMs on one CPU, one after the other: offered exactly what the host switches, as `hv info` says -- under QEMU's TCG nothing, every access a #GP, so run it under KVM on an AMD host (the AX41) for the rest: the second reads SPEC_CTRL as 0 where the first left its own, each runs under its bits over the host's and the CPU's is the host's alone after, every IBPB it asked for is answered before its next entry; and `hv` warns of any CPU whose SPEC_CTRL is not the host's. `hv-fuzz.py` (the policy target holds every bit to what the guest's CPUID promises, the platform target every value handed a backend to what the CPU takes); then VT-x's on the EX44's silicon |
| a guest's x87, SSE and AVX registers, its PKRU and its XCR0: what an entry loads and an exit saves, and the XCR0 each runs under (`hypervisor/hvarch/src/x86/fp.rs`, `GuestFp`), the host's CR4 -- PKE among it -- XCR0 and IA32_XSS while the extension is on (`fp::on`, `fp::off`), the XSETBV a guest stops at (`xsetbv` in `hypervisor/hv/src/cr.rs`), the XSAVE line, PKU and OSPKE and leaf 0xD of the CPUID policy (`hypervisor/hv/src/policy.rs`) | `hv-test.py`, whose `avx` guest -- two CPUs taking turns on one host CPU, by IPI -- must find YMM0-15 zero as each starts, though the other's pattern was in the CPU's registers, and its YMM registers, MXCSR and x87 control word as it left them at every one of 64 turns, its XSETBVs held to what a CPU takes and leaf 0xD as a CPU gives it, and whose `pkru` guest -- the same two CPUs, one with PKRU in its XCR0 and one without -- must find PKRU 0 as each starts and its own at every turn, where `hv info` says PKU is offered; `hv-fuzz.py`, whose `policy` target holds leaf 1's XSAVE line, leaf 7's PKU and OSPKE and leaf 0xD to a model of its own -- for a made-up host and the asking CPU's CR4 and XCR0 -- and whose `platform` target every XSETBV; then `hv-linux-test.py --bzimage <bzImage> --initrd <cpio>` (x86-64, **manual**), whose guest's kernel must turn on the x87, SSE and AVX, 832 bytes of them, and PKRU where the CPU has PKU -- under both backends: VT-x on the EX44's silicon, where nothing nested stands between the switch and the CPU |
| the IO-APIC (`hypervisor/hv/src/devices/ioapic.rs`), what drives its pins -- the PIT's edges, the serial port's interrupt (`devices/uart.rs`), the SCI, virtio's INTx -- the local APIC's trigger-mode register and level EOIs, the mailboxes' level bits, the IO-APIC in the MP table and the MADT | `hv-test.py`, whose `ioapic` guest takes the serial port's pin as an edge and as a level through the IO-APIC's page. Then `hv-linux-test.py --bzimage <bzImage> --initrd <cpio> --cpus 2 --ioapic` (x86-64, **manual**): the timer and the serial port through the IO-APIC, the VM phase, `--cpus 4` with a `--cmdline` without `no_timer_check` (Linux's own check of the timer through it), `--acpi` (the SCI a level on pin 9), `--disk` and `--net` with `pci=nomsi` (virtio's INTx a level through it) and `--attach`; and the gates without `--ioapic` too, whose 8259 and serial port the same code drives -- under both backends. A distribution's kernel: `hv-distro-test.py --iso ... --cpus 2 --ioapic` |
| anything a guest reaches in `hv`: a device (`hypervisor/hv/src/devices/`), the local APIC or IO-APIC, the MMIO path, the Linux loader, the ACPI tables, the CPUID and MSR policy (`hypervisor/hv/src/policy.rs`), the run loop (`hypervisor/hv/src/run.rs`) and what its CPUs reach each other by (`hypervisor/hv/src/smp.rs`); the guests' network: the switch, its DHCP server and NAT (`modules/hypervisor/hv/src/{net,dhcp}.rs`, `layers/net/src/nat.rs`) | `hv-fuzz.py` (host, in CI): those files compiled for the host over stand-ins for the kernel, its network layer and the CPU, with overflow checks on, and driven with random inputs -- each device on its own, a virtio device's notifications both ways held to a Linux driver's every request served and heard of (`virtq`), the switch and NAT on their own and together, and the whole machine through `LinuxGuest::run` on every CPU of a guest of several, a script in place of the CPU saying what the guest does at each entry; the policy against a model of its own, over a host CPU made up an input; where guests are placed (`hypervisor/hv/src/place.rs`) against a model of its own, over made-up hosts; a NIC's offloads (`hypervisor/hv/src/devices/offload.rs`) against a model of their own, frame by frame, and what it puts into its driver's buffers -- mergeable, a send whole, a checksum left partial -- against another. A panic, a broken invariant (an interrupt injected over another, a frame cut from a send other than the model's, a leaf or an MSR the model does not let through, a CPUID bit offered with the MSR it promises refused, a disk request outside the disk, a vCPU entering its guest with a lock held, a NAT rewrite wrong in any field, a guest taking another's traffic or sending as another, a frame reaching another network's guest -- a port handed over at a lock of a send's own among them), a spin and a hang are findings, each with the seed and iteration that make it again. Fixed inputs by default -- the gate; `--seed N --seconds S` for a campaign, and a new target for a new device |
| a guest's ACPI: its tables (`hypervisor/hv/src/acpi.rs`), the fixed hardware and the SCI (`hypervisor/hv/src/devices/pm.rs`, the ELCR in `devices/pic.rs`), what the loader gives a kernel of them (`hypervisor/hv/src/linux.rs`), `hv stop`'s power button (`modules/hypervisor/hv/src/vms.rs`), the panic device in the DSDT (`hypervisor/hv/src/devices/pvpanic.rs`); the SMBIOS tables (`hypervisor/hv/src/smbios.rs`) and a VM's UUID (`uuid` in `modules/hypervisor/hv/src/{config,store,vms}.rs`) | `hv-linux-test.py --bzimage <bzImage> --initrd <cpio> --acpi` (x86-64, **manual**; a kernel with `ACPI`, `ACPI_BUTTON` and `INPUT_EVDEV`), with and without `--cpus 2`: the boot finds the tables and says nothing of a firmware bug, reboots through the FADT's reset register, the SCI is IRQ 9 and the PM timer a clocksource, `hv stop` has BusyBox's `acpid` power a guest off, and a guest that hears the button and does nothing is stopped when its time is up; the gate without `--acpi` too, whose guest never hears the button; `--pvpanic` (a kernel with `ACPI` and `PVPANIC_MMIO`): a guest's panic stops it before its own `panic=` reboots it, the report saying so, one with `restart` booted again up to its limit, its driver bound to `QEMU0001`; `--smbios` (`DMI`, `DMIID`): what `/sys/class/dmi/id` says, and a VM's UUID the same from start to start, given one at its first when its vm.toml has none; `hv-fuzz.py`'s `smbios` target; and `hv-distro-test.py`, whose Alpine and Debian boot on their ACPI and whose Debian `hv stop` shuts down |
| anything the network hands the kernel: the receive path (`layers/net/src/device.rs`), ARP, ICMP, UDP, TCP, the DHCP client, the DNS resolver, the HTTP client over TCP and over TLS (`layers/net/src/http.rs`, `layers/tls/`), TLS between ends that know each other's keys (`layers/tls/src/pinned.rs`: TLS 1.3 -- the host API's server, clients let in by their keys' fingerprints, and the client one host is to reach another's with, the server known by its key), the UDP shell, netconsole; `ssh/` and `modules/net/sshd`; `modules/mgmt/api` | `net-fuzz.py` (host, in CI): the kernel's own `net`, `tls` (both pinned ends too), `fs`, `ssh`, `mgmt` and the sshd and api modules' sources compiled for the host over a kernel written for the purpose -- its tasks run one at a time, the locks and the rules about them checked -- with the world around the machine modelled protocol by protocol (a TCP peer that keeps both streams, DHCP, DNS, HTTP and TLS servers, TLS clients with the known key, a stranger's or none, a log collector, UDP shell and SSH clients, hosts that lie about ARP), and driven with random inputs, one target a protocol. A panic, a broken invariant (a frame no stack sends, a stream byte never sent, a lease kept past its end, an ARP cache holding what nobody said, a log line twice, a body cut short called whole, a login for a key nobody allowed, a TLS client let in or answered without the known key, a TLS server taken without its key, a refusal for any reason but its own, the server's side left in TIME-WAIT), a lock held across a sleep, a task, frame or slot kept for good, a spin and a hang are findings, each with the seed and iteration that make it again. Fixed inputs by default -- the gate; `--seed N --seconds S` for a campaign, and a new target for a new protocol. After touching `layers/tls/src/pinned.rs`, a build of `tls --features pinned` for both kernel targets too (`docs/tls.md`: the kernel's build of either arch makes one, building api.ko) |
| anything a disk hands the kernel: the partition tables (`layers/block/src/lib.rs`), the device table's bounds and claims (`layers/block/src/table.rs`), the disk log (`layers/block/src/disklog.rs`); ext2, the VFS and its C ABI, the file ABI (`layers/fs/src/files.rs`), a disk's image held and read and written off the VFS's lock -- its own map, grown a chunk at a time, a vector at a time -- what a guest discards given back, and where an image has data (`layers/fs/src/image.rs`, `OPEN_HOLD`, `map_blocks`, `map_runs`, `allocate`, `punch`, `extent`), a filesystem's space (`FileSystem::space`, `Vfs::space_of`), the shell's storage commands, `root=` (`layers/fs/src/rootfs.rs`) | `fs-fuzz.py` (host, in CI): the kernel's own `block` and `fs` compiled for the host over the fuzzers' common machine (`fuzz/common/`) and disks whose media are the fuzzer's -- each with a volatile write cache, a power switch and requests that fail -- with images made and judged from the formats (an ext2 mkfs and an e2fsck of the fuzzer's own; an ext4 mkfs and check of its own -- extents, unwritten extents over noise, uninitialised groups, metadata_csum, a journal holding transactions nobody replayed over stale homes, which its check replays by a JBD2 reader of its own and orphans it releases as e2fsck does -- sharing nothing with extfmt; both held to e2fsprogs by `fs-fuzz.py --e2fsck`, in CI (the replay to e2fsck's byte for byte, mke2fs's images and their damage to e2fsck's verdicts, orphans to `e2fsck -fy`'s tree); MBR, GPT, the disk log's area), and driven with random inputs -- every allocation of the kernel's code held to the 512 KiB one allocation of the kernel's heap gives (`fuzz/common/machine/heap.rs`), past which the kernel panics: sound images worked through the VFS against a model of the tree, damaged ones for robustness, many mounts at once. A panic, a broken invariant (a request outside a device, a write to a filesystem mounted read-only, a partition the table does not have, two claims on one sector, an answer the model does not give, a hole an image's extents name where the model has data, a space ext2 says that its bitmaps do not, an image's chunk ext4 refused with room for it and no request failed, an image a clean unmount leaves unclean or a power cut leaves corrupt), a lock rule broken, a spin and a hang are findings, each with the seed and iteration that make it again. Fixed inputs by default -- the gate; `--seed N --seconds S` for a campaign, and a new target for a new filesystem |
| what firmware, a bootloader or a disk hands the C++ -- the device tree and `Board` (`arch/arm64/{fdt,board}.cpp`), Multiboot2's tags (`arch/x86_64/grub.cpp`), the file of Intel's microcode updates GRUB loads beside the kernel (`arch/x86_64/microcode_file.cpp`), the ACPI tables (`drivers/acpi.cpp`), the memory map (`mm/memory_map.cpp`), the command line (`kernel/parameters.cpp`), the GRUB environment block (`lib/grub_env.cpp`), the module loader (`kernel/module.cpp`), and the printf every line is formatted by (`lib/format.cpp`) -- and the memory under everything: the page tables and the physical page allocator (`mm/page_table.cpp`), the kernel heap (`mm/{allocator,pool,page_allocator,va_allocator}.cpp`) | `cpp-fuzz.py` (host, in CI): those files compiled for the host as they are, under ASan and UBSan, over stand-ins for the kernel around them and a host HAL in place of each arch's inline one (`fuzz/cpp/`), driven with random inputs -- trees of QEMU virt's shape and damaged ones, Multiboot2 infos, microcode files, ACPI tables in a physical memory of the input's, firmware maps, command lines, environment blocks, `.ko` files for both arches, sequences of page and heap operations on a machine taken to its last page, a huge pool reserved and its 2 MiB blocks handed out and back, free pages and blocks zeroed ahead -- and held to a model of what each header says: a sound tree's board, the newest update a CPU may load, the machine the tables describe and what of the tmp_map window their parse holds, the host's own `vsnprintf`, the map's answers asked naively, the block's variables, a module's image byte for byte and its pages' permissions, every page's reference count and translation, every heap block its own, every block of the huge pool the pool's and no one else's, every page on the zeroed list and block known zero reading as zeros and taken by a guest's allocation first. A sanitizer's report, a kernel panic, a broken invariant, a lock rule broken (an allocation with a spin lock held), what an error path left behind and a hang are findings, each with the seed and iteration that make it again. Fixed inputs by default -- the gate, a minute and a half; `--seed N --seconds S` for a campaign, and a new target for a new reader of what the C++ is handed |
| NAT (`layers/net/src/nat.rs`), the guests' switch and its DHCP server (`modules/hypervisor/hv/src/{net,dhcp}.rs`), its networks (`network=`: a port on one, or alone), a NIC's queue pairs and receive steering (`hypervisor/hv/src/devices/net.rs`), a NIC's receive interrupts held apart and the wakes the switch holds back for them (`Net::set_rx_moderation`, `rxmod=`, `Backend::quiet_until`, `rx_due` in `run.rs`) | `hv-linux-test.py --bzimage <bzImage> --initrd <cpio> --net [--internet]` (x86-64, **manual**; with `--cpus 2` the NIC's two queue pairs, each its own vector): two guests on one network reach nos and each other, a third alone reaches nos and not them -- its frame for one's MAC dropped and counted in `hv list` -- and through NAT a web server on the test machine -- a page, a megabyte whose md5 must match, a ping; `udhcpc` must get its port's address, nos as its router and nos's DNS server from the switch; a busy guest must be kicked out of its guest for its frames (`hvarch::x86::svm::Kick`); IPv6 between the two, at the addresses the switch's router advertisement has them make and at their link-local ones, and a guest asking for the other's told it is taken; and a guest that claims the other's address, IPv4 or IPv6, must get nothing through, counted against it in `hv list`. The boot self-test checks every rewrite against checksums summed from scratch, on both architectures, every boot; and `hv-fuzz.py` (host, in CI) takes each apart with frames of every kind, as each guest and as each other |
| `hv forward` and its relay (`modules/hypervisor/hv/src/{forward,relay}.rs`), a listener's backlog and buffers (`kernel_tcp_listen`, `TcpListener::bind`, `bind_bulk`) | `hv-fuzz.py` (host, in CI), whose `relay` target drives the relay between two scripted sides: every byte in order and once, neither direction held back by the other's shut window, an end at once and a stop within a round, a side that takes nothing given up at the stall time and not before, no wait longer than the relay was told; `net-fuzz.py`'s `tcp` target for a bulk listener's connections; then `hv-linux-test.py --bzimage <bzImage> --initrd <cpio> --net` (x86-64, **manual**), which fetches a guest's page through a forward, and a megabyte every byte of which must be in its place, has `hv stop` take a VM's forward away with it and `hv start` add the one its vm.toml names; and for the backlog `sshd-test.py` (arm64) and `tcp-test.py` |
| what a distribution's kernel meets that a purpose-built one does not -- the PIT's one-shot modes, the RTC, CPUID's topology, ACPI, a getty (`hv send`), read-only disks, `hv wait boot=N` | `hv-distro-test.py --iso <alpine-virt-*.iso> [--debian <debian-*-nocloud-amd64.raw>]` (x86-64, **manual** -- the images are downloads): Alpine's `virt` ISO as it ships, its ISO the guest's read-only disk, to a login prompt; root logs in, the clock is the host's, a write to the ISO fails, `reboot` and `restart` bring it back, it pings over the switch, and its own sshd -- installed by `apk` from the ISO -- is logged into from outside through `hv forward`; and Debian's cloud image under systemd, provisioned by credentials, `running` with no failed unit, its root written to and still there after a reboot |
| the system image and what pours and boots it: `make image` (and its `/lib/panel/bundle`), `build/image/` (the image's GRUB entries and `/etc/rc`), `scripts/nos-install` (and its `--panel`), `scripts/nos-deploy.py`, `root=UUID=` | `image-test.py` (x86-64, in CI): `nos-install` under Linux, as root, on a loop device -- under sudo, or in the nos-builder image `--privileged` on a Mac -- pours the image onto a partition, every file of the image there as it is in the image (a file the image gains goes into `SYSTEM`/`LAYOUT` in `nos-install` too, or the install refuses it by name), a GRUB standing in for a server's boots nos from it once by UUID, sshd from the image's `/etc/rc` lets in the key it was given, no UDP shell answers and the next boot is the server's own; then `update` keeps the host key and takes the UDP shell asked for, and `--panel` prints root's password for the panel once and keeps its scrypt hash alone, gives /etc/rc the panel's lines, and the next boot's api.ko serves; and `status`, `rollback` and `uninstall` |
| the management panel: `panel/` -- its pages made of the providers' descriptors, its relay to the hosts' APIs, its login and sessions -- its bundle and what its VM runs (`scripts/panel-bundle.py`, `panel/appliance/`: install.sh, the Caddyfile, the units), and `api panel create` (`modules/mgmt/api/src/panel.rs`: the panel's identity and certificate, its seed, the vm provider's create with its `port`, `forwards`, `protected` and `start`, `allow_hv0`) | `cd panel && npx playwright test` (host, in CI): the panel against the mock API, which serves the providers' own descriptors from the tree; `panel-appliance-test.py <bundle>` (host, in CI, Docker): the bundle as `scripts/panel-bundle.py` makes it run as its VM runs it, in Debian's image -- TLS by the address alone answered with the panel's certificate, HTTP/2, root's login by the seed's hash, a call through the panel to the API over mTLS; `api-test.py --arch x86_64`: /etc/rc's `api panel create` makes the VM and starts it as one job, lets its key in and its address in on hv0, and leaves on nos's root a user-data whose seed, keys and certificates pair as OpenSSL takes them; `image-test.py` (`--panel`). The whole of it -- Debian booted under hv, cloud-init, systemd -- is by hand on the EX44 or the AX41: under TCG it is a guest twice emulated |
| a wait primitive (`WaitGroup`, `Mutex`, `RwMutex`, `sleep`, `Event` -- its signal's window and `let_go`), the scheduler's choice of the idle task, the local timer and what arms it (`Cpu::local_timer*`, `hal/timer.h`, the sleep list and `TaskQueue::wake_due`); a task's run state, `block`/`unblock` and the wake stack (`Task::run_state_`, `TaskQueue::push_wake`/`drain_wakes`) | `idle-wait-test.py` (x86-64, needs 12+ CPUs and KVM): a qd=1 read on a machine whose other CPUs are idle. No smoke boot can see this -- it has more polling tasks than CPUs, so nothing ever reaches idle. The boot's own `test_sleep_prompt` holds a sleep to its deadline on every CPU of every smoke boot, and `test_wake_storm` passes a token round a ring of tasks pinned across every CPU and fails the boot on a wakeup lost; `test_event_let_go` frees short-lived waiters as they return while every other CPU signals their event, each signal holding the task it found inside its window, and fails the boot unless some waits met one there and waited it out; after touching the timer, the panic path's ticking too (`panic-test.sh`, both arches) |
| the scheduler's decision: EEVDF's rules (`kernel/eevdf.{h,cpp}`) and how `TaskQueue::schedule` drives them -- a task's weight and slice, its lag across a sleep or a move, a woken task's placement, the queue's current task, the slice's end on the local timer (`arm_slice`), the idle task in the count or out of it | `cpp-fuzz.py` (host, in CI), whose `eevdf` target drives the queue in `schedule`'s order and holds every step to a model of EEVDF computed from scratch in 128 bits -- every pick, every lag a task leaves and joins with, every charge, every task on its list or current -- and tasks that never sleep to their weights' shares; then the smoke boots, whose `test_eevdf` holds a wakeup beside one spinner and two to well under a slice at the median, signalled at random points of the spinners' slices, and nice 0 to about three times nice 5's CPU; and the row above's gates, the scheduler's waits and its timer being theirs |
| the zeroing of free pages ahead of their allocation: the two free lists and the zeroed mark (`Page::ZEROED_BIT`), `zero_free_page`, `zero_free_huge_page` and the pool's zeroed bits, what each allocation takes first (`mm/page_table.cpp`), the zeroing tasks (`mm/page_zeroer.cpp`), `zerofree=` | `cpp-fuzz.py --target pagetable` (host, in CI; its model above: every free page on one list, marked iff zeroed, zeroed pages zeros, a guest's allocation taking them first); the smoke boots, both arches, whose `test_zero_free` zeroes pages before the tasks start, takes them by a guest's allocation under `zerofree=verify`'s check, writes them, frees them and takes them back by the kernel's, zeroed again; and when there is a pool, takes a block written all over from under the zeroing a hundred pages into it -- the allocation's own pick, its every word zero after -- and zeroes a block of the pool whole and hands it out as it is; the tasks themselves only run in a boot that lasts: `trace on mm` and `memusage` on one (all of a 2 GiB HVF machine's free memory zeroed within a second of `boot: complete`); their stop, at the halt: the arm64 halt gates (`ext2-test.py`, `netconsole-test.py`) |
| the heap's counters: `mm/heap_usage.{h,cpp}` -- a CPU's slot written with interrupts off, the folds into the totals at a slot's worth, the peaks raised there and at a read -- what an allocation takes (`AllocatorImpl::footprint`, `Pool::footprint`, `PageAllocatorImpl::footprint`, worked out as `alloc_block` and the page allocator take it), Rust's accounting in `kernel_alloc`/`kernel_free` (the request worked out again from the layout Rust frees with), `free_pages_lowest`, `memusage`'s lines | the smoke boots, both arches: `test_heap_usage` checks the footprint of the sizes the heap treats differently -- pool classes, half a page, page runs, one past the largest -- and a private counter's totals, peaks and counts through folds by bytes and by blocks, exactly; `test_heap_usage_cpus` changes one on every CPU at once, past a slot's worth each time, and finds every change counted and the peak between one CPU's blocks and all of theirs, give or take a slot's worth a CPU; `cpp-fuzz.py` for the allocator's files; the cost by `heapbench` against the tree before (arm64, HVF) |
| a TLB shootdown -- `CpuTable::flush_local_tlb` (this CPU's flush and the mask of the others on one CPU), `get_remote_cpu_mask`, `send_tlb_ipi` -- what holds a caller to its CPU (`preempt_can_move`), a `tmp_map` slot from its map to its unmap, arm64's `switch_context`; the page allocator's checks (`take_kept`'s mark, `check_unreferenced`, a page freed twice) and `heapcheck=` | the smoke boots, both arches: a shootdown's mask, or a `tmp_map` slot, asked for by a caller that could be moved is a panic at its first call, whatever the timing -- every boot frees and maps; `cpp-fuzz.py --target pagetable` and `--target heap` (host, in CI; `heap` with `heapcheck=on` one input in four); then `heapbench roam` (x86-64 under KVM, four CPUs or more, **manual**: `insmod /heapbench.ko`, `heapbench roam ms=10000` a few times, no `CORRUPT` and no panic), unbound workers moved between CPUs as they free -- the tree before the fix panicked in its first round, a kept block written after its free; the race's own window shows only on x86, where a shootdown is IPIs |
| the tracepoints: `kernel/tracepoint.{h,cpp}` (the events, `emit()`, `trace`, `tracepoints=`, and the scheduler's sites -- `sched.cpp`'s `switch_to`, `schedule`, `switch_complete`, `wake_due`, `arm_slice`, `schedule_current` and `note_run`, `task.cpp`'s `unblock`, `start`, `run` and `exit`, `cpu.cpp`'s `idle`, `idle_mwait` and `ipi`, `Task::switched_in_at_`, `TaskQueue`'s open idle -- the wakeup trigger and its arming, `trace dump`), Rust's sites (`kcore::trace`: `on`/`emit`, the payloads and `names!` enumerations in `trace/events.rs` -- in the module digest too; `base/ffi/src/trace.rs`, `rust_ffi.cpp`'s `kernel_tp_*`), the hypervisor's (`hypervisor/hv/src/tp.rs`; `vm::Cpu::enter`'s `hv_exit`, the run loop's `deliver`, halt, wake and timer, `smp.rs`'s doorbells, `post_edge`; hvarch's `Run` stamps around the world switch, taken only while `hv_exit` is on), a guest disk's (`Blk::start` and `complete`, the hv module's disk task, `fs::image`'s synchronous path and `make_written`, `block::queue`'s `start` and `on_done`, ext4's `Txn::commit`), TCP's (`layers/net/src/tcp.rs`: `tcp_send` in `send_segment` with why each went, `tcp_ack`, `tcp_recv` around `handle` with its verdict, `tcp_loss` at the recovery's turns, `tcp_rtt`, `tcp_wait` in `send` and `recv`), the interrupts' (`tp::Irq`, where each architecture's dispatch takes one: x86's IDT stubs' C entries, `arm_irq_entry`'s loop, `ip_interrupt`, `GenericTimer::local_tick`; the record ended before the `preempt()` of `Cpu::ipi`, `Cpu::timer_tick` and `local_tick`; `Cpu::local_timer_late`), the soft IRQs' (`SoftIrq::raise_mask`'s stamps and records, `run_handler`), `kernel/trace_window.{h,cpp}` (what the readers make of the rings: a CPU's part of a window and from where it is whole, a window's field filters -- `Select`, `keeps`, `trace show ... field=value` -- the merge, the waits `trace latency` pairs, the sort), `kernel/trace_ring.{h,cpp}` (a CPU's ring, each slot a seqlock), `CpuTable::synchronize`, a tracepoint's site, `scripts/trace-decode.py` | the smoke boots, whose `test_tracepoints` (once, on the boot CPU) reads a ring of its own back in order and wrapped, has a writer on another CPU and the boot reading as it writes -- none torn -- and the kernel's rings through `emit()`, a hundred of the records Rust's (`rust_trace_emit_test`, through kcore's flags and the kernel's emit), frozen and freed through `CpuTable::synchronize`, and whose `rust_test` holds every Rust payload to the kernel's description of its event -- name, size, every field's name, offset and size, both ways, an enumeration's names value by value -- a panic on the first that differs; after touching the ffi side or `events.rs`, `sshd-test.py` (arm64: the digest every module carries); after touching the hypervisor's, `hv-fuzz.py` (its whole-machine runs build every record, half of them with the events on, each held to its description) and `hv-test.py` (x86-64), whose last phase runs the `exits` and `smp` guests with `trace on hv` and holds the records to what each guest did -- every port access, the CPUID and the HLT; the ICR writes, the rings they made, the timer's interrupt asked for and injected, every halt ended by a wake -- then hvboot's reads of a disk it is given with `trace on blk disk fs` -- each request through every hand-off in order, by the disk's own task -- and a file written, its ext4 commits -- and records nothing with the events off, each window also dumped and read back by `trace-decode.py --hv` and its Chrome export as shipped, held to what the guests did (exits by reason and port, the host's time after each, rings paired with entries, requests paired through their hand-offs); after touching the layers', `fs-fuzz.py` and `net-fuzz.py`, whose machine's `kernel_tp_record` holds every record to its description for half their inputs; `cpp-fuzz.py --target trace` (host, in CI): the ring against a model and a writer thread against a reader; `cpp-fuzz.py --target tracewin` (host, in CI): a schedule played out -- a quarter of its windows read through a field filter or two, each part held to the model's own reading of the records' bytes -- on up to six CPUs' rings -- wakeups onto other queues, deadlines late, switches, displacements, blocks, clears, rings that wrap -- and the window `trace latency` reads of them held to what the schedule did: each part the newest its ring holds that the filter keeps, nothing missing from where it says it is whole, the merge in time order, every wait paired one the schedule had and none left out; the heapsort on a quicksort's worst orders, the five longest, a duration's parse, a window read as a thread writes its ring (the quicksort it replaced is a panic at the 48th input, a window's start ignored fails the 44th); `trace-test.py` (arm64): the `trace` command end to end from a boot with `tracepoints=test`, and the scheduler's events from a `sched probe` -- each record as its description says and on the CPU it was recorded on, every name a task's and at most the field's 12 bytes, the idle ones `ps`'s, and the debug group's `sched_slice` in a phase of its own (`sched_migrate`, which needs the balancer to move a displaced task that a probe's pinned tasks never are, is listed and held to its description should it appear) -- then `trace latency` over a probe, the trigger armed at a second and still armed, re-armed at a microsecond and fired, the rings then provably frozen, `trace dump`'s window line and records parsed as JSON and the shipped `scripts/trace-decode.py` reading them; a group as a window's filter and a word naming nothing refused; every `sched_block`'s `ran_ns` the stretch since its task's switch in, to 50 us (`trace bench` holds the shell's task across a tick first: the formula it replaced is 30 ms off); no task switched to before its `sched_new`, no `sched_exit` after a later task's `sched_new` under its pid; no switch inside its CPU's idle; `trace bench` writing no CPU's ring; frozen rings said by `trace on` and refused by `trace test`, and `trace free` leaving them thawed; then the interrupts' and soft IRQs' events over a probe and the shell's own datagrams -- per CPU each exit after its entry and none inside another, no exit's `ns` past the distance from its entry, every CPU's timer and its lateness, a device's interrupt, each receive run's wait reaching back to the raise that made it pending -- and `trace-decode.py --irq` reading them, whose `--selftest` holds the split of a wait into its CPU's interrupts and softirq runs to a made-up window. A site's cost when off is a byte's load and a branch: measure a hot path's before and after (`trace bench`, `sched`'s decision cost -- interleaved boots of the build before and after, since two boots of one build move more than the sites cost -- and the instructions a site adds to a path the decision cost does not cover, `switch_to`'s) rather than assume it |
| `drivers/net/r8168`, `drivers/net/r8125` | nothing emulates them: the compiler, then the EX44, whose only console is that NIC -- **keep their register sequences as they are** |

### `wx-test.sh` -- W^X

One boot per probe (`wxprobe=text`, `wxprobe=heap`), each of which has to die
on the fault it asked for, at the address it printed. A kernel that survives
a probe prints `SUCCEEDED (W^X broken!)` and the script fails. No smoke boot
notices a mapping that stops being NX -- everything still works -- and the
heap probe is the one that fails if a mapping path ever stops setting NX on
the leaves it writes. `--skip-build`; `WX_HVF=1` for the arm64 boots under
HVF. See [Paging](paging.md#mmio-cacheability-and-wx).

### `panic-test.sh` -- a panic that resets the machine, when asked

Two boots that panic on purpose (`wxprobe=text`, early in boot), under QEMU's
`-no-reboot`, so that a reset is QEMU exiting by itself: with `panic=1` it has
to reset within twenty seconds of the report, having said it would; without,
it has to still be there eight seconds after it, the report on its console. A
server whose only console is the network has nothing else to bring it back
from a panic -- the EX44 stayed dark until it was reset by hand from the
provider's panel after a boot that died before its network was up -- and
the system image boots nos with `panic=10`, after which GRUB boots the
server's own system. `--skip-build`; `PANIC_HVF=1` for the arm64 boots under HVF.

And a third boot, with the netconsole on: the panic a running machine has
from a stack that is no task's, typed at the UDP shell once the collector
has heard from the machine. On x86-64 a real double fault (`panic df`): a
stack pointer nothing can use, so the report runs on the IST stack, and the
stack it came from cannot be read; on arm64, whose exceptions have no stack
of their own, a panic run on a static one (`panic offstack`). The report has
to reach the collector -- the x86 one with the other CPU's stack in it, and
saying the stack it came from could not be read -- and `panic=1` has to
reset the machine. Until the lock path stopped asking which task held a lock
once a panic had begun (`preempt_current_task`), such a report stopped at its
first lock, a panic inside the panic that only spins -- with the netconsole
on, before it had printed a line -- and nothing reset the machine; a double
fault's would have stopped again at the walk of the stack it came from, and
overrun the 4 KiB it was given, where the report with the netconsole sending
it takes 5688 bytes. Put back one at a time, the lock path's check and the
walk's fault are each found by this boot: no panic printed at all, and a
machine still there twenty seconds after the report.

### `parttest.py` -- partition tables

Boots with an MBR disk and a GPT disk it writes itself -- no partitioning
tool, no privileges -- and checks what the kernel made of them: the
partitions registered and their sizes, both tables as `partitions` prints
them, that a partition's sector 0 is its first sector on the disk and that a
read past its end is refused, that a filesystem mounted on a partition
keeps writers off the disk it is on, and that `disks` gives every partition
the id a VM's vm.toml names it by: a GPT partition its unique GUID as Linux
writes a PARTUUID, an MBR one its disk signature and number. The smoke
disks carry no partition table at all, so nothing else reaches this code.

### `ext2-test.py` -- ext2, judged by `e2fsck`

Works the ext2 root filesystem from the shell -- files made, grown past
their direct blocks, copied, renamed, removed, a directory removed whole --
and then lets `e2fsck -fn` judge the image. The boot-time `fstest=on` checks
that the driver reads back what it wrote; this checks the other half, that
what is left on the disk is still a filesystem a checker calls clean.

`find` walks a tree made for it on the root, and each expression is held to
the paths it should pick out -- by name, case or not, by a class and by
path, by type, size and emptiness, by depth, with `!`, `-a`, `-o` and
parentheses -- a directory printed before what is in it; into the procfs
at `/proc`, and kept out of it by `-xdev`; and a path that is not there, a
test it does not know and a `(` left open said.

And files past 4 GiB. The image starts without `large_file`, which nos has
to set itself; a file is grown by `truncate` to 5 GiB and a line appended at
its end, another grown past 6 GiB and cut back to 1 MiB, and `fstest` runs
its far file on the root. Then, besides e2fsck, debugfs reads them off the
image: `large_file` set, the far file's size, its last block under the
triple indirect block (`TIND`) and the line nos wrote there where `bmap`
says it is, and the file cut back holding no block at all.

It ends with a `poweroff`, which has to unmount the root, switch the machine
off and get there with nothing panicked. Until `7908bf6` the arm64 halt
faulted in the static destructors -- an exiting CPU left its one WFI -- and
this test and `sshd-test.py` -- and nanofs's, while there was a nanofs --
stopped reading the log at `Stopping cpu`; now these, with
`netconsole-test.py`, are what judges the arm64 halt. Once they
looked, they found it faulting still, in two runs of three: a parked CPU's
halt was a call, whose frame record every wakeup from WFI wrote onto the
stack the CPU had been interrupted on -- one the boot CPU frees. A parked CPU
stands on a stack of its own now, and halts in a loop that touches no memory
(`Cpu::park`).

### `disklog-test.py` -- the kernel log on a disk

Boots with `disklog=on` over an area it prepares itself, then reads the area
back with `scripts/disklog.py`: the boot's first fifty traced lines compared
with the serial console's, line for line. It checks both halves of the
channel -- that the kernel finds the area, claims the device (so a mount of
that disk and a raw write to it are refused) and writes whole sectors with no
failures, and that the host tool parses what this boot left. Nothing else in
the suite gives `disklog=on`, and without the parameter no disk is so much
as read.

### `netconsole-test.py` -- the kernel log over UDP

Boots with the log streaming to a collector the script runs, and checks what
someone debugging a dead machine depends on: that the log queued before the
link came up arrives once it does, that the sequence numbers are contiguous
-- so that a gap can be told from a machine that stopped -- that lines
produced after link-up keep coming, and that a panic's report gets out with
its backtrace, which is the one time it matters most and the one time the
drain task is not running to send it. Then a second boot is powered off
from the shell over UDP, and the halt's own lines must reach the collector
with no gap in the numbers: the unmount, which comes after every module has
gone -- a module's exit may wait minutes for guests to turn themselves off,
which on a machine with no other console nothing else would show -- and the
netconsole's own last line, traced as it stops and sent by the flush its
drain does on the way out. On the two Hetzner machines netconsole is the
only console there is. See [Netconsole](netconsole.md).

### `tcp-test.py` -- the connecting side of TCP

Fetches from an HTTP server the script runs: a 400 KB body checked by
SHA-256 (enough segments for the window to move), a connection refused, one
to a black hole that has to time out, and twelve in a row to reuse ephemeral
ports through TIME-WAIT. The last check is the one that catches a slot leak:
after all of it the connection table has to be back where it started,
because a leak of one slot per connection is invisible until the
sixty-fourth.

### `api-test.py` -- the host API, end to end

The host API is the one module everything managing the machine goes through,
and what it must not do is the kind of thing that looks like success: let a
stranger's bytes reach its parser, answer a reader's action, leave a slot in
TIME-WAIT for every client it served, hold an unload for a handshake. nos
boots with api.ko, modtest.ko and sshd.ko on its root, an api.toml that
listens on eth0, and two clients' keys -- an admin's and a reader's, made by
`nosctl keygen` -- and a stranger's in no file. Through nosctl's own client
it checks:

- the host's key is the one `api start` and `api cert` print, and the one
  the connection shows; a host with another is refused before a request;
- a stranger, and a client with no certificate, are refused in the TLS
  handshake, counted so -- and the count of requests parsed does not move;
- a reader calls reads and is refused (403) an action, a job and a cancel,
  and is shown no method but reads; an admin calls everything, and each
  change is a line of the audit log;
- a job runs to its end through the API, its log and result read back;
  another is cancelled; a job called as a call is a 409;
- a descriptor's ETag and its 304; a 404, a 405 with `Allow`, a 400, a 413;
- the stream's metrics, and an action's audit line in its log channel;
- a fifth connection of one key let go after its handshake, a ninth reset at
  the door, a third handshake reset while two are going -- and the two let
  go at their time -- a connection let go after its idle time;
- sshd answering while the API holds all eight;
- a client taken off the list refused its next request and its next
  handshake, and let in again once put back;
- no slot of the API's port in TIME-WAIT, the clients having closed first;
- rmmod api with a stream, a connection and a handshake under way unloads at
  once and leaves no slot of the port;
- and it prints what a handshake costs and how deep a connection's task went.

`--arch x86_64` is its short form: nos.iso, a root whose /etc/rc loads the
modules and starts the API, and what needs no shell -- every check above
but the shell's own commands, sshd, the client list's changes and the unload.
And on x86-64, where the guests are, hv.ko and hvboot too, and the `vm`
provider through the API: a VM of hvboot -- which, given no disk, turns its
guest off -- started as a job and found stopped, its console read by a
reader and streamed to one, the reader's typing refused and an admin's at a
VM that does not run refused saying why; a VM made of a raw image the test
serves, its download in its job's log, its disks the image and the seed;
cloned, the clone's seed made anew and its disk no more written than it
holds, and deleted; a protected VM's delete refused without `force`; and a
create of an image served slowly -- 64 MiB, a chunk at a time -- cancelled
while it downloads, ending `cancelled` with nothing of it left.

And the management panel: /etc/rc's `api panel create`, given a password's
hash in `panel.pass`, the test's raw image, a stand-in for a bundle and its
SHA-256, has the vm provider make the panel's VM and start it as one job --
the highest port of the switch its own, its address, nos's 39233 forwarded
to its 443 and `protected` in its vm.toml, a delete without `force` refused
-- lets its key in as `panel`, an admin, and its address alone in on hv0, as
`api.status` says. Then nos is powered off through the API, and its root
read with debugfs: the VM's user-data is cloud-config (which PyYAML reads,
where it is there) whose seed names the API on hv0 and the host's key and
holds the password's hash; its client certificate's key is the one let in;
its certificate as a server is the one the host keeps in `panel.pem`; both
pairs are taken by OpenSSL as they are, each key its certificate's, in a TLS
1.3 handshake whose server requires the client's certificate; the bundle is
fetched by its URL and held to its SHA-256; and api.toml has the address in
`allow_hv0` with every line it had kept. Booted again from that root, /etc/rc's
`api panel create` must make nothing and start the panel's VM -- a `vm.start`
job that ends ok -- as it does at every boot after the first. Needs openssl
and QEMU; a few minutes on arm64 under HVF.

### `panel-appliance-test.py` -- the panel's bundle, run as its VM runs it

`nos-panel-<version>.tar.gz` is what the panel's VM downloads and runs, and
nothing else runs it: a release that built it wrong -- a Node without
`node:sqlite`, a Caddyfile Caddy refuses, a standalone build missing a file
-- would be a panel that never comes up, on a machine whose operator has
nothing but a browser's error. The test unpacks it in Debian 13's image (x86-64,
under emulation on an arm64 Mac) where the VM has it, `/opt/nos-panel`, and
runs it as its units do: the bundle's Node runs the panel with
`nos-panel.service`'s environment and a seed as `api panel create` makes
one, and its Caddy serves it with the bundle's Caddyfile and a P-256
certificate for `nos-panel`; the mock host API (`panel/mock`) stands in for
nos, run by the bundle's Node too. Then, from outside, as a browser through
nos's relay: TLS by the address alone, with no server name, answered with
the panel's certificate and HTTP/2 taken; a wrong password refused, and a
login from no page of the panel's; root's login with the password the
seed's hash is of, its session in a `__Host-` cookie, Secure and HttpOnly;
the host the seed names, its key pinned as the seed has it, reached --
the panel's stream of its numbers up -- and a call through the panel
answered by the host API over mTLS; nothing of the hosts without a session.
`install.sh` is read by `sh -n`; cloud-init and systemd are the VM's alone.
Needs Docker and openssl; a minute, more under emulation.

### `sshd-test.py` -- the listening side, and modules

End to end against OpenSSH's own client (see [sshd](sshd.md)). On arm64 it
loads `sshd.ko` over the UDP shell and has `ssh` work it: commands, a shell
with a terminal and one without, Up and ^R through a terminal's history,
3 MiB of output through the client's window
with rekeys in the middle, a command that prints nothing for five seconds
while the client asks every second whether the server is there (`ssh -o
ServerAliveInterval=1 -o ServerAliveCountMax=2 ... top 5000` -- a session
deaf while its command ran was cut off at three), a key it has to refuse,
sessions at once, a burst
of connections past the four that may be logging in, a stop and an `rmmod`
from inside a session -- and first an `insmod` given a parameter, which a
module that takes none must refuse, saying so; then it puts the module in `/etc/rc`, reboots, and
checks it came back by itself with the same host key, and that `poweroff`
unloads it before the first filesystem is unmounted and then switches the
machine off with nothing panicked on the way. Between the two, `edit.ko`:
`ssh -t` run on a pty of the test's own, of a size it sets -- the status bar
must be drawn on the row before the last of that size, the terminal the
session was told reaching the command -- then typed at and saved, the file
then written by somebody else and saved over after the question, the pty
resized and the screen drawn again whole at the new size, `^Q`, the file
read back; then the connection killed under a change, which must turn up in
`<file>.save` with the file as it was saved; and `edit` with no terminal
must say it needs one. On x86-64 it boots an
ext4 root that already carries the module, a key and an `/etc/rc`, and logs
in. It is also what notices a change to the `ffi` declarations that a module
was not rebuilt against: a module's header carries a digest of them. Needs
`ssh`, `ssh-keygen` and host ports 2222, 8000 and 9000 free.

59 checks pass, 14 of them the editor's. `poweroff unloads sshd before the unmount` used to fail: from
the VFS's move to Rust (`edae1ef`) it waited for `unmounting / (ext2)`, a line
only the C++ VFS printed, where the Rust one prints `vfs: unmounting /`. One
check still fails now and then under load, `connections past
the four logging in are refused` -- stragglers
of the 70-probe burst reach the server after the three quiet seconds the
test waits for, and are counted among the refusals it looks for (18 where it
wants 2). A run that fails anything else is a regression.

### `image-test.py` -- the system image, poured and booted as on a server

What a release's `nos-x86_64.squashfs` has to do on a server
([Install](install.md)), which nothing else here tries. The disk is an MBR
with two partitions: a "host" one holding GRUB as a server's `/boot` would,
whose first entry stands in for the server's own system and says so on the
serial line, and a blank one for nos; it is attached as virtio-blk. The
Hetzner boxes have NVMe, which nos reads as well, but the SeaBIOS of Ubuntu
22.04's QEMU (1.15), which CI runs, boots nothing from an NVMe disk: it says
"Booting from Hard Disk...", GRUB's core image is never read, and nothing
reaches the serial line -- the test's first boot waited out its time in CI
on every run until the disk was moved, while a Mac's newer SeaBIOS booted
it. The test mounts the image and runs the `nos-install` in it as
a server would -- under Linux, as root, on a block device, the partitions as
loop devices: under `sudo` on Linux and in CI, in the nos-builder image with
`--privileged` on a Mac -- with `--ssh-key` and `--boot-once`, then builds
the host's `grub.cfg` the way `grub-mkconfig` would, the machine's entry
first and what `/etc/grub.d/42_nos` prints after it. Every file of the
image must be on the partition then as it is in the image, `nos-install`
aside -- compared file by file against the image, not against
`nos-install`'s own list of what goes where, which is what left
`/lib/hvboot` out of v0.0.26's installs -- and before that, a copy of the
image with a file added must be refused by the file's name with the
partition left blank: an image holding something `nos-install` does not
know where to put is not poured without it.

The first boot must be nos, once -- the entry from the image's `nos.cfg`,
the root mounted by UUID read-write -- with sshd started by the image's
`/etc/rc`, a host key made and the given key let in, the kernel and the root's
`/VERSION` both the image's, no `udpshell` on the command line and no UDP
shell answering. The install is given a made-up machine's `/proc/cpuinfo`
and firmware directory (`NOS_CPUINFO`, `NOS_UCODE_DIR`) holding a file of
one microcode update for its CPU: it has to say it put that file at
`/boot/ucode`, GRUB has to hand it to the kernel as the module `ucode`, and
the kernel has to find it and say why it loads nothing under QEMU, whose
CPU is a hypervisor's. The second boot must be the host's own entry: GRUB
cleared the flag. Then `nos-install update`, finding the partition through the GRUB
entry, over what nos wrote: e2fsck finds the partition clean, the host key
nos made stays, every file of the image is there again, the old kernel,
modules, hvboot and `/VERSION` go to `.prev`, the UDP shell it
is asked for goes into the kernel parameters and a boot is armed; the third
boot must answer with the same host key and, now, on the UDP shell. The
update is given `--panel`: root's password for the panel printed once, the
partition's `/etc/api/panel.pass` the scrypt hash of it alone -- checked
against the password printed -- and `/etc/rc` given the panel's lines in
order after the image's own; the third boot's api.ko serves, and its `api
panel create` -- of an image built without a bundle, as CI's is -- makes no
panel and says why. Last, `status`, `rollback` and `uninstall`, and e2fsck
once more. Needs
`qemu-system-x86_64`, `ssh`, `ssh-keygen`, `ssh-keyscan`, and the image
built (`make image`, `scripts/build-image.sh`); about four minutes under TCG.

### `netblk-test.py` -- a module on the block layer's asynchronous path

Loads `netblk.ko`, serves an NVMe disk -- and a partition of a second one --
on a UDP port, and works it from outside with `scripts/netblk.py`: geometry,
data written and read back, requests it has to refuse, a load test each way,
what the kernel's own reads find on the disk afterwards, and a stop and an
`rmmod` that leave nothing behind. What is written at the start of the
partition has to land at the partition's start on the disk, not at the
disk's. It is the only test of `submit`/`kick`, of frames kept across calls
for the disk to DMA into, and of `kcore`'s module-facing `block` and `net`.
x86-64 runs under KVM unless `--tcg`; `--arch aarch64` is the one that runs
on a Mac. See [netblk](netblk.md#using-it).

### `ipc-test.py` -- the channel between hosts, three kernels on a LAN

Three nos, `a`, `b` and `c`, each with ipcnet.ko and a key the test made
with openssl -- on x86-64 booted from an ISO whose command line has the UDP
shell, `make nos-test.iso TEST_ISO_ARGS=udpshell=9100` -- on the LAN of
`scripts/hub.py`: a learning switch over QEMU's
`-netdev dgram`, which answers each VM's DHCP with 10.9.0.<n>, relays the
test's UDP shell to it from 10.9.0.253 -- past the impairments, which are
the VMs' between themselves -- and drops, delays, jitters and cuts between
any two. `a` and `b` pin each other; `a` pins `c` by a key `c` does not have,
and `c` holds `b`'s key, pinning `a`; `b` pins `a` alone. kernel.echo runs
on each, a `Cluster` name, and `ipc ping` is what goes between them:

- both up, said in the log; 200 small pings each way, eight in flight, and
  sixteen of a megabyte by the bulk connection -- every answer the bytes
  sent, in the order sent; the links' 20 ms floor in `tcpstat`, at the
  end that dialed and at the end that accepted (the accepting end's once
  kept the 200 every connection starts with, and waited out each of its
  losses ten times as long);
- a name nobody has there bounced back (`Gone`), and messages past the
  echo's mailbox (`Full`), every other one answered whole;
- under 5% loss and a jittered delay, every message and every megabyte
  answered whole and in order, the retransmissions counted;
- the link cut: both hosts lose each other within 3 s and a margin, and a
  ping running across it ends with its monitor's Down; healed, up and
  talking again;
- `c`, whose key is not the one pinned for it, never up -- its handshake
  refused by `a`, which dials it -- and its SYNs to `b`'s port, where it is a
  stranger, dropped and counted with no slot taken;
- `rmmod ipcnet` on `a` under `b`'s traffic: prompt, `b`'s ping ending with
  its Down, `b` having `a` down, `a` with no router; loaded again, up and
  talking.

What it does not reach yet: three hosts' traffic at once, a peer at its
limits slowed while another goes on, and the measurements on the boxes --
here a round trip of a small message is some 0.4 ms at the median, through
a switch in Python.

### `cluster-test.py` -- the cluster's registry, three kernels on a LAN

Three nos on `scripts/hub.py`'s LAN as `ipc-test.py` runs them -- on x86-64
from `nos-test.iso` with the UDP shell -- but with no key made by the test
and no peer pinned by hand: each root has ipcnet.ko and cluster.ko and a
hosts.toml naming that host alone, /etc/rc loads both and starts ipcnet,
which makes the host's key, and everything else is the registry's
(docs/cluster.md):

- each serving with no peers; `cluster init` on `a`, which leads its one
  voter, and a second `init` refused;
- `b` invited from `a` with the key `ipcnet id` says on `b` -- the token
  refused by `c`, whose name is not the invitation's -- joined, a voter, and
  `a` and `b` up to each other; the token used again refused;
- `c` invited from `b`, which does not lead and hands the invitation to
  `a`; a join with one byte of the secret changed refused by `b`; `c`, now
  `b`'s peer as a host invited -- `b`'s `ipcnet` saying so -- its pings to
  `b`'s echo never answered, each refused by `b`; then the token taken:
  three voters on every host, every host up to every other, and `c`'s pings
  to `a`'s echo answered over the connection the registry made;
- the `cluster` provider called through the bus (`mgmt call cluster
  hosts`), answering the three hosts;
- `b`'s maintenance on and off, and `c`'s public address set and taken
  away, as every host's `cluster hosts` says them; `c`'s ipc address never
  taken away;
- `c` moved to a new key (`cluster rekey`) while `b` pings `c`'s echo 6000
  times across it: every ping answered as sent, and `a`'s and `b`'s counts
  of `c`'s connections up unmoved -- none lost; `c`'s new key the registry's
  on every host and the old one gone; `c`'s connections made again with the
  new key alone (`ipcnet stop`, `start`); the old key refused when invited
  again;
- all three VMs killed and booted again at once: back from their folders,
  a leader agreed on, three voters, every host up to every other;
- `cluster recover` on `a` refused while the majority answers; `b` and `c`
  killed for good, `a` following no leader once CheckQuorum has it step
  down; the plan said first, a name not the cluster's refused, then `a` the
  one voter and `b` and `c` removed -- `a` leading alone; `c`'s key refused,
  and `c` installed anew joined as node 4 with a new key, two voters;
- the leader of those two removed: it hands the lead to the other first,
  as its log says -- the removal drops its connections as each member
  applies it, and the other could otherwise never be elected -- which leads
  alone after; the removed host's key refused when invited again;
- nothing panicked.


### `netload-test.py` -- the receive path, the frame pool, a module on both

Loads the [`netload` module](modules.md#netload) and hammers its UDP target
from the host. The target answers from inside the receive path: the reply is the frame that arrived, kept past the
callback, its addresses swapped where they lie, and the replies of a batch
go to the NIC together when the batch ends. No other test touches any of
that, and all of it fails quietly -- a frame kept and never released is a
pool that runs dry an hour into a load test, and a batch never handed over
is an echo server that answers nothing and reports no error. So: every echo
is byte for byte a datagram that was sent, and none twice; a burst longer
than the reply batch still comes back; the counters agree with what was
sent; sink mode answers nothing and counts everything; more start/stop
rounds than a device has listener slots; and the frame pool ends where it
began. That last one is also what notices a frame pool that was never built
-- which is how a boot that passed every other gate once spent two hours
taking each frame from the allocator.

Then it turns the module round and checks its source, against a socket of
the script's own. A paced run has to arrive whole: every datagram the size
asked for, marked with its sender and a sequence number, none twice, the
filler intact, the kernel's count of what it sent the same as what came; and
what the script sends back has to be counted as echoes and not answered. A
run with no pace has to finish and account for every datagram as sent or
failed. An address nothing answers ARP for has to be refused with nothing
sent, and so do bad arguments. And while a run with no pace floods, the
shell is asked ten things and has to answer ten: a source that did not leave
room in the transmit queue keeps it full, every answer of the shell's is
released for want of room, and on a machine whose only console is the
network the load test has silenced the console -- which the first version of
the source did, and this check is there because of it.

Last, the module's life: `rmmod` with a target running has to take it down
and give the port back, a second `insmod` has to start on that same port,
and the frame pool has to end where it began. netload is the one module on
the receive path, so this is what tests the typed listener a module is
given -- a handler the listener owns, frames lent, the end of a batch told
(`kcore::net`) -- and the C ABI under it.

And the tick's receive poll, which nothing but `rxpoll=on` turns on. When
that stopped being true nothing showed: the switch lived in the C++ function
the tick called, went with `src/base/net`, and from then on every tick
polled. Under a flood the receive softirq ran on two CPUs in turn -- the
BSP, whose tick it is, and the one the NIC interrupts -- with an IPI at
each handover, and on the AX41 took 70% of each of two CPUs where it had
taken one, while every rate held. So at the end of all the above, booted
without the switch, `net` has to say the tick never polled; and a second,
short boot with `rxpoll=on` has to show it polling, since a switch that no
longer turns the poll on fails as quietly. The switch is in common code,
`DeviceTable::poll_rx`, so the arm64 boots cover both architectures.

It takes about seven minutes: 88 round trips with a three-second collection
window each, by design, and the second boot.

`--arch x86_64` is the short form, a minute of it: `nos.iso`, a root that
carries the module and an `/etc/rc` that loads it, starts the target and
runs the source once -- no shell at all, the x86-64 boot having none over
UDP. It checks what differs between the architectures rather than what does
not: that the module loads and binds there, that its calls across the C ABI
come back right where the ABI is the other one -- two of them return a
structure by value -- and that the target echoes and the source's run
arrives whole. Both forms take `--nic igb`.

### `idle-wait-test.py` -- a wait that costs a tick

Boots with more CPUs than the kernel has busy tasks and runs `blkload` at
`qd=1`: one worker, one I/O in flight, every other CPU with nothing to run.
Asserts the read comes back in well under a tick.

It is here because of what the wait primitives used to do. A task waiting for
another CPU's interrupt -- a synchronous block read, a mutex, the TLB
shootdown's acks -- polls, and polling with `schedule()` lets the scheduler
hand the CPU to the idle task, which halts it. What such a task waits for is a
counter or a store in an interrupt handler: no waiter to unblock, no IPI to
send, so nothing brings the CPU back before its own next tick. The work was
done microseconds in and the wait cost 10 ms, every time.

Every other gate is blind to it, which is the point of having this one. The
precondition is a CPU with *nothing else runnable*, and a smoke boot has more
polling tasks -- shell, udpsh, dhcp, netconsole, usb -- than CPUs, so
`schedule()` never reaches the idle task and every latency looks right. The
same unfixed kernel reads at 45k IOPS at `-smp 4` and at 98 IOPS at `-smp 16`.
It was found on a 12-CPU Hetzner AX41 and nowhere else, after two earlier
sightings of the same shape (`a171a86`, `ce3087c`) left the primitives
themselves unconverted.

There is nothing to tune: the bug puts `p50` at exactly one tick (10223 us at
100 Hz), the fix puts it at ~22 us under KVM. Needs `/dev/kvm` and a host with
at least 12 CPUs, and refuses to report a pass without them -- a gate that
cannot fail is worse than no gate.

### `usb-test.py` -- typing at the kernel

On the Dell Latitude -- UEFI, no serial port, no PS/2 -- the xHCI controller
and the HID boot keyboard on it are the only way in, and no smoke boot
attaches a USB controller at all. The gate boots with an emulated xHCI
controller, two keyboards -- one on a root port, one behind a hub -- and a
device that is neither, then **types at the kernel through QEMU's monitor**
and reads the shell's answer off the serial console. It is built to catch
the one failure that looks like success: a driver that enumerates perfectly
and delivers no report leaves every boot-log check passing, and fails on "a
command typed on the usb keyboard reaches the shell". Then the line editor's
keys, which only a keyboard's decoding makes what a terminal sends: Up twice
must run the command before the last, and ^R and two letters one found by
what it holds -- a keyboard that dropped the arrows runs an empty line, and
one that took Ctrl for nothing types an `r`.

### `insn-test.py` -- the MMIO decoder, against the assembler

The hypervisor performs a guest's MMIO access by decoding the instruction
that faulted ([MMIO](hypervisor.md#mmio)), and what it decodes is the
guest's own bytes. A wrong decode looks like success -- a value in the wrong
register, RIP stepped into the middle of the next instruction -- so the
decoder is checked against the assembler, on the host, where the file
compiles as it is: every form of `mov` Linux's MMIO accessors are -- to and
from registers of each width, REX's and the high bytes, immediates of each
size, `movzx` and `movsx` -- over sixteen addressing forms, assembled by
clang and read back by an LLVM objdump, each decoded to the access and the
length it has; the instructions that must be refused (no memory operand,
another opcode, a string move, a lock, `movabs`); each instruction cut short
at every length, which must be refused rather than read as a shorter one;
and two million random byte strings, which must decode without a panic and
never to a length past what was there. The guest page walker the
instruction is fetched by is checked beside it, against page tables built by
hand: 4 KiB, 2 MiB and 1 GiB pages, five levels, addresses non-canonical and
unmapped, and a fetch across a page boundary into a page mapped elsewhere.
It needs clang, `llvm-objdump` and cargo; CI runs it on its x86-64 leg.

The other decoder in the file, of a `mov` to or from a control register
(`decode_mov_cr`) -- what an intercept of CR4 stops a guest at, on a CPU
that does not name the register -- is checked the same way: every `mov` to
and from CR0, CR2, CR3, CR4 and CR8 through eight registers, REX's among
them, assembled by clang and decoded in 64-bit code and, without a REX --
which 32-bit code reads as an INC or a DEC, and is refused -- in 32-bit
code too; each cut short at every length; by hand, what no assembler gives
and a CPU takes or refuses -- prefixes it passes over, a ModRM whose mod
field it ignores with nothing after it, a lock and a repeat, a REX that is
not the last byte before the opcode; and the random bytes. And the third,
of a `mov` to or from a debug register (`decode_mov_dr`) -- what an
intercept of DR0-DR3 stops a guest at -- the same way over DR0-DR3, DR6 and
DR7, with REX.R's DR8 and up by hand, which a CPU raises #UD for before any
intercept and which `crate::dr` then finds is not the exit's register. Each
decoder must refuse the others' every form. A control run with REX.B left
out of the register failed 31 of its cases; one with the debug registers'
two opcodes swapped, 164.

### `hv-fuzz.py` -- everything a guest reaches, fuzzed

A guest decides every value the hypervisor's devices are handed -- every
port it writes and how wide, every MSR, every byte of its virtio rings and
MSI-X tables, the instruction behind an MMIO fault, when it halts and with
what masked -- so a panic, an overflow or a loop that never ends anywhere in
them is one a guest can cause, on the host, under everyone else's guests.
`fuzz/hv` is a host program built from the hypervisor's own sources
as they are -- `hypervisor/hv/src/devices/*`, `lapic`, `acpi`, `insn`, `walk`,
`linux`, `mmio`, `policy`, `smp` and `run`, hvarch's VMCB layout, the
module's switch and DHCP server (`modules/hypervisor/hv/src/{net,dhcp}.rs`) and NAT
(`layers/net/src/nat.rs`) -- over stand-ins for the kernel (a clock the fuzzer
moves, the locks, a vCPU's wait), for its network layer (the net devices,
their frames, ARP, `hv0`'s end of the switch) and for the CPU, with
overflow checks on as a `RUSTUB=1` kernel has them. Each target turns random bytes into what a guest does to
one device: the serial port, the 8259, the PIT, the RTC, ACPI's fixed
hardware, the panic device (`pvpanic`: every write held to the event it is),
PCI configuration space, the local APIC through its MSRs and its
page, the IO-APIC, MSIs, a virtio disk and NIC driven through rings well
made and not -- and raced: another of the guest's CPUs topping a ring up
by plain writes while the device takes from it, as nothing on the host can
stop it doing; the NIC with its receive interrupts held apart, the clock
moved on between rounds, and held after each to owing none it should have
raised (`rx_due`); its transmit offloads (`offload`), every send -- TCP over
IPv4 to be cut, a frame whose checksum is left to the device, and noise,
each well made or not -- held frame by frame to a model of its own written
from virtio's and the RFCs' words, and on their own terms: no frame over
1514 bytes, every checksum right, the payloads in order adding up to the
send's; what it puts into the buffers its driver posts (`netrx`) -- a frame
into one chain, or spread over mergeable buffers with their count in its
header, a send carried whole with the header that says how to cut it, a
checksum left to finish with the header that says where, or finished first
for a driver that takes none so -- each used element held to the model's
chain, length and bytes -- the notifications between the disk and the NIC
and a driver (`virtq`): Linux's, written from `virtio_ring.c`,
`virtio_blk.c` and `virtio_net.c` -- the kick decided by the device's word
with EVENT_IDX and by its flag without, callbacks turned off, on and put
off to three quarters of what is out, NAPI polls to a budget that refill
the ring or put the refill off, a transmit queue stopped when full until
an interrupt says there is room, and now and then a chain made available
by another CPU after the device's next few looks at the ring, that CPU's
kick decided by the word it read then; a NIC of up to four queue pairs,
the number in use set through the control queue -- each answer held to
the protocol's, and no frame on a receive queue past those in use -- TCP
frames of a few flows for its steering, and the driver now and then gone
and a new one come, the rings zeroed, the first pair alone the device's
again until it says more -- with the device given no round
but those the run loop is sure to make (a notify, the disk's backend
serving, a frame whose wake is not held, `rx_due`, `more`); and held, once
nothing more happens, to every request served and heard of, every frame
sent and every frame received: a kick the device needed and never asked
for, or an interrupt the driver asked for and never got, is a guest's disk
or network stopped for good. Ten mutants of EVENT_IDX -- an ask left out on
each queue, the ask at the next entry rather than the index seen, no look
after it, the interrupt decided against the wrong index or the first one
withheld, the driver's word or flag ignored -- each failed within its first
1,500 inputs; and nine of the queue pairs' -- no delivery when the pairs in
use change, frames steered past them, a count past those offered or with
no multiqueue answered OK, the control queue looked for in the wrong place,
the interrupts or what is owed one only the first pair's, the pairs in use
kept across a reset -- each within its first 4,807. The page walker over random page tables, the decoder, the
Linux loader over headers that parse, the ACPI tables for any machine, the
SMBIOS tables (`smbios`) from any name, version and UUID -- read back as
Linux's `dmi_scan` reads them, the entry point where it scans, every string
there and printable, the UUID and the serial as the model says -- and the
DHCP server over the frames a guest's client sends, well formed and not.
`config` holds a VM's `vm.toml` (`modules/hypervisor/hv/src/config.rs`): whatever the
text, the parser does not panic, and what it reads `render` writes back so
that it reads the same again; a configuration made up the input's way --
its strings full of quotes, backslashes, control characters and characters
past ASCII -- comes back from its own rendering whole, and that rendering
cut, spliced and scribbled on is still read without a panic. A control run
with backslashes left unescaped was found at the 4th input.
`relay` holds `hv forward`'s relay (`modules/hypervisor/hv/src/relay.rs`) between two
sides a script drives (`fuzz/hv/src/relaymodel.rs`): what each sends and
when, a window shut for a while and open again, a side that ends or goes, a
clock that jumps, a stop. Every byte one side sent reaches the other in
order, once, and nothing else does; a side is read from only while the
relay holds none of its bytes, and then every round -- so a direction whose
receiver has no room never holds the other back; an end ends the relay at
once, a stop within a round; and a direction that has moved nothing for the
stall time ends it the moment it is offered and taken nothing, never
before. Eleven mutants -- the `send_all` it had before (a send waited on
until there is room, which kept a relay to a client that stopped reading,
and every join of it, for good), the stall check gone, early or late,
progress not timed, a read with bytes held, the stop, an end or a gone
side ignored, a partial send taken whole, one direction drained before the
other -- each failed within the first 40 inputs.
`place` holds where guests are put on the host's cores (`hypervisor/hv/src/place.rs`)
to a model of its own (`fuzz/hv/src/placemodel.rs`), over hosts made up an
input -- one, two or four threads a core, caches shared by one to sixteen
cores, CPUs missing from the numbering and the extension off on some -- on
which guests are started and stopped, and the rule changed between, as the
input says: a guest refused only where no place exists, and for the reason
that comes first; each vCPU on a CPU of its own the extension is on for,
the first where `cpu=` said; and, its cores its own, none of them with
another guest's CPU on it, every one of them but one filled before the next
is taken, and its disks' task on a spare thread of its own cores, then on a
core no guest has, then beside a vCPU of its own. Five mutants of the rule
-- later cores not checked, one vCPU a core, the disks' task on a held core,
`cpu=`'s core not checked, the disks' task elsewhere before a spare thread
of its own -- each failed within two inputs. And what the guest is told of
where its CPUs sit (`Shape`): threads of a core in twos exactly where each
host core it has holds its CPUs 2k and 2k+1, or its last alone; one cache
exactly where all of them are on the first's -- the model groups them by
core, where the code compares them two by two; a cache always shared, and
pairs not checked both ways, each failed it.
`policy` holds the CPUID and MSR policy to a model of its own
(`fuzz/hv/src/policymodel.rs`), over a host CPU made up an input -- which
vendor's, the tops of its two ranges of leaves, below the policy's and
above, and every bit of every leaf: every leaf a guest can ask answered with
no more of the host's than the model lets through, and nothing for one it
does not name or past the top the guest is told; every MSR the model names
read and written as it says -- a refused write leaving the guest's state as
it was, a taken one read back -- and every other refused both ways; and a
table, from the manuals, of what each CPUID bit promises the guest that sees
it, the MSR it will use or the bit of EFER it will set, which must then be
answered. IBPB offered with PRED_CMD refused, or AutoIBRS with its EFER bit,
is a finding at the first input. The hypervisor's range is held to KVM's
ABI: the signature, NOP_IO_DELAY and with APICs PV_EOI, nothing past
0x40000001. The platform target's scripted guest turns PV EOI on now and
then -- at its word, or at none it has, or unaligned -- and ends its
interrupts as Linux does, through the word where the host set it and by the
register where not. Where the guest's CPUs sit is made up too
-- a host's threads, with SMT or not and its caches shared this widely,
given to a guest of up to 64 CPUs, in order or anywhere -- and every
topology leaf held to what a package of that shape answers: leaf 1's count
of APIC IDs and HTT, the sharing and core counts of leaf 4 and of AMD's
0x8000001D at each level, the last level the host's highest before the
list's end, the x2APIC topology leaf level by level, and AMD's 0x80000008
and 0x8000001E; and the TSC-deadline timer exactly with the APIC. Six
mutants each failed it: the SMT level's shift the package's, the last level
missed by one, 0x8000001E's threads and 0x80000008's CPUs each off by one,
and HTT always on. `lapic` holds the TSC-deadline mode to the SDM, the mode
read as the guest reads it, in the timer's entry: a deadline read back in
that mode and 0 once it has run out, the MSR on the way into the mode 0,
the current count 0 in it, and in any other mode the MSR reading 0 and its
write moving nothing -- a deadline kept after it ran out and one armed in
another mode each failed it. Two mutants pass, and are no change: the
MSR's read and the count's each read 0 outside the mode by a check of
their own, where nothing else ever lets them be anything else. The model is written from the manuals and
not from the policy, so a leaf, a bit or an MSR the policy comes to offer is
a finding until the model is told of it too; nine mutants of the policy --
the old swallowing of every MSR, every leaf passed through, the tops not
held, a PAT of any bytes, addresses not canonical, FFXSR in EFER, an AMD
MSR on Intel, and IBPB and AutoIBRS offered with the model told -- each
failed within its first 1,600 inputs.
The guests' network has three: `switch`, guests on its ports and `hv0`
sending each other frames of every kind -- as themselves and as each other
-- held to a model of where each must go and what each must count, the
ports on two networks and alone, and now and then one handed to another VM
just before one of the locks a guest's send takes (a hook in the stand-in
spin locks, `sync::before_lock`: the moment another CPU's change lands in,
which one thread never makes otherwise), every guest then held to what
some order of the hand-over and the send gives; a guest's bursts among them, past its share of `hv0`'s backlog (the backlog
over the ports in use, counted afresh at each of the stack's passes) and
past its rate to every port, the clock moved on now and then for the rate
to come back, and ports whose NIC holds its receive interrupts, a frame for
which wakes nobody while the inbox is under half full -- and sends of TCP
whole and frames with their checksum left to finish among them, each handed
so to a port whose guest takes it so, and cut, or finished, for every other
and for `hv0` -- and IPv6 among them, neighbour discovery of every kind,
multicast listeners' reports, echoes, UDP from DHCPv6's server's port,
behind extension headers and in fragments, their lengths right and wrong, a
guest now and then using more addresses than a port may have bound, held
to a model of the switch's rules for it written from RFC 6620, 6105, 6980
and 7112 -- whose addresses are bound to which port, the advertisement it
answers a solicitation with laid out byte for byte from RFC 4861; `nat`,
guests' packets out into the world and the world's answers back, each
rewrite checked field by field and its sums summed from scratch; and
`guestnet`, the two as the module wires them, guests on the switch reaching
the world through NAT. `nat` floods the table past full now and then and
holds every flow a full table takes to its rule -- the one used least lately
of the guest that holds the most, the asker's own when it holds as many as
any -- from the table's own listing before the packet, a sample of them,
and a packet given no room to a guest past the 64 the table keeps shares
for; and it runs NAT's boot self-test once, which holds the cases random
input seldom makes, a tie and every slot held. And `platform` does it to the whole machine: a Linux guest's platform built
and loaded as `hv boot` builds it -- CPUs, disks, NICs, IO-APIC and all --
and run by the real run loop, `LinuxGuest::run`, on each of its CPUs, each
on a thread of its own and one at a time -- the one turn handed from thread
to thread directly, and the clock moved on to the next deadline when every
CPU waits, so that nothing sleeps on a real clock: sixteen times the runs a
second of the first harness, whose CPUs slept for real, and ten processes
at once some forty times theirs. Where the loop would enter the
guest, the CPU asks a script what the guest does next: a port, an MSR,
CPUID, a fault on a device's page with the instruction that made it put
where the guest's paging finds it, a `mov` to or from CR4 or DR0-DR3 put
there the same way -- under AMD-V, which stops a guest at each -- a halt, a window
opened, an event taken, another CPU's turn -- and between them what a
driver does over several
exits: the 8259 initialised, the APIC turned on or moved between its modes,
an IO-APIC pin routed, MSI-X turned on and its table filled, a virtio queue
set up and given requests, another CPU started by INIT and start-up IPIs.

A finding is a panic -- an index out of range, an overflow -- or a broken
invariant: an interrupt injected into a guest that cannot take one, or over
an event already on its way in; an exception's vector from the APIC; a disk
request outside the disk, not in sectors, or past the requests in flight; a
device that, its ring raced, takes twice what the ring holds in one call --
a pass that, on a host, would never end; a
frame longer than a frame handed to the switch; a DHCP answer that is no
datagram, or whose checksum is not its own; a frame the switch put
elsewhere than its destination says, or counted otherwise; a packet NAT
rewrote in any field but its own, or with a sum it did not have, a port
given to one flow while another was live on it, an answer let in that no
live flow asked for or kept out that one did, a packet counted twice or
not at all; a guest's packet for where NAT keeps the guests from -- a
metadata server, a private network, the uplink's own subnet -- sent out,
one for where they may go denied, a reject other than an administratively
prohibited to its guest quoting it, or the networks NAT lets through other
than `nat allow` and `nat deny` left them (control runs with nothing
denied, the DNS server not let through, `nat allow` ignored and the
reject's code wrong failed at inputs 16, 29, 1 and 16); what a guest takes
from the world for another guest's address,
and what `hv0` or a guest takes from a guest not sent as that guest; a
vCPU that enters its
guest or waits with one of the kernel's locks held; a guest whose RAM would
cover its devices' pages; a `mov` of CR4 that by the CPU's next entry has
not come to what the script reckons -- a read whose register does not hold
the CR4 the guest wrote, a write a CPU takes that is not the guest's CR4
with MCE in the register and RIP past it, one a CPU refuses that is not a
#GP with CR4 as it was -- or a CR4 written with a bit the guest's CPUID does
not offer, PAE cleared in long mode, or a bit a CPU translates by changed
and the guest's TLB not flushed. (Control runs with the CPUID's mask left
out, PGE's flush left out, and reads answered from the register rather than
the guest's CR4 failed at the first, the first and the fourth input; the
first run of the checks found the script's own 32-bit belief outliving the
INIT that put its CPU back in long mode.) A `mov` of DR0-DR3 the same: a
read whose register does not hold what the guest's writes made the
register, a write that is not the guest's from then on with RIP past it,
one made at CPL 3 -- which a CPU faults before the exit -- that is not a
#GP, or one under DR7's general detect that is not a #DB with DR6.BD set
and GD clear, the instruction not run either way; and the guest's four, at
every one of its entries after, other than its writes made them. (Seven
mutants of `hypervisor/hv/src/dr.rs` -- a write into another register, a read of 0,
no CPL check, no general detect, GD left set, a write in 32-bit code
keeping its high half, the instruction not stepped past -- each failed
within 145 inputs; the first run of the checks found the script's model
outliving the `long_mode` its return from 32-bit code calls, which zeroes
them as a boot loader leaves a CPU.) Or a spin -- the run loop reading the clock a
million times with neither an entry nor a wait between, which on a host is
a CPU spun for as long as the guest stays so -- or a hang, ten seconds with
no input done. Each is reported with the seed and the iteration that make
it again, and the input written to `out/hv-fuzz/findings/`, to replay
(`--replay TARGET FILE`) with the panic's own backtrace.

With no arguments every target runs 50000 inputs from seed 1, the same
ones every time: the gate, which CI runs on its x86-64 leg and which takes
about half a minute. A campaign is `--seed N --seconds S` for as many seeds as
there are CPUs: each seed another set of inputs. Whether the whole-machine
runs go deep is `HV_FUZZ_STATS=1`, which says how they ended and after how
many exits -- a run that ends early reaches little, so every step that ends
one is about one in 4096 -- and what the inputs reach of each file is
clang's coverage (`RUSTFLAGS="-C instrument-coverage"`, `llvm-cov report`):
on the first day the gate's inputs reached 94% of the regions of the
sources it compiles -- 93% of `run.rs`, 96% to 100% of each device, 99% of
the local APIC.

What it found on its first day: a virtio buffer at the top of the address
space made the disk's status write compute its address past 2^64 -- a panic
in a `RUSTUB=1` kernel and a wrap to page 0 in a plain one -- and a buffer's
address is since reached only through `Seg::at`, which does the sum
checked; the Linux loader took a kernel's preferred address below 1 MiB,
over the zero page, the page tables and the GDT it writes there; and a vCPU
halted with a periodic APIC timer's interrupt held back by its priorities
spun its host CPU, its sleep computed from a deadline already past. Reading
the run loop for it turned up a fourth: a guest of more than 4064 MiB had
RAM over its IO-APIC's and local APIC's pages, which in xAPIC mode or with
an IO-APIC left it with devices it could not reach and nothing to say so --
guest RAM below 4 GiB now ends below them (`linux::LOW_RAM_END`), the rest
from 4 GiB. And a fifth, not live
but one check from it: the DHCP server's `is_request` read a frame's
EtherType before anything had checked the frame held one -- an index out
of range on a guest's runt frame, which only the switch's own length check
kept away; it now parses the datagram first, which checks.

And on the day the switch and NAT became targets, `guestnet` found a guest
could send as another: the switch passed on a frame from any MAC and any
address, and NAT answers a flow at the MAC its last packet came from -- so
a guest that sent one packet of another guest's flow, with that guest's
address, took the flow, and the world's answers to it came to its port.
The same frames could tell `hv0`'s ARP table another guest's address was
its own. The switch now checks what comes in at each port, as a cloud's
virtual switch does (`ingress` in `modules/hypervisor/hv/src/net.rs`): IPv4 or ARP
only, from the port's MAC, an IPv4 packet from the port's address and an
ARP message with the port's MAC and address -- or none yet, a probe's --
as its sender; the one packet with no address yet is a DHCP client's,
which the switch answers itself. The rest is dropped and counted -- `hv
list` says how many a guest sent as another -- and the targets hold the
switch to it both ways: `switch` to a model of the rule, `guestnet` to
what the rule is for. IPv6, which the switch then dropped, now goes
between the guests under rules of its own ([A network](hypervisor.md#a-network)),
held to a model by `switch` and, for what it is for, by `hv-linux-test.py
--net`; ten mutants of them -- the packet not cut to its length, a guest's
router advertisement let through, the newest address let go of in place of
the oldest, a VM's addresses kept after it went, IPv6 handed to `hv0`, the
advertisement's checksum without its pseudo-header, duplicate detection
binding nothing, neighbour discovery in fragments, an advertisement's
target taken unchecked, and UDP over IPv6's checksum of 0 left 0 -- each
failed the gate.

What it cannot see: the CPU is a stand-in, so nothing of hvarch -- its
`unsafe`, the VMCB and VMCS, the entry and the exit -- is fuzzed, only what
the run loop does with the exits it decodes; the interleavings are the
script's, at entries and waits, which exercises every path between the
CPUs but finds no race; the devices' backends -- the disk files, the
switch in `platform` -- are stand-ins that check what they are handed; and
under the switch and NAT the net layer is a stand-in too -- its devices,
frames and ARP table, `hv0`'s receive path run on the caller's thread --
so what they do is fuzzed, one frame at a time, and the receive path's own
concurrency is not.

### `net-fuzz.py` -- everything the network hands the kernel, fuzzed

Everything the network hands the kernel is somebody else's to choose --
every frame on the wire, every answer a server gives the HTTP client, the
DHCP client and the resolver, every byte an SSH client sends the server --
so a panic, an overflow, a lock broken or a loop that never ends anywhere on
those paths is one somebody else can cause, and on the Hetzner boxes the
network is the only console there is. `fuzz/net` is a host program
built from the kernel's own crates as they are -- `net`, `netwire`, `tls`,
`fs`, `ssh` and the sshd module's source, over `kcore` and `ffi` -- linked
with the rest of a kernel written for the purpose (`fuzz/common/machine/`,
which `fs-fuzz` shares, and the NIC in `src/machine/`): its C++
half as the `ffi` crate declares it, the locks, tasks, events, soft IRQs,
timers, the clock, the entropy pool, the log, the command table. The tasks
are threads, one of them running at a time as on a CPU of its own, handed
the turn when it blocks or -- as often as the input says -- where it lets a
lock go; a sleep ends at the tick it would end at in the kernel, and the
clock jumps to the next deadline when every task waits, so an hour of a
lease or a day of a DNS record costs what its timers cost. What the kernel
says of its locks is held to: sleeping with a spin lock held or interrupts
off, a soft IRQ handler that sleeps, a lock taken twice or in both orders, a
mutex let go by a task that does not hold it, a task that ends holding one,
an allocation or a free with interrupts off -- each a finding where it
happens. Overflow checks are on, as a `RUSTUB=1` kernel has them. Each input
runs in a process of its own, forked from one booted machine: the layer's
statics are the kernel's, and one input's must not leak into the next.

The machine's NIC is the fuzzer's, and the wire goes to the world
(`src/world/`): a link that delays, reorders, duplicates and loses as the
input says, a LAN whose hosts answer ARP or do not, and a model of each
protocol's other end. Every frame the machine sends is checked as a
receiver would check it (`check.rs`): headers whole and every checksum
right, no source address the machine does not have or no host may have,
nothing to an address no packet may go to (RFC 1122 3.2.1.3), a unicast
never on the link's broadcast address (3.3.6), a multicast on its group's
own, TCP's flags in combinations that mean something. The targets:

- `stack`: frames of every kind -- ARP, ICMP, UDP to every port with a
  listener, TCP, IPv6, noise -- from addresses that are and are not hosts,
  into the receive path;
- `tcp`: connections both ways with a peer model that keeps every byte of
  both streams (each byte a function of its offset, so a byte delivered that
  was never sent, or twice, is seen), checks every segment the machine sends
  against RFC 9293 -- the window, the sequence space, what may carry data --
  and does what peers do: retransmits, probes a zero window, resets, goes
  quiet; and an attacker off the path injecting segments at every sequence
  number, an ICMP error at every quoted one. The machine's connects are
  plain or bulk, and so is one of its listeners, and the peer scales its
  window or not, at any shift -- past RFC 7323's 14 too -- so that the
  window the machine offers runs past 64 KiB and must never shrink but by
  less than its unit (RFC 7323 2.4's retracted window, which 55% of the
  inputs see), never reach past the receive buffer -- every connection's
  furthest edge audited against it, and the kernel's count of bytes inside a
  window offered that found no room held to none; the old edge rounded up,
  put back as a control, fails at the second input -- and what the link
  reorders is kept past a hole and taken in when it fills; the link loses
  the machine's own data, a little or a lot, for its sender's recovery --
  RACK's, the loss probe's, a timeout's slow start, and NewReno's with a
  peer that does not take SACK -- to fill every hole and keep every byte;
  and a blind reset in the window, past the edge of everything the peer
  sent, must change nothing (RFC 5961). Most connections take SACK, the peer
  saying up to four blocks, fewer, or none at all, and now and then letting
  go of what it SACKed: a SACK block of the machine's must name only what
  had reached it, a D-SACK only what had reached it twice, and neither may
  come on a connection that did not agree to SACK; and the machine may never
  send again what the peer had told it it held -- cumulatively, or by SACK
  and not let go of since -- by an acknowledgement the machine had taken in:
  one at its RCV.NXT, which no window turns away, put on its NIC a
  millisecond before, with its transmit side not stopped since (a frame made
  while it took in a batch, or held by a stopped NIC, is seen later than it
  was made). And now and then a second device, eth1, standing for the
  hypervisor's switch, from which a guest sends a SYN to a port the machine
  listens on -- at eth1's address or at eth0's: a listener takes connections
  from its own device alone, so no SYN-ACK may go out to it on either wire
  (the first input of the control run with the device left out of the
  listener's lookup got one);
- `http` and `https`: the HTTP client fetching through chains of redirects
  from a server that answers well -- whose result is then known exactly --
  or to break it: numbers longer than any field, chunk sizes of every
  length, headers that never end, a redirect anywhere, a connection reset,
  a pause past the idle timeout -- and a server that never opens its window,
  which the GET must give up on rather than wait on for good. Over TLS the server is rustls driven by
  hand as the `tls` crate drives the client, with a certificate from the
  fuzzer's own CA (`certs/make.sh`) or one run out, for another address,
  or from nobody -- which the client must refuse before its request goes --
  in TLS 1.2, 1.3 or both, with a close_notify or without, and now and then
  a byte of what it sends flipped: a body the client gives back is the
  answer's, from its start, and one it calls whole is whole;
- `dns`: the resolver against a server that answers with CNAME chains,
  compressed names, errors, nothing, and answers forged by address, port,
  id and flag; the cache held to the TTL, a day at most;
- `dhcp`: the client getting and keeping a lease from a server that NAKs,
  goes quiet, answers another transaction or another client, offers
  addresses no host may have, changes its terms, and a rogue that answers
  first. What it sends must be what its state calls for (RFC 2131 4.3); what
  it binds to, an ACK's terms, all of them, on the device; and no address
  kept past its lease or its refusal;
- `icmp`: pings both ways and ARP under them -- the world's echo requests
  well made and not, ARP requests and replies well made, malformed, and
  lying, and the shell's `ping`, `udpsend` and `arp`: the cache learns only
  what RFC 826's merge lets it, and a host that answers every ping gets
  every round;
- `udpshell`: the shell over UDP asked by clients near and far, in
  datagrams well made and not; every reply whole, in order, flagged at its
  end, and exactly what the command printed;
- `netconsole`: the kernel log to a collector that is there, beyond the
  gateway or not yet, from a machine without an address, whose NIC stalls,
  logging past what the ring holds, from tasks in the middle of a send and
  from interrupt handlers, where there is no task to ask about:
  the datagrams numbered with no gap of the machine's making, each line at
  most once, in order, and all of it in the end but what the ring says it
  dropped;
- `ssh`: the sshd module on a ramfs root, and SSH clients (`sshc.rs`) that
  log in with the key it knows or a stranger's or a signature over the
  wrong session, run a command or a shell with a window of their choosing,
  rekey, answer keepalives or not, or break the protocol at one point of it
  -- a version line of HTTP, no cipher in common, a point of small order, a
  length past any, an IGNORE where the strict exchange allows none, a packet
  tampered with, a channel before the login, more data than the window.
  Every packet the server sends is opened and checked, its key exchange
  signed by its host key over the hash both ends made; a login is had
  exactly when the known key signs the right session; a command's output
  comes back whole; a client that breaks the protocol is told so. Now and
  then the command is `term`, which prints the terminal its session lends
  it -- run with none, or after a pty-req of a size and window-changes to
  others, sizes of 0 and of `u32::MAX` among them -- and must say the last
  size sent and how many changes there were, as `edit` is told them;
- `tls_server`: the TLS server the host API serves over (`tls::pinned::Server`), run
  as the module runs it -- a listener and a task a connection over the
  kernel's TCP, at most four at once -- letting in one key and answering
  each line said to it; and the world's TLS clients (`tlsc.rs`, rustls'
  own, pinning the server's key as `nosctl` does) that show the known key,
  a stranger's or none, speak TLS 1.3 or 1.2 alone, offer the API's
  protocol, HTTP/1.1, both, none or only `h2`, or break the protocol: an
  HTTP request where the ClientHello would be, nothing at all, a
  ClientHello and goodbye, a record tampered with -- or say goodbye with
  their last line, before the answers. Over any link no key but
  the known one is ever let in, and no client without it is answered a
  byte; over a good one each is refused for its own reason -- unknown key,
  no certificate, incompatible, protocol, timeout, closed -- or let in with
  the protocol both prefer, every line back in order and a goodbye; the
  server is never the side left in TIME-WAIT, and once it stops nothing is
  left behind;
- `tls_client`: the pinned client (`tls::pinned::Client`), what ipcnet's
  link to another host will be, run as a module will -- a task that
  connects over the kernel's TCP, says lines and reads them back -- against
  the world's rustls server, which shows its own key, a stranger's, or its
  own certificate signed by another key; asks for the client's certificate
  and lets the machine's key in, asks and lets in another's, or does not
  ask; or breaks the protocol -- an HTTP answer for a ServerHello, silence,
  a hang-up, TLS 1.2 alone, no protocol in common, a byte flipped. Over any
  link a session is up only with the server's own key, proved, and nothing
  is read that was not said; over a good one each connection is refused for
  its plan's reason, or every byte comes back and the client closes cleanly,
  first. Five mutants of the client -- the key not checked, the signature
  not checked, TLS 1.2 spoken, the client waiting for the server's FIN, the
  key not kept -- each fail it;
- `api`: the host API module's own source (`modules/mgmt/api`) on a ramfs root,
  its api.toml and clients written there first, its host key found there or
  made on its first start; on the bus the kernel's system provider (its
  source too), the network layer's, the module's own and one of the
  fuzzer's -- a read, an action and a job, each keeping the tag its caller
  gave it. The world's clients (`tlsc.rs` speaking HTTP and WebSocket
  through `httpc.rs`, which reads the server's answers and frames as
  strictly as HTTP and RFC 6455 have a server's) show the admin's key, the
  reader's, a stranger's, none or the admin's certificate signed by
  another, or break TLS, and ask for every route the API has and some it
  has not: descriptors and then with their ETags, reads, actions and jobs,
  a job's record and a cancel, the stream; a method a route does not take,
  HEAD, requests the parser refuses, one past the size, HTTP/1.0; one after
  another with a pause or all at once, a body after the server's 100
  Continue, the connection to close, or kept open and quiet; a stream that
  answers the pings or not, pings, closes, or sends what the server will
  not take. Meanwhile the console takes a key off the list and puts it
  back, writes a clients file the API must refuse and reloads it, and stops
  and starts the server. What must hold: no byte of HTTP for a client the
  server was not given a key of, or one its allowlist keeps out; a reader
  never makes an action or a job run -- the provider holds the tags of what
  ran -- and every action an admin was told ran, ran once, a line of the
  audit log each and no other line; every answer well formed, of the
  status the model gives the route, the method's kind and the role, its
  error's code the bus's for that status; a key taken off the list let do
  nothing more -- nothing it sends parsed, a stream of it closed; the
  server's own count of its
  connections (`api.status`) within api.toml's limits, in all, in the
  handshake and of a key; a stream's messages of the channels asked for,
  its log lines each once and in order, its quiet client let go at its idle
  time and not before; `api stop` within three seconds whatever is
  connected; the server never the side left in TIME-WAIT; and once the
  module is unloaded, nothing left behind. Its first runs found a HEAD
  answered with its body, refusals of HTTP's coded `invalid` whatever their
  status, a request of a key taken off the list parsed before it was
  refused, and `Connection` read from its first line alone (in
  `web::http`), a stream that went on after its key was taken off the
  list, and a client's close_notify that came with its last request taken
  for the session's end (in `tls::server`, whose own target now sends
  one); 27 bugs planted in the module one at a time were each caught, 26 of
  them within the gate's 500 inputs;
- `ipcnet`: the ipcnet module's own source (`modules/cluster/ipcnet`) as host `b`
  of a cluster, over the image's channel (the `ipc` crate), its hosts.toml
  and key on a ramfs root, and two mailboxes of the image's: `nbs.sink`, a
  Cluster name that keeps what it gets -- paused when the input says, so
  that it fills -- and `kernel.hidden`, a Host name; and a third, the
  victim, closed when the input says. The world is host `a`, dialing `b` as
  a host of nos does -- the control connection, then the bulk one once it
  has said its Hello, and now and then both at once -- with the key `b`
  pins, a stranger's or an impostor's, and a Hello right or of another
  cluster, for another host, from a host `b` does not know, or none; and,
  once up, speaking ipcwire through `tlsc.rs`'s talk: messages to the sink
  by name and by address, by both classes, of every size, to the Host name
  and to addresses nobody has, bursts into the paused sink, monitors of the
  victim, its own Pings, silence, frames the wire refuses or out of their
  place, connections closed; while the sink answers `nbs.w@a`, and the
  console stops and starts the server. What must hold: nothing reaches the
  Host name; what reaches the sink is the world's, each once, in order
  within a class and a sender, and by its address only once the sink has
  sent to `a`, with the peer's number in its sender; what reaches the world
  is the sink's, each once and in order, from the sink's own address; a key
  not pinned is never up, a wrong Hello never answered; a burst past the
  sink's places bounced back for each it had no room for, and a bounce only
  of a message the sink never got; a Down only of a monitor asked, once,
  Closed only once the victim is closed -- and, on a perfect link, every
  monitor of it on a session still up told; the world's Pings answered; a
  silent world lost within 3 s and a margin; and once the module is
  unloaded, no router and nothing left behind. Its control mailbox,
  `ipcnet.ctl`, is asked by a task of the image's as the cluster's member
  asks it: who this host is, held to `b`'s name, cluster, key and address;
  lists of peers -- `a` alone, `a` with another, another cluster's, one
  naming `b` itself, noise -- hosts.toml read before and after each, saying
  exactly the list taken and nothing changed by a refusal, and `a`'s
  connection never ended for a list that keeps it -- among the lists, `a`
  made a host invited, which may reach `nbs.sink` by that name alone, and a
  host again: its line in hosts.toml and in `ipcnet` saying so, the peer
  never made anew either way (its counts never going back), and while it is
  invited nothing of its reaching the sink by address and no monitor of its
  answered Closed; `a` pinned by a second key beside its own, kept with its
  connections; and a key staged for `b`, the same one each time asked and
  said by `TAG_ID`. Five bugs planted in ipcnet each fail it: a delivery
  past the restriction (the 126th input), a monitor taken from a host
  invited (the 281st), a restriction lifted by dropping the peer or made by
  making it anew, and a peer made anew for a change of its keys (the
  first). Nine of ten bugs planted in
  the module are caught -- a peer's message it may not reach bounced, a full
  mailbox's not, two messages of one pair begun at once, silence never
  losing the peer, a Hello of another cluster or for another host taken, a
  Down under the router's own id or never said, a Pong never said; the
  tenth, a loss that bounces nothing of what never went, is not: the world
  cannot tell what was queued from what was begun, and a message begun is
  lost with its connection by design;
- `terminal`: the session's line editor (`ssh::terminal`) on its own, typed
  at with any byte -- weighted to the keys it acts on and a few letters, so
  that lines repeat and a search finds them, with runs of one key past the
  longest line and the history's end. Every key is held to a model of the
  editor: the line Enter runs, a logout on ^D on an empty line and on
  nothing else, and what a terminal of one row shows once it has acted on
  the echo -- text, CR, LF, Backspace, erase to the end of the line and
  clear the screen, anything else a finding -- the prompt and the line, or
  the search's prompt, its query and what it found, exactly. A search that
  skipped its newest candidate and an erase that left its character behind
  each failed the first input.

At the end of every input the machine must hold nothing: no TCP slot, no
frame out of the pool, no task. A finding is any of that, a panic, a spin
(a million reads of the clock with no other task given the turn) or a hang
(twenty seconds of an input), each with the seed and iteration that make it
again and the input written to `out/net-fuzz/findings/` -- `--replay
TARGET FILE`, with `--trace` for every frame both ways and every line
traced.

With no arguments every target runs its own number of inputs from seed 1 --
three hundred of `ssh`, five thousand of `icmp` -- the same ones every
time: the gate, a couple of minutes, which CI runs on its arm64 leg. A
campaign is `--seed N --seconds S`, a seed for each CPU.

What it found on its first day, each fixed:

- the receive path delivered IPv4 it should have dropped: fragments, bad
  header checksums, sources of 0.0.0.0 and broadcast addresses, UDP with a
  wrong checksum -- and answered them, as addresses not its own;
- TCP took an out-of-window segment's window for the connection's, so a
  guessed segment could shut a connection's window for good; took a SYN
  with ACK, RST or FIN for a connection request; answered port 0 and
  broadcast addresses; answered nothing to a zero-window probe; kept the
  window of before a retransmission and sent past the peer's; and its
  connect called a peer that answered and closed at once a failure;
- the HTTP client's chunk sizes, status codes and redirect ports
  overflowed -- a panic in a `RUSTUB=1` kernel -- it called broken chunking
  complete, and it took a response cut inside its headers for a body made of
  its own status line;
- the TLS client took a stream cut inside a record, and a receive that
  timed out, for the end of the stream: a body with no length that stalled
  came back whole;
- ARP waited ten seconds a try on an idle CPU; its cache learnt every
  sender of every request on the link, which RFC 826 does not -- a busy
  link pushing out the hosts the machine talks to -- and took broadcast,
  multicast and its own MAC, and 0.0.0.0, broadcast and its own address;
- a UDP datagram or a ping to a host whose ARP went unanswered was sent to
  the link's broadcast address, every host on it handed somebody else's
  datagram -- and netconsole's records were spent on it, lost; datagrams to
  0.0.0.0 and 127/8 left the machine, and one could leave from 0.0.0.0 when
  DHCP let the address go mid-send;
- the DHCP client, once it had renewed, started again from DISCOVER while
  bound -- its lease overwritten by an offer's terms and never renewed
  again; it kept its address past the lease and after a refusal, put only
  the address of a renewal on the device, and took addresses no host may
  have;
- the UDP shell ran a line longer than it holds cut short;
- netconsole's drain slept 200 ms at a time holding the log's lock, with
  interrupts off, whenever the device had no address -- the guard of a
  `match`'s scrutinee lives to the end of the match -- and every line any
  CPU traced waited behind it; and a batch whose records the ring evicted
  while it was on its way was sent again;
- and later, by a panic made to run on a parked CPU's stack rather than by
  an input: netconsole asked which task was logging in the way that
  complains off a task's stack, so a panic reported from one -- x86's
  double fault's, a parked CPU's -- panicked inside itself at its first
  line, before the netconsole had any of it, and went no further. It asks
  the way that answers 0 there now, and the target logs from interrupt
  handlers, where the machine's stand-in for the kernel makes the same
  complaint: put back, the old call is found at the first input.

### `fs-fuzz.py` -- everything a disk hands the kernel, fuzzed

What is on a disk is whoever wrote it's to choose -- a disk that came with
the machine, an image somebody made, a filesystem another kernel left half
written -- and a device says itself how many sectors it has and how big
one is. So a panic, an overflow, a lock broken, a loop that never ends or a
write where nothing may write, anywhere on the paths that read a disk, is
one somebody else can cause. `fuzz/fs` is a host program built from the
kernel's own crates -- `block` and `fs`, over `kcore` and `ffi` as they are
-- on the machine `net-fuzz` runs on (`fuzz/common/`: the C++ half, the
tasks one at a time, the lock rules, a process for each input), and disks
whose media are the fuzzer's (`src/machine/disk.rs`): of any geometry a
device may claim -- 520-byte sectors, 2^64 of them -- each with a volatile
write cache that a plain write goes into and a flush or a forced write
through, a power switch that pictures, at chosen requests, what the medium
would hold -- the cache's writes each put down or not -- and requests that
fail. Every request is checked where it arrives, as the block layer's
contract with a driver says: whole sectors, inside the device, never from
where the caller may not sleep, and no write to a disk nothing should be
writing -- a filesystem mounted read-only or not mounted, a partition table
being read. And every allocation of the kernel's code is held to the kernel
heap's ceiling -- 128 pages at once, past which `kernel_alloc` returns null
and what cannot fail panics -- where the host's allocator would give
gigabytes (`fuzz/common/machine/heap.rs`): the fuzzer's own big allocations,
its model's buffers, images and judges, are marked as its own
(`sched::harness`), and a refusal says the backtrace of what asked. A list
grown with a disk's contents once reached 512 KiB unseen by every gate --
the journal replay's, at 8193 transactions -- and panicked the mount of a
real root. The `vfs` target lifts the ceiling for itself: ramfs keeps a
file's data as one allocation, which its model grows past 512 KiB.

The images are the fuzzer's own (`src/image/`), made from the formats and
not from the drivers: an ext2 as mke2fs lays one out -- every block size,
one group or many, sparse superblocks or not, 128- and 256-byte inodes --
with a tree put on it, files with holes and without, directories with
deleted entries between their live ones; MBRs and GPTs; the disk log's
header. And judged the same
way: `ext2::check` reads an image as e2fsck -fn does -- every inode with
links in use, every block claimed once, directories parsed, link counts,
bitmaps and every count -- and sorts what it finds into *corrupt* (what no
crash of a correct driver may leave: a block in use and free, a block two
inodes hold, a name leading to an inode not in use) and *unclean* (what
e2fsck still fixes: a leak, a count, a link count, the state bit). The two
are held to e2fsprogs by `fs-fuzz.py --e2fsck` (in CI on the x86-64 leg;
`FS_FUZZ_DUMP=<dir>` writes each image judged): what `check` calls clean,
e2fsck passes; what it calls unclean for more than the state bit, e2fsck
faults.

ext4's judge (`fuzz/fs/src/image/ext4/`) is the fuzzer's own as ext2's is,
sharing nothing with `extfmt` -- `fs-fuzz.py` refuses to run if a judge uses
it. Its mkfs makes what the ext4 driver is to read and write: blocks of 1, 2
and 4 KiB, 32- and 64-byte descriptors, flex_bg or not, metadata_csum seeded
by the UUID or by the superblock's own seed (the UUID changed after), every
checksum on every block, groups that hold nothing left uninitialised with
noise where nothing was written, files in extent trees up to four levels
deep -- deeper than their files need, at times -- with runs of zeros
allocated as unwritten extents over noise, which must read as zeros, entries
the kernel's tree leaves out (names of 64 bytes or more, fast and slow
symlinks, a device node), directories indexed now and then -- an htree of
one level, each name in the leaf of its half-MD4 hash's range, by a hash of
the judge's own (`ext4::hash`) -- and a journal: clean, or holding the newest copies
of some metadata blocks, and of some of the files' data as Linux's
data=journal has a journal hold it, in transactions nobody replayed, their
homes given noise -- with a noise copy of a block a later transaction revokes, and a
last transaction never committed, both of which a replay must leave alone.
The `ext4mkfs` target makes such images, and `--e2fsck` holds each to
e2fsprogs: clean by `e2fsck -fn` (its journal replayed by `-E journal_only`
first), and every directory and file of its tree in what `debugfs rdump`
reads out of it, byte for byte. e2fsck runs with `-E no_optimize_extents`:
a tree wider or deeper than its file needs is the format's to allow and the
kernel's to leave. Four levels, not the kernel's five: e2fsck 1.46.5 writes
past a five-level array on a tree of five (the bug CVE-2022-1304's fix
closed).

Its check (`ext4::check`) reads an image as e2fsck -fn does, after doing
first what e2fsck does first: replaying the journal by a JBD2 reader of its
own (`ext4::replay`), and releasing the superblock's orphans -- one of no
links deleted, its blocks and inode free; one with links truncated to its
size, its blocks past it free and its block count put right as it goes.
Then the passes: every inode in use below its group's unused tail, its
checksum and its extent tree -- each node's magic, depth, checksum, entries
in order, every block claimed once and none of it metadata -- or its block
map; directories, by their tails or their index blocks' own; where every
directory hangs, its `..`; link counts; the bitmaps, their checksums and an
uninitialised group's as its own metadata makes it, against what owns each
block; the groups' counts. Corrupt and unclean as ext2's are; the
superblock's free counts not judged, Linux keeping them lazily. The resize
inode's tree of reserved descriptor blocks is left to e2fsck: nos never
writes it. `--e2fsck` holds the check to e2fsprogs three more ways: its
replay of each planted journal to `e2fsck -E journal_only`'s, byte for byte
but for e2fsck's own superblock fields; mke2fs's images -- 1.46's defaults,
1 KiB blocks with 128-byte inodes and no 64bit, metadata_csum_seed with the
UUID changed, an htree directory, an attribute block -- clean to it, and
every byte of their metadata e2fsck notices changed noticed by it too, thirty
a filesystem; and orphans debugfs leaves -- deleted and truncated -- released
to the tree `e2fsck -fy` leaves. The targets:

- `part`: MBRs and GPTs of every shape -- protective and not, CRCs right
  and wrong, entry sizes and counts past sense, slots overlapping, LBAs at
  the top of the numbers -- on disks of every geometry; the partitions the
  probe registers held to a reader of the table written from `block`'s
  documentation (which slots, where, under what name), and the probe's
  writes to none; I/O and batches through every device held to its bounds
  and rebased onto the right sectors of its disk; claims taken and given
  back, held to what they are for: never two writers on a sector;
- `disklog`: prepared areas on disks and partitions, headers damaged, the
  device held by something else, sector sizes of every kind, lines logged
  before the area is known and after, from tasks and interrupt handlers;
  the area the documentation says is taken, its header right, its text every
  line in the order it came as far as the area holds it, nothing written
  anywhere else;
- `ext2`: sound images mounted, read-only and not, and worked
  through the VFS -- opens with every flag, reads, writes and seeks at every
  edge of a block and of the indirect blocks of every level, across 2 and
  4 GiB and at the end of what a file can be, truncates both ways, creates,
  renames, removes, chains of directories forty deep, files of two names
  (ext2's, as Linux makes them: one name removed, the other the file, and
  what one is given, both have) -- each call's answer
  held to a model of the tree and the files open on it (`targets/fsops.rs`,
  the VFS's rules and the filesystem's limits written down), the tree read
  back through the VFS held to the model's, the image a clean unmount
  leaves clean and holding the model's tree, and mounted again, the same;
  what ext2 says it has room for (`Vfs::space_of`) held, before that
  unmount and after the mount again, to the blocks its bitmaps leave free.
  A file held as a disk's image is walked for where it has data
  (`Image::extent`), a call at a time to its end: what a call says is holes
  must be zeros in the model, so that a VM's disk cloned with its holes left
  out loses nothing -- and the walk goes forward, inside the file. A
  call the filesystem refused for want of room is held to having changed
  nothing -- a write to having left the file its size, a first part of the
  new data in it at most, and a batch of a disk's image's requests to every
  write the image answered done there whole, read back as its guest reads
  it: through the image, since the VFS reads what an image wrote over
  unwritten extents as zeros until the image's next sync makes them
  written -- and now and then the filesystem is filled to its
  last block first, so that writes run out of room part way. Now and then
  `find` runs over the tree as the shell runs it, with an expression of the
  input's -- or words that are none, nested too deep, too long -- and what
  it prints is held to a model of its own (`targets/find.rs`): a parser
  that builds a tree where the kernel's emits postfix, patterns matched by
  dynamic programming where the kernel's go back to the last `*`, a walk
  of the model's tree; every path the model gives and no other, a
  directory before what is in it, a name that is not text and a path too
  long said where they are, and an expression the model refuses refused.
  Control runs with `-size -N` taking N itself, and with a `*` gone back to
  only once, failed at the 1189th input and the 13th. Its images have
  files past 4 GiB, sparse, under the triple indirect block, and filesystems
  with `large_file` and without it, which the driver must set for the first
  file of 2 GiB or more; the checker holds a large file without the flag,
  and a size past what its tree maps, unclean, as e2fsck does. Every power
  cut is at worst unclean -- which is what ext2's commit order promises --
  and so is a run in which requests failed;
- `ext2bad`: sound images with a few fields made what they
  must not be -- the superblock's geometry, a group's descriptor, an inode's
  mode, size, links and block pointers (outside, into the metadata, into
  another file, at itself), a directory's entries, an indirect block, a
  bitmap -- mounted if the driver takes them and worked through the VFS, held to what
  holds for any image: a directory that lists the same twice running, a name
  it lists that can be looked up, a file that reads no more than its size, a
  walk of it all that changes nothing leaving the kernel's heap as it found
  it, nothing read or written once it is unmounted;
- `ext4mkfs`, `ext4` and `ext4bad`: images of ext4's judge's mkfs -- made
  and nothing more, for `--e2fsck` to judge; mounted read-only by the ext4
  driver, with the disk's every write a finding, and worked through the VFS,
  every answer the model's and the whole tree read back at the end, a
  journal holding the newest copies of blocks whose homes are noise read
  through so that the tree is still the one mkfs was given, and orphans
  mkfs left on the list -- a file of no links named nowhere, a file cut
  short, its extents and its last block's old bytes running on past the cut
  -- read as the release would leave them; and damaged -- a
  byte or three of the metadata at the start of the first groups, or of
  anything else -- refused, or mounted and walked as ext2bad's are, never a
  panic, a write or a leak;
- `ext4rw` (`targets/ext4rw.rs`): the same images mounted for writing -- a
  planted journal replayed by the mount, planted orphans released by it,
  now and then a disk all but full, a file of unwritten extents (what
  fallocate leaves a disk's image) over every free block but a few, held as
  an image and written into and discarded from -- and worked as the VFS's callers
  work a filesystem: files and directories made, a directory grown by
  dozens of entries, renames and moves, removes, recursive ones among them,
  now and then a name only the disk has (refused) and an htree directory
  written into (its index dropped); files opened, written, read, sought,
  grown and cut through their handles and by path, written whole; disks'
  images held, read, written and discarded off the VFS's lock -- some
  writes aimed at blocks mkfs left unwritten -- and files of two names, one
  held as an image while the other is opened for writing (refused);
  unmounted and mounted again midway. Every answer is the model's -- but a
  refusal for want of an inode or a block, which the model does not count,
  and a punch is never refused for one on a disk that had what ext4 keeps
  for a punch free when it was mounted, however full it is now -- one mkfs
  left with less may refuse it, and must leave the range as it was with an
  end of it given back -- and the
  image a clean unmount leaves must be clean to the judge, nothing left to
  replay or release, and the model's tree. A remove or a cut of a file of
  more extents than a transaction gives back goes through the orphan list,
  so that a picture taken between its transactions holds an orphan, which
  the judge's replay releases as e2fsck does and the kernel's replay of the
  picture must release too, leaving none. With the power cut at
  chosen requests -- and now and then before every request of one call --
  each picture is judged when its call is done: replayed by the judge's own
  reader and clean to it, its tree what the call may leave -- the tree
  before it or after it; for a remove, a node at a time, the tree before
  less some of what was under the path; for a write, every sector of the
  file (and its other names) what it held or what it was given, its size
  between, and nothing of any other file's -- and then mounted by the
  kernel, on a disk of its own beside the one under test, read-only (the
  journal read through) and for writing (replayed), both holding the
  judge's tree, the image that unmount leaves clean -- a picture whose
  superblock a checkpoint tore across its two sectors among them, which
  only the journal's copy makes sound. With requests failing,
  no call may succeed that the model does not have, nothing may be written
  in a mount after its first failed write of anything but a file's data or
  a disk image's own flush (the journal aborted) -- a file's data block that
  does not write fails its call alone, which the judge's map of every block
  tells apart, and a disk's image goes on writing its file's blocks and
  flushing off the VFS's lock whatever its filesystem's state -- and
  the image, replayed, must be what the call that met the failure may
  leave, between the tree before it and the tree it was asked for. A filesystem whose journal is 64-bit where the filesystem is not
  (as `resize2fs -s` leaves one) is among the images: every reader of a
  revoke record must take its width from the journal. `--e2fsck` holds the
  clean images to e2fsck -fn and their tree to debugfs's rdump, and every
  picture, replayed by e2fsck alone (`-E journal_only` without `-f`, which
  would go on to repair), to e2fsck -fn -- but for the superblock's free
  counts after a crash, which Linux keeps lazily too, and a lost+found the
  calls removed, which only e2fsck's convention wants;
- `ext4sys` (`targets/ext4sys.rs`): an image whose one file's extent names
  the filesystem's own blocks -- an inode table, the descriptors, the
  journal's log -- its inode's checksum put right, mounted for writing:
  a write to the file, a read, a cut, a disk's image of it read and written,
  a remove, each refused; the file beside it read as it was; the blocks the
  damaged file names left as they were by the time of the unmount; and the
  filesystem written to after, mounted again. The driver once trusted a
  file's own extent, and the control -- the system zone not held to -- is
  found at the first input, its write going over the inode table;
- `vfs`: a ramfs at the root held to the model, and procfs, ext2 and
  ramfs mounted beside it -- an ext2 asked of a disk that carries nothing
  refused -- and taken down again at paths nested,
  doubled, without a slash and too long; tasks of their own at work on those
  meanwhile, their disks now and then slow enough that a task's call sleeps
  on one holding its mount's lock -- and a call on the root through before
  it wakes, which a VFS of one lock never lets be (reached in a sixth of the
  gate's inputs; 0 on the VFS before the lock a mount, the control); the C
  ABI (`kernel_vfs_*`) handed words that are open files,
  were, and never were; and the file ABI (`kernel_file_*`) a module keeps its
  configuration through, each call held to what its composition of VFS
  calls does to the model -- a directory read an entry at a time
  (`kernel_dir_entry`, now and then with too little room for a name) to the
  model's entries and nothing else, an append to the end of a file made if
  it is missing, a rename, as hv's VMs in their folders use them. A control
  run whose append wrote at the start of the file was found at the 47th
  input. A mount taken exactly when its path is free and
  well formed, its device nobody else's and there is room; an unmount
  refused while anything is open on it; a rename between mounts refused;
  and the disks' filesystems clean after `unmount_all`. Which file an open
  file is (`kernel_file_mount` and `kernel_file_key`, what a guest's disk
  claims its image by): one for two handles on one path and two for two paths, on every
  filesystem the model holds (`Fs::compare`, so ext2's target too); two
  mounts for files on two, whatever their inodes, which two mounts may
  number alike; and a word that is no open file told so. A
  control run with the node's key left out of a ramfs file's identity
  failed at the first input, and one with the mount left out at the 41st;
- `shell`: the storage commands the console, the UDP shell and SSH run --
  `mount`, `cp -r`, `find`, `fstest`, `diskwrite` and the rest -- with
  arguments of every kind, after `fstest` has passed on ramfs and on two
  ext2s of their own block sizes as each came; every disk no command wrote to raw clean at the end;
- `rootfs`: `root=` none, `auto`, a device, a label, a UUID, over disks and
  partitions carrying an ext2 labelled `nos` or not, something that
  only looks like ext2, or nothing -- now and then two partitions of one UUID,
  as a copy's would be; what is mounted where held to what `rootfs.rs`
  documents -- a label or a UUID two devices carry taking neither, since one
  may be a guest's -- and what it mounted for writing clean after the
  shutdown's unmount. The rule's control, the first match taken as before,
  fails at the 137th input;
- `format` (`targets/format.rs`): the shell's `format` on a disk of any
  size the command may meet and either sector size, holding nothing, noise,
  an ext2 or ext4 of the fuzzer's own, or an MBR with one of those in its
  partition -- the disk or the partition named, `force` or not, labels and
  block sizes right and wrong, what is on it mounted first now and then, its
  requests failing and the power going at chosen ones, or on a small disk at
  every one. A refusal must write nothing: no filesystem it does not make,
  no option it does not take, nothing over a device in use, a disk of
  partitions or -- without `force` -- a device holding anything; a write
  that failed must be said. What it says it made must be what the judges of
  `image/` find, clean and empty but for lost+found: the blocks, block size,
  groups, inodes and free blocks it said, the UUID it said and a random
  one's, the label and block size asked for, nothing left in the device's
  cache; the kernel's driver then mounts it, its `fstest` passes, and the
  unmount leaves it clean. A partition's making must leave every byte
  outside the partition as it was. And every picture -- each power cut's,
  and the end of a making cut short -- must hold the superblock that was
  there, none, or the whole new filesystem, never a superblock over half of
  one. Its controls: the primary superblock written first fails within three
  inputs of each seed, barriers that do not flush within two, and a label
  given twice leaving the tail of the first within fifteen.

Each finding comes with the seed and iteration that make it again and the
input in `out/fs-fuzz/findings/` -- `--replay TARGET FILE`, with `--trace` for
every request each disk takes and every call the target makes.
`FS_FUZZ_STATS=1` adds how often each target reached each state it counts.
With no arguments every target runs its own number of inputs from seed 1,
the same every time: the gate, about two minutes, which CI runs on its x86-64
leg. A campaign is `--seed N --seconds S`.

What it found on its first day, each fixed:

- the device table bounded a partition's requests and not a whole disk's:
  a filesystem's block number past the end of its disk went to the driver,
  every driver trusted to refuse it; now nothing outside a device reaches
  its driver, the asynchronous path included;
- the GPT reader added to an LBA and a count that come off the disk
  without a check -- a table on a disk that claims 2^64 sectors overflowed
  them, a panic in a `RUSTUB=1` kernel -- and the claims kept a disk and its
  partitions apart but not two partitions a table has overlap: two writers
  on the same sectors;
- the disk log, on a device whose sectors do not divide a page -- 520 bytes,
  as some disks have -- ran a full batch past its buffer: a panic at boot
  with `disklog=on`; and a header's boot count at its top overflowed;
- ext2 took the superblock's geometry on trust: a block count past the
  device, an inode count that is not its groups', a group's inode table
  running out of the filesystem -- a crafted image overflowed the group
  count, a panic, and could point the driver outside its device;
- ext2's `create_dir` wrote the new directory's inode before the bitmap that
  marks its block, and `create_file` before its inode's bit: a power cut
  between leaves an inode in use on bits the bitmap has free, for the next
  create to take. And a create undone after an error freed the bits under an
  inode already on disk in use -- a block two files come to share. The bits
  now go down first, and an undo writes the inode deleted before it frees
  anything, and leaves a leak when it cannot;
- an ext2 write that ran out of room part way left the blocks it had taken
  past the end of the file, which e2fsck calls a bad size; one past what the
  blocks map went in part way, and a truncate past it made a file that
  could not be read to its end. A write past the end is refused whole now,
  and one that fails gives back what it took and leaves the file its size;
- ext2's remove was a recursion capped at 32 levels: ext2 made a tree it
  could not remove. It is a walk now, with no limit but the tree's;
- an ext2 directory whose load failed part way -- a block past what the
  driver maps -- kept what it had read so far and read it again at every
  look: every `ls` of it grew the tree, for good. A load is all or nothing
  now; `ext2bad` walks every image three times and holds the
  kernel's heap to what it was after the second;
- names with a NUL in them went onto disk, which e2fsck calls illegal; the
  VFS refuses them;
- `fstest / 1` underflowed the big file's patch offset -- a panic from any
  shell.

And when ext2 learnt files past 4 GiB (the triple indirect block,
`large_file`, a walk of a truncate that steps over holes), each fixed:

- a write that ran out of room on the block after the last one wrote the
  last block's tail past the end of the file -- the batch it stopped short
  holds what was meant for it -- and, having allocated nothing, gave nothing
  back: the file growing over that tail later showed the bytes of a write
  that had failed, where it should read zeros. Found once the target filled
  the filesystem first; a failed write that would have grown the file now
  cuts it back to its size, which zeroes that tail;
- a damaged bitmap that has the filesystem's own blocks free -- the boot
  block, a copy of the superblock, a bitmap, the inode table -- had the
  allocator hand them out, and the first indirect block made written over
  the superblock. They are marked in use and stepped over now, as Linux's
  ext2 does a block "in system zone";
- and one in reading the code rather than by an input: a cached indirect
  block freed by a truncate kept the flag that said pointers were waiting
  in it, which a commit that failed at its flush leaves set, and the next
  flush of the caches would have written it to block 0 -- with 4 KiB
  blocks, the superblock. Freeing one forgets it whole now, a cache that
  holds no block is never written, and an attempt is a panic naming it.

A control run of each of five mistakes in the new code -- `large_file`
never set, an empty triple indirect block kept, a size's high word not
written, a run of holes stepped over a block too far, a truncate walked from
the old size rather than the tree's end -- was found by the gate's own
inputs, at the 31st, 16th, 3rd, 13th and 40th.

What it leaves, known: after a device error in the middle of a create or a
rename, the directory in memory may disagree with the one on disk until the
next mount, and a later create of the same name can leave that name in the
directory twice. Nothing is lost or shared -- the first is the one looked
up, and e2fsck renames the other -- so the checks count it unclean.

### `cpp-fuzz.py` -- what the C++ is handed, and the memory under everything, fuzzed

The kernel's C++ reads things nobody checked before it: the device tree the
arm64 boot is handed, the Multiboot2 information GRUB hands the x86 one and
the ACPI tables its firmware leaves in memory, the firmware's memory map,
the command line, the GRUB environment block `grubenv` reads off `/boot`, a
module's `.ko`. Most of it is read before a headless machine has any
console at all. And under everything the
kernel does is its C++ memory management, whose error paths -- a table that
cannot be allocated half way down a walk, a heap with no page left -- no boot
takes. A read past an end, an index out of range or a wrap in any of them is
a boot that dies without a word, or a page handed out that something still
uses; and in C++ the language does not stop it where it happens. The
scheduler's rules are here too: one reckoned wrong is a task that waits,
which nothing reports. So
`fuzz/cpp` builds each of them for the host from the kernel's own sources,
as they are, under the address and undefined-behaviour sanitizers, over
stand-ins for the kernel around them (`fuzz/cpp/common/`: the tracer, the
panic path -- a panic is a finding --, the locks, which hold the kernel's
rules -- one taken twice, one let go of when not held, an allocation with one
held are findings --, the heap with each block counted and able to fail) and
a HAL of the host's own (`fuzz/cpp/host/`), which comes before `src/base` on
the include path and takes the place of each arch's inline one: no
privileged instruction, and the MMU's hooks -- a TLB flush, an entry made
valid -- where a target that models the MMU sees what the kernel did to its
tables. `fuzz/cpp/Makefile` lists what each target is built from, as the
kernel's Makefile lists what it is.

The targets, each holding what the code makes of its input to a model of
what its header says it does:

- `fdt`: device trees of QEMU virt's shape -- memory, chosen, PSCI, a GICv3
  and its ITS, PCIe, the PL011 and PL031, virtio-mmio, the timer, the CPUs --
  with cell counts, property lengths, strings, interrupts and windows of
  every kind, and damaged: nesting that ends more nodes than it began,
  lengths and name offsets that lie, a header that does, a blob cut short.
  A sound one's board -- what `Board::setup` took of each node, the
  refusals counted, the fallbacks for the rest -- is held to a reader of the
  tree written from `board.h`; any board to what the kernel acts on:
  windows the linear map reaches, interrupts it can take, bus numbers that
  are bus numbers;
- `memmap`: firmware maps with regions that overlap, are empty, or run past
  the physical address space, and the questions the page allocator asks of
  them -- the free-page scan, `is_reserved`, `is_usable_ram`, the RAM totals --
  answered naively, page by page, in 128-bit arithmetic that cannot wrap;
- `grubenv`: blocks grub-editenv could have written, damaged, and bytes;
  `set`, `unset`, `get` and `for_each` held to the model's variables, room
  counted to the byte -- and on any block, a `set` that says yes is what `get`
  gives back and one that says no changes nothing;
- `module`: `.ko` files as the build makes them, for x86-64 and for arm64 --
  segments, dynamic symbols and relocations, the function table, the kmod
  header -- made to say what the loader must refuse, and damaged, handed to
  the loader's own `stage` (all of `load` up to the module's init, which a
  fuzzer does not run). A sound one's image is held to the segments and the
  relocations, byte for byte; its pages' permissions, its imports, its
  function table and the lookups a backtrace makes through it to the file.
  Nothing is written past the image (ASan poisons a page either side),
  nothing is asked to be writable and executable, and whatever the loader
  refused, or failed to allocate, leaves every page, mapping and heap block
  as it found them;
- `pagetable`: the page tables and the physical page allocator past
  `setup`, whose state is set up as `setup` leaves it (it runs on the
  bootstrap linear map, which a host process has not got) over a physical
  memory of the host's and an MMU emulated where the kernel reaches memory
  through it, the tmp_map window: a slot is loaded with the page its entry
  maps when the kernel flushes it or makes the entry valid, the page gets the
  slot's contents back when it is unmapped, and an unmapped slot is poisoned
  for ASan. Pages allocated one at a time and in runs, mapped in every form,
  unmapped, protected, temp-mapped and written through, frames copied
  through a CPU's slot, MMIO mapped -- on machines taken to their last pages
  and in a window with two slots left -- are held to `page_table.h`'s
  rules: every page's reference count, every translation, every page free,
  held, a table or the machine's and never two, pages handed out zeroed, a
  map that fails leaving nothing of its own, no tmp_map slot left behind.
  A machine of more than 4 MiB may reserve a huge pool (`hugepages=`) right
  after its free list, as the boot does, and hand its 2 MiB blocks out and
  back: the highest free blocks taken and the pages kept left free, a block
  of the pool never on the free list nor anyone else's, zeroed each time it
  is handed out, and the pool's counts its bitmaps'. Free pages are zeroed
  ahead as the zeroing tasks do it (`zero_free_page`, `zero_free_huge_page`),
  a step or a block's worth at a time: the page zeroed the other list's
  tail, put at the zeroed list's head and reading as zeros, a pool block
  known zero once its 512th page is done; every free page on exactly one of
  the two lists, marked iff on the zeroed one, no page not free marked, and
  every page and block known zero still zeros while free; a guest's
  allocation (`alloc_page_keeping`, `alloc_huge_frame`) taking a page or
  block known zero before any other, the kernel's (`alloc_page`) one that is
  not, and with no block known zero the block the zeroing is part way
  through taken next, its pages not yet done zeroed by the allocation; a
  quarter of the machines with `zerofree=verify`'s reading on. A
  zeroing that leaves a page as it was is a finding at the first input, and
  a guest's allocation that does not prefer the zeroed list at the second;
- `heap`: `mm::alloc` and `mm::free`, the pools, the page runs and the three
  ways a driver maps pages, over a page table as `page_table.h` promises it
  and a VA region of the host's -- mapped VA is memory, the rest poisoned --
  with pages and maps failing on the input's word: every block the size
  asked for, aligned, mapped and nobody else's, what was written into it
  there when it is freed, nothing refused while there is memory and VA to
  give, and every page back but the bitmaps' once everything is freed;
- `cmdline`: command lines of the parameters the kernel takes, with values
  of every kind -- numbers at and past their limits, addresses that are
  not, labels and UUIDs --, words it does not know, strays, and lines longer
  than the buffer, cut anywhere: every getter held to a reader of
  `parameters.h`, and a parameter the buffer's end cuts through left out
  whole, never read as the shorter one it would be;
- `multiboot`: Multiboot2 infos as GRUB makes them -- memory maps of every
  entry size, and one longer than the map holds; the command line; both
  RSDP tags; the framebuffer; modules, `ucode` among other names and names
  the tag cuts short, of ranges empty or not; the tags this kernel skips --
  and damaged: sizes that lie, a tag past the info's end, no end tag. What
  `parse_multi_boot_info` leaves -- the memory map region by region, the
  command line, the RSDP's copy, the framebuffer, the microcode module's
  range -- is held to the tags read in order, the map's reserved regions
  before its usable RAM;
- `ucode`: files of Intel's microcode updates (`arch/x86_64/microcode_file.cpp`),
  what GRUB loads from `/boot/ucode` and the BSP chooses its CPU's update
  from, built update by update as Intel lays one out -- the original
  2048-byte format, data and total sizes of their own, an extended signature
  table naming other CPUs and platforms, every checksum made right -- for the
  CPU the input makes up or for another, of revisions above and below the
  one it runs; then one update made unsound in a way the SDM rules out, bytes
  after the last, or bytes of anything written over it. Over a file the
  generator made, `choose` is held to a model written from the SDM (Vol. 3A,
  10.11.1): the newest update for the CPU above its revision, of those before
  the first unsound one; over any file, a choice is an update inside the
  file, sound by the model's reading of one, for the CPU and newer than what
  it runs;
- `acpi`: the tables in a physical memory of the input's, sparse, zeros
  where nothing was put: an RSDP in the Multiboot tag's copy, the EBDA or
  the BIOS area, among decoys; an RSDT or an XSDT; and tables below and
  above 4 GiB, across page ends and over each other -- a MADT of every entry
  kind, FADTs, HPET tables and WDATs of every length and each side of the
  fields the kernel reads, the SSDTs and the rest a firmware lists, seconds
  of the kinds read among them --, now and then damaged anywhere. The tmp_map
  window is emulated to the slot: a mapping is a copy of the pages it maps
  between poisoned ones, a table's mapping is poisoned outside the table,
  so that a read past a table's length is a report and not a read of its
  neighbour, and the window refuses a mapping where the input says. A
  machine is held to a reader of it written from `acpi.h` -- the RSDP the
  search finds, the first table of each kind that can be mapped, the CPUs,
  the IO-APIC the driver can drive, the overrides, the FADT's, HPET's and
  WDAT's fields, whether it boots at all; and any machine, the window cut
  short or not, to this: nothing mapped past the physical address space or
  longer than a table may be, no more of the window held at once than
  `acpi.h` bounds, and when the parse is done nothing held but the two
  APICs' pages -- and not those if it failed;
- `format`: `stdlib::vsn_printf`, which every traced line, panic and
  command's output goes through, against the host's own `vsnprintf` --
  what `PRINTF_FORMAT`'s check rests on. Random formats of what it
  implements (`d u x X c s`, `%%`, the flags `0` and `-`, widths, the length
  modifiers `hh h l ll z`) among literal text, over two fixed lists of
  twelve arguments -- ints, longs and strings side by side, some in
  registers and some on the stack -- each conversion matching the type at
  its place, and each int made by cutting a long at the call, so that a
  reader taking an int as a long reads the other half of its slot: what
  fits must be the host's output byte for byte and its length, and what
  does not, the host's output cut where `vsn_printf` cuts it and marked with
  `...`;
- `eevdf`: the scheduler's rules (`kernel/eevdf.cpp`): one CPU's queue and
  a handful of tasks of random weights and slices, some that never sleep and
  some that sleep, wake, yield and move away and back -- and one that joins
  on the CPU, as the idle task does when it starts to run something -- with
  time going by a tick, a slice's end or a random quantum at a time, and
  every decision made in `TaskQueue::schedule`'s order: the next picked while
  the last is still current, the last gone or back on the list, the next
  taken. Every step is held to a model of EEVDF computed from scratch in 128
  bits: the one picked is eligible and no eligible one's deadline is sooner;
  a task that joins has the lag it left with, clamped, within the rounding of
  its inflation; one that leaves the lag V gives it, V counting everything
  on the queue, the one picked next among them; one that ran is charged what
  its weight says; every task on the queue is on its list or current, never
  both or neither, and the list's count is the model's. A third of the
  inputs are tasks that never sleep, run long enough for slices and quanta to
  even out: each must have had the CPU in proportion to its weight. A `leave`
  that reckoned V without the leaver in it failed the first input;
- `lineedit`: the console shell's line editor (`kernel/line_editor.cpp`),
  what a terminal on the serial port types at it: any byte, weighted to the
  keys it acts on and a few letters, so that lines repeat and a search finds
  them, with runs of one key past the longest line and the history's end.
  Every key is held to a model in `std::string`: the line Enter hands the
  shell, and what the screen shows -- built from the editor's output, text
  and Backspace alone, as the console's is -- the prompt and the line, or
  the search's prompt, its query and what it found, exactly; a Backspace on
  an empty row, which erases the line above, is a finding too. A redraw one
  Backspace short failed the first input, and a search that kept no line
  to come back to the tenth;
- `trace`: the tracepoints' ring (`kernel/trace_ring.cpp`) held to a model:
  `init` taking a power of two of chunks and keeping nothing of one that
  failed for memory, every read the record the model wrote or none; and a
  writer thread against a reader, every record kept the one its index says.
  A writer that did not clear the sequence word first failed the 103rd
  input, a torn record;
- `tracewin`: what the shell's readers make of the rings
  (`kernel/trace_window.cpp`), on rings of `trace_ring.cpp`'s own. A
  schedule is played out on up to six CPUs -- wakeups onto other CPUs'
  queues, some a deadline's and late, switches, displacements, blocks, idle
  and IPI records, a `trace clear`, rings that wrap -- and the window read
  as `trace latency` reads it, with a size and an event filter of the
  input's, is held to what the schedule did: each CPU's part the newest
  records its ring holds that the filter keeps; nothing the CPU wrote
  missing from where the part says it is whole; the merge in time order;
  every wait paired one the schedule had, none it had left out -- through a
  field filter, the first switch to the task the window kept, which a filter
  that dropped the one that answered the wakeup makes a later one. Then the
  heapsort against `std::sort` on orders made to be a quicksort's worst,
  the five longest, a duration's parse, and a ring's window read while a
  thread writes it. The quicksort it replaced is a panic at the 48th input,
  and a window whose start is ignored -- waits paired across a switch a ring
  lost -- fails the 44th.

The runner (`fuzz/cpp/common/runner.cpp`) is `fuzz/common`'s, but for one
thing: inputs run in batches, one after another in a process forked for the
batch, the target's `Reset` putting the kernel's statics back between them
-- a fork of a process under ASan costs macOS fifteen milliseconds. A
finding ends its batch and is run again alone in a fresh process, whose
report is the one given; one that does not come back alone is `Reset`'s own
bug and says so. Each finding comes with the seed and iteration that make it
again and the input in `out/cpp-fuzz/findings/` -- `--replay TARGET FILE`,
with `--trace` for the kernel's trace lines. `CPP_FUZZ_STATS=1` adds how
often each target reached each state it counts. With no arguments every
target runs its own number of inputs from seed 1: the gate, a minute and a half,
which CI runs on its arm64 leg. It needs clang and make on the host, and
nothing else; the programs of each host are built in a directory of their
own (`out/cpp-fuzz/<system>-<machine>/`), since a Mac and a container share
the tree.

What it found on its first day, each fixed:

- the device tree reader let an `END_NODE` with no node open take the depth
  below the root, and the next node wrote its cell counts at index -1 of the
  walker's stacks: a write into `.bss` outside them, from the first thing
  the arm64 boot reads. And `Board::setup` read every `reg` and `interrupts`
  by the tree's own cell counts and not the property's length -- a count of
  3 or 2^32, or a property shorter than it said, read past the blob -- and
  took `bootargs` without its NUL. Properties are read through `Fdt::Prop`
  now, every read bounded by its length, a third cell refused;
- `Board` took whatever the tree said and the kernel then acted on it: an
  interrupt number the arm64 interrupt table (256 INTIDs, an u8 at
  registration) would cut to another, a device window past the linear map,
  bus numbers truncated to u8. What the kernel cannot act on is refused now,
  counted, and said at boot; and the UART's and the RTC's interrupts fall
  back on their own, where one the tree placed with no interrupt was set up
  on INTID 0, an SGI. The blob is held to the 2 MiB the boot protocol
  allows;
- the memory map kept a region running past 2^64 as it was, and every
  question about it wrapped: a reserved region hidden from the free-page
  scan, its pages handed out. A region is cut at 2^52, the physical address
  space of both arches, when it is added, empty ones dropped -- one of those
  counted as overlapping by `is_reserved` and not by the scan;
- the GRUB environment block's `for_each` read a line with no `=` into the
  next line's name, where `set` found that line by itself: `set` said yes
  and `get` -- and GRUB -- read something else. `set` and `unset` also wrote
  blocks `for_each` then called malformed, and `unset` one with no newline
  before the padding. The two ways of reading the block share one parser
  now, and `set` and `unset` refuse a block it rejects, as `grub_env.h`
  said they did;
- the heap's pools allocated a page with their spin lock held, interrupts
  off: when mapping it failed, the page allocator shot down every CPU's TLB
  and waited for them, and a CPU spinning on that pool's lock never answers.
  The page is allocated with the lock let go of now, as `Pool::free` already
  gave one back.

And two loud ends made plainer: with no tmp_map slot free, `zero_page` and an
unmap's walk -- `map_range_locked`'s undo among them, which would otherwise
leave pages mapped behind a map that said no -- panic saying so, where one
was a bare `BUG_ON` and the other returned without a word. No walk needs more
than two slots, which `pagetable` checks.

And when the x86 boot's own readers were added, each fixed:

- Multiboot2: the walk read a tag's type before it knew the tag's header was
  inside the info -- an info with no end tag read past its end. And a
  firmware map longer than the memory map's 64 regions was a panic before
  any console a headless machine has: the map holds 128 now, what Linux's
  boot protocol passes, and a map longer than that is not a boot that stops
  -- its reserved regions go in first, and the usable RAM past what leaves
  room for the kernel's own regions is left out, and said;
- ACPI: every table the root listed stayed mapped in the tmp_map window for
  good -- the EX44's 25, of a window of 448 slots every page allocation needs
  one of -- where four are read. The four are mapped while `Parse` reads them
  and let go of after, and so is the root, whichever way it ends; what is
  kept is the values, and the two APICs' pages. A table with a length short
  of its header, or one the window could not map, failed the whole parse --
  a panic at boot -- where only the MADT is needed: a table not to be mapped
  is skipped now, and said. Nothing bounded a table's length -- 4 GiB failed
  the boot, a few hundred pages would have held the window --, and an XSDT
  entry past the physical address space was mapped, its bits above 51
  landing in the PTE's flags: a table longer than 64 KiB, or out of the
  address space, is not mapped. The MADT was read past a table shorter than
  its own header; each IO-APIC listed was mapped over the one before, the
  last kept and the others' slots leaked -- on the AX41, whose MADT lists
  the FCH's at GSI 0 first and the GNB's past it second, the legacy IRQs, the
  HPET's tick among them, were set up on pins of the GNB's that none of
  those lines reaches ([Real hardware](real-hardware.md)) --, where the
  driver can drive the one at GSI 0; an APIC whose registers would run past
  its page, which put the drivers' accesses in the window's next slot, is
  refused; a MADT naming no IO-APIC booted on to a fault at address 0 in
  its driver, where it fails the parse, saying so. More than 64 interrupt
  source overrides failed the boot, a second for an IRQ took a slot nothing
  read, one to a GSI past 255 was cut to a byte by the interrupt layer: the
  first for an IRQ is taken, and the rest said.
- the command line: `Parameters`'s constructor left two members unset --
  zeros in the kernel only because the object's storage is static.

And when the formatter was put under the compiler's check: `vsn_printf`
read every integer conversion as a 64-bit slot, whatever the argument was,
and nothing checked a call -- the convention was to cast each argument to
`ulong`. The ones that were not were read with whatever the other half of
their register or stack slot held: the trace level in every `trace` (an
int under `%u`), and a few `%d`s and `%u`s of ints. `vsn_printf` reads C's
printf now, the length modifiers included; every printf-like function
carries `PRINTF_FORMAT`, the build has `-Wformat=2`, and 924 conversions
were rewritten to say what they print -- `%lu`, and `%lX` for the addresses
passed as `ulong` under `%p`, which prints what `%p` printed. Two formats
no check could see, and which the new reading would have misread, were
found by `-Wformat-nonliteral` and the template it cannot look through:
`profile`'s usage, a variable, and the idle and softirq tasks' names,
handed through `talloc<Task>` -- `"idle%u"` over a `ulong`. The first is a
constant the check reads, and a `Task` takes a name rather than a
format. The boot log is
the same line for line on both arches, digits aside. And `MemoryMap`'s
region hint, which `memcheck` read and wrote with no lock while other CPUs
mapped, is an `Atomic`.

What it does not reach yet: `PageTable::setup` and the free list's build,
which run on the bootstrap linear map; anything that needs a second CPU;
and the block allocator under the dmesg ring (`mm/block_allocator.cpp`),
which every traced line takes a message from -- though not, as this page
used to say, something nothing uses. The legacy
search finds an RSDP only whole within a page -- the kernel maps the BIOS
area a page at a time --, which the reader holds it to: the Multiboot tag,
GRUB's copy, is the path every machine this kernel boots on takes.

### `hv-test.py` -- the extension turned on and off again, and guests under it

The [hypervisor](hypervisor.md) is a module, and the one piece of state it
takes does not belong to it: `EFER.SVME` and `MSR_VM_HSAVE_PA` on AMD,
`CR4.VMXE` and VMX root operation on Intel are the CPU's, not a task's, and
they outlive an `rmmod` that forgets them. What is left behind then is worse
than a leak -- the page the CPU was told to save host state into has been
freed and handed to somebody else, and the code that would have turned it
off has been unmapped -- and nothing in the kernel would say so.

So the gate is a load, a turn-on for every CPU, an `rmmod` that is *not*
preceded by an `hv off`, and a load again; and the module answers "which
CPUs is it on for" by sending each CPU an IPI that reads the register,
rather than by reading its own bookkeeping, which is what makes the second
load able to see what the first unload left. It also checks that `hv info`
reports nested paging, which a guest cannot do without, that one named CPU
can be turned on without the others, and that a second `insmod`, a CPU that
does not exist, a word that is not a subcommand and a guest that does not
exist are each refused. Its last loads are of hv's one parameter: a load
with none keeps the VMs' folders in `/vms`, a parameter hv has not got and
a `vms=` that is no absolute path refuse the load, each saying why, and
`vms=/data/vms/` is kept -- its slash let go -- as `hv` and `hv vms` must
say. On arm64, which runs no Linux guest yet, a parameter is refused. On
x86-64 it loads hv once more and `hv boot`s hvboot with no disk: a PVH
image ([A guest's own kernel](hypervisor.md#a-guests-own-kernel-pvh-and-hvboot)),
started at its 32-bit entry with paging off -- under VT-x, by unrestricted
guest -- which has to say it read the start info hv laid out (its RAM, and
its command line), that it has no disk, and turn the guest off through its
ACPI. Last, on both architectures, `hv convert`
([A disk from a cloud's image](hypervisor.md#a-disk-from-a-clouds-image)):
qcow2 images qemu-img made of a disk the test makes -- compressed, version
3, and version 2 of 4 KiB clusters -- converted on nos's own ext2 and held
to the disk by the SHA-256 `sha256` prints; a second conversion over the
first refused; an image cut short refused as one, with nothing of it left
behind; a disk that is raw already refused, saying so. The host needs
qemu-img for it. And on x86-64, whose nos it gives a NIC on QEMU's user
network for it, `hv create <name> <url>`
([A VM of a cloud's image](hypervisor.md#a-vm-of-a-clouds-image)) from a web
server of the test's own at 10.0.2.2: the compressed image downloaded,
made a disk, grown to `size=` -- the disk held to the image's grown with
zeros by its SHA-256 -- its folder holding the disk, the seed and a
vm.toml that boots it by hvboot, the download gone; a raw image taken as it
is; and a name that has a vm.toml, a URL with nothing at it (404), a file
that is no image and a `size=` under the image's each refused, the last
three leaving no folder behind. Then what is done to them
([Cloned, deleted, changed](hypervisor.md#cloned-deleted-changed)): the
first cloned, the clone's disk held to the source's by its SHA-256 and
written no more than it holds -- 2 MiB of its 16, the rest a hole -- its
folder the disk, a seed made anew, the user-data kept and a vm.toml with a
UUID of its own; a clone to a name taken refused; a forward added through
the `vm` provider (`mgmt call vm forward_add`) and found in the clone's
vm.toml, the file's other lines as they were and the old one kept beside it
as vm.toml.prev, then taken out; the clone made protected through the
provider's `config_set`, its delete refused without `force` and the
provider saying it is protected and off, then forced; the provider's list;
and the second deleted keeping its disks, which stay in a folder `hv vms`
no longer lists.

### `hv-device-test.py` -- a partition or a disk of nos's as a guest's disk

A guest's disk on a device of nos's goes past the filesystem altogether
([A device as a guest's disk](hypervisor.md#a-device-as-a-guests-disk)),
and what keeps it from wrecking something is all in the giving: the id that
finds the same device at the next boot, the claims, and the refusals. None
of that shows in a guest that boots -- a VM given the wrong drive boots
just as well, onto somebody's data -- so this gate looks at each:

- two boots of `nos.iso` over one root with two NVMe drives beside it: A,
  a GPT of four partitions the test writes itself (an ext4's superblock
  labelled `devtest`, zeros, an EFI System partition, bytes of 0xFF), and
  B, blank, each with a serial of its own;
- the first boot: `disks` gives each partition its GUID and B its model
  and serial; `hv devices` says what each holds -- the ext4 and its label,
  empty, an EFI System partition, data nos does not recognize, the root
  held by its mount; `hv create` refuses the EFI System partition outright
  and the ext4 without `force`, gives the empty partition to d1 and B to d3,
  each vm.toml naming its device by its id and never its name, and refuses
  the whole of A for overlapping d1's partition; `hv convert` refuses the
  0xFF partition for the bytes on it and pours a qcow2 image onto it with
  `force` -- read back on the host after, byte for byte the image's disk,
  its holes zeros over the 0xFF; hvboot booted on a partition by its name
  reads it and finds nothing to boot; d1 started holds its partition --
  `hv devices` says so, the shell's write is refused -- and stopped lets it
  go; a device attached to d3 with `force` and detached; a clone of d1
  refused;
- the second boot with the drives the other way round, as the EX44's two
  are from boot to boot: every id found where it was under the other name,
  d1 started on its partition by its id, `hv delete d1` leaving the device
  as it is.

x86-64, under KVM where the host has an extension and TCG otherwise (QEMU
9.2 or later, as `hv-test.py` wants): 15 s under TCG on an M4 Pro, a
`UBSAN=1 RUSTUB=1` build's as long; `--keep` keeps the drives and the
serial logs.

### `hvboot-test.py` -- what hvboot would boot, decided on the host

hvboot reads a guest's partition table, its ext2/3/4 /boot and its GRUB
configuration, and boots the entry GRUB would: get any of that wrong and
the guest boots something else -- an older kernel, the recovery entry --
with nothing to say so. `fuzz/hvboot` compiles that code, its own sources
(`src/crates/hypervisor/hvboot/src/{gpt,ext,grub,bzimage}.rs`, over extfmt and the
tree's `crc`), into a host
program, and the gate runs it over disks it makes: a GPT whose /boot is a
partition of its own (an XBOOTLDR, as Ubuntu's image has, beside an ESP), an
MBR whose /boot is on the root filesystem (as Debian's), and a disk that is
one filesystem whole; ext4 with extents, ext3, and ext2 with 1 KiB blocks,
whose 6 MiB kernel runs into the double-indirect blocks. On each, a
grub.cfg written as grub-mkconfig writes Ubuntu's -- `load_env`, the
`next_entry` conditional, `$menuentry_id_option`, `search --fs-uuid` in both
branches of an `if`, a submenu, the firmware entry inside an `if` -- and the
cases GRUB decides among: no grubenv, the first entry; `hvboot.entry=1>1`,
the submenu's recovery entry; grubenv's `next_entry=1>2`, an older kernel
with two initrds, joined at GRUB's 4-byte alignment; GRUB_DEFAULT=saved
with `saved_entry` naming a submenu's entry by both their ids; and a
`saved_entry` that names no entry, which boots the first and says so. Each
must give the entry, the command line (`BOOT_IMAGE=` first) and the kernel
and initrd bytes, read by hvboot's own reader, as they were written. The
partition tables are the script's own, written by the UEFI specification's
layout and CRCs; the filesystems are mke2fs's.

Then each disk is fuzzed: the sectors a clean run reads as metadata --
tables, superblocks, descriptors, inodes, extent and directory blocks,
grub.cfg -- have up to eight bytes changed at random, a disk at a time, and
the code is held to never panicking (overflow checks on), never reading
outside the disk and always ending. `--image <disk.raw>` does the same for
a distribution's image: Ubuntu 24.04's and Debian 13's were read so on
2026-10-24 -- their kernels and initrds as `debugfs` reads them, byte for
byte -- and fuzzed for 50,000 disks each, with no finding.

In between it runs guests. Before `hv on`, and bound to a CPU the extension
is off for, a guest is not run, and says which CPU. With it on everywhere,
every [built-in guest](hypervisor.md#the-built-in-guests) runs bound to the
first CPU and then to the last, and each has to have done what it was told
-- not only "ok": the report's own lines are checked, the nested page
fault's address, error code and instruction -- and that the state a report
of it would show is the guest's at the fault: a null DS it loaded without
an exit, which under VT-x, where an EPT violation reads only what
performing an access takes, is there only if the rest was read after it --
the port and CPUID counts, the
fifteen registers each way across the hypercall, a host interrupt count
above zero for the spin, for `asid` that the third VM was given an ASID
one of the first two had, after a generation ended, and each read its own
page, and for `mmio` that its fourteen stores and loads of a device in the
MMIO window -- an immediate of each size, REX's registers and a high byte,
`movzx` and `movsx` -- were each decoded and performed, the device holding
what was stored and every register what was loaded: the MMIO path
([MMIO](hypervisor.md#mmio)) on each backend, with no guest kernel to
depend on; and for `ioapic` that the IO-APIC sent five interrupts, three
of them levels each ended by EOI -- its serial port's pin taking two edges
and none while masked, then a level sent again at each EOI while the line
stayed up, its remote IRR set in service and clear after ([The
IO-APIC](hypervisor.md#the-io-apic)); for `apicv`, on the same machine in
x2APIC mode, that self-IPIs at and below CR8's class were held -- none
taken, both read in IRR -- and with CR8 0 taken by priority, 0x50 and then
0x30, with 0x70 nested in 0x30's handler, ISR read showing each in service;
that three levels from the IO-APIC were each ended by the x2APIC's EOI and
sent again while the line stayed up; that nothing was left in service; and
who did it: the host, injecting all six, under AMD-V or where the CPU has no
APIC virtualization, and the CPU -- `hv info`'s *APIC virtualization* --
delivering all six and taking their EOIs under VT-x, the guest stopped after
the three level-triggered ones alone ([Interrupts the CPU delivers
itself](hypervisor.md#interrupts-the-cpu-delivers-itself-vt-xs-apic-virtualization));
there `tpr`'s CR8 is the virtual APIC's VTPR and no `mov` of it exits; for
`pveoi`, on the same machine with its interrupts the host's to inject on
either backend and KVM's PV EOI on, that each handler's EOI went through
the guest's word where the host had set it and by the register where not
-- 0x50 alone and 0x70 nested through the word, 0x30's own by the
register, at whose exit 0x70's was done first, and 0x50 again by the
register with PV EOI off -- that the word was never set while 0x70 waited
in IRR, that nothing was left in service, and that the host did two EOIs
for the word and injected four interrupts; a run loop that never did the
EOI for the word, and one that did it while the guest still held the bit,
each failed it ([KVM's paravirtual
interface](hypervisor.md#kvms-paravirtual-interface-as-far-as-it-is-backed)); for
`posted`, two CPUs both running, that the first counted all thousand of the
fixed IPIs the second sent it while it spun -- and who handed them over:
where the CPU has posted interrupts (`hv info`) each was posted and the
notification sent for nine in ten at least, the first CPU taking them in
its guest and leaving it a tenth as many times at most, and elsewhere the
host; and for `debug`, through the run loop
a Linux guest's CPUs go through, that a single step and an `int1` -- each a
#DB the host intercepts -- reached the guest's own handler, the first with
DR6.BS and after the stepped instruction, the second after the ICEBP: a
user process's `stepi` once stopped its whole VM. Under VT-x the same --
its DR6 the one the exit said, VT-x writing none at a #DB that exits, and
its handler's `mov`s of DR6 answered by the host. And for
`cr4`, through the same loop, that the guest read back each CR4 it wrote --
MCE cleared, then PGE set -- while the CPU's CR4 kept MCE; that a bit its
CPUID does not offer -- PKE where it is not offered PKU, CET where it is,
which no guest is -- PAE cleared in long mode and bit 32 were each a
#GP with no error code, CR4 as it was; and that the host performed all
eleven of its accesses. The bit is PKE on a Zen 2 because the Zen 2
refuses a write of CET itself, before its intercept, as it does bit 32:
with CET, none of the three refusals reached the host there. On QEMU's TCG -- known by its hypervisor leaf's
signature -- which checks a read of any control register against CR0's
intercept and so stops no read of CR4, it holds the reads to the register
instead, MCE in it, and the host to its five writes; its VMCB must ask for
the reads either way. A control run with MCE kept nowhere -- neither
`hvarch` setting it at entry nor `set_cr4` -- failed there: the register it
read lost MCE at the first write. Under VT-x, which stops a guest at a
write of CR4 only where it changes a bit the host keeps, and answers its
reads from the read shadow, it holds the reads to the same CR4s, the CPU's
to MCE kept and VMXE, and the host to three writes performed -- MCE
cleared, PKE or CET, bit 32 -- and two refused: PAE cleared in long mode is the
CPU's own #GP, and PGE set its own write. And for `msr`, through the same loop,
that AMD's VIRT_SPEC_CTRL and FLUSH_CMD, which its CPUID does not offer,
AMD's LS_CFG, which the policy names for no CPU, and MTRRs turned on,
which nothing keeps, were each a #GP its own handler took -- in that
order, with no error code, and in the host's count of five reads, three
writes and four refusals; that MCG_CAP, MCG_STATUS and the PAT were
answered; and that CPUID told it of no VIRT_SSBD, no INVLPGB and no PMU,
of no basic leaf past 0x16, and of a hypervisor. Every MSR but the three segment bases comes to
the same policy on either backend, so it says the same under VT-x. A
control build whose policy did not name MCG_STATUS failed it, a fifth #GP
on both CPUs it ran on. And for `dr`, two VMs of one CPU each, one after
the other on the CPU the guests run on: that the second read 0 in DR0-DR3
as it started, where the first had left its own -- and where the CPU's
four were the guests', it read those -- that each read back the pattern it
wrote, and that each one's breakpoint, on a `ret` of its own, fired once,
with DR6.B0 and at the `ret`, which only its own DR0 made the CPU's before
its entry can do; then that every one of its fourteen `mov`s of them was
the host's to perform. An execution breakpoint, not a data one: QEMU
10.2's TCG never gets past a store a data breakpoint watches in a nested
guest -- it retries the store for good and takes no interrupt, so `hv run`
never comes back -- and a control build that left the four the CPU's hung
the same. The world switch before this failed it on both CPUs, the second
VM reading the first's breakpoint address and pattern. Without the load
before the entry, and with a load only when the guest's DR7 enabled a
breakpoint as it was entered -- which a guest then enables without an
exit -- the first VM's breakpoint never fired, on both CPUs. Under VT-x,
which stops a guest at every `mov` of a debug register, the same, with
eighteen `mov`s the host's: DR7 armed, and in the handler DR6 read and
cleared and DR7 turned off. And for `avx`, two CPUs of one guest whose tasks
are both on the CPU the guests run on, taking turns by IPI: that each found
YMM0-15 0 as it started, though the other's pattern was in the CPU's
registers; that at every one of its 64 turns it found its YMM registers,
MXCSR and x87 control word as it had left them -- its pattern one more in
every word each turn -- the host having put its registers back each time
the other had had the CPU, at least 64 times each; that its CPUID offered
XSAVE and AVX, and OSXSAVE only once its CR4 had it; that its XCR0 was 1 out
of reset and still after the five XSETBVs a CPU refuses, each a #GP, and 7
after its own; that its XSETBV with OSXSAVE clear was a #UD -- the host's
under QEMU's TCG, whose intercept comes before the CPU's checks, the CPU's
own under VT-x and on a Zen 2, 6 and 5 refused by the host accordingly;
and that leaf 0xD gave the x87, SSE and AVX, the area's sizes 576 and 832,
no XSAVES, and AVX's component at 576. Its second CPU turns its local APIC
on first: it comes out of INIT software-disabled, and a guest that waits
for an IPI there before it does waits for good. And for `tscaux`, two VMs
of one CPU each, one after the other on the CPU the guests run on: that
each was told RDTSCP, RDPID and INVPCID exactly as `hv info` says they run
in a guest, RDTSCP everywhere; that the second read TSC_AUX as 0 by RDTSCP,
RDPID and RDMSR as it started, where the first had left its own; that each
read back the value it wrote every way it has, RDTSCP's TSC between two
RDTSCs, and had a write of bit 32 refused, its value kept; that INVPCID's
four types ran and a fifth was a #GP where it is offered -- the EX44's
silicon -- and every one a #UD where not, QEMU's TCG and a Zen 2; and that
the run loop saw its three RDMSRs and two WRMSRs and stopped it at no
RDTSCP. And for `spec`, two VMs of one CPU each, one after the other on the
CPU the guests run on: that each was offered exactly the speculation
controls the host switches, as `hv info` says a guest has them -- none
under QEMU's TCG, where every access to SPEC_CTRL and PRED_CMD has to be a
#GP; that where SPEC_CTRL is offered the second read it as 0 as it
started, where the first had left its own, each read back the bits it
wrote, had bit 62 refused with its value kept, and ran under its bits over
the host's, the CPU's the host's alone after its run; that where IBPB is
offered each of the three it asked for was answered by a barrier before
its next entry; and that PRED_CMD's read and bit 1 were each a #GP. On the
AX41's Zen 2, under KVM, three control builds failed it: one that left
the guest's bits out of the CPU's (it ran under the host's 0x2, not 0x6),
one whose IBPB between two guest CPUs looked at the guest CPU alone and
not at the IBPBs it asked for (three asked, none answered), and one that
left the guest's bits in the host's after the exit. And for `pkru`, two CPUs
of one guest whose tasks are both on the CPU the guests run on, taking
turns by IPI as `avx`'s do: that each found RDPKRU a #UD with CR4.PKE
clear; and where PKU is offered -- `hv info` says so, and QEMU's TCG and
the EX44 have it, a Zen 2 not -- that each was told OSPKE only once its
CR4.PKE was set, that its XCR0 was its own, PKRU in the first's and not in
the second's, that leaf 0xD gave PKRU's component as the host has it and
the area's sizes for each XCR0, that PKRU was 0 as it started though the
other's was in the CPU's register, and that at every one of its 64 turns
it found its own as it left it, put back each time the other had had the
CPU -- whatever its XCR0 says of PKRU. A control build that never loaded a
guest's PKRU failed it under QEMU's TCG: the second CPU found the first's
as it started. After both rounds `hv` must count at least two
switches between guests a guest -- each guest's VM on CPU 0 and on CPU 3,
after another guest's -- each with an IBPB where the CPU offers one, which
QEMU's TCG does not, and `hv` must say so; on an AMD CPU a guest could
otherwise steer the host's branches or returns on -- the AX41's Zen 2,
under KVM -- that is the IBPB after every exit, issued at least once a
switch -- between two guest CPUs, two CPUs of one guest that share a CPU
among them, as `avx`'s do. And where the CPU has IA32_SPEC_CTRL every CPU's
register, read back, must be what the host runs with there: what `hv on`
set -- under VT-x, its IBRS; under AMD-V, STIBP, for which the machine's
cores have two threads each -- and none of a guest's bits left over it. Every guest ran through the run stub's clearing of the
registers and refill of the return stack after each exit. The unload that
follows, and the load after it, are then of a hypervisor that has run
guests on those CPUs.

`smp` is the one guest of two CPUs, its second on another host CPU than
the first ([More than one CPU](hypervisor.md#more-than-one-cpu)), and its
verdict is the guest's own record, read out of its memory: the second CPU,
started by INIT and a start-up IPI, came up in real mode and reached long
mode, and says it is x2APIC ID 1 and CPUID's 1; its IPI reached the first;
the first's one-shot APIC timer then ran out and interrupted it, and reads
0 after; and the counts agree -- one start, one IPI each way, one timer.
Under TCG it needs QEMU 9.2 or later, and the gate refuses an older one:
before 9.2, TCG did not put a guest with paging off through the nested
table at all, and the second CPU's real mode would have run on nos's own
memory.

Which backend it exercises is the host's. Under KVM, `-cpu host` gives the
guest the host CPU's own extension -- AMD-V on an AMD host, Intel VT-x on an
Intel one, nested, since nos is itself a KVM guest -- and TCG's `-cpu max`
gives AMD-V where there is no KVM, TCG having no VMX at all. So the Intel
backend is reached only through KVM and the AMD one three ways, and between
them both are covered. Three guests test a mechanism only one vendor has:
AMD-V's software VMCB check (`refused`), its ASID recycling (`asid`) and its
shadow task-priority register (`tpr`). Under VT-x the CPU refuses a bad VMCS
itself and there is no ASID (VPID is off), so those two check the Intel
equivalent instead -- a non-canonical guest RIP refused at entry, and three
VMs isolated by their own EPTs -- and `tpr`, the same bytes on both, is
stopped at each CR8 access and answered from a shadow, where AMD-V's `V_TPR`
never stops it; either way the host's own CR8 has to read 0 afterwards, on
the CPU the guest ran on, which is why the guests run bound to one. The
expected output is chosen from which extension `hv info` names. It was the
`refused` guest, run before any `hv on`, that first caught a VMX bug the
standalone runs could not: a VMCS `vmclear`ed at construction, where VMX may
be off, faults, so the clear had to move to the first entry.

What `asid` is for -- a translation a reused ASID should not have had --
can only show on a CPU that keeps translations between entries: TCG flushes
its own on every `vmrun`. Under TCG it checks the allocator's side, that the
third VM's ASID is one of the first two's and that a generation ended in
between; on the AX41 it checks the TLB's.

It has been shown to fail the way it is meant to, three ways. With
`disable_here` changed to report every CPU turned off and turn none of them
off -- the silent failure the unload round trip exists for -- six checks
fail, the second load's among them, and the module's own warnings name the
CPUs left on. With the run stub storing R8 into R9's slot, the hypercall
guest fails on both CPUs, naming R8 as 0. And with the stub's `vmload` of
the host's state taken out, the kernel panics at the first exit -- a page
fault at 0 in `hal::get_current_cpu_hw_id()`, the guest's GS base -- and the
gate says so. And `smp` fails two ways more: with the start-up IPI ignored,
the second CPU never enters, and the report shows the first halted where it
waits for the second's IPI; with the APIC timer never running out, the
first halts where it waits for the timer instead.

x86-64 boots the ISO with `-cpu max`: the default `qemu64` model reports SVM
without nested paging. It uses KVM only when the host CPU has AMD-V, since
`-cpu host` hands the guest the host's own extension and the guests run
under AMD-V; on an Intel host it is TCG. `--arch aarch64` checks the other
half -- that on a kernel running at EL1 the module loads, says a guest
cannot run here and names the exception level, and refuses `hv on` and
`hv run` rather than attempting them.

### `extfmt-test.py` -- the ext2/3/4 format, held to e2fsprogs

`src/crates/common/storage/extfmt` is the ext2/3/4 on-disk format as bytes -- views of the
superblock, the group descriptors, inodes, extent trees and block maps,
directory and extended attribute blocks -- and every checksum metadata_csum,
and gdt_csum before it, puts on them. hvboot reads a guest's /boot through it,
and the kernel's ext4 driver is to read and write through it, so a checksum
computed wrong would be a sound filesystem refused as corrupt -- or, once the
driver writes, one Linux refuses. `fuzz/extfmt` compiles the crate into a host
program, and the gate runs it over filesystems mke2fs makes: ext4 as it makes
one by default (metadata_csum, 64bit, flex_bg, extents, 256-byte inodes) with
4 KiB blocks; with metadata_csum_seed and its UUID changed after it was made,
so that the seed is no longer the UUID's; with 1 KiB blocks, 128-byte inodes
-- whose checksums are 16 bits -- and 32-byte descriptors; with gdt_csum's
CRC16 in place of metadata_csum; ext3 and ext2. Each holds small and large
files, a sparse file of two thousand pieces whose extent tree is two levels
deep, a directory of four hundred entries indexed by `e2fsck -D`, fast and
slow symlinks, and extended attributes in an inode and in a block of their
own (`debugfs ea_set`); and `e2fsck -fn` must pass it.

On each, every checksum of every piece of metadata a driver reads -- the
superblock, every descriptor, both bitmaps of every group that has them
written, every inode in use, every extent block of every tree, every block
of every directory (leaves by their tail, index blocks by theirs), every
attribute block -- must be the one extfmt computes, and there must be at
least one of every kind the image's features put a checksum on. Then bytes
the checksums cover are changed one at a time, each kind in turn, and both
extfmt and `e2fsck -fn` must say so: e2fsck's answering no to a fix counts as
much as its exit code, since a descriptor's checksum wrong with its fields
sound is reported and left out of the code. Then each image is fuzzed: up to
eight bytes of the metadata its walk read changed at a time, two thousand
times, the walk held to never panicking (overflow checks on) and always
ending.

Then the journal, which extfmt's recovery planner (`extfmt::recovery`) is to
replay for the driver: the transactions in the log as a plan of which copy
goes to which home block, worked out before anything is written. debugfs
writes into an image's journal and leaves it unreplayed (`jo -c` for v3's
checksums, `jw -b` blocks, `jw -r` revokes, `jw -c` a transaction left
uncommitted, `jc`), on images with v3's checksums and without, 64-bit tags
and 32-bit: a transaction of three blocks; one of a block whose first word is
the journal's magic, escaped in the log; one that only revokes a block of the
first; one writing a block of the first again; a revoke and then the revoked
block written again, which is replayed, the revoke covering only what came
before it; three hundred blocks at once, more than a descriptor holds; and one
uncommitted. The planner's replay and `e2fsck -E journal_only`'s must leave
the same image byte for byte but for the superblock fields e2fsck sets for
itself -- the time of its check, the kilobytes it counts written, and the
checksum over them -- the journal's superblock included, empty at the same
next sequence; and so must they with the journal's ring turned so that its log
starts five blocks short of the ring's end and wraps. With checksums: the last
commit torn must end the log before its transaction, where e2fsck ends it; a
descriptor damaged under a sound commit, and a copy that fails its tag's
checksum, must be refused, as a replay around them would leave the filesystem
neither as it was nor as it was to be. And the planner is fuzzed over each
journal damaged at random -- its superblock and its log's first blocks above
all -- held to never panicking, always ending and planning no copy from outside
the ring. And a log of 9000 transactions, a copy each, is planned
(`long-log`) without the planner asking the heap for more than 128 pages at
once, what one allocation of the kernel's gives: a list of every transaction
asked for 917504 bytes at 8193 of them -- nos's own journal on a root of 32
GiB or more holds that many after hours of small commits -- and the mount that
replayed it panicked, every boot.

Then what extfmt makes rather than reads: `extfmt::mkfs`, the layout of a
new ext4 or ext2 and every block of it, which the shell's `format` writes
([Making one from the shell](filesystems.md#making-one-from-the-shell)).
`extfmt-test mkfs` writes its pieces into a file, and the gate makes them
over a matrix of devices -- ext4 of 2 MiB, 8 MiB, 64 MiB and a few bytes,
with 2 and 4 KiB blocks, past 512 MiB where 4 KiB blocks start, of 1 GiB in
128 groups of 1 KiB blocks, of 3 GiB; ext2 from 64 KiB up -- those up to
64 MiB first filled with noise, so that a block left to the device's zeros
would be garbage: `e2fsck -fn` must find nothing, dumpe2fs must name the features
and label asked for (and for ext2 none of ext4's), debugfs must list an
empty lost+found. And what it refuses it must refuse: a device too small
for ext4's journal or for ext2's least group, a block smaller than a 4 KiB
sector, a block size of 8 KiB.

Needs cargo and e2fsprogs: Linux, or on a Mac the nos-builder container
(`docker run --platform linux/amd64 --rm -v "$PWD":/src -w /src nos-builder
python3 scripts/extfmt-test.py`), where it takes under a minute.

### `edit-test.py` -- the editor, decided on the host

`edit` is a full-screen editor run in the kernel (`src/crates/modules/tools/edit`,
[edit](modules.md#edit)) over a crate with no kernel in it,
`src/crates/common/text/edit`. `fuzz/edit` compiles that crate, as it is, into a host
program that runs it the way the module does -- the bytes a terminal sends
through the key decoder, its saves and quits answered, its frames drawn on a
terminal of the test's own, which does with them what xterm does: wraps text
at the right edge, scrolls at the bottom, and takes the last column's
character with an erase sent while the cursor waits to wrap. The editor
never means any of those, so any is a failure, as is a sequence the editor
does not send or a character it shows a stand-in for sent as it is. Then:

- sessions of keys, each held to what it must leave in the file, at the
  cursor, in the clipboard and on the screen: typing; `^A` to the indent
  and then the start, `^E`; Enter, Backspace, Delete, `^D`; a selection cut,
  pasted and undone, typed over and given back by one undo; lines cut in a
  run and pasted back above the cursor's; undo and redo, typing one step
  and Enter the end of it; Option's words, `^W`, `^U`, Option-Delete; find
  -- next, back, round the end, either case or one, not found and the
  cursor where it was; going to a line; the questions before a quit, a
  pasted `y` answering none; a save over a file changed on disk; a paste
  that is text, a `^S` and an `ESC` sequence in it included, one undo step,
  64 KiB pieces and all; moves, the column kept through a short line, and
  every sequence a terminal sends a key as; the screen -- the status bar,
  the keys, control bytes as `^X` and invalid UTF-8 in hex in reverse, a
  wide character's two columns, a long line's `>`, a line exactly as wide as
  the terminal drawn whole, a terminal too small; a new file;
- 2000 files of random bytes -- CRs, newlines, NULs, invalid UTF-8, a
  newline at the end or not -- each read and written back byte for byte;
- sessions of random keys on random files and terminals: characters, CJK,
  emoji, combining marks, every special key with every modifier, Control
  letters and the questions' answers, pastes with control bytes, ESC and CR
  LF in them, cut short or not ended, garbage, pauses inside sequences,
  resizes to terminals from 1x1 up, and someone else writing the file.
  After every one the editor keeps its promises (`Editor::check`: the
  cursor and the selection in the text where glyphs start, the history's
  sizes its own) and the terminal has had nothing it would not take; now and
  then what was drawn a row at a time must be what a full redraw draws, and
  undoing every step must give back the file as it was read, and redoing as
  many the file as it was.

A failure names its seed and session, and `--session N` runs that session
again. The gate is a fixed seed, 19 cases, 2000 files and 3000 sessions,
about ten seconds; `--seconds` is a campaign. It found, while the editor was
written, that a screen resized and resized back between two frames is drawn
at the size it had and not redrawn -- a terminal does not keep its screen
through that -- which is why a session counts its window-changes for the
command ([sshd](sshd.md#the-shell-behind-it)). Needs cargo.

### `diskimage-test.py` -- what `hv convert` makes of an image, decided on the host

`hv convert` makes a cloud's qcow2 image the raw disk a guest is given
([A disk from a cloud's image](hypervisor.md#a-disk-from-a-clouds-image)):
code in the kernel reading a download. Get a cluster wrong and the guest
boots a disk with the wrong bytes in it -- a filesystem that fsck finds
damaged, or that only one file in a thousand is wrong in -- with nothing to
say so. `fuzz/diskimage` compiles the `diskimage` crate, as it is, into a
host program, and the gate holds it to QEMU's own reading:

- a disk made here -- zeros, with pieces of random, text and repeated bytes
  at random places, a byte to hundreds of KiB, its last sector written and
  its size no whole number of clusters -- made a qcow2 image by qemu-img in
  every version (2 and 3), every cluster size (512 bytes, 4 KiB, 64 KiB,
  2 MiB), stored and compressed, and with extended L2 entries; each
  converted, and the disk held byte for byte to the one it was made of, no
  more written than its blocks that are not zeros;
- images qemu-io writes into: zero clusters, discarded ones and a write
  inside a cluster; an internal snapshot and writes after it; metadata
  preallocated, in version 3 and in version 2; an image left dirty
  (`abort` with lazy refcounts), which is said to be; subclusters written,
  zeroed and discarded -- each converted and held to what `qemu-img
  compare` reads, its size the image's;
- what must be refused, and that a refusal leaves no disk behind: an
  overlay on a backing file, LUKS and AES, zstd, an external data file, a
  corrupt mark, an incompatible feature it does not know, version 4,
  clusters of 256 bytes; an image cut short inside a compressed cluster's
  stream, halfway through its clusters, and before its L2 table, each as
  "is the image whole?"; a raw disk, an ISO, a gzip stream, a VMDK, VHDX,
  VDI and VHD; and a disk that is there already, which is not written over;
- the DEFLATE decoder (`common/storage/diskimage/src/inflate.rs`) against Python's zlib --
  the zlib QEMU decodes compressed clusters with: streams zlib makes at
  every level and strategy, with flushes inside, asked for the bytes they
  hold, fewer and more; the same with a bit, a byte or the end changed;
  streams the script writes itself, block by block -- stored, fixed and
  dynamic, their codes complete, single, empty or broken, repeats before
  any length or past the end, matches too far back, symbols no stream may
  hold, the end of block missing; and noise. Each is asked for a number of
  bytes, as QEMU asks for a cluster, and every answer -- good or bad, and
  the bytes of every good one -- must be zlib's;
- what a device is seen to hold before `hv` gives it to a guest
  (`diskimage::contents`): each kind's mark where its own documents put it,
  on a disk of zeros -- ext2, ext3 and ext4 told apart by their features
  and a journal device, XFS, btrfs, F2FS, swap of three page sizes and the
  old signature, LVM, md RAID's superblocks of versions 0.90, 1.0 and 1.2,
  LUKS, ISO 9660, NTFS, exFAT, FAT12/16 and FAT32, GPT and MBR, squashfs,
  nanofs, nos's disk log -- each with its label where the kind has one, a
  boot signature alone and stray bytes as data nos does not recognize, and
  zeros as nothing; then what the host's own tools make, those it has --
  mke2fs's ext2, ext3 and ext4, mkswap, mkfs.vfat, mkfs.xfs, mkfs.btrfs --
  each called what it is;
- each of five images with its header, L1 and L2 tables and clusters
  changed at random, up to eight bytes at a time, or cut short: no panic
  (overflow checks on), no read outside the image, no write outside the
  disk or out of order, every run ending; and the compressed images with
  only their compressed data changed, converted by qemu-img too -- the two
  must give the same answer every time, and where both convert one, the
  same disk.

And the seed `hv create` gives cloud-init (`common/storage/diskimage/src/iso.rs`): ISO 9660
images of a cloud-init seed, of an empty file beside names alike in 8.3,
and of eight files of 1 MiB in all, each read back by Rock Ridge's names
-- by xorriso, or libarchive's bsdtar where there is no xorriso -- every
byte its own, labelled `cidata`; where there is blkid, `LABEL=cidata
TYPE=iso9660`, which is what cloud-init looks for; and where the host lets
the script mount (Linux, root or sudo), Linux's isofs reading every file
by its name. A label or a name it may not have, two files of one name,
nine files and more than 1 MiB are refused.

`--image <qcow2>` converts a distribution's image and holds it to `qemu-img
compare`, writing no second copy: Ubuntu 24.04's, serial 20261017, was
identical on 2026-10-25. Needs cargo, qemu-img and qemu-io.

### `web-test.py` -- the host API's wire, decided on the host

The host API reads what anyone who connects sends it: a request, a
WebSocket frame, a JSON text. A misread there is a panic in the kernel, or
two readers that disagree about where a request ends; a certificate made
wrong is one no client takes, or one that takes the wrong thing. `fuzz/web`
compiles the `web` and `mgmtwire` crates, as they are, into a host program:

- **Known answers.** SHA-1's FIPS 180 vectors (a million a's among them);
  RFC 6455's accept key and its masked "Hello"; requests, frames, JSON
  texts, parameters and networks, each with what it must make or the status
  it must be refused with -- a line ended by LF alone, two Content-Lengths,
  a Transfer-Encoding, a header folded; a client's frame unmasked, a length
  not in its shortest form, a ping fragmented, a close code nobody may send;
  a fraction, a leading zero, a key twice, a surrogate alone. A response's
  head with a CRLF in a value is refused, not written.
- **Every parser fuzzed**, with what a generator makes and then breaks:
  never a panic (overflow checks on, as a RUSTUB=1 kernel has them); what
  the JSON writer wrote read back the same, and what parses written again
  and read back the same; every prefix of a whole request or frame "not
  yet"; every message of a generated WebSocket stream, fragments and pings
  between them, read back as it was sent; the limits it is given kept.
- **The certificates judged by others.** OpenSSL reads the server's, the
  client's and one valid into 2050 -- version 3, ECDSA over P-256, no CA,
  signing only, for a server or a client, the addresses and names asked for,
  its end a GeneralizedTime -- and its fingerprint and its key's are the ones
  `web::x509` gives. Python's ssl takes the server's key as its
  certificate's, serves TLS with them to a client that trusts the
  certificate and checks the name it was asked for, and refuses a name the
  certificate is not for; a server that asks for a client's certificate
  takes the client's.
- **The descriptors.** A descriptor with every member the format has must
  be taken, and each of thirty-one defects of it refused -- a text without
  its Russian, a method not described or of another kind, a read that asks
  to be confirmed, a type the format does not have, an enum without its
  values, a resource listed by an action, a column that is no field, a
  parameter from a field there is not, a resource its own parent, a metric
  read by an action, a bound written as a string -- and the same descriptor
  broken at random is never a panic. Then every `*.descriptor.json` in the
  tree is checked as the bus checks it, for the methods it says it has (the
  boot then holds those to what the provider registers), and read by
  Python as well: no key twice, every title, help, confirmation and enum
  value in English and Russian, none empty, none left untranslated.

Nineteen mutants of the two crates -- a key twice, a leading zero or a lone
surrogate taken; a big count written as a number, a quote not escaped; a
parameter not taken let through; a bare LF, two Content-Lengths, a
Transfer-Encoding, a body not waited for, a CR in a header written; an
unmasked frame, text not UTF-8, a message past its limit in fragments within
it; SHA-1's round constant; GeneralizedTime from 2051, a client's
certificate for servers, a serial's top bit set; a leading zero in an
address -- each failed it. Needs cargo and openssl; `--seed N --iters N` for
a campaign.

### `crc-test.py` -- the tree's CRCs, decided on the host

One crate, `src/crates/common/crc`, computes every CRC the tree's formats
are checked by: CRC-32 (GPT's header and partition array, the disk log's
header, the `crc32` command), CRC-32C (ext4's metadata_csum
and journal, raft's log) and CRC-16 (ext4's gdt_csum) -- for the kernel,
for hvboot in a guest and for the fuzzers on the host alike, where each of
those had once a copy of its own. A CRC computed wrong is a disk that looks
corrupt, or one that does not, and nothing else says which: `fuzz/crc`
compiles the crate, as it is, into a host program and holds it to

- the known answers: each CRC's check value (`123456789`), CRC-32's of the
  quick brown fox, RFC 3720's four CRC-32C vectors, CRC-16 from 0 (ARC) and
  from !0 (MODBUS, as gdt_csum starts it), and the references below held to
  the same values, since a reference wrong in the crate's way would pass it;
- 20,000 random messages -- most of them short, many around the eight-byte
  steps the crate takes, some of 9000 bytes, each at a random alignment --
  every function held to the CRC computed from its definition: the
  polynomial as it is written, not reflected, divided into the message a
  bit at a time with the bytes and the register reflected around it, which
  shares nothing with the crate's reflected tables but the answer. `raw`
  from a random register, `update` from a random CRC, `checksum`, each whole
  and carried on across a random split; `with_hole` against the block with
  its field zeroed, and refusing a field not inside the block;
- CRC-32 against Python's zlib, over messages the script makes: every length
  to 64 bytes, then lengths around and past the 64 KiB the `crc32` command
  reads at a time, each whole and fed in random pieces.

It takes about five seconds. Six bugs planted by hand in the crate -- two
of the tables swapped, a polynomial's bit changed in CRC-32C and in CRC-16,
`update` not inverting its result, the bytes past the last eight shifted by
seven, `with_hole`'s bound one byte long -- each failed it. Every boot holds
the kernel's own build of the crate to the check values as well
(`crc32::selftest` in `rust_test`). `--seed N --cases N` for a campaign.
Needs cargo.

### `raft-test.py` -- consensus, decided on the host

Consensus that loses a committed entry, elects two leaders in a term or
confirms a stale read looks like a group that works -- until the one
schedule of crashes and losses that breaks it. `fuzz/raft` compiles the
raft crate, as it is, into a host program and holds it, in under five
seconds, to:

- **Known answers**: every kind of message written and read back, a vote's
  bytes as the format has them, bytes that are not a message refused each
  for its reason; the storage's log across its pieces, written over and
  compacted; a lone voter that leads, commits and reads on its own; the
  membership change rules; a member added later making the group's
  membership of the log it is sent.
- **The known traps**, each played out step by step: the paper's figure 8, a
  deposed leader asked for a read, a membership change before the leader's
  first commit (the 2015 erratum), a snapshot overtaken while it is sent and
  its pieces lost, doubled and reordered, a member back under its old id with
  a wiped disk -- which the checks must catch, and the harness refuses -- a
  vote not written before it is answered, the election timer and a vote
  given in the same term, a follower's commit past what its append showed,
  a PreVote taken for a vote, an append after an entry of another term.
- **The node target**, 2000 groups of one to seven, 5000 steps each: a
  network that delays, loses, doubles, reorders and cuts; members crashing
  and back with exactly what they wrote; members added, promoted, demoted
  and taken out; leadership handed over; snapshots taken and sent. At every
  step: one leader a term; a new leader holding every entry committed in an
  earlier term; logs that agree on an entry's term agreeing on everything
  before it; no committed entry changed, and every member applying the same
  entries to the same state, a snapshot taken in carrying it; no read
  confirmed behind what was committed before it was asked; no election
  before the timer ran out since a vote, a leader or a term; no message
  taken for one that cannot be. Then the network healed: a leader, and a new
  entry applied by every voter, within 600 rounds.
- **The wire target**: messages written and read back; noise and broken
  messages read without a panic, anything taken written back to its very
  bytes.
- **The node target's disks fail reads**, in a fifth of its runs, one read
  in some hundreds: a member must stop on the first (`fault` said, no
  `Ready` after it) and is crashed and started again, every property above
  still held; and once healed, every read asked of a member still leading
  in its term must be answered -- which found, besides the round asked once,
  a round a change of voters confirmed that nothing answered.
- **The storage target**, 20 runs of 300 operations: `MemStorage` against a
  model of its log -- appends over its end and into it, snapshots taken in
  logs of tens of thousands of entries and sent -- every term and range of
  entries the model's.
- **The log target**, 300 runs of 300 steps of `raft::log`'s `DiskLog` on a
  disk of the fuzzer's own, which keeps POSIX's promises and no more. A
  file's writes are on the medium once it is synced, a name once the
  directory is. At a power cut every write since a file's last sync is kept,
  lost or torn on its own (its tail zeros, noise or the file's end), and a
  run of the directory's changes since its last sync is kept, in order. The
  steps are:
  - appends over the log's end and into what is not committed, the hard
    state, syncs;
  - snapshots taken and sent, segments rolled and compacted;
  - power cuts, and restarts with no cut.

  After each cut or restart the log must read back as the state at the last
  sync with some first records written after it, at a record's boundary --
  nothing synced lost, nothing torn read as an entry, nothing that was cut
  read again -- or as the state a snapshot that failed half way would have
  left. Other runs:
  - calls of the disk fail, now and then, in a third of the runs: the log
    stops on the first, says `Stopped` after, and reads back the same way;
  - a byte of a sealed segment or of the snapshot is changed, and the log
    refuses to open; with the byte put back, it reads back to the byte;
  - noise named as the log names its files, some of it under a segment
    header that checks: refused, or read as an empty log.

  It found four bugs, each now one of seven scenarios (`log_*`): a leader's
  snapshot taken below the commit; an open that used what the medium did
  not yet hold; a leader's snapshot that stopped dropping the log before it
  once a snapshot was taken after it; and an entry below a snapshot whose
  truncation was skipped on replay. `RAFT_LOG_TRACE=1` prints each step and
  the disk at each cut. `mutants.py`'s six `log_*` mutants take out the CRC
  check, the seal of a segment before the next, the syncs at open, the
  snapshot's sync before its rename, the compaction's bound, and the
  installed snapshot's drop; each fails the gate and its own scenario.
- **The rsm target**, 100 runs of 1500 steps. A register goes through
  `raft::rsm`, in groups of three or five on the log on disk over the crash
  disk. The world:
  - a network that delays, loses, doubles and cuts;
  - members crashed and started again over what their disks kept;
  - two to four clients, with sessions, doing writes, adds -- which a
    double application shows -- compare-and-sets and linearizable reads,
    one at a time, each retried of another member after a timeout;
  - snapshots every few hundred entries, written over several calls with
    applying going on between them.

  Once calm, every client must be answered. Every operation's history, from
  its asking to its answer, must then be a register's (Wing and Gong's
  search with Lowe's memo, held to known histories by
  `rsm_linearizable_check`). `mutants.py`'s five `rsm_*` mutants take out:
  - the session's answer to a retry;
  - the read's wait for its index;
  - the snapshot's index as frozen;
  - the start's nonce in a read's context;
  - the sessions in the snapshot.

  Each fails its own scenario. The target alone catches every one but the
  nonce within forty runs. The nonce's bug -- a leader's answer to a read
  asked before a restart taken for a later one's -- was a campaign's
  finding at one run in 130,000, and has `rsm_read_after_restart` to hold
  it. `RAFT_RSM_STATS=1` prints each run's operations, applied indexes and
  snapshots, and `RAFT_RSM_TRACE=1` what every member does and, for a
  history no register makes, its shortest prefix.
- **The registry target**, 200 runs of 400 steps: the cluster's registry
  (`src/crates/common/cluster/registry`, plans/cluster.md) -- raft's first user -- held to
  a model of its own.
  - Alone: the registry and the model take the same commands -- init,
    invitations, joins, removes, revokes, entries that are no command --
    under a clock that goes on and now and then back. The commands draw on
    small pools of names, addresses, keys and invitation numbers, so that
    they meet; on every third seed on wide pools with few removes, so that
    the cluster fills to its sixteen and has learners. After each one, its
    answer and everything the registry shows -- its hosts, invitations,
    removed hosts, voters and every host's peers -- must be the model's.
  - Its snapshot must read back as itself, and its frozen pieces must be
    that snapshot. A snapshot changed -- a byte, cut short, more bytes,
    noise -- must be refused, or read as a state that holds to every rule
    and reads back as itself. A replica started from a snapshot part way
    must end as the registry does.
  - On every fourth seed, a group: the registry through `raft::rsm` on
    three or five members over a network that loses, doubles, reorders and
    cuts, and a client with a session retrying of another member after a
    timeout. Once calm, every member's registry must be the same, and the
    model's once it takes the commands in the order the log applied them,
    answering each as the members did.

  The commands include a host's maintenance, its addresses by role and the
  keys it moves between -- one added beside its own, the old one dropped and
  retired -- and the scenarios `registry_keys`, `registry_addresses` and
  `registry_maintenance` play each out, a snapshot giving a host a retired
  key refused among them.

  `mutants.py`'s nine `registry_*` mutants take out:
  - an invitation's expiry;
  - the join's use of its invitation;
  - the refusal of a removed host's key;
  - nodes never given twice;
  - the snapshot's check of keys;
  - the voters' cut at five;
  - the refusal of a key a host dropped;
  - the refusal to drop a host's last key;
  - the refusal of an address another host has.

  Each fails the gate and its own scenario. `RAFT_REGISTRY_STATS=1` prints
  each run's answers by kind and how many hosts it reached.
- **The nbsmeta target**, 200 runs of 400 steps: NBS's metadata
  (`src/crates/common/storage/nbsmeta`, plans/nbs.md) -- raft's second user
  -- held to a model of its own, as the registry is: alone, the metadata and
  the model take the same commands -- the NBS cluster made, members added
  and removed, entries that are no command -- from small pools of names and
  incarnations that meet (a host added again as itself and as another
  incarnation, an incarnation another host's or removed before, the last
  member's removal, a remove asked again), and on every third seed wide
  ones that fill the cluster to its sixteen. After each one, its answer and
  everything it shows -- the cluster, its members in their order, those
  removed, the voters and the learners -- must be the model's; its snapshot
  read back, frozen in pieces, changed and refused or held to every rule,
  and a replica from a snapshot part way ending as it does. The scenarios
  `nbsmeta_replace`, `nbsmeta_voters`, `nbsmeta_bounds` and
  `nbsmeta_snapshot_rules` play each rule out, and `mutants.py`'s six
  `nbsmeta_*` mutants take out: the refusal of an incarnation removed, of a
  host's new incarnation for its old one, a remove asked again answered as
  done, the refusal of the last member's removal, the voters' cut at five,
  and the snapshot's check of members removed -- each failing the gate and
  its own scenario. `RAFT_NBSMETA_STATS=1` prints each run's answers by
  kind. What raft's rsm does with a state machine is the rsm and registry
  targets' to hold: this one is the state's own rules.
- **The hostile target**, 2000 runs of 3000 steps of one member fed what no
  correct member sends -- every kind of message, of terms and indexes about
  its own and of any value, from members and strangers, noise the wire
  takes, and answers to what it sent, true and false, so that it leads and
  sends snapshots too -- crashed, compacted and started over: no panic, and
  its term, its vote within a term and its commit never going back, nothing
  committed past what it holds, entries handed out in order and as written,
  a vote answered only once it is on the disk, nothing sent the wire would
  not read back as it was.

Every allocation of all of it is held to what the kernel's heap gives at
once, 512 KiB (`fuzz/raft/src/heap.rs`): a larger block is a finding, the
first with its backtrace. `long_log` is the test for it -- 30,000 empty
entries and 12,000 membership changes, started over and applied again, a
leader ticking 20,000 times with nobody taking its messages -- which, before
the change list was kept in pieces and every batch held to a count as well
as bytes, asked for eleven blocks past it, the largest 4 MiB.

`fuzz/raft/mutants.py` takes one rule at a time out of the crate -- the
figure 8 check, a vote written before it is answered, the timer started
over by a vote, a follower's commit held to its append, a PreVote kept
apart from a vote, an append's previous term checked, the 2015 rule, a
read's round of heartbeats, a failed read stopping the member, a round
asked by every heartbeat, an append taken in part, a snapshot that keeps
the log in pieces, and of `raft::recover::force_alone` the commit of what it
appends and the learners it takes out (`force_alone_after_loss`) -- and each
must fail the gate, and fail the scenario written for it on its own. Campaigns found three things the gate now holds: an
append window filled with appends lost for good, which nothing emptied (a
heartbeat's answer now frees one); a candidate of an older membership
standing term after term ahead of the voters of the newer, which never
caught up; and a leader cut off that never heard of the later term (a vote
request and an append of an older term are both now answered with ours).
The review of the first version found six things, each now a scenario
that failed on it (`fuzz/raft/src/review.rs`): a storage's failed read
taken as no entry (a vote given on it), an encode that grew its buffer
past a block of the heap, a read whose round was lost never answered, a
snapshot that copied the log after it, an append dropped whole where part
of it fit, and a storage's or configuration's values taken unbounded --
and a snapshot given up mid-way held until the term changed. The hostile
target found the harness's own overflow first and nothing of the crate's
since; terms and indexes no group reaches (2^62 and past) are
refused before they are looked at, so no arithmetic on what a peer says can
overflow. `--seed N --seconds S` for a campaign of the node target;
`--mutants` to run the mutants after the gate; `raft-fuzz hostile --seed N`
and `wire --seed N` run those targets by hand.

### `nbs-fuzz.py` -- NBS's metadata, decided on the host

A state machine that every member applies alike can be wrong in a way no
member notices: a chunk's replicas twice on one host, room reserved twice, a
chunk freed while a map still names it, a snapshot that reads back as
another state. And a chunk server can be wrong in a way no test that never
cuts the power sees: a fence or an open mark not yet durable when it is
answered, zeros a record outlived, a torn copy taken for the record.
`fuzz/nbs` compiles `nbsmeta`, `nbscs` and `nbswire` as they are into a host
program, every target under a few seconds. Its `meta` target -- 1000 runs of
400 steps -- holds the metadata to a model of its own, which shares no code
with it:

- **The commands** are of every kind, from small pools that meet, and now
  and then spoiled into bytes that are no command:
  - members added and removed;
  - devices added under ids that collide, a host's own or another's, of
    sizes from none to past the limit, and drained;
  - volumes made -- of sizes from none to past 16 TiB, under names that are
    none -- grown, made smaller and deleted;
  - attaches for writing and for reading, forced or not, and detaches by
    the writer's generation and by others;
  - replicas taken out by a volume's writer; cs's going down, up and out;
    joins begun -- a member back in its own place, another in a place gone
    or drained -- ended and given up, at versions of now and of before (N9a);
  - snapshots, freezes and clones (N10a), mostly of what they can be made
    of; copy-ups begun by a volume's writer, mostly of a place it shares,
    and done, mostly of a copy-up pending -- and detaches, freezes and
    deletes now and then of a volume with copy-ups pending, and deletes of a
    volume that shares a copy-up pending's old chunk (N10c);
  - the leader's digests, true and false.
- **Every answer** must be the model's, and so must everything the state
  shows: the members, the cs's with their state, devices and reserved room,
  the volumes with their writers and readers, the three cs's each chunk is
  on, of three hosts, and each set's members down, the version each was
  active until, and its joiner.
- **A chunk's identity** (N10a): the model knows each chunk by an identity
  of its own, shared by the maps a snapshot or a clone copied and counted by
  them and by a copy-up pending on it. It cannot know the number the
  metadata's free list gives a chunk: it learns it as the chunk first shows
  -- in a map, a copy-up pending, a copy-up's answer -- and holds it from
  then on, one number one chunk, of the incarnation it had, counted as the
  model counts it.
- **The placement** is the model's own, made by the plan's rules alone. A
  refusal for room is a finding when the chunks would have fitted by
  counting: `n` chunks fit on three hosts each exactly when the hosts' free
  room, each counted up to `n`, adds up to `3n`.
- **An fsck of its own** after every step:
  - every chunk in use counted as often as the maps name it -- and a
    copy-up pending on it, its new chunk in the old one's set, the old one
    where the volume's map has it -- and a free one neither counted nor
    named;
  - an image and a snapshot written by none, a snapshot made of a volume,
    none made of itself;
  - every set's count of chunks the chunks in it;
  - every cs's reserved room what its sets hold, a set it joins in
    another's place among them;
  - every set with a member active; a member down active until a version
    before the set's, an active one until none; a joiner of a cs there is,
    in its own place down or in one no other member holds, joined at a
    version of its set's;
  - the free list walked -- through free chunks only, none twice -- to as
    many as are free.
- **A replica** fed the same entries holds the same state and takes the same
  digests, and a checkpoint's digest is the state's own.
- **The snapshot**:
  - written whole and in pieces of every size, the same bytes;
  - read back as the same state, with the same digest;
  - one read back part way and fed the rest of the entries ends as the
    state does;
  - spoiled a few bytes at a time, it is refused, or taken as a state that
    keeps every rule the fsck holds, and goes on keeping them through more
    commands; now and then a set's joiner is found by its own values and
    made one no set can have -- of a place past the three or another
    member's, of an active member's cs, joined at 0 or at a version to
    come -- the rest left whole, so that the check of joiners is the one to
    refuse it; and now and then, of a randomness of its own, the next
    device's or volume's number found and made 0 -- refused, or, taken, a
    device and a volume added to it numbered as the state numbers them: a
    counter of 0 breaks no rule the fsck sees until something is numbered
    by it, and bytes spoiled at random reach the counters once in a long
    while.
- **A divergence**: a digest of the leader's that is not the state's own --
  false on purpose, or made so by spoiling -- must be said, at the entry it
  was for.
- **The controller's progress** (`nbsmeta::repair::plan`, N9a): from the
  state at a step, with some hosts gone for good and the others answering,
  its turns are applied until it proposes nothing. Every proposal must be
  answered as the model answers it and none refused, and at the end no
  member is down whose cs answers, no place is left to fill that a cs had
  the room for, no join is under way, every cs that answers is up and every
  one gone is out. Joins end there as a copy done would end them; the copy
  is N9b's.

Every allocation of the crate's is held to what the kernel's heap gives at
once, 512 KiB (fuzz/raft's `heap.rs`); the model's own are exempt.

Its `join` target -- 800 runs of 3000 steps -- is one join's copy (N9b):
nbswire's `copy` from a source onto its joiner, both nbscs's engine over the
cs target's disks, the source served by nbswire's `serve`, the hosts' mds
played from the set's state the target keeps:

- the network delays, reorders, drops and repeats the copy's requests and
  answers;
- up to three writes of the map now in flight at once, apart, each delivery
  anywhere -- one to the joiner long before its other to the source;
- writers of maps before the join, of any version since the member went
  down, writing the source alone: taken where the source knows the chunk at
  that epoch, refused once the copy has moved it on, and then written again
  as a writer of the map now;
- the set moving on, the copy's view refreshed a while after; the join given
  up and made again, the writes between to the source alone, the copy begun
  a while after the joiner's first writes;
- a sector of a chunk being checked rotted on the joiner;
- the joiner's power failing mid-copy -- now and then a few writes before a
  chunk is closed -- after which it comes back with what its disk kept and
  its join is made again;
- in a third of the runs a member back, holding the set's chunks as they
  were when it went down, some written since on the source alone, and some
  found open after a cut;
- in a tenth of the runs each, one race a random run seldom puts together,
  set up on purpose: the power failing inside a close (armed two writes
  ahead as a sums answer comes); a writer of a map before the join at the
  epoch the source knows a chunk at, as the copy closes it; a member back
  given up once it has touched, and writers of maps before at the epochs
  the touches left, once the next copy has listed; a write on the joiner
  before the next copy begins, its delivery to the source held past the
  copy; a member back with every chunk found open, given up at its first
  pull. A mutant of each of these rules survived a hundred random runs, and
  was caught once in thousands of them.

At its end every chunk of the set on the joiner is the source's, byte for
byte, and every chunk the copy closed is still so after a power cut. It
found, as it was written, that a granule left out of the check whole for a
client's write in it let a rot stand; that a member back's first write to a
chunk found open was refused as failed, which drops its join each time it is
made; that a write in flight of an epoch the cs's cache once vouched for was
taken after a pull had moved the chunk on, and moved its epoch back; that a
copy given up part way left a chunk found open no more, which the next copy
then touched; and -- once its checks after a cut were made to find something
to check, which they had not, and passed -- that a record of the copy's, at
no writer's generation, read back as lost, every chunk a copy had taken in
gone at the cs's next start.

Its `cs` target -- 200 runs of 300 steps -- is the chunk server's engine
(`src/crates/common/storage/nbscs`) over one to three disks of 512-byte or
4 KiB sectors, each with a write cache and a power switch. The fuzzer is the
cs's clients and its host's mds: it decides every chunk's incarnation and
epoch and every volume's generation, and keeps what each sector may hold.

- **The steps**: fences, new and old; writes at the fence, older and newer
  than it, checked or not, and now and then by another volume's writer
  (N10b) where the chunk is not open for its own; reads by the writer,
  unvouched first now and then -- asked of the mds where the cs does not
  hold the chunk, and nowhere else (N10c) -- and by another volume,
  unvouched first; a read vouched for at an epoch the cs did not know, and
  the same once the chunk is freed, which asks the mds again (N10c); flushes; closes; a chunk
  freed and its number taken by a new incarnation of another volume, the cs
  told or not; a set's version moved on; a request of a number's chunk
  before; and copy-ups (N10b), served by nbswire's `serve` as nbs-cs serves
  them, the fuzzer the mds it asks -- mostly of a chunk the cs holds by a
  volume fenced, now and then by an older attach or a newer one, of two
  chunks of two epochs or one, of an epoch older than the one the cs knows,
  refused by the mds, or the last one asked again -- drawn of a randomness of
  their own, so that every other draw goes as it did.
- **The power** fails within a step's next few writes, or between steps,
  and in a copy-up at any of its writes and flushes.
  Each write the cache held is kept, lost or torn -- some of its sectors
  kept -- and now and then one of the cs's own sectors is torn inside, or a
  bit of a copy A rotted when no record of it was in flight.
- **After every cut** the cs is opened again from its disks, in either
  order, and held to:
  - every fence answered there, at least as new;
  - every chunk written since its last close open, at a generation at least
    the newest answered, as the volume's whose writer wrote it;
  - a chunk closed reading exactly what it held at the close, and a
    copy-up's what it was copied from -- one cut in it reading that or
    nothing;
  - every sector reading its value at the last flush answered, or one
    written since;
  - a chunk never written, freed, or taken by a new incarnation reading
    zeros where the new one was not written;
  - no record lost, and every slot free, held or lost, none twice.
- **Between cuts**: every read exactly the last write, and every refusal the
  one the request deserves; another volume's chunk read or written only once
  vouched for, and written, that volume's from then on; a copy-up asking the
  mds where the cs does not hold its new chunk and nowhere else, done a copy
  of its chunk -- closed, the volume's, at its epoch -- and asked again,
  nothing.
- **Before each run**: a blank disk opens as blank, one with a byte past a
  superblock's in its first two pages -- a boot sector's signature, a GPT
  header, ext's magic, anywhere -- opens as something else's and is not
  written, and another cs's is told apart.

Its `scrub` target -- 300 runs of 3000 steps -- is the leader's scrub
(N9e), nbswire's, over a set of three members whose copies of each chunk
are the target's own: whether the member holds it, each granule's sum,
whether it is open there. A member holds a chunk once its first write
reaches it, as a cs does, and answers `NotHeld` before; of the volume's six
chunks the writer writes three often and the others seldom, so that some
are held by none for long. A writer's writes and closes reach each member
on a delay of its own, a close once every member has the writes before it,
as a client closes a chunk once each acknowledged them; the scrub's asks
reach the members on another way, no order between the two, so that a
write and a close fall between two members' answers to one look. Now and
then a granule of a closed chunk rots on one member, or on two, or a member
loses a chunk it held -- read as zeros then, as a cs's chunk it holds none
of is; told of a finding, a person has the members rotted taken down and
joined back a few seconds on, through the metadata's `ReplicaBad`, `Join`
and `Joined`, the scrub looking at the chunk meanwhile. Of a randomness of
its own, a member is now and then taken down and joined back a moment
later, its copy the others' then: the set moves on under looks in flight.
And of another, the races random runs seldom put together are set up: a
write and a close raced through a look -- one member's answers taken at
once, the write and the close then on every member, the others' answers
after -- and a member's close held back a few seconds, the chunk open there
and closed on the others through two looks; the writer leaves a chunk
alone while its close is on its way. Every finding is held to the set as it
is -- at its version now -- and to what each member held when it answered
-- a member rotted or lost alone the one unlike the two others, two no two
alike, none a finding of nothing -- and said once; at the end, the writer
done and what it sent delivered, every chunk closed, every rot left is to
be said within a minute, and the findings acted on leave every copy alike;
and a look every member answered `NotHeld` of is followed by the next well
inside half a look's turn, a chunk held by none taking no turn of the
pace.

Its `wire` target -- 100 runs of 500 steps -- is nbswire's messages: every
request, answer, question and what the mds tells, a client's asks and what
it is told, made at random, encoded, read back as itself and encoded the
same, then cut short, spoiled and given bytes past its end, each read as
none or as a message that encodes to the bytes read.

Its `cluster` target -- 1,000 runs of 2,000 steps -- is NBS end to end: the
client (`src/crates/common/storage/nbsclient`) writing, reading, flushing and
closing a 24 MiB volume through six hosts' cs's -- nbscs's engine, done by
nbswire's `serve` as nbs-cs.ko does it -- asking its host's mds, whose answers
are nbswire's `mds` over nbsmeta's state, as nbs-mds.ko's are; the leader's
look at a cs before it is taken out the fuzzer's, mostly that it answers.
The leader's controller runs there as nbs-mds runs it (N9d): a cs down two
seconds after its host is, out after four (`repair::After`, the thresholds a
parameter of the plan) -- some two restarts in five come back members still
-- up once back; its places joined by its own cs back,
or by another, and each joiner's cs running nbswire's copy from a source, as
nbs-cs does, over the same network. The network delivers a few messages a
step, in order between two ends and the pairs in any order, time going by a
little and now and then past a silence.

- **What happens**: a host crashes now and then -- one at a time, its disk's
  write cache kept, lost or torn -- and comes back a while on; a writer on
  it is lost with it, what it sent to a cs still on its way. A writer is
  taken over by force from another host, now or once it is at rest -- asked
  to flush, then nothing -- and the old one is asked on to write and to
  flush, as its host's driver would until it learns it is fenced. A writer
  whose chunk's last replica restarted fails, as it should, and another
  starts. A host is drained now and then, one a run, its places taken by
  others while it serves. At the end of some runs a host is lost for good,
  where every chunk has a replica in on another: the one down kept down, or
  nbs-test's repair phase shaped -- every chunk written, so that each
  replica in owes a flush; a host that holds some lost; every chunk written
  past it, so that it is out of every set and still owed one; then a flush
  and a close. In half of those the controller has the host down first,
  before the writer touches its sets: a flush goes to it by the view of
  before, and the writes past it read the map again, dropping it -- and in
  half of those again the target waits until the host's places are all
  others', so that the writer's asks to take it out are refused as no
  replica of their sets. In some other runs a host that holds some is
  crashed with every chunk open on it and drained, the writer stalled
  until the controller has its cs down in every set; back with the chunks
  found open, it is sent the writer's closes by a view from before, and
  refuses them as not in the chunks' sets; in others a set's first active
  member -- the source a copy asks first -- crashes with every chunk
  written since the last flush and comes back at once, its chunks found
  open and what its cache lost gone, while the writer stalls and another
  member of the set is drained: the joiner is to take another source; in
  others (N10c) a host that holds some crashes with every chunk written
  since the last flush and comes back at once, its chunks found open, the
  writer flushes -- which that replica is to refuse, and be taken out by --
  and the host crashes and comes back again, held then to what was
  flushed. These ends are drawn of their own, so that a run they do not
  shape goes as it did. The leader's scrub runs throughout (N9e), as nbs-mds runs it, a
  chunk every four seconds of the network's time; its asks and answers in a
  queue and a randomness of their own, so that the run they look at goes as
  it would without them. Once every set is whole at the end, a sector of a
  closed chunk rots on one member now and then, or on two; the scrub is to
  find it, and the members are taken down half a minute on, as a person
  would -- past the scrub's next round over the chunk. Snapshots of the
  volume are taken now and then between two writers (N10c) -- the writer
  stopped, a flush and its closes done, the snapshot taken, a writer started
  again -- two kept at most, the oldest dropped for a new one and one dropped
  now and then, of a randomness of their own; the writers after them copy up
  each chunk a snapshot shares before they write there. Each cs's garbage
  pass runs every three seconds of the network's time, as nbs-cs's does,
  freeing what copy-ups and drops leave.
- **Held to**:
  - a write done held by every replica the metadata has in, read back from
    each then and there -- but one that restarted with the chunk open at
    the writer's generation, which refuses it;
  - a read each block's last write done when it was asked, or one after
    that -- out, failed, or done since;
  - a host back holding, of every block, what a flush made durable or
    something written since;
  - once a newer writer serves, no active replica doing an older
    generation's write or flush -- held where the cs serves it;
  - the replicas alike once a writer serves, and at the end, after a writer
    is stopped;
  - a writer failing only as its chunk's last active replica refuses it,
    and an operation of its -- a read, a write, a flush, a close -- only at
    its deadline or with its session: a refusal sends it to another
    replica, or has the map read again;
  - with a host lost for good, the writer's writes past it, its flush and
    its close done -- none failed at its deadline -- no flush sent to its
    cs once neither the state nor the writer's view has it in a set (judged
    as sent: one sent while the view had it in may get there after), and
    the stop detached. A stop held up by a chunk whose replicas in are all
    on the host lost is none of this: that chunk has nowhere else to be;
  - a cs asked out of a set only where the writer's view writes it there:
    one asked about once the view has it out would be asked about for as
    long as the session lives;
  - the scrub finding nothing the target did not rot, of a set at its
    version now, and what it rotted found as it is -- one member rotted the
    one unlike the two others, two rotted no two alike -- said once, and
    every replica alike once the members are copied again;
  - every set back at three active members by the end, none on a drained
    cs and none with a joiner, within ten minutes of the network's time --
    but where a set's active members are all on the host lost; and every
    proposal of the controller's applied, none refused;
  - every snapshot reading what the volume held when it was taken (N10c):
    each block written by then -- the last write done, or one failed or lost
    after it -- and each written first since, zeros, read on every active
    replica up of its chunk alike, one of those values, and the same at
    every look: once it is taken, now and then, and at the end; and once a
    newer writer serves, no active replica doing an older generation's
    copy-up either.

  A writer that follows another holds what that one had done and not
  flushed as a crash would leave it: there or not.

  It did not find the close that nbs-test's repair phase left waiting for
  good on a cs no set had in any more, though it still owed a flush (N9c):
  only the writer took a replica out then, the hosts always came back, and
  a run's writes reach three of the volume's six chunks, so a host lost was
  never out of every set. Hence the end above. With the controller and the
  copies in it (N9d) it found, each fixed with its mutant:
  - in the client, a set's version taken from a take-out's answer past the
    view -- a join begun between, the joiner never written; a write out by
    a view the map read since changed, done once its one replica, down, was
    taken out, by no replica at all; a member joined while the start fenced,
    never fenced, the start waiting for it for good; a read with no active
    replica in a view behind its set, failing a reconcile and its session;
    a session detached failed by a late answer; and, with the controller's
    thresholds at two seconds and four, where more restarts come back
    members, an ask-out refused as no replica of its set -- a cs lost for
    good, its places joined by others -- asked again a second on for good,
    the map never read again, and the stop waiting on the cs: a refusal has
    the map read again; and a close a cs back refused as not in the chunk's
    set, sent again on each map read for good: it is done so now;
  - in the cs, a copy's close that left out its flush where a client's write
    left the chunk open; a pull's moving the epoch on that took the mark of
    a chunk found open off; a chunk found open serving as a copy's source;
    a joiner taking a write of an epoch before its join, from a writer
    taken over since; and a copy keeping an open mark from before its join
    -- the joiner a member then, taken out with the chunk open -- which the
    joiner's crash made found open, its set's last member then no source
    for the joiner after it: the copy closes such a mark at its end;
  - in the scrub as first drawn (N9e), which took down the member unlike the
    two others itself: a joiner had copied a source that rotted before the
    scrub found it, two members were alike in rot, and the scrub took the
    one good copy down, its join back about to copy the rot over it -- the
    scrub says now, and a person takes a member down;
  - in the copy, a member back's touch keeping a write a writer taken over
    had made on it while it was out and not fenced: it holds every chunk to
    its source's by sums now, and `Op::Touch` is gone.

  With snapshots and copy-ups in it (N10c) it found, each fixed with its
  mutant:
  - in the copy (N9's, never met before: no chunk was freed under a join),
    a chunk the source freed under the copy -- its garbage pass, a copy-up's
    old chunk named by no map -- dropped with its pulls still in flight, whose
    answers were taken for the next chunk's: a late refusal dropped that one
    too after its first piece, the join ending without it, and a late pull's
    data would have been written into it. Dropping a chunk drops its asks;
  - in the client's copy-up as first written, done without the one member a
    view behind its set had asked -- down, taken out, at a version past the
    view -- so that no member copied and the new chunk was zeros under the
    writes after: a take-out past the view has it asked again from its start;
  - in the cs, what the mds vouched for taken for a chunk the cs does not
    hold: a read of a chunk never written, vouched for at a set's version,
    kept; the set joined; the writer's first write of it, by its view of
    before, taken as vouched for, the joiner never written. A vouch is taken
    only for a chunk held, whose record refuses an older epoch;
  - in the metadata as first written for N10b, a copy-up refused for room
    where a member of the set is drained -- its devices count no room --
    failing the writes held: it is refused as `Draining` now, and asked
    again;
  - in the target itself, the end's wait for the scrub skipping ahead while
    the scrub's asks and answers waited their turn: every look of a quiet
    end timed out, and a rot went unfound once snapshots made the rounds
    longer.

`--mutants` runs `fuzz/nbs/mutants.py`, which takes out one rule at a time.
Each of its hundred and seventy-one must fail the gate -- run as `nbs-fuzz gate
--first`, stopped at the first target that fails: the gate's verdict, sooner.
The metadata's seventy-one:
- placement: the home host first, the most room first, rule 6, three full
  hosts refused as no room rather than too few hosts, new chunks only on
  cs's that are up and only into a clean set, an empty set made again
  before a new one, and only where it is no clean set the placement fills;
- room given back, a volume refused whole, a forced attach's new
  generation, a device of another host's, a name taken, an attached volume
  kept, and a writer's attach asked again answered with the generation it
  gave;
- a chunk's incarnation moved on when its number is taken again;
- a replica taken out raising its set's version, and only by the volume's
  writer of now; a set's last active replica never taken out, nor taken
  with a cs down; a cs down dropped as the joiner of its sets, and a member
  down keeping the last version it was active at;
- a join: refused where its set moved on since its plan, one a set at a
  time, in another's place only with room for the set and only where that
  member's cs is gone or drained, holding the room it takes; the member it
  replaces giving its room back, the three in order again, and a joiner
  silent to the writer dropped;
- a divergence said; a snapshot's free list, room reserved, chunks' counts,
  incarnations, counters, joiner and members down each held to what they
  can be; a snapshot written in pieces as in one;
- a member found bad by the scrub (N9e) taken down only while it is up in
  the set -- refused as stale past its version and with a joiner in the set,
  as a join is, and never the last active member, as a replica taken out;
- the controller's five: a member back joining its own place, another
  taking the place of a member gone or drained, a join only where there is
  room, joins waiting for a turn without a cs's change, and a cs silent past
  `out_after` out;
- N10a's twenty-two: a snapshot of a volume only, at the writer's
  generation that paused for it or of none, answered as it is when made
  again; a freeze only while attached by none, giving up the copy-ups
  pending; a clone only of an image or a snapshot, answered as it is when
  made again; a snapshot or a clone counting every chunk it shares; a
  copy-up only by the volume's writer of now and of a volume's, answered its
  pending chunk when asked again and the chunk itself when shared no
  longer, its room taken only where every cs of the set has a slot left, its
  new chunk in the old one's set; a copy-up done only of its pending chunk,
  counting the old one down; a delete giving up the copy-ups pending; an
  image or a snapshot never resized nor attached for writing, and a volume
  made again answered as it is only where it is one; a snapshot's copy-ups
  pending and read-only volumes each held to what they can be -- the meta
  target aims a spoil at each, of a randomness of its own, and its fsck
  holds what a spoiled snapshot taken says. A free chunk's link is aimed at
  the same way, which random bytes reach once in a long while;
- N10b's two: no copy-up into a set being joined, and a join giving up
  its set's copy-ups pending;
- N10c's two: no copy-up into a set with a member drained, and a copy-up
  pending of a chunk named by one map now given up.

The cs's forty-one: a slot's zeros flushed before the record that takes it,
and the slot zeroed at all; an open mark durable before a generation's first
write; copy B written only once copy A is flushed; a fence durable before it
is answered; a close only after the chunk's data; a slot of a number's chunk
before never taken for the one it is now; a request of the chunk before
refused; a request older than the fence refused; a record whose CRC fails
taken as no record; a record's first write torn in copy A taken as never
written; a fence page with a torn copy written again at the open; a device
with anything past a superblock's bytes never taken for blank; nothing of one
volume's read or written through another's unless the mds vouches that its
map names it, and a write of another volume's chunk making it the writer's
volume's; a chunk the cs does not hold read as zeros only once the mds
vouches (N10c); a freed slot given back -- eleven of a copy-up's (N10b): by the
volume's writer of now, of two chunks of one epoch, asked again changing
nothing, its chunk taken only once vouched for, none of a chunk found open
nor of an epoch older than the one the cs knows, the whole chunk copied, its
data durable before its record, its chunk closed and written at its own
epoch, and its slot off the free list once the record is written -- and
twelve of the copy's: a chunk found open no copy's source, and an open mark no client
has written since the join began closed by the copy at its end; a write of
an epoch before the
chunk's refused, though a check of it was vouched for, and a joiner taking no
write of an epoch before its join; a client's write to a set joined marked
before it is written, and a copy writing no sector so marked; a pull moving
the chunk's epoch on, durably, before it reads; a join's marks not those of
the join before it; a copy into a set not joined refused; a chunk the copy
closes closed only after its data is durable, and what it took in made
durable where a client's write left the chunk open at its generation; and a
record of the copy's, at no writer's generation, read back as itself.

The wire's thirty-one: a request read whole, nothing past its end, a flag read
as 0 or 1 alone, and a map asked for past its most read as none; of `serve`,
a chunk found open refused to its writer, a flush refused to a writer with
one and to a writer fenced since, a write vouched for as the joiner's marking
the set from then on, and a found-open chunk written by its set's joiner not
refused as failed, a copy-up's chunk the cs does not hold asked of the mds
first, one the mds refuses not done, and what the mds vouched for taken only
for a chunk the cs holds (N10c); of the mds, the map naming a set's joiner
and saying a place's chunk shared where another map names it (N10c), and a
copy-up's copy vouched for only by an mds as far as its
writer's entry, only where it is pending from that chunk to that one, of
their incarnations, on an active member of the set, of both chunks at the
set's version now, and for the volume's writer of now; of the copy, a
chunk the source freed under it dropping its asks in flight (N10c), a
member back holding every chunk to its source's by sums, those it holds too,
a chunk pulled held to its source's by sums, and a granule that differs held
to the source's a sector at a time; of the scrub, a difference a finding only
when a second look finds every member as the first, only of a chunk closed on
every member and of a set at the version it was asked at, a member unlike the
two others only where those two agree, each finding said once for the set's
version, a member that holds no copy compared as zeros -- not let be -- a
chunk empty only where no member holds it, and a chunk held by none taking
no turn of the pace.

And the client's twenty-eight: a write done only once every replica in has it,
a set's joiner among them; every cs fenced before the writer serves, and one
come into the view while the start fences fenced too; a chunk left open made
the same on its replicas; a read only of an active replica, and one with
none in the view waiting for the map; a flush done only after the flushes
before it, of every cs that acknowledged a write since the last began, and
only of those some set of the volume still writes; a flush, a close or a
fence to a cs no set writes done, not waited on; two writes that overlap
never out at once; what was asked before the stop done before it; a writer
its chunk's last replica refuses failed; a replica that refuses asked out as
refusing though asked out as silent; a set's version from a take-out's
answer taken only one past the view, else the set read again; a write out by
a view a map read finds changed sent again by the new one; a session
detached deaf to the answers of before; an ask-out refused as no replica
of its set having the map read again, which drops every ask about a set
that no longer writes its cs; a close a cs refuses as not in the chunk's
set done; and N10c's seven, the copy-up: a write to a place whose chunk is
shared held while the chunk is copied up, every active member of the set
asked to copy, the copy-up told done only once each has copied or is out,
asked again from its start where a take-out's answer is past the view,
where a member refuses it as of another epoch or map, and where its end is
refused, and a chunk shared no longer written as it is.

`--seed N --seconds S` runs a campaign, of the target `--target` names
(`meta` unless it names `cs`, `wire` or `cluster`). `NBS_CLUSTER_STATS=1`
says what a `cluster` run did -- writes, reads, flushes and closes done and
failed, crashes, takeovers, replicas taken out, the controller's proposals,
joins, drains -- and `--trace` every operation, request and answer, a copy's
among them. `NBS_DEBUG_REPORT=<client>:<from>:<to>` prints a client's report
and its view of the sets (`Session::view`) at each step of a span. `NBS_META_STATS=1` and
`NBS_CS_STATS=1` print each run's answers and steps by kind.

### `hv-linux-test.py` -- a real Linux bzImage, for a while and for good

The third and fourth of the hypervisor demos ([the hypervisor
page](hypervisor.md#what-comes-next)): `hv boot` loads an unmodified 64-bit
Linux `bzImage` by the boot protocol, runs it on a vCPU for a set time, and
reports its serial console. The gate boots nos with a `bzImage` on its root
filesystem and an `/etc/rc` that turns the extension on and runs `hv boot`,
and looks in the report for the kernel's banner and the first lines of its
early setup -- and for what must not be there: an "unchecked MSR access
error", a refusal of an MSR the kernel took for there and read without
being ready for a fault, or an "MCA architectural violation", the panic a
refused machine-check register is. Which MSRs a kernel reads that way
depends on its configuration and on the CPU's vendor, family and model, so
after touching the CPUID and MSR policy run the boot with a kernel that has
machine checks, MTRRs and PAT (`X86_MCE`, `MTRR`, `X86_PAT`, as a
distribution's does), on TCG's `max` and with `--qemu-cpu EPYC-Rome-v1` (a
Zen 2, family 0x17, whose kernel polls the machine-check banks at boot,
reads AMD's NB_CFG and probes LS_CFG) and `--qemu-cpu
Skylake-Client-v1,+svm,+npt,+nrip-save` (an Intel CPU given AMD-V, whose
kernel reads MISC_ENABLE, the microcode revision and the platform ID) --
TCG's `max` is an AMD family 6, which takes few of those paths. The MSRs
the guest was refused, which it was ready for, are printed as a note. And
its kernel must have turned on the x87, SSE and AVX in XCR0, 832 bytes of
them ("Enabled xstate features 0x7"): in XSAVE's standard format under TCG,
compacted on a CPU with XSAVEC -- the AX41's Zen 2 -- which the policy
passes on and Linux then saves with; and where the CPU has PKU -- TCG's
`max`, the EX44's -- PKRU with them, 0x207: 2696 bytes in the standard
format, 840 in the compacted one.

It is a **manual** gate, not in CI: a `bzImage` is megabytes and CI cannot
build one in the time it has, so the gate is pointed at a kernel by hand.
Build a small 64-bit guest -- a `tinyconfig` with the 8250 serial console and
an early console on, PCI, ACPI and SMP off is enough -- and run

```sh
scripts/hv-linux-test.py --bzimage /path/to/bzImage
```

A guest's memory as the platform lays it out past 4 GiB, and from the
huge pool: `--mem` in MiB past 4064 gives the guest RAM from 4 GiB as well as
below the devices' pages, which its memory map must say and its kernel must
take, and `--nos-mem` (QEMU's `-m` for nos, 2G by default) has to be the
guest's and the kernel's reserve more; `--nos-iso` boots nos from another
ISO than nos.iso -- `make nos-test.iso TEST_ISO_ARGS=hugepages=5G` is
nos.iso with that on its command line -- so that every guest's memory is the
huge pool's, 2 MiB an entry of its nested table, and a guest the pool has no
room for is refused. Together:

```sh
make nos-test.iso TEST_ISO_ARGS=hugepages=5G
scripts/hv-linux-test.py --bzimage <bzImage> --initrd <cpio> --nos-iso nos-test.iso --nos-mem 8G --mem 4608
```

The initramfs `--initrd` takes is a static BusyBox and an `/init` that
mounts `/proc` and `/sys`, says `nos-guest: init is up`, turns the kernel's
console messages down (`dmesg -n 1`) and runs `sh` -- a kernel line printed
after the shell's prompt hides the prompt from the module. Which BusyBox
matters three ways. Its line editor has to ask the terminal where the
cursor is (`FEATURE_EDITING_ASK_TERMINAL`): the module types a line at a
guest only once it has heard that question, which a shell asks at its
prompt, and with a BusyBox that never asks, every `hv exec` waits out its
time and says the guest is not at a prompt. `--net` wants its `httpd`; and
its exec has to run a script with no `#!` line through the shell, as
glibc's does, for the `udhcpc -s` script `--net` writes. Ubuntu 24.04's
static BusyBox 1.36.1 does all three; Alpine's `busybox-static` has no
`httpd` and runs no such script; the official `busybox` image's never asks.

Under AMD-V, which on a machine with no hardware SVM is QEMU's TCG, itself
under nothing faster -- so the guest's decompressor and early boot are twice
emulated and slow, and the whole run is a couple of minutes (`--secs` sets
the guest's budget, `--deadline` the gate's patience). With `--initrd` it
checks the guest reaches its `init` and a BusyBox shell, and with `--input
'id\n' --expect uid=0` that the shell runs a command typed at it. The same
guest has been run by hand on the AX41's real AMD-V, where it found what TCG
cannot ([the hypervisor page](hypervisor.md#on-real-hardware)).

With `--initrd` it then goes on to [guests that stay
up](hypervisor.md#guests-that-stay-up), in the same boot: two `hv start`ed
side by side and one stopped mid-boot; `hv exec` typed at the other while it
is still booting -- answered at the prompt printed after the line went in,
not at the one before it -- and again at its prompt, where the echo must come
back whole (the cursor-position answer once made BusyBox wrap it); `hv send`,
`hv wait`, `hv console`; a third guest with no init and `panic=1`, which
panics, reboots, and must stop with its reset as the reason rather than spin
on an 8042 that is not there; `hv off` refusing to turn the extension off
under the running guest; and `rmmod hv` pressing that guest's power button --
the load having said `stop_secs=2` -- and stopping it when the time is up,
before it turns the extension off, which the next load confirms and `dmesg`
shows in order: `nothing in it listens to its power button` for a kernel
without ACPI, `its power button went unanswered for 2 s` for one with. Once
the shell has the console, the kernel's own lines go to the log and not to
it -- so the gate ends on a command's output (`version`) rather than on
`rc: /etc/rc done`, and reads what the vCPU tasks and the unload said with
`dmesg hv:`. `--skip-boot` and `--skip-vms` run one half. The same phase
restarts guests: a stopped one by `hv restart`, one started with `restart`
that resets itself until the sixth reset in a minute leaves it stopped, and
the running one, which must answer a line typed during its new boot. And
one more guest is started with an `input=` and restarted before its first
prompt -- the input held for that prompt, so the boot after throws it away
unread, every time -- and must answer the next line at its prompt: input a
boot throws away once left every later `hv exec` waiting out its time and
then saying the line was not read. The guest ran the line all the same, so
its answer is in the output either way, and the check that holds is that
`hv exec` said nothing of a line not read: a control run with the fix taken
out passed while it looked for the answer alone, and fails it now. Then five
guests are started against one image on nos's root, `/claimed.img`: a guest
whose disk writes it, and then a second writer and a reader, both refused;
the writer stopped, two readers, side by side, and a writer refused under
them -- each refusal saying whose the image is, and every refused start
taking a number all the same, which is what the checks count on. The guests
have no initrd and get no further than their panic, holding their disks the
while. Then one guest started with `log`, whose console nos's log takes,
prints twenty thousand lines as fast as it can (`seq`, the end marked by a
line it computes, `flood-$((6*7))`, so that the shell's echo of what was
typed is not taken for it): nos's log must take fewer than half of them --
a boot's worth and then a rate. A dmesg is a ring that holds the newest two
thousand lines, so what the flood put in it is read from its count of the
lines it pushed out, before the flood and after, the ring's slots added on
top. Of the lines left out, nos says how many once a second at most, and
each notice must name exactly the gaps in `seq`'s numbers since the one
before, over what of the flood the ring still holds. And the guest's own
console must keep the last line. Two of these are bound to time under TCG
-- six resets in a minute, the flood within `--vm-secs` -- and with `--cpus
2` a loaded machine fails them with or without the change under test: back
to back on 2026-10-12, a build with and one without the speculation work
failed the same three, the restarting guest never making six resets inside
a minute in its 500 s, where the one build had passed all of them an hour
before. Run it again on
a quiet machine before reading a failure of those two as the change's.

`--attach` is a boot of its own for `hv attach`: sshd and a guest started
from `/etc/rc`, then an `ssh -tt ... hv attach 0` that types a line, waits
for its answer and types ^] -- the answer must come back, the detach be
said, no cursor query reach the terminal, and the guest run on; `hv attach`
from `/etc/rc`, where nobody can type, must refuse. And a command line of
over 300 characters sent over ssh must run whole: the dispatch once refused
anything past 255, and a distribution's `hv start` is longer. It needs `ssh` and
`ssh-keygen` on the host.

`--disk` is another: an ext4 image with a file in it, put on nos's root and
given to the guest with `disk=`; the guest mounts it, reads the file, writes
4 MiB and a file of its own, syncs, and reads both back after a remount;
writes four files side by side and, its page cache gone with a remount,
reads them back side by side, every sum the same; then deletes what it
wrote and trims its free space (`fstrim`), which must report bytes
trimmed. The report must count the disk's reads, writes, flushes and
discards and no error; the image, held while the guest ran, must say as it
is let go of that it read and wrote off the VFS's lock and gave back 4 MiB
of nos's root at least (`fs::image`); nos's root, which every write of the
guest's went through -- the image's holes filled, its blocks written over,
given back -- must pass `e2fsck` as nos left it, stopped rather than
unmounted; and the image, taken back out of it with `debugfs`, must pass
`e2fsck` too and hold what the guest wrote. It needs a
guest kernel with PCI, legacy virtio-pci, virtio-blk and ext4 built in.
`--nvme-root` puts nos's root on an NVMe disk instead of virtio-blk: each
driver takes ext2's batches of blocks its own way, and NVMe's is the one the
AX41 runs -- and NVMe's is the one with an asynchronous path, so there the
guest's requests go to the disk through the image's queue as they come,
each back when it is done (`fs::image`'s `start` and `reap`; the image says
how many it served so); on virtio-blk the queue serves every one by the
batch.

`--cpus N` gives every guest of the run N CPUs, the boot's and the VMs'.
The boot must say `smp: Brought up 1 node, N CPUs`, and its report that the
guest started N-1 of them; in the VM phase every CPU must be online, each
must have taken its own local timer's interrupts, rescheduling IPIs must
have gone between them, and `hv list` must name the host CPU of each. nos
runs with two cores of two threads (`-smp 4,cores=2,threads=2`) in every
boot of this gate, so that where a guest's CPUs go has a topology to be
wrong on: a guest's cores being its own, a guest of two must be on one
core's two threads and the next guest on the other core. (Where the guests
are all one's own, `hv cores shared` puts a guest's CPUs on cores apart:
two busy loops on one core's two threads ran at three quarters of the
speed, 13 and 12 on the AX41.) In every run the VM phase must put vm 0 and
vm 1 on cores apart, refuse a third guest while the two hold both cores --
saying the free cores hold none, before the guest takes a number -- and
have `hv cores` name which guest holds which CPUs; the image claims, three
guests at once beside vm 0, run under `hv cores shared` and switch it back
after. And `--disk`'s guest must have its disk served on its own core: the
other thread of its CPU's, or, its two CPUs filling that core, the other
core. It goes with
`--disk`, `--nvme-root`, `--net` and `--attach`, whose guests get the same
N CPUs -- they once started theirs with one whatever `--cpus` said -- and
there the guest's `/proc/interrupts` must show its virtio device on MSI-X,
a vector for the queue, with more than one CPU, and on the 8259 (`XT-PIC`)
with one, as a guest of one CPU always was. The guest kernel
needs Linux 6.6 or later with `SMP`, `X86_X2APIC` and `X86_MPPARSE` ([More
than one CPU](hypervisor.md#more-than-one-cpu) says why), and under TCG the
gate wants QEMU 9.2 or later, as `hv-test.py` does.

`--net` is the guests' network: two guests with `net`; each has its port's
address and MAC; a guest pings nos at 10.0.100.1 and each pings the other;
nos pings a guest out of `hv0`; and a page from one guest's `httpd` is
fetched from outside QEMU through `hv forward`. Then where NAT keeps the
guests from: a fetch from the test machine -- 10.0.2.2, on nos's uplink's
own subnet -- and one from 169.254.169.254, a cloud's metadata server, must
each fail at once with "No route to host", and `nat allow 10.0.2.2/32` must
let the test machine through (on the stack before the deny list, the guest
had the page, and NAT mapped its connection to the metadata address). Then
the way out, through nos's NAT: the guest's `/proc/net/pnp` names nos's DNS server (`ip=`'s
`dns0`); it fetches a page and a megabyte from a web server the test runs on
127.0.0.1 -- 10.0.2.2 to QEMU's user network, so beyond nos -- and the
megabyte's md5 must match; it pings 10.0.2.2; `udhcpc` must be given its
port's address, nos as its router and nos's DNS server by the switch; and
`nat` must say it is on from `hv0` through eth0 and count the megabyte's
segments back, say where the guests are kept from, what is let through and
how many packets it denied, `hv list` the address the guests go out from and
the DHCP answers. With `--internet`, a name is also looked up through that DNS server
-- which needs the test machine to resolve names. One question at a time:
BusyBox's `nslookup` asks for A and AAAA at once and matches answers to
questions by their ID alone, which musl takes from the clock's nanoseconds,
and a TCG guest's clock moves in ticks -- both questions got one ID, and one
answer was thrown away as the other's duplicate (the capture showed both
answered). Last, the megabyte again into a guest kept busy by a loop: it
must arrive whole, the switch must have kicked the vCPU out of its guest
for it (`kicks` in `hv list` above 0), and the port must have dropped
nothing. Under TCG only the first two tell anything -- with the kick taken
out, `kicks` reads 0 and the check fails, but slirp never outruns the host's
tick, so no frame was dropped either; the drops show at line rate, on the
AX41. Nor may either guest's traffic so far -- DHCP, ARP, pings, the two
megabytes -- have gone past its share of `hv0`'s backlog or its rate to
every port: those are for a guest that floods, which the `switch` target of
`hv-fuzz.py` makes. And one guest claims the other's address -- `ip addr add` of it,
`arping` from it, a ping from it -- and must get nothing back: the switch
stops what a guest sends as another at its port, so nos, which would answer
both, hears neither; and `hv list` must count the frames against the guest
that sent them and no other. (nos's ARP table is no steady witness: with
the check taken out of the switch the claim reached it, and the other
guest's own ARP put the entry right again -- once before the test could
look, and once not, when the fetch through `hv forward`, which the test
makes last, reached the claiming guest instead of the other's `httpd`.)
IPv6 between the two, which nos does not speak: each must have its address
in `fd6e:6f73:64::/64` -- the prefix of the switch's router advertisement,
the address its MAC's EUI-64 -- no longer tentative; one pings the other at
it and the other pings back at the link-local address; the advertisement
must have put the prefix on the link and given no default route. One guest
adds the other's address and must find it `dadfailed`: its duplicate
detection's solicitation went through and the owner answered. It adds it
again with its duplicate detection off (`accept_dad` 0: BusyBox's `ip` has
no `nodad`) and `ndisc_notify` on, so that it tells the link the address is
its, and pings the other's link-local address from it: nothing comes back,
the other's count of echoes asked of it (`Icmp6InEchos`) is the same after
as before, and its kernel never says another advertised its address -- the
switch stopped every frame from the address, bound to the other's port. A
control run with the bindings never refusing failed those checks. `hv list` must count
them against the guest that sent them, none against the other, and say how
many advertisements it sent and addresses it bound.
Then one guest is stopped with a forward to it and its flows in NAT's table:
its forward has to go with it, the other's stay, and `nat` show no mapping
of its address -- the next VM on its port has its MAC and address, and was
once relayed to and answered for the stopped one. Last, nos's own services
are not the guests': sshd is started on nos, and must give its banner to
the test machine through a port QEMU forwards to eth0 -- so a guest's
refusal is the listener's doing, not a server that never started -- while
a guest's `nc` to nos's port 22, at 10.0.100.1 on `hv0` and at eth0's
address through `hv0`, must be refused. A guest once had the banner both
ways, and four connections that never logged in held every slot sshd has
for logging in -- on the Hetzner boxes, the only console there is. It
needs a guest kernel with networking, virtio-net, packet sockets and `ip=`
configuration built in, and POSIX timers: BusyBox's `ping` paces itself
with `alarm()`, which a `tinyconfig` leaves out, and without it the guest's
`ping` waits for ever after its first answer -- which reads as a network
that stopped.

`--acpi` says the guest kernel has ACPI, with its power button (`ACPI_BUTTON`)
and the input device BusyBox's `acpid` reads it from (`INPUT_EVDEV`), and
checks that it takes the tables the machine gives it ([ACPI](hypervisor.md#acpi)):
its boot must find the RSDP at 0xE0000, the FADT, DSDT and FACS, the PM timer
at 0x608, S5, PIC mode, the PCI host bridge and PCI's interrupts routed by its
`_PRT`, the power button, and the PM timer as a clocksource -- and must say
nothing of an ACPI error or warning, a firmware bug, the ELCR set behind its
back, a PM timer that failed its checks, a TSC the clocksource watchdog
marked unstable, or a PCI interrupt without a GSI. Its reboots must go
through the FADT's reset register (`0x06 to port 0xcf9`, where a kernel
without ACPI pulses the 8042's line). In the VM phase one more guest runs
BusyBox's `acpid`, with a handler that powers off -- a devtmpfs mounted
first, which the initramfs's `/init` does not mount and without which there
is no `/dev/input` for `acpid` to read the button from (the gate failed so,
the button unanswered, until it mounted one), the button's `event` device
then checked for: its SCI must be IRQ 9 on
the 8259, the PM timer one of its clocksources, and `hv stop` must end it as
`the guest powered itself off`, the report counting the press; and one more
without `acpid`, which hears the button and does nothing, must be stopped
when `secs=5` is up, `its power button went unanswered for 5 s`. Without
`--acpi`, a guest's `hv stop` must say nothing in it listens to its power
button. In both, after the reload, two guests at their shells are stopped
together by `hv stop all secs=3`, which must press both buttons at once and
say of each what became of its press. `--acpi` goes with `--cpus`, `--disk`, `--net` and `--attach`, whose
guests then find their devices through the DSDT's host bridge. With
`--disk`, the guest mounts its disk once more, writes a file to it and is
left so, `acpid` given a handler that unmounts it and powers off -- and nos
itself is powered off: its unload must press the button and say so, the
guest unmount the disk and turn itself off, `stopped for the unload -- the
guest powered itself off`, before nos unmounts its own root and goes; and
the guest's ext4, taken out of nos's root, must be clean -- its state
clean, no journal left to replay -- and hold the file written last. A guest
stopped where it was, as every one was before the unload asked, leaves its
journal to replay and that file in a page cache that is gone.

`--xapic` has the guests' local APICs come out of reset in xAPIC mode (`hv
start ... xapic`), and a kernel of more than one CPU then reaches them
through the page, every access a nested fault the hypervisor decodes and
performs ([MMIO](hypervisor.md#mmio)): the boot must say so, its report
counting the accesses, and every check of the VM phase -- the IPIs, the
timers, a guest's MSI-X with `--disk` and `--net` -- holds as it does in
x2APIC mode. A `--cmdline` with `nox2apic` is the same from the other
side: the APICs come out in x2APIC mode and the kernel takes them out of
it. It is what has a Linux 6.1 without ACPI (`acpi=off`) bring up more than
one CPU: its MP-table path reads the page before it looks at the mode
firmware left. The kernel
the gate was brought up with is the 6.18 tinyconfig above plus `ACPI`,
`ACPI_BUTTON`, `X86_PM_TIMER`, `INPUT` and `INPUT_EVDEV`.

`--ioapic` (with `--cpus 2` or more) gives the guests an IO-APIC (`hv start
... ioapic`, [The IO-APIC](hypervisor.md#the-io-apic)), their APICs in
xAPIC mode with it: the boot must find it -- `IOAPIC[0]: ... version 32,
address 0xfec00000, GSI 0-23` -- and its report count interrupts sent
through it; in the VM phase `/proc/interrupts` must have the timer's
ticks on its pin 2 and the serial port's on pin 4, each taken. With
`--acpi` the kernel routes by it, `Using IOAPIC for interrupt routing`, its
SMP configuration all from the MADT, and the SCI must come in as a level
on pin 9. A `--cmdline` without `no_timer_check` has Linux count the
timer's ticks through it before it trusts it; one with `pci=nomsi` puts a
virtio device's interrupts on its INTx line, and `--disk` and `--net` must
then find them level-triggered through the IO-APIC -- sent again at each
EOI while the device's status is unread, which a lost EOI or a remote IRR
left set would stall the disk or the network over.

### `hv-distro-test.py` -- a distribution, as it ships

hv-linux-test's guest is a kernel built for the purpose. This one is a
distribution's, built for every machine instead: Alpine's `virt` ISO, its
kernel and initramfs taken out of it with `xorriso` and the ISO itself given
to the guest as a read-only disk (`disk=...:ro`), unmodified. Also manual --
the ISO is a download -- and run as

```sh
scripts/hv-distro-test.py --iso alpine-virt-3.24.2-x86_64.iso
```

It checks that Alpine's initramfs finds its ISO and OpenRC brings it to a
login prompt; that root logs in by `hv send` (a getty asks for no cursor, so
`exec` alone would never type at it) and it is Alpine on its `virt` kernel;
that its clock is the host's, from the emulated RTC, to within minutes; that
the ISO is read-only to it -- the driver says so and a write fails; that
`reboot` resets it, `restart` boots it again, and root logs in again; that
its initramfs configured eth0 from the VM's `ip=`, it pings nos and nos
pings it; that its `resolv.conf` names nos's DNS server and it fetches a page
from the test machine through NAT -- which `/etc/rc` lets through first,
`nat allow 10.0.2.2/32`, the test machine being on nos's uplink's own subnet
-- (with `--internet`, it also looks Alpine's
mirror up by name and `apk update`s from it); that `apk add openssh-server`
installs from the ISO; and that the
test, from outside QEMU, logs into the guest's own sshd through QEMU's
forward, nos's `hv forward` and the switch. Two boots and apk under TCG:
about five minutes. It needs `xorriso`, `ssh` and `ssh-keygen` on the host.

With `--debian debian-13-nocloud-amd64.raw` (the .qcow2 through `qemu-img
convert -O raw`) it boots Debian's cloud image the same way -- its kernel
and initrd read out of the image's `/boot` with `sfdisk` and `debugfs`, the
image, copied sparse, the guest's writable disk -- with systemd-firstboot
given the root password, locale, keymap and timezone as credentials on the
kernel command line; and checks that root logs in with that password, that
`systemctl is-system-running` says running with no unit failed, that its
root is `/dev/vda1`, ext4, read-write, that its clock is the host's, that
glibc's own loader finds the CPU x86-64-v3 (`ld.so --help`: AVX and AVX2,
FMA, F16C, BMI and the rest, what a distribution built for it needs); that
systemd-networkd, given a `.network` for its ethernet as one more
credential, takes its port's address, nos as its router and nos's DNS server
from the switch's DHCP server, reaches nos, and fetches a page from the test
machine through NAT with curl (with `--internet`, resolves Debian's mirror
and fetches from it); and that nos reaches it, and
that a file written to its root is still there after `reboot` (waited for
with `hv wait ... boot=1`, since systemd's `reboot` hands the shell its
prompt back first); and that `hv stop` presses its power button and
systemd-logind has it shut down and turn itself off.

Both boot on the machine's ACPI tables, as their kernels are built to;
`--acpi-off` boots them with `acpi=off` instead, the machine as it was
before it had them -- the MP table, PCI by configuration mechanism 1, no
power button -- and then Debian's `hv stop` must find nothing listening.

Both modes also time the guest's clock against nos's across 40 s of idle --
its `/proc/uptime` against nos's `uptime` -- and want them within 10%: an
idle guest whose vCPU was woken only at the host's tick was once given a
timer edge for every two or three of its periods, and counted 40% of real
time on the AX41 (0.514 under TCG, which is what this check read before
the fix and reads 1.000 after). The ±5 minutes the date is allowed could
not see it.

Under TCG the guest cannot calibrate its TSC against the PIT -- an exit
costs more than the calibration loop allows -- so it stays on jiffies and
the PIT's periodic mode. `--cmdline-extra "tsc_early_khz=<kHz>
tsc=reliable"` puts it on the TSC and high-resolution timers, which drive
the PIT in one-shot mode, as a real CPU's calibration does by itself.

`--ubuntu` boots Ubuntu 24.04's server cloud image (made raw) as a cloud
does: its kernel, initrd and GRUB's command line (`root=LABEL=cloudimg-rootfs
ro console=tty1 console=ttyS0`) read out of its own /boot partition and
given unchanged, and cloud-init provisioning it from a NoCloud seed -- an ISO
labelled `cidata`, the VM's second, read-only disk -- with root's password
and the machine's name. It is checked as Debian is -- systemd running with
no unit failed, its root read-write and kept across a reboot, the network
and the way out, `hv stop` turning it off -- but its network comes up from
the `ip=` hv gives a networked guest, which its initramfs sets and
cloud-init keeps, rather than by DHCP.

`--hvboot` boots Debian's and Ubuntu's images the way a disk boots, with
nothing taken out of them: the VM's kernel is hvboot (`out/x86_64/hvboot`,
which `make` builds), which reads the image's own grub.cfg, picks the entry
GRUB would boot, and boots its kernel and initrd off the image's own /boot
([A guest's own kernel](hypervisor.md#a-guests-own-kernel-pvh-and-hvboot)).
The VM's command line is only what the profile adds after the entry's --
Debian's credentials; Ubuntu, nothing. The checks are the same, and two
more: hvboot reached the kernel -- the last line it prints before the jump
-- and its console says it booted the default entry of the image's own
grub.cfg. Both, under TCG on the Mac (2026-10-24): 23 of 23, hvboot at the
kernel 0.6 s after the VM starts.

`--cpus N` gives each guest N CPUs, and their local APICs -- the command
line then without the `nolapic` a guest of one CPU is given -- and every
one must come online; with `--ioapic` too, an IO-APIC, and the serial
console's interrupts -- the getty's, the shell's -- must have come through
its pin 4 ([The IO-APIC](hypervisor.md#the-io-apic)): a distribution's own
kernel, not one built for the gate, routing by it.

`--create <url>` is a VM the way a cloud makes one, with nothing done on the
host: `hv create cloud <url> size=4G password=...` has nos fetch the image
itself, over HTTPS and through its redirects, make it the disk and grow it,
write cloud-init's seed -- the key the test put in nos's
`/etc/ssh/authorized_keys`, the password -- and the vm.toml
([A VM of a cloud's image](hypervisor.md#a-vm-of-a-clouds-image)); `hv start
cloud` boots it by hvboot. cloud-init must print that it finished; the
image's own user (`debian`, `ubuntu`) logs in on the console with the seed's
password; `cloud-init status --long` says done, from NoCloud; the machine
is named `cloud`; the user's authorized_keys has nos's key; the disk is the
size create grew it to and the root grew into it; the seed is a disk
labelled `cidata`; the user may sudo; and `hv stop`'s power button turns it
off. Debian's genericcloud image is the small one to run it with:

```sh
scripts/hv-distro-test.py --create https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2
```

12 of 12 under TCG on the Mac (2026-10-25): the image fetched in 35.5 s
(9.1 MiB/s), converted in 51.5 s (907 MiB of 3,072 written), and cloud-init
done 49.9 s after the VM started, from `DataSourceNoCloud [seed=/dev/vdb]`.
The user is typed only once the serial getty's own prompt is there:
Debian's comes after cloud-init's last line, and agetty throws away what
was typed before it opened the port -- the first run lost the user that
way, and logged in as the password.

## The hardware NIC drivers

`tcp-test.py` and `netload-test.py` take `--nic igb` (and so does
`netblk-test.py`, on x86-64), which swaps virtio-net for QEMU's 82576 (QEMU
8.0 or later; CI's does not have the device, so this one is run by hand). It is the only way any of the
three hardware NIC drivers runs anywhere but on a Hetzner box -- MSI-X with
two vectors, the descriptor rings, the receive poll and its interrupt
throttle -- so run `tcp-test.py --nic igb` and `netload-test.py --nic igb`
after touching `drivers/net/igb` or the driver seam in `layers/net/src/device.rs`.

`r8168` and `r8125` have no emulation at all. A change to either is checked
by the compiler and then on the EX44, whose only console is that NIC: a
mistake there is a machine that has to be rescued by hand. Keep their
register sequences as they are. See [Real hardware](real-hardware.md).
