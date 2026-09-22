# Loadable kernel modules

A kernel service can be built on its own, as a module, and put into a running
kernel -- and taken out again -- without rebuilding or rebooting it:

```
$ wget http://10.0.2.2:8000/hello.ko /hello.ko
$ insmod /hello.ko
module: hello loaded at 0xFFFF8000...
$ hello nos
hello, nos -- call 1 since the module was loaded
$ lsmod
hello  24 KiB at 0xFFFF8000..., 7 kernel imports
$ rmmod hello
module: hello unloaded
```

Modules are written in Rust, and only in Rust. A module is a crate that runs
in the kernel and reaches it through exactly the API the drivers built into
the kernel use -- the `extern "C"` functions the `ffi` crate declares, wrapped
safely by `kcore` -- and through nothing else: the loader binds a module
against that API and refuses anything it does not recognise.

## Writing one

A module lives in `src/rust/modules/<name>/`. `hello` is the smallest one
worth reading:

```rust
#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write;
use kcore::cmd::Command;

struct Hello {
    _cmd: Command,
}

impl kmod::Module for Hello {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("hello", "hello [name] - ...", |args, out| {
        let _ = writeln!(out, "hello, {}", args);
    })?;
    Ok(Box::new(Hello { _cmd: cmd }))
}

kmod::module!(name: "hello", init: init);
```

