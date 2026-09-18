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

/* The VFS is Rust (src/rust/fs), and a vnode is what the two sides pass each
   other: the same struct, laid out the same way. crate::vnode::VNode asserts
   the same number, because a disagreement would be no compile error anywhere
   -- it would be a filesystem reading fields at the wrong offsets. */
static_assert(sizeof(VNode) == 160, "src/rust/fs/src/vnode.rs mirrors this");

}
