---
title: "When a wait never ends"
description: "Task::Wait, the join every stop in nos makes, used to wait forever in silence. Now it reports who waits, for which task, and where that task is."
date: 2026-10-22
tags: [safety, debugging, hypervisor, testing, x86-64, arm64]
---

One rule in the nos project's CLAUDE.md is put more bluntly than the rest: "A hang with no report is the worst outcome there is, and a change that turns one into a report is worth making." Some waits in the kernel already keep it. On x86 a TLB shootdown asks every other CPU to drop a stale address translation, and if one never answers, the kernel panics after ten seconds and names it ([that story](/blog/no-allocation-under-spinlock)). A spinlock held for ten seconds is a panic too.

One wait was still silent, though, and it was the one that every stop makes. Until 22 October, `Task::Wait` waited for another task to end with no deadline and without a word. Here is how it learned to speak up, and how a new function, `Task::Locate`, tells it what to say.

## The join behind every stop

A join is one task waiting for another to finish. In nos that is `Task::Wait`, and every stop path in the kernel goes through it. A Rust `TaskHandle` waits for its task when it is dropped ([task.rs](https://github.com/irqlevel/nos/blob/master/src/rust/kcore/src/task.rs)). `hv stop` waits for the guest's task in `finish()`. A guest's disk, `FileDisk`, waits for its own disk task when it goes away. `rmmod` and the host's shutdown end in joins too.

Here is the whole of the old join:

```cpp
void Task::Wait()
{
    while (State.Get() != StateExited)
    {
        Sleep(1 * Const::NanoSecsInMs);
    }
}
```

Once a millisecond it checks whether the task has exited. If the task never exits, the waiter waits forever, and nothing reaches the log. A disk task stuck in a read was enough to hang `hv stop`, the unload of the `hv` module and the poweroff, all three in silence. The review of the hypervisor on 10 October had already written the gap down ([ddf53d3](https://github.com/irqlevel/nos/commit/ddf53d3c77bd30c83093803bb16df233f36f5420)): "Wanted: a wait that names what did not answer, as the TLB shootdown does."

## Why it still never gives up

The obvious fix is a timeout, and it would be wrong. A task that has not exited is still using whatever its waiter is about to free. A join that gave up would hand its caller memory that another task still uses, and that is worse than a hang.

So the join still waits as long as it takes, but no longer quietly ([9b1bcbc](https://github.com/irqlevel/nos/commit/9b1bcbca765ec4d86e0bb97d32b38e352752c779)). Ten seconds in, it writes a report to the kernel log. After that it reports again each time the wait has doubled, at about 20 seconds, then 40, then 80, but never more than five minutes apart:

```cpp
void Task::Wait(ulong reportNs)
{
    const ulong start = GetBootTime().GetValue();
    ulong reportAt = reportNs;

    while (State.Get() != StateExited)
    {
        Sleep(1 * Const::NanoSecsInMs);

        ulong waited = GetBootTime().GetValue() - start;
        if (waited < reportAt)
            continue;

        ReportJoin(waited);
        reportAt = waited + Stdlib::Min(waited, JoinReportMaxNs);
    }
}
```

`reportNs` defaults to ten seconds, so every caller gets the report without changing a line. Here is what one looks like, as [the scheduler documentation](/docs/scheduler#waiting-for-one-to-end) shows it:

```text
join: shell (pid 12) has waited 10000 ms for hv/vm3/vda (pid 57) to exit, and it is blocked
join:   [0] 0xFFFF80000105AF99 Kernel::TaskQueue::Schedule+0x449
join:   [1] 0xFFFF80000105B594 Kernel::ScheduleCurrent+0x154
...
```

The first line says who waits (the shell, pid 12), for which task (`hv/vm3/vda`, the disk task of the guest `vm3`), for how long, and what state that task is in. The lines after it say where the task is, one frame per line, each return address named from the kernel's symbol table.

## Finding a task: Task::Locate

The hard part of the report is where the task is. It can be in one of three places, and `Task::Locate` handles each differently ([task.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/task.cpp)).

**Off its CPU.** A task that is blocked, asleep, or runnable and waiting for its turn runs nowhere. Its last context switch saved its registers on its own stack, including the frame pointer, which starts the chain linking each stack frame to its caller's. Locate reads the saved one and walks the chain, staying inside the bounds of the task's stack.

The catch is that the task may start running while we read, because nothing holds it still. So the read works like a seqlock: note a sequence number, read, and keep what was read only if the number has not changed. Here the sequence number is the task's own count of context switches:

```cpp
    long switches = ContextSwitches.Get();
    ulong base = (ulong)StackPtr;
    ulong limit = (ulong)&StackPtr->StackTop[0];
    ulong sp = __atomic_load_n(&Rsp, __ATOMIC_RELAXED);

    size_t count = StackTrace::CaptureFrom(Hal::TaskSavedFramePointer(sp, base, limit),
        base, limit, where.Frames, WhereFrames);

    Hal::SmpRmb();
    if (State.Get() != StateWaiting || ContextSwitches.Get() != switches)
        return false;
```

If the task was switched in or out during the read, its state or its count shows it, and Locate throws the result away and looks again. A lost race can name wrong frames, but it cannot fault. The asker holds a reference to the task, so the stack stays mapped. And `Hal::TaskSavedFramePointer` now refuses a saved frame that is not inside the stack, so a stale stack pointer cannot send the walk into unmapped memory.

**On a CPU.** A running task's frames change all the time, so Locate asks instead of reading. It sends an IPI, an inter-processor interrupt, to every other running CPU. Each handler checks whether the task it interrupted is the one wanted, and if so records the interrupted instruction and the frames above it. Locate waits up to a second for the answers.

**On a CPU that does not answer.** A CPU with interrupts off never takes the IPI, and that is usually exactly where a stuck task is: spinning with interrupts off. So after a second, Locate names the silent CPUs instead, as a mask: "on a cpu that would not answer: cpus 0x20 took no interrupt in 1000 ms".

That last case needed care. A CPU that takes the IPI late still writes its answer when it gets there, and on the asker's stack it would write into a stack long since reused. So each CPU gets one slot for its answers, made the first time it is asked and kept for good. A slot whose IPI is still unanswered is not sent again, and its CPU is simply named again. A slot answered late is reused.

## bt finds what it could not

The shell's `bt <pid>` prints a task's backtrace, and it now uses `Locate` too. For a task off its CPU, the old `bt` walked the saved frame pointer inside the bounds of the shell's own stack. The task's frames live on a different stack, so the walk stopped at once, and `bt` never printed a single frame of a blocked or sleeping task. For a running task, the old `bt` sent an IPI to the CPU whose queue held the task and took whatever that CPU happened to run when the interrupt landed. Now the first line of `bt` says where the task is, in the same words the join report uses: blocked, asleep, runnable, running on a CPU at a named function, or on a CPU that took no interrupt in a second.

A smaller fix came with it. On x86, every task's backtrace used to end in one extra frame read out of the stack's poison, the pattern `0x5A5A5A5A5A5A5A5A` that fills a new stack. Now `Hal::BuildTaskFrame` leaves a 0 where the entry function's return address would be, and the walk stops there.

## Held to it at every boot

A report that only shows up when something is already stuck is easy to break without noticing. So a boot self-test, `TestTaskLocate`, runs once from the boot CPU with the others up. A task blocked on an event must be read off its stack, with a frame naming its function. For a task that keeps another CPU deaf, with interrupts off, Locate must name that CPU as silent after the tenth of a second the test waits. A task spinning on that same CPU must then be caught there by the IPI, which proves that the slot the deaf CPU answered late can be reused. And a join of 50 ms, with the report interval set to 20 ms, must be reported.

If the test never tells the deaf task to stop, it gives up after two seconds and the test fails, rather than wedging the boot. A failed check panics with "Task locate test failed", and the test runs on every boot of both architectures, so every smoke test gates it. The commit also ran the smoke tests under `UBSAN=1 RUSTUB=1`, the builds that catch undefined behaviour at run time, and [the C++ fuzzer](/blog/fuzzing-the-kernel-on-the-host) through 935,000 inputs with no finding. Its review for undefined behaviour names one limit: on arm64 the read can name wrong frames for a few instructions after a switch-in, but it never faults.

## Why it matters

The join report does for a task what the TLB shootdown's list of silent CPUs does for a CPU, and the rule in CLAUDE.md now names it next to the shootdown and the watchdogs. The item from the review is struck through in [the hypervisor documentation](/docs/hypervisor#lifecycle-and-stages-4-and-5). A stuck disk task will still hang `hv stop`, because freeing what it uses would be worse. But now, ten seconds in, the log says which task is stuck, in which guest, and where.

The same merge, [b235726](https://github.com/irqlevel/nos/commit/b235726b72db128f36fd84ad239578600001e6f5), also shut an AMD host to BTI, Retbleed and SRSO from its guests. That part is in [the update to the post on guest speculation](/blog/guest-speculation-isolation).
