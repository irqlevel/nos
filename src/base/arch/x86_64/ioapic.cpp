#include "ioapic.h"
#include <drivers/acpi.h>

#include "asm.h"
#include <kernel/klog.h>
#include <mm/mmio.h>
#include <lib/lock.h>

namespace kernel
{

IoApic::IoApic()
    : pins_(0)
    , base_address_(nullptr)
{
    base_address_ = Acpi::get_instance().get_io_apic_address();

    klog(IO_APIC_LL, "IO Apic addr 0x%p", base_address_);
}

IoApic::~IoApic()
{
}

u32 IoApic::read_register(u8 reg)
{
    stdlib::AutoLock lock(op_lock_);

    mm::MmIo::write32(stdlib::mem_add(base_address_, REG_SEL), reg);

    hal::dma_wmb();

    return mm::MmIo::read32(stdlib::mem_add(base_address_, REG_WIN));
}

void IoApic::write_register(u8 reg, u32 value)
{
    stdlib::AutoLock lock(op_lock_);

    mm::MmIo::write32(stdlib::mem_add(base_address_, REG_SEL), reg);

    hal::dma_wmb();

    mm::MmIo::write32(stdlib::mem_add(base_address_, REG_WIN), value);

    hal::dma_wmb();
}

void IoApic::enable()
{
    write_register(APIC_ID, 0x0);

    u32 ver = read_register(APIC_VER);
    size_t pins = ((ver >> 16) & 0xFF) + 1;
    if (pins > MAX_PINS)
    {
        klog(0, "IO Apic says %lu pins, more than its registers reach: %lu taken", pins, MAX_PINS);
        pins = MAX_PINS;
    }

    // Disable all entries
    for (size_t i = 0; i < pins; i++)
    {
        set_entry((u8)i, 1 << MASKED_SHIFT);
    }
    pins_ = pins;

    klog(IO_APIC_LL, "IO Apic ver 0x%X pins %lu", ver, pins);
}

bool IoApic::has_pin(u8 irq) const
{
    return irq < pins_;
}

void IoApic::mask_irq(u8 irq)
{
    if (!has_pin(irq))
        return;

    /* The low register read and written back under one hold of the lock,
       the select register still naming it */
    stdlib::AutoLock lock(op_lock_);

    mm::MmIo::write32(stdlib::mem_add(base_address_, REG_SEL), (u32)(RED_TBL + 2 * irq));

    hal::dma_wmb();

    u32 low = mm::MmIo::read32(stdlib::mem_add(base_address_, REG_WIN));
    mm::MmIo::write32(stdlib::mem_add(base_address_, REG_WIN), low | (1U << MASKED_SHIFT));

    hal::dma_wmb();
}

void IoApic::set_entry(u8 index, u64 data)
{
    /* High (destination) first: writing the low half first can briefly
       unmask the entry with a stale destination */
    write_register(RED_TBL + 2 * index + 1, (u32)(data >> 32));
    write_register(RED_TBL + 2 * index, (u32)data);
}

u64 IoApic::check_dest(u8 irq, u64 apic_id)
{
    if (can_target(apic_id))
        return apic_id;

    /* Never silently alias onto whatever apic id the low four bits happen to
       name -- that is how an interrupt goes missing for good. Fall back to
       the boot CPU, which is always addressable, and say so. */
    klog(0, "IoApic: irq 0x%lX apic_id %lu exceeds physical destination range %lu, using 0",
        (ulong)irq, (ulong)apic_id, (ulong)MAX_PHYSICAL_DEST);

    return 0;
}

bool IoApic::set_irq(u8 irq, u64 apic_id, u8 vector)
{
    u64 data = 0;

    if (!has_pin(irq))
        return false;

    apic_id = check_dest(irq, apic_id);

    data |= vector; // interrupt vector
    data |= DM_FIXED << DELIV_MODE_SHIFT; // delivery mode: fixed
    data |= 0 << DEST_MODE_SHIFT; // destination: physical
    data |= 0 << DELIV_STATUS_SHIFT; // delivery status : relaxed
    data |= 0 << POLARITY_SHIFT; // pin polarity: active high
    data |= TRIGGER_EDGE << TRIGGER_MODE_SHIFT; // trigger mode: edge
    data |= 0 << MASKED_SHIFT; // disable: no
    data |= apic_id << DEST_SHIFT; //destination id

    klog(IO_APIC_LL, "set_irq irq 0x%lX apic_id 0x%lX vector 0x%lX data 0x%lX",
        (ulong)irq, (ulong)apic_id, (ulong)vector, (ulong)data);

    set_entry(irq, data);
    return true;
}

u64 IoApic::set_irq_destination(u8 irq, u64 apic_id)
{
    /* Only a line register_level programmed is retargeted */
    if (BUG_ON(!has_pin(irq)))
        return apic_id;

    apic_id = check_dest(irq, apic_id);

    /* The destination id occupies bits 56-63 of the redirection entry,
       i.e. bits 24-31 of the high register. A single 32-bit write
       switches the destination without touching mask/trigger/vector. */
    u32 high = read_register(RED_TBL + 2 * irq + 1);
    high = (high & 0x00FFFFFFU) | ((u32)apic_id << 24);
    write_register(RED_TBL + 2 * irq + 1, high);

    return apic_id;
}

bool IoApic::set_irq_level(u8 irq, u64 apic_id, u8 vector, bool active_high)
{
    u64 data = 0;

    if (!has_pin(irq))
        return false;

    apic_id = check_dest(irq, apic_id);

    data |= vector; // interrupt vector
    data |= DM_FIXED << DELIV_MODE_SHIFT; // delivery mode: fixed
    data |= 0 << DEST_MODE_SHIFT; // destination: physical
    data |= 0 << DELIV_STATUS_SHIFT; // delivery status : relaxed
    data |= (active_high ? 0UL : 1UL) << POLARITY_SHIFT; // pin polarity
    data |= TRIGGER_LEVEL << TRIGGER_MODE_SHIFT; // trigger mode: level
    data |= 0 << MASKED_SHIFT; // disable: no
    data |= apic_id << DEST_SHIFT; //destination id

    klog(IO_APIC_LL, "set_irq_level irq 0x%lX apic_id 0x%lX vector 0x%lX data 0x%lX active_high %lu",
        (ulong)irq, (ulong)apic_id, (ulong)vector, (ulong)data, (ulong)active_high);

    set_entry(irq, data);
    return true;
}

}