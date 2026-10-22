---
title: "The deadlock that never panicked: freeing memory under a spinlock"
description: "A TLB shootdown waited on a CPU spinning on the lock its sender held. How one silent wedge became a rule, a frame pool, a gate and a ten-second panic."
date: 2026-09-04
tags: [safety, memory, debugging, networking, fuzzing, x86-64]
---

The EX44 stopped answering. It is one of the two Hetzner servers nos runs on, and its only console is the network: the [UDP shell](/docs/udp-shell) to type at, [netconsole](/docs/netconsole) to read the log. A `ps` typed into the shell got no reply on the third try; the next time, on the seventh. There was no panic and no fault. The netconsole log simply ended mid-sentence, and nothing reset the machine, because nothing had gone wrong in a way the kernel knew how to notice.

## Two stacks

We reproduced it on a 20-CPU QEMU guest by running `ps` over the UDP shell in a loop: it stopped somewhere between the 60th and the 140th iteration. With GDB on QEMU's stub, two CPUs told the whole story, as the fix's commit message recorded it:

```text
CPU#0  UdpShell::Run -> SendUdp -> SendRaw -> NetDevice::SubmitTx
       -> RawSpinLock::LockIrqSave(TxQueueLock)          spinning, IRQs off

CPU#1  softirq -> VirtioNet::DrainTx (holds TxQueueLock) -> CompleteTx
       -> NetFrame::Put -> Mm::Free -> PageAllocator::Free
       -> CpuTable::InvalidateTlbRange -> SendTlbIPI(mask 0xFFFFD)
       -> WaitGroup::Wait                                waiting for CPU#0
```

CPU#1 was completing transmitted frames while holding the device's TX lock. Dropping the last reference to a frame freed it, and the free unmapped a page. On x86 `invlpg` touches only the local TLB, so `CpuTable::InvalidateTlbRange` sends every other running CPU an IPI to drop the stale translation and waits until each one has acknowledged ([Paging](/docs/paging#tlb-shootdown-and-the-deadlock-it-can-cause)). The mask, `0xFFFFD`, is every CPU but the sender, CPU#0 included. And CPU#0 was spinning on that same TX lock, which `LockIrqSave` takes with interrupts off, so the IPI was never taken. CPU#1 waited for CPU#0; CPU#0 waited for CPU#1's lock. The other eighteen CPUs had answered and gone idle.

No fault, so no panic. The TCO watchdog was not armed, so no reset. And the netconsole drain needed the same TX lock to send a single line, which is why the log stopped mid-sentence.

The part that stings: the hazard was already written down, in that very driver. Its interrupt handler deferred TX completion to the soft IRQ precisely because "Mm::Free() can trigger a TLB shootdown IPI [and] if another CPU has IRQs disabled the IPI cannot be acknowledged, causing a deadlock". The deferral was necessary and not sufficient: the soft IRQ then took the TX lock with interrupts off and freed under it anyway. The freeing CPU's interrupt state was never the problem. The spinning CPU's was.

## Collect, unlock, release

