//! The vnode: a name, a type and a place in a tree, and the one piece of the
//! filesystem layer both languages still share.
//!
//! A filesystem makes vnodes and owns them; the VFS walks them and never
//! allocates one. While a filesystem is still written in C++ its `VNode`
//! (fs/vnode.h) is the same struct as this one, laid out the same way -- the
//! static assertions on both sides are what keeps that true -- so the two
//! can pass nodes to each other with nothing in between.

use core::ffi::c_int;

/// What a name fits in, NUL included (VNode::Name).
pub const NAME_MAX: usize = 64;

pub const TYPE_DIR: c_int = 0;
pub const TYPE_FILE: c_int = 1;

/// Set on a directory once its entries are in `children`: the in-memory
/// filesystems are born complete, ext2 reads a directory on first use.
pub const FLAG_DIR_LOADED: usize = 1;

/// The kernel's intrusive list link (Stdlib::ListEntry).
#[repr(C)]
pub struct ListEntry {
    pub flink: *mut ListEntry,
    pub blink: *mut ListEntry,
}

#[repr(C)]
pub struct VNode {
    pub name: [u8; NAME_MAX],
    pub node_type: c_int,
    pub parent: *mut VNode,
    /// Head of the child list, for a directory
    pub children: ListEntry,
    /// This node's link in its parent's list
    pub sibling: ListEntry,

    /* A file's contents, for a filesystem that keeps them in memory */
    pub data: *mut u8,
    pub size: usize,
    pub capacity: usize,

    /// The on-disk inode number (nanofs, ext2); 0 where there is none
    pub ino: usize,
    pub flags: usize,
    /// Handles open on this node, which the VFS keeps
    pub open_count: usize,
}

/* The C++ side asserts the same number (fs/vnode.h). A vnode crosses between
 * them by pointer, so a disagreement here would not be a compile error
 * anywhere -- it would be a filesystem reading fields at the wrong offsets. */
const _: () = assert!(core::mem::size_of::<VNode>() == 160);

impl VNode {
    pub fn is_dir(&self) -> bool {
        self.node_type == TYPE_DIR
    }

    pub fn is_file(&self) -> bool {
        self.node_type == TYPE_FILE
    }

    /// The name, without the NUL the C side terminates it with.
    pub fn name(&self) -> &[u8] {
        let len = self.name.iter().position(|b| *b == 0).unwrap_or(NAME_MAX);
        &self.name[..len]
    }

    pub fn name_is(&self, other: &[u8]) -> bool {
        self.name() == other
    }
}

/// Walk a directory's children. The list is circular through the `sibling`
/// link of each child, with the directory's `children` as its head.
///
/// # Safety
/// `dir` is a live vnode, and nothing adds to or removes from its child list
/// while the walk runs -- which is what holding the VFS lock guarantees.
pub unsafe fn children(dir: *mut VNode) -> Children {
    let head = unsafe { core::ptr::addr_of_mut!((*dir).children) };
    Children { head, at: unsafe { (*head).flink } }
}

pub struct Children {
    head: *mut ListEntry,
    at: *mut ListEntry,
}

impl Iterator for Children {
    type Item = *mut VNode;

    fn next(&mut self) -> Option<*mut VNode> {
        if self.at.is_null() || self.at == self.head {
            return None;
        }

        let link = self.at;
        self.at = unsafe { (*link).flink };

        /* The link sits inside the child, at a known offset from its start:
         * the same arithmetic the C++ side does as CONTAINING_RECORD. */
        let offset = core::mem::offset_of!(VNode, sibling);
        Some((link as usize - offset) as *mut VNode)
    }
}

/// Whether a node, or anything under it, has an open handle.
///
/// # Safety
/// As for `children`.
pub unsafe fn has_open_files(node: *mut VNode) -> bool {
    if unsafe { (*node).open_count } != 0 {
        return true;
    }

    for child in unsafe { children(node) } {
        if unsafe { has_open_files(child) } {
            return true;
        }
    }

    false
}

/// Whether `node` is `other` or one of its ancestors -- what says a
/// directory is being moved inside itself.
///
/// # Safety
/// Both are live vnodes.
pub unsafe fn is_ancestor(node: *mut VNode, other: *mut VNode) -> bool {
    let mut at = other;
    while !at.is_null() {
        if at == node {
            return true;
        }
        at = unsafe { (*at).parent };
    }
    false
}
