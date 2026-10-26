#pragma once

#include <include/types.h>
#include <include/const.h>

namespace kernel
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
        const char* name;       /* node name (with unit address) */
        ulong offset;           /* structure-block offset of this node */
        int depth;
        u32 address_cells;       /* #address-cells inherited from parent */
        u32 size_cells;          /* #size-cells inherited from parent */
    };

    /* A property's value: its bytes, and how many there are. Every read is
       bounded by the length, so a property shorter than its reader expects
       is a refusal, never a read past its end. */
    class Prop final
    {
    public:
        Prop()
            : data_(nullptr)
            , len_(0)
        {
        }

        Prop(const u8* data, u32 len)
            : data_(data)
            , len_(len)
        {
        }

        bool present() const { return data_ != nullptr; }
        u32 length() const { return len_; }

        /* The big-endian u32 cell at index; false past the value's end. */
        bool cell(ulong index, u32& value) const;

        /* count cells from cell index on, read as one big-endian number:
           no cells are 0, and more than two do not fit a u64 and are
           refused, as is a run past the value's end. */
        bool cells(ulong index, u32 count, u64& value) const;

        /* The value as a string: only if it is NUL-terminated within its
           length, nullptr otherwise. */
        const char* string() const;

    private:
        const u8* data_;
        u32 len_;
    };

    bool setup(const void* dtb);

    bool is_valid() const { return valid_; }

    /* Iterate nodes in structure order. Pass zeroed Node to start; returns
       false when the tree is exhausted, or found corrupt. */
    bool next_node(Node& node);

    /* The property of the node the cursor points at; absent if the node has
       none of that name. */
    Prop get_prop(const Node& node, const char* name);

    /* True if the node's compatible list contains the given string. */
    bool is_compatible(const Node& node, const char* compat);

    ulong get_total_size() const { return total_size_; }

    static u32 be32(const void* p);

    /* The largest blob the arm64 boot protocol allows (Linux's
       Documentation/arch/arm64/booting.rst): nothing past it is the DTB's. */
    static const ulong MAX_SIZE = 2 * consts::MB;

private:
    const u8* base_ = nullptr;
    ulong total_size_ = 0;
    ulong struct_off_ = 0;
    ulong struct_size_ = 0;
    ulong strings_off_ = 0;
    ulong strings_size_ = 0;
    bool valid_ = false;

    static const u32 MAGIC = 0xD00DFEED;
    static const u32 TOK_BEGIN_NODE = 1;
    static const u32 TOK_END_NODE = 2;
    static const u32 TOK_PROP = 3;
    static const u32 TOK_NOP = 4;
    static const u32 TOK_END = 9;

    u32 token_at(ulong off) const;
    const char* string(u32 off) const;
    /* Length of the NUL-terminated string at structure offset `off`,
       bounded by the structure block; (ulong)-1 if unterminated. */
    ulong bounded_name_len(ulong off) const;
    /* StrCmp(String(nameOff), name), refusing to run past the strings
       block on a corrupt nameOff / unterminated string. */
    bool string_matches(u32 name_off, const char* name) const;
};

}
