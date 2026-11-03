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
The code is `kernel/tracepoint.{h,cpp}`, `kernel/trace_ring.{h,cpp}` (a CPU's
ring) and `kernel/trace_window.{h,cpp}` (what the shell's readers make of the
rings: the window, the merge, the waits -- with nothing of the kernel in it,
so that a fuzzer runs it on the host).

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
`trace bench` measures both on the CPU it runs on -- the one that is on into
a ring of its own, through the same path, so that what the kernel's rings
hold is left as it was:

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
| `trace show [n] [cpu=N] [event\|group\|all...]` | the newest `n` records (50; 4096 at most) of every CPU, or one, merged by time, oldest first; with events or groups named, only theirs. A word that names nothing -- or a cpu not running, or `n` of 0 -- is refused, not taken for a filter that matches nothing |
| `trace dump [n] [cpu=N] [event\|group\|all...]` | the same window as JSON, a record a line, times in exact ns, after a line saying from when the window is whole: what `scripts/trace-decode.py` reads |
| `trace latency [n] [cpu=N] [event\|group\|all...]` | the window's wakeups, each paired with the switch that answered it: p50/p90/p99/max and the longest five, with the task and the CPUs ([below](#how-long-a-woken-task-waited)) |
| `trace stat` | each CPU's ring: written, kept, dropped; and the wakeup trigger, armed or what it fired on |
| `trace clear` | what is recorded forgotten: `show` starts after it |
| `trace size <KiB>` | the rings' size, 64 KiB times a power of two up to 64 MiB; rings there are made again at it, what they held with them -- and a freeze, or a trigger armed to keep it |
| `trace free` | every event off, every ring given back, no freeze or trigger left: as before the first `trace on` |
| `trace freeze [wakeup>N<ns\|us\|ms\|s>]` / `thaw` | nothing more recorded until thawed, so that what is in the rings stays as it was -- at once, or when a wakeup waits longer than N ([below](#a-wakeup-that-waited-too-long)). `trace on` says so when it turns events on into frozen rings, and `trace test` refuses them |
| `trace test [n]` | `n` records of the `test` event on this CPU, preemption off for the loop so that all of them are |
| `trace bench` | what a tracepoint costs this CPU off and on |

A record prints as its time since boot, its CPU, the event's name and its
fields as `name=value` -- from the event's description, one format for all.
`dump` prints the same records as objects -- `{"t":<ns>,"cpu":N,"ev":"<name>","f":{...}}`,
the CPU outside the fields' object, as some of them are called `cpu` too --
so a host reads numbers, not a format. Its first line is the window's own,
`{"window":{"records":N,"complete_from":<ns>,"cpu":K}}`: from when on no CPU's
part of it lacks a record, and whose part sets that (0 when nothing is
missing; `null` when a CPU's part could not be read at all).

A CPU whose part begins later than what is shown -- its older records
written over by its ring, or left out for room -- is said above the records
(`cpu 3: nothing before 4.123456`), so that its silence before then is not
read as a CPU that did nothing.

`tracepoints=<list>` on the kernel command line turns events on as the boot
ends, once every CPU has its ring ([kernel parameters](kernel-parameters.md)).

## How long a woken task waited

`trace latency` is the shell's own answer to the question a schedule trace
is usually read for: a task woken at one instant and running at another,
how long between? It walks the window `show` prints -- the newest `n`
records of every CPU, merged by time -- keeping each `sched_wakeup`'s pid
and instant in a table by pid (ids are 0 to 255, so the table is an array),
and pairs it with the first `sched_switch` after it whose next task is that
pid. The difference is the wait; a deadline's wakeup is counted from the
deadline itself, its record's `late_ns` before it, as `sched` and the
trigger below count it. A wakeup whose switch is not in the window -- still
waiting, or after it -- is counted as not run in it, never guessed.

Each CPU's part of the window is its own ring's newest records, and a busy
CPU's ring reaches back milliseconds where an idle one's reaches back
seconds: before the latest point from which every part is whole (the
`complete_from` above), a wakeup's switch may be among what one ring lost,
and the next switch to its task -- after it was displaced and put back --
would be taken for it. So the walk starts there, and what is older is
counted and left out (`the oldest 120 left out: cpu 2 holds nothing before
...`). A task woken again before any switch to it says the window lost one,
and is said too.

```
> trace on sched
> trace latency 4000
trace latency: 2000 records on 4 cpus -- 43 wakeups, 43 ran in the window, 0 not run in it
  wait p50 208ns p90 10.1us p99 15.3us max 20.2us
  the longest:
     20.2us pid 9 udpsh        cpu 1 -> 1
     15.3us pid 11 probe/wait  cpu 3 -> 3
```

The percentiles are exact, not buckets: the waits are sorted -- a heapsort,
in place and n log n whatever the order, where a quicksort's stack pushed
the wrong half and was a panic on waits that rose and fell. The five
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
to keep. The scheduler reads a word that is 0 unless a trigger is armed, and
the first CPU to fire claims the record with an exchange and writes it as a
seqlock, then freezes the rings and puts the word back to 0. Every arming,
disarming and thaw first puts that word to 0 and waits out every CPU
(`CpuTable::synchronize`): the trigger is asked with interrupts off, inside
the decision, so a CPU that has taken an interrupt since is out of any
firing of the arming before -- none of its stores can land after the new
arming, after a thaw's "recording", or into a record being reset. `trace
stat` reads the record a bounded number of times and says so if it was
being written at every look, rather than waiting on it. `trace thaw`
records again and forgets the record; a plain `trace freeze` keeps it;
`trace free` and `trace size` leave neither a freeze nor a trigger.

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
same records: from where the dump's `window` line says every part is whole
(the latest of them, for a dump read in parts), a deadline's wakeup from its
deadline. The Chrome trace is a track a CPU, a slice every stretch a
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
| `sched_wakeup` | sched | a task handed back to its queue: `pid`, `comm`, the `cpu`, whether its `deadline` came or someone's `unblock()`, the task that woke it (`waker_pid` -- the one an interrupt landed on, from an interrupt's context -- 4294967295 where nobody did), and for a deadline's how late the sleep list was walked past it (`late_ns`): its wait is counted from the deadline |
| `sched_block` | sched | a task that left its CPU because it cannot run: whether it is `deadline`-bound, and how long it `ran_ns` -- from the switch that gave it the CPU, across every decision that left it there |
| `sched_migrate` | sched | the load balancer moving a runnable task: `from` and `to` a CPU's queue |
| `sched_new` | sched | a task put on a queue for the first time, before it can run |
| `sched_exit` | sched | a task that has exited, as it gives its pid back: before any task started after it can be given that pid |
| `sched_ipi` | sched | an interrupt another CPU sent, and the queued work it found (`tasks`) -- a wakeup's reschedule, a TLB shootdown |
| `sched_preempt_deferred` | sched | a reschedule that landed on a task with preemption disabled: its count as the reschedule found it (1 for one lock held), and whether it was an idle task -- the case a woken vCPU waited a slice in |
| `idle_enter`, `idle_exit` | idle | a CPU about to halt, and its idle over -- the halt returned, or a decision took the CPU from the idle task first: the idle task's `pid`. What an idle cost is the two's distance |
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

`TaskQueue::switch_to` comes after the decision's cost is taken, so what it
gained when the idle's end and `sched_block`'s base moved into it was
counted in the code instead: against the build before, its path with the
events off has three instructions more on arm64 -- a store of
`switched_in_at_`, a byte's load of the open-idle flag, a `cbnz` not taken
-- and four on x86-64 (the store, the load, `testb`, a `jne` not taken);
the rest of the difference is the block that closes an open idle, out of
the path's line, and the registers it renamed.

Where each sits, and why there. A record that hands a task to another --
a queue, a CPU, a pid's next owner -- is written before the hand-off, so
that nothing the other side does can come before it in the merged trace,
and so that the task is still the writer's alone to read: `sched_wakeup`
after the compare-and-swap that made the task `RUN_WAKING` and before
`push_wake` hands it to its CPU (which may run it, see it exit and clear
`task_queue_`), and at the sleep list's walk before the enqueue;
`sched_new` before the insert that lets it run -- an interrupt the insert's
unlock lets in may switch to it on that very CPU; `sched_migrate` with the
task off the old queue and not yet on the new; `sched_exit` in
`Task::exit` before the pid goes back to the `TaskTable`, which gives the
lowest free id to the next task started anywhere. `sched_switch` is in
`TaskQueue::switch_to`, the two tasks as the CPU leaves them, the queue's
lock still held; `sched_block` where the decision took the task off the run
list, `ran_ns` from `Task::switched_in_at_` -- set by the switch alone,
where `run_start_time_` moves on at every decision that leaves the task its
CPU; `sched_ipi` in `Cpu::ipi`; `sched_preempt_deferred` where
`schedule_current` puts the reschedule off.

The idle events: `idle_enter` before the halt opens an idle
(`TaskQueue::open_idle`), and the first of three closes it with the
`idle_exit` -- MWAIT's wake, read before the interrupt that ended it is
taken (`Cpu::idle_mwait`); the switch that takes the CPU from the idle task
(`TaskQueue::switch_to`, before its `sched_switch`); or the halt returning
to `Cpu::idle`. So no switch on a CPU falls inside its idle, and the time a
woken task then ran is not counted as one. An interrupt in the few
instructions between the enter and the halt that switches away ends the
idle there; the halt the idle task then makes when it is switched back to
goes unrecorded. The names the events record are a task's own, written
when it is made and never moved, so reading one in an interrupt's context
needs no lock.

## Tracepoints from Rust

Rust's sites -- in the image (the block, net and filesystem layers, the
drivers) and in a module (`hv.ko`) alike -- record the kernel's events
through `kcore::trace`, into the same rings by the same `emit()`:

```rust
if trace::on::<HvExit>() {
    trace::emit(&HvExit { vcpu, reason, ..Default::default() });
}
```

An event's description stays the kernel's, one table for both languages: its
number in `tp::Id`, its payload struct and fields' table, its line in
`EVENTS` (below). What Rust has of one is in
`src/crates/kcore/src/trace/events.rs`, one list read by one macro
(`events!`): the event's number, its payload -- a `#[repr(C)]` struct of
integers and arrays of them, with no padding, which is asserted, since the
ring copies out every byte -- and the event it records, which ties the two
together, so that no payload can be emitted as another event's. A payload
of the kernel's layout written twice, once in each language, is a payload
that will one day differ: every boot's `rust_test` holds each one to the
kernel's description -- its name, its size, every field's name, offset and
size, both ways, and an enumeration's names value by value
(`kcore::trace::check`) -- and panics on the first that does not match, so a
field added on one side alone, or a value named on one side alone, fails the
smoke boots. An enumerated field is declared `as` a type of `names!` in the
same file -- `reason: u8 as ExitReason` -- whose variants a site writes
(`ExitReason::Msr as u8`) and whose names are the kernel's `Field::names`.

A module's copy of kcore is never checked so: the payloads it builds are the
ones its source declared. So the file is part of what the loader holds a
module to -- `MODULE_ABI`, the digest every `.ko` carries, is of the `ffi`
crate's sources and of `events.rs` -- and a module of other payloads is
refused at `insmod`, rather than its records printed wrong or `emit()`'s
`BUG_ON` reached.

`on()` is two loads and a branch: the flags' address, asked of the kernel
once by each copy of kcore -- the image's, and each module's, which reaches
the kernel through its export table (`kernel_tp_flags`) and keeps the
address -- and the event's byte. A module imports functions, not data, so it
cannot read `tp::enabled` by name as the C++ does; that is the second load.
`emit()` is the kernel's, BUG_ON and all: a payload of a size the
description does not have stops the kernel at the first record.

The boot's `test_tracepoints` has Rust write a hundred records of the test
event (`rust_trace_emit_test`) into the ring it then reads back, beside the
C++ side's hundred, each as written and in order -- the path a module's
tracepoints take, from the flags' address to the ring.

## The hypervisor's events

Group `hv`, recorded by the hv module (`src/crates/hv`): the payloads are
kcore's (`events.rs`), the records built in `hv/src/tp.rs` and written at
the sites named here. Each says its guest CPU by its number in its guest
(`vcpu`) and its task's `pid` -- the BSP's is the task that runs the VM,
each other's `hv/<vm>/cpu<n>` -- which tells one VM's from another's and
ties the records to the scheduler's of the same task.

| event | what it records |
|---|---|
| `hv_exit` | a guest CPU out of its guest, or turned back on its way in (`kicked`): its `reason` -- one value a kind of hv's `Exit` -- what it was about (`detail`: io the port, msr the MSR, npf the guest physical address, exception and eoi the vector, cpuid the leaf, hypercall RAX, cr0 the value, cr4 and cr8 the register's number, dr the debug register's, host the host's vector that took it out under VT-x -- AMD-V does not say -- other the CPU's own exit code) and more of it (`info`: a port access's size, 0x100 an IN, 0x200 a string, 0x400 REP; 1 for a write of an MSR or a register; cpuid the subleaf; npf and exception the error code), its `rip`, and three times: `guest_ns` from the entry to the exit, `out_ns` from the exit before to this entry -- the host's handling, the loop's look, a halt's sleep -- and `since_ns` from the exit to the record: the host's interrupt it was for, taken first, and whatever ran in its wake |
| `hv_inject` | what went in at an entry: an `exception` of the guest's own given back, an `nmi`, an interrupt from the 8259 (`pic`) or the local APIC (`apic`), or `window` -- none it can take yet, the CPU asked to exit when it can -- and its `vector`. Under VT-x with APIC virtualization the CPU delivers the APIC's interrupts itself, and none of those is recorded |
| `hv_halt` | a halted CPU with nothing to take: what it sleeps `until` -- its APIC timer's edge, the PIT's or the devices' next round (the first CPU's), the end of its run, or nothing armed (10 ms) -- `wait_ns` to it, and the spin it tries first (`poll_ns`, 0 for none) |
| `hv_wake` | that halt over: `polled` (rung within the spin), `rung` (while asleep), `timed` (its time came) or `advanced` (woke short of its timer's edge and spun the rest); `halted_ns` from the halt's look, `late_ns` past the time it slept to |
| `hv_timer` | a CPU's APIC timer interrupt asked for, `late_ns` past its edge, and what its task did before the round that found it (`how`, the run loop's `Step`: `woke_late`, `woke_on_time`, `not_slept_for`, `spun`, `off_cpu`, `host`, `exit`, `other`) -- what `hv`'s report counts the late ones by |
| `hv_ring` | a CPU rung: its event signalled and its kick sent (`rung`), found spinning for a ring (`polling`), its event alone (`signalled`, a key at its console), or a posted interrupt's notification sent to the CPU it runs its guest on (`notified`); `pid` the task that rang -- another CPU's for an IPI, a device's, the one an interrupt landed on |

`hv_exit` is written in `vm::Cpu::enter`, which every entry of every guest
comes through -- a Linux guest's and a built-in one's -- after the exit is
decoded. Its times are the cycle counter's, read by hvarch around the world
switch with interrupts off (`hvarch::x86::svm::Run`): just before `vmrun`,
`vmlaunch` or `vmresume`, and just after the exit. Off, an entry reads
nothing: the stamps are taken only while `hv_exit` is on (or `hv bench`'s
profile asks), so the off path is the flag's load and a branch in hvarch
and the same in `Cpu::enter`. An exit's own time is its record's less
`since_ns`, and the entry it ended its exit's less `guest_ns`; the host's
time over an exit is the next record's `out_ns` -- so a trace of `hv_exit`
alone, one record an exit, has every exit's cost in it.

`hv_inject` is written in the run loop's `deliver`, as each event goes in;
`hv_halt` and `hv_wake` around a halted CPU's spin and sleep (not a CPU
asleep for good -- INIT-waiting, or halted with interrupts off -- which only
another CPU wakes); `hv_timer` where the loop asks the APIC for the timer's
interrupt. `hv_ring` is written by `Doorbell::ring` and `signal`, before the
vCPU is woken or kicked -- the hand-off rule above, so that its `hv_wake`
comes after it -- and in `post_edge` after a notification is sent, the one
hand-off with no record of its own on the other side: the CPU takes a posted
interrupt in the guest.

