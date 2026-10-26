#include "checksum.h"

namespace stdlib
{

namespace
{

/* The reflected polynomial of CRC-32 (IEEE 802.3) */
const u32 CRC32_POLY = 0xEDB88320;
const u32 CRC32_TABLE_SIZE = 256;
const u32 BITS_PER_BYTE = 8;

struct Crc32Table
{
    u32 value[CRC32_TABLE_SIZE];
};

/* Made by the compiler. The table used to be filled on first use behind a
   plain bool, with neither a lock nor a barrier: two CPUs could both fill
   it, and -- the stores free to be reordered once the kernel was built
   optimised -- one could see the flag set before the table it stood for. A
   table that is constant has no first use. */
constexpr Crc32Table make_crc32_table()
{
    Crc32Table table = {};
    for (u32 i = 0; i < CRC32_TABLE_SIZE; i++)
    {
        u32 crc = i;
        for (u32 j = 0; j < BITS_PER_BYTE; j++)
            crc = (crc & 1) ? (crc >> 1) ^ CRC32_POLY : crc >> 1;
        table.value[i] = crc;
    }
    return table;
}

constexpr Crc32Table THE_CRC32_TABLE = make_crc32_table();

}

u32 crc32_update(u32 crc, const void* data, ulong size)
{
    const u8* p = static_cast<const u8*>(data);
    crc = crc ^ 0xFFFFFFFF;

    for (ulong i = 0; i < size; i++)
    {
        crc = THE_CRC32_TABLE.value[(crc ^ p[i]) & 0xFF] ^ (crc >> BITS_PER_BYTE);
    }

    return crc ^ 0xFFFFFFFF;
}

u32 crc32(const void* data, ulong size)
{
    return crc32_update(0, data, size);
}

}
