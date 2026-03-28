#include "trace.h"
#include "panic.h"
#include "time.h"
#include "mutex.h"
#include "wait_group.h"
#include "spin_lock.h"
#include "raw_spin_lock.h"
#include "raw_rw_spin_lock.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "entropy.h"
#include "idt.h"
#include "interrupt.h"
#include "asm.h"
#include "softirq.h"
#include "timer.h"
#include <mm/new.h>
#include <mm/page_allocator.h>
#include <mm/page_table.h>
#include <lib/stdlib.h>
#include <drivers/pci.h>
#include <drivers/msix.h>
#include <drivers/lapic.h>
#include <block/block_device.h>
#include <net/net_device.h>
#include <net/net_frame.h>

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

unsigned long kernel_task_spawn_on(
    void (*func)(void*), void* ctx,
    unsigned long affinity_mask)
{
    Kernel::Task* t = Kernel::Mm::TAlloc<Kernel::Task, RustAllocTag>("rust");
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

    RustIPIAdapter a{handler, ctx};
    Kernel::IPITask task(RustIPITrampoline, &a);
    Kernel::CpuTable::GetInstance().GetCpu(cpu).QueueIPITask(task);
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

unsigned long kernel_virt_to_phys(const void* virt_addr)
{
    if (!virt_addr)
        return 0;
    return Kernel::Mm::PageTable::GetInstance().VirtToPhys((ulong)virt_addr);
}

int kernel_get_random(unsigned char* buf, unsigned long len)
{
    Kernel::EntropySource* src =
        Kernel::EntropySourceTable::GetInstance().GetDefault();
    if (!src || !buf || len == 0)
        return 0;
    return src->GetRandom(buf, (ulong)len) ? 1 : 0;
}

/* ---- Soft IRQ ---- */

void kernel_softirq_raise(unsigned long type)
{
    Kernel::SoftIrq::GetInstance().Raise(type);
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

class RustMsixHandler : public Kernel::InterruptHandler
{
public:
    Kernel::InterruptHandlerFn Stub;

    void OnInterruptRegister(u8 irq, u8 vector) override
    {
        (void)irq;
        (void)vector;
    }

    Kernel::InterruptHandlerFn GetHandlerFn() override
    {
        return Stub;
    }
};

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

unsigned char kernel_msix_enable_vector(unsigned long handle, unsigned short index,
    void (*isr_fn)())
{
    if (handle == 0 || !isr_fn)
        return 0;
    auto* t = reinterpret_cast<Kernel::MsixTable*>(handle);
    RustMsixHandler adapter;
    adapter.Stub = isr_fn;
    return t->EnableVector(index, adapter);
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

unsigned long kernel_task_spawn_ctx(
    void (*func)(void*), void* ctx)
{
    return kernel_task_spawn(func, ctx);
}

unsigned long kernel_task_spawn_on_ctx(
    void (*func)(void*), void* ctx,
    unsigned long affinity_mask)
{
    return kernel_task_spawn_on(func, ctx, affinity_mask);
}

} /* extern "C" */

/* ---- Legacy (INTx) interrupts ---- */

static const ulong RustIrqSlotCount = 8;
static const u8 RustIrqVectorBase = 0x38;

struct RustIrqSlot
{
    void (*Handler)(void*);
    void* Ctx;
    u8 Vector;
    bool Used;
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
        RustIrqLock.ReadLock();
        auto handler = RustIrqSlots[SlotIndex].Handler;
        auto uctx = RustIrqSlots[SlotIndex].Ctx;
        RustIrqLock.ReadUnlock();
        if (handler)
            handler(uctx);
    }
};

static RustLegacyHandler RustLegacyHandlers[RustIrqSlotCount];

extern "C" {

void RustInterruptDispatch(Kernel::Context* ctx, int slot)
{
    (void)ctx;
    if (slot < 0 || (ulong)slot >= RustIrqSlotCount)
    {
        Kernel::Lapic::EOI();
        return;
    }
    RustIrqLock.ReadLock();
    auto handler = RustIrqSlots[slot].Handler;
    auto uctx = RustIrqSlots[slot].Ctx;
    RustIrqLock.ReadUnlock();
    if (handler)
        handler(uctx);
    Kernel::Lapic::EOI();
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

            RustLegacyHandlers[i].SlotIndex = i;

            u8 vector = (u8)(RustIrqVectorBase + i);
            Kernel::Interrupt::RegisterLevel(RustLegacyHandlers[i], irq_line, vector);

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
        RustTimerLock.ReadLock();
        auto handler = RustTimerSlots[SlotIndex].Handler;
        auto ctx = RustTimerSlots[SlotIndex].Ctx;
        RustTimerLock.ReadUnlock();
        if (handler)
            handler(ctx);
    }
};

static RustTimerAdapter RustTimerAdapters[RustTimerSlotCount];

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

            RustTimerAdapters[i].SlotIndex = i;

            Stdlib::Time period(period_ns);
            if (!Kernel::TimerTable::GetInstance().StartTimer(
                    RustTimerAdapters[i], period))
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
    ulong flags = RustTimerLock.WriteLockIrqSave();
    Kernel::TimerTable::GetInstance().StopTimer(RustTimerAdapters[i]);
    RustTimerSlots[i].Handler = nullptr;
    RustTimerSlots[i].Ctx = nullptr;
    RustTimerSlots[i].Active = false;
    RustTimerLock.WriteUnlockIrqRestore(flags);
}

} /* extern "C" */

