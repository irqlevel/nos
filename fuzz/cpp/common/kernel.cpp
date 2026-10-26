// The kernel around the C++ under test, written for the purpose: what its
// sources call that the fuzzers do not compile -- the tracer, the panic
// path, the clock, the locks, the atomics, the string routines the kernel
// has in assembly. Single-threaded, as a fuzzed input runs, and strict where
// the kernel's rules are: a panic is a finding, and so is a lock taken twice,
// let go of when not held, or held when the input ends.
#include "host.h"

#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <hal/mmu.h>
#include <kernel/panic.h>
#include <kernel/raw_spin_lock.h>
#include <kernel/spin_lock.h>
#include <kernel/time.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>

#include "fuzz.h"
#include "kernel.h"

#include <stdarg.h>
#include <stdio.h>
#include <string.h>

namespace fuzz
{

namespace
{

/* Spin locks held right now, by the only task there is. */
long spin_locks_held;

/* The kernel's clock: it moves a microsecond each time it is read. */
ulong clock_ns;

}

/* The host HAL's state (fuzz/cpp/host). */
bool interrupts_on = true;
u64 cycles;
ulong current_cpu;

void host_hal_unreachable(const char* what)
{
    finding(__FILE__, __LINE__, "%s: nothing the fuzzed code may call", what);
}

/* No MMU to tell, unless the target models one (targets/mm.cpp). */
__attribute__((weak)) void host_tlb_flush_page(ulong virt_addr)
{
    (void)virt_addr;
}

__attribute__((weak)) void host_tlb_flush_all()
{
}

__attribute__((weak)) void host_pte_made_valid()
{
}

__attribute__((weak)) ulong host_get_translation_root()
{
    return 0;
}

__attribute__((weak)) void host_set_translation_root(ulong phys)
{
    (void)phys;
}

/* The kernel heap's, when the target has heap.cpp's. */
__attribute__((weak)) void heap_reset()
{
}

void reset_kernel()
{
    spin_locks_held = 0;
    interrupts_on = true;
    current_cpu = 0;
    heap_reset();
}

long held_spin_locks()
{
    return spin_locks_held;
}

void check_no_locks_held(const char* when)
{
    INVARIANT(spin_locks_held == 0, "%ld spin locks still held %s", spin_locks_held, when);
}

/* The kernel's own formatting, as its tracer and panic path do it. */
static void format(char* buf, size_t size, const char* fmt, va_list args)
{
    stdlib::vsn_printf(buf, size, fmt, args);
}

}

/* ---- the string routines, which the kernel has in assembly ---- */

