---
title: "Who runs next: EEVDF in nos"
description: "nos's scheduler was a round robin that walked every task and kept a woken one waiting a tick behind each busy task. The new one is EEVDF, with weights, slices and lag: the same wakeup now takes microseconds."
date: 2026-10-25
tags: [performance, safety, debugging, testing, fuzzing, x86-64, arm64]
---

There is one situation a scheduler is judged by. A task has just been woken up, say because the disk read it waited for has finished, and its CPU is busy with tasks that never stop to wait. How long until the woken task runs?

Until 25 October the answer in nos was a tick for every busy task ahead of it in line: 8 ms behind two of them, 33 ms behind four. The CPU was not split evenly either. Four tasks that all wanted it got 8, 25, 33 and 33 per cent of it. Now the same wakeup takes microseconds, and tasks share the CPU by their weights: four equal ones get 25 per cent each, to within a tenth of a per cent. The new scheduler is EEVDF, the policy Linux has used for ordinary tasks since version 6.6.

It came in three steps: measure, rebuild the run queue, change the policy. This post goes through all three, the two bugs found on the way, and why the more interesting of them can no longer be written.

## The old scheduler: a round robin over everything

Each CPU had a queue, and the queue had one list: every task that CPU was home to. Tasks that could run, tasks blocked on something, tasks asleep until a deadline and the CPU's idle task all sat in it together. To decide who ran next, the scheduler walked the list from the head, skipped whatever could not run, and took the first task that could. The task it displaced went to the tail. That is a round robin. Here is the walk as it was ([sched.cpp at 8d5df60](https://github.com/irqlevel/nos/blob/8d5df60c3fb5534becfde26129538e9d73a8e21f/src/cpp/kernel/sched.cpp)), with the comments cut:

```cpp
for (auto currEntry = TaskList.Flink; currEntry != &TaskList; currEntry = currEntry->Flink)
{
    Task* cand = CONTAINING_RECORD(currEntry, Task, ListEntry);
    if (cand == curr)
    {
        cand->ListEntry.Remove();
        TaskList.InsertTail(&cand->ListEntry);
        break;
    }
    if (cand->PreemptDisableCounter.Get() != 0)
        continue;
    if (cand->IsBlocked() && !SleepIsOver(cand, &now))
        continue;
    if (cand->IsIdle())
    {
        if (idle == nullptr)
            idle = cand;
        continue;
    }
    next = cand;
    break;
}
```

Four things were wrong with it.

- **The cost grew with every task, not just the runnable ones.** A CPU home to a dozen sleeping tasks walked past all of them at every decision, and after every timer interrupt it walked again, looking for the sleeper whose deadline had come.
- **A woken task waited behind everyone ahead of it.** The waker sends the CPU an IPI, an inter-processor interrupt, so that it reschedules at once. But the reschedule takes the first runnable task in the list, and that is rarely the one just woken. Each task ahead of it then runs until the next tick, and the tick comes every 10 ms.
- **The turns were unequal.** Those early switches cut somebody's turn short every time, and the shares drifted apart.
- **Every task was the same.** There were no priorities and no weights, no way to say that one task should get more of the CPU than another, or get it sooner.

And nothing measured any of it.

## Step 0: measure first

The kernel has a sampling profiler, `profile`, but it cannot see inside the scheduler where it has no NMI to sample with: on arm64, and under QEMU's TCG emulation. Nothing at all counted how long a woken task waited for the CPU, which is the very number a better scheduler is supposed to improve. So the first commit ([7a896aa](https://github.com/irqlevel/nos/commit/7a896aa6556b4a4239571206d7c559e33ce27c81)) added counters to every CPU's queue: how long woken tasks waited, how long displaced ones waited to run again, what a decision cost and how many tasks it touched, and how many tasks could run each time the CPU's timer looked. A wakeup's wait starts at the `Unblock()` that woke the task, which stamps the time, or at a sleeper's deadline, and ends when the task runs. Waits are kept in log2 buckets of microseconds, so a percentile prints as a bucket's bound, such as `<256us`.

Only the queue's own CPU writes its counters, under the queue lock it already holds. The first version made them atomics, and on x86 an atomic store is a `lock xchg`, a locked instruction, five of them in every decision. Plain relaxed stores are enough ([da9c429](https://github.com/irqlevel/nos/commit/da9c429196322be1a7d0ffaa78e1d1afe2f540bd)): there is one writer, and a reader on another CPU sees a value from before an update or after it, never a torn one.

