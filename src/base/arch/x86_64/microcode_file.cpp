#include "microcode.h"

#include <lib/stdlib.h>

/* The reading of a file of Intel's microcode updates: what a bootloader hands
   the kernel from a disk, every size and checksum in it the file's to say,
   and so held to the format before any of it is believed. Pure -- nothing of
   the CPU's -- so that cpp-fuzz.py compiles it alone (`ucode`). */

namespace kernel
{
namespace microcode
{

namespace
{

/* An update's header, as the SDM lays it out (Vol. 3A, Table 10-7) */
struct Header
{
    u32 header_version;
    u32 revision;
    u32 date;
    u32 signature;
    u32 checksum;
    u32 loader_revision;
    u32 processor_flags;
    u32 data_size;
    u32 total_size;
    u32 reserved[3];
};
static_assert(sizeof(Header) == 48, "an update's header is 48 bytes");

/* The extended signature table's header, and one of its signatures */
struct ExtHeader
{
    u32 count;
    u32 checksum;
    u32 reserved[3];
};
static_assert(sizeof(ExtHeader) == 20, "the extended table's header is 20 bytes");

struct ExtSignature
{
    u32 signature;
    u32 processor_flags;
    u32 checksum;
};
static_assert(sizeof(ExtSignature) == 12, "an extended signature is 12 bytes");

const u32 HEADER_VERSION = 1;
const u32 LOADER_REVISION = 1;
/* A data size of 0 is the original format: 2000 bytes of data, 2048 in all */
const ulong DEFAULT_DATA_SIZE = 2000;
const ulong DEFAULT_TOTAL_SIZE = 2048;
/* Every update's total size is a whole number of KiB */
const ulong TOTAL_SIZE_UNIT = 1024;
const ulong DWORD_SIZE = 4;

/* The sum of the dwords of `bytes` bytes at `p`, as the checksums are made:
   every update's, header and data, and every extended table's, sum to 0 */
u32 sum_dwords(const u8* p, ulong bytes)
{
    u32 sum = 0;
    for (ulong i = 0; i + DWORD_SIZE <= bytes; i += DWORD_SIZE)
    {
        u32 dword;
        stdlib::mem_cpy(&dword, p + i, DWORD_SIZE);
        sum = sum + dword;
    }
    return sum;
}

/* Whether a CPU of `sig` and `pf` is one an update -- or an extended
   signature -- of `s` and `f` is for: the signature, and a platform both
   have, or neither naming one */
bool matches(u32 sig, u32 pf, u32 s, u32 f)
{
    if (sig != s)
        return false;
    if (pf == 0 && f == 0)
        return true;
    return (pf & f) != 0;
}

}

bool choose(const u8* file, ulong len, u32 sig, u32 pf, u32 current, Choice* out)
{
    bool found = false;
    ulong off = 0;

    /* off <= len throughout: it moves only by an update's total size, which
       is checked against what is left before it is believed */
    while (len - off >= sizeof(Header))
    {
        const u8* update = file + off;
        Header h;
        stdlib::mem_cpy(&h, update, sizeof(h));
        if (h.header_version != HEADER_VERSION || h.loader_revision != LOADER_REVISION)
            break;

        ulong data = (h.data_size != 0) ? h.data_size : DEFAULT_DATA_SIZE;
        ulong total = (h.data_size != 0) ? h.total_size : DEFAULT_TOTAL_SIZE;
        if (data % DWORD_SIZE != 0 || total % TOTAL_SIZE_UNIT != 0)
            break;
        if (total < sizeof(Header) || total - sizeof(Header) < data || total > len - off)
            break;
        if (sum_dwords(update, sizeof(Header) + data) != 0)
            break;

        bool fits = matches(sig, pf, h.signature, h.processor_flags);

        /* What is past the data is an extended signature table, exactly, or
           nothing: other signatures and platforms the same update is for */
        ulong ext = total - sizeof(Header) - data;
        if (ext != 0)
        {
            if (ext < sizeof(ExtHeader) || (ext - sizeof(ExtHeader)) % sizeof(ExtSignature) != 0)
                break;
            const u8* table = update + sizeof(Header) + data;
            ExtHeader e;
            stdlib::mem_cpy(&e, table, sizeof(e));
            if (e.count != (ext - sizeof(ExtHeader)) / sizeof(ExtSignature))
                break;
            if (sum_dwords(table, ext) != 0)
                break;

            bool sound = true;
            for (ulong i = 0; i < e.count; i++)
            {
                ExtSignature es;
                stdlib::mem_cpy(&es, table + sizeof(ExtHeader) + i * sizeof(ExtSignature), sizeof(es));
                /* Its checksum is the update's, its signature and platforms
                   in place of the header's */
                if (h.signature + h.processor_flags + h.checksum != es.signature + es.processor_flags + es.checksum)
                {
                    sound = false;
                    break;
                }
                if (matches(sig, pf, es.signature, es.processor_flags))
                    fits = true;
            }
            if (!sound)
                break;
        }

        if (fits && h.revision > current && (!found || h.revision > out->revision))
        {
            out->offset = off;
            out->size = total;
            out->revision = h.revision;
            out->date = h.date;
            found = true;
        }
        off += total;
    }

    return found;
}

}
}
