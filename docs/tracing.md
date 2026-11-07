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
that is on is straight-line code with no call in it: interrupts off by the
CPU's own instructions (PUSHFQ and CLI; MRS and MSR of DAIF), this CPU's ring
found by one load through its per-CPU register, the cycle counter read in
place and unordered (RDTSC; CNTVCT_EL0 with no ISB before it -- the ring
keeps its records' stamps in order itself, [below](#the-rings)), the slot's
ten stores and two barriers, and the interrupts put back as they were (STI,
not POPF). A kernel site builds its payload and writes it in line; one
anywhere else -- Rust's, a test's -- calls `tp::record()`, every argument of
which is in a register: the event's number and size in one, the payload's
five words in the others, from the site that built them to the slot with no
copy in memory between.

`trace bench [cpu=N]` measures a CPU -- the shell's, or N's by a task pinned
to it -- off, and on into a ring of its own through the same path, so that
what the kernel's rings hold is left as it was; and it times a record's
parts, each alone over an empty loop's iteration. They overlap in a record,
so their sum is more than its cost:

| | off | on, a record | interrupts off and back | the counter (ordered) | the slot's stores |
|---|---:|---:|---:|---:|---:|
| x86-64, the EX44's P-core (cpu 16) | 0.50-0.54 ns | 16.1 ns | 7.6-7.7 ns | 5.3-5.5 (5.3-5.6) ns | 2.3-2.4 ns |
| x86-64, the EX44's E-core (cpus 56, 62) | 0.45-0.54 ns | 10.3 ns | 1.7 ns | 8.6 (8.6) ns | 3.2-3.4 ns |
| arm64, Apple Silicon under HVF | 0-0.03 ns | 4.1-5.0 ns | 3.8-5.1 ns | 0 (8.6-10.8) ns | 2.1-2.6 ns |

A record cost 87.8 ns on the EX44 and 23.5-25.2 under HVF before this path
(a0ae873e), and the record a guest's exit makes (`hv bench` with `trace on
hv`) 125-140 ns on a P-core and 270-285 on an E-core. On x86 each of
PUSHFQ, CLI, RDTSC and POPFQ was a function of `asm.asm`'s behind a
`-mcmodel=large` call -- a MOVABS and an indirect CALL -- and POPF, a
microcoded instruction, put the interrupts back; the payload was copied
twice, the second time by a REP MOVSB behind two more calls; the slot's
write was a call of its own; and a scheduler's record copied its tasks'
names with a `str_len` and two string instructions. On arm64 the ISB before
every stamp was two thirds of a record.

Now a guest's exit pays about 45 ns for its record on a P-core (25-60 over
the runs) and about 105 on an E-core (95-115) -- 6ecddfef; d4170e50's path
alone left 55-75 and 130-140. With the
events on and the rings frozen, the site does all its work and the kernel's
write returns at its look at `frozen`, and `hv bench` says 15-40 and 75 of
it are the site's: the payload built and the jump into the kernel, a few
hundred instructions run right after the exit with the return stack just
refilled by hvarch's stub, which `trace bench`'s loop does not see. They
were 60 and 100 before the site stopped reading the counter for `since_ns`
(below) and before `exit_fields` and `times` went in line -- each a call
through the module's GOT, its answer a trip through memory -- and the
bridge's `kernel_tp_record` became a jump into `record()`. The exit with
every event off is 25-35 ns cheaper than it was before d4170e50 as well:
the IRQ wrappers in place are on its path in every IRQ-safe lock it takes.
On a P-core the interrupts off and back -- PUSHFQ, CLI, STI -- are half a
record, 7.6 ns on Raptor Cove against 1.7 on Gracemont, whose RDTSC is the
slower one, 8.6 ns against 5.3.

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

A record's stamp is the counter as the CPU read it, unordered -- possibly a
few instructions early -- and never before the stamp of the ring's record
before it: the write takes the later of the two, so a ring's records are in
the order of their stamps as they are of their indices, which its readers
count on (the merge, a window's start). The ISB that would order the read
(`hal::read_cycle_counter`, what times an interval) costs more than all the
rest of a record on arm64, and x86's RDTSC was never ordered.

The ring itself -- its chunks' table, its head, the flag of a write in
progress and its newest stamp -- is one cache line, every CPU's in one block
from the heap's page path, so page-aligned: what a CPU writes at every
record shares its line with no other CPU's ring and no other allocation. A
chunk is 4 KiB pages, not physically contiguous, mapped one by one as the
heap maps a block past its pools -- the kernel maps no 2 MiB page of its
own; the huge pool (`hugepages=`) is guests' memory. A record touches one
line of one page, a translation every 64 records: `trace bench`'s slot's
stores into a ring of `trace size`'s size, against those into its 64 KiB
one, say what the pages and the lines past the caches cost -- under HVF
2.5-3.0 ns against 2.1-2.6 for a ring of 1 or 4 MiB, on the EX44 2.3-2.4 against 2.3-2.4 on a P-core and 3.1-4.5 against 3.2-3.4 on an E-core --
the most a 2 MiB mapping could take off a record.

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
| `trace show [n] [cpu=N] [event\|group\|all...] [field=value...]` | the newest `n` records (50; 4096 at most) of every CPU, or one, merged by time, oldest first; with events or groups named, only theirs; with a field filter -- up to four, all of which a record must pass -- only the records whose field of that name is the value: a number, decimal or `0x` hex, or an enumeration's name, each event's own (`tcp_recv verdict=outside`, `sched_switch prev_state=blocked`). A record of an event without the field is none the filter keeps. A word that names nothing -- or a cpu not running, `n` of 0, a field no event of the window has or a value it cannot be -- is refused, not taken for a filter that matches nothing |
| `trace dump [n] [cpu=N] [event\|group\|all...] [field=value...]` | the same window as JSON, a record a line, times in exact ns, after a line saying from when the window is whole: what `scripts/trace-decode.py` reads |
| `trace latency [n] [cpu=N] [event\|group\|all...] [field=value...]` | the window's wakeups, each paired with the switch that answered it: p50/p90/p99/max and the longest five, with the task and the CPUs ([below](#how-long-a-woken-task-waited)) |
| `trace stat` | each CPU's ring: written, kept, dropped; and the wakeup trigger, armed or what it fired on |
| `trace clear` | what is recorded forgotten: `show` starts after it |
| `trace size <KiB>` | the rings' size, 64 KiB times a power of two up to 64 MiB; rings there are made again at it, what they held with them -- and a freeze, or a trigger armed to keep it |
| `trace free` | every event off, every ring given back, no freeze or trigger left: as before the first `trace on` |
| `trace freeze [wakeup>N<ns\|us\|ms\|s>]` / `thaw` | nothing more recorded until thawed, so that what is in the rings stays as it was -- at once, or when a wakeup waits longer than N ([below](#a-wakeup-that-waited-too-long)). `trace on` says so when it turns events on into frozen rings, and `trace test` refuses them |
| `trace test [n]` | `n` records of the `test` event on this CPU, preemption off for the loop so that all of them are |
| `trace bench [cpu=N]` | what a tracepoint costs this CPU, or N, off and on, a record split into its parts, and the slot's stores into a ring of `trace size`'s size ([What a tracepoint costs](#what-a-tracepoint-costs)) |

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
and is said too. A field filter is another way for a window to lose a
switch, and one the walk cannot see: a filter that drops the switch that
answered a wakeup and keeps a later one to the task -- after it was displaced
and put back -- has the wakeup paired with that later switch, its wait longer
than it was. Through a filter, the waits are the window's, not necessarily
the schedule's.

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

A guest's records -- `trace on hv blk disk fs` -- are read too, after the
scheduler's, or alone with `--hv`:

```
scripts/trace-decode.py --hv dump.txt
```

-- here of `hv-test.py`'s windows, under QEMU's TCG (twice emulated, so
every time is long): the `smp` guest's, and hvboot reading a disk:

```
hv: 22 exits of 2 vCPUs
  vcpu 0 (pid 11): 15 exits over 5.35ms, 5.1% of it in its guest
  vcpu 1 (pid 12): 7 exits over 761.0us, 20.9% of it in its guest
  reason           exits   share   out of the guest after it (the host's cost)
  msr                 15   68.2%   p50 57.0us p99 256.0us max 306.0us
      x2apic icr write                5   p50 41.0us p99 86.0us max 306.0us
      x2apic id read                  2   p50 15.0us p99 15.0us max 256.0us
      x2apic eoi write                2   p50 28.0us p99 28.0us max 78.0us
      ...
  hlt                  4   18.2%   p50 1.52ms p99 1.52ms max 2.53ms -- a halt's sleep, idle
  cpuid                2    9.1%   p50 16.0us p99 16.0us max 62.0us
      leaf 0x1                        2   p50 16.0us p99 16.0us max 62.0us
  cr4                  1    4.5%   p50 324.0us p99 324.0us max 324.0us
  halts: 2 (1 rung, 1 timed); halted p50 1.12ms p99 1.12ms max 2.39ms; woke past its time ...
  timer interrupts after woke_on_time: 1, late p50 467.0us p99 467.0us max 467.0us
  injected: apic 0x40 x1, apic 0x41 x1
  rings rung: 6, to the vCPU's next entry p50 435.0us p99 440.0us max 598.0us
  2 rings not paired: their vCPU's number more than one task's, or no entry after

disk: 2 requests, 2 of them whole in the window
  read              2   in all p50 119.0us p99 119.0us max 1.23ms
      to its task   p50 50.0us p99 50.0us max 265.0us
      served        p50 25.0us p99 25.0us max 735.0us
      back          p50 44.0us p99 44.0us max 226.0us
      served: 2 at_once
  image served then and there: 2 (0 writes), p50 21.0us p99 21.0us max 476.0us
  ext4 commits: 2, 12 journal blocks, p50 504.0us p99 504.0us max 3.57ms, checkpoints 0ns in all
```

(the two rings not paired are the stop's, after which nothing enters).

What an exit cost the host is the `out_ns` of its vCPU's next record --
from that exit to the entry after it -- by its reason and by what it was
about: the port (named where it is one of the PC's or a virtio device's,
its notify said), the MSR, the guest's page (the xAPIC's, the IO-APIC's),
the vector (the host's, under VT-x). A HLT's is the halt's sleep, idle,
said so. A ring is paired with the next entry of the vCPU it named, where
that number is one task's alone among the records -- one VM's, or each VM a
CPU of a number of its own; a posted interrupt's notification is counted and
not paired, the CPU taking it in the guest with no entry at all. A guest disk's request is paired through its
hand-offs by its disk, slot, op, offset and length, which the virtio
disk's records and the disk task's both carry, and the device's commands
with it by their buffer, which is the request's slot: its four stretches --
to the disk's task, served, the device's share, back -- each a spread of
its own. In the Chrome trace a vCPU is a track of its own, a slice every
stretch in its guest and every exit out of it, named by what it was; a
guest disk a track, a slice each request, its stretches in its arguments.

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
`src/crates/base/kcore/src/trace/events.rs`, one list read by one macro
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
`emit()` lays the payload into a slot's five words -- copies of a size the
compiler knows, a few loads -- and hands them to the kernel in registers with
the event's number and size in a sixth (`kernel_tp_record`, a safe call: it
takes values only), the kernel storing them into the slot as they are; a
module's call is one indirect call through its export table. The checks are
the kernel's, BUG_ON and all: a payload of a size the description does not
have stops the kernel at the first record. The hypervisor's records of a
guest CPU's own loop -- `hv_exit`, `hv_inject`, `hv_halt`, `hv_wake`,
`hv_timer` -- carry the pid its `vm::Cpu` asked the kernel once, of the task
that runs it (`note_task`, at the run loop's start), rather than a call into
the kernel at each (`kcore::trace::pid`, which the other sites make).

The boot's `test_tracepoints` has Rust write a hundred records of the test
event (`rust_trace_emit_test`) into the ring it then reads back, beside the
C++ side's hundred, each as written and in order -- the path a module's
tracepoints take, from the flags' address to the ring.

## The hypervisor's events

Group `hv`, recorded by the hv module (`src/crates/hypervisor/hv`): the payloads are
kcore's (`events.rs`), the records built in `hypervisor/hv/src/tp.rs` and written at
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
alone, one record an exit, has every exit's cost in it. `since_ns` costs the
site no read of the counter: the record holds the exit stamp's low half,
and the kernel reads it back as the nanoseconds from it to the record's own
stamp, which its write reads anyway (a field of `Kind::Since`) -- `trace
show` and `trace dump` print it in nanoseconds as any other.

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
each hand-off: group `blk` the virtio disk (`hypervisor/hv/src/devices/blk.rs`),
group `disk` what serves it -- the hv module's disk task
(`modules/hypervisor/hv/src/disk.rs`), the image (`layers/fs/src/image.rs`) and the block
layer's asynchronous queue (`layers/block/src/queue.rs`) -- and group `fs` the
ext4 journal (`layers/fs/src/ext4/journal.rs`). A request is said by its `disk`
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

## TCP's events

A connection's life on the wire and in its program, a record at each step,
group `tcp` (`layers/net/src/tcp.rs`). Every record says the connection by its
`slot` -- the same in all of them -- and its two ports:

| event | where, and what it records |
|---|---|
| `tcp_send` | every segment a connection sends but a pure acknowledgement (`send_segment`): `why` it went -- `new` data, `again` (a copy a recovery sent: RACK's, PRR's, NewReno's), a loss `probe`, the `timeout`'s, the `handshake`'s, the `close`'s FIN, a `reset`, the `persist` timer's probe -- its `seq`, `len`, `flags`, the window field it offered, `cwnd` and `pipe` (what was in flight against it), the sender's `recovery`, and whose task sent it |
| `tcp_ack` | every pure acknowledgement: `ack`, the window field, the right `edge` the window offered, the receive buffer's `free` room behind it -- an edge past the room is a promise the receiver cannot keep -- and its SACK `blocks`, a D-SACK among them (`dsack`), and the stretches it holds past a hole (`ooo`) |
| `tcp_recv` | every segment a connection takes in (`process`, around `handle`): its `seq`, `ack`, `len`, `flags`, window field and SACK blocks, the `verdict` -- `taken`, a `copy` answered with a D-SACK, `outside` the window, a `bad_ack` (RFC 5961 5.2), a `syn` on a synchronized connection, a `reset` taken or `reset_refused` -- and the receiver as the segment found it: `rcv_nxt`, its buffer's `free` room, and how far past RCV.NXT the window it last offered still reaches (`owed`) |
| `tcp_loss` | what the sender made of a loss: a `recovery` RACK began, its `end`, a `timeout`, one found `spurious`, an `undo` by D-SACKs, a `probe_repair`, a peer that `reneged` -- each with `cwnd`, `ssthresh`, `pipe`, what the peer holds past a hole (`sacked`), what is `lost`, `una` and `nxt` after it |
| `tcp_rtt` | a round trip measured (Karn's: the segment timed was sent once): the `sample_us`, the estimate and the timeout after it, and RACK's least round trip |
| `tcp_wait` | a program's `send` or `recv` that waited: `what` for -- the send `buffer`, the peer's `window`, the `cwnd`, what was `lost` to go again first, room in the `table` of what is in flight, or `data` -- how long (`ns`), and whose task |

So where a connection's time goes is in its records: a receive's `tcp_wait`
for `data` is the network's share of a request, a send's for `cwnd` or
`window` the path's or the peer's, and a `tcp_recv` `outside` the window
beside a `tcp_ack` whose `edge` is past its `free` is a receiver that
promised room it did not have. With a field filter, over the UDP shell's
32 KiB, the needle is had without the hay:

```
> trace on tcp
> trace show 50 tcp_recv verdict=outside     # what a receiver turned away
> trace show 50 tcp_send why=again           # what was sent twice
> trace show tcp_loss slot=3                 # one connection's recoveries
> trace show tcp_wait what=cwnd              # sends the congestion window held
```

## The memory's events

Group `mm`: the zeroing of free pages ahead of their allocation
([Paging](paging.md#zeroed-ahead-free-pages-and-the-two-lists)), a record a
zeroing task's batch (`mm/page_zeroer.cpp`):

| event | what it records |
|---|---|
| `mm_zero` | a batch: the `cpu` it ran on, the 4 KiB `pages` it moved onto the zeroed list and the `huge_pages` of the pool's free blocks it zeroed, how long it took (`ns`), and what ended it -- `full` (256 pages), `none` left to zero, the CPU `busy` with another task, or the task told to `stop` |

So how fast free memory is zeroed, on which CPUs, and how often a task gave
its CPU up to another is in the records: `trace on mm`, then `trace show
mm_zero` -- or `trace show mm_zero end=busy` for the batches another task cut
short. A batch that zeroed nothing is no record.

## The interrupts' events

Group `irq`, the hardware interrupts, recorded where each architecture's
dispatch takes one ([Interrupts](interrupts.md)); group `softirq`, the work
their handlers put off, recorded by `kernel/softirq.cpp`:

| event | what it records |
|---|---|
| `irq_entry` | an interrupt taken: its `vector` -- x86's; the INTID on arm64 -- its `kind`, and the `pid` of the task it landed on, whose time it takes (its CPU's idle task when it had nothing to run) |
| `irq_exit` | its handler done: the same three, how long the handler took (`ns`, from the entry), and for the local timer how far past the deadline it was armed for the interrupt was taken (`late_ns`; 0 for any other kind, whose raise is a device's and unseen) |
| `softirq_raise` | a soft IRQ raised for its CPU's softirq task: its `type` (`net_rx`, `blk_io`, `net_tx`) and what the raise did (`how`): joined a run already owed (`pending`), left to the softirq task because it is the one running there -- its handler asking for another pass, or an interrupt that landed on it (`running`) -- woke the task and kicked its CPU (`kicked`), or found no task to run it (`none`, before they are made or once they are stopped) |
| `softirq_entry` | a type's handler about to run on the softirq task: how long the work waited for it (`wait_ns`), from the raise that made the type pending; 0 when that raise was not timed |
| `softirq_exit` | that handler done: how long it ran (`ns`) -- a preemption of the task in it counted in, which the scheduler's events show -- and whether it was raised again meanwhile (`again`), so that the task's loop runs it again |

The `kind` says where the dispatch took it:

| kind | what | x86 | arm64 |
|---|---|---|---|
| `timer` | this CPU's local timer: its tick, or a sleeper's deadline | vector 0xFD | the timer's PPI (27) |
| `ipi` | another CPU's interrupt -- a wakeup's reschedule, queued work, a TLB shootdown -- or a soft IRQ's kick to its own CPU | 0xFE | SGI 1 |
| `msi` | a device's message-signalled interrupt | MSI-X, 0x40 on | an LPI, 8192 on |
| `line` | a device's line | a driver's INTx, 0x38-0x3F | a GIC SPI |
| `shared` | a line several drivers' handlers share | the first in service | -- |
| `pit`, `hpet` | the boot CPU's timekeeping | 0x20 | -- |
| `kbd`, `serial` | the 8042, the UART | 0x21, 0x24 | -- |
| `posted` | a posted interrupt's notification that found its vCPU out of its guest | 0xFC | -- |
| `stray`, `spurious` | a vector or INTID no handler has; the controller's "nothing" | the vector's own, 0xFF | its INTID |

Each record says enough alone. An exit carries its handler's time, its
kind and vector and the task it took the time from: `trace on irq_exit`
alone is the cheap way to have them, one record an interrupt. A soft IRQ's
entry carries the deferred work's wait for its task -- the time from the
interrupt that raised it to the handler, the latency the soft IRQ adds --
and its exit the handler's run:

```
> trace on irq softirq
> trace show 20 irq_exit kind=timer        # every CPU's tick, and how late it came
> trace show 20 irq_exit kind=msi          # the devices' interrupts and their handlers' time
> trace show softirq_entry type=net_rx     # how long received frames waited for their task
> trace on sched
> trace dump 2000 irq softirq sched        # and on the host, trace-decode.py --irq
```

`trace-decode.py --irq` reads them back: each kind and vector's count and
its handlers' p50, p99 and most, each CPU's share of the window's time in
handlers, how late the local timer's interrupts came, and the tasks the
interrupts took their time from; each soft IRQ's runs, waits and raises.
Here an arm64 boot under HVF, four CPUs, a `sched probe` and the UDP shell's
own datagrams, every CPU's part dumped in turn from frozen rings:

```
irq: 368 interrupts on 4 cpus, 0.33% of their time in handlers
  kind        vector   count   handler                                     in all
  timer         0x1b     255   p50 2.0us p99 41.8us max 56.7us             1.65ms
  line          0x4e      22   p50 6.7us p99 29.1us max 60.4us            301.8us
  ipi            0x1      91   p50 1.5us p99 24.7us max 29.4us            261.1us
  cpu 0: 63 interrupts, 703.3us in handlers, 0.41% of 170.40ms
  ...
  the local timer, past the deadline it was armed for: p50 5.2us p99 2.28ms max 2.34ms (255)
softirq: 33 handler runs, 0.02% of the cpus' time
  type      runs   waited for its task                   ran                                       in all  again
  net_rx      11   p50 7.5us p99 10.0us max 14.2us       p50 13.7us p99 24.7us max 25.1us         161.4us      0
  net_tx      22   p50 15.8us p99 32.3us max 41.3us      p50 83ns p99 333ns max 375ns               2.7us      0
  raises of net_rx: 11 -- 11 kicked
```

-- the virtio-net card's line, whose handler's reads and writes of its
registers each exit to the hypervisor, 6.7 us; a tick 5.2 us late at the
median and 2.3 ms at the 99th percentile, those of CPUs halted in WFI, which
HVF wakes late; a received frame's work 7.5 us from the interrupt's raise to
its handler: the rest of the card's handler, the kick's IPI to its own CPU
and the switch to `softirq/0`. Each of the card's interrupts raises what it
has work for ([Interrupts](interrupts.md#deferred-work-softirq)): a reply's
transmit completion alone, then the next request with the reply's last
completion riding on it -- both due, raised in one call, their two records
in the same microsecond and one kick after them -- which `trace show` has
in that order:

```
    3.196148  0 softirq_raise: type=net_tx how=kicked
    3.196155  0 softirq_entry: type=net_tx wait_ns=6.375us
    3.196155  0 softirq_exit: type=net_tx ns=0.208us again=0
    3.196234  0 softirq_raise: type=net_rx how=kicked
    3.196234  0 softirq_raise: type=net_tx how=kicked
    3.196239  0 softirq_entry: type=net_rx wait_ns=4.833us
    3.196246  0 softirq_exit: type=net_rx ns=7.625us again=0
    3.196247  0 softirq_entry: type=net_tx wait_ns=12.583us
```

With the scheduler's records beside them it says what the interrupts and
the soft IRQs did to the tasks: of each of the longest waits for a CPU, how
much was that CPU's interrupts' handlers, and how much its softirq task's
runs -- which the interrupts inside them are counted in too:

```
  the longest waits for a CPU, and what of them was its interrupts and its softirq task:
       32.5us pid 10 udpsh (cpu 2 -> 2): interrupts 4.5us (1), softirq 0ns (0)
       28.6us pid 10 udpsh (cpu 2 -> 2): interrupts 4.2us (1), softirq 0ns (0)
```

-- the shell's task woken onto an idle CPU, its wait the wakeup's IPI
reaching a CPU halted in WFI, not that CPU's work. Its Chrome trace has,
under each CPU's track, a row of its interrupts and one of its soft IRQs.

Where each sits, and why there. On x86 each IDT stub's C entry --
`ip_interrupt`, `lapic_timer_interrupt`, `posted_interrupt`,
`dispatch_rust_msix`, `dispatch_rust_interrupt`, the shared dispatch, the
PIT's, the HPET's, the 8042's and the serial port's `interrupt()`,
`dummy_interrupt_dispatch` and `spurious_interrupt`; on arm64 the loop of
`arm_irq_entry` -- each INTID it acknowledges, its LPIs', its lines', a
stray or spurious one -- but for the IPI, which is `ip_interrupt`'s on both,
and the timer, `GenericTimer::local_tick`'s. The site is a `tp::Irq` on the
entry's stack (`kernel/tracepoint.h`), whose exit is written as it goes out
of scope -- past the EOI -- or before then: the IPI's and the timer's
handlers end in `preempt()`, which may switch to another task and come back
to this one only later, so `Cpu::ipi`, `Cpu::timer_tick` and `local_tick`
end the record before it. The reschedule is the scheduler's to record. The
exit's `ns` costs the handler no read of the counter: the entry's stamp's
low half is the field (`Kind::Since`), right for any handler under 2^32
cycles. The task the interrupt landed on is the one whose stack it runs on
-- every interrupt here does -- as a wakeup's waker is found. `late_ns` is
what cyclictest measures, with no task in it: how far past the deadline its
CPU took the timer's interrupt, read before the interrupt arms the timer
again -- interrupts off, a deep idle's exit, a hypervisor's delivery.

A soft IRQ's wait is counted from a stamp the raise that made the type
pending writes, per CPU and type, while `softirq_entry` is on; the run that
takes the type reads it -- while the bit is still up, when no raise writes
one -- and zeroes it whether the event is on or not, so that no stamp
outlives its pending. A raise that finds the type pending leaves the stamp
that is there, and one that wrote a stamp and then lost the bit to another
-- an interrupt's raise, landing between a task's look and its bit -- takes
its own back: the run says it was not timed, rather than a wait from a
stamp of its own. A raise is recorded before the hand-off: from the
softirq task's wakeup on, its handler may run, and its entry must come
after.

What they cost when off was counted in the code, as `switch_to`'s was
above, from the objects of the build before them and after, both
architectures, function by function. A site on x86 is the entry's frame
for its `tp::Irq` and four stores zeroing it, the flags' address, the two
flags' bytes or'ed and a branch, and the destructor's byte compared and a
branch: thirteen instructions on an MSI-X interrupt's path
(`dispatch_rust_msix`), its slot's handler still in line in it; fourteen on
the IPI's and the local timer's entries, which hand the scope on, and three
more in `Cpu::ipi` and five in `Cpu::timer_tick` -- the scope's reference
kept, its end's look, and the timer's look at whether the exit is timed.
On arm64 the flags' address is the loop's, loaded once: each interrupt
`arm_irq_entry` takes pays three stores, two loads, an `orr`, a `cbnz`, and
the destructor's load and `cbz` -- nine instructions. A soft IRQ's raise is
a flag's load and a branch for the stamp, a register cleared, and a flag's
load and a branch for its record on whichever way it goes out; a run, a
flag's load and a branch, the stamp zeroed, and a branch on the two flags
before its handler. On, an interrupt is two records (`trace bench`'s cost
of one, above), the task's lookup and a read of the counter; `irq_exit`
alone is one record.

## An event of its own

An event is four things in `kernel/tracepoint.{h,cpp}`: its number in
`tp::Id` (appended, never reordered -- a saved trace names events by
number), its payload struct (40 bytes at most, `static_assert`ed), its fields'
table (name, `offsetof`, `sizeof`, how to print it), and its line in the
`EVENTS` table (name, group, help, size). A site checks `tp::on()` and calls
`tp::emit()` with exactly the payload's size -- a size its description does
not have is a `BUG_ON`. An event Rust records has one thing more, its line in
`base/kcore/src/trace/events.rs`: its number, and its payload's fields with the
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
  inputs, the machine's `kernel_tp_record` holding each record to its
  description, an enumeration's value one it names and every pad zero; and
  `hv-test.py` (x86-64) traces hvboot's reads of a disk it is given -- each
  taken off the ring, picked up by the disk's task, served by the image then
  and there, done and given back, in that order, slot by slot, by the
  disk's own task -- and a file written, its ext4 transactions committed.
- TCP's events: every boot's `rust_test` holds their payloads to the
  descriptions; `net-fuzz.py` (host, in CI) runs every one under its
  overflow checks for half of each target's inputs -- `tcp` among them, its
  peer taking SACK or not, losing and reordering -- the machine's
  `kernel_tp_record` holding each record to its description, an
  enumeration's value one it names and every pad zero.
- The interrupts' and the soft IRQs' events: `trace-test.py` turns them on
  over a probe and the shell's own datagrams and holds what it reads back
  to what the kernel did -- every kind one it has and every interrupt
  landed on a task; per CPU each exit after its entry, the same vector and
  kind, none inside another; no exit's `ns` past the distance from its
  entry, and nine in ten of them that distance less the entry record's
  tail; the local timer's interrupts on every CPU, each `late_ns` under a
  second and some of them late; a device's interrupt, a line's or an
  MSI's; per CPU each soft IRQ's exit after its entry; each receive run's
  `wait_ns` reaching back to the raise that made the type pending, which
  is recorded before the run; a filter by an interrupt's kind; and
  `trace-decode.py --irq` reading the dump. `trace-decode.py --selftest`
  holds the report to a made-up window -- a task woken halfway through the
  softirq task's run, its wait split into the CPU's interrupts and that
  run from the wakeup on, and an idle CPU's one interrupt a share of the
  window's whole stretch. Controls: the share of a wait counted without
  clipping to it, an exit with no time taken as an interrupt of none, and a
  CPU's share of its own records' span, each fail it. `trace-test.py
  --arch x86_64`, where grub-mkrescue is, holds x86-64's interrupts to the
  same phase.
- A window's field filters: `cpp-fuzz.py --target tracewin` gives a quarter
  of its windows one or two -- a record's bytes at an offset and a size of
  its event's payload, and now and then a field of another event, at some
  place, holding some value -- and holds each CPU's part to the model's own
  reading of those bytes. Control: `keeps` passing a record whatever its
  field's value is caught at the first input. `trace-test.py` filters by a
  number, decimal and hex, and by an enumeration's name, and has each kind
  of filter that would match nothing refused.
- `trace-decode.py --hv`: `hv-test.py` dumps each of its windows and reads
  them back with it, as shipped, held to what the guests did -- the
  `exits` guest's exits by reason and by what each was about, the 20 outs
  to the debug port and the one in, each with the host's time after it;
  the `smp` guest's rings each paired with its CPU's next entry, its halts
  and its timer interrupt; hvboot's two reads paired through every
  hand-off with nothing left over -- and the Chrome trace of each, its
  vCPU's track and its disk's.
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
