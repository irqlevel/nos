---
title: "The deadlock that never panicked: freeing memory under a spinlock"
description: "A TLB shootdown waited for a CPU that was spinning on a lock the sender held. How one silent hang became a rule, a frame pool, a gate and a 10-second panic."
date: 2026-09-04
tags: [safety, memory, debugging, networking, fuzzing, x86-64]
---

The EX44 stopped answering. It is one of the two Hetzner servers nos runs on, and its only console is the network: we type into the [UDP shell](/docs/udp-shell) and read the log through [netconsole](/docs/netconsole). We typed `ps` into the shell, and the third one got no reply. The next time, the seventh one hung. There was no panic and no fault. The netconsole log simply stopped mid-sentence, and nothing reset the machine, because nothing had gone wrong in a way the kernel knew how to notice.

## Two stack traces

We reproduced it on a 20-CPU QEMU guest by running `ps` over the UDP shell in a loop. It hung somewhere between the 60th and the 140th iteration. With GDB attached to QEMU's stub, two CPUs told the whole story. Here they are as the fix's commit message recorded them:

```text
CPU#0  UdpShell::Run -> SendUdp -> SendRaw -> NetDevice::SubmitTx
       -> RawSpinLock::LockIrqSave(TxQueueLock)          spinning, IRQs off

CPU#1  softirq -> VirtioNet::DrainTx (holds TxQueueLock) -> CompleteTx
       -> NetFrame::Put -> Mm::Free -> PageAllocator::Free
       -> CpuTable::InvalidateTlbRange -> SendTlbIPI(mask 0xFFFFD)
       -> WaitGroup::Wait                                waiting for CPU#0
```

CPU#1 was completing transmitted frames while holding the device's TX lock. Dropping the last reference to a frame freed it, and the free unmapped a page. On x86, `invlpg` flushes only the local TLB. So `CpuTable::InvalidateTlbRange` sends every other running CPU an IPI asking it to drop the stale translation, and waits until each one has acknowledged ([Paging](/docs/paging#tlb-shootdown-and-the-deadlock-it-can-cause)).

The mask, `0xFFFFD`, is every CPU except the sender, CPU#0 included. But CPU#0 was spinning on that same TX lock, which `LockIrqSave` takes with interrupts off, so it never took the IPI. CPU#1 waited for CPU#0, and CPU#0 waited for CPU#1's lock. The other eighteen CPUs had answered and gone idle.

There was no fault, so there was no panic. The TCO watchdog (a hardware watchdog in the chipset) was not armed, so nothing reset the machine. And the netconsole drain needed the same TX lock to send even one line, which is why the log stopped mid-sentence.

The part that stings is that the hazard was already written down, in that very driver. Its interrupt handler deferred TX completion to the soft IRQ precisely because "Mm::Free() can trigger a TLB shootdown IPI [and] if another CPU has IRQs disabled the IPI cannot be acknowledged, causing a deadlock". The deferral was necessary but not sufficient. The soft IRQ then took the TX lock with interrupts off and freed frames under it anyway. The interrupt state of the freeing CPU was never the problem. The interrupt state of the spinning CPU was.

## Collect, unlock, release

