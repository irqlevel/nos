#pragma once

#include <include/types.h>
#include <include/const.h>

namespace Stdlib
{

class Bitmap
{
public:
    Bitmap(void* addr, ulong bitCount);
    ~Bitmap();

    void SetBit(ulong bitNum);
    void ClearBit(ulong bitNum);
    long FindSetZeroBit();

private:
    Bitmap(const Bitmap& other) = delete;
    Bitmap(Bitmap&& other) = delete;
    Bitmap& operator=(const Bitmap& other) = delete;
    Bitmap& operator=(Bitmap&& other) = delete;

    void* Addr;
    ulong BitCount;
};

}