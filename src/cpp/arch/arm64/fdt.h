#pragma once

#include <include/types.h>
#include <include/const.h>

namespace Kernel
{

/* Minimal flattened-device-tree (DTB) reader: structure-block walk with
   #address-cells/#size-cells tracking. Read-only, no allocation; parsed
   once at early boot by Board::Setup(). Not a general-purpose parser --
   just enough for QEMU virt (memory, chosen, psci, gic, pl011, pl031,
   virtio_mmio, timer, cpus).

   The blob is whatever the firmware put there, so nothing it says is
   trusted past what it can be checked against: the header's sizes against
   the largest blob the boot protocol allows, every offset against the block
   it points into, the nesting against the depth the walker keeps, and every
   property read against the property's own length (Prop). */
class Fdt final
{
public:
    /* Callback-style iteration is impossible without lambdas; instead the
       walker exposes a cursor over nodes with their parent cell counts. */
    struct Node
    {
        const char* Name;       /* node name (with unit address) */
        ulong Offset;           /* structure-block offset of this node */
        int Depth;
        u32 AddressCells;       /* #address-cells inherited from parent */
        u32 SizeCells;          /* #size-cells inherited from parent */
    };

    /* A property's value: its bytes, and how many there are. Every read is
       bounded by the length, so a property shorter than its reader expects
       is a refusal, never a read past its end. */
    class Prop final
    {
    public:
        Prop()
            : Data(nullptr)
            , Len(0)
        {
        }

        Prop(const u8* data, u32 len)
            : Data(data)
            , Len(len)
        {
        }

        bool Present() const { return Data != nullptr; }
        u32 Length() const { return Len; }

        /* The big-endian u32 cell at index; false past the value's end. */
        bool Cell(ulong index, u32& value) const;

        /* count cells from cell index on, read as one big-endian number:
           no cells are 0, and more than two do not fit a u64 and are
           refused, as is a run past the value's end. */
        bool Cells(ulong index, u32 count, u64& value) const;

        /* The value as a string: only if it is NUL-terminated within its
           length, nullptr otherwise. */
        const char* String() const;

    private:
        const u8* Data;
        u32 Len;
    };

    bool Setup(const void* dtb);

    bool IsValid() const { return Valid; }

    /* Iterate nodes in structure order. Pass zeroed Node to start; returns
       false when the tree is exhausted, or found corrupt. */
    bool NextNode(Node& node);

    /* The property of the node the cursor points at; absent if the node has
       none of that name. */
    Prop GetProp(const Node& node, const char* name);

    /* True if the node's compatible list contains the given string. */
    bool IsCompatible(const Node& node, const char* compat);

    ulong GetTotalSize() const { return TotalSize; }

    static u32 Be32(const void* p);

    /* The largest blob the arm64 boot protocol allows (Linux's
       Documentation/arch/arm64/booting.rst): nothing past it is the DTB's. */
    static const ulong MaxSize = 2 * Const::MB;

private:
    const u8* Base = nullptr;
    ulong TotalSize = 0;
    ulong StructOff = 0;
    ulong StructSize = 0;
    ulong StringsOff = 0;
    ulong StringsSize = 0;
    bool Valid = false;

    static const u32 Magic = 0xD00DFEED;
    static const u32 TokBeginNode = 1;
    static const u32 TokEndNode = 2;
    static const u32 TokProp = 3;
    static const u32 TokNop = 4;
    static const u32 TokEnd = 9;

    u32 TokenAt(ulong off) const;
    const char* String(u32 off) const;
    /* Length of the NUL-terminated string at structure offset `off`,
       bounded by the structure block; (ulong)-1 if unterminated. */
    ulong BoundedNameLen(ulong off) const;
    /* StrCmp(String(nameOff), name), refusing to run past the strings
       block on a corrupt nameOff / unterminated string. */
    bool StringMatches(u32 nameOff, const char* name) const;
};

}
