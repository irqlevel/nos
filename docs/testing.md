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

Self-tests live in `src/cpp/kernel/test.cpp`; each is a
`Stdlib::Error TestXxx()` function. They run early in boot, from
`Test::Test()` (called in `main.cpp:Main2`, and from `main_arm64.cpp`) and
`Test::TestMultiTasking()`, and a failing one returns a non-success
`Stdlib::Error`. The Rust side's run from the same place, through
`rust_test` (`src/rust/kernel/src/lib.rs`).

- To add a test, write a `TestXxx()` and register it in the `Test()`
  dispatcher.
- To run a single test, edit `Test()` to call only that function, rebuild,
  boot, and watch `nos.log`.

Two more run at boot when there is something to run them on: `TestModules`
loads the `modtest` module embedded in the image and then feeds the loader
damaged copies of it (see [Loadable modules](modules.md#testing)), and
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
blk, net and rng. Both boot with `fstest=on`.

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

| Gate | Arch | Run it after touching |
|---|---|---|
| `wx-test.sh [--arch x86_64\|aarch64]` | both | a mapping path |
| `parttest.py [--arch aarch64\|x86_64]` | both | the block layer, the partition tables |
| `ext2-test.py` | arm64 | ext2 |
| `nanofs-test.py` | arm64 | nanofs |
| `disklog-test.py` | arm64 | the disk log |
| `netconsole-test.py` | arm64 | the trace path, a net device, netconsole |
| `tcp-test.py` | arm64 | TCP, the HTTP client |
| `sshd-test.py [--arch aarch64\|x86_64]` | both | TCP's listening side, the module loader, the `ffi` declarations |
| `netblk-test.py [--arch x86_64\|aarch64]` | both | the block layer's asynchronous path, the C ABI a module reaches `block` and `net` through |
| `netload-test.py [--arch aarch64\|x86_64]` | both | the receive path, the frame pool, `modules/netload`, `kcore::net`'s listener |
| `usb-test.py` | x86-64 | `drivers/usb/` |

### `wx-test.sh` -- W^X

One boot per probe (`wxprobe=text`, `wxprobe=heap`), each of which has to die
on the fault it asked for, at the address it printed. A kernel that survives
a probe prints `SUCCEEDED (W^X broken!)` and the script fails. No smoke boot
notices a mapping that stops being NX -- everything still works -- and the
heap probe is the one that fails if a mapping path ever stops setting NX on
the leaves it writes. `--skip-build`; `WX_HVF=1` for the arm64 boots under
HVF. See [Paging](paging.md#mmio-cacheability-and-wx).

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

### `nanofs-test.py` -- nanofs

nanofs is the kernel's own small checksummed filesystem, and nothing else in
the suite touches it: the smoke boots and `ext2-test.py` both run on ext2.
The gate is the `fstest` self-test over a freshly formatted disk, then files
written, synced, unmounted and read back from a second mount, so that what
is judged is what reached the disk rather than what a cache still held --
and last the image parsed by the script itself. A driver reading back its
own writes never notices a field at the wrong offset; a second
implementation of the format does.

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
drain task is not running to send it. On the two Hetzner machines netconsole
is the only console there is. See [Netconsole](netconsole.md).

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
with a terminal and one without, 3 MiB of output through the client's window
with rekeys in the middle, a key it has to refuse, sessions at once, a burst
of connections past the four that may be logging in, a stop and an `rmmod`
from inside a session; then it puts the module in `/etc/rc`, reboots, and
checks it came back by itself with the same host key. On x86-64 it boots an
ext2 root that already carries the module, a key and an `/etc/rc`, and logs
in. It is also what notices a change to the `ffi` declarations that a module
was not rebuilt against: a module's header carries a digest of them. Needs
`ssh`, `ssh-keygen` and host ports 2222, 8000 and 9000 free.

40 checks pass. One fails, `poweroff unloads sshd before the unmount`, for
reasons of its own that have nothing to do with TCP. A run that fails
anything else is a regression.

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

It takes about six minutes: 88 round trips with a three-second collection
window each, by design.

`--arch x86_64` is the short form, a minute of it: `nos.iso`, a root that
carries the module and an `/etc/rc` that loads it, starts the target and
runs the source once -- no shell at all, the x86-64 boot having none over
UDP. It checks what differs between the architectures rather than what does
not: that the module loads and binds there, that its calls across the C ABI
come back right where the ABI is the other one -- two of them return a
structure by value -- and that the target echoes and the source's run
arrives whole. Both forms take `--nic igb`.

### `usb-test.py` -- typing at the kernel

On the Dell Latitude -- UEFI, no serial port, no PS/2 -- the xHCI controller
and the HID boot keyboard on it are the only way in, and no smoke boot
attaches a USB controller at all. The gate boots with an emulated xHCI
controller, two keyboards -- one on a root port, one behind a hub -- and a
device that is neither, then **types at the kernel through QEMU's monitor**
and reads the shell's answer off the serial console. It is built to catch
the one failure that looks like success: a driver that enumerates perfectly
and delivers no report leaves every boot-log check passing, and fails on "a
command typed on the usb keyboard reaches the shell".

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
