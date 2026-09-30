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
`rust_test` (`src/rust/kernel/src/lib.rs`) -- among them the frame test
(`kernel/src/frames.rs`): a page mapped nowhere, written a piece at a time at
every alignment and read back on every running CPU, each through its own
slot of the temporary window, which is how every copy of a guest's memory is
made. A slot that maps the wrong page fails it (`frame selftest: ...`), and
so the smoke boot, on both architectures.

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
| `parttest.py [--arch aarch64\|x86_64]` | both | the block layer, the partition tables |
| `ext2-test.py` | arm64 | ext2 |
| `nanofs-test.py` | arm64 | nanofs |
| `disklog-test.py` | arm64 | the disk log |
| `netconsole-test.py` | arm64 | the trace path, a net device, netconsole |
| `tcp-test.py` | arm64 | TCP, the HTTP client |
| `sshd-test.py [--arch aarch64\|x86_64]` | both | TCP's listening side, the module loader, the `ffi` declarations |
| `netblk-test.py [--arch x86_64\|aarch64]` | both | the block layer's asynchronous path, the C ABI a module reaches `block` and `net` through |
| `netload-test.py [--arch aarch64\|x86_64]` | both | the receive path, the frame pool, `modules/netload`, `kcore::net`'s listener, the tick's receive poll (`rxpoll`) |
| `usb-test.py` | x86-64 | `drivers/usb/` |
| `hv-test.py [--arch x86_64\|aarch64]` | both | `hv`, `hvarch`, `modules/hv` -- the hypervisor |
| `insn-test.py` | host (CI) | the hypervisor's MMIO decoder and guest page walker (`hv/src/{insn,walk}.rs`), against the encodings clang gives |
| `hv-fuzz.py [--seed N --seconds S]` | host (CI) | anything a guest reaches in the hypervisor -- its devices, local APIC and IO-APIC, the MMIO path, the Linux loader and ACPI tables, the run loop that dispatches its exits, on every CPU of a guest of several, and the guests' network: the switch, its DHCP server and NAT -- fuzzed with overflow checks on |
| `net-fuzz.py [--seed N --seconds S]` | host (CI) | anything the network hands the kernel -- the receive path, ARP, ICMP, UDP and TCP; the DHCP client and the DNS resolver; the HTTP client over TCP and over TLS; the UDP shell; netconsole; `sshd` over the `ssh` crate -- fuzzed with overflow checks on, each protocol against a model of it |
| `fs-fuzz.py [--seed N --seconds S]` | host (CI) | anything a disk hands the kernel -- the partition tables, the disk log's area, ext2 and nanofs images sound and damaged -- and the storage layers above it: the block table's bounds and claims, the VFS and its C ABI, the file ABI, the shell's storage commands, `root=`; fuzzed with overflow checks on, the filesystems against a model of the tree, e2fsck's judgement and power cuts |
| `hv-linux-test.py --bzimage <img> [--initrd <cpio>]` | x86-64, by hand | the Linux loader, the CPUID/MSR policy, the emulated devices -- a real kernel to its shell; with an initrd, guests that stay up and the commands that reach them; `--net`, the guests' switch, NAT, its DHCP server and the DNS server they are given; `--cpus N`, guests of N CPUs, their local APICs and IPIs; `--xapic`, those APICs in xAPIC mode, every access of theirs by MMIO; `--ioapic`, an IO-APIC routing the timer, the serial port, the SCI and -- with `pci=nomsi` -- virtio's INTx; `--acpi`, a guest kernel with ACPI: the tables it is given, the PM timer and the SCI, the reset register, and `hv stop`'s power button |
| `hv-distro-test.py --iso <alpine-virt.iso> [--debian <nocloud.raw>] [--ubuntu <cloudimg.raw>] [--internet] [--cpus N [--ioapic]]` | x86-64, by hand | a distribution as it ships -- Alpine's kernel, initramfs and packages, its ISO a read-only disk: login, clock, reboot, network and the way out through NAT, and its own sshd reached from outside; Debian's cloud image, systemd provisioned by credentials, networkd by DHCP, its root written to and kept across a reboot -- both on their ACPI (`--acpi-off`: without), Debian shut down by `hv stop`'s power button |
| `idle-wait-test.py [--smp N]` | x86-64 | a wait primitive, the scheduler's choice of the idle task |

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
with rekeys in the middle, a command that prints nothing for five seconds
while the client asks every second whether the server is there (`ssh -o
ServerAliveInterval=1 -o ServerAliveCountMax=2 ... top 5000` -- a session
deaf while its command ran was cut off at three), a key it has to refuse,
sessions at once, a burst
of connections past the four that may be logging in, a stop and an `rmmod`
from inside a session; then it puts the module in `/etc/rc`, reboots, and
checks it came back by itself with the same host key. On x86-64 it boots an
ext2 root that already carries the module, a key and an `/etc/rc`, and logs
in. It is also what notices a change to the `ffi` declarations that a module
was not rebuilt against: a module's header carries a digest of them. Needs
`ssh`, `ssh-keygen` and host ports 2222, 8000 and 9000 free.

