#pragma once

#include <include/types.h>

namespace Stdlib
{

inline u16 Htons(u16 v) { return (u16)((v >> 8) | (v << 8)); }
inline u32 Htonl(u32 v)
{
    return ((v >> 24) & 0xFF) |
           ((v >> 8) & 0xFF00) |
           ((v << 8) & 0xFF0000) |
           ((v << 24) & 0xFF000000);
}
inline u16 Ntohs(u16 v) { return Htons(v); }
inline u32 Ntohl(u32 v) { return Htonl(v); }

} /* namespace Stdlib */
