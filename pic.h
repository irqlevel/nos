#pragma once

#include "types.h"

namespace Kernel
{


namespace Core
{

class Pic final
{
public:
    static Pic& GetInstance()
    {
        static Pic instance;
        return instance;
    }

    void Remap(int offset1 = 0x20, int offset2 = 0x28);

private:
    Pic();
    ~Pic();
    Pic(const Pic& other) = delete;
    Pic(Pic&& other) = delete;
    Pic& operator=(const Pic& other) = delete;
    Pic& operator=(Pic&& other) = delete;    

    void IoWait();

    static const u8 PIC1 = 0x20;	/* IO base address for master PIC */
    static const u8 PIC2 = 0xA0;	/* IO base address for slave PIC */
    static const u8 PIC1_COMMAND = PIC1;
    static const u8 PIC1_DATA = (PIC1+1);
    static const u8 PIC2_COMMAND = PIC2;
    static const u8 PIC2_DATA = (PIC2+1);
    static const u8 PIC_EOI = 0x20;

    static const u8 ICW1_ICW4 = 0x01;		/* ICW4 (not) needed */
    static const u8 ICW1_SINGLE	= 0x02;		/* Single (cascade) mode */
    static const u8 ICW1_INTERVAL4 = 0x04;		/* Call address interval 4 (8) */
    static const u8 ICW1_LEVEL = 0x08;	/* Level triggered (edge) mode */
    static const u8 ICW1_INIT = 0x10;	/* Initialization - required! */
    
    static const u8 ICW4_8086 = 0x01;	/* 8086/88 (MCS-80/85) mode */
    static const u8 ICW4_AUTO = 0x02;		/* Auto (normal) EOI */
    static const u8 ICW4_BUF_SLAVE = 0x08;		/* Buffered mode/slave */
    static const u8 ICW4_BUF_MASTER = 0x0C;		/* Buffered mode/master */
    static const u8 ICW4_SFNM = 0x10;		/* Special fully nested (not) */

};

}
}