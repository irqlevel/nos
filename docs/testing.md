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
moves. And on every CPU `test_sleep_prompt` has a task of its own sleep a
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
blk, net and rng. Both boot with `fstest=on`. The arm64 one boots with
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
| `nanofs-test.py` | arm64 | nanofs, the halt |
| `disklog-test.py` | arm64 | the disk log |
| `netconsole-test.py` | arm64 | the trace path, a net device, netconsole, what reaches the network as the machine halts |
| `tcp-test.py` | arm64 | TCP, the HTTP client |
| `sshd-test.py [--arch aarch64\|x86_64]` | both | TCP's listening side, the module loader, the `ffi` declarations, the terminal a session lends a command, `edit` in nos; on arm64 the halt |
| `netblk-test.py [--arch x86_64\|aarch64]` | both | the block layer's asynchronous path, the C ABI a module reaches `block` and `net` through |
| `netload-test.py [--arch aarch64\|x86_64]` | both | the receive path, the frame pool, `modules/netload`, `kcore::net`'s listener, the tick's receive poll (`rxpoll`) |
| `usb-test.py` | x86-64 | `drivers/usb/`; what a keyboard sends the shell's line editor |
| `hv-test.py [--arch x86_64\|aarch64]` | both | `hv`, `hvarch`, `modules/hv` -- the hypervisor; on x86-64 a PVH image too, hvboot started at its 32-bit entry, and `hv create` of VMs from a web server's images; on both, `hv convert` |
| `edit-test.py [--seed N] [--sessions N] [--seconds S] [--session N]` | host (CI) | the editor (`src/crates/edit`), from its own sources: key sessions held to what they leave in the file and on the screen, files read and saved back byte for byte, random sessions on a terminal of the test's own |
| `diskimage-test.py [--image <qcow2>] [--iterations N]` | host (CI) | the disk image converter (`src/crates/diskimage`, `hv convert`), from its own sources: qcow2 images qemu-img and qemu-io write -- versions 2 and 3, clusters of 512 bytes to 2 MiB, stored, compressed, extended, zero, discarded, preallocated, snapshotted, dirty -- converted and held byte for byte to what qemu-img reads, its holes kept; what it must refuse; its DEFLATE decoder against zlib; each image's metadata fuzzed; and the cloud-init seed's ISO 9660, read back by xorriso or bsdtar, blkid and Linux's isofs |
| `hvboot-test.py [--image <disk.raw>] [--iterations N]` | host (CI) | hvboot's partition, ext2/3/4 and GRUB readers and its bzImage header, from their own sources: on disks it makes -- GPT, MBR and none, ext4, ext3 and 1 KiB-block ext2, a grub-mkconfig grub.cfg and grubenv -- the entry GRUB would boot and its files byte for byte, then each disk's metadata fuzzed |
| `insn-test.py` | host (CI) | the hypervisor's MMIO decoder, its decoder of a `mov` of a control register, and its guest page walker (`hv/src/{insn,walk}.rs`), against the encodings clang gives |
| `hv-fuzz.py [--seed N --seconds S]` | host (CI) | anything a guest reaches in the hypervisor -- its devices, local APIC and IO-APIC, the MMIO path, the Linux loader and its ACPI and SMBIOS tables, the CPUID and MSR policy, the run loop that dispatches its exits, on every CPU of a guest of several, and the guests' network: the switch, its DHCP server and NAT -- fuzzed with overflow checks on; and the virtio notifications, kicks and interrupts with EVENT_IDX and without, held to a Linux driver's every request served (`virtq`) |
| `net-fuzz.py [--seed N --seconds S]` | host (CI) | anything the network hands the kernel -- the receive path, ARP, ICMP, UDP and TCP; the DHCP client and the DNS resolver; the HTTP client over TCP and over TLS; the UDP shell; netconsole; `sshd` over the `ssh` crate, and the session's line editor -- fuzzed with overflow checks on, each protocol against a model of it |
| `fs-fuzz.py [--seed N --seconds S]` | host (CI) | anything a disk hands the kernel -- the partition tables, the disk log's area, ext2 and nanofs images sound and damaged -- and the storage layers above it: the block table's bounds and claims, the VFS and its C ABI, the file ABI, the shell's storage commands, `root=`; fuzzed with overflow checks on, the filesystems against a model of the tree, e2fsck's judgement and power cuts |
| `cpp-fuzz.py [--seed N --seconds S]` | host (CI) | what firmware, a bootloader or a disk hands the kernel's C++ -- the device tree and what `Board` takes of it, the memory map, the GRUB environment block, a module's `.ko`, the x86 boot's Multiboot2 tags, microcode file, ACPI tables and command line, and the printf every line is formatted by, and what a terminal types at the console's line editor -- and the memory management under everything: the page tables and the physical page allocator past `setup`, the kernel heap; the kernel's own sources under the address and undefined-behaviour sanitizers, each against a model of what its header promises |
| `hv-linux-test.py --bzimage <img> [--initrd <cpio>]` | x86-64, by hand | the Linux loader, the CPUID/MSR policy, the emulated devices -- a real kernel to its shell; with an initrd, guests that stay up and the commands that reach them, and a VM kept in a folder (`hv create`, `hv start <name>`, its `console.log`), in `/vms` and in a folder `insmod /hv.ko vms=` names; `--net`, the guests' switch, NAT, its DHCP server and the DNS server they are given; `--cpus N`, guests of N CPUs, their local APICs and IPIs; `--xapic`, those APICs in xAPIC mode, every access of theirs by MMIO; `--ioapic`, an IO-APIC routing the timer, the serial port, the SCI and -- with `pci=nomsi` -- virtio's INTx; `--acpi`, a guest kernel with ACPI: the tables it is given, the PM timer and the SCI, the reset register, and `hv stop`'s power button -- with `--disk`, the one nos's own poweroff presses, the guest's disk left mounted coming back clean; `--qemu-cpu MODEL`, nos on a named TCG CPU -- `EPYC-Rome-v1`, `Skylake-Client-v1,+svm,+npt,+nrip-save` -- whose family and model put the guest's kernel on that CPU's paths through its MSRs; `--mem` past 4064 with `--nos-mem`, a guest with RAM from 4 GiB; `--nos-iso`, nos booted from `make nos-test.iso TEST_ISO_ARGS=hugepages=...`, every guest's memory the huge pool's, 2 MiB an entry of its nested table |
| `hv-distro-test.py --iso <alpine-virt.iso> [--debian <nocloud.raw>] [--ubuntu <cloudimg.raw>] [--internet] [--cpus N [--ioapic]] [--hvboot]` | x86-64, by hand | a distribution as it ships -- Alpine's kernel, initramfs and packages, its ISO a read-only disk: login, clock, reboot, network and the way out through NAT, and its own sshd reached from outside; Debian's cloud image, systemd provisioned by credentials, networkd by DHCP, its root written to and kept across a reboot -- both on their ACPI (`--acpi-off`: without), Debian shut down by `hv stop`'s power button; with `--hvboot`, Debian's and Ubuntu's booted by hvboot off their own /boot, nothing taken out of them; with `--create <url>`, a VM made by `hv create` of a cloud image nos fetches itself, booted and provisioned by cloud-init from nos's seed |
| `idle-wait-test.py [--smp N]` | x86-64 | a wait primitive, the scheduler's choice of the idle task |
| `image-test.py [--image FILE]` | x86-64 (CI) | the system image and what pours and boots it: `make image`, `build/image/`, `nos-install`, `root=UUID=` |

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
read past its end is refused, and that a filesystem mounted on a partition
keeps writers off the disk it is on. The smoke disks carry no partition
table at all, so nothing else reaches this code.

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
this test, `nanofs-test.py` and `sshd-test.py` stopped reading the log at
`Stopping cpu`; now these three are what judges the arm64 halt. Once they
looked, they found it faulting still, in two runs of three: a parked CPU's
halt was a call, whose frame record every wakeup from WFI wrote onto the
stack the CPU had been interrupted on -- one the boot CPU frees. A parked CPU
stands on a stack of its own now, and halts in a loop that touches no memory
(`Cpu::park`).

