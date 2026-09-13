# Scheduler

Preemptive round-robin over per-CPU run queues, with no priorities and no
user space. Everything in this kernel that is not an interrupt handler runs
as a `Task`: the shell, the softirq workers, the USB poll loop, the DHCP
client, and each CPU's idle loop. The code is `kernel/sched.cpp`,
`kernel/task.cpp` and `kernel/preempt.cpp`, with the context switch itself in
`arch/x86_64/asm.asm` and `arch/arm64/asm.S`.

## The model

One `TaskQueue` per CPU, owned by that CPU's `Cpu` object. A queue is a
linked list plus a spinlock; `Schedule()` rotates it. Tasks move between
queues only when the load balancer says so.

`SelectNext` walks the list from the head and takes the first candidate that
is not the current task, not preemption-disabled, not blocked and not the
idle task:

- **Blocked** tasks are skipped outright. A task blocks itself immediately
  before the `Schedule()` that switches it out, and whoever has work for it
  clears the flag; a blocked task has nothing to run, so it is not even kept
  as a fallback.
- **The idle task** is remembered as a last resort and taken only if nothing
  else can run. It used to sit in the list as an equal, which meant a
  reschedule — and every softirq raised from an interrupt handler forces one
  — could pick it over the task that had just been given work. It then
  halted the CPU, and the work waited for whatever interrupt came next.
- If the walk reaches the current task before finding a candidate, nothing
  ahead of it was runnable: it is rotated to the tail and the walk stops.
  The CPU keeps it — unless the idle task was passed on the way, in which
  case that is what runs, for a voluntary `Schedule()`. The reschedule an
  interrupt asks for, `Preempt()`, keeps a task that can still run over the
  idle task (below).

The chosen task and the current one are both moved to the tail, which is
what makes the rotation round-robin.

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
  lighter CPU, move it if so, and drop its preempt-disable count.
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
  keeps the CPU; `SelectNext` also passes over any *other* task with a
  non-zero count, since it cannot be switched to safely either. A
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
ends in `Preempt()`. So does the IPI handler, which is what makes an IPI a
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

