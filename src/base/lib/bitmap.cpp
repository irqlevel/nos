#include "bitmap.h"
#include "stdlib.h"
#include <kernel/panic.h>

namespace stdlib
{

Bitmap::Bitmap(void* addr, ulong bit_count)
    : addr_(addr)
    , bit_count_(bit_count)
{
    BUG_ON((ulong)addr % sizeof(ulong));
}

Bitmap::~Bitmap()
{
}

void Bitmap::set_bit(ulong bit_num)
{
    BUG_ON(bit_num >= bit_count_);

    ulong offset = bit_num / (8 * sizeof(ulong));
    ulong shift = bit_num % (8 * sizeof(ulong));

    *((ulong *)addr_ + offset) |= (1UL << shift);
}

void Bitmap::clear_bit(ulong bit_num)
{
    BUG_ON(bit_num >= bit_count_);

    ulong offset = bit_num / (8 * sizeof(ulong));
    ulong shift = bit_num % (8 * sizeof(ulong));

    *((ulong *)addr_ + offset) &= ~(1UL << shift);    
}

bool Bitmap::test_bit(ulong bit_num)
{
    BUG_ON(bit_num >= bit_count_);

    ulong offset = bit_num / (8 * sizeof(ulong));
    ulong shift = bit_num % (8 * sizeof(ulong));

    return (*((ulong *)addr_ + offset) & (1UL << shift)) ? true : false;
}

long Bitmap::find_set_zero_bit()
{
    ulong* curr = (ulong*)addr_;
    long rest_bit_count = bit_count_;

    while (rest_bit_count > 0)
    {
        ulong value = *curr;
        if (value != ~0UL)
        {
            for (ulong shift = 0; shift < min<ulong>(8 * sizeof(ulong), rest_bit_count); shift++)
            {
                if (!(value & (1UL << shift)))
                {
                    *curr |= (1UL << shift);
                    long bit_num = 8 * ((ulong)curr - (ulong)addr_) + shift;
                    BUG_ON((ulong)bit_num >= bit_count_);
                    return bit_num;
                }
            }
        }
        rest_bit_count -= 8 * sizeof(ulong);
        curr++;
    }
    return -1;
}

}