41 checks pass. One fails, `poweroff unloads sshd before the unmount`, for
reasons of its own that have nothing to do with TCP; and `connections past
the four logging in are refused` fails now and then under load -- stragglers
of the 70-probe burst reach the server after the three quiet seconds the
test waits for, and are counted among the refusals it looks for (18 where it
wants 2). A run that fails anything else is a regression.

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
the tick called, went with `src/cpp/net`, and from then on every tick
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
shootdown's acks -- polls, and polling with `Schedule()` lets the scheduler
hand the CPU to the idle task, which halts it. What such a task waits for is a
counter or a store in an interrupt handler: no waiter to unblock, no IPI to
send, so nothing brings the CPU back before its own next tick. The work was
done microseconds in and the wait cost 10 ms, every time.

Every other gate is blind to it, which is the point of having this one. The
precondition is a CPU with *nothing else runnable*, and a smoke boot has more
polling tasks -- shell, udpsh, dhcp, netconsole, usb -- than CPUs, so
`Schedule()` never reaches the idle task and every latency looks right. The
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
command typed on the usb keyboard reaches the shell".

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
hardware, PCI configuration space, the local APIC through its MSRs and its
page, the IO-APIC, MSIs, a virtio disk and NIC driven through rings well
made and not, the page walker over random page tables, the decoder, the
Linux loader over headers that parse, the ACPI tables for any machine, and
the DHCP server over the frames a guest's client sends, well formed and not.
The guests' network has three: `switch`, guests on its ports and `hv0`
sending each other frames of every kind -- as themselves and as each other
-- held to a model of where each must go and what each must count; `nat`,
guests' packets out into the world and the world's answers back, each
rewrite checked field by field and its sums summed from scratch; and
`guestnet`, the two as the module wires them, guests on the switch reaching
the world through NAT. And `platform` does it to the whole machine: a Linux guest's platform built
and loaded as `hv boot` builds it -- CPUs, disks, NICs, IO-APIC and all --
and run by the real run loop, `LinuxGuest::run`, on each of its CPUs, each
on a thread of its own and one at a time -- the one turn handed from thread
to thread directly, and the clock moved on to the next deadline when every
CPU waits, so that nothing sleeps on a real clock: sixteen times the runs a
second of the first harness, whose CPUs slept for real, and ten processes
at once some forty times theirs. Where the loop would enter the
guest, the CPU asks a script what the guest does next: a port, an MSR,
CPUID, a fault on a device's page with the instruction that made it put
where the guest's paging finds it, a halt, a window opened, an event taken,
another CPU's turn -- and between them what a driver does over several
exits: the 8259 initialised, the APIC turned on or moved between its modes,
an IO-APIC pin routed, MSI-X turned on and its table filled, a virtio queue
set up and given requests, another CPU started by INIT and start-up IPIs.