`top` prints how many reschedules were deferred since boot and how many of
those landed on an idle task. Under netblk's load in QEMU (virtio-net, 12
vCPUs), 20 seconds of it put more than a thousand on idle tasks, and on the
AX41 some 200 a second land there -- before, each one a CPU asleep on work
until its next tick or IPI. Made instead of dropped, they took the AX41's
receive path from 397 silences in 5 seconds to none (see
[netblk](netblk.md#the-receive-path-lost-ticks-too) for the rest).

`Sleep(ns)` is worth knowing about: it *spins* on `GetBootTime()` calling
`Schedule()`, it does not block. It yields the CPU but keeps the task
runnable, so a task sleeping in a loop is still on the queue and still gets
turns. Code that wants to be out of the way until woken uses
`Block()`/`Unblock()` instead.

## Blocking and waking

`Block()` and `Unblock()` are a single atomic bit each and take no lock;
`Block()` is called by the task itself, `Unblock()` from any CPU including
hard IRQ context. The ordering is what makes it safe, and both sides do the
mirror image of each other:

- The sleeper sets the flag **first**, then re-checks whether there is work,
  then calls `Schedule()`.
- The waker publishes the work **first** (sets the pending bit, or clears
  the "running" bit), then clears the flag.

A wakeup landing anywhere in that window is either seen by the re-check or
clears the flag before `Schedule()` can act on it. Both stores are
sequentially consistent (`lock bts` on x86, `stlr` on arm64); relaxed
ordering would hold on x86 and fail on arm64 only.

The sleeper's side runs with **interrupts off**, from the flag going up
until it is down again, its own `Schedule()` included. The tick and every
IPI end in a reschedule of their own, and one landing while the flag is up
switches the task out as a blocked one -- asleep on work it never saw, or had
just seen and not yet cleared the flag for, while a waker that found the
work already published may not clear it again. `Schedule()` saves and
restores interrupts per task, so a task that blocks with them off comes back
with them off, and turns them back on itself.

The idle task only halts (`Cpu::Idle`), so a task that *yields* on a CPU with
nothing else to run gives that CPU to it: the CPU sleeps until the next
interrupt it takes itself -- the tick, when the device's interrupt is routed
elsewhere -- and a waker that finds the task merely runnable sends no IPI.
`Schedule()` is a way to let others run, not a way to wait: wait by blocking,
or poll with `YieldToRunnable()`, which gives the CPU to another runnable task
and never to the idle task, returning at once when there is none. A plain spin
is no better than the yield: it keeps whatever else is runnable on the CPU off
it until the tick -- a softirq task the tick preempted mid-handler included,
with its type's work held up on every CPU meanwhile. `WaitGroup::Wait` and
`Sleep()` still wait by yielding, which is why a synchronous NVMe read on
arm64 under TCG takes a tick (`blkload nvme0 randread qd=1`, 10 ms).

`SoftIrq::Run` is the worked example, and it also shows the one case where
"the task is already running" is true and useless: between `Block()` and the
`Schedule()` that follows it, a task is out of the scheduler's walk while
still being the current task, so a raise landing there must kick anyway. Its
`TickKick` is the belt and braces — one atomic read per tick turns a lost
wakeup from a wedged CPU into at most a tick of latency. (Before its window
had interrupts off, that tick was paid a few times in every thousand network
round trips.)

`Event` (`kernel/event.h`) packages the pattern for any task with something
to wait for: one waiter, `Signal()` from anywhere, a hard IRQ handler
included, and an IPI to the waiter's CPU when it finds the waiter blocked.
The netblk module's worker sleeps on one. A CPU's idle task must never block,
on an event or otherwise: it is the scheduler's last resort, and a task
exiting on a CPU whose idle task is blocked has nothing to switch to.

## Load balancing

`Task::SelectNextTaskQueue()` returns the queue this task should be on: the
shortest one its affinity mask allows, or `nullptr` for "stay put". It runs
at task creation and on every context switch (for the task being displaced),
and it reads the lock-free mirror of the running-CPU set rather than taking
the CPU table lock, because it is on the switch path.

Two rules keep it from thrashing:

- The queue the task is already on competes like any other. It used to be
  skipped, which meant every task moved to another CPU on every context
  switch — not balancing, just motion, and it threw away whatever the task
  had warm in that core's caches.
- A move needs a difference of **two** (`MigrateThreshold`). The task is
  still counted where it is, so moving it takes one off that side and adds
  one to the other; a difference of one would be reversed on the next switch
  and the task would trade places forever.

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
calls `GetCurrentTask`.

Stacks are filled with a pattern before first use, so `stacks` can report
each task's high-water mark: what is still intact is what the task never
reached. That catches a spike that lasted microseconds during boot and costs
nothing while the machine runs.

Tasks are refcounted (`Get`/`Put`), registered in a 512-bucket `TaskTable`
hashed by pointer, and given a pid by an `ObjectTable`. A task ends by
returning from its function: `ExecCallback` calls `Exit()`, which stamps the
exit time, removes the task from the table, sets `StateExited` and calls
`Schedule()` — which never returns.

## The idle task

Each CPU's idle task is created by `Cpu::Run()`, pinned to that CPU and
marked with `FlagIdleBit`. Its body is the CPU's bring-up function
(`ApStartup` / `BpStartup`), which ends in a loop over `Cpu::Idle()`:
reap any tasks that exited on this CPU, then `hlt` (x86) or `wfi` (arm64).
The BSP's idle task additionally watches for a shutdown or reboot request
from the shell.

Because the idle task is now the scheduler's last resort rather than an
equal, a CPU with something always runnable may not reach `ReapExited` for a
while — which is why the softirq task calls it too. The same starvation
used to swallow `poweroff` and `reboot` outright: `Sleep()` yields rather
than blocks, so a CPU carrying the shell, DHCP and USB poll tasks never ran
its idle task again, and the request sat unseen (on a two-CPU QEMU, every
time). `Cmd::RequestShutdown()`/`RequestReboot()` now clear the BSP idle
task's `FlagIdleBit`, making it an ordinary task for what remains of its
life: it takes its turn, sees the flag, unmounts the filesystems while the
soft IRQs still run, and halts.

## Looking at it from the shell

| Command | Shows |
|---|---|
| `ps` | Every task: pid, state, flags, accumulated runtime, context switches, name |
| `top [ms]` | Per-task CPU use over a window, as a percentage per CPU (a busy thread reads 100%, a 20-CPU box tops out at 2000%), plus the migration count and the deferred reschedules |
| `stacks` | High-water mark of every task stack and every static boot stack |
| `bt <pid>` | Stack trace of a task, using an IPI to capture it if it is running on another CPU |
| `profile` | Where the time actually goes — see [Profiler](profiler.md) |
