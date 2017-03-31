#include "8042.h"
#include "memory.h"
#include "trace.h"
#include "helpers32.h"

namespace Kernel
{

namespace Core
{

struct IDT {
        u16 high;
        u16 sel;
        u8 reserved0;
        u8 attr;
        u16 low;

	operator void*() const {
		return (void *) ( (((u32) high) << 16) + ((u32) low) );
	}

	IDT& operator= (void* ptr) {
		high = ((u32) ptr) >> 16;
		sel  = ptr ? 0x8E : 0; /* P, DPL=0, !S, 80386 32bit intgate */
		low  = ((u32) ptr) & 0xFFFF;
		return *this;
	}
} __attribute((packed));

IO8042::IO8042()
    : Buf(new u8[BufSize]),
      BufPtr(Buf)
{
        // FIXME: write interrupt vector setup routines
        IDT *idt;
        u64 idtr = 0;

        get_idt_32(&idtr);
        Trace(0, "idtr = %p%p", idtr >> 32, idtr & 0xFFFFFFFF);
        idt = (IDT *) (idtr >> 16);
        Trace(0, "idt @ %p", idt);
        idt[1] = (void *) &IO8042::Interrupt;
}

IO8042::~IO8042()
{
}

void IO8042::Interrupt(void __attribute((unused)) *frame)
{
        Trace(0, "8042 interrupt!");
	IO8042& io8042 = IO8042::GetInstance();
        __asm__ __volatile__ (
                  "inb %%dx, %%al"
                : "=a" (*io8042.BufPtr++)
                : "d" (io8042.Port)
        );
}

}
}
