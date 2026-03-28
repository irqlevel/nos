#include "trace.h"
#include "panic.h"
#include "time.h"
#include <mm/new.h>
#include <mm/page_allocator.h>
#include <lib/stdlib.h>

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
    (void)align;
    return Kernel::Mm::Alloc(size, 0);
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

} /* extern "C" */