The shell's `sched [ms]` prints a window of these counters, a line per CPU. `sched probe` stages the situation from the top of this post: spinners, tasks pinned to one CPU that never yield; a waiter pinned beside them, asleep on an event; and the shell's own task on another CPU, signalling the waiter one sample at a time and timing how long it takes to run. Here is the round robin under HVF, Apple's hypervisor, on four vCPUs. The EX44, a 20-CPU Intel server, gave the same numbers.

| Spinners | Wakeup's wait, median | Spinners' shares of the CPU, per mille |
|---|---|---|
| 0 | <32 µs | — |
| 1 | <8 µs | 995 |
| 2 | <8.1 ms | 248, 743 |
| 4 | <32.7 ms | 82, 247, 331, 331 |

One spinner costs nothing, because the woken task is the only other one in the list. From two on, every spinner ahead of it costs a tick.

## Step 1: a queue of only the tasks that can run

The second step ([1bd0b8d](https://github.com/irqlevel/nos/commit/1bd0b8d069d63232d70f9c398d8da269d88c860e)) left the policy alone, so the turns were still a round robin, and rebuilt the queue around what a decision needs:

- **the run list** holds only tasks that can run. A task that blocks is off it until it is woken, so a decision never steps over a task that cannot run;
- **the sleep list** holds the tasks asleep until a deadline, soonest first. The CPU's one-shot timer is armed for its head, and each decision looks only at the head;
- **the idle task** has a place of its own, apart from both;
- **the wake stack** holds the tasks woken since the last decision.

Blocking used to be one bit in the task's flags. `Block()` set it, `Unblock()` cleared it, and the task stayed in the list either way, for every walk to step over. Now `Block()` and `Unblock()` are compare-and-swap operations on one word of the task, `RunState`, which says where the task is with its queue:

| State | Means |
|---|---|
| Queued | can run: on the run list, or on its CPU |
| Blocking | has called `Block()` and is still on its CPU |
| Sleeping | switched out by its CPU, on no run list |
| Waking | an `Unblock()` found it Sleeping and is handing it back |

Here is the waker's side ([task.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/task.cpp)):

```cpp
void Task::Unblock()
{
    long was = RunState.Cmpxchg(RunQueued, RunBlocking);
    if (was != RunSleeping)
        return;

    if (RunState.Cmpxchg(RunWaking, RunSleeping) != RunSleeping)
        return;

    WakeTime.Set((long)GetBootTime().GetValue());
    TaskQueue->PushWake(this);
}
```

If the task is still Blocking on its CPU, the first compare-and-swap makes it Queued again, and the `Schedule()` that was about to take it off finds it runnable and keeps it. If it is Sleeping, the second one makes it Waking. Of two wakers, or of a waker and the deadline of a sleeping task, exactly one wins that step, and only the winner pushes the task onto its queue's wake stack.

The wake stack is a lock-free stack that anyone can push onto from any context, interrupt handlers included, and that only its own CPU ever empties, all of it at once, in its next decision. Since nothing is ever popped one element at a time, it has no ABA problem: that is the classic trap of a lock-free stack, where an element is taken off and pushed back between another CPU's read and its compare-and-swap.

There is a subtle point about memory ordering. The waker first publishes the work, setting a pending flag for example, and then looks at the task's state; the sleeper does the mirror image, raising its state first and checking for work after. The waker's look is always a locked operation, even when it finds nothing to do, because a failed compare-and-swap is still one. A plain read there would let arm64 make the look before the published work was visible to the other CPU, and the sleeper could check, find nothing and sleep forever with its work waiting. On x86 the plain read would have been safe. On arm64 it would not.

A decision got a little cheaper and a lot simpler. Measured side by side under HVF at rest, it costs 660 ns against the round robin's 740, and it touches 1.1 tasks on average where the old walk looked at 2.6. On the EX44 it went from 1.5 µs to 1.2–1.3.

### The boot that waited thirty seconds

Each CPU's idle task is also what runs that CPU's bring-up code, and on the boot CPU that means the boot itself, self-tests included. The old scheduler treated the idle task as the last resort whatever it was running. In the new queue that turned into a real bug. The ring self-test starts a producer pinned to the boot CPU, which polls for a consumer the boot has not started yet. The producer took the CPU at the next tick and kept it, and the boot sat until the producer gave up after 30 seconds. Once in a few hundred boots under HVF, the self-test failed, saying that two values out of a hundred thousand had come out.

It was found by booting in a loop until a boot stalled, then attaching lldb to QEMU's GDB stub and asking where every CPU was: the producer running, CPUs 1 to 3 halted, and no consumer on any queue. The old walk had hidden the same flaw by an accident of list order. Now the idle task is the last resort only while it idles, inside `Cpu::Idle()`, and running the boot it takes its turns like any other task. A boot self-test, `TestIdleTakesTurns`, holds this rule: beside a task pinned to the boot CPU that only ever yields, a 1 ms sleep in the boot takes 1 ms; with the old rule it took 2000. Four hundred boots in a row have not stalled since.

### A storm of wakeups

A lost wakeup is the worst kind of scheduler bug: a task asleep forever, and no panic to say so. So another self-test, `TestWakeStorm`, makes wakeups in every window the four states leave. A ring of tasks, two per CPU and each pinned to its own, passes a token round, so every pass is a wakeup on another CPU. Half the waits have a deadline, a task sleeps now and then so that a deadline races the signal coming for it, and on every other CPU a task keeps yielding so that the run lists are never empty. If the token stops for two seconds, the boot fails and names the task the token was sent to and where that task is. A control run that dropped one wakeup in 1500 failed at the 1633rd pass. On the EX44, 3008 passes among 32 tasks on 20 CPUs take 60 ms.

## Step 2: EEVDF

EEVDF stands for Earliest Eligible Virtual Deadline First. Ion Stoica and Hussein Abdel-Wahab described it in 1995, and since Linux 6.6 it decides who runs among Linux's ordinary tasks. In nos it is [eevdf.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/eevdf.cpp), a few hundred lines that know nothing of tasks, locks or CPUs, only entities and the queue they are on ([0ce9d2f](https://github.com/irqlevel/nos/commit/0ce9d2ffc63966bfa03ac821ed391eed5229bfb2)). It works like this:

- Every task has a **weight**, from its nice value, -20 to 19. The table is Linux's: 1024 at nice 0, and each step about 1.25 times the next, so that one nice apart is about ten per cent of the CPU between two tasks that both want all of it.
- Every task has a **virtual runtime**, the CPU time it has had, scaled: a nanosecond on the CPU adds 1024/weight virtual nanoseconds to it, so a heavier task's grows more slowly.
- **V** is the weighted average of the virtual runtimes of all the tasks on the queue: where each of them would be if it had had exactly its share.
- A task's **lag** is V minus its virtual runtime, what it is owed. A task is **eligible** while its lag is not negative.
- Every task asks for a **slice** of the CPU at a time, 3 ms by default, and its **virtual deadline** is where its virtual runtime will be once it has had that slice.

The next to run is the eligible task whose deadline comes first. The two knobs do different things. The weight decides how much of the CPU a task gets. The slice decides how soon after it becomes eligible it gets it: a short slice means an early deadline, which is lower latency, not a larger share.

### Why the woken task now runs at once

The answer to the wakeup case falls out of the lag. A task that goes to sleep takes its lag with it and brings it back when it wakes. The probe's waiter spends its life waiting while spinners run, so it is always owed something. When it wakes it is eligible, its deadline is the soonest on the queue, and the IPI that comes for it hands it the CPU there and then. A spinner never sleeps, so it never carries anything owed into a wakeup.

For that to work, the lag has to survive the moment the task rejoins the queue. Joining moves V, because the task's own weight now counts in the average, and that alone would take away part of the lag it brought. So, as Linux's `place_entity` does, the lag is scaled up by (W + w) / W first, where W is the weight already on the queue and w the task's own:

```cpp
lag = Stdlib::Max(-se->LagLimit(), Stdlib::Min(se->LagLimit(), lag));
u64 load = Stdlib::Min(sums.Load, (u64)1 << 24);
lag = (lag * (s64)(load + se->Weight)) / (s64)load;
v = average - (u64)lag;
```

The lag is clamped to two slices or a tick, whichever is longer, so a task cannot bank credit forever. A brand-new task joins with no lag and half a slice: it runs soon, but not a whole slice ahead of everybody. A task that moves to another CPU takes its lag with it too, so neither sleeping nor moving is a way to be owed more or to owe less.

### Without floating point

The kernel keeps SSE and the x87 switched off, because it does not save their registers on a context switch, so floating point would turn into slow software emulation. Everything here is integers. A weight's inverse is fixed point, (1024 << 32) / weight, and eligibility is tested without a division, the way Linux tests it: measured from the smallest virtual runtime on the queue, a task is eligible when its distance times the queue's total weight is at most the weighted sum of everybody's distances. The products need 128 bits:

```cpp
bool EevdfQueue::Eligible(const SchedEntity* se, const Sums& sums)
{
    u64 span = Stdlib::Min(se->VRuntime - sums.Base, MaxSpan);
    return (unsigned __int128)span * sums.Load <= (unsigned __int128)sums.Weighted;
}
```

Virtual times are only ever compared as differences, so they may wrap around without harm.

### O(n), on purpose

The original idea was a decision in O(1). Exact EEVDF needs the eligible task with the earliest deadline, and finding that quickly takes an ordered structure: Linux keeps an augmented red-black tree, which is O(log n). O(1) would mean buckets of virtual time and an approximation. nos does neither. Each decision walks the run list and computes V and every eligibility from scratch, which is O(n) in the tasks that can run on that CPU, with nothing kept between decisions that could drift out of date.

The counters from step 0 say why that is the right trade. The run list holds only tasks that can run, and there are few of those: on an idle EX44 the timer finds 0.2 runnable tasks per CPU on average, and five at the most since boot. Under HVF with eight `netload` senders, CPU 0 found two or more at 58% of its looks. A tree would cost more than it saves on lists that short. Buckets can wait until a measurement shows a CPU with many runnable tasks.

### The rest

- On an interrupt, the running task is one of the candidates and keeps the CPU unless another eligible task's deadline comes sooner. A task that yields has its deadline moved to at least a slice past V. That is bounded, so a task yielding forever cannot push its deadline out forever.
- The CPU's one-shot timer is armed for the end of the running task's slice, so a slice ends when its deadline comes, not at the next tick. But only while another task is waiting: alone on its CPU, a task's slice would end deciding nothing, and a guest's vCPU on a core of its own would leave its guest every 3 ms for it.
- The idle task counts only while it runs something other than its idling, such as the boot: it joins as it stops idling and leaves, its lag kept, when it idles again.
- `sched queue [cpu]` prints what EEVDF knows about each task on a CPU: its nice, slice, lag, deadline and whether it is eligible. `sched nice <pid> <n>` and `sched slice <pid> <us>` change a task's weight and slice.

## A bug that can no longer be written

The first version of EEVDF in the scheduler had a bug of ordering. The queue was handed the task on the CPU as a parameter. Having picked the next task, the scheduler took it off the run list straight away. Then the current task went to sleep, and its lag was computed from V. But at that moment the next task was neither on the list nor on the CPU, so V did not count it.

Beside one spinner that V was the sleeper's own virtual runtime, so the sleeper went to sleep owed nothing, every single time. At every wakeup it had to wait out whatever was left of the spinner's slice, up to the whole 3 ms, while beside two spinners a wakeup took a few microseconds, because there V still counted the other one. That inversion was the clue. A line added to `sched probe` to print the lag the waiter woke with confirmed it: zero, beside one spinner, every time.

The first fix moved the removal after the sleep and left a comment explaining why the order mattered. The project's own rules ask for better than that: between two designs, take the one where the bug cannot be written over the one where a comment asks nobody to write it. So the queue now owns the task on the CPU ([eevdf.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/eevdf.cpp)). A task on the queue is either on the list or current, counted in V either way, and never neither. Five calls move it, `Join`, `JoinCurrent`, `Leave`, `PutBack` and `Take`, and `Take` refuses with a `BugOn`, a kernel panic that names the line, while anything is still current:

```cpp
void EevdfQueue::Leave(SchedEntity* se)
{
    /* On the queue: current, or on the list */
    BugOn(se != Curr && se->Entry.IsEmpty());

    /* V with se in it */
    s64 lag = (s64)(V() - se->VRuntime);
    s64 limit = se->LagLimit();
    se->VLag = Stdlib::Max(-limit, Stdlib::Min(limit, lag));

    if (se == Curr)
    {
        Curr = nullptr;
        return;
    }
    se->Entry.RemoveInit();
    Count--;
}

void EevdfQueue::Take(SchedEntity* se)
{
    BugOn(Curr != nullptr || se->Entry.IsEmpty());

    se->Entry.RemoveInit();
    Count--;
    Curr = se;
}
```

That leaves a decision one order to be made in: pick the next task while the last one is still current, then the last one goes to sleep or back onto the list, and only then is the next one taken.

## A test that passed with the bug in it

Each boot runs `TestEevdf` on a CPU other than the boot's ([test.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/test.cpp)). A wakeup beside one spinner and beside two must take under half a millisecond and under a millisecond at the median, and a spinner at nice 0 must get 2.4 to 3.9 times the CPU of one at nice 5. The weights say 3.06.

To make sure the test would catch the bug, the bug was put back. The test passed, at 153 µs, comfortably under its bound. It signalled the waiter at a fixed rhythm, a millisecond's poll and a 2 ms gap, against a 3 ms slice, and the wakeups had settled just before the end of a slice, where waiting out "the rest of the slice" costs almost nothing. Now the test waits a random part of a slice between signals, drawn from a fixed seed:

```cpp
        /* xorshift64 */
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        Sleep(EevdfGapNs + (ulong)(x % SchedEntity::DefaultSliceNs));
```

With the bug back the median is 1256 µs and the boot fails. Without it, a wakeup takes 4 to 8 µs under HVF and 18 to 26 µs under TCG, and the share comes out at 2.9 to 3.1. A test is not proven until it has failed the run it is there to fail.

The queue also has a target of its own in [the C++ fuzzer](/blog/fuzzing-the-kernel-on-the-host), `eevdf` ([fuzz/cpp/targets/eevdf.cpp](https://github.com/irqlevel/nos/blob/master/fuzz/cpp/targets/eevdf.cpp)). It compiles the queue for the host and drives it in the scheduler's order with tasks of random weights and slices that run, sleep, wake, yield and move away and back. Every step is held to a model computed from scratch in 128 bits: the task picked is eligible and no eligible one has a sooner deadline, a task rejoins with the lag it left with, a task that leaves takes the lag V gives it, a task that ran is charged what its weight says, and every task on the queue is on its list or current. A copy of `Leave` that computed V without the leaving task failed on the first input.

The review of the change for undefined behaviour found one data race as well. Setting a task's nice or slice read the task's queue pointer under the queue's lock alone, while the queue's teardown wrote it under the task's lock alone. Both setters now take both locks, in the order the scheduler takes them.

## The numbers

`sched probe` under HVF on four vCPUs, the same machine as the round robin's table above:

| Spinners | Median, round robin | Median, EEVDF | p99, EEVDF | Shares with EEVDF, per mille |
|---|---|---|---|---|
| 0 | <32 µs | <32 µs | <128 µs | — |
| 1 | <8 µs | <32 µs | <128 µs | 998 |
| 2 | <8.1 ms | <32 µs | <256 µs | 498, 497 |
| 4 | <32.7 ms | <16 µs | <512 µs | 249, 250, 249, 249 |
| 8 | — | <16 µs | <1.0 ms | 124 each |

On the EX44's own silicon, on one of its performance cores, a wakeup waits under 2 µs at the median beside one, two and four spinners, under 64 µs at the 99th percentile, and the shares come out 998; 498 and 499; and 250, 249, 249 and 249 per mille. The boot's `TestEevdf` there, on an efficiency core, measures 1 µs beside one spinner and beside two, and 3.0 for the share. EEVDF does cost something per decision, because V and every eligibility are computed from scratch two to four times in each one. On the EX44 at rest a decision takes 1.2 to 1.6 µs on each CPU, about what the round robin's 1.5 µs was; under HVF, measured side by side, it takes 1.0 µs against the round robin's 0.74. The block layer does not notice the change: random 4 KiB reads from NVMe run at 26,768 IOPS with one request in flight (26,788 before) and 180,610 with eight (178,913). The idle-wait test, which exists because a wait on an otherwise idle CPU once cost a whole tick, still measures 22 µs.

The longest wait of each probe run there is 5 to 7 ms. It shows up with no spinner at all, and the round robin had it too, so it is not the queue's doing.

## What is left

The hypervisor's vCPUs are ordinary tasks to the scheduler, so a guest can now be given a weight and a slice of its own. Nothing sets them yet. The O(1) buckets wait for a measurement that asks for them. The details are in [the scheduler documentation](/docs/scheduler#who-runs-next-eevdf), and the tests in [the testing guide](/docs/testing).