/* ---- MSI-X callback slots ---- */

static const ulong RustMsixSlotCount = 16;

struct RustMsixSlot
{
    void (*Handler)(void*);
    void* Ctx;
    bool Used;
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
};

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
};

static RustMsixSlotHandler RustMsixSlotHandlers[RustMsixSlotCount];

extern "C" {

void RustMsixDispatch(Kernel::Context* ctx, int slot)
{
    (void)ctx;
    if (slot < 0 || (ulong)slot >= RustMsixSlotCount)
    {
        Kernel::Lapic::EOI();
        return;
    }
    RustMsixLock.ReadLock();
    auto handler = RustMsixSlots[slot].Handler;
    auto uctx = RustMsixSlots[slot].Ctx;
    RustMsixLock.ReadUnlock();
    if (handler)
        handler(uctx);
    Kernel::Lapic::EOI();
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

            RustMsixSlotHandlers[i].SlotIndex = i;

            auto* t = reinterpret_cast<Kernel::MsixTable*>(msix_handle);
            u8 vector = t->EnableVector(msix_index, RustMsixSlotHandlers[i]);
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
}

} /* extern "C" */

/* ---- Block device bridge ---- */

struct RustBlockDeviceOps
{
    const char* Name;
    unsigned long long Capacity;
    unsigned long long SectorSize;
    int (*ReadSectors)(void* ctx, unsigned long long sector,
                       void* buf, unsigned int count);
    int (*WriteSectors)(void* ctx, unsigned long long sector,
                        const void* buf, unsigned int count, int fua);
    int (*Flush)(void* ctx);    /* may be nullptr */
    void* Ctx;
};

class RustBlockDevice : public Kernel::BlockDevice
{
public:
    RustBlockDeviceOps Ops;

    const char* GetName() override { return Ops.Name; }
    u64 GetCapacity() override { return (u64)Ops.Capacity; }
    u64 GetSectorSize() override { return (u64)Ops.SectorSize; }

    bool ReadSectors(u64 sector, void* buf, u32 count) override
    {
        return Ops.ReadSectors(Ops.Ctx, (unsigned long long)sector, buf,
                               (unsigned int)count) != 0;
    }

    bool WriteSectors(u64 sector, const void* buf, u32 count, bool fua) override
    {
        return Ops.WriteSectors(Ops.Ctx, (unsigned long long)sector, buf,
                                (unsigned int)count, fua ? 1 : 0) != 0;
    }

    bool Flush() override
    {
        if (!Ops.Flush)
            return true;
        return Ops.Flush(Ops.Ctx) != 0;
    }
};

extern "C" {

unsigned long kernel_blockdev_register(const RustBlockDeviceOps* ops)
{
    if (!ops || !ops->Name || !ops->ReadSectors || !ops->WriteSectors)
        return 0;

    RustBlockDevice* dev = Kernel::Mm::TAlloc<RustBlockDevice, RustAllocTag>();
    if (!dev)
        return 0;

    dev->Ops = *ops;

    if (!Kernel::BlockDeviceTable::GetInstance().Register(dev))
    {
        dev->~RustBlockDevice();
        Kernel::Mm::Free(dev);
        return 0;
    }

    return (unsigned long)dev;
}

} /* extern "C" */

/* ---- Net device bridge ---- */

struct RustNetDeviceOps
{
    const char* Name;
    unsigned char Mac[6];
    void (*FlushTx)(void* ctx);
    void (*ProcessRx)(void* ctx);
    void* Ctx;
};

class RustNetDevice : public Kernel::NetDevice
{
public:
    RustNetDeviceOps Ops;
    u64 TxPackets;
    u64 RxPackets;
    u64 RxDropped;

    const char* GetName() override { return Ops.Name; }
    u64 GetTxPackets() override { return TxPackets; }
    u64 GetRxPackets() override { return RxPackets; }
    u64 GetRxDropped() override { return RxDropped; }

    /* Called while TxQueueLock is held by base SubmitTx. */
    void FlushTx() override
    {
        Ops.FlushTx(Ops.Ctx);
    }

