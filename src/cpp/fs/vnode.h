#pragma once

#include <include/types.h>
#include <lib/list_entry.h>

namespace Kernel
{

struct VNode
{
    enum Type { TypeDir, TypeFile };

    char Name[64];
    Type NodeType;
    VNode* Parent;
    Stdlib::ListEntry Children;   // head of child list (for dirs)
    Stdlib::ListEntry SiblingLink; // link in parent's Children list

    // File data (only for TypeFile)
    u8* Data;
    ulong Size;
    ulong Capacity;
};

}