extern "C" {

/* The kernel image's bounds, which the linker script gives the kernel; a
   target that places a kernel image of its own defines them (mm.cpp). */
__attribute__((weak)) char kernel_start;
__attribute__((weak)) char kernel_end;

void asm_memset(void* p, unsigned char c, size_t n)
{
    memset(p, c, n);
}

void asm_memcpy(void* d, const void* s, size_t n)
{
    memcpy(d, s, n);
}

void asm_memmove(void* d, const void* s, size_t n)
{
    memmove(d, s, n);
}

int asm_memcmp(const void* a, const void* b, size_t n)
{
    return memcmp(a, b, n);
}

size_t asm_strlen(const char* s)
{
    return strlen(s);
}

int asm_strcmp(const char* a, const char* b)
{
    return strcmp(a, b);
}

const char* asm_strstr(const char* h, const char* n)
{
    return strstr(h, n);
}

/* ---- the atomics of hal/cpu.h, which each arch has in assembly ---- */

void atomic_inc(volatile long* p)
{
    __atomic_add_fetch(p, 1, __ATOMIC_SEQ_CST);
}

void atomic_dec(volatile long* p)
{
    __atomic_sub_fetch(p, 1, __ATOMIC_SEQ_CST);
}

void atomic_add(volatile long* p, long delta)
{
    __atomic_add_fetch(p, delta, __ATOMIC_SEQ_CST);
}

long atomic_read(volatile long* p)
{
    return __atomic_load_n(p, __ATOMIC_SEQ_CST);
}

void atomic_write(volatile long* p, long v)
{
    __atomic_store_n(p, v, __ATOMIC_SEQ_CST);
}

long atomic_read_and_dec(volatile long* p)
{
    return __atomic_fetch_sub(p, 1, __ATOMIC_SEQ_CST);
}

long atomic_read_and_inc(volatile long* p)
{
    return __atomic_fetch_add(p, 1, __ATOMIC_SEQ_CST);
}

long atomic_cmpxchg(volatile long* p, long exchange, long comparand)
{
    long expected = comparand;
    __atomic_compare_exchange_n(p, &expected, exchange, false, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    return expected;
}

long atomic_test_and_set_bit(volatile long* p, ulong bit)
{
    long mask = 1L << bit;
    return (__atomic_fetch_or(p, mask, __ATOMIC_SEQ_CST) & mask) != 0;
}

long atomic_test_and_clear_bit(volatile long* p, ulong bit)
{
    long mask = 1L << bit;
    return (__atomic_fetch_and(p, ~mask, __ATOMIC_SEQ_CST) & mask) != 0;
}

long atomic_test_bit(volatile long* p, ulong bit)
{
    return (__atomic_load_n(p, __ATOMIC_SEQ_CST) >> bit) & 1;
}

}

namespace kernel
{

/* ---- the tracer: its lines on stderr under --trace ---- */

Tracer::Tracer()
    : level_(0)
    , console_suppressed_(false)
{
}

Tracer::~Tracer()
{
}

void Tracer::output(const char* fmt, ...)
{
    char line[1024];
    va_list args;
    va_start(args, fmt);
    fuzz::format(line, sizeof(line), fmt, args);
    va_end(args);
    if (fuzz::tracing())
        fputs(line, stderr);
}

void Tracer::output(stdlib::Error& err, const char* fmt, ...)
{
    (void)err;
    char line[1024];
    va_list args;
    va_start(args, fmt);
    fuzz::format(line, sizeof(line), fmt, args);
    va_end(args);
    if (fuzz::tracing())
        fputs(line, stderr);
}

void Tracer::set_level(int level)
{
    level_ = level;
}

int Tracer::get_level()
{
    /* Every line formatted, whatever it would have been: a trace a level
       hides is still a call whose arguments must be sound. */
    return MAX_TRACE_LEVEL;
}

void Tracer::set_console_suppressed(bool suppressed)
{
    console_suppressed_ = suppressed;
}

bool Tracer::is_console_suppressed()
{
    return console_suppressed_;
}

/* ---- the panic path: a finding, at the place the panic names ---- */

Panicker::Panicker()
{
}

Panicker::~Panicker()
{
}

void Panicker::do_panic(const char* fmt, ...)
{
    char msg[1024];
    va_list args;
    va_start(args, fmt);
    fuzz::format(msg, sizeof(msg), fmt, args);
    va_end(args);

    /* "PANIC:func():file,line: what" -- the place is file:line */
    char file[256] = "panic";
    int line = 0;
    const char* p = strstr(msg, "():");
    if (p != nullptr)
    {
        p += 3;
        const char* comma = strchr(p, ',');
        if (comma != nullptr && static_cast<size_t>(comma - p) < sizeof(file))
        {
            memcpy(file, p, comma - p);
            file[comma - p] = '\0';
            line = atoi(comma + 1);
        }
    }
    size_t n = strlen(msg);
    while (n > 0 && msg[n - 1] == '\n')
        msg[--n] = '\0';
    fuzz::finding(file, line, "kernel panic: %s", msg);
}

void Panicker::do_panic_ctx(Context* ctx, bool has_error_code, const char* fmt, ...)
{
    (void)ctx;
    (void)has_error_code;
    char msg[1024];
    va_list args;
    va_start(args, fmt);
    fuzz::format(msg, sizeof(msg), fmt, args);
    va_end(args);
    fuzz::finding("panic", 0, "kernel panic: %s", msg);
}

bool Panicker::is_active()
{
    return false;
}

/* ---- the clock ---- */

stdlib::Time get_boot_time()
{
    fuzz::clock_ns += 1000;
    return stdlib::Time(fuzz::clock_ns);
}

/* ---- the locks: one task, and the rules checked ---- */

RawSpinLock::RawSpinLock(bool watched)
    : value_(0)
    , watched_(watched)
    , preempt_task_(nullptr)
    , watchdog_reported_(0)
    , watchdog_lock_time_(0)
{
    watchdog_list_entry_.init();
}

RawSpinLock::~RawSpinLock()
{
}

void RawSpinLock::lock()
{
    INVARIANT(value_.get() == 0, "a spin lock taken by the task that holds it: a deadlock");
    value_.set(1);
    fuzz::spin_locks_held++;
}

void RawSpinLock::unlock()
{
    INVARIANT(value_.get() == 1, "a spin lock let go of that is not held");
    value_.set(0);
    fuzz::spin_locks_held--;
}

bool RawSpinLock::try_lock()
{
    if (value_.get() != 0)
        return false;
    value_.set(1);
    fuzz::spin_locks_held++;
    return true;
}

ulong RawSpinLock::lock_irq_save()
{
    lock();
    return 0;
}

void RawSpinLock::unlock_irq_restore(ulong flags)
{
    (void)flags;
    unlock();
}

ulong RawSpinLock::try_lock_irq_save(bool& acquired)
{
    acquired = try_lock();
    return 0;
}

SpinLock::SpinLock()
    : raw_lock_(true)
    , owner_(nullptr)
{
}

SpinLock::~SpinLock()
{
}

/* The owner is the one task there is: the lock's own address stands for it */
void SpinLock::lock()
{
    raw_lock_.lock();
    owner_ = this;
}

void SpinLock::unlock()
{
    owner_ = nullptr;
    raw_lock_.unlock();
}

void SpinLock::lock(ulong& flags)
{
    flags = raw_lock_.lock_irq_save();
    owner_ = this;
}

void SpinLock::unlock(ulong flags)
{
    owner_ = nullptr;
    raw_lock_.unlock_irq_restore(flags);
}

void SpinLock::shared_lock(ulong& flags)
{
    lock(flags);
}

void SpinLock::shared_unlock(ulong flags)
{
    unlock(flags);
}

void SpinLock::unwatch()
{
}

}
