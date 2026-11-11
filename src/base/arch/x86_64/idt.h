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

    /* An entry may be written while its vector can be taken on another CPU
       -- an MSI-X entry given another handler, a line's taken off -- and the
       CPU that takes it reads the entry's two halves with no lock. It never
       reads one of each: the upper half is the handler's upper 32 bits, the
       same for every handler in the kernel's text, and the lower half goes
       in with one aligned 8-byte store. A handler whose upper half is
       another's would break that, and is refused here (BUG_ON). */
    void set_descriptor(u16 index, const IdtDescriptor& desc);

    /* The entry as it was before anything was registered at it: the stub of
       a vector nobody has (dummy_interrupt) */
    void reset_descriptor(u16 index);

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
