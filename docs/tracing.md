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
| arm64, Apple Silicon under HVF | 0.14-0.37 ns | 31-47 ns |
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
| `trace dump [n] [cpu=N] [event...]` | the same window as JSON, a record a line, times in exact ns: what `scripts/trace-decode.py` reads |
| `trace latency [n] [cpu=N] [event...]` | the window's wakeups, each paired with the switch that answered it: p50/p90/p99/max and the longest five, with the task and the CPUs |
| `trace stat` | each CPU's ring: written, kept, dropped; and the wakeup trigger, armed or what it fired on |
| `trace clear` | what is recorded forgotten: `show` starts after it |
| `trace size <KiB>` | the rings' size, 64 KiB times a power of two up to 64 MiB; rings there are made again at it, what they held with them |
| `trace free` | every event off, every ring given back |
| `trace freeze [wakeup>N<ns\|us\|ms\|s>]` / `thaw` | nothing more recorded until thawed, so that what is in the rings stays as it was -- at once, or when a wakeup waits longer than N ([below](#a-wakeup-that-waited-too-long)) |
| `trace test [n]` | `n` records of the `test` event on this CPU |
| `trace bench` | what a tracepoint costs this CPU off and on |

A record prints as its time since boot, its CPU, the event's name and its
fields as `name=value` -- from the event's description, one format for all.
`dump` prints the same records as objects -- `{"t":<ns>,"cpu":N,"ev":"<name>","f":{...}}`,
the CPU outside the fields' object, as some of them are called `cpu` too --
so a host reads numbers, not a format.

`tracepoints=<list>` on the kernel command line turns events on as the boot
ends, once every CPU has its ring ([kernel parameters](kernel-parameters.md)).

## How long a woken task waited

`trace latency` is the shell's own answer to the question a schedule trace
is usually read for: a task woken at one instant and running at another,
how long between? It walks the window `show` prints -- the newest `n`
records of every CPU, merged by time -- keeping each `sched_wakeup`'s pid
and instant in a table by pid (ids are 0 to 255, so the table is an array),
and pairs it with the first `sched_switch` after it whose next task is that
pid. The difference is the wait. A wakeup whose switch is not in the window
-- still waiting, or its record written over -- is counted as not run in
it, never guessed.

```
> trace on sched
> trace latency 4000
trace latency: 2000 records on 4 cpus -- 43 wakeups, 43 ran in the window, 0 not run in it
  wait p50 208ns p90 10.1us p99 15.3us max 20.2us
  the longest:
     20.2us pid 9 udpsh        cpu 1 -> 1
     15.3us pid 11 probe/wait  cpu 3 -> 3
```

The percentiles are exact, not buckets: the waits are sorted (a quicksort
over an explicit 32-entry stack, no allocation beyond the array). The five
longest say what a histogram cannot: which task, and whether it ran on the
CPU it was queued on. `sched`'s own wakeup histogram ([Scheduler](scheduler.md#what-it-is-measured-by))
is the live one, always on and coarse; this is the window's, exact, and it
works on a frozen trace after the fact.

The idle events are a CPU's own every few microseconds, so a ring holds
milliseconds of them: leave `idle` out of a window a latency is read from,
or the wakeups a probe left are written over before the read.

## A wakeup that waited too long

The rings wrap, so the trace of a 10 ms wait is gone a few milliseconds
later unless something keeps it. `trace freeze wakeup>N` arms a trigger:
the scheduler asks on every wakeup (the wait is already in its hand, in
`TaskQueue::note_run`) and the first one over N freezes the rings where
they are. The trigger then writes down what it fired on, which `trace stat`
prints -- the wait itself is the one record that is not in the rings,
because it froze them:

```
> trace freeze wakeup>1ms
trace: armed -- the rings freeze when a wakeup waits over 1.000ms
> ...
> trace stat
trace: rings of 1024 KiB a cpu (16384 records), frozen
trace: frozen by the wakeup trigger: pid 42 probe/wait waited 12.400ms on cpu 0, at 3.249165
```

Arming it thaws the rings first -- a frozen ring has nothing for a trigger
to keep -- and it is armed and spent with an atomic each way: the scheduler
reads a word that is 0 unless a trigger is armed, and the first CPU to fire
claims the record with an exchange, the record a seqlock `trace stat` reads
as any other. `trace thaw` records again and disarms. A wakeup that
claimed the trigger a moment before it was re-armed can still freeze the
new arming; the record says which wait it was.

## Reading a trace on the host

`trace dump` is the window as JSON lines, and `scripts/trace-decode.py`
reads them back from a file, a saved serial log or a netconsole capture:

```
> trace on sched
> sched probe spinners=2 ms=300
> trace dump 4000
...
```

then, on the host (from the serial log, filtering out everything that is
not a record):

```
scripts/trace-decode.py dump.txt                      # what the window holds
scripts/trace-decode.py --chrome trace.json dump.txt  # chrome://tracing or Perfetto
scripts/trace-decode.py --json raw.json dump.txt      # the records, normalised
scripts/trace-decode.py --latency dump.txt            # every wait, longest first
```

Its text says how many switches, wakeups, blocks and idles the window has,
which task ran on each CPU and for how long, and the waits -- the same
pairing the kernel's `trace latency` does, computed a second way from the
same records. The Chrome trace is a track a CPU, a slice every stretch a
task ran on it, a wakeup or a block an instant event on the track of the
CPU it happened on: a schedule read as a picture of what waited for what.

The shell's UDP reply is 32 KiB, so a `dump` of more than a couple of
hundred records does not arrive whole over it; read the serial console or
netconsole for a big window, or dump in parts -- the decode does not care
where the lines were cut.

## The scheduler's events

| event | group | what it records |
|---|---|---|
| `sched_switch` | sched | a CPU's next task and the one it left: `prev_pid`/`prev_comm`, `next_pid`/`next_comm`, `next_cpu`, what `prev_state` went to be (runnable, blocked, sleeping, exited) and the decision's `reason` (voluntary, yield, preempt) |
| `sched_wakeup` | sched | a task handed back to its queue: `pid`, `comm`, the `cpu`, whether its `deadline` came or someone's `unblock()`, and the task that woke it (`waker_pid` -- the one an interrupt landed on, from an interrupt's context -- 4294967295 where nobody did) |
| `sched_block` | sched | a task that left its CPU because it cannot run: whether it is `deadline`-bound, and how long it `ran_ns` this time |
| `sched_migrate` | sched | the load balancer moving a runnable task: `from` and `to` a CPU's queue |
| `sched_new` | sched | a task put on a queue for the first time |
| `sched_exit` | sched | a task whose CPU is taking its last reference and its stack |
| `sched_ipi` | sched | an interrupt another CPU sent, and the queued work it found (`tasks`) -- a wakeup's reschedule, a TLB shootdown |
| `sched_preempt_deferred` | sched | a reschedule that landed on a task with preemption disabled: its count, and whether it was an idle task -- the case a woken vCPU waited a slice in |
| `idle_enter`, `idle_exit` | idle | a CPU about to halt and back from it: the idle task's `pid`. What an idle cost is the two's distance |
| `sched_slice` | debug | the slice the decision gave the current task, and how many could run -- a record each decision, so opt in |

`trace on sched` turns the eight decision events on; `trace on idle` the
two the idle loop makes; `trace on debug` the slice one. A trace of
`trace on sched idle` is a schedule that can be read as a timeline:
`sched_switch` from `prev` to `next` on a CPU, `sched_wakeup` for whoever
was handed back, `idle_enter`/`idle_exit` around what the CPU did instead.

What they cost when off was measured rather than assumed: ten interleaved
boots of the build before and the build with them, fifteen 3 s windows of
`sched` each (an idle 4-CPU machine under HVF), put the median window at
897 ns a decision without the sites and 887 ns with them -- one build's own
windows differ by 42 ns (one standard deviation) -- and `trace bench`'s own
check at 0.14-0.37 ns an iteration, in both builds. That the measurement
would see the sites at all: the same boot with `sched` and `idle` on, every
decision recording, put it at 1.0 us.

Where each sits, and why there: `sched_switch` in `TaskQueue::switch_to`
(the two tasks as the CPU leaves them, the queue's lock still held);
`sched_block` where the decision took the task off the run list, so that
`ran_ns` is the stretch it just ran; `sched_wakeup` at the compare-and-swap
that made a task `RUN_WAKING`, and at the sleep list's walk for a
deadline's own; `sched_migrate` after the task is on its new queue;
`sched_new` where a task is first put on one; `sched_exit` before its stack
is given back; `sched_ipi` in `Cpu::ipi`; `sched_preempt_deferred` where
`schedule_current` puts the reschedule off; the two idle events around the
halt itself. The names they record are a task's own, written when it is
made and never moved, so reading one in an interrupt's context needs no
lock.

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
  freed and made again, and every word the command refuses. Then the
  scheduler's, in three phases: `sched_new` and `sched_exit` alone (a task
  made and a task exiting); the decision events and the two the idle loop
  makes, from a `sched probe`; and `sched_slice` on its own -- a record a
  decision, so it fills a ring while the others' would be read. Every record
  held to its description's fields, every name a switch records one a task
  has and at most the field's 12 bytes, and the idle ones among the ones
  `ps` lists.
