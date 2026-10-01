#include "checksum.h"

namespace Stdlib
{

namespace
{

/* The reflected polynomial of CRC-32 (IEEE 802.3) */
const u32 Crc32Poly = 0xEDB88320;
const u32 Crc32TableSize = 256;
const u32 BitsPerByte = 8;

struct Crc32Table
{
    u32 Value[Crc32TableSize];
};

/* Made by the compiler. The table used to be filled on first use behind a
   plain bool, with neither a lock nor a barrier: two CPUs could both fill
   it, and -- the stores free to be reordered once the kernel was built
   optimised -- one could see the flag set before the table it stood for. A
   table that is constant has no first use. */
constexpr Crc32Table MakeCrc32Table()
{
    Crc32Table table = {};
    for (u32 i = 0; i < Crc32TableSize; i++)
    {
        u32 crc = i;
        for (u32 j = 0; j < BitsPerByte; j++)
            crc = (crc & 1) ? (crc >> 1) ^ Crc32Poly : crc >> 1;
        table.Value[i] = crc;
    }
    return table;
}

constexpr Crc32Table TheCrc32Table = MakeCrc32Table();

}

u32 Crc32Update(u32 crc, const void* data, ulong size)
{
    const u8* p = static_cast<const u8*>(data);
    crc = crc ^ 0xFFFFFFFF;

    for (ulong i = 0; i < size; i++)
    {
        crc = TheCrc32Table.Value[(crc ^ p[i]) & 0xFF] ^ (crc >> BitsPerByte);
    }

    return crc ^ 0xFFFFFFFF;
}

u32 Crc32(const void* data, ulong size)
{
    return Crc32Update(0, data, size);
}

}
