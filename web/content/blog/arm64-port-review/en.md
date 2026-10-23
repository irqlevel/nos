---
title: "The barriers x86 forgave: a review of the arm64 port"
description: "A wrong memory barrier compiles to nothing on x86 and is a real bug on arm64. What a full review of the nos arm64 port found after every smoke test passed."
date: 2026-07-17
tags: [arm64, memory, drivers, qemu, safety]
---

On x86, nearly every memory barrier in nos compiles to nothing. The CPU keeps loads in order with loads and stores with stores, and DMA is coherent with the caches, so a barrier only has to stop the compiler. Put one in the wrong place, or pick the wrong kind, and x86 does not notice. On arm64 every barrier is a real instruction with a precise meaning, and the wrong one is a bug waiting for the right timing.

nos got its arm64 port on July 16 and 17. By the evening of the 17th it booted on four CPUs, the smoke tests were green on both architectures, and PCIe interrupts worked. Then we reviewed the whole port, and one commit ([7408805](https://github.com/irqlevel/nos/commit/7408805)) fixed about 25 bugs. Here are the most instructive of them, plus one fixed just before the review, and why a green boot showed none of them.

## Why arm64, and how the port went in

The reason was the development loop. We develop nos on an Apple Silicon Mac, where x86 nos runs under QEMU's TCG, which emulates the CPU in software. arm64 nos runs under HVF, Apple's hypervisor framework, on the Mac's own cores. The arm64 smoke boot took 16 seconds under TCG and 4 under HVF ([4171bc2](https://github.com/irqlevel/nos/commit/4171bc2)), and that gain is why the roadmap moved arm64 ahead of the hypervisor ([the plan](https://github.com/irqlevel/nos/blob/master/plans/02-hal-arm64.md)).

First the x86 code moved behind a HAL, a hardware abstraction layer, without changing what it did. x86 had to pass a new boot smoke test ([4fe3796](https://github.com/irqlevel/nos/commit/4fe3796)) after every step. The x86-specific code moved to `src/cpp/arch/x86_64/`, and common code switched to the headers in `src/cpp/hal/`, each of which picks its backend at compile time ([mmu.h](https://github.com/irqlevel/nos/blob/master/src/cpp/hal/mmu.h)):

```cpp
#if defined(__x86_64__)
#include <arch/x86_64/hal_mmu_inline.h>
#elif defined(__aarch64__)
#include <arch/arm64/hal_mmu_inline.h>
#else
#error "unsupported architecture"
#endif
```

Then came the arm64 side, in six milestones: "hello" on the serial port, memory management, GICv3 (Arm's interrupt controller), SMP through PSCI (the firmware interface that starts the other CPUs), virtio over MMIO, and a smoke test of its own. Rust, PCIe with MSI, [W^X](/blog/wx-everywhere), per-CPU timers and broadcast TLB invalidation followed, and the last of them merged on July 17 ([2971f3a](https://github.com/irqlevel/nos/commit/2971f3a)).

## A barrier no x86 test can check

The x86 kernel had one barrier macro, `Barrier()`: an empty `asm` with a memory clobber, which only stops the compiler. Commit [be3767b](https://github.com/irqlevel/nos/commit/be3767b) replaced every use of it with a barrier that says what it orders, then deleted the macro, so new code has to choose ([barrier.h](https://github.com/irqlevel/nos/blob/master/src/cpp/hal/barrier.h)):

```cpp
#if defined(__x86_64__)
static inline __attribute__((always_inline)) void SmpWmb() { CompilerBarrier(); }
static inline __attribute__((always_inline)) void DmaWmb() { CompilerBarrier(); }
// ...
#elif defined(__aarch64__)
static inline __attribute__((always_inline)) void SmpWmb() { __asm__ __volatile__("dmb ishst" : : : "memory"); }
static inline __attribute__((always_inline)) void DmaWmb() { __asm__ __volatile__("dmb oshst" : : : "memory"); }
```

`Smp*` orders memory between CPUs, `Dma*` between a CPU and a device, and `dmb` is arm64's data memory barrier. The header says it plainly: "classifying a site never changes x86 codegen". That is the catch: every choice was made on x86, where the right barrier and the wrong one give the same bytes. They have to be checked by reading them with arm64's rules in mind, which is what the review did.

## A fence on the wrong side of the check

The first bug sat in the virtio queue, the ring through which a virtio disk or network card returns finished requests. The device writes an entry into the "used" ring, then increments the ring's index. The driver compares that index with its own counter and, if it has moved, reads the entry. There was a read barrier before the comparison, and the review added one after it ([7408805](https://github.com/irqlevel/nos/commit/7408805)):

```cpp
bool VirtQueue::GetUsed(u32& id, u32& len)
{
    Hal::DmaRmb();
    if (LastUsedIdx == Used->Idx)
        return false;

    /* Order the Idx load above before the Ring/payload loads below: usedIdx
       derives from LastUsedIdx (no address dependency), and a control
       dependency does not order load->load on arm64 — without this fence a
       fresh Idx can pair with a stale Ring entry. Also fences the callers'
       reads of device-written buffers (status bytes, RX data) that follow. */
    Hal::DmaRmb();

    u16 usedIdx = LastUsedIdx % QueueSize;
    id = Used->Ring[usedIdx].Id;
    len = Used->Ring[usedIdx].Len;
```

The first barrier keeps earlier loads ahead of the index load, but says nothing about the index load and the ring loads, which both come after it. The ring loads take their slot from `LastUsedIdx`, the driver's own counter, not from the index just read, so no address dependency makes the CPU wait. All that links them is the `if`, a control dependency. On arm64 that orders a load before later stores, but not before later loads: the CPU predicts the branch and issues the next load early. So a fresh index can pair with a stale ring entry. The same goes for whatever the caller reads next, such as a disk request's status byte or a received frame.

x86 never reorders a load with another load, so this cannot happen there. Two Rust drivers had the same pattern. NVMe's `read_cqe` checked a completion entry's phase bit, which the controller flips on every pass around the queue, and then read the entry. r8168's `harvest` checked a receive descriptor's OWN bit, which says whether the NIC or the driver holds it, and then handed out a frame whose address came from a shadow array. Both now call `kcore::barrier::dma_rmb()` right after the check ([nvme](https://github.com/irqlevel/nos/blob/master/src/rust/drivers/nvme/src/queue.rs), [r8168](https://github.com/irqlevel/nos/blob/master/src/rust/drivers/r8168/src/desc.rs)).

## The right instruction for the wrong audience

The second bug was a barrier that did emit an instruction, just the wrong one. The day before, the Rust NVMe driver had gained a fence before its doorbell write, and r8168's descriptor publishes had their compiler fences upgraded ([d6983cd](https://github.com/irqlevel/nos/commit/d6983cd)). All of them were `core::sync::atomic::fence(Ordering::Release)`: free on x86 and a `dmb` on arm64, as the commit said. But the `dmb` that LLVM emits for an atomic fence is `dmb ish`.

The `ish` names a shareability domain. Arm groups everything that observes memory into domains, and a barrier promises an order only to observers in the domain it names. The inner-shareable domain is typically the CPUs that run one operating system. The outer-shareable domain is wider and can include other bus masters, such as a PCIe device doing DMA. So `dmb ish` ordered the NVMe command before the doorbell as other CPUs see it, and promised nothing to the controller that actually reads it.

The fix is a new module, `kcore::barrier`, the Rust half of `Hal::DmaWmb` and `Hal::DmaRmb` ([barrier.rs](https://github.com/irqlevel/nos/blob/master/src/rust/kcore/src/barrier.rs)):

```rust
pub fn dma_wmb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dmb oshst", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
```

`dmb oshst` orders stores for the outer-shareable domain, and the `dmb oshld` in `dma_rmb` does the same for loads. The NVMe doorbell and both r8168 publishes now use it:

```diff
-        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
+        kcore::barrier::dma_wmb();
```

## A TLBI that is allowed to do nothing

TLBI is the arm64 instruction that removes entries from the TLB, the CPU's cache of address translations. Its `is` forms broadcast the invalidation to every CPU in the inner-shareable domain. So on the afternoon of the 17th, arm64 stopped sending IPIs for TLB shootdowns ([e531dc3](https://github.com/irqlevel/nos/commit/e531dc3), [Paging](/docs/paging#tlb-shootdown-and-the-deadlock-it-can-cause)), and one instruction in `Hal::TlbFlushPage` became the whole shootdown of a page.

The operand of `tlbi vaae1is` carries the page number, VA[55:12], in bits 43:0. ARMv8.4 added FEAT_TTL, which makes bits 47:44 a hint: the page size and table level of the entry being removed, so the CPU can narrow the search. Zero means no hint. The code passed `virtAddr >> 12`, and kernel addresses in nos start with 0xFFFF8, so the shift left 0b1111 in the hint field. That reads as "64 KiB pages, level 3", and nos uses 4 KiB pages. When the hint does not match the entry, the architecture allows the CPU to invalidate nothing. The fix is a mask ([hal_mmu_inline.h](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/arm64/hal_mmu_inline.h)):

```cpp
asm volatile(
    "dsb ishst\n"
    "tlbi vaae1is, %0\n"
    "dsb ish\n"
    "isb\n"
    :: "r"((virtAddr >> 12) & ((1UL << 44) - 1)) : "memory");
```

QEMU's TCG ignores the hint, so no TCG boot could have shown this, CI's included. Under HVF the instruction runs on the Mac's own core, and the comment above the fix names Apple cores as ones where it bites. A missed invalidation does not fail on the spot. It leaves a CPU able to reach a page through a mapping the kernel has already removed: memory corruption that passes in silence.

## The interrupt that was never finished

One bug was fixed hours before the review, in the merged branch. It is the clearest case of a failure that looks like success.

In GICv3 an interrupt's number is its INTID. Numbers up to 1019 cover IPIs, per-CPU interrupts such as the timer (27) and device interrupts, and 1020 to 1023 are special values meaning "nothing to acknowledge". PCIe MSIs go through the ITS, the Interrupt Translation Service. A device writes its event number to an ITS register, and the ITS turns the device and the event into an LPI, an interrupt numbered 8192 or higher ([Interrupts](/docs/interrupts#arm64-gicv3-and-the-its)).

The ITS went in on July 17 as opt-in, with an open issue in its commit message: once NVMe armed its MSI-X vector, "the GIC stops delivering interrupts (the timer tick and the LPI both stop)" ([7c37fb2](https://github.com/irqlevel/nos/commit/7c37fb2)). The cause was one comparison in `ArmIrqEntry`, the loop that acknowledges interrupts by reading `ICC_IAR1_EL1`:

```cpp
u32 intId = Gic::ReadIar();
if (intId >= Gic::SpuriousIntId)
{
    /* An empty first read is a real spurious interrupt; an empty
       re-read is just the dispatch loop draining */
    if (!handled)
        InterruptStats::Inc(IrqSpurious);
    return;
}
```

`SpuriousIntId` is 1020. The check was meant for the four special values, but every LPI is above 1020 too. So the loop acknowledged the NVMe interrupt, treated it as spurious and returned without writing `ICC_EOIR1_EL1`. Acknowledging an interrupt raises the CPU's running priority to that interrupt's, and only the end-of-interrupt write lowers it again. Every interrupt in nos has the same priority, 0xA0, so from then on nothing could interrupt that CPU, not even the timer. The boot still printed `boot: complete`, because that path is synchronous. The shell task was just never scheduled again.

The fix bounds the check to the special range ([bba6446](https://github.com/irqlevel/nos/commit/bba6446)):

```cpp
if (intId >= Gic::SpuriousIntId && intId < Its::LpiIntIdBase)
```

After it, each NVMe I/O produced exactly one LPI 8192, and the ITS became the default. The review found more in the ITS. Its tables and command queue had been declared inner non-cacheable (`1<<59`) instead of write-back (`0x7<<59`). And `Its::WaitCommands` logged a stalled command queue, then returned as if the command had run. Now a stall is an error, so `MapEvent` no longer hands a driver an interrupt that will never come.

## What QEMU forgave

The review sorted its findings into three groups. The first holds the bugs that bite on QEMU and HVF today: the barriers, the TLBI, and the boot identity map that left kernel text writable and executable at its physical address ([W^X everywhere](/blog/wx-everywhere)). The last holds latent bugs, such as a page allocator that handed out the device tree's memory as free pages. The group in the middle, bugs that real hardware would hit but QEMU tolerates, reads like a list of what QEMU quietly does for you. Two examples:

- QEMU's PSCI preserves registers x4 to x17 across a firmware call. The calling convention lets firmware clobber them, and TF-A (Trusted Firmware-A) does, so `PsciCall` now declares them clobbered ([cpu_start_arm64.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/arm64/cpu_start_arm64.cpp)).
- QEMU numbers CPUs one after another in the lowest affinity field of MPIDR, the register that identifies a CPU, and `SendSgi` took that field for the CPU's index. On hardware with clusters an IPI would reach the wrong CPU, so the affinity now comes from the device tree's CPU list. A `dsb ishst` before the send also makes the work queued for the target visible before the IPI arrives ([gicv3.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/arm64/gicv3.cpp)).

## What stays

None of this could have shown up on x86. Its DMA barriers are compiler barriers, its `invlpg` takes no hint, and it has no GIC. The smoke tests were green before the review and after it, on x86 and on arm64 under both TCG and HVF.

What a boot cannot check, a rule has to. The same commit wrote the lessons into the project's rules ([CLAUDE.md](https://github.com/irqlevel/nos/blob/master/CLAUDE.md)):

> `core::sync::atomic::fence` is NOT a substitute: it emits `dmb ish`, whose inner-shareable domain does not order against a PCIe master. Consuming device-written state needs a read barrier *after* the index/phase/OWN check and before the payload reads

Part of it is now in the code. With `Barrier()` gone, every new barrier has to say what it orders. With `kcore::barrier`, the right DMA barrier in Rust is also the shortest one to write. The read barrier after the check is still something a driver author has to know, so each of the three places that needed one carries a comment explaining why.
