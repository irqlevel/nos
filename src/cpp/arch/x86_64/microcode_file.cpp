#include "microcode.h"

#include <lib/stdlib.h>

/* The reading of a file of Intel's microcode updates: what a bootloader hands
   the kernel from a disk, every size and checksum in it the file's to say,
   and so held to the format before any of it is believed. Pure -- nothing of
   the CPU's -- so that cpp-fuzz.py compiles it alone (`ucode`). */

namespace Kernel
{
namespace Microcode
{

namespace
{

/* An update's header, as the SDM lays it out (Vol. 3A, Table 10-7) */
struct Header
{
    u32 HeaderVersion;
    u32 Revision;
    u32 Date;
    u32 Signature;
    u32 Checksum;
    u32 LoaderRevision;
    u32 ProcessorFlags;
    u32 DataSize;
    u32 TotalSize;
    u32 Reserved[3];
};
static_assert(sizeof(Header) == 48, "an update's header is 48 bytes");

/* The extended signature table's header, and one of its signatures */
struct ExtHeader
{
    u32 Count;
    u32 Checksum;
    u32 Reserved[3];
};
static_assert(sizeof(ExtHeader) == 20, "the extended table's header is 20 bytes");

struct ExtSignature
{
    u32 Signature;
    u32 ProcessorFlags;
    u32 Checksum;
};
static_assert(sizeof(ExtSignature) == 12, "an extended signature is 12 bytes");

const u32 HeaderVersion = 1;
const u32 LoaderRevision = 1;
/* A data size of 0 is the original format: 2000 bytes of data, 2048 in all */
const ulong DefaultDataSize = 2000;
const ulong DefaultTotalSize = 2048;
/* Every update's total size is a whole number of KiB */
const ulong TotalSizeUnit = 1024;
const ulong DwordSize = 4;

/* The sum of the dwords of `bytes` bytes at `p`, as the checksums are made:
   every update's, header and data, and every extended table's, sum to 0 */
u32 SumDwords(const u8* p, ulong bytes)
{
    u32 sum = 0;
    for (ulong i = 0; i + DwordSize <= bytes; i += DwordSize)
    {
        u32 dword;
        Stdlib::MemCpy(&dword, p + i, DwordSize);
        sum = sum + dword;
    }
    return sum;
}

/* Whether a CPU of `sig` and `pf` is one an update -- or an extended
   signature -- of `s` and `f` is for: the signature, and a platform both
   have, or neither naming one */
bool Matches(u32 sig, u32 pf, u32 s, u32 f)
{
    if (sig != s)
        return false;
    if (pf == 0 && f == 0)
        return true;
    return (pf & f) != 0;
}

}

bool Choose(const u8* file, ulong len, u32 sig, u32 pf, u32 current, Choice* out)
{
    bool found = false;
    ulong off = 0;

    /* off <= len throughout: it moves only by an update's total size, which
       is checked against what is left before it is believed */
    while (len - off >= sizeof(Header))
    {
        const u8* update = file + off;
        Header h;
        Stdlib::MemCpy(&h, update, sizeof(h));
        if (h.HeaderVersion != HeaderVersion || h.LoaderRevision != LoaderRevision)
            break;

        ulong data = (h.DataSize != 0) ? h.DataSize : DefaultDataSize;
        ulong total = (h.DataSize != 0) ? h.TotalSize : DefaultTotalSize;
        if (data % DwordSize != 0 || total % TotalSizeUnit != 0)
            break;
        if (total < sizeof(Header) || total - sizeof(Header) < data || total > len - off)
            break;
        if (SumDwords(update, sizeof(Header) + data) != 0)
            break;

        bool fits = Matches(sig, pf, h.Signature, h.ProcessorFlags);

        /* What is past the data is an extended signature table, exactly, or
           nothing: other signatures and platforms the same update is for */
        ulong ext = total - sizeof(Header) - data;
        if (ext != 0)
        {
            if (ext < sizeof(ExtHeader) || (ext - sizeof(ExtHeader)) % sizeof(ExtSignature) != 0)
                break;
            const u8* table = update + sizeof(Header) + data;
            ExtHeader e;
            Stdlib::MemCpy(&e, table, sizeof(e));
            if (e.Count != (ext - sizeof(ExtHeader)) / sizeof(ExtSignature))
                break;
            if (SumDwords(table, ext) != 0)
                break;

            bool sound = true;
            for (ulong i = 0; i < e.Count; i++)
            {
                ExtSignature es;
                Stdlib::MemCpy(&es, table + sizeof(ExtHeader) + i * sizeof(ExtSignature), sizeof(es));
                /* Its checksum is the update's, its signature and platforms
                   in place of the header's */
                if (h.Signature + h.ProcessorFlags + h.Checksum != es.Signature + es.ProcessorFlags + es.Checksum)
                {
                    sound = false;
                    break;
                }
                if (Matches(sig, pf, es.Signature, es.ProcessorFlags))
                    fits = true;
            }
            if (!sound)
                break;
        }

        if (fits && h.Revision > current && (!found || h.Revision > out->Revision))
        {
            out->Offset = off;
            out->Size = total;
            out->Revision = h.Revision;
            out->Date = h.Date;
            found = true;
        }
        off += total;
    }

    return found;
}

}
}
