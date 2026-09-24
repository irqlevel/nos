#include "trace.h"
#include "input.h"
#include "panic.h"
#include "time.h"
#include "mutex.h"
#include "wait_group.h"
#include "event.h"
#include "lockless_ring.h"
#include "spin_lock.h"
#include "raw_spin_lock.h"
#include "raw_rw_spin_lock.h"
#include "rw_mutex.h"
#include "task.h"
#include "sched.h"
#include "preempt.h"
#include "cpu.h"
#include "random.h"
#include "entropy.h"
#include "interrupt.h"
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>
#include "softirq.h"
#include "timer.h"
#include "cmd.h"
#include <mm/new.h>
#include <mm/page_allocator.h>
#include <mm/page_table.h>
#include <lib/stdlib.h>
#include <drivers/pci.h>
#include <drivers/msix.h>
#include <hal/irqchip.h>
#include <drivers/hpet.h>
#include <drivers/acpi.h>
#include "parameters.h"
#include "version_gen.h"
#include "dmesg.h"

static const ulong RustAllocTag = 'rust';

/* kernel_printer_write hands a Printer at most this much at a time */
static const unsigned long PrinterChunkSize = 128;

/* Longer than any net device's name: VirtioNet's are at most 7 */

/* kernel_ring_create's ceiling: a ring's cells are one allocation */
static const unsigned long RingMaxCapacity = 1UL << 20;

/* kernel_cmd_dispatch: the longest command line it runs -- as long as a line
   of /etc/rc (cmd.cpp's ScriptLineMax): a guest's `hv start` over SSH with a
   distribution's kernel command line runs past two hundred characters before
   it says anything of its own, and was refused here at 255 on the AX41. The
   line is copied through a buffer this long on the stack of the task that
   dispatches it -- an SSH session's or the UDP shell's, 64 KiB each. The UDP
   shell's own datagrams stay at its CMD_MAX. */
static const unsigned long DispatchLineMax = 1023;

/* kernel_cmd_dispatch: how much of a command's output waits in memory for a
   moment it may be sent */
static const unsigned long SinkBufferSize = 64 * 1024;

/* What a command prints, for Rust -- an SSH session's channel -- instead of
   a console. A command may print holding a spinlock with interrupts off
   (ps, stacks, arp) or a mutex (ls holds the Vfs's), and passing text on
   means the network: allocating, and waiting for room as long as the client
   takes. So the text goes into Buf, and on to Fn only where the caller may
   block (PreemptCanBlock): when Buf is full; at a line end once its oldest
   byte has waited FlushDelayNs, so a command printing a line a second is
   seen as it prints while one printing everything at once goes out after it
   returns, its locks let go; and whatever is left when it has. What does
   not fit while nothing may be sent is dropped, and said so at the end. */
class SinkPrinter final : public Stdlib::Printer
{
public:
    typedef void (*SinkFn)(void* ctx, const unsigned char* buf, unsigned long len);
    typedef long (*SourceFn)(void* ctx, unsigned char* buf, unsigned long len,
        unsigned long long timeoutNs);

    SinkPrinter(SinkFn fn, SourceFn src, void* ctx, unsigned char* buf, unsigned long size)
        : Fn(fn)
        , Src(src)
        , Ctx(ctx)
        , Buf(buf)
        , Size(size)
        , Len(0)
        , Oldest(0)
        , Dropped(0)
    {
    }

    virtual void Printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        VPrintf(fmt, args);
        va_end(args);
    }

    virtual void VPrintf(const char *fmt, va_list args) override
    {
        char text[FormatMax];
        if (Stdlib::VsnPrintf(text, sizeof(text), fmt, args) < 0)
            return;
        Put(text);
    }

    virtual void PrintString(const char *s) override
    {
        if (s != nullptr)
            Put(s);
    }

    virtual void Backspace() override
    {
    }

    /* From the session's source, for a command that asks -- after what it
       has printed has gone: a prompt, or the echo of what was typed, has no
       line end to send it on its way, and a command reading its answer must
       not leave the question waiting in Buf. Not where the caller may not
       block: the source waits for the network. */
    virtual long ReadInput(unsigned char* buf, unsigned long len, unsigned long long timeoutNs) override
    {
        if (Src == nullptr || buf == nullptr || len == 0 || !Kernel::PreemptCanBlock())
            return -1;

        Flush();
        return Src(Ctx, buf, len, timeoutNs);
    }

    /* The command has returned: what is left goes, and what was lost is
       said */
    void Finish()
    {
        Flush();
        if (Dropped == 0)
            return;

        char note[128];
        Stdlib::BufferPrinter bp(note, sizeof(note));
        bp.Printf("\n[%u bytes of output dropped: printed with a lock held, past the %u that can wait]\n",
            Dropped, Size);
        Fn(Ctx, reinterpret_cast<const unsigned char*>(note), Stdlib::StrLen(note));
        Dropped = 0;
    }

private:
    SinkPrinter(const SinkPrinter& other) = delete;
    SinkPrinter& operator=(const SinkPrinter& other) = delete;

    void Put(const char* s)
    {
        bool lineEnd = false;
        for (; *s != '\0'; s++)
        {
            if (Len == Size)
            {
                if (!Kernel::PreemptCanBlock())
                {
                    Dropped++;
                    continue;
                }
                Flush();
            }
            if (Len == 0)
                Oldest = Kernel::GetBootTime().GetValue();
            Buf[Len++] = (unsigned char)*s;
            if (*s == '\n' || *s == '\r')
                lineEnd = true;
        }

        if (lineEnd && Len != 0 &&
            Kernel::GetBootTime().GetValue() - Oldest >= FlushDelayNs &&
            Kernel::PreemptCanBlock())
            Flush();
    }

    void Flush()
    {
        if (Len != 0)
        {
            Fn(Ctx, Buf, Len);
            Len = 0;
        }
    }

    static const unsigned long FormatMax = 512;
    static const unsigned long long FlushDelayNs = 50ULL * 1000 * 1000;

    SinkFn Fn;
    SourceFn Src;
    void* Ctx;
    unsigned char* Buf;
    unsigned long Size;
    unsigned long Len;
    unsigned long long Oldest;
    unsigned long Dropped;
};