`kmod::module!` names the module -- what `lsmod` shows and `rmmod` takes, at
most 31 printable characters -- and says which function `insmod` runs. That
function builds the module's state and hands it back as a `Box<dyn
kmod::Module>`; an error instead fails the `insmod`. The kernel keeps the box
while the module is loaded, and `rmmod` drops it: there is no exit function to
write, because the module's `Drop` is its exit. Whatever the state holds --
the commands it registered, the tasks it started, the timers and interrupts
it took -- is released by the drops of the `kcore` handles in it, and must be,
because the module's code is freed right after. The state has to be `Send`:
the task that runs `rmmod` drops what the task that ran `insmod` built.

Dropping a handle is enough because every unregister the kernel offers waits
out a callback still running on another CPU before it returns: a command
(`kernel_cmd_unregister`), a timer, a legacy or MSI-X interrupt, a task
(`TaskHandle` joins it). One registration has no unregister at all: a softirq
handler (`kcore::softirq::register_for`) is the kernel's to call for as long
as it runs. A module that imports it is **permanent**: the loader sees the
import, `lsmod` says so, and `rmmod` refuses it -- the way Linux keeps a
module that has no exit function. (A disk's or a NIC's *driver* cannot be a
module at all: it registers with the block or the net layer as a Rust trait
object, from inside the kernel image, and there is no C name for that. A
module uses disks and NICs -- `kcore::block::Disk`, `kcore::net::Nic` -- it
does not provide them.)

The `kmod` crate supplies the rest of what every module needs once: the
global allocator (the kernel heap, through `kernel_alloc`), the panic handler
(a module that panics panics the kernel, as the in-kernel Rust does) and the
header the loader looks for. `kcore::cmd` is how a module puts a command in
front of whoever runs the shell, on the console or over the [UDP
shell](udp-shell.md); the handler gets the rest of the command line and an
`Output` that is `core::fmt::Write`.

To add a module:

1. `src/rust/modules/<name>/Cargo.toml`, a `staticlib` whose library is named
   `mod_<name>`, depending on `kmod` and `kcore` (copy `hello`'s);
2. add `modules/<name>` to the workspace `members` in `src/rust/Cargo.toml`
   -- not to `default-members`, which is what the kernel's own build compiles;
3. add `<name>` to `MODULES` in the `Makefile`.

## Building

```
make modules                 # out/x86_64/modules/<name>.ko
make modules ARCH=aarch64    # out/aarch64/modules/<name>.ko
```

`make` and `make nocheck` build them too. Each module is compiled on its own,
with its own copy of `core`, `alloc`, `kcore` and `kmod`, into a target
directory of its own (`src/rust/target/modules`), because it is compiled with
flags the kernel's Rust is not:

- `-Crelocation-model=pic`, since nobody knows where it will be loaded;
- on x86, `-Ccode-model=small`: the kernel is built with the large code model
  so it can be linked in the top half of the address space, and a module has
  no need of that -- everything a PIC module addresses of its own is
  `rip`-relative, and everything of the kernel's goes through the GOT;
- compiler-builtins' own `memcpy`, `memset` and friends, so that a module
  imports nothing but the kernel's API.

The staticlib is then linked into an ELF shared object:

- `-Bsymbolic`, so every reference the module makes to itself is bound at
  link time and what is left for the loader is `RELATIVE` relocations and the
  kernel's functions;
- `-z separate-loadable-segments -z max-page-size=4096`: every segment starts
  on a page of its own, since page permissions are per page and each segment
  gets its own;
- a version script that exports `nos_module_info` and nothing else;
- `-z now`, `-z norelro`: every import is bound at load time, and there is no
  lazy binding and no dynamic linker to protect anything from.

Last, `llvm-nm -n -C` lists the module's functions, demangled, and
`llvm-objcopy` adds the list to the `.ko` as a `.nos_syms` section -- what
backtraces name a module's frames with (below). Rust's v0 names would need a
demangler in the kernel otherwise.

`hello.ko` comes out around 25 KiB.

### Releases

A tagged release (`.github/workflows/release.yml`) carries every module the
Makefile builds, next to the kernels it was built with, as `<name>-x86_64.ko`
and `<name>-arm64.ko` -- so a running nos can fetch one over HTTPS and load
it:

```
$ wget https://github.com/irqlevel/nos/releases/download/<tag>/blkload-x86_64.ko /blkload.ko
$ insmod /blkload.ko
```

Take it from the release the running kernel came from (`version` names it):
the kernel from another release may have another kernel interface, and then
refuses the module ([What a module may call](#what-a-module-may-call)). The
release's `SHA256SUMS` covers the modules too, for `sha256` to check.

## Loading

`insmod <path>` reads a `.ko` off any mounted filesystem -- put it on the root
filesystem with `wget` from a web server on the build host, or build it into
a fresh root image with `scripts/mkrootfs.sh <image> <MiB> <dir>`. The loader
(`kernel/module.cpp`) then:

1. **checks the file**: ELF64, little-endian, a shared object for this CPU,
   headers inside the file. No thread-local or interpreter segment. Every
   loadable segment page-aligned, inside the file, clear of the others and
   never both writable and executable.
2. **binds its imports**, before mapping anything. Each undefined symbol in
   `.dynsym` is looked up by name in the kernel's export table; a module the
   kernel cannot satisfy is refused with the list of everything it lacks. A
   weak import may go unresolved and binds to 0.
3. **maps the image**: pages of its own, which need not be physically
   contiguous, mapped writable into one run of kernel virtual addresses --
   from a window of 16 MiB blocks set aside for runs the page allocator's own
   blocks, 512 KiB at most, cannot hold (`Mm::MapLargePages`). The segments
   are copied in; the zeroed tail of the last is `.bss`. The function names
   from `.nos_syms` go into the same pages, past the segments, indexed by
   offset.
4. **relocates it**: every `RELA` section the loader is meant to see (`.rela.dyn`,
   `.rela.plt`). Three kinds of relocation reach it, and the architecture
   says which is which (`Hal::ClassifyModuleReloc`):

   | Meaning | x86-64 | arm64 |
   |---|---|---|
   | nothing | `NONE` | `NONE` |
   | load base + addend | `RELATIVE` | `RELATIVE` |
   | symbol + addend | `64`, `GLOB_DAT`, `JUMP_SLOT` | `ABS64`, `GLOB_DAT`, `JUMP_SLOT` |

   Anything else is refused. In practice x86 modules carry `RELATIVE` and
   `GLOB_DAT` relocations and arm64 ones `RELATIVE` and `JUMP_SLOT` -- calls
   through the PLT, whose slots are bound now rather than lazily.
5. **checks the header**, `nos_module_info`, now that its pointers are real:
   the magic, the layout version, the name, init and exit pointing into the
   module's own code -- and the kernel interface the module was built
   against (below).
6. **protects it**: each segment gets the permissions its program header
   asks for -- read-only, read-execute or read-write -- every CPU's TLB is
   shot down, and on arm64 the new code is cleaned from the data cache and
   invalidated in every CPU's instruction cache (`Hal::SyncInstructionCache`),
   which x86 does not need.
7. **runs its init**. A second module by the same name is refused before
   this, and an init that fails leaves nothing behind.

`rmmod <name>` runs the module's exit -- dropping its state -- and only then
unmaps and frees its pages. A command the module registered is taken away
with it, and `kernel_cmd_unregister` waits for any call of it still running
before it returns, so the code under a running command is never freed.
`rmmod` must not be run from the module's own command, which would wait for
itself.

Loads and unloads run in task context: a module's init and exit may sleep,
and an exit that waits out a command still running may take as long as that
command does. So nothing is held across an init or an exit. The table's lock
covers looking a module up and moving it from one phase to the next --
loading, live, unloading -- and nothing else: a load or unload that takes its
time holds up no other, and `lsmod` never waits. A module is on the list
while its init runs, so a second `insmod` of it is refused, as is an `rmmod`
of one still loading or already unloading.

`insmod` and `rmmod` each hand the work to a task of their own and wait for it
at most five seconds (`ModuleTable::ShellWaitMs`). What the loader says is
printed when it is done by then. When it is not -- an unload waiting on a
command, say -- the shell says so and gets its prompt back; `lsmod` shows the
module as unloading meanwhile, and the kernel log says how it ended:

```
$ rmmod slowexit
rmmod: slowexit is taking its time and goes on in the background -- lsmod shows where it is, the kernel log will say how it ended
$ lsmod
slowexit  12 KiB at 0xFFFF8000..., 5 kernel imports, unloading
$ dmesg 5 module:
... module: rmmod slowexit, done in the background, error 0: module: slowexit unloaded
```

`poweroff` and `reboot` unload every module that can be unloaded, newest
first, once the shells have stopped and before the filesystems are unmounted
and the soft IRQs stop -- so a module's exit still has the kernel services it
may need, and nothing of the module is left running while they go.

## What a module may call

The export table is generated by the build, like the [symbol
table](debug.md): from `pass1.elf`, the kernel's first link, the Makefile
takes every function named in an `extern "C"` block of the `ffi` crate that the
kernel defines, and writes their names and addresses into
`out/<arch>/module_exports.S`, which the final link takes in. So what a module
can call is what an in-kernel Rust driver can call; adding a kernel service
for modules is the same three steps as adding one for the drivers (see
[Rust in the kernel](rust.md#adding-a-kernel-service)), and the table picks
it up.

A module compiled against one version of those declarations and loaded into
a kernel built from another would call functions with arguments they no
longer take. The build hashes the `ffi` crate's sources into a digest that
goes into every module's header and into the kernel, and the loader refuses a
module whose digest is not the kernel's:

```
module: built against another kernel interface (ffi 3fa81c..., this kernel 9b02e7...) -- rebuild it from this tree
```

A module built outside the Makefile carries `unset` and is refused by a kernel
that was not.

## blkload

`blkload` (`src/rust/modules/blkload`) runs a short load test against a block
device -- a disk, or one of its partitions, by the name `disks` gives it --
and reports IOPS, bandwidth and latency. Under QEMU, against its emulated
NVMe disk:

```
$ insmod /blkload.ko
$ blkload nvme0 randread qd=8 secs=2
blkload nvme0: randread, bs 4 KiB, qd 8, 2 s over 64 MiB
  51965 ios in 2.06 s: 25223 IOPS, 98.5 MiB/s
  latency us: min 56.0, avg 297.6, p50 270.3, p90 434.1, p99 770.0, p99.9 1146.8, max 9557.0
