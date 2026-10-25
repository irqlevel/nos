# Scheduler

Preemptive round-robin over per-CPU run queues, with no priorities and no
user space. Everything in this kernel that is not an interrupt handler runs
as a `Task`: the shell, the softirq workers, the USB poll loop, the DHCP
client, and each CPU's idle loop. The code is `kernel/sched.cpp`,
`kernel/task.cpp` and `kernel/preempt.cpp`, with the context switch itself in
`arch/x86_64/asm.asm` and `arch/arm64/asm.S`.

## The model

One `TaskQueue` per CPU, owned by that CPU's `Cpu` object, under one
spinlock. It is home to every task that runs on that CPU, and keeps them in
the places a decision needs, so that a decision is never a walk:

- **The run list** -- the tasks that can run, but for the one on the CPU,
  in the order of their turns. Only those: a task that blocks is off it
  until it is woken, so the next to run is the list's head.
- **The sleep list** -- the tasks asleep until a deadline (`Sleep()`,
  `Event::WaitFor()`), soonest first: what the CPU's timer is armed for, and
  what each decision looks at the head of.
- **The idle task's place** -- apart from the run list, taken only when
  nothing else can run. It used to sit in the list as an equal, which meant
  a reschedule -- and every softirq raised from an interrupt handler forces
  one -- could pick it over the task that had just been given work. It then
  halted the CPU, and the work waited for whatever interrupt came next.
- **The wake stack** -- the tasks woken since the last decision, pushed by
  their wakers from any CPU and any context without a lock (below).
- And a list of every task the queue is home to, asleep or not, which only
  its bookkeeping walks.

`Schedule()` takes the wake stack and puts what was on it on the run list,
puts the sleepers whose time has come there too, and switches to the run
list's head; the task it displaces, if it can still run, goes to the tail,
which is what makes the turns a round robin. With nothing on the run list
the CPU keeps a task that can still run when an interrupt asked
(`Preempt()`) or it is polling (`YieldToRunnable()`), and otherwise goes to
the idle task -- for a voluntary `Schedule()`, and for a task that has just
blocked.

Before, the queue was one list of every task, blocked and asleep ones
included, and each decision walked it from the head, past every task that
could not run, the idle task and any task whose preemption count was up --
and once more after the timer fired, for the soonest sleeper. Both walks
are gone: what a decision touches is the head of two lists and whatever was
woken since the last one.

Tasks move between queues only when the load balancer says so, and only
while they can run.

## The context switch

`TaskQueue::Switch` marks the outgoing task `StateWaiting`, charges it the
time it ran, marks the incoming one `StateRunning`, records
`next->Prev = curr`, and calls:

```
SwitchContext(next->Rsp, &curr->Rsp, &TaskQueue::SwitchComplete, next)
```

`SwitchContext` pushes the callee-saved state (x86: `PushAll` — all GPRs and
the flags; arm64: x19–x30 and DAIF), stores the old stack pointer through
`currSp`, loads the new one, and **calls the completion callback on the new
stack** before popping the incoming task's registers.

That callback is the whole trick. The queue lock, the outgoing task's lock
and the incoming task's lock were all taken by the *previous* task, on the
*previous* stack; they are released by `SwitchComplete` running as the
*incoming* task. It then does one of two things with the task it just
displaced:

- Still runnable: ask `SelectNextTaskQueue()` whether it belongs on a
  lighter CPU, and drop its preempt-disable count. Moved, it comes off this
  queue first, has its count dropped while it is on none, and only then goes
  onto the other: no decision ever finds a task on a run list whose count
  is up. It used to go onto the other first, and every walk had to pass over
  a task whose count was up; now that would be a bug, and is a `BugOn`.
  Asleep, it stays: its wakeups and its deadline are this queue's.
- Exited: put it on the CPU's `ExitedList` instead of releasing it. The
  final `Put()` frees a 32 KiB stack, which triggers a blocking cross-CPU
  TLB shootdown, and this code runs with interrupts off — two CPUs freeing
  exited stacks here concurrently would deadlock on each other's shootdown
  IPI. `ReapExited()` does it later from the idle task and the softirq task,
  both of which run with interrupts enabled.

