#pragma once

#include <include/types.h>
#include <lib/list_entry.h>
#include <kernel/wait_group.h>

namespace Kernel
{

struct BlockRequest
{
    enum Type : u8 { Read, Write, Flush };

    Type RequestType;
    bool Fua;
    u64 Sector;
    u32 SectorCount;
    void* Buffer;           /* Must be page-aligned; DMA directly from/to here */
    bool Success;
    WaitGroup Completion;   /* Init to 1 */
    Stdlib::ListEntry Link;

    BlockRequest()
        : RequestType(Read)
        , Fua(false)
        , Sector(0)
        , SectorCount(0)
        , Buffer(nullptr)
        , Success(false)
        , Completion(1)
    {
    }
};

}
