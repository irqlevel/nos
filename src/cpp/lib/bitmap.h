#pragma once

#include <include/types.h>
#include <include/const.h>

namespace stdlib
{

class Bitmap
{
public:
    Bitmap(void* addr, ulong bit_count);
    ~Bitmap();

    void set_bit(ulong bit_num);
    void clear_bit(ulong bit_num);
    bool test_bit(ulong bit_num);
    long find_set_zero_bit();

private:
    Bitmap(const Bitmap& other) = delete;
    Bitmap(Bitmap&& other) = delete;
    Bitmap& operator=(const Bitmap& other) = delete;
    Bitmap& operator=(Bitmap&& other) = delete;

    void* addr_;
    ulong bit_count_;
};

}