extern "C" {

void kernel_trace(unsigned int level, const unsigned char* msg, unsigned long len)
{
    /* The level, as Trace() has it. A trace!(3, ...) used to be printed at
       every level: the NVMe interrupt handler's "IRQ spurious (no CQEs)",
       forty thousand times in a few seconds of netblk load, each one
       formatted and logged from the handler. */
    if (level > (unsigned int)Kernel::Tracer::GetInstance().GetLevel())
        return;

    char buf[512];
    unsigned long n = (len < sizeof(buf) - 1) ? len : sizeof(buf) - 1;
    Stdlib::MemCpy(buf, msg, n);
    buf[n] = '\0';
    auto time = Kernel::GetBootTime();
    Kernel::Tracer::GetInstance().Output("%u:%u.%06u:%s\n",
        level, time.GetSecs(), time.GetUsecs(), buf);
}

void* kernel_alloc(unsigned long size, unsigned long align)
{
    if (align == 0 || (align & (align - 1)) != 0)
        Panic("kernel_alloc: bad align %u", align);

    /* Mm::Alloc guarantees 8-byte alignment */
    if (align <= 8)
        return Kernel::Mm::Alloc(size, RustAllocTag);

    /* Over-aligned (#[repr(align)] / SIMD types): over-allocate and stash
       the original pointer just below the aligned address for kernel_free */
    if (size > (unsigned long)-1 - align - sizeof(void*))
        return nullptr;
    void* raw = Kernel::Mm::Alloc(size + align + sizeof(void*), RustAllocTag);
    if (raw == nullptr)
        return nullptr;
    unsigned long aligned =
        Stdlib::RoundUp((unsigned long)raw + sizeof(void*), align);
    ((void**)aligned)[-1] = raw;
    return (void*)aligned;
}

void kernel_free(void* ptr, unsigned long size, unsigned long align)
{
    (void)size;
    if (ptr == nullptr)
        return;
    if (align > 8)
        ptr = ((void**)ptr)[-1];
    Kernel::Mm::Free(ptr);
}

[[noreturn]] void kernel_panic(const unsigned char* msg, unsigned long len)
{
    /* Big enough for a panic from a UB check, whose message alone runs past
       200 characters; what does not fit is truncated here and marked by
       VsnPrintf below. */
    char buf[512];
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

unsigned long long kernel_get_boot_time_ns()
{
    return Kernel::GetBootTime().GetValue();
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

unsigned long kernel_waitgroup_create()
{
    Kernel::WaitGroup* wg = Kernel::Mm::TAlloc<Kernel::WaitGroup, RustAllocTag>();
    return (unsigned long)wg;
}

void kernel_waitgroup_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    Kernel::WaitGroup* wg = reinterpret_cast<Kernel::WaitGroup*>(handle);
    wg->~WaitGroup();
    Kernel::Mm::Free(wg);
}

void kernel_waitgroup_add(unsigned long handle, long delta)
{
    reinterpret_cast<Kernel::WaitGroup*>(handle)->Add(delta);
}

void kernel_waitgroup_done(unsigned long handle)
{
    reinterpret_cast<Kernel::WaitGroup*>(handle)->Done();
}

void kernel_waitgroup_wait(unsigned long handle)
{
    reinterpret_cast<Kernel::WaitGroup*>(handle)->Wait();
}

unsigned long kernel_event_create()
{
    Kernel::Event* e = Kernel::Mm::TAlloc<Kernel::Event, RustAllocTag>();
    return (unsigned long)e;
}

void kernel_event_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    Kernel::Event* e = reinterpret_cast<Kernel::Event*>(handle);
    e->~Event();
    Kernel::Mm::Free(e);
}

void kernel_event_wait(unsigned long handle)
{
    reinterpret_cast<Kernel::Event*>(handle)->Wait();
}

int kernel_event_wait_for(unsigned long handle, unsigned long long timeoutNs)
{
    return reinterpret_cast<Kernel::Event*>(handle)->WaitFor(timeoutNs) ? 1 : 0;
}

void kernel_event_signal(unsigned long handle)
{
    reinterpret_cast<Kernel::Event*>(handle)->Signal();
}

/* The name ps and top show for a task Rust starts: the caller's, a Rust
   string with no terminator, cut to the room a task has for one */
static Kernel::Task* NewRustTask(const unsigned char* name, unsigned long nameLen)
{
    char buf[Kernel::TaskNameLen];
    const unsigned long len = Stdlib::Min(nameLen, (unsigned long)sizeof(buf) - 1);

    Stdlib::MemCpy(buf, name, len);
    buf[len] = '\0';
    /* Never the format itself: a '%' in the name is the caller's */
    return Kernel::Mm::TAlloc<Kernel::Task, RustAllocTag>("%s", buf);
}

unsigned long kernel_task_spawn(const unsigned char* name, unsigned long nameLen,
    void (*func)(void*), void* ctx)
{
    Kernel::Task* t = NewRustTask(name, nameLen);
    if (!t)
        return 0;
    if (!t->Start(func, ctx))
    {
        t->Put();
        return 0;
    }
    return (unsigned long)t;
}

unsigned long kernel_task_spawn_on(const unsigned char* name, unsigned long nameLen,
    void (*func)(void*), void* ctx,
    unsigned long affinity_mask)
{
    Kernel::Task* t = NewRustTask(name, nameLen);
    if (!t)
        return 0;
    t->SetCpuAffinity(affinity_mask);
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

/* Whether the calling task has been asked to stop: what a loop that runs
   for the life of the kernel checks each time round. */
int kernel_task_stopping()
{
    Kernel::Task* task = Kernel::Task::GetCurrentTask();
    return (task != nullptr && task->IsStopping()) ? 1 : 0;
}

void kernel_task_put(unsigned long handle)
{
    reinterpret_cast<Kernel::Task*>(handle)->Put();
}

void kernel_sleep_ns(unsigned long long ns)
{
    Kernel::Sleep((ulong)ns);
}

void kernel_task_yield_to_runnable()
{
    Kernel::YieldToRunnable();
}

/* The calling task -- a handle to compare with a TaskHandle's, never to
   wait on or put */
unsigned long kernel_task_current()
{
    return (unsigned long)Kernel::Task::GetCurrentTask();
}

/* The same, but 0 rather than a complaint when the stack is not a task's.
   For code that runs wherever it is called from -- the tracer's side of the
   disk log above all, where the complaint would itself trace and recurse. */
unsigned long kernel_task_current_or_none()
{
    return (unsigned long)Kernel::Task::TryGetCurrentTask();
}

unsigned int kernel_get_cpu_id()
{
    return (unsigned int)Kernel::GetCpu().GetIndex();
}

unsigned int kernel_cpu_count()
{
    ulong mask = Kernel::CpuTable::GetInstance().GetRunningCpus();
    unsigned int count = 0;
    while (mask) { count += mask & 1; mask >>= 1; }
    return count;
}

unsigned long kernel_cpu_online_mask()
{
    return Kernel::CpuTable::GetInstance().GetRunningCpus();
}

} /* extern "C" */

struct RustIPIAdapter
{
    void (*Handler)(void*);
    void* Ctx;
};

static void RustIPITrampoline(void* actx, Kernel::Context*)
{
    auto* a = static_cast<RustIPIAdapter*>(actx);
    a->Handler(a->Ctx);
}

extern "C" {

void kernel_cpu_run_on(unsigned int cpu,
    void (*handler)(void*), void* ctx)
{
    if (!handler || cpu >= (unsigned int)Kernel::MaxCpus)
        return;

    /* A CPU that never started or already exited will not drain its IPI task
       list; queueing would block Completion.Wait() forever (SendIPISelf
       silently refuses to send in those states). */
    auto& target = Kernel::CpuTable::GetInstance().GetCpu(cpu);
    ulong state = target.GetState();
    if (!(state & Kernel::Cpu::StateRunning) ||
        (state & (Kernel::Cpu::StateExiting | Kernel::Cpu::StateExited)))
    {
        Trace(0, "kernel_cpu_run_on: cpu %u not running (state 0x%p)",
              (ulong)cpu, state);
        return;
    }

    RustIPIAdapter a{handler, ctx};
    Kernel::IPITask task(RustIPITrampoline, &a);
    target.QueueIPITask(task);
}

/* An interrupt to `cpu`, now, and nothing waited for: what makes a vCPU that
   is running a guest there leave it -- a physical interrupt ends the guest's
   turn -- and costs any other CPU an interrupt with nothing to do. Sent the
   way SoftIrq's kick is, straight to the interrupt controller: the caller may
   have interrupts off, and CpuTable::SendIPI takes locks its handler takes
   too. A CPU that is not running holds it pending, or never sees it. */
void kernel_cpu_kick(unsigned int cpu)
{
    if (cpu >= (unsigned int)Kernel::MaxCpus)
        return;

    Hal::SendIpi(cpu, Kernel::CpuTable::IPIVector);
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
    if (num_pages == 0)
        return nullptr;

    ulong size = num_pages * Const::PageSize;
    ulong va = Kernel::Mm::PageTable::GetInstance().MapMmioRegion(phys_base, size);
    if (va == 0)
        return nullptr;
    return (void*)va;
}

void kernel_unmap_phys(void* virt_addr, unsigned long num_pages)
{
    /* MapMmioRegion maps at physAddr + KernelSpaceBase via direct PTEs;
     * these mappings are permanent and cannot be unmapped in the current
     * page-table implementation. */
    (void)virt_addr;
    (void)num_pages;
}

unsigned long kernel_virt_to_phys(const void* virt_addr)
{
    if (!virt_addr)
        return 0;
    return Kernel::Mm::PageTable::GetInstance().VirtToPhys((ulong)virt_addr);
}

/* ---- Frames: pages of RAM mapped nowhere (kcore::frame) ---- */

unsigned long kernel_frame_alloc()
{
    /* AllocPage zeroes what it hands out, through the temp map. */
    Kernel::Mm::Page* page = Kernel::Mm::PageTable::GetInstance().AllocPage();
    return page ? page->GetPhyAddress() : 0;
}

void kernel_frame_free(unsigned long phys)
{
    Kernel::Mm::PageTable::GetInstance().FreeFrame(phys);
}

/* Copy between a frame and the caller's memory through one temp-map slot:
   out of the frame into `buf`, or `data` into it -- exactly one of the two
   is given. Preemption and interrupts are off from the map to the unmap: the
   unmap invalidates only this CPU's TLB, so a task moved to another CPU
   mid-copy would leave that slot's translation stale behind it (VirtToPhys
   has the same window, for the same reason).

   What a frame holds may be a guest's memory, written while the copy runs,
   so nothing here may hold a view of it the compiler could reason about.
   The range is one call of the architecture's memcpy -- assembly on x86, a
   function of its own on arm64 -- which the compiler sees nothing of: each
   byte is read or written once, where the copy says, and a guest racing its
   own buffer gets what it raced for. (It used to go a word at a time
   through a volatile pointer, with a MemCpy call for each word on the
   caller's side: 512 calls a page, and two thirds of a busy guest's vCPU
   while it wrote to its disk.) */
static int FrameCopy(unsigned long phys, unsigned long offset,
    unsigned char* buf, const unsigned char* data, unsigned long len)
{
    if (len == 0)
        return 0;
    if ((buf == nullptr) == (data == nullptr))
        return -1;
    if (offset >= Const::PageSize || len > Const::PageSize - offset)
        return -1;

    auto& pt = Kernel::Mm::PageTable::GetInstance();
    if (!pt.IsFrameAddress(phys))
        return -1;

    ulong flags = Kernel::PreemptIrqSave();
    ulong va = pt.TmpMapPage(phys);
    if (va == 0)
    {
        Kernel::PreemptIrqRestore(flags);
        return -1;
    }

    /* Inside the page mapped at va: offset and len were checked above. */
    void* frame = reinterpret_cast<void*>(va + offset);
    if (data != nullptr)
        Stdlib::MemCpy(frame, data, len);
    else
        Stdlib::MemCpy(buf, frame, len);

    pt.TmpUnmapPage(va);
    Kernel::PreemptIrqRestore(flags);
    return 0;
}

int kernel_frame_read(unsigned long phys, unsigned long offset,
    unsigned char* buf, unsigned long len)
{
    return FrameCopy(phys, offset, buf, nullptr, len);
}

int kernel_frame_write(unsigned long phys, unsigned long offset,
    const unsigned char* data, unsigned long len)
{
    return FrameCopy(phys, offset, nullptr, data, len);
}

int kernel_get_random(unsigned char* buf, unsigned long len)
{
    if (!buf || len == 0)
        return 0;

    /* The pool, not a source: on a bare-metal machine with no virtio-rng
       there is no source to read, and this returning 0 is what a TLS
       handshake fails with (rustls: FailedToGetRandomBytes). An unseeded pool
       still has to fail here -- a handshake keyed from a zero pool would be
       worse than no handshake. */
    auto& random = Kernel::Random::GetInstance();
    if (!random.IsSeeded())
        return 0;

    random.GetBytes(buf, (ulong)len);
    return 1;
}

/* ---- Soft IRQ ---- */

void kernel_softirq_raise(unsigned long type)
{
    Kernel::SoftIrq::GetInstance().Raise(type);
}

/* Whether that soft IRQ is already asked for. What lets a poll tell a pass
   it caused from one an interrupt caused. */
int kernel_softirq_pending(unsigned long type)
{
    return Kernel::SoftIrq::GetInstance().IsPending(type) ? 1 : 0;
}

void kernel_softirq_register(unsigned long type,
    void (*handler)(void*), void* ctx)
{
    Kernel::SoftIrq::GetInstance().Register(type, handler, ctx);
}

/* ---- PCI ---- */

struct RustPciDeviceInfo
{
    unsigned short bus;
    unsigned short slot;
    unsigned short func;
    unsigned short vendor;
    unsigned short device;
    unsigned char cls;
    unsigned char subclass;
    unsigned char prog_if;
    unsigned char revision;
    unsigned char irq_line;
    unsigned char irq_pin;
};

static void FillRustPciInfo(RustPciDeviceInfo* out, const Pci::DeviceInfo* dev)
{
    out->bus = dev->Bus;
    out->slot = dev->Slot;
    out->func = dev->Func;
    out->vendor = dev->Vendor;
    out->device = dev->Device;
    out->cls = dev->Class;
    out->subclass = dev->SubClass;
    out->prog_if = dev->ProgIF;
    out->revision = dev->RevisionID;
    out->irq_line = dev->InterruptLine;
    out->irq_pin = dev->InterruptPin;
}

long kernel_pci_find_device(unsigned short vendor, unsigned short device,
    unsigned long start_index, RustPciDeviceInfo* out)
{
    if (!out)
        return -1;
    auto& pci = Pci::GetInstance();
    for (unsigned long i = start_index; i < pci.GetDeviceCount(); i++)
    {
        auto* dev = pci.GetDevice(i);
        if (dev && dev->Valid && dev->Vendor == vendor && dev->Device == device)
        {
            FillRustPciInfo(out, dev);
            return (long)i;
        }
    }
    return -1;
}

unsigned long kernel_pci_device_count()
{
    return Pci::GetInstance().GetDeviceCount();
}

int kernel_pci_get_device(unsigned long index, RustPciDeviceInfo* out)
{
    if (!out)
        return 0;
    auto* dev = Pci::GetInstance().GetDevice(index);
    if (!dev || !dev->Valid)
        return 0;
    FillRustPciInfo(out, dev);
    return 1;
}

unsigned int kernel_pci_get_bar(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned char bar)
{
    return Pci::GetInstance().GetBAR(bus, slot, func, bar);
}

void kernel_pci_enable_bus_mastering(unsigned short bus, unsigned short slot,
    unsigned short func)
{
    Pci::GetInstance().EnableBusMastering(bus, slot, func);
}

unsigned char kernel_pci_find_capability(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned char cap_id, unsigned char start_offset)
{
    return Pci::GetInstance().FindCapability(bus, slot, func, cap_id, start_offset);
}

unsigned char kernel_pci_read_config8(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset)
{
    return Pci::GetInstance().ReadByte(bus, slot, func, offset);
}

unsigned short kernel_pci_read_config16(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset)
{
    return Pci::GetInstance().ReadWord(bus, slot, func, offset);
}

unsigned int kernel_pci_read_config32(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset)
{
    return Pci::GetInstance().ReadDword(bus, slot, func, offset);
}

void kernel_pci_write_config8(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset, unsigned char val)
{
    Pci::GetInstance().WriteByte(bus, slot, func, offset, val);
}

void kernel_pci_write_config16(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset, unsigned short val)
{
    Pci::GetInstance().WriteWord(bus, slot, func, offset, val);
}

void kernel_pci_write_config32(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset, unsigned int val)
{
    Pci::GetInstance().WriteDword(bus, slot, func, offset, val);
}

/* ---- MSI-X ---- */

static Pci::DeviceInfo* FindPciDevByBdf(unsigned short bus, unsigned short slot,
    unsigned short func)
{
    auto& pci = Pci::GetInstance();
    for (ulong i = 0; i < pci.GetDeviceCount(); i++)
    {
        auto* d = pci.GetDevice(i);
        if (d && d->Valid && d->Bus == bus && d->Slot == slot && d->Func == func)
            return d;
    }
    return nullptr;
}

unsigned long kernel_msix_create(unsigned short bus, unsigned short slot,
    unsigned short func, const unsigned long* mapped_bars)
{
    auto* dev = FindPciDevByBdf(bus, slot, func);
    if (!dev)
        return 0;

    auto* t = Kernel::Mm::TAlloc<Kernel::MsixTable, RustAllocTag>();
    if (!t)
        return 0;

    if (!t->Setup(dev, mapped_bars))
    {
        t->~MsixTable();
        Kernel::Mm::Free(t);
        return 0;
    }
    return (unsigned long)t;
}

void kernel_msix_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    auto* t = reinterpret_cast<Kernel::MsixTable*>(handle);
    t->~MsixTable();
    Kernel::Mm::Free(t);
}

void kernel_msix_mask(unsigned long handle, unsigned short index)
{
    if (handle == 0)
        return;
    reinterpret_cast<Kernel::MsixTable*>(handle)->Mask(index);
}

void kernel_msix_unmask(unsigned long handle, unsigned short index)
{
    if (handle == 0)
        return;
    reinterpret_cast<Kernel::MsixTable*>(handle)->Unmask(index);
}

unsigned short kernel_msix_table_size(unsigned long handle)
{
    if (handle == 0)
        return 0;
    return reinterpret_cast<Kernel::MsixTable*>(handle)->GetTableSize();
}

int kernel_msix_is_ready(unsigned long handle)
{
    if (handle == 0)
        return 0;
    return reinterpret_cast<Kernel::MsixTable*>(handle)->IsReady() ? 1 : 0;
}

} /* extern "C" */

