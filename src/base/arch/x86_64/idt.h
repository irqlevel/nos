#pragma once

#include "idt_descriptor.h"
#include <kernel/atomic.h>

#include <include/types.h>

namespace kernel
{

struct Context;

class Idt final
{
public:

    static Idt& get_instance()
    {
        static Idt instance;
        return instance;
    }

    void save();

    IdtDescriptor get_descriptor(u16 index);

    void set_descriptor(u16 index, const IdtDescriptor& desc);

    u32 get_base();

    u16 get_limit();

    void dummy_interrupt(Context* ctx, u8 vector);

private:
    Idt();
    ~Idt();

    Idt(const Idt& other) = delete;
    Idt(Idt&& other) = delete;

    Idt& operator=(const Idt& other) = delete;
    Idt& operator=(Idt&& other) = delete;

    /* Slots below this are CPU exceptions (Intel reserved); device
       interrupts start at 0x20 */
    static const u8 FIRST_DEVICE_VECTOR = 0x20;

    /* Report a stray interrupt this many times, then only count it: a
       level-triggered source nobody can acknowledge repeats forever */
    static const long MAX_DUMMY_REPORTS = 8;

    struct TableDesc {
	    u16 limit;
	    u64 base;
    } __attribute((packed));

    u64 base_;
    u16 limit_;

    IdtDescriptor entry_[256];
    Atomic dummy_interrupt_counter_;
};

}
