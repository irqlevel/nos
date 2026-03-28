#include "checksum.h"

namespace Stdlib
{

static u32 Crc32Table[256];
static bool Crc32TableReady;

static void InitCrc32Table()
{
    for (u32 i = 0; i < 256; i++)
    {
        u32 crc = i;
        for (u32 j = 0; j < 8; j++)
        {
            if (crc & 1)
                crc = (crc >> 1) ^ 0xEDB88320;
            else
                crc >>= 1;
        }
        Crc32Table[i] = crc;
    }
    Crc32TableReady = true;
}

u32 Crc32(const void* data, ulong size)
{
    if (!Crc32TableReady)
        InitCrc32Table();

    const u8* p = static_cast<const u8*>(data);
    u32 crc = 0xFFFFFFFF;

    for (ulong i = 0; i < size; i++)
    {
        crc = Crc32Table[(crc ^ p[i]) & 0xFF] ^ (crc >> 8);
    }

    return crc ^ 0xFFFFFFFF;
}

}
