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

namespace Fuzz
{

namespace
{

/* Spin locks held right now, by the only task there is. */
long SpinLocksHeld;

/* The kernel's clock: it moves a microsecond each time it is read. */
ulong ClockNs;

}

/* The host HAL's state (fuzz/cpp/host). */
bool InterruptsOn = true;
u64 Cycles;
ulong CurrentCpu;

void HostHalUnreachable(const char* what)
{
    Finding(__FILE__, __LINE__, "%s: nothing the fuzzed code may call", what);
}

/* No MMU to tell, unless the target models one (targets/mm.cpp). */
__attribute__((weak)) void HostTlbFlushPage(ulong virtAddr)
{
    (void)virtAddr;
}

__attribute__((weak)) void HostTlbFlushAll()
{
}

__attribute__((weak)) void HostPteMadeValid()
{
}

__attribute__((weak)) ulong HostGetTranslationRoot()
{
    return 0;
}

__attribute__((weak)) void HostSetTranslationRoot(ulong phys)
{
    (void)phys;
}

/* The kernel heap's, when the target has heap.cpp's. */
__attribute__((weak)) void HeapReset()
{
}

void ResetKernel()
{
    SpinLocksHeld = 0;
    InterruptsOn = true;
    CurrentCpu = 0;
    HeapReset();
}

long HeldSpinLocks()
{
    return SpinLocksHeld;
}

void CheckNoLocksHeld(const char* when)
{
    INVARIANT(SpinLocksHeld == 0, "%ld spin locks still held %s", SpinLocksHeld, when);
}

/* The kernel's own formatting, as its tracer and panic path do it. */
static void Format(char* buf, size_t size, const char* fmt, va_list args)
{
    Stdlib::VsnPrintf(buf, size, fmt, args);
}

}

/* ---- the string routines, which the kernel has in assembly ---- */

