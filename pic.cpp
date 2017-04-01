#include "pic.h"
#include "helpers32.h"

namespace Kernel
{

namespace Core
{

Pic::Pic()
{
}

Pic::~Pic()
{
}

void Pic::IoWait()
{
    for (int i = 0; i < 1000; i++)
    {
        pause_32();
    }
}   

void Pic::Remap(int offset1, int offset2)
{
	u8 a1, a2;
 
	a1 = inb(PIC1_DATA);                        // save masks
	a2 = inb(PIC2_DATA);
 
	outb(PIC1_COMMAND, ICW1_INIT+ICW1_ICW4);  // starts the initialization sequence (in cascade mode)
	IoWait();
	outb(PIC2_COMMAND, ICW1_INIT+ICW1_ICW4);
	IoWait();
	outb(PIC1_DATA, offset1);                 // ICW2: Master PIC vector offset
	IoWait();
	outb(PIC2_DATA, offset2);                 // ICW2: Slave PIC vector offset
	IoWait();
	outb(PIC1_DATA, 4);                       // ICW3: tell Master PIC that there is a slave PIC at IRQ2 (0000 0100)
	IoWait();
	outb(PIC2_DATA, 2);                       // ICW3: tell Slave PIC its cascade identity (0000 0010)
	IoWait();
 
	outb(PIC1_DATA, ICW4_8086);
	IoWait();
	outb(PIC2_DATA, ICW4_8086);
	IoWait();
 
	outb(PIC1_DATA, a1);   // restore saved masks.
	outb(PIC2_DATA, a2);    
}

void Pic::EOI()
{
    outb(PIC1, PIC_EOI);
}

}

}