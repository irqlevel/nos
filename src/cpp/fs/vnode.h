#pragma once

#include <include/types.h>

namespace Kernel
{

/* What a path names. The vnode itself -- the tree, the names, the sizes --
   lives in Rust with the filesystems (src/rust/fs/src/vnode.rs); what is
   left here is what Stat and ReadDir answer with, so that `VNode::TypeDir`
   still reads the way it always has at the callers. */
struct VNode
{
    enum Type { TypeDir, TypeFile };

    /* What a name fits in, NUL included (crate::vnode::NAME_MAX) */
    static const ulong NameMax = 64;
};

}