extern "C" {

/* The kernel image's bounds, which the linker script gives the kernel; a
   target that places a kernel image of its own defines them (mm.cpp). */
__attribute__((weak)) char KernelStart;
__attribute__((weak)) char KernelEnd;

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

void AtomicInc(volatile long* p)
{
    __atomic_add_fetch(p, 1, __ATOMIC_SEQ_CST);
}

void AtomicDec(volatile long* p)
{
    __atomic_sub_fetch(p, 1, __ATOMIC_SEQ_CST);
}

void AtomicAdd(volatile long* p, long delta)
{
    __atomic_add_fetch(p, delta, __ATOMIC_SEQ_CST);
}

long AtomicRead(volatile long* p)
{
    return __atomic_load_n(p, __ATOMIC_SEQ_CST);
}

void AtomicWrite(volatile long* p, long v)
{
    __atomic_store_n(p, v, __ATOMIC_SEQ_CST);
}

long AtomicReadAndDec(volatile long* p)
{
    return __atomic_fetch_sub(p, 1, __ATOMIC_SEQ_CST);
}

long AtomicReadAndInc(volatile long* p)
{
    return __atomic_fetch_add(p, 1, __ATOMIC_SEQ_CST);
}

long AtomicCmpxchg(volatile long* p, long exchange, long comparand)
{
    long expected = comparand;
    __atomic_compare_exchange_n(p, &expected, exchange, false, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    return expected;
}

long AtomicTestAndSetBit(volatile long* p, ulong bit)
{
    long mask = 1L << bit;
    return (__atomic_fetch_or(p, mask, __ATOMIC_SEQ_CST) & mask) != 0;
}

long AtomicTestAndClearBit(volatile long* p, ulong bit)
{
    long mask = 1L << bit;
    return (__atomic_fetch_and(p, ~mask, __ATOMIC_SEQ_CST) & mask) != 0;
}

long AtomicTestBit(volatile long* p, ulong bit)
{
    return (__atomic_load_n(p, __ATOMIC_SEQ_CST) >> bit) & 1;
}

}

namespace Kernel
{

/* ---- the tracer: its lines on stderr under --trace ---- */

Tracer::Tracer()
    : Level(0)
    , ConsoleSuppressed(false)
{
}

Tracer::~Tracer()
{
}

void Tracer::Output(const char* fmt, ...)
{
    char line[1024];
    va_list args;
    va_start(args, fmt);
    Fuzz::Format(line, sizeof(line), fmt, args);
    va_end(args);
    if (Fuzz::Tracing())
        fputs(line, stderr);
}

void Tracer::Output(Stdlib::Error& err, const char* fmt, ...)
{
    (void)err;
    char line[1024];
    va_list args;
    va_start(args, fmt);
    Fuzz::Format(line, sizeof(line), fmt, args);
    va_end(args);
    if (Fuzz::Tracing())
        fputs(line, stderr);
}

void Tracer::SetLevel(int level)
{
    Level = level;
}

int Tracer::GetLevel()
{
    /* Every line formatted, whatever it would have been: a trace a level
       hides is still a call whose arguments must be sound. */
    return MaxTraceLevel;
}

void Tracer::SetConsoleSuppressed(bool suppressed)
{
    ConsoleSuppressed = suppressed;
}

bool Tracer::IsConsoleSuppressed()
{
    return ConsoleSuppressed;
}

/* ---- the panic path: a finding, at the place the panic names ---- */

Panicker::Panicker()
{
}

Panicker::~Panicker()
{
}

void Panicker::DoPanic(const char* fmt, ...)
{
    char msg[1024];
    va_list args;
    va_start(args, fmt);
    Fuzz::Format(msg, sizeof(msg), fmt, args);
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
    Fuzz::Finding(file, line, "kernel panic: %s", msg);
}

void Panicker::DoPanicCtx(Context* ctx, bool hasErrorCode, const char* fmt, ...)
{
    (void)ctx;
    (void)hasErrorCode;
    char msg[1024];
    va_list args;
    va_start(args, fmt);
    Fuzz::Format(msg, sizeof(msg), fmt, args);
    va_end(args);
    Fuzz::Finding("panic", 0, "kernel panic: %s", msg);
}

bool Panicker::IsActive()
{
    return false;
}

/* ---- the clock ---- */

Stdlib::Time GetBootTime()
{
    Fuzz::ClockNs += 1000;
    return Stdlib::Time(Fuzz::ClockNs);
}

/* ---- the locks: one task, and the rules checked ---- */

RawSpinLock::RawSpinLock(bool watched)
    : Value(0)
    , Watched(watched)
    , PreemptTask(nullptr)
    , WatchdogReported(0)
    , WatchdogLockTime(0)
{
    WatchdogListEntry.Init();
}

RawSpinLock::~RawSpinLock()
{
}

void RawSpinLock::Lock()
{
    INVARIANT(Value.Get() == 0, "a spin lock taken by the task that holds it: a deadlock");
    Value.Set(1);
    Fuzz::SpinLocksHeld++;
}

void RawSpinLock::Unlock()
{
    INVARIANT(Value.Get() == 1, "a spin lock let go of that is not held");
    Value.Set(0);
    Fuzz::SpinLocksHeld--;
}

bool RawSpinLock::TryLock()
{
    if (Value.Get() != 0)
        return false;
    Value.Set(1);
    Fuzz::SpinLocksHeld++;
    return true;
}

ulong RawSpinLock::LockIrqSave()
{
    Lock();
    return 0;
}

void RawSpinLock::UnlockIrqRestore(ulong flags)
{
    (void)flags;
    Unlock();
}

ulong RawSpinLock::TryLockIrqSave(bool& acquired)
{
    acquired = TryLock();
    return 0;
}

SpinLock::SpinLock()
    : RawLock(true)
    , Owner(nullptr)
{
}

SpinLock::~SpinLock()
{
}

/* The owner is the one task there is: the lock's own address stands for it */
void SpinLock::Lock()
{
    RawLock.Lock();
    Owner = this;
}

void SpinLock::Unlock()
{
    Owner = nullptr;
    RawLock.Unlock();
}

void SpinLock::Lock(ulong& flags)
{
    flags = RawLock.LockIrqSave();
    Owner = this;
}

void SpinLock::Unlock(ulong flags)
{
    Owner = nullptr;
    RawLock.UnlockIrqRestore(flags);
}

void SpinLock::SharedLock(ulong& flags)
{
    Lock(flags);
}

void SpinLock::SharedUnlock(ulong flags)
{
    Unlock(flags);
}

void SpinLock::Unwatch()
{
}

}
