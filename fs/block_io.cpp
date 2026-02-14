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
    return Dev->ReadSectors(startSector, buf, SectorsPerBlock);
}

bool BlockIo::WriteBlock(u32 blockIdx, const void* buf, bool fua)
{
    if (Dev == nullptr || SectorsPerBlock == 0)
    {
        Trace(0, "BlockIo::WriteBlock: dev null or bad config");
        return false;
    }

    u64 startSector = (u64)blockIdx * SectorsPerBlock;
    return Dev->WriteSectors(startSector, buf, SectorsPerBlock, fua);
}

bool BlockIo::Flush()
{
    return Dev->Flush();
}

BlockDevice* BlockIo::GetDevice()
{
    return Dev;
}

}
