# Profiler

A sampling profiler that answers "where is this machine actually spending
its time" from the shell of a running kernel — including over the
[UDP shell](udp-shell.md), which is the only way to ask on a box whose
console is a network socket.

```
> profile                    # 1000 ms, every task, the 8 hottest chains
> profile 2000 all 3         # 2 s, every task, 3 chains -- fits one screen
> profile 500 12             # 500 ms, only pid 12
```

The window is 1–2000 ms. `all` is how a chain count is given without a pid
filter: on a console with no scrollback the third argument is the one that
matters, and it should not require inventing a pid.

The code is `kernel/profiler.cpp`; the performance-counter backend is
`arch/x86_64/pmu.cpp` behind `hal/pmu.h`.

## How a sample is taken

Most of a profiler already existed in this kernel and is reused rather than
rebuilt: `SymbolTable::Resolve` turns an address into a name using the table
the [two-pass link](debug.md) bakes into the image, and
`StackTrace::CaptureFromSp` walks the frame-pointer chain, which is intact
because the whole kernel is built with `-fno-omit-frame-pointer`. What was
left was to take a sample and count them up.

A sample is a `Record`: the pid, and up to 8 frames. Frame 0 is the
interrupted instruction taken **exactly** from the `Context` the interrupt
stub pushed — not a walk from inside the handler, which would have to guess
how many of its own frames to skip. The rest come from the frame-pointer
walk starting at that context's frame pointer and stack pointer.

Nothing is resolved at sample time: a symbol lookup has no business in an
interrupt handler, and addresses keep just as well.

Each CPU writes only its own buffer, from its own interrupt, with interrupts
already off — so there is no lock anywhere on this path and nothing to
contend for. An NMI cannot be nested by another NMI on the same CPU, so that
holds for the counter path too. Buffers hold 1024 samples per CPU (one
second at the counter rate, ten at the tick rate) and are allocated only for
the CPUs that are actually running. They are kept for the life of the kernel
rather than freed on stop: a CPU can be inside `Sample()` having already
passed the active check when another CPU stops the run, and freeing
underneath it would be a use-after-free for the sake of reclaiming a
megabyte.

## Two sample sources

**The performance counter.** Where the hardware has a usable one, it is
programmed to overflow into an NMI about every millisecond — ten times the
tick rate, and, because it arrives as an NMI, it also samples code running
with interrupts disabled, which a tick by construction never catches.

Two entirely different interfaces count the same quantity:

- **Intel** — architectural perfmon version 2 or later, fixed counter 1,
  unhalted core cycles. Present on every part that has architectural
  perfmon at all, including the hybrid ones where the two core types agree
  about nothing else. Overflow is reported exactly by a global status MSR.
  `IA32_DEBUGCTL.FREEZE_PERFMON_ON_PMI` is cleared first — set, the counters
  stop dead on the first overflow, and firmware and hypervisors do set it.
- **AMD** — no fixed counters, so general counter 0 is programmed with event
  `PMCx076`, "CPU Clocks not Halted": the same quantity, under the one event
  number that has not moved across AMD families. Only the six-counter core
  extension at `0xC0010200` is touched, never the legacy MSRs at
  `0xC0010000` — no CPUID bit says the legacy ones exist, so a guest whose
  host has turned the virtual PMU off takes a `#GP` (a panic here) from the
  first write. On parts before Zen 4 there is no global status register at
  all: the counter is armed below zero and the overflow is recognised by its
  sign bit going clear. Zen 4 and later add PerfMonV2, whose global control
  and status work like Intel's and are used when present.

**The tick.** Everywhere else — including arm64, where PMUv3 exists on the
hardware but is not wired up — the per-CPU timer tick takes the sample, at
100 Hz per CPU. Enough to find a hot function, not enough to show its shape.

`Report()` names the source it used, because reading the numbers depends on
it.

## CPUID is not trusted on its own

A hypervisor can answer CPUID for a performance monitor it does not
virtualise and then swallow every MSR write to it. Nothing in CPUID tells
that machine apart from one where the counters are real, and a profiler that
believed it would arm the counter on every CPU and report an empty profile
with no hint as to why.

