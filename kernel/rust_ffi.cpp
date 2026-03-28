#include "trace.h"
#include "panic.h"
#include "time.h"
#include "mutex.h"
#include "spin_lock.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "entropy.h"
#include <mm/new.h>
#include <mm/page_allocator.h>
#include <mm/page_table.h>
#include <lib/stdlib.h>

static const ulong RustAllocTag = 'rust';

extern "C" {

void kernel_trace(unsigned int level, const unsigned char* msg, unsigned long len)
{
    char buf[256];
    unsigned long n = (len < sizeof(buf) - 1) ? len : sizeof(buf) - 1;
    Stdlib::MemCpy(buf, msg, n);
    buf[n] = '\0';
    auto time = Kernel::GetBootTime();
    Kernel::Tracer::GetInstance().Output("%u:%u.%06u:rust: %s\n",
        level, time.GetSecs(), time.GetUsecs(), buf);
}

void* kernel_alloc(unsigned long size, unsigned long align)
{
    if (align == 0 || (align & (align - 1)) != 0 || align > 8)
        Panic("kernel_alloc: bad align %u", align);
    return Kernel::Mm::Alloc(size, RustAllocTag);
}

void kernel_free(void* ptr)
{
    Kernel::Mm::Free(ptr);
}

[[noreturn]] void kernel_panic(const unsigned char* msg, unsigned long len)
{
    char buf[256];
    unsigned long n = (len < sizeof(buf) - 1) ? len : sizeof(buf) - 1;
    Stdlib::MemCpy(buf, msg, n);
    buf[n] = '\0';
    Kernel::Panicker::GetInstance().DoPanic("RUST PANIC: %s\n", buf);
    for (;;) {}
}

void kernel_get_boot_time(unsigned long* secs, unsigned long* usecs)
{
    if (!secs || !usecs)
        return;
    auto t = Kernel::GetBootTime();
    *secs = t.GetSecs();
    *usecs = t.GetUsecs();
}

unsigned long kernel_get_wall_time_secs()
{
    return Kernel::GetWallTimeSecs();
}

unsigned long kernel_mutex_create()
{
    Kernel::Mutex* m = Kernel::Mm::TAlloc<Kernel::Mutex, RustAllocTag>();
    return (unsigned long)m;
}

void kernel_mutex_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    Kernel::Mutex* m = reinterpret_cast<Kernel::Mutex*>(handle);
    m->~Mutex();
    Kernel::Mm::Free(m);
}

void kernel_mutex_lock(unsigned long handle)
{
    reinterpret_cast<Kernel::Mutex*>(handle)->Lock();
}

void kernel_mutex_unlock(unsigned long handle)
{
    reinterpret_cast<Kernel::Mutex*>(handle)->Unlock();
}

unsigned long kernel_spinlock_create()
{
    Kernel::SpinLock* s = Kernel::Mm::TAlloc<Kernel::SpinLock, RustAllocTag>();
    return (unsigned long)s;
}

void kernel_spinlock_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    Kernel::SpinLock* s = reinterpret_cast<Kernel::SpinLock*>(handle);
    s->~SpinLock();
    Kernel::Mm::Free(s);
}

unsigned long long kernel_spinlock_lock(unsigned long handle)
{
    unsigned long flags = 0;
    reinterpret_cast<Kernel::SpinLock*>(handle)->Lock(flags);
    return (unsigned long long)flags;
}

void kernel_spinlock_unlock(unsigned long handle, unsigned long long flags)
{
    reinterpret_cast<Kernel::SpinLock*>(handle)->Unlock((unsigned long)flags);
}

unsigned long kernel_task_spawn(void (*func)(void*), void* ctx)
{
    Kernel::Task* t = Kernel::Mm::TAlloc<Kernel::Task, RustAllocTag>("rust");
    if (!t)
        return 0;
    if (!t->Start(func, ctx))
    {
        t->Put();
        return 0;
    }
    return (unsigned long)t;
}

void kernel_task_wait(unsigned long handle)
{
    reinterpret_cast<Kernel::Task*>(handle)->Wait();
}

void kernel_task_set_stopping(unsigned long handle)
{
    reinterpret_cast<Kernel::Task*>(handle)->SetStopping();
}

void kernel_task_put(unsigned long handle)
{
    reinterpret_cast<Kernel::Task*>(handle)->Put();
}

void kernel_sleep_ns(unsigned long long ns)
{
    Kernel::Sleep((ulong)ns);
}

unsigned int kernel_get_cpu_id()
{
    return (unsigned int)Kernel::GetCpu().GetIndex();
}

void* kernel_alloc_dma_pages(unsigned long count,
    unsigned long* phys_out, unsigned long* actual_pages_out)
{
    if (count == 0 || !phys_out || !actual_pages_out)
        return nullptr;

    unsigned long phys = 0;
    void* p = Kernel::Mm::AllocMapPages((size_t)count, &phys);
    if (!p)
    {
        *phys_out = 0;
        *actual_pages_out = 0;
        return nullptr;
    }

    *phys_out = phys;
    *actual_pages_out = (unsigned long)(1UL << Stdlib::Log2((size_t)count));
    return p;
}

void kernel_free_dma_pages(void* ptr)
{
    if (ptr)
        Kernel::Mm::UnmapFreePages(ptr);
}

void* kernel_map_phys(unsigned long phys_base, unsigned long num_pages)
{
    if (num_pages == 0 ||
        num_pages > Kernel::Mm::PageTable::MaxContiguousPages)
        return nullptr;

    unsigned long addrs[Kernel::Mm::PageTable::MaxContiguousPages];
    for (unsigned long i = 0; i < num_pages; i++)
        addrs[i] = phys_base + i * Const::PageSize;

    return Kernel::Mm::MapPages((size_t)num_pages, addrs);
}

void kernel_unmap_phys(void* virt_addr, unsigned long num_pages)
{
    if (virt_addr && num_pages > 0)
        Kernel::Mm::UnmapPages(virt_addr, (size_t)num_pages);
}

int kernel_get_random(unsigned char* buf, unsigned long len)
{
    Kernel::EntropySource* src =
        Kernel::EntropySourceTable::GetInstance().GetDefault();
    if (!src || !buf || len == 0)
        return 0;
    return src->GetRandom(buf, (ulong)len) ? 1 : 0;
}

} /* extern "C" */
