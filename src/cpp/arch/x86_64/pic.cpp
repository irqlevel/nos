#include "pic.h"

#include "asm.h"

namespace kernel
{

Pic::Pic()
{
}

Pic::~Pic()
{
}

void Pic::io_wait()
{
    for (int i = 0; i < 1000; i++)
    {
        cpu_relax();
    }
}   

void Pic::remap(int offset1, int offset2)
{
	u8 a1, a2;
 
	a1 = inb(PIC1_DATA);                        // save masks
	a2 = inb(PIC2_DATA);
 
	outb(PIC1_COMMAND, ICW1_INIT+ICW1_ICW4);  // starts the initialization sequence (in cascade mode)
	io_wait();
	outb(PIC2_COMMAND, ICW1_INIT+ICW1_ICW4);
	io_wait();
	outb(PIC1_DATA, offset1);                 // ICW2: Master PIC vector offset
	io_wait();
	outb(PIC2_DATA, offset2);                 // ICW2: Slave PIC vector offset
	io_wait();
	outb(PIC1_DATA, 4);                       // ICW3: tell Master PIC that there is a slave PIC at IRQ2 (0000 0100)
	io_wait();
	outb(PIC2_DATA, 2);                       // ICW3: tell Slave PIC its cascade identity (0000 0010)
	io_wait();
 
	outb(PIC1_DATA, ICW4_8086);
	io_wait();
	outb(PIC2_DATA, ICW4_8086);
	io_wait();
 
	outb(PIC1_DATA, a1);   // restore saved masks.
	outb(PIC2_DATA, a2);    
}

void Pic::eoi(int irq)
{
    /* Slave-PIC IRQs (8-15) need an EOI to the slave as well */
    if (irq >= 8)
        outb(PIC2, PIC_EOI);
    outb(PIC1, PIC_EOI);
}

void Pic::disable()
{
	outb(PIC2_DATA, 0xFF);
	outb(PIC1_DATA, 0xFF);
}

}