The fix ([eaff359](https://github.com/irqlevel/nos/commit/eaff359)) has the shape that every later fix of this kind has had. Under the lock, move whatever needs releasing onto a local list. Drop the lock. Then release.

```cpp
void VirtioNet::DrainTx()
{
    Stdlib::ListEntry done;
    done.Init();

    ulong flags = TxQueueLock.LockIrqSave();
    CompleteTx(done);
    FlushTx();
    TxQueueLock.UnlockIrqRestore(flags);

    // ...
    while (!done.IsEmpty())
    {
        NetFrame* frame = CONTAINING_RECORD(done.RemoveHead(), NetFrame, Link);
        frame->Put();
    }
}
```

The reproducer then passed 800 iterations. Nothing about the rule is specific to networking, so it went into CLAUDE.md: never allocate or free with a spinlock held. On arm64 this exact cycle cannot close. There `TLBI` broadcasts in hardware, `Hal::TlbShootdownNeedsIpi()` returns false, and nobody waits. But the code is common to both architectures, and all three real machines nos runs on are x86-64.

## The wrong driver

Sixteen minutes later came a correction ([fc314e7](https://github.com/irqlevel/nos/commit/fc314e7)). The reproducer ran on virtio-net, because that is what QEMU gives you. But the EX44 has an RTL8125. Its Rust driver, like the r8168 one, reaped its TX ring inside `flush_tx`, which the net stack called under that same TX lock, and dropped each finished frame right there. It was the same deadlock, on the machine that actually had it.

We had said the other drivers were checked. The grep behind that claim looked for a function name that does not exist in this tree. It returned nothing, and we read that nothing as clean.

So the release moved out of the drivers and into the layer that owns the lock, where a driver cannot get it wrong. Today the net layer is written in Rust. A driver's `flush_tx` borrows a `TxQueue` for the length of the call, and a finished frame has one documented way out ([device.rs](https://github.com/irqlevel/nos/blob/master/src/rust/net/src/device.rs)):

```rust
/// A transmitted frame, for release once the lock is down. Never dropped
/// here instead: dropping frees, a free can reach the page allocator,
/// which shoots down the TLB on every other CPU and waits for each -- and
/// a CPU spinning on this lock has interrupts off and never answers.
pub fn done(&mut self, frame: Frame) {
    self.done.push(frame);
}
```

The layer empties that queue after it has released the lock:

```rust
pub fn release_tx_done(&self) {
    loop {
        let frame = self.tx.lock().done.pop();
        match frame {
            /* Outside the lock, always: see `tx_done` */
            Some(frame) => drop(frame),
            None => return,
        }
    }
}
```

`self.tx` is an `IrqSpinLock<Tx>`, a lock that owns what it guards ([sync.rs](https://github.com/irqlevel/nos/blob/master/src/rust/kcore/src/sync.rs)). The queue cannot be reached at all except through the lock's guard, and it is the type that enforces this, not a comment. The guard that `lock()` returns here is a temporary, dropped at the end of the `let` statement. So by the time `drop(frame)` runs, the lock is already released. `submit_tx` adds one more subtlety: it releases frames only with interrupts on. A caller with interrupts off could not answer a shootdown itself, so it raises the TX soft IRQ and leaves the release to it.

## Ten seconds, then a report

A rule in a file stops the next instance only if someone reads it, and we had just shown how easy it is to miss one. So the same commit made the failure loud. The wait for acknowledgements used to be a loop that only an answer could end. Now `CpuTable::SendTlbIPI` gives up after ten seconds, far longer than any legitimate round of IPIs takes, and panics, naming the CPUs that never answered ([cpu.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/cpu.cpp)):

```cpp
    static const ulong AckTimeoutNs = 10 * Const::NanoSecsInSec;
    // ...
    for (ulong i = 0; i < MaxCpus; i++)
    {
        // ...
        while (tasks[i].Completion.GetCounter() != 0)
        {
            if (GetBootTime() >= deadline)
            {
                silent |= (1UL << i);
                break;
            }

            YieldToRunnable();
        }
    }

    if (silent != 0)
        Panic("TLB shootdown: cpus 0x%lX never acknowledged (asked 0x%lX)",
            silent, cpuMask);
```

Later that day the lock watchdog learned the same lesson ([22e49c2](https://github.com/irqlevel/nos/commit/22e49c2)). It had only ever watched `SpinLock`, and `TxQueueLock` was a `RawSpinLock`. That is why `dmesg 2048 Spinlock` had come back empty on a machine whose TX lock had been held for minutes. The instrumentation moved down into `RawSpinLock`, and a lock held for ten seconds became a panic instead of one more trace line.

The panic path also learned to send its report without the TX lock. It tries the lock once and goes on without it, because otherwise the report would have to leave through the very door that was jammed. Then we hung a machine on purpose, with a shell command that holds the TX lock forever with interrupts off. The report reached the log collector like this:

```text
Spinlock 0xFFFF800040420330 is held too long 27055333
PANIC: Spinlock 0xFFFF800040420330 stuck, held 10003951083 ns
CPU: 3  Task: pid 3 name idle3
Backtrace: DoPanic <- Watchdog::Check <- GenericTimer::LocalTick <- ArmIrqEntry
```

## A datapath that never frees

Even better than releasing outside the lock is never releasing to the allocator at all. Also on September 4 ([c3e1d8d](https://github.com/irqlevel/nos/commit/c3e1d8d)), network frames moved into a pool built once at boot. The frames have a fixed size, and their physical addresses are resolved once. They are handed out from per-CPU caches, which are touched with interrupts off and need no atomics. Behind the caches sits one lockless ring, Vyukov's bounded MPMC (multi-producer, multi-consumer) queue. The caches refill from it and spill back to it in batches of 32.

The commit put the priority plainly: a datapath that never calls the allocator cannot have that bug at all, and that is worth more than the speed. On a 4-CPU arm64 guest, 500 shell round trips took 506 frames from the pool with zero misses and touched the ring 31 times. The pool is in Rust now ([frame.rs](https://github.com/irqlevel/nos/blob/master/src/rust/net/src/frame.rs)). By default it holds 4096 frames. `netframes=` changes that, and `netpool` shows its state.

The speed came anyway. On the AX41, the first flood after every boot used to lose frames at the NIC: 755,105 of 2.06 million, all in the first two seconds. The receive ring had been filled from `Mm::Alloc` before the pool was set up. Releasing each of those frames meant a TLB shootdown that waited on every other CPU, about 4 ms apiece on that machine ([e02dfe3](https://github.com/irqlevel/nos/commit/e02dfe3)). Now the pool comes up before any driver.

## The rule as an automatic check

The fuzzers ([Testing](/docs/testing)) enforce the rule mechanically. The allocator under the network and storage fuzzers checks every allocation and every free. One made with interrupts off is a finding, reported at the line that made it, with the seed and iteration that reproduce it. The panic says why: "a spin lock held: a free can wait on every other CPU, and one spinning on this lock never answers" ([heap.rs](https://github.com/irqlevel/nos/blob/master/fuzz/common/machine/heap.rs)). The C++ fuzzer's heap target asks the same of every page the allocator takes: `INVARIANT(Fuzz::HeldSpinLocks() == 0, "a page allocated with a spin lock held")`.

On September 30 the C++ fuzzer found a violation in the last place we would have looked: the kernel heap itself ([00631ab](https://github.com/irqlevel/nos/commit/00631ab)). `Pool::Alloc`, which serves the heap's small allocations, took a new page from the page allocator while holding the pool's own spinlock. When mapping that page failed, the page allocator shot down every CPU's TLB and waited, and a CPU spinning on the pool's lock would never have answered. The fix has the same shape once more. Allocate after letting go of the lock. If another CPU put a page on the list in the meantime, give the spare back, also outside the lock ([pool.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/mm/pool.cpp)):

```cpp
    if (block == nullptr)
    {
        Page* page = NewPage();
        if (page == nullptr)
            return nullptr;

        Page* spare = nullptr;
        {
            Stdlib::AutoLock lock(Lock);

            if (FreePageList.IsEmpty())
            {
                FreePageList.InsertHead(&page->Link);
                EmptyPages++;
            }
            else
                spare = page;
            block = TakeLocked();
        }

        if (spare != nullptr)
            PgAlloc->Free(spare);
    }
```

This class of bug has not gone away, and the kernel does not pretend it has. What changed is that it no longer passes in silence. A rule in CLAUDE.md names it. On the transmit path, the net layer gives a driver one way to hand a frame back. The fuzzers flag a violation at the line where it happens. The frame pool keeps the busiest path away from the allocator. And on a real machine, a new instance is a panic after ten seconds, sent over netconsole and naming the CPUs that never answered, instead of a box that just stops.

The EX44 had one more silent stop in store, on the receive side, and we fixed it the next day. That one is [a story of its own](/blog/rtl8125-lost-msix-edge).