### `nanofs-test.py` -- nanofs

nanofs is the kernel's own small checksummed filesystem, and nothing else in
the suite touches it: the smoke boots and `ext2-test.py` both run on ext2.
The gate is the `fstest` self-test over a freshly formatted disk, then files
written, synced, unmounted and read back from a second mount, so that what
is judged is what reached the disk rather than what a cache still held --
and last the image parsed by the script itself. A driver reading back its
own writes never notices a field at the wrong offset; a second
implementation of the format does. Its `poweroff` has to switch the machine
off with nothing panicked on the way down, as `ext2-test.py`'s does.

### `disklog-test.py` -- the kernel log on a disk

Boots with `disklog=on` over an area it prepares itself, then reads the area
back with `scripts/disklog.py`: the boot's first fifty traced lines compared
with the serial console's, line for line. It checks both halves of the
channel -- that the kernel finds the area, claims the device (so a mount and
a format of that disk are refused) and writes whole sectors with no
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
ext2 root that already carries the module, a key and an `/etc/rc`, and logs
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
boot must answer with the same host key and, now, on the UDP shell. Last,
`status`, `rollback` and `uninstall`, and e2fsck once more. Needs
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
as they are -- `hv/src/devices/*`, `lapic`, `acpi`, `insn`, `walk`,
`linux`, `mmio`, `policy`, `smp` and `run`, hvarch's VMCB layout, the
module's switch and DHCP server (`modules/hv/src/{net,dhcp}.rs`) and NAT
(`net/src/nat.rs`) -- over stand-ins for the kernel (a clock the fuzzer
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
`config` holds a VM's `vm.toml` (`modules/hv/src/config.rs`): whatever the
text, the parser does not panic, and what it reads `render` writes back so
that it reads the same again; a configuration made up the input's way --
its strings full of quotes, backslashes, control characters and characters
past ASCII -- comes back from its own rendering whole, and that rendering
cut, spliced and scribbled on is still read without a panic. A control run
with backslashes left unescaped was found at the 4th input.
`relay` holds `hv forward`'s relay (`modules/hv/src/relay.rs`) between two
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
`place` holds where guests are put on the host's cores (`hv/src/place.rs`)
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
mutants of `hv/src/dr.rs` -- a write into another register, a read of 0,
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
virtual switch does (`ingress` in `modules/hv/src/net.rs`): IPv4 or ARP
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
  plain or bulk, and the peer scales its window or not, at any shift -- past
  RFC 7323's 14 too -- so that the window the machine offers runs past 64
  KiB and must never shrink, not even by the rounding of its units, and what
  the link reorders is kept past a hole and taken in when it fills; and a
  blind reset in the window, past the edge of everything the peer sent, must
  change nothing (RFC 5961). And now and then a second
  device, eth1, standing for the hypervisor's switch, from which a guest
  sends a SYN to a port the machine listens on -- at eth1's address or at
  eth0's: a listener takes connections from its own device alone, so no
  SYN-ACK may go out to it on either wire (the first input of the control
  run with the device left out of the listener's lookup got one);
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
being read.

The images are the fuzzer's own (`src/image/`), made from the formats and
not from the drivers: an ext2 as mke2fs lays one out -- every block size,
one group or many, sparse superblocks or not, 128- and 256-byte inodes --
with a tree put on it, files with holes and without, directories with
deleted entries between their live ones; a nanofs as `format` and the
driver leave one; MBRs and GPTs; the disk log's header. And judged the same
way: `ext2::check` reads an image as e2fsck -fn does -- every inode with
links in use, every block claimed once, directories parsed, link counts,
bitmaps and every count -- and sorts what it finds into *corrupt* (what no
crash of a correct driver may leave: a block in use and free, a block two
inodes hold, a name leading to an inode not in use) and *unclean* (what
e2fsck still fixes: a leak, a count, a link count, the state bit). The two
were checked against e2fsprogs over hundreds of images (`FS_FUZZ_DUMP=<dir>`
writes each image judged): what `check` calls clean, e2fsck passes; what
e2fsck faults, `check` calls unclean or corrupt. The targets:

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
- `ext2` and `nanofs`: sound images mounted, read-only and not, and worked
  through the VFS -- opens with every flag, reads, writes and seeks at every
  edge of a block and of the indirect blocks of every level, across 2 and
  4 GiB and at the end of what a file can be, truncates both ways, creates,
  renames, removes, chains of directories forty deep -- each call's answer
  held to a model of the tree and the files open on it (`targets/fsops.rs`,
  the VFS's rules and the filesystem's limits written down), the tree read
  back through the VFS held to the model's, the image a clean unmount
  leaves clean and holding the model's tree, and mounted again, the same. A
  call the filesystem refused for want of room is held to having changed
  nothing -- a write to having left the file its size, a first part of the
  new data in it at most -- and now and then the filesystem is filled to its
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
  cut is at worst unclean -- which is what ext2's commit order and nanofs's
  copy-on-write promise -- and so is a run in which requests failed;
- `ext2bad` and `nanofsbad`: sound images with a few fields made what they
  must not be -- the superblock's geometry, a group's descriptor, an inode's
  mode, size, links and block pointers (outside, into the metadata, into
  another file, at itself), a directory's entries, an indirect block, a
  bitmap; nanofs's with their checksums put right, as a lie told well is --
  mounted if the driver takes them and worked through the VFS, held to what
  holds for any image: a directory that lists the same twice running, a name
  it lists that can be looked up, a file that reads no more than its size, a
  walk of it all that changes nothing leaving the kernel's heap as it found
  it, nothing read or written once it is unmounted;
- `vfs`: a ramfs at the root held to the model, and procfs, ext2, nanofs
  and ramfs mounted beside it and taken down again at paths nested,
  doubled, without a slash and too long; tasks of their own at work on those
  meanwhile; the C ABI (`kernel_vfs_*`) handed words that are open files,
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
  filesystem the model holds (`Fs::compare`, so ext2's and nanofs's targets
  too); two mounts for files on two -- an ext2's and a nanofs's numbers are
  alike; and a word that is no open file told so. A
  control run with the node's key left out of a ramfs file's identity
  failed at the first input, and one with the mount left out at the 41st;
- `shell`: the storage commands the console, the UDP shell and SSH run --
  `mount`, `format`, `cp -r`, `find`, `fstest`, `diskwrite` and the rest -- with
  arguments of every kind, after `fstest` has passed on ramfs, ext2 and
  nanofs as each came; every disk no command wrote to raw clean at the end;
- `rootfs`: `root=` none, `auto`, a device, a label, a UUID, over disks and
  partitions carrying an ext2 labelled `nos` or not, a nanofs, something that
  only looks like ext2, or nothing; what is mounted where held to what
  `rootfs.rs` documents, and what it mounted for writing clean after the
  shutdown's unmount.

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
- ext2's remove was a recursion capped at 32 levels and nanofs's one without
  a cap, and nanofs's mount read the tree by a recursion that stopped at 32:
  ext2 made a tree it could not remove, and nanofs one whose deep part was
  gone at the next mount. All three are walks now, with no limit but the
  tree's;
- an ext2 directory whose load failed part way -- a block past what the
  driver maps -- kept what it had read so far and read it again at every
  look: every `ls` of it grew the tree, for good. A load is all or nothing
  now; `ext2bad` and `nanofsbad` walk every image three times and hold the
  kernel's heap to what it was after the second;
- names with a NUL in them went onto disk, which e2fsck calls illegal and
  nanofs cuts at the NUL; the VFS refuses them;
- a nanofs mounted read-only wrote its superblock at the unmount, and at the
  mount when it repaired a bitmap;
- `format nanofs` on a device too small for nanofs wrote its first two
  blocks over whatever was there before it failed; format and mount now
  refuse such a device first;
- nanofs checked a file's whole checksum on every read, so reading a file a
  piece at a time cost the square of its length -- a megabyte through `cat`,
  256 reads of all 256 blocks; now once after a mount or a write;
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
  is handed out, and the pool's counts its bitmaps';
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
  to come back to the tenth.

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
three leaving no folder behind.

### `hvboot-test.py` -- what hvboot would boot, decided on the host

hvboot reads a guest's partition table, its ext2/3/4 /boot and its GRUB
configuration, and boots the entry GRUB would: get any of that wrong and
the guest boots something else -- an older kernel, the recovery entry --
with nothing to say so. `fuzz/hvboot` compiles that code, its own sources
(`src/crates/hvboot/src/{gpt,ext,grub,bzimage,crc32}.rs`), into a host
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

### `edit-test.py` -- the editor, decided on the host

`edit` is a full-screen editor run in the kernel (`src/crates/modules/edit`,
[edit](modules.md#edit)) over a crate with no kernel in it,
`src/crates/edit`. `fuzz/edit` compiles that crate, as it is, into a host
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
- the DEFLATE decoder (`diskimage/src/inflate.rs`) against Python's zlib --
  the zlib QEMU decodes compressed clusters with: streams zlib makes at
  every level and strategy, with flushes inside, asked for the bytes they
  hold, fewer and more; the same with a bit, a byte or the end changed;
  streams the script writes itself, block by block -- stored, fixed and
  dynamic, their codes complete, single, empty or broken, repeats before
  any length or past the end, matches too far back, symbols no stream may
  hold, the end of block missing; and noise. Each is asked for a number of
  bytes, as QEMU asks for a cluster, and every answer -- good or bad, and
  the bytes of every good one -- must be zlib's;
- each of five images with its header, L1 and L2 tables and clusters
  changed at random, up to eight bytes at a time, or cut short: no panic
  (overflow checks on), no read outside the image, no write outside the
  disk or out of order, every run ending; and the compressed images with
  only their compressed data changed, converted by qemu-img too -- the two
  must give the same answer every time, and where both convert one, the
  same disk.

And the seed `hv create` gives cloud-init (`diskimage/src/iso.rs`): ISO 9660
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

Nineteen mutants of the two crates -- a key twice, a leading zero or a lone
surrogate taken; a big count written as a number, a quote not escaped; a
parameter not taken let through; a bare LF, two Content-Lengths, a
Transfer-Encoding, a body not waited for, a CR in a header written; an
unmasked frame, text not UTF-8, a message past its limit in fragments within
it; SHA-1's round constant; GeneralizedTime from 2051, a client's
certificate for servers, a serial's top bit set; a leading zero in an
address -- each failed it. Needs cargo and openssl; `--seed N --iters N` for
a campaign.

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
AX41 runs.

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
after touching `drivers/igb` or the driver seam in `net/src/device.rs`.

`r8168` and `r8125` have no emulation at all. A change to either is checked
by the compiler and then on the EX44, whose only console is that NIC: a
mistake there is a machine that has to be rescued by hand. Keep their
register sequences as they are. See [Real hardware](real-hardware.md).
