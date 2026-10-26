#pragma once

#include <include/types.h>

namespace stdlib
{

u32 crc32(const void* data, ulong size);

/* Streaming form: start from 0, feed the data in any pieces, and the result
   is what Crc32 would give over the whole of it. */
u32 crc32_update(u32 crc, const void* data, ulong size);

}
