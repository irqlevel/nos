#pragma once

#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace kernel
{

class IoApic final
{
public:
    static IoApic& get_instance()
    {
        static IoApic instance;
        return instance;
    }

    void enable();

    void set_irq(u8 irq, u64 apic_id, u8 vector);
    void set_irq_level(u8 irq, u64 apic_id, u8 vector, bool active_high);

    /* Rewrite only the destination of an already programmed entry */
    void set_irq_destination(u8 irq, u64 apic_id);

    /* Physical destination mode names the target in bits 59:56 of the
       redirection entry -- four bits, so only apic ids 0..15 are
       addressable. Bits 63:60 are reserved and do not extend the field, so a
       higher id silently aliases: 24 lands on 8, and 50 lands on 2. On a
       hybrid Intel part, whose apic ids run 0,1,8,9,16,17,...,48,49,...,
       several of those aliases name an id no CPU has, and the line is then
       never delivered at all -- an IRQ that simply stops arriving.
       IrqBalance picks IOAPIC destinations through this. */
    static const ulong MAX_PHYSICAL_DEST = 15;

    static bool can_target(ulong apic_id)
    {
        return apic_id <= MAX_PHYSICAL_DEST;
    }

private:
    IoApic();
    ~IoApic();
    IoApic(const IoApic& other) = delete;
    IoApic(IoApic&& other) = delete;
    IoApic& operator=(const IoApic& other) = delete;
    IoApic& operator=(IoApic&& other) = delete;

    u32 read_register(u8 reg);
    void write_register(u8 reg, u32 value);

    void set_entry(u8 index, u64 data);

    /* Backstop for a destination the entry cannot encode; the caller is
       supposed to have filtered it out with can_target() already. */
    u64 check_dest(u8 irq, u64 apic_id);

    static const ulong REG_SEL = 0x0;
    static const ulong REG_WIN = 0x10;

    static const ulong APIC_ID = 0x0;
    static const ulong APIC_VER = 0x1;
    static const ulong APIC_ARB = 0x2;
    static const ulong RED_TBL = 0x10;

    static const ulong DELIV_MODE_SHIFT = 8;
    static const ulong DEST_MODE_SHIFT = 11;
    static const ulong DELIV_STATUS_SHIFT = 12;
    static const ulong POLARITY_SHIFT = 13;
    static const ulong REMOTE_IRR_SHIFT = 14;
    static const ulong TRIGGER_MODE_SHIFT = 15;
    static const ulong MASKED_SHIFT = 16;
    static const ulong DEST_SHIFT = 56;

    static const ulong TRIGGER_EDGE = 0;
    static const ulong TRIGGER_LEVEL = 1;

    static const ulong DM_FIXED = 0x0;

    void *base_address_;
    SpinLock op_lock_;
};

}