/* ---- Legacy (INTx) interrupts ---- */

static const ulong RustIrqSlotCount = 8;
static const u8 RustIrqVectorBase = 0x38;

/* Every member initialised, so that the static table is too: a default
   constructor that left one indeterminate is not a constant expression, and
   the compiler then made a runtime constructor this kernel never runs. */
struct RustIrqSlot
{
    void (*Handler)(void*) = nullptr;
    void* Ctx = nullptr;
    u8 Vector = 0;
    bool Used = false;
    Kernel::Atomic InFlight; /* ISRs currently executing Handler */
};

static RustIrqSlot RustIrqSlots[RustIrqSlotCount];
static Kernel::RawRwSpinLock RustIrqLock;

static Kernel::InterruptHandlerFn RustStubTable[RustIrqSlotCount] = {
    RustInterruptStub0,
    RustInterruptStub1,
    RustInterruptStub2,
    RustInterruptStub3,
    RustInterruptStub4,
    RustInterruptStub5,
    RustInterruptStub6,
    RustInterruptStub7,
};

class RustLegacyHandler : public Kernel::InterruptHandler
{
public:
    ulong SlotIndex;

    void OnInterruptRegister(u8 irq, u8 vector) override
    {
        (void)irq;
        RustIrqSlots[SlotIndex].Vector = vector;
    }