So after CPUID says yes, the counter is asked to prove it counts: enabled
without the interrupt bit, read in a loop until it moves, bounded by the
cycle counter at about ten microseconds. The bound is a duration rather than
a count of `PAUSE`s on purpose — `PAUSE` is ten cycles on one part and a
hundred and forty on the next, so a fixed number of them is a duration
nobody can state, and this is time spent with interrupts off. A live counter
has already moved by the first check; only a counter that is not really
there spends the whole budget.

The answer is cached and published behind a barrier, because the probe runs
on the CPU that starts the profiling run and every other CPU then arms its
own counter on the strength of it. The probe also runs with interrupts off
and preemption implicitly pinned, so the two reads bracketing the spin come
off one CPU's counter rather than two.

`lscpu` reports the outcome without running a profile: what the part
claims, and which counter `profile` would actually use — or `tick only`.

## Arming, and stopping

`Profiler::Start()` is called from the shell task; it allocates, zeroes the
counters, asks once whether there is a PMU, and sets the active flag. Each
CPU then arms **its own** counter on its next tick — the control registers
are per CPU and cannot be written from anywhere else. Once armed, that CPU's
tick stops sampling, so nothing is counted twice.

`Stop()` just clears the flag. Each CPU disarms its own counter on its next
tick; until then it keeps overflowing and `Sample()` drops what arrives. An
overflow in flight when the counter is disarmed is still this profiler's, so
the "armed" flag is set *before* the counter and cleared *after* it —
load-bearing on AMD without PerfMonV2, where the overflow is recognised by
the sign bit of a counter that keeps whatever value it stopped at, and where
a genuine NMI minutes later would otherwise be read as a sample and
swallowed instead of panicking.

## The report

Samples are folded by their **whole call chain**, not by their leaf. The
leaf folds by symbol and the callers fold by exact address, and the two
differ for a reason:

- A sample can land on any instruction of the function it interrupted, so
  folding the leaf by address would shatter one hot function into a chain
  per instruction. It folds by name instead and carries the span of offsets
  seen — which says whether the samples sat on one instruction or spread
  across the body. A hot spinlock, for instance, says whether it sat on the
  exchange or in the pause loop.
- A return address is not like that: each call site has exactly one, so
  folding callers by address separates paths that really are different and
  keeps the offset that names which call led here.

```
> profile 2000 all 3
profiled 2000 ms
4102 samples on 4 cpus, 0 dropped, source pmu/nmi intel fixed-ctr1
31.2% 1280 MemCpy+0x1a..0x3e
        <- NetFrame::Copy+0x44
        <- VirtioNet::Receive+0x1b2
...
```

The leaf line is percent, sample count, symbol and offset (or an offset
span). Each `<-` line is a return address — the same ones the panic
backtrace prints, so the two read alike, and the offset points just past the
call that led there.

Limits are reported rather than hidden: the header carries the total, the
number of CPUs that contributed, and how many samples were dropped because a
per-CPU buffer filled; a line is printed if more distinct chains were seen
than the 128 the table holds.

Two empty-report cases are worth knowing:

- **On the counter**, an idle CPU contributes nothing at all — the counter
  counts *unhalted* cycles. A machine that spent the window in `hlt`
  produces no samples, where the tick would have filled the profile with
  `Hlt`. The report says so.
- **With a pid filter**, no samples means that task may simply have been
  asleep the whole window, which is what a task that is not running looks
  like.

`netload` exists partly for this: it gives `profile` something to look at
other than an idle machine.

## Cost

While it is running: one atomic read per tick per CPU to check the flag,
plus a sample — a stack walk of at most 8 frames into a preallocated
buffer — at 100 Hz or ~1 kHz per CPU. No locks, no allocation, no symbol
lookup. `Report` allocates its chain table on the heap (it runs in task
context, where that is fine) rather than on the stack, where a table this
size would be a third of the headroom `stacks` measures.