    void ProcessRx() override
    {
        Ops.ProcessRx(Ops.Ctx);
    }

    /* Dequeue one TX frame without acquiring TxQueueLock (lock already held). */
    Kernel::NetFrame* TxDequeue()
    {
        if (TxQueue.IsEmpty())
            return nullptr;
        Stdlib::ListEntry* entry = TxQueue.RemoveHead();
        TxCount--;
        TxPackets++;
        return CONTAINING_RECORD(entry, Kernel::NetFrame, Link);
    }
};

extern "C" {

unsigned long kernel_netdev_register(const RustNetDeviceOps* ops)
{
    if (!ops || !ops->Name || !ops->FlushTx || !ops->ProcessRx)
        return 0;

    /* Heap-allocate so that NetDevice's constructor (which calls
       ListEntry::Init on TxQueue/RxQueue) runs reliably -- static arrays
       of non-trivial objects are forbidden in this freestanding kernel. */
    RustNetDevice* dev = Kernel::Mm::TAlloc<RustNetDevice, RustAllocTag>();
    if (!dev)
        return 0;

    dev->Ops = *ops;
    dev->TxPackets = 0;
    dev->RxPackets = 0;
    dev->RxDropped = 0;

    Kernel::Net::MacAddress mac;
    Stdlib::MemCpy(mac.Bytes, ops->Mac, 6);
    dev->SetMac(mac);

    if (!Kernel::NetDeviceTable::GetInstance().Register(dev))
    {
        dev->~RustNetDevice();
        Kernel::Mm::Free(dev);
        return 0;
    }

    return (unsigned long)dev;
}

static RustNetDevice* NetDevFromHandle(unsigned long handle)
{
    return reinterpret_cast<RustNetDevice*>(handle);
}

void kernel_netdev_set_ip(unsigned long handle, unsigned int ip)
{
    if (!handle) return;
    NetDevFromHandle(handle)->SetIp(Kernel::Net::IpAddress(ip));
}

void kernel_netdev_set_mask(unsigned long handle, unsigned int mask)
{
    if (!handle) return;
    NetDevFromHandle(handle)->SetSubnetMask(Kernel::Net::IpAddress(mask));
}

void kernel_netdev_set_gw(unsigned long handle, unsigned int gw)
{
    if (!handle) return;
    NetDevFromHandle(handle)->SetGateway(Kernel::Net::IpAddress(gw));
}

/* Called from inside Rust FlushTx callback. TxQueueLock is already held. */
unsigned long kernel_netdev_tx_dequeue(unsigned long handle)
{
    if (!handle)
        return 0;
    Kernel::NetFrame* frame = NetDevFromHandle(handle)->TxDequeue();
    return (unsigned long)frame;
}

void kernel_netdev_tx_notify(unsigned long handle)
{
    (void)handle;
    /* Placeholder for hardware doorbell. FlushTx returns to SubmitTx
       which raises SoftIrq for any retry if frames remain. */
}

unsigned long kernel_netframe_alloc_rx(unsigned long data_len)
{
    Kernel::NetFrame* frame = Kernel::NetFrame::AllocTx((ulong)data_len);
    if (!frame)
        return 0;
    frame->Direction = Kernel::NetFrame::Rx;
    return (unsigned long)frame;
}

void kernel_netdev_enqueue_rx(unsigned long dev_handle, unsigned long frame_handle)
{
    if (!dev_handle || !frame_handle)
        return;
    RustNetDevice* dev = NetDevFromHandle(dev_handle);
    auto* frame = reinterpret_cast<Kernel::NetFrame*>(frame_handle);
    if (!dev->EnqueueRx(frame))
    {
        dev->RxDropped++;
        frame->Put();
    }
    else
    {
        dev->RxPackets++;
    }
}

unsigned char* kernel_netframe_data(unsigned long handle)
{
    if (!handle) return nullptr;
    return reinterpret_cast<Kernel::NetFrame*>(handle)->Data;
}

unsigned long kernel_netframe_data_phys(unsigned long handle)
{
    if (!handle) return 0;
    return reinterpret_cast<Kernel::NetFrame*>(handle)->DataPhys;
}

unsigned long kernel_netframe_len(unsigned long handle)
{
    if (!handle) return 0;
    return reinterpret_cast<Kernel::NetFrame*>(handle)->Length;
}

void kernel_netframe_set_len(unsigned long handle, unsigned long len)
{
    if (!handle) return;
    reinterpret_cast<Kernel::NetFrame*>(handle)->Length = len;
}

void kernel_netframe_put(unsigned long handle)
{
    if (!handle) return;
    reinterpret_cast<Kernel::NetFrame*>(handle)->Put();
}

} /* extern "C" */
