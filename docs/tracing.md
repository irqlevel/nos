# Tracepoints

Events a subsystem records at the moment they happen, turned on and off on a
running kernel, into a ring of the recording CPU's own, and read back from
every CPU's ring merged by time:

```
> trace on test                 # the event on, the rings made
> trace test 300                # 300 test records on this CPU
> trace show 5                  # the newest five, oldest first
    4.185290  1 test: seq=295 value=0x...
    ...
> trace stat                    # each CPU's ring: written, kept, dropped
> trace off all
```

The log (`klog()`, `klog!`) is for lines a person reads; a tracepoint is for
events too many and too fast for that -- every context switch, every wakeup --
recorded in a few dozen nanoseconds, with no formatting until someone asks.
The code is `kernel/tracepoint.{h,cpp}` and `kernel/trace_ring.{h,cpp}`.

## What a tracepoint costs

A tracepoint that is off is one byte's load -- from a line every CPU only
reads -- and a branch not taken:

```cpp
if (unlikely(tp::on(tp::Id::SchedSwitch)))
    emit_sched_switch(prev, next, why);   // its arguments computed only here
```

No arguments are computed and no call is made until the event is on. One
that is on is a call to `tp::emit()`: interrupts off, the cycle counter read,
eight 64-bit stores and two barriers into this CPU's ring, interrupts back.
`trace bench` measures both on the CPU it runs on:

| | off, over an empty loop's iteration | on, a record |
|---|---:|---:|
| arm64, Apple Silicon under HVF | 0.01-0.13 ns | 31-47 ns |
| arm64, QEMU's TCG | 0.28 ns | 239 ns |

There are no static keys -- the branch is not patched out of the code. The
image is W^X, so patching would need a writable alias of the text and each
architecture's protocol for code changed under a running CPU, all for a
fraction of a cycle a site; the numbers above say it is not worth it.

## The rings

Each running CPU has a ring of its own, made by the first `trace on` (1 MiB
by default: 16384 records) and never shared: only its CPU writes it, with
interrupts off, so a write takes no lock and waits for nothing. Anyone reads
any ring at any time without stopping its writer.

A slot is 64 bytes: a sequence word, the cycle counter, the event's number
and size, and up to 40 bytes of payload. Each slot is a seqlock of its own:
the writer clears the sequence word, writes the rest and then writes the
sequence word again -- the record's index plus one -- with a write barrier
between each step; a reader takes the word, the rest, and the word again
after a read barrier, and keeps its copy only when both looks give the index
it asked for. A slot being written over as it is read is left out, never
read half old and half new. Every word is an atomic, so the race between the
two is no data race in C++'s sense, only a copy thrown away.

The ring wraps: once full, each record takes the slot of the oldest, and a
writer never waits for room. Its slots are in 64 KiB chunks from the kernel
heap, a power of two of them, so a slot is found with a shift and a mask.

A ring is freed -- `trace size`, `trace free` -- only once no CPU can still
be writing into it. The new pointers go in first, and then every other CPU
is sent an interrupt and waited for (`CpuTable::synchronize`): a write runs
with interrupts off, so a CPU that has taken one is out of any write it was
in. A CPU that never answers is a panic after ten seconds, naming it, as a
TLB shootdown's is.

An NMI that comes in while its CPU is writing takes nothing and is counted
(`dropped` in `trace stat`); no tracepoint sits in an NMI's path today.

## `trace`

| | |
|---|---|
| `trace [list]` | every event: on or off, its group, what it records |
| `trace on <event\|group\|all>...` | turned on (the rings made first, if there are none); names may be comma-separated |
| `trace off <...>` | turned off; what was recorded stays |
| `trace show [n] [cpu=N] [event...]` | the newest `n` records (50; 4096 at most) of every CPU, or one, merged by time, oldest first; with events named, only theirs |
| `trace stat` | each CPU's ring: written, kept, dropped |
| `trace clear` | what is recorded forgotten: `show` starts after it |
| `trace size <KiB>` | the rings' size, 64 KiB times a power of two up to 64 MiB; rings there are made again at it, what they held with them |
| `trace free` | every event off, every ring given back |
| `trace freeze` / `thaw` | nothing more recorded until thawed, so that what is in the rings stays as it was |
| `trace test [n]` | `n` records of the `test` event on this CPU |
| `trace bench` | what a tracepoint costs this CPU off and on |

A record prints as its time since boot, its CPU, the event's name and its
fields as `name=value` -- from the event's description, one format for all.

`tracepoints=<list>` on the kernel command line turns events on as the boot
ends, once every CPU has its ring ([kernel parameters](kernel-parameters.md)).

## An event of its own

An event is four things in `kernel/tracepoint.{h,cpp}`: its number in
`tp::Id` (appended, never reordered -- a saved trace names events by
number), its payload struct (40 bytes at most, `static_assert`ed), its fields'
table (name, `offsetof`, `sizeof`, how to print it), and its line in the
`EVENTS` table (name, group, help, size). A site checks `tp::on()` and calls
`tp::emit()` with exactly the payload's size -- a size its description does
not have is a `BUG_ON`. The emit path takes no lock and allocates nothing:
it may be called with locks held and interrupts off, from interrupt context
too.

## What holds it

- The smoke boots' `test_tracepoints`, once on the boot CPU: a ring of its
  own read back in order, wrapped with the oldest written over and no longer
  read, a payload too large refused; a writer on another CPU and the boot's
  reader reading as it writes -- on arm64 under HVF, 1.9 million records read
  whole and 75 000 seen written over in 20 ms, none torn; the kernel's rings
  through `emit()`, a freeze, and every ring freed through
  `CpuTable::synchronize`.
- `cpp-fuzz.py --target trace` (host, in CI): the ring held to a model --
  `init` taking a power of two of chunks and keeping nothing of one that
  failed for memory, every read the record the model wrote or none -- and a
  writer thread against a reader, every record kept the one its index says.
  Control: a writer that does not clear the sequence word first is caught at
  the 103rd input, a torn record.
- `trace-test.py` (arm64; x86-64 where grub-mkrescue is): a kernel booted
  with `tracepoints=test`, and `trace` driven over the UDP shell -- the
  boot's events on, records shown merged and in order, rings made again at
  another size, three times round one of 1024, cleared, frozen and thawed,
  freed and made again, and every word the command refuses.