    Kernel::InterruptHandlerFn GetHandlerFn() override
    {
        return RustStubTable[SlotIndex];
    }

    void OnInterrupt(Kernel::Context* ctx) override
    {
        (void)ctx;
        Kernel::Task* preempt = RustIrqLock.ReadLock();
        auto handler = RustIrqSlots[SlotIndex].Handler;
        auto uctx = RustIrqSlots[SlotIndex].Ctx;
        if (handler)
            RustIrqSlots[SlotIndex].InFlight.Inc();
        RustIrqLock.ReadUnlock(preempt);
        if (handler)
        {
            handler(uctx);
            RustIrqSlots[SlotIndex].InFlight.Dec();
        }
    }
};

static char RustLegacyHandlersBuf[RustIrqSlotCount][sizeof(RustLegacyHandler)];
static bool RustLegacyHandlersInit[RustIrqSlotCount];

static RustLegacyHandler& GetLegacyHandler(ulong i)
{
    if (!RustLegacyHandlersInit[i])
    {
        new (&RustLegacyHandlersBuf[i]) RustLegacyHandler();
        RustLegacyHandlersInit[i] = true;
    }
    return *reinterpret_cast<RustLegacyHandler*>(&RustLegacyHandlersBuf[i]);
}

extern "C" {

void RustInterruptDispatch(Kernel::Context* ctx, int slot)
{
    (void)ctx;

    /* The legacy line, counted for the same reason as the MSI-X path below:
       the driver falls back to INTx when MSI-X is unavailable, and which of
       the two a card ended up on was not visible from the running machine. */
    Kernel::InterruptStats::Inc(Kernel::IrqShared);

    if (slot < 0 || (ulong)slot >= RustIrqSlotCount)
    {
        Hal::IrqEoi();
        return;
    }
    /* InFlight is raised inside the read-locked section so unregister (which
       nulls Handler under the write lock) can wait out a mid-call ISR before
       its caller frees ctx. */
    Kernel::Task* preempt = RustIrqLock.ReadLock();
    auto handler = RustIrqSlots[slot].Handler;
    auto uctx = RustIrqSlots[slot].Ctx;
    if (handler)
        RustIrqSlots[slot].InFlight.Inc();
    RustIrqLock.ReadUnlock(preempt);
    if (handler)
    {
        handler(uctx);
        RustIrqSlots[slot].InFlight.Dec();
    }
    Hal::IrqEoi();
}

unsigned long kernel_interrupt_register_level(
    unsigned char irq_line,
    void (*handler)(void*), void* ctx,
    unsigned char* out_vector)
{
    if (!handler || !out_vector)
        return 0;

    ulong flags = RustIrqLock.WriteLockIrqSave();
    for (ulong i = 0; i < RustIrqSlotCount; i++)
    {
        if (!RustIrqSlots[i].Used)
        {
            RustIrqSlots[i].Handler = handler;
            RustIrqSlots[i].Ctx = ctx;
            RustIrqSlots[i].Used = true;
            RustIrqSlots[i].Vector = 0;

            auto& lh = GetLegacyHandler(i);
            lh.SlotIndex = i;

            u8 vector = (u8)(RustIrqVectorBase + i);
            Kernel::Interrupt::RegisterLevel(lh, irq_line, vector);

            /* RegisterLevel is void and refuses silently (GSI owned by an
               edge handler, vector already claimed, shared list full). Its
               OnInterruptRegister callback sets the slot's Vector; still 0
               means no IOAPIC pin was programmed -- roll the slot back
               instead of handing the driver a dead handle it would trust
               (NIC enabled, no interrupts ever delivered). */
            if (RustIrqSlots[i].Vector == 0)
            {
                RustIrqSlots[i].Handler = nullptr;
                RustIrqSlots[i].Ctx = nullptr;
                RustIrqSlots[i].Used = false;
                RustIrqLock.WriteUnlockIrqRestore(flags);
                Trace(0, "kernel_interrupt_register_level: irq %u refused",
                      (ulong)irq_line);
                return 0;
            }

            *out_vector = RustIrqSlots[i].Vector;
            RustIrqLock.WriteUnlockIrqRestore(flags);
            return i + 1;
        }
    }
    RustIrqLock.WriteUnlockIrqRestore(flags);

    Trace(0, "kernel_interrupt_register_level: no free slots");
    return 0;
}

void kernel_interrupt_unregister(unsigned long handle)
{
    if (handle == 0 || handle > RustIrqSlotCount)
        return;
    ulong i = handle - 1;
    ulong flags = RustIrqLock.WriteLockIrqSave();
    RustIrqSlots[i].Handler = nullptr;
    RustIrqSlots[i].Ctx = nullptr;
    RustIrqSlots[i].Used = false;
    RustIrqSlots[i].Vector = 0;
    RustIrqLock.WriteUnlockIrqRestore(flags);

    /* Wait out an ISR mid-call on another CPU: the caller may free ctx the
       moment we return. An ISR on this CPU cannot be mid-call here --
       interrupts run to completion. */
    while (RustIrqSlots[i].InFlight.Get() != 0)
        Pause();
}

} /* extern "C" */

