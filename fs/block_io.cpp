#include "block_io.h"

#include <lib/stdlib.h>
#include <kernel/trace.h>

namespace Kernel
{

BlockIo::BlockIo(BlockDevice* dev, u32 blockSize)
    : Dev(dev)
    , BlkSize(blockSize)
    , SectorsPerBlock(0)
{
    if (Dev != nullptr && Dev->GetSectorSize() > 0)
        SectorsPerBlock = BlkSize / (u32)Dev->GetSectorSize();
}

BlockIo::~BlockIo()
{
}

bool BlockIo::ReadBlock(u32 blockIdx, void* buf)
{
    if (Dev == nullptr || SectorsPerBlock == 0)
    {
        Trace(0, "BlockIo::ReadBlock: dev null or bad config");
        return false;
    }

    u64 startSector = (u64)blockIdx * SectorsPerBlock;
    u8* p = static_cast<u8*>(buf);
    u32 sectorSize = (u32)Dev->GetSectorSize();

    for (u32 i = 0; i < SectorsPerBlock; i++)
    {
        if (!Dev->ReadSector(startSector + i, p))
        {
            Trace(0, "BlockIo::ReadBlock: sector %u failed (block %u)",
                  (ulong)(startSector + i), (ulong)blockIdx);
            return false;
        }
        p += sectorSize;
    }

    return true;
}

bool BlockIo::WriteBlock(u32 blockIdx, const void* buf)
{
    if (Dev == nullptr || SectorsPerBlock == 0)
    {
        Trace(0, "BlockIo::WriteBlock: dev null or bad config");
        return false;
    }

    u64 startSector = (u64)blockIdx * SectorsPerBlock;
    const u8* p = static_cast<const u8*>(buf);
    u32 sectorSize = (u32)Dev->GetSectorSize();

    for (u32 i = 0; i < SectorsPerBlock; i++)
    {
        if (!Dev->WriteSector(startSector + i, p))
        {
            Trace(0, "BlockIo::WriteBlock: sector %u failed (block %u)",
                  (ulong)(startSector + i), (ulong)blockIdx);
            return false;
        }
        p += sectorSize;
    }

    return true;
}

BlockDevice* BlockIo::GetDevice()
{
    return Dev;
}

}