```

    blkload <dev> [randread|randwrite|read|write] [bs=4k] [qd=1] [secs=5] [force]

- `randread`, the default, and `randwrite` pick blocks at random over the
  whole device; `read` and `write` go through it in order, the tasks taking
  consecutive blocks.
- `qd` tasks, up to 64, each keep one I/O in flight: the queue depth asked
  of the driver, which passes on as many as it has room for -- 8 at a time
  for virtio-blk, 63 for NVMe. A burst past that waits for a command to
  complete rather than failing.
- `bs` is a whole number of sectors, up to 512 KiB -- and no more than the
  driver takes in one I/O, which today is a page for virtio-blk (one
  descriptor per request) and two for NVMe (two PRP entries, no PRP lists).
  Past that the first I/O fails, and blkload says the largest size that does
  go through.
- `secs`, 1 to 60: the command holds its shell that long. Over the [UDP
  shell](udp-shell.md) the reply comes whole once the command is done, and
  `udpsh.py` gives up on one after 30 s unless its third argument says
  otherwise -- so give a longer run a longer wait: `udpsh.py <host> 9000 90`.
- Latency is timed per I/O on the kernel's clock, to the nanosecond; the
  percentiles come off a histogram with sixteen buckets to each power of two,
  so they are within 1/16. It runs from submission to the task running
  again: the kernel's waits yield rather than sleep, so with more tasks than
  CPUs it includes waiting for one, and the test keeps the CPUs busy.

Reads are always allowed. A write test destroys what is on the device, so it
has to be asked for by name, and it claims the device first
(`kernel_blockdev_claim_as`, which mounts and the disk log take too, and the
shell's `format` and `diskwrite` while they write). The claim is refused
while a mounted filesystem, the disk log or another writer holds the device
-- or the disk it is a partition of, or one of its partitions -- whatever
else is said; once taken, it holds all of those off until the test is done,
so nothing can be mounted under it midway. Unless `force` is given the test
is also refused on a disk with partitions, and on a device that starts with
an ext2 or nanofs superblock, a partition table, a boot sector or a prepared
disk log area. A write test ends with a flush, timed.

To try a spare partition on a machine's NVMe disk, find it with `disks` --
a partition is named after its disk and numbered: `nvme01`, `nvme02` -- fetch
the module from the release the machine's kernel came from
([Releases](#releases)), and start with reads:

```
wget https://github.com/irqlevel/nos/releases/download/<tag>/blkload-x86_64.ko /blkload.ko
insmod /blkload.ko
blkload nvme02 randread qd=32 secs=10
blkload nvme02 randwrite qd=32 secs=10
blkload nvme02 write bs=8k qd=16
```

The module and the machine's kernel have to share a kernel interface -- see
[What a module may call](#what-a-module-may-call) -- so take both from one
release, or build both from one tree.

## netload

`netload` (`src/rust/modules/netload`) is a UDP load test from either end of
the wire, so that [`profile`](profiler.md) has something to look at other
than an idle machine: an idle twenty-CPU box spends about 0.06% of a core on
anything at all, and the paths worth seeing -- the frame pool, the driver
rings, soft IRQ dispatch -- only appear when packets are moving. Two
machines running it, one each way, move them with nothing else in the
picture. Under QEMU, the source against a host that sends back what it gets:

```
$ insmod /netload.ko
$ netload send 10.0.2.2 7777 size=200 pps=500 count=300 tasks=2
netload: sending to 10.0.2.2:7777 from port 9998, 200 byte datagrams, 2 task(s), 500 a second, 300 of them
$ netload
netload: sending to 10.0.2.2:7777 from port 9998, 200 byte datagrams, 2 task(s), 500 a second, 300 of them -- done in 0.613 s
sent 300 packets, 72600 bytes, 0 failed
echoes 300 packets, 72600 bytes, 0 from elsewhere
rate 0 tx-pps, 0 rx-pps, 0 tx-bytes/s
per cpu tx: 0:150 1:150
```

and the target, on another machine, is `netload start` -- then
`netload send <its address> 9999` from the first.

    netload start [port] [sink]
    netload send <ip> <port> [size=64] [pps=0] [secs=0] [count=0] [tasks=1] [sport=9998]
    netload            what either has counted
    netload stop       both
    netload reset      the counters

**The target** (`start`, port 9999 unless told) answers every datagram, which
exercises receive and transmit together; `sink` drops them, which isolates
the receive half. The reply is the frame that arrived with its addresses
swapped where they lie -- no copy, no allocation -- built in the receive
callback itself, because a load target that woke a task per packet would be
measuring the wakeup; and the replies of one receive batch go to the NIC
together when the batch ends, one transmit lock and one doorbell for the
lot. It can be driven from any host as well: `scripts/netload.py` (`--pps`
to hold a rate, `--threads` to raise the ceiling).

**The source** (`send`) has `tasks` sender tasks, up to 16, each building its
datagrams straight into frames off the pool -- one copy of a template made
once, and a sequence number -- and handing them to the NIC a batch at a
time.

- `size` is the payload, 16 to 1472 bytes. `pps` is a rate over all the
  senders, and 0 is as fast as the NIC takes them. The run ends when `count`
  datagrams are sent, when `secs` have passed, or at `netload stop`; a
  finished run stays to be asked about until the next `send`.
- The destination's Ethernet address is asked of ARP once, before the first
  frame -- the gateway's, for an address off the subnet. If nothing answers,
  nothing is sent: a load test does not fall back to broadcast.
- A sender asks the NIC's transmit queue how much room it has before it
  builds a batch, so that it loses nothing of its own to a full queue, and
  leaves a quarter of the queue alone. A source that took every slot the
  moment one opened would keep the queue full for as long as it ran, and
  everybody else's frames would find no room: the shell's answers, the
  netconsole's lines. On a machine whose only console is the network, the
  load test would silence the one channel that says how it is going.
  `failed` counts what found no room anyway -- with several senders the
  room one was told of may be another's by the time it has built its batch.
- What comes back to `sport` is counted as `echoes` when it is from where the
  run is sending, and as `from elsewhere` when it is not -- and never
  answered, so two sources pointed at each other do not bounce one datagram
  for good. Pointed at a target of this same module, a source therefore sees
  how much of what it sent made the round trip.
- A datagram says which one it is: `NLD1`, the sender's number (four bytes,
  big-endian), a sequence number (eight), then filler whose every byte is
  its own offset. Whatever receives them can tell what was lost, what came
  twice and what was damaged; a sequence number is spent whether or not its
  frame found room, so what the far end misses is what this end counted
  failed, plus what the wire lost.

Both write a line a second to the kernel log while they run -- packets,
failures, the frame pool's misses and what is out of it, how often the tick
had to find the frames an interrupt should have announced. The point is not
the numbers: it is that the line keeps arriving. A machine goes deaf under
load -- the shell stops answering and so does ping -- and every channel that
could say why is a network channel. The [netconsole](netconsole.md) only
sends, so if these lines continue after the machine has stopped receiving,
the machine is alive and the receive path is what died; if they stop with
it, the kernel itself is wedged.

It is the one module that lives on the receive path, and the reason a
module's UDP listener is what it is: `kcore::net::Nic::listen` takes a
handler the listener owns, lends it each frame as a `Lent`, tells it when a
receive batch ends, and lends both calls the `RxContext` that reaches what
only the receive path touches (`RxOwned`) with no lock -- here, the `TxBatch`
the replies gather in. The module has no `unsafe` in it; the frame formats
are the `netwire` crate's, which the network layer is built on too.
`scripts/netload-test.py` is its test ([Tests and gates](testing.md)).

## netblk

`netblk` (`src/rust/modules/netblk`) serves an NVMe disk, or a partition of
one, over UDP, zero-copy both ways: `netblk start nvme0 7000`, then
`scripts/netblk.py <host> 7000 ...` from anywhere else. Its own page is
[netblk](netblk.md). It takes its requests through the same listener netload
does, and takes them apart with `netwire`. It is the module that stops cleanly with the most in
flight -- a receive callback on one CPU, the disk's interrupt handler on
another, a worker task, frames the NIC still holds -- and its `Drop` is the
order that takes: the listener, the worker, then `kcore::cpu::synchronize` for
an interrupt handler still on its way out of the module's code.

## sshd

`sshd` (`src/rust/modules/sshd`) is an SSH server: `sshd allow <key>`, `sshd
start`, and an OpenSSH client logs in to the kernel's shell. The protocol is a
crate of its own, `src/rust/ssh`, which knows nothing of the kernel; the
module is the kernel's side -- a listener task, a task for each connection,
the files it keeps under `/etc/ssh`. Its page is [sshd](sshd.md). Nothing loads
a module at boot by itself, so a machine that is to be reached only through
it gets two lines in `/etc/rc`, which the shell runs once the network is up:
`insmod /sshd.ko` and `sshd start`.

It needed the kernel to export more than the drivers ever had: TCP's passive
side (`kcore::tcp::TcpListener`, `TcpStream`), running a shell command with
its output handed back (`kcore::cmd::dispatch`), files (`kcore::fs`) and the
calling task's identity (`kcore::task::current_id`). And it carries its own
copy of the cryptography the TLS client uses: modules share nothing with the
kernel but the exported functions.

## hv

`hv` (`src/rust/modules/hv`) is the [hypervisor](hypervisor.md), the
beginning of [stage 3](../plans/03-hypervisor.md). It is two crates --
`hvarch`, the CPU's virtualization extension and all of the hypervisor's
`unsafe`, and `hv`, everything above it, which has two sites -- plus this
module, which is their lifetime and their shell command. Neither crate is a
default member of the workspace, so a kernel built without the module has no
hypervisor in it at all.

It is the module that makes the clearest case for the shape. The one piece
of state it takes is not the module's: `EFER.SVME` and `MSR_VM_HSAVE_PA` on
AMD, `CR4.VMXE` and VMX root operation on Intel belong to the CPU, and an
`rmmod` that forgot them would leave the CPU saving host state into a page
that has been freed and given to somebody else, with the code that would
have turned it off unmapped. So the module turns the extension off in its
own `Drop` -- after the command is unregistered, so nothing can ask it
anything -- reads back what every CPU has, and says so if anything is left.
`scripts/hv-test.py` is that round trip: load, turn on, unload without
turning off, load again and ask the CPUs.

The rest of it is what a module does with CPUs. A page per CPU (`CpuPage`)
is allocated in task context, because the IPI handler that hands it to the
CPU runs with interrupts off and may not allocate; `kcore::cpu::run_on_with`
carries it over as a borrowed value rather than a word to cast back, which
is sound because `run_on` does not return until the handler has run.

## Backtraces

A frame in a module's code is named like the kernel's own, with the module
after it -- in the panic report, in `bt` and in the profiler's call chains:

```
Backtrace:
  [0] 0xFFFF800012DA1C40 mod_hello::init::{closure#0}+0x3c [hello]
  [1] 0xFFFF80000104A2F1 Kernel::Cmd::DispatchDynamic+0xd1
...
Modules: hello 0xFFFF800012DA0000+0x5000
```

`SymbolTable::Describe` asks the kernel's table first -- which now answers
only for an address in the kernel's text, rather than naming whatever lies
past its last function after that function -- and then the module table,
whose names come from each module's `.nos_syms`. The name is copied out under
the table's lock, since the module may be unloaded right after; the lock is
only tried, never waited on, so a panic can ask too. The panic report ends
with where each module sits (`Modules:`), which places any frame the list
could not name.

## Testing

The boot self-test loads a module on both architectures. `modtest`
(`src/rust/modules/modtest`) is built with the kernel and embedded in its
image (`out/<arch>/modtest_blob.S`); `TestModules` in `kernel/test.cpp` loads
it, and its init checks what a loader can get wrong -- initialised data,
`.bss` (a megabyte of it, past the 512 KiB images were once held to), a
table of function pointers, a trait object's vtable, allocations from the
kernel heap, a kernel object created and destroyed through the export table
-- and fails the load if anything comes out wrong. The test then runs the
command the module registered through the shell's dispatcher, has the
symbolizer name the function whose address the command prints, checks a
second copy is refused, unloads it and checks the command went with it,
twice; and last feeds the loader damaged copies -- truncated, another
machine's, an import the kernel lacks, no header, a bad magic, another kernel
interface -- each of which it must refuse.

`slowexit` (`src/rust/modules/slowexit`) is never loaded by the kernel itself:
its exit takes eight seconds, for testing an `rmmod` that outlasts the
shell's wait by hand or from a script.

## Limits

- A module's image, function names included, is at most 16 MiB
  (`PageTable::MaxLargeMapPages` pages), and so is the `.ko` file. The window
  they are mapped from holds 63 such runs at a time.
- A module that registers a block device, a net device or a softirq handler
  cannot be unloaded (above).
- An unload waits for a running call of the module's commands for as long as
  it runs -- in a task of its own, so it holds up nothing else, but a command
  that never returns leaves its module unloading for good.
- Modules cannot call each other; each carries its own `kcore`, so they share
  no statics either.
- Only the functions `ffi` declares are exported -- not C++ classes, not
  arbitrary kernel symbols.
- No signatures: whoever can run `insmod` can run code in the kernel. On
  nos that is whoever can reach the shell, which already could.
