#include "fdt.h"

#include <lib/stdlib.h>

namespace Kernel
{

namespace
{

struct FdtHeader
{
    u32 Magic;
    u32 TotalSize;
    u32 OffDtStruct;
    u32 OffDtStrings;
    u32 OffMemRsvmap;
    u32 Version;
    u32 LastCompVersion;
    u32 BootCpuidPhys;
    u32 SizeDtStrings;
    u32 SizeDtStruct;
};

const int MaxDepth = 16;

/* Cell counts inherited while walking; single-threaded early boot only. */
u32 AddressCellsStack[MaxDepth];
u32 SizeCellsStack[MaxDepth];

ulong AlignUp4(ulong v)
{
    return (v + 3) & ~3UL;
}

}

u32 Fdt::Be32(const void* p)
{
    const u8* b = static_cast<const u8*>(p);
    return ((u32)b[0] << 24) | ((u32)b[1] << 16) | ((u32)b[2] << 8) | (u32)b[3];
}

u32 Fdt::TokenAt(ulong off) const
{
    return Be32(Base + off);
}

const char* Fdt::String(u32 off) const
{
    return reinterpret_cast<const char*>(Base + StringsOff + off);
}

ulong Fdt::BoundedNameLen(ulong off) const
{
    ulong limit = StructOff + StructSize;
    for (ulong i = off; i < limit; i++)
    {
        if (Base[i] == '\0')
            return i - off;
    }
    return (ulong)-1;
}

bool Fdt::StringMatches(u32 nameOff, const char* name) const
{
    if (nameOff >= StringsSize)
        return false;

    const char* s = String(nameOff);
    ulong maxLen = StringsSize - nameOff;
    for (ulong i = 0; i < maxLen; i++)
    {
        if (s[i] != name[i])
            return false;
        if (s[i] == '\0')
            return true;
    }
    return false; /* unterminated within the strings block */
}

bool Fdt::Setup(const void* dtb)
{
    if (dtb == nullptr)
        return false;

    const u8* base = static_cast<const u8*>(dtb);
    FdtHeader hdr;
    Stdlib::MemCpy(&hdr, base, sizeof(hdr));

    if (Be32(&hdr.Magic) != Magic)
        return false;

    Base = base;
    TotalSize = Be32(&hdr.TotalSize);
    StructOff = Be32(&hdr.OffDtStruct);
    StructSize = Be32(&hdr.SizeDtStruct);
    StringsOff = Be32(&hdr.OffDtStrings);
    StringsSize = Be32(&hdr.SizeDtStrings);

    /* A truncated/corrupt header must not steer the walkers out of the
       blob; every later bound derives from these */
    if (StructOff > TotalSize || StructSize > TotalSize - StructOff ||
        StringsOff > TotalSize || StringsSize > TotalSize - StringsOff)
        return false;

    Valid = true;
    return true;
}

bool Fdt::NextNode(Node& node)
{
    if (!Valid)
        return false;

    ulong off;
    int depth;

    if (node.Name == nullptr)
    {
        /* Start of walk */
        off = StructOff;
        depth = -1;
        AddressCellsStack[0] = 2;
        SizeCellsStack[0] = 1;
    }
    else
    {
        /* Resume after the previously returned node: skip its BEGIN_NODE
           token + name, then continue scanning. */
        ulong nameLen = BoundedNameLen(node.Offset + 4);
        if (nameLen == (ulong)-1)
            return false;
        off = node.Offset + 4 + AlignUp4(nameLen + 1);
        depth = node.Depth;
    }

    ulong limit = StructOff + StructSize;
    while (off + 4 <= limit)
    {
        u32 tok = TokenAt(off);

        if (tok == TokBeginNode)
        {
            if (BoundedNameLen(off + 4) == (ulong)-1)
                return false; /* unterminated name: corrupt blob */
            const char* name = reinterpret_cast<const char*>(Base + off + 4);
            if (depth + 1 >= MaxDepth)
                return false;
            depth++;
            /* Children inherit the parent's cells until overridden */
            if (depth + 1 < MaxDepth)
            {
                AddressCellsStack[depth + 1] = (depth >= 0) ? AddressCellsStack[depth] : 2;
                SizeCellsStack[depth + 1] = (depth >= 0) ? SizeCellsStack[depth] : 1;
            }

            node.Name = name;
            node.Offset = off;
            node.Depth = depth;
            node.AddressCells = AddressCellsStack[depth];
            node.SizeCells = SizeCellsStack[depth];

            /* Peek this node's #address-cells/#size-cells for its children */
            u32 len;
            const void* p = GetProp(node, "#address-cells", len);
            if (p != nullptr && len == 4 && depth + 1 < MaxDepth)
                AddressCellsStack[depth + 1] = Be32(p);
            p = GetProp(node, "#size-cells", len);
            if (p != nullptr && len == 4 && depth + 1 < MaxDepth)
                SizeCellsStack[depth + 1] = Be32(p);

            return true;
        }
        else if (tok == TokEndNode)
        {
            depth--;
            off += 4;
        }
        else if (tok == TokProp)
        {
            if (off + 12 > limit)
                return false;
            u32 len = Be32(Base + off + 4);
            if (len > limit - off - 12)
                return false;
            off += 12 + AlignUp4(len);
        }
        else if (tok == TokNop)
        {
            off += 4;
        }
        else
        {
            return false; /* TokEnd or corrupt */
        }
    }

    return false;
}

const void* Fdt::GetProp(const Node& node, const char* name, u32& lenOut)
{
    lenOut = 0;
    if (!Valid)
        return nullptr;

    /* Properties come right after BEGIN_NODE + name, before any subnode */
    ulong nameLen = BoundedNameLen(node.Offset + 4);
    if (nameLen == (ulong)-1)
        return nullptr;
    ulong off = node.Offset + 4 + AlignUp4(nameLen + 1);
    ulong limit = StructOff + StructSize;

    while (off + 4 <= limit)
    {
        u32 tok = TokenAt(off);
        if (tok == TokProp)
        {
            if (off + 12 > limit)
                return nullptr;
            u32 len = Be32(Base + off + 4);
            u32 nameOff = Be32(Base + off + 8);
            if (len > limit - off - 12)
                return nullptr; /* value would run past the struct block */
            if (StringMatches(nameOff, name))
            {
                lenOut = len;
                return Base + off + 12;
            }
            off += 12 + AlignUp4(len);
        }
        else if (tok == TokNop)
        {
            off += 4;
        }
        else
        {
            /* BEGIN_NODE (subnode), END_NODE or END: no more properties */
            return nullptr;
        }
    }
    return nullptr;
}

const char* Fdt::GetPropString(const Node& node, const char* name)
{
    u32 len;
    const void* p = GetProp(node, name, len);
    if (p == nullptr || len == 0)
        return nullptr;
    return static_cast<const char*>(p);
}

u64 Fdt::ReadCells(const void* prop, ulong index, u32 cellCount)
{
    const u8* p = static_cast<const u8*>(prop) + index * cellCount * 4;
    u64 val = 0;
    for (u32 i = 0; i < cellCount; i++)
    {
        val = (val << 32) | Be32(p + i * 4);
    }
    return val;
}

bool Fdt::IsCompatible(const Node& node, const char* compat)
{
    u32 len;
    const void* p = GetProp(node, "compatible", len);
    if (p == nullptr)
        return false;

    const char* s = static_cast<const char*>(p);
    if (len == 0 || s[len - 1] != '\0')
        return false; /* a compatible list is NUL-terminated by definition */

    ulong pos = 0;
    while (pos < len)
    {
        if (Stdlib::StrCmp(s + pos, compat) == 0)
            return true;
        pos += Stdlib::StrLen(s + pos) + 1;
    }
    return false;
}

}
