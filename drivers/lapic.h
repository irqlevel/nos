#pragma once

#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

class Lapic final
{
public:

    static void Enable();

    static void EOI(u8 vector);

    static u8 GetApicId();

    static void SendInit(u32 apicId);

    static void SendStartup(u32 apicId, u32 vector);

    static void SendIPI(u32 apicId, u32 vector);

private:
    Lapic() = delete;
    ~Lapic() = delete;
    Lapic(const Lapic& other) = delete;
    Lapic(Lapic&& other) = delete;
    Lapic& operator=(const Lapic& other) = delete;
    Lapic& operator=(Lapic&& other) = delete;

    static u32 ReadReg(ulong index);
    static void WriteReg(ulong index, u32 value);
    static void* GetRegBase(ulong index);
    static bool CheckIsr(u8 vector);

    static const ulong ApicIdIndex = 2;
    static const ulong TprIndex = 0x8;
    static const ulong LdrIndex = 0xD;
    static const ulong DfrIndex = 0xE;
    static const ulong EoiIndex = 0xB;
    static const ulong SpIvIndex = 0xF;
    static const ulong IsrBaseIndex = 0x10;

    static const ulong IcrLowIndex = 0x30;
    static const ulong IcrHighIndex = 0x31;

    static const u32 IcrFixed = 0x0;
    static const u32 IcrLowest = 0x100;
    static const u32 IcrSmi = 0x200;
    static const u32 IcrNmi = 0x400;
    static const u32 IcrInit = 0x500;
    static const u32 IcrStartup = 0x600;
    static const u32 IcrPhysical = 0x0;
    static const u32 IcrLogical = 0x800;
    static const u32 IcrIdle = 0x0;
    static const u32 IcrSendPending = 0x1000;
    static const u32 IcrDeassert = 0x0;
    static const u32 IcrAssert = 0x4000;
    static const u32 IcrEdge = 0x0;
    static const u32 IcrLebel = 0x8000;
    static const u32 IcrNoShorthand = 0x0;
    static const u32 IcrSelf = 0x40000;
    static const u32 IcrAllIncludingSelf = 0x80000;
    static const u32 IcrAllExcludingSelf = 0xc0000;
    static const u32 IcrDestinationShift = 24;

    static const ulong BaseMsr = 0x1B;

};

}