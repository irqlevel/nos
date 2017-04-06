#pragma once

#include "stdlib.h"


namespace Kernel
{


namespace Core
{

class Lapic final
{
public:
    static Lapic& GetInstance()
    {
        static Lapic instance;
        return instance;
    }

    void Enable();

    void EOI(u8 vector);

    u8 GetId();

private:
    Lapic();
    ~Lapic();
    Lapic(const Lapic& other) = delete;
    Lapic(Lapic&& other) = delete;
    Lapic& operator=(const Lapic& other) = delete;
    Lapic& operator=(Lapic&& other) = delete;

    u32 ReadReg(ulong index);
    void WriteReg(ulong index, u32 value);
    volatile void* GetRegBase(ulong index);

    bool CheckIsr(u8 vector);

    void *BaseAddress;  
 
    static const ulong ApicIdIndex = 2;
    static const ulong TprIndex = 0x8;
    static const ulong LdrIndex = 0xD;
    static const ulong DfrIndex = 0xE;
    static const ulong EoiIndex = 0xB;
    static const ulong SpuriousInterruptVectorIndex = 0xF;
    static const ulong IsrBaseIndex = 0x10;

    static const ulong BaseMsr = 0x1B;

};

}
}