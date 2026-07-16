#pragma once

#include <include/types.h>

namespace Kernel
{

/* Minimal flattened-device-tree (DTB) reader: structure-block walk with
   #address-cells/#size-cells tracking. Read-only, no allocation; parsed
   once at early boot by Board::Setup(). Not a general-purpose parser --
   just enough for QEMU virt (memory, chosen, psci, gic, pl011, pl031,
   virtio_mmio, timer, cpus). */
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

    bool Setup(const void* dtb);

    bool IsValid() const { return Valid; }

    /* Iterate nodes in structure order. Pass zeroed Node to start; returns
       false when the tree is exhausted. */
    bool NextNode(Node& node);

    /* Property access on the node the cursor points at. */
    const void* GetProp(const Node& node, const char* name, u32& lenOut);
    const char* GetPropString(const Node& node, const char* name);

    /* Read cell i (0-based) of a reg-like property as a cellCount-cell
       big-endian integer. */
    static u64 ReadCells(const void* prop, ulong index, u32 cellCount);

    /* True if the node's compatible list contains the given string. */
    bool IsCompatible(const Node& node, const char* compat);

    ulong GetTotalSize() const { return TotalSize; }

    static u32 Be32(const void* p);

private:
    const u8* Base = nullptr;
    ulong TotalSize = 0;
    ulong StructOff = 0;
    ulong StructSize = 0;
    ulong StringsOff = 0;
    bool Valid = false;

    static const u32 Magic = 0xD00DFEED;
    static const u32 TokBeginNode = 1;
    static const u32 TokEndNode = 2;
    static const u32 TokProp = 3;
    static const u32 TokNop = 4;
    static const u32 TokEnd = 9;

    u32 TokenAt(ulong off) const;
    const char* String(u32 off) const;
};

}
