#pragma once

#include <include/types.h>

namespace stdlib
{

inline u16 cpu_to_be16(u16 v) { return (u16)((v >> 8) | (v << 8)); }
inline u32 cpu_to_be32(u32 v)
{
    return ((v >> 24) & 0xFF) |
           ((v >> 8) & 0xFF00) |
           ((v << 8) & 0xFF0000) |
           ((v << 24) & 0xFF000000);
}
inline u16 be16_to_cpu(u16 v) { return cpu_to_be16(v); }
inline u32 be32_to_cpu(u32 v) { return cpu_to_be32(v); }

} /* namespace Stdlib */