/* ---- Periodic timers ---- */

static const ulong RustTimerSlotCount = 8;

struct RustTimerSlot
{
    void (*Handler)(void*);
    void* Ctx;
    bool Active;
};

static RustTimerSlot RustTimerSlots[RustTimerSlotCount];
static Kernel::RawRwSpinLock RustTimerLock;

class RustTimerAdapter : public Kernel::TimerCallback
{
public:
    ulong SlotIndex;

    void OnTick(Kernel::TimerCallback& callback) override
    {
        (void)callback;
        Kernel::Task* preempt = RustTimerLock.ReadLock();
        auto handler = RustTimerSlots[SlotIndex].Handler;
        auto ctx = RustTimerSlots[SlotIndex].Ctx;
        RustTimerLock.ReadUnlock(preempt);
        if (handler)
            handler(ctx);
    }
};

static char RustTimerAdaptersBuf[RustTimerSlotCount][sizeof(RustTimerAdapter)];
static bool RustTimerAdaptersInit[RustTimerSlotCount];

static RustTimerAdapter& GetTimerAdapter(ulong i)
{
    if (!RustTimerAdaptersInit[i])
    {
        new (&RustTimerAdaptersBuf[i]) RustTimerAdapter();
        RustTimerAdaptersInit[i] = true;
    }
    return *reinterpret_cast<RustTimerAdapter*>(&RustTimerAdaptersBuf[i]);
}

extern "C" {

unsigned long kernel_timer_start(
    void (*handler)(void*), void* ctx,
    unsigned long long period_ns)
{
    if (!handler || period_ns == 0)
        return 0;

    ulong flags = RustTimerLock.WriteLockIrqSave();
    for (ulong i = 0; i < RustTimerSlotCount; i++)
    {
        if (!RustTimerSlots[i].Active)
        {
            RustTimerSlots[i].Handler = handler;
            RustTimerSlots[i].Ctx = ctx;
            RustTimerSlots[i].Active = true;

            auto& ta = GetTimerAdapter(i);
            ta.SlotIndex = i;

            Stdlib::Time period(period_ns);
            if (!Kernel::TimerTable::GetInstance().StartTimer(
                    ta, period))
            {
                RustTimerSlots[i].Active = false;
                RustTimerSlots[i].Handler = nullptr;
                RustTimerSlots[i].Ctx = nullptr;
                RustTimerLock.WriteUnlockIrqRestore(flags);
                return 0;
            }
            RustTimerLock.WriteUnlockIrqRestore(flags);
            return i + 1;
        }
    }
    RustTimerLock.WriteUnlockIrqRestore(flags);
    return 0;
}

void kernel_timer_stop(unsigned long handle)
{
    if (handle == 0 || handle > RustTimerSlotCount)
        return;
    ulong i = handle - 1;

    /* Clear the slot under the lock, then release it BEFORE StopTimer. StopTimer
       spin-waits for an in-flight OnTick on another CPU to finish, and OnTick
       needs RustTimerLock in read mode; holding the write lock across that wait
       would deadlock (AB/BA). Clearing Handler first makes a racing OnTick a
       no-op once it acquires the read lock. */
    ulong flags = RustTimerLock.WriteLockIrqSave();
    RustTimerSlots[i].Handler = nullptr;
    RustTimerSlots[i].Ctx = nullptr;
    RustTimerSlots[i].Active = false;
    RustTimerLock.WriteUnlockIrqRestore(flags);

    Kernel::TimerTable::GetInstance().StopTimer(GetTimerAdapter(i));
}

} /* extern "C" */

/* ---- MSI-X callback slots ---- */

static const ulong RustMsixSlotCount = 32;

/* Every member initialised, as RustIrqSlot's are. */
struct RustMsixSlot
{
    void (*Handler)(void*) = nullptr;
    void* Ctx = nullptr;
    bool Used = false;
    Kernel::Atomic InFlight; /* ISRs currently executing Handler */
};

static RustMsixSlot RustMsixSlots[RustMsixSlotCount];
static Kernel::RawRwSpinLock RustMsixLock;

static Kernel::InterruptHandlerFn RustMsixStubTable[RustMsixSlotCount] = {
    RustMsixStub0,
    RustMsixStub1,
    RustMsixStub2,
    RustMsixStub3,
    RustMsixStub4,
    RustMsixStub5,
    RustMsixStub6,
    RustMsixStub7,
    RustMsixStub8,
    RustMsixStub9,
    RustMsixStub10,
    RustMsixStub11,
    RustMsixStub12,
    RustMsixStub13,
    RustMsixStub14,
    RustMsixStub15,
    RustMsixStub16,
    RustMsixStub17,
    RustMsixStub18,
    RustMsixStub19,
    RustMsixStub20,
    RustMsixStub21,
    RustMsixStub22,
    RustMsixStub23,
    RustMsixStub24,
    RustMsixStub25,
    RustMsixStub26,
    RustMsixStub27,
    RustMsixStub28,
    RustMsixStub29,
    RustMsixStub30,
    RustMsixStub31,
};

