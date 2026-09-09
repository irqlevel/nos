#pragma once

#include <include/types.h>
#include <lib/list_entry.h>

namespace Kernel
{

struct VNode
{
    enum Type { TypeDir, TypeFile };

    /* Set on a directory once its entries are in Children. The in-memory
       filesystems are born complete; ext2 reads a directory on first use
       (FileSystem::LoadDir) and sets it then. */
    static const ulong FlagDirLoaded = 1;

    char Name[64];
    Type NodeType;
    VNode* Parent;
    Stdlib::ListEntry Children;   // head of child list (for dirs)
    Stdlib::ListEntry SiblingLink; // link in parent's Children list

    // File data (only for TypeFile)
    u8* Data;
    ulong Size;
    ulong Capacity;

    ulong Ino;       // on-disk inode number (nanofs, ext2); 0 for in-memory filesystems
    ulong Flags;     // VNode::Flag*
    ulong OpenCount; // File handles open on this node, kept by the Vfs
};

}
