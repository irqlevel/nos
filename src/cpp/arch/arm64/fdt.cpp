#include "fdt.h"

#include <lib/stdlib.h>

namespace kernel
{

namespace
{

struct FdtHeader
{
    u32 magic;
    u32 total_size;
    u32 off_dt_struct;
    u32 off_dt_strings;
    u32 off_mem_rsvmap;
    u32 version;
    u32 last_comp_version;
    u32 boot_cpuid_phys;
    u32 size_dt_strings;
    u32 size_dt_struct;
};

const int MAX_DEPTH = 16;

/* Cell counts inherited while walking; single-threaded early boot only. */
u32 address_cells_stack[MAX_DEPTH];
u32 size_cells_stack[MAX_DEPTH];

ulong align_up4(ulong v)
{
    return (v + 3) & ~3UL;
}

}

u32 Fdt::be32(const void* p)
{
    const u8* b = static_cast<const u8*>(p);
    return ((u32)b[0] << 24) | ((u32)b[1] << 16) | ((u32)b[2] << 8) | (u32)b[3];
}

u32 Fdt::token_at(ulong off) const
{
    return be32(base_ + off);
}

const char* Fdt::string(u32 off) const
{
    return reinterpret_cast<const char*>(base_ + strings_off_ + off);
}

ulong Fdt::bounded_name_len(ulong off) const
{
    ulong limit = struct_off_ + struct_size_;
    for (ulong i = off; i < limit; i++)
    {
        if (base_[i] == '\0')
            return i - off;
    }
    return (ulong)-1;
}

bool Fdt::string_matches(u32 name_off, const char* name) const
{
    if (name_off >= strings_size_)
        return false;

    const char* s = string(name_off);
    ulong max_len = strings_size_ - name_off;
    for (ulong i = 0; i < max_len; i++)
    {
        if (s[i] != name[i])
            return false;
        if (s[i] == '\0')
            return true;
    }
    return false; /* unterminated within the strings block */
}

bool Fdt::setup(const void* dtb)
{
    if (dtb == nullptr)
        return false;

    const u8* base = static_cast<const u8*>(dtb);
    FdtHeader hdr;
    stdlib::mem_cpy(&hdr, base, sizeof(hdr));

    if (be32(&hdr.magic) != MAGIC)
        return false;

    base_ = base;
    total_size_ = be32(&hdr.total_size);
    struct_off_ = be32(&hdr.off_dt_struct);
    struct_size_ = be32(&hdr.size_dt_struct);
    strings_off_ = be32(&hdr.off_dt_strings);
    strings_size_ = be32(&hdr.size_dt_strings);

    /* The blob's own size is the one thing nothing else can check, so it
       is held to what the boot protocol allows: the walkers read no byte
       past it, and the memory map reserves exactly that much */
    if (total_size_ < sizeof(FdtHeader) || total_size_ > MAX_SIZE)
        return false;

    /* A truncated/corrupt header must not steer the walkers out of the
       blob; every later bound derives from these */
    if (struct_off_ > total_size_ || struct_size_ > total_size_ - struct_off_ ||
        strings_off_ > total_size_ || strings_size_ > total_size_ - strings_off_)
        return false;

    valid_ = true;
    return true;
}

bool Fdt::next_node(Node& node)
{
    if (!valid_)
        return false;

    ulong off;
    int depth;

    if (node.name == nullptr)
    {
        /* Start of walk */
        off = struct_off_;
        depth = -1;
        address_cells_stack[0] = 2;
        size_cells_stack[0] = 1;
    }
    else
    {
        /* Resume after the previously returned node: skip its BEGIN_NODE
           token + name, then continue scanning. */
        ulong name_len = bounded_name_len(node.offset + 4);
        if (name_len == (ulong)-1)
            return false;
        off = node.offset + 4 + align_up4(name_len + 1);
        depth = node.depth;
    }

    ulong limit = struct_off_ + struct_size_;
    while (off + 4 <= limit)
    {
        u32 tok = token_at(off);

        if (tok == TOK_BEGIN_NODE)
        {
            if (bounded_name_len(off + 4) == (ulong)-1)
                return false; /* unterminated name: corrupt blob */
            const char* name = reinterpret_cast<const char*>(base_ + off + 4);
            if (depth + 1 >= MAX_DEPTH)
                return false;
            depth++;
            /* Children inherit the parent's cells until overridden. depth is
               the root's 0 at least: an END_NODE with none open is refused */
            if (depth + 1 < MAX_DEPTH)
            {
                address_cells_stack[depth + 1] = address_cells_stack[depth];
                size_cells_stack[depth + 1] = size_cells_stack[depth];
            }

            node.name = name;
            node.offset = off;
            node.depth = depth;
            node.address_cells = address_cells_stack[depth];
            node.size_cells = size_cells_stack[depth];

            /* Peek this node's #address-cells/#size-cells for its children */
            u32 cells;
            Prop prop = get_prop(node, "#address-cells");
            if (prop.length() == 4 && prop.cell(0, cells) && depth + 1 < MAX_DEPTH)
                address_cells_stack[depth + 1] = cells;
            prop = get_prop(node, "#size-cells");
            if (prop.length() == 4 && prop.cell(0, cells) && depth + 1 < MAX_DEPTH)
                size_cells_stack[depth + 1] = cells;

            return true;
        }
        else if (tok == TOK_END_NODE)
        {
            /* One more end than there were beginnings: no node is open to
               end, and the cell stacks have no entry below the root's */
            if (depth < 0)
                return false;
            depth--;
            off += 4;
        }
        else if (tok == TOK_PROP)
        {
            if (off + 12 > limit)
                return false;
            u32 len = be32(base_ + off + 4);
            if (len > limit - off - 12)
                return false;
            off += 12 + align_up4(len);
        }
        else if (tok == TOK_NOP)
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

Fdt::Prop Fdt::get_prop(const Node& node, const char* name)
{
    if (!valid_)
        return Prop();

    /* Properties come right after BEGIN_NODE + name, before any subnode */
    ulong name_len = bounded_name_len(node.offset + 4);
    if (name_len == (ulong)-1)
        return Prop();
    ulong off = node.offset + 4 + align_up4(name_len + 1);
    ulong limit = struct_off_ + struct_size_;

    while (off + 4 <= limit)
    {
        u32 tok = token_at(off);
        if (tok == TOK_PROP)
        {
            if (off + 12 > limit)
                return Prop();
            u32 len = be32(base_ + off + 4);
            u32 name_off = be32(base_ + off + 8);
            if (len > limit - off - 12)
                return Prop(); /* value would run past the struct block */
            if (string_matches(name_off, name))
                return Prop(base_ + off + 12, len);
            off += 12 + align_up4(len);
        }
        else if (tok == TOK_NOP)
        {
            off += 4;
        }
        else
        {
            /* BEGIN_NODE (subnode), END_NODE or END: no more properties */
            return Prop();
        }
    }
    return Prop();
}

bool Fdt::Prop::cell(ulong index, u32& value) const
{
    if (data_ == nullptr || index >= len_ / 4)
        return false;

    value = be32(data_ + index * 4);
    return true;
}

bool Fdt::Prop::cells(ulong index, u32 count, u64& value) const
{
    /* Two cells are a u64; a third would be shifted out without a word */
    static const u32 MAX_CELLS = 2;

    if (data_ == nullptr || count > MAX_CELLS || index > len_ / 4 || count > len_ / 4 - index)
        return false;

    value = 0;
    for (u32 i = 0; i < count; i++)
        value = (value << 32) | be32(data_ + (index + i) * 4);
    return true;
}

const char* Fdt::Prop::string() const
{
    if (data_ == nullptr || len_ == 0 || data_[len_ - 1] != '\0')
        return nullptr;

    return reinterpret_cast<const char*>(data_);
}

bool Fdt::is_compatible(const Node& node, const char* compat)
{
    Prop prop = get_prop(node, "compatible");

    /* A compatible list is NUL-terminated by definition */
    const char* s = prop.string();
    if (s == nullptr)
        return false;
    const ulong len = prop.length();

    ulong pos = 0;
    while (pos < len)
    {
        if (stdlib::str_cmp(s + pos, compat) == 0)
            return true;
        pos += stdlib::str_len(s + pos) + 1;
    }
    return false;
}

}
