#include <hal/cpu.h>

/* arm64 implementations of the portable extern "C" CPU primitives that
   x86 provides in arch/x86_64/asm.asm. Atomics use the __atomic builtins
   (seq_cst), which lower to inline LDXR/STXR loops with
   -mno-outline-atomics. switch_context/set_jmp/long_jmp live in asm.S. */

extern "C" void arm64_task_entry_thunk();
extern "C" void __attribute__((noreturn)) arm64_run_on_stack(ulong stack_top, void (*fn)(void*), void* ctx);

namespace hal
{

/* switch_context's frame in asm.S, in words, and the one x29 is saved in */
static const ulong SWITCH_FRAME_WORDS = 14;
static const ulong SWITCH_FRAME_FP = 12;

/* Frame layout must match switch_context in asm.S:
   [DAIF, pad][x19,x20][x21,x22][x23,x24][x25,x26][x27,x28][x29,x30] */
ulong build_task_frame(ulong stack_top, ulong entry, ulong arg)
{
    ulong* sp = (ulong*)(stack_top & ~0xFUL);
    sp -= SWITCH_FRAME_WORDS;
    for (ulong i = 0; i < SWITCH_FRAME_WORDS; i++)
        sp[i] = 0;
    /* DAIF: IRQs enabled, D/A/F stay masked — the PSTATE every other
       context runs with (boot masks all four, interrupt_enable clears only
       I), so SError delivery does not depend on which task is current */
    const ulong daif_daf = (1UL << 9) | (1UL << 8) | (1UL << 6);
    sp[0] = daif_daf;
    sp[2] = arg;                        /* x19 */
    sp[3] = entry;                      /* x20 */
    sp[13] = (ulong)&arm64_task_entry_thunk; /* x30 */
    return (ulong)sp;
}

ulong task_saved_frame_pointer(ulong saved_sp, ulong stack_base, ulong stack_limit)
{
    if (saved_sp % sizeof(ulong) != 0 || saved_sp < stack_base || saved_sp > stack_limit ||
        stack_limit - saved_sp < SWITCH_FRAME_WORDS * sizeof(ulong))
        return 0;

    /* switch_context frame slot 12 = x29 (see asm.S layout comment) */
    return ((ulong*)saved_sp)[SWITCH_FRAME_FP];
}

void run_on_stack(ulong stack_top, void (*fn)(void*), void* ctx)
{
    arm64_run_on_stack(stack_top, fn, ctx);
}

}

extern "C"
{

void cpu_relax(void)
{
    asm volatile("yield");
}

void cpu_halt(void)
{
    asm volatile("wfi");
}

void interrupt_enable(void)
{
    asm volatile("msr daifclr, #2" ::: "memory");
}

void interrupt_disable(void)
{
    asm volatile("msr daifset, #2" ::: "memory");
}

void spin_lock_lock(ulong *lock)
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

void spin_lock_unlock(ulong *lock)
{
    __atomic_store_n(lock, 0, __ATOMIC_RELEASE);
}

void atomic_inc(volatile long *pvalue)
{
    __atomic_add_fetch(pvalue, 1, __ATOMIC_SEQ_CST);
}

void atomic_dec(volatile long *pvalue)
{
    __atomic_sub_fetch(pvalue, 1, __ATOMIC_SEQ_CST);
}

void atomic_add(volatile long *pvalue, long delta)
{
    __atomic_add_fetch(pvalue, delta, __ATOMIC_SEQ_CST);
}

long atomic_read(volatile long *pvalue)
{
    return __atomic_load_n(pvalue, __ATOMIC_SEQ_CST);
}

void atomic_write(volatile long *pvalue, long new_value)
{
    __atomic_store_n(pvalue, new_value, __ATOMIC_SEQ_CST);
}

long atomic_read_and_dec(volatile long *pvalue)
{
    return __atomic_fetch_sub(pvalue, 1, __ATOMIC_SEQ_CST);
}

long atomic_read_and_inc(volatile long *pvalue)
{
    return __atomic_fetch_add(pvalue, 1, __ATOMIC_SEQ_CST);
}

long atomic_cmpxchg(volatile long *pvalue, long exchange, long comparand)
{
    long expected = comparand;
    __atomic_compare_exchange_n(pvalue, &expected, exchange, false,
                                __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    return expected; /* original value: comparand on success */
}

long atomic_test_and_set_bit(volatile long *pvalue, ulong bit)
{
    long mask = 1L << bit;
    long old = __atomic_fetch_or(pvalue, mask, __ATOMIC_SEQ_CST);
    return (old & mask) ? 1 : 0;
}

long atomic_test_and_clear_bit(volatile long *pvalue, ulong bit)
{
    long mask = 1L << bit;
    long old = __atomic_fetch_and(pvalue, ~mask, __ATOMIC_SEQ_CST);
    return (old & mask) ? 1 : 0;
}

long atomic_test_bit(volatile long *pvalue, ulong bit)
{
    long mask = 1L << bit;
    return (__atomic_load_n(pvalue, __ATOMIC_SEQ_CST) & mask) ? 1 : 0;
}

}
