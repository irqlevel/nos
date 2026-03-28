#include "bitmap.h"
#include "stdlib.h"
#include <kernel/panic.h>

namespace Stdlib
{

Bitmap::Bitmap(void* addr, ulong bitCount)
    : Addr(addr)
    , BitCount(bitCount)
{
    BugOn((ulong)addr % sizeof(ulong));
}

Bitmap::~Bitmap()
{
}

void Bitmap::SetBit(ulong bitNum)
{
    BugOn(bitNum >= BitCount);

    ulong offset = bitNum / (8 * sizeof(ulong));
    ulong shift = bitNum % (8 * sizeof(ulong));

    *((ulong *)Addr + offset) |= (1UL << shift);
}

void Bitmap::ClearBit(ulong bitNum)
{
    BugOn(bitNum >= BitCount);

    ulong offset = bitNum / (8 * sizeof(ulong));
    ulong shift = bitNum % (8 * sizeof(ulong));

    *((ulong *)Addr + offset) &= ~(1UL << shift);    
}

long Bitmap::FindSetZeroBit()
{
    ulong* curr = (ulong*)Addr;
    long restBitCount = BitCount;

    while (restBitCount > 0)
    {
        ulong value = *curr;
        if (value != ~0UL)
        {
            for (ulong shift = 0; shift < Min<ulong>(8 * sizeof(ulong), restBitCount); shift++)
            {
                if (!(value & (1UL << shift)))
                {
                    *curr |= (1UL << shift);
                    long bitNum = 8 * ((ulong)curr - (ulong)Addr) + shift;
                    BugOn((ulong)bitNum >= BitCount);
                    return bitNum;
                }
            }
        }
        restBitCount -= 8 * sizeof(ulong);
        curr++;
    }
    return -1;
}

}