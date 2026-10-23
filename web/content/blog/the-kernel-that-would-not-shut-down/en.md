---
title: "The kernel that would not shut down"
description: "In September 2026 poweroff did nothing, hung or faulted. The last two bugs: a CPU that woke from its final halt, and a heap that unmapped live objects."
date: 2026-09-13
tags: [debugging, memory, arm64, x86-64]
---

In September 2026 nos could do almost anything except switch itself off. A `poweroff` typed into the shell sometimes did nothing at all. With a disk log area prepared, it hung. And when it did get past unmounting the filesystems, it often faulted afterwards, during the static destructors: on arm64 every time, on x86-64 in one or two runs out of three.

It was a chain of bugs rather than one. The last two, both fixed on September 13, looked different on each architecture but had the same shape. Something was still running, or still linked into a list, after the halt had started to free the memory under it.

## Nobody had pressed the button

The bugs surfaced so late because nothing ran the shutdown path. Both smoke tests wait for their boot markers, exit, and kill QEMU on the way out ([`smoke-test.sh`](https://github.com/irqlevel/nos/blob/master/scripts/smoke-test.sh)), and a QEMU session in development usually ends the same way. A kernel that faulted on the way down looked exactly like one that did not.

That changed on September 9, when nos got a read-write ext2 root filesystem ([7e0b5db](https://github.com/irqlevel/nos/commit/7e0b5db2a57c8e3741fdfff1263201e7c87b17bb)). Like Linux, the driver clears the `valid` bit in the superblock when it mounts read-write and sets it again at a clean unmount. A boot that ends any other way shows up at the next mount as "not cleanly unmounted". From then on, `poweroff` had real work to do.

## A request nobody heard

The quietest failure came first: `poweroff` did nothing. The shell's `poweroff` and `reboot` only set a flag, and the BSP's idle task (the boot CPU's task of last resort) checks it between halts. "Last resort" was new. Since September 5 the scheduler picks an idle task only when nothing else on its CPU can run ([021cbbf](https://github.com/irqlevel/nos/commit/021cbbf3a19f9327ee37584418e8ee4e6f4f66e7)).

At the time, though, `Sleep()` yielded the CPU instead of blocking. A CPU that carried the shell, the DHCP client and the USB poll task always had something runnable. Its idle task never got a turn, and the request sat unseen. On a two-CPU QEMU that happened every time. The ext2 commit of September 9 fixed this too: a request now clears the idle task's `FlagIdleBit`, so from then on that task takes turns like any other ([`cmd.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/cmd.cpp), [Scheduler](/docs/scheduler#the-idle-task)):

```cpp
static void WakeShutdownWatch()
{
    auto& cpus = CpuTable::GetInstance();
    Task* idle = cpus.GetCpu(cpus.GetBspIndex()).GetIdleTask();
    if (idle != nullptr)
        idle->ClearIdle();
}
```

The order of the halt needed work too. Unmounting ext2 writes the superblock, and a block request completes through the BLK_IO soft IRQ, the deferred half of interrupt handling. So the unmount moved ahead of `SoftIrq::Stop()`. Two days later the disk log, which copies the kernel log to a raw partition, turned out to have the same problem ([0d70452](https://github.com/irqlevel/nos/commit/0d704526d5d421c69576d2e79a1af453a4e666df)). With a log area prepared, 0 of 3 poweroffs under QEMU finished. The serial output stopped after the APs (the secondary CPUs) exited: the kernel was waiting on a disk log write that never completed. Now the disk log stops after the unmount and before the soft IRQs.

Here is the x86 order as it stands today, from [`kernel/main.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/main.cpp):

```text
BpStartup, once the request is seen
  network services, USB, the shells, DHCP stopped
  ModuleTable::UnloadAll      a module's exit still has files and block I/O
  kernel_vfs_unmount_all      ext2 writes its superblock, through BLK_IO
  rust_disklog_stop           the disk log's writer finishes
  SoftIrq::Stop               the soft IRQ tasks exit
PrepareHalt
  ExitAllExceptSelf           "Stopping cpu's" ... "Cpu's stopped"
  CpuTable::Reset             exited tasks reaped, run queues emptied
  switch to the static stack
FinalizeOnStaticStack
  task->Put                   the idle task and its stack freed
  rust_fini, __cxa_finalize   Rust teardown, then the static destructors
  "Finalized", then power off or reset
```

Everything above `PrepareHalt` runs on a live machine. Everything below it is the kernel taking itself apart, and that is where the faults were.

## arm64: a halt that did not hold

`ExitAllExceptSelf` stops the other CPUs one at a time. It marks a CPU as exiting and keeps sending it IPIs until that CPU reports `StateExited`. The IPI handler, `Cpu::IPI`, set `StateExited` as soon as it saw the request, drained its queued work, and then ended like this:

```cpp
        InterruptDisable();
        Hlt();
        return;
```

But a single halt is not a place where a CPU stays. On arm64 `Hlt()` is WFI, and WFI may complete at any moment: a pending interrupt ends it even while interrupts are masked. Under HVF (the Apple Hypervisor.framework that QEMU uses on Apple Silicon), all three APs of a four-CPU guest were out of their WFI within milliseconds. Each one returned from the IPI into the task it had interrupted. Meanwhile CPU 0 had seen `StateExited` and was freeing the tasks, their stacks and at last the heap. One of the APs wrote to a page that had just been unmapped and took a data abort, in the middle of CPU 0's static destructors.

The report is where we went wrong. The panic path writes to the PL011 serial port without taking the console lock. So the AP's report came out interleaved, character by character, with CPU 0's traces from the destructors. Read that way, it looked like a fault in `TaskTable::~TaskTable` itself, and the bug went by that name for a while. The SSH server's test, written the day before the fix, accepts a panic at its poweroff and blames `TaskTable::~TaskTable` in a comment ([`sshd-test.py`](https://github.com/irqlevel/nos/blob/master/scripts/sshd-test.py)).

The single halt was old. It came in 2018 with a commit called "kernel: fix crash on shutdown" ([28ba855](https://github.com/irqlevel/nos/commit/28ba855c298d452d91abadd1362a6a47bd1fd42a)), which replaced an endless spin loop with it. On x86 a halt with interrupts off lasts until the first NMI (non-maskable interrupt). The arm64 port runs the same code, and WFI makes no such promise.

The fix ([7908bf6](https://github.com/irqlevel/nos/commit/7908bf6f5ddf8006aeda5cba5dfdbdb93259b526)) gives an exiting CPU one place to end, `Cpu::Park()`. This is how it looks today, with the `Parked` flag from a fix described below ([`cpu.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/cpu.cpp)):

```cpp
void Cpu::Park()
{
    InterruptDisable();

    Parked.Set(1);

    {
        Stdlib::AutoLock lock(Lock);
        State |= StateExited;
    }

    for (;;)
    {
        Hlt();
    }
}
```

`StateExited` is now published last, because the CPU that asked starts freeing the moment it sees it. The halt is a loop, so a WFI that completes simply halts again. And `Park()` is declared `noreturn`, so the IPI handler has no way back to the interrupted task. On arm64 under HVF, 3 of 3 poweroffs had faulted before the fix. With it and the next fix, 15 of 15 were clean.

## x86-64: the heap went first

x86 faulted less often and somewhere else: in `__cxa_finalize`, at `Watchdog::UnregisterSpinLock+0xAA`. On September 11 the disk log commit filed it as "the known race with task memory freed during the halt". The destructor that faulted changed from run to run: `Cpu`, `TaskQueue` and `Serial` were all seen.

`__cxa_finalize` runs the destructors of the kernel's singletons in reverse order of construction. On x86 the heap's `AllocatorImpl` is built in `Main2` after `CpuTable`, `Serial`, `Dmesg` and the watchdog, so it is destroyed before all of them. Each of its size classes is a `Pool`, and `~Pool` gave back every page still on its lists ([Paging and memory](/docs/paging#the-allocators)). But `Free()` returned a page the moment its last block went. A page still on a list at that point held a block nobody had freed, so the loops unmapped live objects and nothing else.

Leaked objects at halt would be harmless if nothing touched them, but the watchdog did. Every watched spin lock registers itself with the watchdog when it is constructed, in one of 512 buckets: doubly linked lists, picked by hashing the lock's address. A leaked object's lock stays linked into its bucket. When a static destroyed after the heap unregisters its own lock, unlinking it writes into its neighbours. If a neighbour sits in a page that `~Pool` has just unmapped, that write faults.

The fault address said exactly that. In the case we caught, the leaked objects were the soft IRQ tasks, `softirq/N`, allocated with the tag `'SIrq'` and left on their CPUs' exited lists at the halt. `Cpu::~Cpu` faulted with cr2 (the register that holds the faulting address) equal to a task's address plus 0x68. That is the `Blink` field of the task's `Lock.RawLock.WatchdogListEntry`. Which static faulted depended on which bucket the leaked lock had landed in.

The fix ([a51290e](https://github.com/irqlevel/nos/commit/a51290e256fce4e488e59dda2c1c7bd963d95e16)) deletes the two loops and puts a comment in their place that begins "The pages stay mapped" ([`pool.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/mm/pool.cpp)):

```diff
-    while (!FreePageList.IsEmpty())
-    {
-        PgAlloc->Free(FreePageList.RemoveHead());
-    }
-
-    while (!PageList.IsEmpty())
-    {
-        PgAlloc->Free(PageList.RemoveHead());
-    }
```

`~Pool` still traces every leaked block with its tag, so a leak stays loud. It just no longer unmaps the page the block sits in. With the arm64 fix in place, x86 under TCG (QEMU's software CPU) went from 2 faulting poweroffs out of 3 to 15 clean out of 15.

## Then the leaks themselves

Keeping the pages made the leaks harmless, not absent. Two more commits on the same day closed them.

The soft IRQ tasks leaked for a different reason on each architecture ([c686108](https://github.com/irqlevel/nos/commit/c6861086d2ec52180b118c6017e82fac6b0c6b47)). On x86, `SoftIrq::Stop()` stops them moments before the halt. An exited task's last reference is dropped by its CPU's idle loop in `TaskQueue::ReapExited()`, and once the halt has begun no CPU idles again. `CpuTable::Reset()` now reaps every CPU's exited list itself, before it takes the table lock. Freeing a task frees its stack, and on x86 the TLB shootdown for that reads the running CPUs under the same lock ([why that matters](/blog/no-allocation-under-spinlock)). On arm64 the halt never called `SoftIrq::Stop()` at all. Now it does.

The USB poll task leaked a third way ([9e236b2](https://github.com/irqlevel/nos/commit/9e236b2cbc6d734fa672a29020294ba481043e71)). `Stop()` waited for the task to exit but never dropped the reference that `Start()` had made it with. After these fixes `~Pool` finds nothing at all at an arm64 poweroff. On x86 it finds one block tagged `'rust'` and three untagged 16-byte ones, none of them tasks.

## A panic that would bury itself

One hole was left, in the panic path ([893c11d](https://github.com/irqlevel/nos/commit/893c11d2d10deeea55009f9bd1e506c9457fcbdd)). Since September 4 a panic asks the other CPUs for their stacks over NMI ([edf299c](https://github.com/irqlevel/nos/commit/edf299c92bf7f7e6ce42266577df1034377fb864)). It asked every CPU in the running set, and a parked CPU never leaves that set. On x86, delivering an NMI pushes a frame onto the stack the CPU stopped on, which the halt may have freed by then. The fault that followed would bury the report that asked for it.

So `Park()` sets `Parked` before it publishes `StateExited`, and `Panicker::CollectRemoteStacks` skips those CPUs. Where their backtraces would be, it prints `Cpu N parked`. We checked it with a panic forced in `FinalizeOnStaticStack`. The report now ends with `Cpu 1 parked` through `Cpu 3 parked`, where those three CPUs used to answer with `ExcNMI` frames from the stacks they were parked on. arm64 sends no NMIs yet, so in practice this one is for x86.

## What stays fixed

The leak fixes were single cases. The other three close a door for good. An exiting CPU has exactly one place to end, a `noreturn` loop, and the CPU that frees memory waits until that CPU has nothing left to do. A leak at halt is a line in the log that names its owner by tag, not a page fault in somebody else's destructor. And a panic during the halt names the parked CPUs instead of reaching onto their stacks.

We also kept two habits for the next fault on the way down. Before trusting a backtrace from the halt, look for a second CPU's output woven into it. And check whether the fault comes after `~AllocatorImpl`: everything destroyed after the heap runs with the heap already gone.