The fix ([eaff359](https://github.com/irqlevel/nos/commit/eaff359)) has the shape every later instance has had. Under the lock, move what needs releasing onto a local list; drop the lock; then release.

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

800 iterations of the reproducer passed, and CLAUDE.md got the rule, since nothing about it is specific to networking: never allocate or free with a spinlock held. On arm64 this exact cycle cannot close, because `TLBI` broadcasts in hardware, `Hal::TlbShootdownNeedsIpi()` answers false and nobody waits. But the code is common to both architectures, and all three real machines nos runs on are x86-64.

## The wrong driver

Sixteen minutes later came the correction ([fc314e7](https://github.com/irqlevel/nos/commit/fc314e7)). The reproducer ran on virtio-net, because that is what QEMU gives you. The EX44 has an RTL8125, and its Rust driver, like r8168's, reaped its TX ring inside `flush_tx`, which the net stack called under that same TX lock, and dropped each finished frame right there. Same deadlock, on the machine that actually had it. We had said the other drivers were checked. The grep behind that claim looked for a function name that does not exist in this tree, returned nothing, and the nothing was read as clean.

So the release moved out of the drivers and into the layer that owns the lock, where a driver cannot get it wrong. Today the net layer is Rust: a driver's `flush_tx` is lent a `TxQueue` for the length of the call, and a finished frame has one documented way out ([device.rs](https://github.com/irqlevel/nos/blob/master/src/rust/net/src/device.rs)):

```rust
/// A transmitted frame, for release once the lock is down. Never dropped
/// here instead: dropping frees, a free can reach the page allocator,
/// which shoots down the TLB on every other CPU and waits for each -- and
/// a CPU spinning on this lock has interrupts off and never answers.
pub fn done(&mut self, frame: Frame) {
    self.done.push(frame);
}
```

The layer empties that queue once the lock is down:

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

`self.tx` is an `IrqSpinLock<Tx>`, a lock that owns what it guards ([sync.rs](https://github.com/irqlevel/nos/blob/master/src/rust/kcore/src/sync.rs)): the queue cannot be reached at all except through the guard, and that is the type's doing, not a comment's. The guard `lock()` returns here is a temporary, dropped at the end of the `let`, so by the time `drop(frame)` runs the lock is already released. `submit_tx` adds one more subtlety: it releases only with interrupts on. A caller with interrupts off could not answer a shootdown itself, so it raises the TX soft IRQ and leaves the release to it.

## Ten seconds, then a report

A rule in a file stops the next instance only if someone reads it, and we had just shown how easily one is missed. So the same commit made the failure loud. The wait for acknowledgements had been a loop that only an answer could end; `CpuTable::SendTlbIPI` now gives up after ten seconds, far past any legitimate round of IPIs, and panics, naming the CPUs that never answered ([cpu.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/cpu.cpp)):

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

Later that day the lock watchdog learned the same lesson ([22e49c2](https://github.com/irqlevel/nos/commit/22e49c2)). It had only ever watched `SpinLock`, and `TxQueueLock` was a `RawSpinLock`, which is why `dmesg 2048 Spinlock` had come back empty on a machine whose TX lock had been held for minutes. The instrumentation moved down into `RawSpinLock`. A lock held for ten seconds became a panic instead of one more trace line. And the panic path learned to send its report without the TX lock, trying it once and going on without it, because otherwise the report had to leave through the very door that was jammed. A deliberate wedge, a shell command that holds the TX lock forever with interrupts off, then reached the collector as:

```text
Spinlock 0xFFFF800040420330 is held too long 27055333
PANIC: Spinlock 0xFFFF800040420330 stuck, held 10003951083 ns
CPU: 3  Task: pid 3 name idle3
Backtrace: DoPanic <- Watchdog::Check <- GenericTimer::LocalTick <- ArmIrqEntry
```

## A datapath that never frees

Better than releasing off the lock is not releasing to the allocator at all. Also on September 4 ([c3e1d8d](https://github.com/irqlevel/nos/commit/c3e1d8d)), network frames moved into a pool built once at boot. The frames are of a fixed size, with their physical addresses resolved once. They are handed out from per-CPU caches, touched with interrupts off and no atomics, and behind the caches is one lockless ring, Vyukov's bounded MPMC queue, which the caches refill from and spill to in batches of 32. The commit put the priority plainly: a datapath that never calls the allocator cannot have that bug at all, which is worth more than the speed. On a 4-CPU arm64 guest, 500 shell round trips took 506 frames from the pool with zero misses, and the ring was touched 31 times. The pool is Rust now ([frame.rs](https://github.com/irqlevel/nos/blob/master/src/rust/net/src/frame.rs)): 4096 frames by default, `netframes=` to change that, `netpool` to watch it.

The speed came anyway. On the AX41 the first flood after every boot used to lose frames at the NIC, 755,105 of 2.06 million, all in its first two seconds. The receive ring had been filled from `Mm::Alloc` before the pool was set up, and releasing each of those frames was a TLB shootdown waiting on every other CPU, some 4 ms apiece there ([e02dfe3](https://github.com/irqlevel/nos/commit/e02dfe3)). The pool now comes up before any driver.

## The rule as a gate

The fuzzers ([Testing](/docs/testing)) enforce the rule mechanically. The allocator under the network and storage fuzzers checks every allocation and every free, and one made with interrupts off is a finding at the line that made it, with the seed and iteration that make it again; the panic says why: "a spin lock held: a free can wait on every other CPU, and one spinning on this lock never answers" ([heap.rs](https://github.com/irqlevel/nos/blob/master/fuzz/common/machine/heap.rs)). The C++ fuzzer's heap target asks the same of every page the allocator takes: `INVARIANT(Fuzz::HeldSpinLocks() == 0, "a page allocated with a spin lock held")`.

On September 30 it found one in the last place we would have looked: the kernel heap itself ([00631ab](https://github.com/irqlevel/nos/commit/00631ab)). `Pool::Alloc`, behind the heap's small allocations, took a new page from the page allocator while holding the pool's own spinlock. When mapping that page failed, the page allocator shot down every CPU's TLB and waited, and a CPU spinning on the pool's lock would never have answered. The fix has the same shape once more: allocate with the lock let go of, and if another CPU put a page on the list in the meantime, give the spare back, off the lock too ([pool.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/mm/pool.cpp)):

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

The bug class has not gone away, and the kernel does not pretend it has. What changed is that it no longer passes in silence. A rule in CLAUDE.md names it. On the transmit path the net layer gives a driver one way to hand a frame back. The fuzzers flag a violation at the line where it happens. The frame pool keeps the busiest path away from the allocator. And on a real machine a new instance is a ten-second panic over netconsole naming the CPUs that never answered, instead of a box that just stops. The EX44 had one more silent stop in store, on the receive side, fixed the next day; that one is [a story of its own](/blog/rtl8125-lost-msix-edge).