A finding is a panic -- an index out of range, an overflow -- or a broken
invariant: an interrupt injected into a guest that cannot take one, or over
an event already on its way in; an exception's vector from the APIC; a disk
request outside the disk, not in sectors, or past the requests in flight; a
frame longer than a frame handed to the switch; a DHCP answer that is no
datagram, or whose checksum is not its own; a frame the switch put
elsewhere than its destination says, or counted otherwise; a packet NAT
rewrote in any field but its own, or with a sum it did not have, a port
given to one flow while another was live on it, an answer let in that no
live flow asked for or kept out that one did, a packet counted twice or
not at all; what a guest takes from the world for another guest's address,
and what `hv0` or a guest takes from a guest not sent as that guest; a
vCPU that enters its
guest or waits with one of the kernel's locks held; a guest whose RAM would
cover its devices' pages. Or a spin -- the run loop reading the clock a
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
guest RAM now ends below them (`run::MAX_MEM_BYTES`). And a fifth, not live
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
what the rule is for.

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
  number, an ICMP error at every quoted one;
- `http` and `https`: the HTTP client fetching through chains of redirects
  from a server that answers well -- whose result is then known exactly --
  or to break it: numbers longer than any field, chunk sizes of every
  length, headers that never end, a redirect anywhere, a connection reset,
  a pause past the idle timeout. Over TLS the server is rustls driven by
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
  logging past what the ring holds and from tasks in the middle of a send:
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
  comes back whole; a client that breaks the protocol is told so.

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
  while it was on its way was sent again.

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
  edge of a block and of the indirect blocks, truncates both ways, creates,
  renames, removes, chains of directories forty deep -- each call's answer
  held to a model of the tree and the files open on it (`targets/fsops.rs`,
  the VFS's rules and the filesystem's limits written down), the tree read
  back through the VFS held to the model's, the image a clean unmount
  leaves clean and holding the model's tree, and mounted again, the same. A
  call the filesystem refused for want of room is held to having changed
  nothing -- a write to having left the file its size, a first part of the
  new data in it at most. Every power cut is at worst unclean -- which is
  what ext2's commit order and nanofs's copy-on-write promise -- and so is
  a run in which requests failed;
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
  calls does to the model. A mount taken exactly when its path is free and
  well formed, its device nobody else's and there is room; an unmount
  refused while anything is open on it; a rename between mounts refused;
  and the disks' filesystems clean after `unmount_all`;
- `shell`: the storage commands the console, the UDP shell and SSH run --
  `mount`, `format`, `cp -r`, `fstest`, `diskwrite` and the rest -- with
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