extern "C" void RustMsixDispatch(Kernel::Context* ctx, int slot);

class RustMsixSlotHandler : public Kernel::InterruptHandler
{
public:
    ulong SlotIndex;

    void OnInterruptRegister(u8 irq, u8 vector) override
    {
        (void)irq;
        (void)vector;
    }

    Kernel::InterruptHandlerFn GetHandlerFn() override
    {
        return RustMsixStubTable[SlotIndex];
    }

    /* Object-based dispatch path (arm64 GIC/ITS LPIs). On x86 MSI-X goes
       through the IDT stub -> RustMsixDispatch instead, so this override is
       unused there. */
    void OnInterrupt(Kernel::Context* ctx) override
    {
        RustMsixDispatch(ctx, (int)SlotIndex);
    }
};

static char RustMsixSlotHandlersBuf[RustMsixSlotCount][sizeof(RustMsixSlotHandler)];
static bool RustMsixSlotHandlersInit[RustMsixSlotCount];

static RustMsixSlotHandler& GetMsixSlotHandler(ulong i)
{
    if (!RustMsixSlotHandlersInit[i])
    {
        new (&RustMsixSlotHandlersBuf[i]) RustMsixSlotHandler();
        RustMsixSlotHandlersInit[i] = true;
    }
    return *reinterpret_cast<RustMsixSlotHandler*>(&RustMsixSlotHandlersBuf[i]);
}

extern "C" {

void RustMsixDispatch(Kernel::Context* ctx, int slot)
{
    (void)ctx;

    /* Counted here because nothing counted it anywhere: IrqMsix existed only
       to name a row in `irqstat`, so that row read zero for the life of every
       boot -- including on a machine whose NIC had just delivered thirteen
       thousand packets. Asking whether the card's interrupts had stopped was
       therefore unanswerable, which is most of why its receive stall took so
       long to corner. */
    Kernel::InterruptStats::Inc(Kernel::IrqMsix);

    if (slot < 0 || (ulong)slot >= RustMsixSlotCount)
    {
        Hal::IrqEoi();
        return;
    }
    /* Same in-flight discipline as RustInterruptDispatch */
    Kernel::Task* preempt = RustMsixLock.ReadLock();
    auto handler = RustMsixSlots[slot].Handler;
    auto uctx = RustMsixSlots[slot].Ctx;
    if (handler)
        RustMsixSlots[slot].InFlight.Inc();
    RustMsixLock.ReadUnlock(preempt);
    if (handler)
    {
        handler(uctx);
        RustMsixSlots[slot].InFlight.Dec();
    }
    Hal::IrqEoi();
}

unsigned long kernel_msix_register_handler(
    unsigned long msix_handle, unsigned short msix_index,
    void (*handler)(void*), void* ctx,
    unsigned char* out_vector)
{
    if (!handler || !out_vector || msix_handle == 0)
        return 0;

    ulong flags = RustMsixLock.WriteLockIrqSave();
    for (ulong i = 0; i < RustMsixSlotCount; i++)
    {
        if (!RustMsixSlots[i].Used)
        {
            RustMsixSlots[i].Handler = handler;
            RustMsixSlots[i].Ctx = ctx;
            RustMsixSlots[i].Used = true;

            auto& mh = GetMsixSlotHandler(i);
            mh.SlotIndex = i;

            auto* t = reinterpret_cast<Kernel::MsixTable*>(msix_handle);
            u8 vector = t->EnableVector(msix_index, mh);
            if (vector == 0)
            {
                RustMsixSlots[i].Handler = nullptr;
                RustMsixSlots[i].Ctx = nullptr;
                RustMsixSlots[i].Used = false;
                RustMsixLock.WriteUnlockIrqRestore(flags);
                return 0;
            }
            *out_vector = vector;
            RustMsixLock.WriteUnlockIrqRestore(flags);
            return i + 1;
        }
    }
    RustMsixLock.WriteUnlockIrqRestore(flags);
    Trace(0, "kernel_msix_register_handler: no free slots");
    return 0;
}

void kernel_msix_unregister_handler(unsigned long handle)
{
    if (handle == 0 || handle > RustMsixSlotCount)
        return;
    ulong i = handle - 1;
    ulong flags = RustMsixLock.WriteLockIrqSave();
    RustMsixSlots[i].Handler = nullptr;
    RustMsixSlots[i].Ctx = nullptr;
    RustMsixSlots[i].Used = false;
    RustMsixLock.WriteUnlockIrqRestore(flags);

    /* Wait out an ISR mid-call on another CPU (see kernel_interrupt_unregister) */
    while (RustMsixSlots[i].InFlight.Get() != 0)
        Pause();
}

} /* extern "C" */

/* ---- Entropy source bridge ---- */

/* A source of raw entropy implemented in Rust -- the virtio-rng driver --
   put in front of the kernel's pool (kernel/entropy.h). Registration is for
   good, as it is for a C++ source, so nothing here is ever freed. */
struct RustEntropyOps
{
    const char* Name;
    int (*GetRandom)(void* ctx, void* buf, unsigned long len);
    void* Ctx;
};

class RustEntropySource : public Kernel::EntropySource
{
public:
    RustEntropyOps Ops;

    const char* GetName() override { return Ops.Name; }

    bool GetRandom(u8* buf, ulong len) override
    {
        return Ops.GetRandom(Ops.Ctx, buf, (unsigned long)len) == 0;
    }
};

extern "C" {

unsigned long kernel_entropy_source_register(const char* name,
    int (*getRandom)(void* ctx, void* buf, unsigned long len), void* ctx)
{
    if (!name || !getRandom)
        return 0;

    RustEntropySource* src = Kernel::Mm::TAlloc<RustEntropySource, RustAllocTag>();
    if (!src)
        return 0;

    src->Ops.Name = name;
    src->Ops.GetRandom = getRandom;
    src->Ops.Ctx = ctx;

    if (!Kernel::EntropySourceTable::GetInstance().Register(src))
    {
        src->~RustEntropySource();
        Kernel::Mm::Free(src);
        return 0;
    }

    return (unsigned long)src;
}

} /* extern "C" */

/* ---- Net device bridge ---- */

/* The net devices, the frames and the queues between them are Rust
   (src/rust/net/src/device.rs and frame.rs), and so is the C ABI a driver
   registers through: the kernel_netdev_* and kernel_netframe_* names are
   defined there now. Nothing of them is left here. */

