#pragma once

#include <include/types.h>
#include <block/block_device.h>

namespace Kernel
{

class BlockIo
{
public:
    BlockIo(BlockDevice* dev, u32 blockSize = 4096);
    ~BlockIo();

    bool ReadBlock(u32 blockIdx, void* buf);
    bool WriteBlock(u32 blockIdx, const void* buf, bool fua = false);
    bool Flush();

    BlockDevice* GetDevice();

private:
    BlockIo(const BlockIo& other) = delete;
    BlockIo(BlockIo&& other) = delete;
    BlockIo& operator=(const BlockIo& other) = delete;
    BlockIo& operator=(BlockIo&& other) = delete;

    BlockDevice* Dev;
    u32 BlkSize;
    u32 SectorsPerBlock;
};

}