A brand-new task has no saved frame, so `Hal::BuildTaskFrame` fabricates one
that looks exactly like a frame `SwitchContext` would have pushed, with the
entry point where the return address goes (x86) or in `x30` via a thunk
(arm64). The first switch into a task is therefore indistinguishable from
any other.

## Preemption

Two independent gates:

- **Global.** `PreemptOn()` is called once by the BSP, late in boot, right
  before the `Preempt is now on` marker. Until then `Schedule()` returns
  immediately, which is what lets the whole bring-up run without a scheduler.
  APs spin in `PreemptOnWait()` until they see it.
- **Per task.** `Task::PreemptDisableCounter`. `Schedule()` increments it and
  bails out if it was already non-zero, so a task inside `PreemptDisable()`
  keeps the CPU; and no *other* task with a non-zero count is on a run list
  when a decision is made (`SwitchComplete`, above), since it could not be
  switched to safely either. A
  reschedule that bails out is not forgotten: it leaves `Task::PreemptPending`
  set, and the `PreemptEnable()` that brings the count back to zero makes it
  (see [below](#a-reschedule-that-finds-preemption-off)).
  `PreemptIrqSave()`/`PreemptIrqRestore()` pair the two with interrupt
  disabling, stashing "preemption was on" in bit 63 of the saved flags so
  the restore balances correctly even if the global gate moves in between.
- **Spinlocks.** `RawSpinLock::Lock()` — and so every `SpinLock` — disables
  preemption until `Unlock()`, as Linux's `spin_lock` does: a holder is
  never switched away with the lock taken, so no taker spins out a time
  slice waiting for it. The lock remembers *whose* count it raised: the
  scheduler takes its own locks in one task and releases them in the next,
  across `SwitchContext`, and `SwitchComplete` checks that `Schedule()`'s is
  the only count the switched-out task still has. `PreemptCanBlock()` asks
  the whole question — interrupts on and preemption enabled — for code that
  has to know whether it may wait. `RawRwSpinLock` does the same on both
  sides; a read lock has many holders at once, so `ReadLock()` hands back
  whose count it raised and `ReadUnlock()` takes it, the way
  `WriteLockIrqSave()` hands back its flags.

Involuntary preemption comes from the per-CPU timer tick — the local APIC
timer on x86-64, the generic timer on arm64, both at 100 Hz — whose handler
ends in `Preempt()`. The timer is one-shot, armed for the next tick or
sooner for a task asleep until a deadline ([The local
timer](#the-local-timer)); an interrupt for a deadline only reschedules. So does the IPI handler, which is what makes an IPI a
reschedule request. Voluntary preemption is a direct `Schedule()` call.

The two differ in one case: when nothing but the idle task could take the
CPU. `Schedule()` hands it over — its caller asked to let others run, and the
idle task halting the CPU until the next interrupt is what makes `Sleep()`
cheap. `Preempt()` leaves the CPU with the task the interrupt landed on, if
that task can still run. The tick and the IPI handler used to end in
`Schedule()`, and that made stalls: the idle task only halts, and a task
displaced while runnable — not blocked — gets no IPI from whoever has work
for it, so it waited for the next tick. On the AX41 the netblk worker,
polling for requests with `poll=` and never blocking, was switched out that
way by the tick some 25 times a second, a tick each: the idle task took a
quarter of its CPU while it had work, and the server's p99 was 10 ms. An IPI
landing on a running task could do the same to it.

### A reschedule that finds preemption off

The tick or an IPI can land on a task with preemption off -- inside a
spinlock, most often -- where the reschedule cannot be made. It used to be
dropped: the task went on, and whatever the reschedule was for waited for the
next tick or IPI. For the tick's own round-robin that costs nothing; for the
IPI a wakeup sends, the wakeup is lost. `SoftIrq::Raise` in an interrupt
handler unblocks its CPU's softirq task and sends the CPU an IPI to switch to
it. Landing on the idle task inside the lock `ReapExited()` takes -- a few
instructions, with interrupts on -- that IPI's reschedule was dropped, and the
idle task went on to halt with the softirq task runnable. Nothing else woke
it: a raise that finds its bit already up sends no IPI, a device interrupt
never reschedules on its own, and the igb driver keeps its vector masked
until the receive poll re-arms it. The CPU slept on its receive queue until
its next tick. On the AX41 under netblk's load that was the receive path going
quiet for up to 10 ms some 80 times a second -- 13% of round trips over 2 ms,
nearly every silence ending on the same 10 ms grid, one CPU's tick -- while
netblk's own service time stayed under half a millisecond (see
[netblk](netblk.md#the-receive-path-lost-ticks-too)).

A refused reschedule now sets `Task::PreemptPending`, and `PreemptEnable()`
or `PreemptEnableTask()` -- every unlock goes through one of them -- makes it
the moment the count is back to zero with interrupts on: on that idle task,
right after the unlock and before the `hlt`. With interrupts off it waits,
since that is an interrupt handler, whose own `Preempt()` or the next tick
will do, or a section its caller keeps closed, until its next
`PreemptEnable()`. Only the running task's reschedule is made that way
(`SwitchComplete` releases locks for the task it switched away from), and
none once a panic has begun. `Schedule()`, when it keeps the CPU, also drops
its own count before it turns interrupts back on, so an IPI waiting on the
way out finds the task preemptible.

One consequence for code that sends and then waits for an answer: the answer
can be in before the send returns. The receive softirq may run on the way out
of the transmit path, at the transmit lock's unlock -- rare while it took a
tick landing just there, likely now that the reschedule an interrupt asked for
meanwhile is made there. The DHCP client cleared its answer flag after
sending, and over QEMU's igb, which transmits within the doorbell's write,
lost every answer that way; it clears it before the send now
(`DhcpClient::ArmResponse`). Arm first, then send.

`top` prints how many reschedules were deferred in its window and since
boot, and how many of those landed on an idle task; the count since boot
starts at one per CPU, made on purpose on each idle task by the boot
self-test (`TestPreemptDeferred`). Under netblk's load in QEMU (virtio-net, 12
vCPUs), 20 seconds of it put more than a thousand on idle tasks, and on the
AX41 some 200 a second land there -- before, each one a CPU asleep on work
until its next tick or IPI. Made instead of dropped, they took the AX41's
receive path from 397 silences in 5 seconds to none (see
[netblk](netblk.md#the-receive-path-lost-ticks-too) for the rest).

`Sleep(ns)` blocks. The task sets its deadline (`Task::SleepUntil`) and
`Block()`s, interrupts off as below, and nobody wakes it: the `Schedule()`
that switches it out puts it on its queue's sleep list, soonest first, and
every scheduling point the CPU has looks at that list's head and puts the
sleepers whose time has come back on the run list (`TaskQueue::WakeDue`).
The CPU's timer makes one at the deadline
([The local timer](#the-local-timer)), so a sleep ends there. It used to end
at the first tick after its deadline -- a sleep of a millisecond took ten --
which is where it ended when `Sleep` was a loop around `Schedule()` that
checked the clock each time it got the CPU back. That loop kept the task
runnable: alone on its CPU it let the idle task halt it until the tick, but
two sleepers on one CPU handed it to each other without end and the idle task
never ran. Two idle hypervisor guests on one CPU of the AX41, each vCPU asleep
until its guest's next timer edge, kept that CPU busy between them, 46% each;
a `usb` poll task and an ssh session's `top` did the same. The self-test
`TestSleepBlocks` puts two sleepers on one CPU at every boot and fails it if
they run more than a tenth of what they sleep -- a poller runs close to half.
Where a task cannot block -- before preemption is on, off a task's stack, in
a CPU's idle task, with preemption disabled -- `Sleep` still polls.

## Blocking and waking

`Block()` and `Unblock()` take no lock; `Block()` is called by the task
itself, `Unblock()` from any CPU including hard IRQ context. Each is a
compare-and-swap on one word, `Task::RunState`, whose values say where the
task is with its queue:

| State | Means | Made by |
|---|---|---|
| Queued | can run: on the run list, or on its CPU | the task's creation; a waker or the deadline from Blocking or Sleeping; its CPU from Waking |
| Blocking | has called `Block()`, still on its CPU | the task itself, from Queued |
| Sleeping | off its CPU, on no list but the sleep list if it has a deadline | its CPU's `Schedule()`, from Blocking, as it switches it out |
| Waking | an `Unblock()` found it Sleeping and is handing it back | that `Unblock()`, from Sleeping |

`Unblock()` that finds a task Blocking makes it Queued, and the `Schedule()`
that was about to take it off finds it so and keeps it runnable. One that
finds it Sleeping makes it Waking -- having won it from any other waker and
from its deadline, since each of those is a compare-and-swap from Sleeping
too and exactly one succeeds -- and pushes it onto its queue's wake stack:
a Treiber stack that is pushed onto from any context and only ever taken
whole, by its CPU's next `Schedule()` (`TaskQueue::PushWake`,
`DrainWakes`), so it has no pop and no ABA. That `Schedule()` is the waker's
to bring about, as it always was: `Event::Signal` and `SoftIrq::Raise` send
the CPU an IPI. A task asleep is never moved to another queue, so the queue
an `Unblock()` reads is the one the task sleeps on.

The ordering is what makes it safe, and both sides do the mirror image of
each other:

- The sleeper raises its state **first**, then re-checks whether there is
  work, then calls `Schedule()`.
- The waker publishes the work **first** (sets the pending bit, or clears
  the "running" bit), then looks at the state -- with a locked operation
  whatever it finds, a failed compare-and-swap included.

A wakeup landing anywhere in that window is either seen by the re-check or
finds the task Blocking or Sleeping and puts it back. Every step is
sequentially consistent (`lock cmpxchg` on x86, a sequentially consistent
compare-and-swap on arm64); a plain read of the state on the waker's side
would let it be made before the work it published was seen, which holds on
x86 and fails on arm64 only.

The sleeper's side runs with **interrupts off**, from the state going up
until it is down again, its own `Schedule()` included. The tick and every
IPI end in a reschedule of their own, and one landing while it is up would
switch the task out as a sleeping one -- asleep on work it never saw, or had
just seen and not yet put the state down for, while a waker that found the
work already published may not look again. `Schedule()` saves and restores
interrupts per task, so a task that blocks with them off comes back with
them off, and turns them back on itself.

A wakeup that is lost is a task asleep for good, which no panic reports.
The boot self-test `TestWakeStorm` makes them in every window the state has:
a ring of tasks, two a CPU, each pinned to its own, passing one token round
-- every pass a wakeup on another CPU, half of the waits with a deadline and
half without, a sleep now and then so that a deadline races the signal that
comes for it, and a task yielding on every other CPU so that the run lists
are not empty. It fails the boot when the token stops for two seconds,
naming the task it was sent to and where that task is -- "woken, and not yet
back on its cpu's run list" for a wakeup its CPU never took. 3000 passes
take some 70 ms under HVF on four vCPUs. A control run that dropped one
push in 1500 stopped the token at pass 1633 and failed the boot.

The idle task only halts (`Cpu::Idle`), so a task that *yields* on a CPU with
nothing else to run gives that CPU to it: the CPU sleeps until the next
interrupt it takes itself -- the tick, when the device's interrupt is routed
elsewhere -- and a waker that finds the task merely runnable sends no IPI.
`Schedule()` is a way to let others run, not a way to wait: wait by blocking,
or poll with `YieldToRunnable()`, which gives the CPU to another runnable task
and never to the idle task, returning at once when there is none. A plain spin
is no better than the yield: it keeps whatever else is runnable on the CPU off
it until the tick -- a softirq task the tick preempted mid-handler included,
with its type's work held up on every CPU meanwhile. `Sleep()` is not for
waiting on work either: it ends at its deadline, which the work may come long
before; block on the work, with `Event::WaitFor` when there is a deadline as
well.

`SoftIrq::Run` is the worked example, and it also shows the one case where
"the task is already running" is true and useless: between `Block()` and the
`Schedule()` that follows it, a task is Blocking while still being the
current task, so a raise landing there must kick anyway. Its
`TickKick` is the belt and braces — one atomic read per tick turns a lost
wakeup from a wedged CPU into at most a tick of latency. (Before its window
had interrupts off, that tick was paid a few times in every thousand network
round trips.)

`Event` (`kernel/event.h`) packages the pattern for any task with something
to wait for: one waiter, `Signal()` from anywhere, a hard IRQ handler
included, and an IPI to the waiter's CPU when it finds the waiter blocked.
`WaitFor(ns)` is the same wait with `Sleep()`'s deadline beside it -- the
task blocked, woken by a `Signal()` at once or by the scheduler when its time
has come -- and says which: how a halted guest's vCPU waits for its timer's
next edge or a frame, whichever comes first.
The netblk module's worker sleeps on one. A CPU's idle task must never block,
on an event or otherwise: it is the scheduler's last resort, and a task
exiting on a CPU whose idle task is blocked has nothing to switch to.

## The local timer

Each CPU's timer -- the local APIC's on x86-64, the generic timer's CNTV on
arm64 -- is one-shot (`Cpu::LocalTimer*`, `hal/timer.h`). It is armed for
the earlier of two things: the CPU's next tick, on a 100 Hz grid that the
tick keeps however the interrupts in between fall, and the soonest deadline
of a task asleep on the CPU until one -- `Sleep()`, `Event::WaitFor()`. An
interrupt at the tick does the tick's work, as the periodic timer's did; one
before it only reschedules, and the decision that follows puts the sleeper
whose time has come back on the run list.

Who arms it for what:

- Every `Schedule()` brings its CPU's timer forward to the head of the
  queue's sleep list -- the soonest deadline, a task going to sleep in that
  very `Schedule()` included -- with interrupts off and the queue's lock
  held: a write of the local APIC's initial count, or of CNTV's compare
  value, when the deadline is sooner than what the timer has, and nothing
  otherwise.
- The interrupt re-arms it for the tick only, and leaves a flag; the next
  `Schedule()` on the CPU -- the `Preempt()` at the end of the same handler,
  as a rule -- brings it forward again, and the flag is what has the queue
  counted for `sched` and looked at for a task left waiting too long. That
  is under the queue lock `Schedule()` holds anyway, so the interrupt takes
  no lock of its own -- one taken there would spin, interrupts off, on a CPU
  whose queue another holds, and a stuck one would stop that CPU's tick and
  with it the watchdog that reports it.
- A task asleep is not moved to another CPU at its switch-out
  (`SwitchComplete`): only its own CPU's timer is armed for it, and its
  wakeups go to its queue. It moves, if it should, once it runs.
- A panic re-arms the tick and nothing else: the queue's lock may be the
  panicking CPU's.

So every task asleep on a CPU has that CPU's timer armed at or before its
deadline, but in the moment between an interrupt and the decision after it
-- which is what `Preempt()` makes, or the `PreemptEnable()` that makes a
reschedule the interrupt found deferred. An interrupt for a deadline that a
`Signal()` already answered finds nothing to run and costs a reschedule.

`TestSleepPrompt`, at every boot on every CPU, has a task of its own sleep a
millisecond at a time while the CPU's idle task halts between interrupts --
not polling the clock, which would wake the sleeper on time whatever the
timer did -- and fails the boot if the median sleep is over 5 ms. Measured:
1.007-1.07 ms under HVF, 1.1-2.4 ms under TCG; with the arming taken out,
10.0 ms on both architectures. The median, because a TCG host that stalls a
vCPU now and then puts a sleep or two at 15-50 ms. On the EX44 a guest's
`usleep 1000`, its vCPU halted between, took 10 ms before.

Everything that polls with a short sleep now polls at the rate it asked for:
the USB poll at 4 ms instead of the tick's 10, netconsole's idle poll at 2,
an ARP wait at 1 -- shorter waits, and more wakeups. TCP's blocking calls --
connect, accept, send and recv, which nothing signals -- polled at 1 ms
too, and on the EX44 an idle nos with sshd up and two sessions open woke
2,220 times a second, 1.2% busy over its 20 CPUs; they back off now, 1 ms
doubling to 10 (`Poll` in `net/src/tcp.rs`), and the same nos woke 741 times
a second, 0.4% busy, most of it the USB poll.

## Load balancing

`Task::SelectNextTaskQueue()` returns the queue this task should be on: the
one its affinity mask allows with the fewest tasks that can run there -- the
one on the CPU included, the idle task not (`GetRunnableCount`) -- and of
two as busy, the one home to fewer tasks; or `nullptr` for "stay put". It
runs at task creation and on every context switch for the task being
displaced, if that task can still run, and it reads the lock-free mirror of
the running-CPU set rather than taking the CPU table lock, because it is on
the switch path.

It used to count every task a queue was home to, asleep or not, so a CPU
home to a dozen sleepers looked busier than one with two tasks spinning,
and tasks were sent where they had to wait. Tasks asleep cost a CPU nothing;
what could run there is what a task placed there competes with. They still
break a tie, so that new tasks -- most of which sleep -- spread out rather
than all go to the first CPU. A task that blocks is no longer weighed at its
switch-out: it stays on the queue its wakeups go to, and is weighed the next
time it is displaced while it can run.

Two rules keep it from thrashing:

- The queue the task is already on competes like any other. It used to be
  skipped, which meant every task moved to another CPU on every context
  switch — not balancing, just motion, and it threw away whatever the task
  had warm in that core's caches.
- A move needs a difference of **two** (`MigrateThreshold`). The task can
  run and is still counted where it is, so moving it takes one off that side
  and adds one to the other; a difference of one would be reversed on the
  next switch and the task would trade places forever.

### A task left waiting

A task on a run list longer than `TaskQueue::StallReportNs` -- five seconds
-- is reported, once a wait: `sched: <name> (pid N) could run, and has
waited M ms for cpu C, with K that can run there`. The run list is in the
order of its turns, so its head has waited longest, and the look is one
comparison each time the CPU's timer has the queue looked at. The report is
noted under the queue's lock and said once the CPU holds none of the
scheduler's locks: the trace ends in the console and, with `disklog=on`, in
a disk write. A round robin cannot starve a task; a scheduler with
priorities can, and this is what says so.

Affinity is a `ulong` bitmask, which is one of the reasons `MaxCpus` is 64.
Idle tasks and softirq tasks are pinned to their own CPU; everything else
runs anywhere. `top` prints the number of migrations since boot, which is
the only way to know whether "rare" is actually true.

## Tasks and stacks

A task's stack is 32 KiB and carries its own identity: a `Task*` and a magic
word at the base, a second magic word at the top. `Task::GetCurrentTask()`
rounds the stack pointer down to a 32 KiB boundary and reads the pointer
from there — no per-CPU variable, no register convention. It works because
the page allocator's 8-page bucket hands out blocks aligned to their own
size. The bottom page is a tripwire: a stack pointer inside it is a
`BugOn`, not a silent corruption. There is deliberately no `Trace()` in
`GetCurrentTask` — tracing takes a lock, which calls `PreemptDisable`, which
calls `GetCurrentTask`. Every lock asks that way (`PreemptCurrentTask`) until a
panic has begun; from then on a stack that is no task's -- x86's double
fault's, a static one -- has nothing to count, since the panic switches to no
task again, and the `BugOn` there would be a panic inside the panic that only
spins, the report stopped at its first lock.

Stacks are filled with a pattern before first use, so `stacks` can report
each task's high-water mark: what is still intact is what the task never
reached. That catches a spike that lasted microseconds during boot and costs
nothing while the machine runs.

Tasks are refcounted (`Get`/`Put`), registered in a 512-bucket `TaskTable`
hashed by pointer, and given a pid by an `ObjectTable`. A task ends by
returning from its function: `ExecCallback` calls `Exit()`, which stamps the
exit time, removes the task from the table, sets `StateExited` and calls
`Schedule()` — which never returns.

### Waiting for one to end

`Task::Wait()` is the join every stop path makes -- a Rust `TaskHandle`
dropped or waited on among them: `hv stop` waiting for a guest's task, the
guest's disk waiting for its own, `rmmod`, the host's shutdown. It never
gives up, because a task that has not exited is still using whatever its
waiter is about to free. It does not wait in silence either: ten seconds in,
and again each time the wait has doubled (five minutes apart at most), it
reports to the kernel log who waits, for which task, for how long, and where
that task is, its frames named --

```
join: shell (pid 12) has waited 10000 ms for hv/vm3/vda (pid 57) to exit, and it is blocked
join:   [0] 0xFFFF80000105AF99 Kernel::TaskQueue::Schedule+0x449
join:   [1] 0xFFFF80000105B594 Kernel::ScheduleCurrent+0x154
...
```

-- which is what the TLB shootdown's naming of the CPUs that never answered
is for a CPU, for a task. Where a task is comes from `Task::Locate`, which
`bt` uses too:

- **Off its CPU** -- blocked, asleep, or runnable and waiting for its turn --
  it is read off its own stack: the frame pointer its switch saved, the
  chain walked inside the stack's bounds (`Hal::TaskSavedFramePointer`
  checks the saved frame lies inside them too, since the saved stack pointer
  is read with nothing holding the task still). The read is a seqlock's:
  kept only if the task's switch count and state show it did not run while
  it was read, and looked at again if it did. A lost race names wrong frames
  and is thrown away; it cannot fault, because the stack stays mapped while
  the asker holds its reference.
- **On a CPU**, it is asked about: an IPI to every other CPU, whose handler
  checks whether the task it landed on is the one wanted and, if so, records
  where it was -- the interrupted instruction and the frames above it.
- **On a CPU that does not answer** -- interrupts off, which is usually
  where such a task is -- the CPU is named after a second instead: "on a cpu
  that would not answer: cpus 0x20 took no interrupt in 1000 ms". What the
  IPI writes into is one slot per CPU, made the first time that CPU is asked
  and kept for good, never the asker's stack: a CPU that takes the IPI late
  would otherwise write into a stack long since reused. A slot whose IPI is
  still unanswered is not sent again, and the CPU is named again; one
  answered late is reused.

The boot self-test `TestTaskLocate` holds each case to frames that name the
function the task is in -- a CPU kept deaf for a tenth of a second included
-- and has a join reported.

## The idle task

Each CPU's idle task is created by `Cpu::Run()`, pinned to that CPU and
marked with `FlagIdleBit`. Its body is the CPU's bring-up function
(`ApStartup` / `BpStartup`), which ends in a loop over `Cpu::Idle()`:
reap any tasks that exited on this CPU, then `hlt` (x86) or `wfi` (arm64).
The BSP's idle task additionally watches for a shutdown or reboot request
from the shell.

It is the scheduler's last resort only while it idles: inside `Cpu::Idle()`,
about to halt or halted (`Task::SetIdling`), an interrupt that finds
something else to run switches to it and leaves the idle task in its place,
to run again only when nothing else can. Running anything else -- the boot
and its self-tests, which are what the bring-up function runs before its
loop, or the shutdown -- it is a task like any other, displaced to the run
list's tail and taking its turn. It used to be the last resort whatever it
ran, and a task the boot started on the boot CPU that never blocked kept the
boot itself off that CPU for as long as it ran: the ring selftest's producer,
pinned to the boot CPU and polling for a consumer the boot had not started
yet, took the CPU at the next tick, and the boot sat for the thirty seconds
the producer gave the consumer -- one boot in a few hundred under HVF, and
the selftest's failure said two values of a hundred thousand came out. The
old walk had hidden it by an accident of list order: it stopped at the idle
task, and a task inserted behind it was not reached. `TestIdleTakesTurns`
holds the rule at every boot: beside a task pinned to the boot CPU that
only ever yields, a millisecond's sleep in the boot must come back in about
a millisecond -- with the old rule it came back in the two seconds that task
waited.

Because the idle task is the scheduler's last resort while it idles, a CPU
with something always runnable may not reach `ReapExited` for a
while — which is why the softirq task calls it too. The same starvation
used to swallow `poweroff` and `reboot` outright: `Sleep()` yielded rather
than blocked then, so a CPU carrying the shell, DHCP and USB poll tasks never
ran its idle task again, and the request sat unseen (on a two-CPU QEMU, every
time). `Cmd::RequestShutdown()`/`RequestReboot()` now clear the BSP idle
task's `FlagIdleBit`, making it an ordinary task for what remains of its
life: it takes its turn, sees the flag, unmounts the filesystems while the
soft IRQs still run, and halts.

## What it is measured by

Each queue counts what its CPU's scheduler does, as it does it
(`SchedStats` in `kernel/sched.h`), and `sched [ms]` prints a window of it:

- **A wakeup's wait**: from the moment a task could run again until it ran.
  For a task unblocked by someone, that moment is the `Unblock()` that took
  it from Sleeping -- stamped there (`Task::WakeTime`), from whatever CPU and
  context the waker is in, and only by the call that won it. For one asleep
  until a deadline, it is the deadline (`TaskQueue::WakeDue` stamps it). A
  stamp from before the task last left its CPU is not this wait's -- the
  wakeup before -- and is not counted.
- **A delay**: a task displaced while it could still run -- by the tick, or
  an IPI -- waits until it runs again. Kept apart from wakeups: it is the
  round robin's own turn-taking, where a wakeup's wait is latency someone is
  waiting on.
- **A decision**: what `TaskQueue::Schedule` cost from the moment it turned
  interrupts off until it switched or returned -- the queue lock's wait
  included, the switch not -- in cycles, and how many tasks it touched: one
  for the head it took, and one for each task it put back on the run list,
  woken or due. When the decision walked the queue, this column was how far
  the walk went.
- **The queue at the timer**: each time the CPU's timer has the queue looked
  at, how many tasks there could run, the one on the CPU included and the
  idle task not -- and whether two or more could, which is where a woken
  task has to wait its turn behind another.

Waits go into log2 buckets of microseconds, so a percentile is a bucket's
bound ("<256us"), and the longest since boot is kept exactly. The counters
are the owning CPU's to write, under its queue lock, with no locked
instruction; `sched` reads them from another CPU a word at a time, so a
reading is a moment's and not a consistent whole. The idle task's turns are
nobody's wait and are not counted.

Two things a reading says about the machine rather than the scheduler.
Under a hypervisor a wakeup that has to bring a halted vCPU back -- an IPI
to a CPU in its idle task's `hlt` or `wfi` -- waits for the host to run that
vCPU again, which is part of the number. And the boot's own
`TestLocateOnCpu` keeps the highest CPU deaf for a tenth of a second on
purpose, so that CPU's longest wait since boot is about that.

### A woken task behind spinners

`sched probe` makes the case a scheduler is chosen by and measures it:
spinners pinned to one CPU that never yield, a waiter pinned beside them
asleep on an `Event`, and the shell's task on another CPU signalling it a
sample at a time. With the round robin above, under HVF on four vCPUs:

| Spinners | Waited, p50 | Their shares of the CPU, per mille |
|---|---|---|
| 0 | <32us | -- |
| 1 | <8us | 995 |
| 2 | <8.1ms | 248, 743 |
| 4 | <32.7ms | 82, 247, 331, 331 |

A woken task waits behind every task that is ahead of it in the list, a
tick each -- the IPI that comes for it switches the CPU to whichever runnable
task is first, which is not the one woken -- and those early switches make
the turns unequal too.

## Looking at it from the shell

| Command | Shows |
|---|---|
| `ps` | Every task: pid, state, flags, accumulated runtime, context switches, name |
| `top [ms]` | Per-task CPU use over a window, as a percentage per CPU (a busy thread reads 100%, a 20-CPU box tops out at 2000%), plus the migration count and the deferred reschedules |
| `sched [ms]` | Per CPU over a window: wakeups and how long they waited for the CPU, displaced tasks' delays, decisions and their cost, the walk's length, and how many tasks could run at the timer ([above](#what-it-is-measured-by)) |
| `stacks` | High-water mark of every task stack and every static boot stack |
| `bt <pid>` | Where a task is and its stack trace (`Task::Locate`, [above](#waiting-for-one-to-end)): read off its stack when it is off its CPU, caught by an IPI on the CPU running it, or the CPUs that would not answer one named |
| `profile` | Where the time actually goes — see [Profiler](profiler.md) |