A time of 4.29 s or more is held at that, the most a u32 of nanoseconds has.

## A guest disk's events

A request from a guest's virtio disk to the device and back, a record at
each hand-off: group `blk` the virtio disk (`hv/src/devices/blk.rs`),
group `disk` what serves it -- the hv module's disk task
(`modules/hv/src/disk.rs`), the image (`fs/src/image.rs`) and the block
layer's asynchronous queue (`block/src/queue.rs`) -- and group `fs` the
ext4 journal (`fs/src/ext4/journal.rs`). A request is said by its `disk`
(vda 0) and its `slot`, the buffer it has while it is out; a command to
the device by its queue's `entry` and `buffer`.

| event | where, and what it records |
|---|---|
| `blk_submit` | the request taken off the ring and handed to the backend (`Blk::start`), by the vCPU whose notify it was -- or the first CPU's round: its `op` (read, write, flush, discard, write_zeroes), `offset` and `len` |
| `disk_start` | picked up by the disk's task (`serve`), before it goes to the image |
| `image_sync` | served by the image then and there, through its filesystem, after what it had in flight (`Image::start`'s synchronous path): a write into a hole, a request its blocks do not line up for; its `ns`, the drain included -- what a first write costs |
| `bio_submit` | a command of the image's handed to the device (`Queue::start`), before the device can have it: its `sector`, `count` and the queue's commands `in_flight` with it |
| `bio_done` | that command's completion, from the device's interrupt (`on_done`), on the CPU that took it -- or, `refused`, its submit's refusal, so that every `bio_submit` has its one `bio_done` |
| `image_note` | the blocks written over unwritten extents, made written in the filesystem at last (`make_written`): `from`, `blocks`, `ns` |
| `ext4_commit` | a transaction committed to the journal: its `seq`, the journal's `blocks` it took, its `ns` and the part of it a checkpoint took (`checkpoint_ns`) -- a commit of nothing is no record |
| `disk_done` | the request done in the disk's task, before it goes back: `how` -- with the device and reaped (`async`), done by the image's start (`at_once`), or a flush, discard or write of zeros served on its own (`command`) |
| `blk_complete` | back at the virtio disk (`Blk::complete`), in the first CPU's round, before it is given back to the guest |

So a request's time is the sum of its hand-offs: `blk_submit` to
`disk_start` is the disk's task waking for it, `disk_start` to `disk_done`
the image's work -- `bio_submit` to `bio_done` the device's share of it --
and `disk_done` to `blk_complete` the vCPU's round coming for it. With
`trace on hv` beside it, the notify's `hv_exit` comes before the
`blk_submit` and the interrupt's `hv_inject` after the `blk_complete`.

## An event of its own

An event is four things in `kernel/tracepoint.{h,cpp}`: its number in
`tp::Id` (appended, never reordered -- a saved trace names events by
number), its payload struct (40 bytes at most, `static_assert`ed), its fields'
table (name, `offsetof`, `sizeof`, how to print it), and its line in the
`EVENTS` table (name, group, help, size). A site checks `tp::on()` and calls
`tp::emit()` with exactly the payload's size -- a size its description does
not have is a `BUG_ON`. An event Rust records has one thing more, its line in
`kcore/src/trace/events.rs`: its number, and its payload's fields with the
same names, offsets and sizes -- an enumeration's with a `names!` type of
the same names -- and its padding named, `_pad`, so that the struct has
none; the boot's check holds it to the C++ table, which says the order a
record prints its fields in. The emit path takes no lock and allocates nothing:
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
- `cpp-fuzz.py --target tracewin` (host, in CI): `kernel/trace_window.cpp`
  on rings of `trace_ring.cpp`'s own. A schedule is made up and played out
  on up to six CPUs -- tasks woken by one CPU onto another's queue, some at
  a deadline and late, switched to, displaced, switched to again, blocked,
  idle and IPI records between, a `trace clear` now and then, rings that
  wrap -- and the window read as `trace latency` reads it, with a window
  size and an event filter of the input's, is held to what the schedule
  did: each CPU's part the newest records its ring holds that the filter
  keeps, each as written; nothing the CPU wrote missing from where its part
  says it is whole; the merge the newest of them all in time order; every
  wait paired one the schedule had -- its task, its CPUs, from its wakeup
  (a deadline's from the deadline) to the first switch to it -- none left
  out, and with switches in the window no task found woken twice. Then the
  sort against `std::sort` on orders made to be a quicksort's worst, the
  five longest against a stable sort, a duration's parse against a model,
  and a writer thread filling a ring while its window is read. Controls:
  the quicksort it replaced is a panic at the 48th input; the window's
  start ignored -- the pairing across a ring's loss behind `trace latency`'s
  made-up waits -- is caught at the 44th.
- The hypervisor's events: every boot's `rust_test` holds their payloads to
  the descriptions, names and all; `hv-fuzz.py` (host, in CI) builds them
  with its overflow checks on, half its whole-machine runs with them on --
  from every exit the scripts make, and every halt, wake, injection and
  ring the run loop meets -- and holds each record to its description, an
  enumeration's value one it has a name for and every pad zero; and
  `hv-test.py` (x86-64) runs two built-in guests with `trace on hv`: the
  `exits` guest's 21 port accesses, its CPUID of its leaf and its HLT, each
  recorded with what it was about and nothing else but the host's own
  interrupts, one task's, every entry but the first with its time out of
  the guest; the `smp` guest's four ICR writes on CPU 0 and one on CPU 1,
  CPU 1 rung for the INIT and both start-up IPIs by CPU 0's task and CPU 0
  for the IPI by CPU 1's, its timer's interrupt asked for once and -- where
  the host injects them -- the IPI and the timer's interrupt injected once
  each, every halt ended by a wake; and with every event off, nothing.
- A guest disk's events: every boot's `rust_test` holds their payloads to
  the descriptions; `hv-fuzz.py` builds `blk_submit` and `blk_complete`
  under its overflow checks for half of every target's inputs -- the disk's
  and the virtio queues' among them; `fs-fuzz.py` and `net-fuzz.py` run the
  layers' own -- the image's, the block queue's, ext4's -- for half their
  inputs, the machine's `kernel_tp_emit` holding each record to its
  description, an enumeration's value one it names and every pad zero; and
  `hv-test.py` (x86-64) traces hvboot's reads of a disk it is given -- each
  taken off the ring, picked up by the disk's task, served by the image then
  and there, done and given back, in that order, slot by slot, by the
  disk's own task -- and a file written, its ext4 transactions committed.
- `trace-test.py` (arm64; x86-64 where grub-mkrescue is): a kernel booted
  with `tracepoints=test`, and `trace` driven over the UDP shell -- the
  boot's events on, records shown merged and in order, rings made again at
  another size, three times round one of 1024, cleared, frozen and thawed,
  freed and made again, and every word the command refuses. Then the
  scheduler's, in phases: `sched_new` and `sched_exit` alone (a task made
  and a task exiting); the decision events and the two the idle loop makes,
  from a `sched probe`; and `sched_slice` on its own -- a record a
  decision, so it fills a ring while the others' would be read. Every record
  held to its description's fields and to the CPU it was recorded on, every
  name a switch records one a task has and at most the field's 12 bytes,
  and the idle ones among the ones `ps` lists. Then `trace latency` over a
  probe, the trigger armed at a second and still armed, re-armed at a
  microsecond and fired, the rings then provably frozen; `trace dump`'s
  window line and records parsed as JSON, and `trace-decode.py` reading
  them. Then, each for a fault the review of these found: a group as a
  window's filter, and a word naming nothing refused; every `sched_block`'s
  `ran_ns` the stretch since its task's switch in, to within 50 us of the
  two records' times -- `trace bench` keeping the shell's command task on
  its CPU across the tick first, so that the formula it replaced, 30 ms off,
  fails it; no task switched to before its `sched_new`, and no task's
  `sched_exit` after a later task's `sched_new` under its pid; no switch
  inside its CPU's idle; `trace bench` writing no CPU's ring; `trace on` and
  `trace test` saying the rings are frozen, and `trace free` leaving them
  thawed.