extern "C" {

unsigned long long kernel_hpet_read_ns()
{
    auto& hpet = Kernel::Hpet::GetInstance();
    if (!hpet.IsAvailable()) return 0;
    return hpet.GetTime().GetValue();
}

bool kernel_hpet_is_available()
{
    return Kernel::Hpet::GetInstance().IsAvailable();
}

bool kernel_acpi_has_firmware_watchdog()
{
    return Kernel::Acpi::GetInstance().HasFirmwareWatchdog();
}

/* Shell commands from Rust, a loadable module's in particular (kcore::cmd).
   Rust's handler takes the arguments as *const u8, which is the same thing
   to the ABI as the const char* DynamicHandler is declared with. */
unsigned long kernel_cmd_register(const unsigned char* name, unsigned long nameLen,
    const unsigned char* help, unsigned long helpLen,
    Kernel::Cmd::DynamicHandler handler, void* ctx)
{
    return Kernel::Cmd::GetInstance().RegisterDynamic(reinterpret_cast<const char*>(name),
        nameLen, reinterpret_cast<const char*>(help), helpLen, handler, ctx);
}

void kernel_cmd_unregister(unsigned long handle)
{
    Kernel::Cmd::GetInstance().UnregisterDynamic(handle);
}

/* out is the Stdlib::Printer a command handler was given */
void kernel_printer_write(void* out, const unsigned char* buf, unsigned long len)
{
    if (out == nullptr || buf == nullptr)
        return;

    auto* printer = static_cast<Stdlib::Printer*>(out);
    char chunk[PrinterChunkSize];
    while (len != 0)
    {
        unsigned long n = (len < sizeof(chunk) - 1) ? len : sizeof(chunk) - 1;
        Stdlib::MemCpy(chunk, buf, n);
        chunk[n] = '\0';
        printer->PrintString(chunk);
        buf += n;
        len -= n;
    }
}

/* kernel_cmd_dispatch and kernel_cmd_dispatch_io: a command line run as the
   console would run it, what it prints handed to sink(ctx, ...) as
   SinkPrinter passes it on, and -- given a source -- what is typed while it
   runs read from source(ctx, ...). Sleeps as long as the command runs: task
   context only, with no lock held. */
static void DispatchTo(const unsigned char* line, unsigned long len,
    SinkPrinter::SinkFn sink, SinkPrinter::SourceFn source, void* ctx)
{
    if (line == nullptr || sink == nullptr)
        return;

    if (len > DispatchLineMax)
    {
        char note[96];
        Stdlib::BufferPrinter bp(note, sizeof(note));
        bp.Printf("command too long: %u characters, the most is %u\n", len, DispatchLineMax);
        sink(ctx, reinterpret_cast<const unsigned char*>(note), Stdlib::StrLen(note));
        return;
    }

    /* Before the command runs: the one moment sure to be allowed to */
    auto* buf = static_cast<unsigned char*>(Kernel::Mm::Alloc(SinkBufferSize, RustAllocTag));
    if (buf == nullptr)
    {
        static const char NoMemory[] = "no memory for the command's output\n";
        sink(ctx, reinterpret_cast<const unsigned char*>(NoMemory), sizeof(NoMemory) - 1);
        return;
    }

    char cmd[DispatchLineMax + 1];
    Stdlib::MemCpy(cmd, line, len);
    cmd[len] = '\0';

    SinkPrinter out(sink, source, ctx, buf, SinkBufferSize);
    Kernel::Cmd::Dispatch(cmd, out);
    out.Finish();
    Kernel::Mm::Free(buf);
}

/* A shell command line run for Rust -- an SSH session's -- with nobody to
   type at it */
void kernel_cmd_dispatch(const unsigned char* line, unsigned long len,
    SinkPrinter::SinkFn sink, void* ctx)
{
    DispatchTo(line, len, sink, nullptr, ctx);
}

/* The same with someone to type: an SSH session's command can read what the
   client types while it runs (kernel_printer_read) */
void kernel_cmd_dispatch_io(const unsigned char* line, unsigned long len,
    SinkPrinter::SinkFn sink, SinkPrinter::SourceFn source, void* ctx)
{
    DispatchTo(line, len, sink, source, ctx);
}

/* What is typed at a command, for one that asks: see Printer::ReadInput */
long kernel_printer_read(void* out, unsigned char* buf, unsigned long len,
    unsigned long long timeoutNs)
{
    if (out == nullptr || buf == nullptr || len == 0)
        return -1;

    return static_cast<Stdlib::Printer*>(out)->ReadInput(buf, len, timeoutNs);
}

/* What procfs puts in its files: the kernel's version, the command line it
   was booted with, and the interrupt counters. Each answers how many bytes
   it wrote, which is never more than the buffer holds. */
unsigned long kernel_version_string(char* buf, unsigned long len)
{
    if (buf == nullptr || len == 0)
        return 0;

    int n = Stdlib::SnPrintf(buf, len, "nos %s (%s)", KERNEL_VERSION, KERNEL_GIT_REV);
    return (n > 0) ? (unsigned long)n : 0;
}

unsigned long kernel_cmdline_string(char* buf, unsigned long len)
{
    if (buf == nullptr || len == 0)
        return 0;

    const char* cmdline = Kernel::Parameters::GetInstance().GetCmdline();
    int n = Stdlib::SnPrintf(buf, len, "%s", cmdline);
    return (n > 0) ? (unsigned long)n : 0;
}

/* How many interrupt sources there are, and what the index'th one is
   called and has counted. -1 past the end. */
unsigned long kernel_interrupt_source_count()
{
    return Kernel::InterruptStats::Count;
}

long kernel_interrupt_source(unsigned long index, char* name, unsigned long nameLen)
{
    if (index >= Kernel::InterruptStats::Count)
        return -1;

    Kernel::InterruptSource src = static_cast<Kernel::InterruptSource>(index);
    if (name != nullptr && nameLen != 0)
        Stdlib::SnPrintf(name, nameLen, "%s", Kernel::InterruptStats::GetName(src));
    return Kernel::InterruptStats::Get(src);
}

/* What netconsole= asked for: the collector's address into ip, its port into
   port, and the nctail= cap in KiB into tailKb. 1 when it was asked for at
   all, 0 when it was not. */
int kernel_netconsole_params(unsigned int* ip, unsigned short* port,
    unsigned long* tailKb)
{
    auto& params = Kernel::Parameters::GetInstance();
    if (!params.IsNetconsoleEnabled())
        return 0;

    if (ip != nullptr)
        *ip = params.GetNetconsoleIp();
    if (port != nullptr)
        *port = params.GetNetconsolePort();
    if (tailKb != nullptr)
        *tailKb = params.GetNetconsoleTailKb();
    return 1;
}

/* Every message the log already holds, oldest first, handed to `line`. What
   primes the netconsole ring with the boot that happened before it was set
   up, so a collector still sees the whole of it. */
void kernel_dmesg_replay(void (*line)(void* ctx, const unsigned char* s,
    unsigned long len), void* ctx)
{
    if (line == nullptr)
        return;

    Kernel::DmesgMsg* msg = Kernel::Dmesg::GetInstance().Next(nullptr);
    for (ulong replayed = 0; msg != nullptr; replayed++)
    {
        ulong len = Stdlib::StrLen(msg->Str);
        if (len != 0)
            line(ctx, reinterpret_cast<const unsigned char*>(msg->Str), len);

        /* Never outlast the log itself: a walk past its capacity is chasing
           a tail someone else is still extending. */
        if (replayed + 1 == Kernel::Dmesg::MaxMsgs)
        {
            Kernel::Dmesg::GetInstance().Release(msg);
            break;
        }

        msg = Kernel::Dmesg::GetInstance().Next(msg);
    }
}

/* Interrupts and preemption off, and back on: what code holding a lock of
   its own uses when it cannot allocate one -- netconsole arms its capture
   ring before the page allocator exists. The order matters: reading the CPU
   first and disabling after leaves a window to be preempted onto another. */
unsigned long kernel_irq_save()
{
    return Kernel::PreemptIrqSave();
}

void kernel_irq_restore(unsigned long flags)
{
    Kernel::PreemptIrqRestore(flags);
}

/* Preemption off, and back on: what a spin lock of the kernel's own kind
   holds while it is taken. Interrupts stay on -- a lock an interrupt handler
   also takes needs kernel_irq_save instead. The holder cannot be switched
   away, which is what keeps every other taker from spinning out a whole time
   slice. */
void kernel_preempt_disable()
{
    Kernel::PreemptDisable();
}

void kernel_preempt_enable()
{
    Kernel::PreemptEnable();
}

/* The same, for code that runs on stacks that are not a task's -- the
   tracer's, an interrupt handler's, an AP on its way up. Answers with the
   task whose count went up, or 0 when there was none to raise, and that goes
   back to kernel_preempt_enable_task: the count belongs to the task that
   took it. */
unsigned long kernel_preempt_disable_task()
{
    return (unsigned long)Kernel::PreemptDisableTask();
}

void kernel_preempt_enable_task(unsigned long task)
{
    Kernel::PreemptEnableTask(reinterpret_cast<Kernel::Task*>(task));
}

/* Whether preemption is on at all yet: before it is, no task will ever be
   scheduled and a caller with work to hand off has to do it itself. */
int kernel_preempt_is_on()
{
    return Kernel::PreemptIsOn() ? 1 : 0;
}

/* May the caller wait -- for a completion, or to be scheduled away? Not with
   interrupts off, not off a task stack, and not with preemption disabled,
   which every spinlock holds for as long as it is held. */
int kernel_preempt_can_block()
{
    return Kernel::PreemptCanBlock() ? 1 : 0;
}

/* Whether interrupts are on for this CPU. What tells a caller it may
   release something whose free waits for every other CPU to answer -- with
   interrupts off it could not answer one itself. */
int kernel_interrupts_enabled()
{
    return Hal::IsInterruptEnabled() ? 1 : 0;
}

/* Whether a panic has started: what tells code to write without taking a
   lock, because another CPU may hold it and is on its way to a halt. */
int kernel_panic_active()
{
    return Kernel::Panicker::GetInstance().IsActive() ? 1 : 0;
}

/* One decoded keystroke from a driver in Rust -- the USB keyboard -- to
   every observer: the shell, and whatever else is listening. `code` is the
   PS/2 set-1 make code, which is what consumers key off. */
void kernel_input_key(char c, unsigned char code)
{
    Kernel::KeyboardInput::GetInstance().Emit(c, code);
}

/* dhcp=off: the shell's `dhcp` says so rather than running one. */
int kernel_param_dhcp_off()
{
    return Kernel::Parameters::GetInstance().IsDhcpOff() ? 1 : 0;
}

/* dns=on: a lease's DNS server is worth starting a resolver on. */
int kernel_param_dns_on()
{
    return Kernel::Parameters::GetInstance().IsDnsEnabled() ? 1 : 0;
}

/* rxpoll=on: the tick's receive poll, which the network layer's
   DeviceTable::poll_rx asks about on every call. */
int kernel_param_rxpoll_on()
{
    return Kernel::Parameters::GetInstance().IsRxPollEnabled() ? 1 : 0;
}

/* Whether the kernel log is to be written to a prepared disk area: 1 for
   disklog=on, 0 without it, and -1 before the command line has been read at
   all -- when nobody knows yet, and every line is kept because the first
   lines are part of the boot the area is meant to hold. */
int kernel_disklog_wanted()
{
    auto& params = Kernel::Parameters::GetInstance();
    if (!params.IsParsed())
        return -1;
    return params.IsDiskLogOn() ? 1 : 0;
}

/* What the root filesystem is to be, off the kernel command line: the mode
   (Parameters::RootMode), the device name or label into `value`, and the
   parsed UUID into `uuid`. See rust_mount_root_fs. */
int kernel_root_spec(char* value, unsigned long valueLen, unsigned char* uuid,
    unsigned long uuidLen)
{
    const Kernel::Parameters::RootSpec& spec = Kernel::Parameters::GetInstance().GetRoot();

    if (value != nullptr && valueLen != 0)
        Stdlib::SnPrintf(value, valueLen, "%s", spec.Value);
    if (uuid != nullptr && uuidLen >= sizeof(spec.Uuid))
        Stdlib::MemCpy(uuid, spec.Uuid, sizeof(spec.Uuid));

    return (int)spec.Mode;
}

/* ro: the root is to be mounted read-only */
int kernel_root_read_only()
{
    return Kernel::Parameters::GetInstance().IsRootReadOnly() ? 1 : 0;
}

/* fstest=on: run the filesystem self-test on / once it is mounted */
int kernel_root_fstest()
{
    return Kernel::Parameters::GetInstance().IsFsTest() ? 1 : 0;
}

/* The kernel's lockless ring for Rust (kcore::ring): a bounded MPMC queue of
   words, safe from any context. The ring and its cells are allocated apart,
   as NetFramePool does it. */
struct RustRing
{
    Kernel::LocklessRing Ring;
    Kernel::LocklessRing::Cell* Cells;
};

unsigned long kernel_ring_create(unsigned long capacity)
{
    if (capacity == 0 || capacity > RingMaxCapacity || (capacity & (capacity - 1)) != 0)
        return 0;

    auto* cells = static_cast<Kernel::LocklessRing::Cell*>(
        Kernel::Mm::Alloc(capacity * sizeof(Kernel::LocklessRing::Cell), RustAllocTag));
    if (cells == nullptr)
        return 0;

    RustRing* ring = Kernel::Mm::TAlloc<RustRing, RustAllocTag>();
    if (ring == nullptr)
    {
        Kernel::Mm::Free(cells);
        return 0;
    }

    ring->Cells = cells;
    if (!ring->Ring.Setup(cells, capacity))
    {
        ring->~RustRing();
        Kernel::Mm::Free(ring);
        Kernel::Mm::Free(cells);
        return 0;
    }

    return (unsigned long)ring;
}

void kernel_ring_destroy(unsigned long handle)
{
    if (handle == 0)
        return;

    RustRing* ring = reinterpret_cast<RustRing*>(handle);
    Kernel::LocklessRing::Cell* cells = ring->Cells;
    ring->~RustRing();
    Kernel::Mm::Free(ring);
    Kernel::Mm::Free(cells);
}

int kernel_ring_push(unsigned long handle, unsigned long value)
{
    return reinterpret_cast<RustRing*>(handle)->Ring.Enqueue((void*)value) ? 1 : 0;
}

int kernel_ring_pop(unsigned long handle, unsigned long* value)
{
    void* data;
    if (!reinterpret_cast<RustRing*>(handle)->Ring.Dequeue(data))
        return 0;

    *value = (unsigned long)data;
    return 1;
}

unsigned long kernel_ring_count(unsigned long handle)
{
    return reinterpret_cast<RustRing*>(handle)->Ring.Count();
}

/* Net devices from Rust, both sides of them -- the driver's and the
   consumer's -- are defined in src/rust/net/src/device.rs, alongside the
   devices themselves. Nothing of them is left here. */

} /* extern "C" */
