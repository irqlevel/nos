#include <hal/cpu.h>

/* arm64 implementations of the portable extern "C" CPU primitives that
   x86 provides in arch/x86_64/asm.asm. Atomics use the __atomic builtins
   (seq_cst), which lower to inline LDXR/STXR loops with
   -mno-outline-atomics. SwitchContext/SetJmp/LongJmp live in asm.S. */

extern "C" void Arm64TaskEntryThunk();
extern "C" void __attribute__((noreturn)) Arm64RunOnStack(ulong stackTop, void (*fn)(void*), void* ctx);

namespace Hal
{

/* Frame layout must match SwitchContext in asm.S:
   [DAIF, pad][x19,x20][x21,x22][x23,x24][x25,x26][x27,x28][x29,x30] */
ulong BuildTaskFrame(ulong stackTop, ulong entry, ulong arg)
{
    ulong* sp = (ulong*)(stackTop & ~0xFUL);
    sp -= 14;
    for (int i = 0; i < 14; i++)
        sp[i] = 0;
    /* DAIF: IRQs enabled, D/A/F stay masked — the PSTATE every other
       context runs with (boot masks all four, InterruptEnable clears only
       I), so SError delivery does not depend on which task is current */
    const ulong DaifDAF = (1UL << 9) | (1UL << 8) | (1UL << 6);
    sp[0] = DaifDAF;
    sp[2] = arg;                        /* x19 */
    sp[3] = entry;                      /* x20 */
    sp[13] = (ulong)&Arm64TaskEntryThunk; /* x30 */
    return (ulong)sp;
}

ulong TaskSavedFramePointer(ulong savedSp)
{
    /* SwitchContext frame slot 12 = x29 (see asm.S layout comment) */
    return ((ulong*)savedSp)[12];
}

void RunOnStack(ulong stackTop, void (*fn)(void*), void* ctx)
{
    Arm64RunOnStack(stackTop, fn, ctx);
}

}

extern "C"
{

void Pause(void)
{
    asm volatile("yield");
}

void Hlt(void)
{
    asm volatile("wfi");
}

void InterruptEnable(void)
{
    asm volatile("msr daifclr, #2" ::: "memory");
}

void InterruptDisable(void)
{
    asm volatile("msr daifset, #2" ::: "memory");
}

void SpinLockLock(ulong *lock)
{
    for (;;)
    {
        ulong expected = 0;
        if (__atomic_compare_exchange_n(lock, &expected, 1, false,
                                        __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
            return;
        while (__atomic_load_n(lock, __ATOMIC_RELAXED) != 0)
            asm volatile("yield");
    }
}

void SpinLockUnlock(ulong *lock)
{
    __atomic_store_n(lock, 0, __ATOMIC_RELEASE);
}

void AtomicInc(volatile long *pvalue)
{
    __atomic_add_fetch(pvalue, 1, __ATOMIC_SEQ_CST);
}

void AtomicDec(volatile long *pvalue)
{
    __atomic_sub_fetch(pvalue, 1, __ATOMIC_SEQ_CST);
}

void AtomicAdd(volatile long *pvalue, long delta)
{
    __atomic_add_fetch(pvalue, delta, __ATOMIC_SEQ_CST);
}

long AtomicRead(volatile long *pvalue)
{
    return __atomic_load_n(pvalue, __ATOMIC_SEQ_CST);
}

void AtomicWrite(volatile long *pvalue, long newValue)
{
    __atomic_store_n(pvalue, newValue, __ATOMIC_SEQ_CST);
}

long AtomicReadAndDec(volatile long *pvalue)
{
    return __atomic_fetch_sub(pvalue, 1, __ATOMIC_SEQ_CST);
}

long AtomicReadAndInc(volatile long *pvalue)
{
    return __atomic_fetch_add(pvalue, 1, __ATOMIC_SEQ_CST);
}

long AtomicCmpxchg(volatile long *pvalue, long exchange, long comparand)
{
    long expected = comparand;
    __atomic_compare_exchange_n(pvalue, &expected, exchange, false,
                                __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    return expected; /* original value: comparand on success */
}

long AtomicTestAndSetBit(volatile long *pvalue, ulong bit)
{
    long mask = 1L << bit;
    long old = __atomic_fetch_or(pvalue, mask, __ATOMIC_SEQ_CST);
    return (old & mask) ? 1 : 0;
}

long AtomicTestAndClearBit(volatile long *pvalue, ulong bit)
{
    long mask = 1L << bit;
    long old = __atomic_fetch_and(pvalue, ~mask, __ATOMIC_SEQ_CST);
    return (old & mask) ? 1 : 0;
}

long AtomicTestBit(volatile long *pvalue, ulong bit)
{
    long mask = 1L << bit;
    return (__atomic_load_n(pvalue, __ATOMIC_SEQ_CST) & mask) ? 1 : 0;
}

}
