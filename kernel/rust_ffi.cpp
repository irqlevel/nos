#include "trace.h"
#include "panic.h"
#include "time.h"
#include "mutex.h"
#include "spin_lock.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "entropy.h"
#include "idt.h"
#include "interrupt.h"
#include "asm.h"
#include "softirq.h"
#include <mm/new.h>
#include <mm/page_allocator.h>
#include <mm/page_table.h>
#include <lib/stdlib.h>
#include <drivers/pci.h>
#include <drivers/msix.h>
#include <drivers/lapic.h>

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
        auto& slot = RustIrqSlots[SlotIndex];
        if (slot.Handler)
            slot.Handler(slot.Ctx);
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
    auto& s = RustIrqSlots[slot];
    if (s.Handler)
        s.Handler(s.Ctx);
    Kernel::Lapic::EOI();
}

unsigned long kernel_interrupt_register_level(
    unsigned char irq_line,
    void (*handler)(void*), void* ctx,
    unsigned char* out_vector)
{
    if (!handler || !out_vector)
        return 0;

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
            return i + 1;
        }
    }

    Trace(0, "kernel_interrupt_register_level: no free slots");
    return 0;
}

void kernel_interrupt_unregister(unsigned long handle)
{
    if (handle == 0 || handle > RustIrqSlotCount)
        return;
    ulong i = handle - 1;
    RustIrqSlots[i].Handler = nullptr;
    RustIrqSlots[i].Ctx = nullptr;
    RustIrqSlots[i].Used = false;
    RustIrqSlots[i].Vector = 0;
}

} /* extern "C" */