What it leaves, known: after a device error in the middle of a create or a
rename, the directory in memory may disagree with the one on disk until the
next mount, and a later create of the same name can leave that name in the
directory twice. Nothing is lost or shared -- the first is the one looked
up, and e2fsck renames the other -- so the checks count it unclean.

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
exist are each refused.

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
IO-APIC](hypervisor.md#the-io-apic)). The unload that follows, and the
load after it, are then of a hypervisor that has run guests on those CPUs.

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
fault at 0 in `Hal::GetCurrentCpuHwId()`, the guest's GS base -- and the
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

### `hv-linux-test.py` -- a real Linux bzImage, for a while and for good

The third and fourth of the hypervisor demos ([the hypervisor
page](hypervisor.md#what-comes-next)): `hv boot` loads an unmodified 64-bit
Linux `bzImage` by the boot protocol, runs it on a vCPU for a set time, and
reports its serial console. The gate boots nos with a `bzImage` on its root
filesystem and an `/etc/rc` that turns the extension on and runs `hv boot`,
and looks in the report for the kernel's banner and the first lines of its
early setup.

It is a **manual** gate, not in CI: a `bzImage` is megabytes and CI cannot
build one in the time it has, so the gate is pointed at a kernel by hand.
Build a small 64-bit guest -- a `tinyconfig` with the 8250 serial console and
an early console on, PCI, ACPI and SMP off is enough -- and run

```sh
scripts/hv-linux-test.py --bzimage /path/to/bzImage
```

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
under the running guest; and `rmmod hv` stopping that guest before it turns the
extension off, which the next load confirms and `dmesg` shows in order. Once
the shell has the console, the kernel's own lines go to the log and not to
it -- so the gate ends on a command's output (`version`) rather than on
`rc: /etc/rc done`, and reads what the vCPU tasks and the unload said with
`dmesg hv:`. `--skip-boot` and `--skip-vms` run one half. The same phase
restarts guests: a stopped one by `hv restart`, one started with `restart`
that resets itself until the sixth reset in a minute leaves it stopped, and
the running one, which must answer a line typed during its new boot.

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
4 MiB and a file of its own, syncs, and reads both back after a remount; the
report must count the disk's reads, writes and flushes and no error;
nos's root, which every write of the guest's went through -- the image's
holes filled, its blocks written over -- must pass `e2fsck` as nos left it,
stopped rather than unmounted; and the image, taken back out of it with
`debugfs`, must pass `e2fsck` too and hold what the guest wrote. It needs a
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
wrong on: a guest of two must be on both cores, never on one core's two
threads -- where the first placement put it (13 and 12 on the AX41, and
two busy loops there ran at three quarters of the speed). It goes with
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
fetched from outside QEMU through `hv forward`. Then the way out, through
nos's NAT: the guest's `/proc/net/pnp` names nos's DNS server (`ip=`'s
`dns0`); it fetches a page and a megabyte from a web server the test runs on
127.0.0.1 -- 10.0.2.2 to QEMU's user network, so beyond nos -- and the
megabyte's md5 must match; it pings 10.0.2.2; `udhcpc` must be given its
port's address, nos as its router and nos's DNS server by the switch; and
`nat` must say it is on from `hv0` through eth0 and count the megabyte's
segments back, `hv list` the address the guests go out from and the DHCP
answers. With `--internet`, a name is also looked up through that DNS server
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
AX41. And one guest claims the other's address -- `ip addr add` of it,
`arping` from it, a ping from it -- and must get nothing back: the switch
stops what a guest sends as another at its port, so nos, which would answer
both, hears neither; and `hv list` must count the frames against the guest
that sent them and no other. (nos's ARP table is no steady witness: with
the check taken out of the switch the claim reached it, and the other
guest's own ARP put the entry right again -- once before the test could
look, and once not, when the fetch through `hv forward`, which the test
makes last, reached the claiming guest instead of the other's `httpd`.) It
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
BusyBox's `acpid`, with a handler that powers off: its SCI must be IRQ 9 on
the 8259, the PM timer one of its clocksources, and `hv stop` must end it as
`the guest powered itself off`, the report counting the press; and one more
without `acpid`, which hears the button and does nothing, must be stopped
when `secs=5` is up, `its power button went unanswered for 5 s`. Without
`--acpi`, a guest's `hv stop` must say nothing in it listens to its power
button. In both, after the reload, two guests at their shells are stopped
together by `hv stop all secs=3`, which must press both buttons at once and
say of each what became of its press. `--acpi` goes with `--cpus`, `--disk`, `--net` and `--attach`, whose
guests then find their devices through the DSDT's host bridge.

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
from the test machine through NAT (with `--internet`, it also looks Alpine's
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
root is `/dev/vda1`, ext4, read-write, that its clock is the host's; that
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

`--cpus N` gives each guest N CPUs, and their local APICs -- the command
line then without the `nolapic` a guest of one CPU is given -- and every
one must come online; with `--ioapic` too, an IO-APIC, and the serial
console's interrupts -- the getty's, the shell's -- must have come through
its pin 4 ([The IO-APIC](hypervisor.md#the-io-apic)): a distribution's own
kernel, not one built for the gate, routing by it.

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
