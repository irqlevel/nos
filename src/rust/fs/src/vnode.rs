//! The vnode: a name, a type and a place in a tree.
//!
//! A filesystem makes vnodes and owns them; the VFS walks them and never
//! allocates one. Nothing outside this crate sees one any more -- C++ knows
//! only what `Stat` and `ReadDir` answer with -- so the layout below is the
//! only one there is.

use core::ffi::c_int;

/// What a name fits in, NUL included. The C++ side states the same number
/// as `VNode::NameMax`, for the length `Vfs::MaxName` promises.
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

/* The size is not a contract with anything any more; it is here because a
 * vnode is allocated and freed by its layout (see `alloc`), and a field
 * added by accident is worth noticing. */
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

/* ---- making and unmaking vnodes ----
 *
 * A filesystem owns the vnodes it makes; the VFS only walks them. They are
 * taken from the kernel heap without going through `Box`, so that a tree
 * bigger than the memory for it is a failure to report rather than a panic:
 * a mounted image says how many there will be, and an image is not to be
 * trusted with that. */

use core::alloc::Layout;

fn layout() -> Layout {
    Layout::new::<VNode>()
}

/// A zeroed vnode with both list links initialised, or null when there is no
/// memory for one. Everything else is the caller's to fill.
pub fn alloc() -> *mut VNode {
    let node = unsafe { alloc::alloc::alloc_zeroed(layout()) } as *mut VNode;
    if !node.is_null() {
        unsafe {
            list_init(core::ptr::addr_of_mut!((*node).children));
            list_init(core::ptr::addr_of_mut!((*node).sibling));
        }
    }
    node
}

/// # Safety
/// `node` came from `alloc`, is off every list, and is not used again.
pub unsafe fn free(node: *mut VNode) {
    if !node.is_null() {
        unsafe { alloc::alloc::dealloc(node as *mut u8, layout()) };
    }
}

/// Free a node and everything under it.
///
/// # Safety
/// As for `free`, and nothing below it is held anywhere else.
pub unsafe fn free_tree(node: *mut VNode) {
    loop {
        let child = unsafe { first_child(node) };
        if child.is_null() {
            break;
        }
        unsafe {
            unlink(child);
            free_tree(child);
        }
    }
    unsafe { free(node) };
}

/// # Safety
/// `entry` is a list link of a live vnode.
pub unsafe fn list_init(entry: *mut ListEntry) {
    unsafe {
        (*entry).flink = entry;
        (*entry).blink = entry;
    }
}

/// Put `child` at the end of `dir`'s children.
///
/// # Safety
/// Both are live vnodes, `child` is on no list, and nothing else is walking
/// the list -- which is what holding the VFS lock guarantees.
pub unsafe fn insert_child(dir: *mut VNode, child: *mut VNode) {
    unsafe {
        let head = core::ptr::addr_of_mut!((*dir).children);
        let entry = core::ptr::addr_of_mut!((*child).sibling);
        let tail = (*head).blink;

        (*entry).flink = head;
        (*entry).blink = tail;
        (*tail).flink = entry;
        (*head).blink = entry;
    }
}

/// Take a node off its parent's children. It keeps its `parent` pointer,
/// which the caller replaces or drops.
///
/// # Safety
/// As for `insert_child`.
pub unsafe fn unlink(node: *mut VNode) {
    unsafe {
        let entry = core::ptr::addr_of_mut!((*node).sibling);
        let flink = (*entry).flink;
        let blink = (*entry).blink;
        if !flink.is_null() && !blink.is_null() {
            (*blink).flink = flink;
            (*flink).blink = blink;
        }
        list_init(entry);
    }
}

/// The first of a directory's children, or null if it has none.
///
/// # Safety
/// As for `children`.
pub unsafe fn first_child(dir: *mut VNode) -> *mut VNode {
    unsafe {
        let head = core::ptr::addr_of_mut!((*dir).children);
        let first = (*head).flink;
        if first.is_null() || first == head {
            return core::ptr::null_mut();
        }
        (first as usize - core::mem::offset_of!(VNode, sibling)) as *mut VNode
    }
}

/// Give a node a new name and a new parent -- what a rename leaves behind in
/// memory once the directories on disk say so.
///
/// # Safety
/// As for `insert_child`.
pub unsafe fn rename(node: *mut VNode, new_parent: *mut VNode, new_name: &[u8]) {
    unsafe {
        unlink(node);
        (*node).name = [0; NAME_MAX];
        let len = new_name.len().min(NAME_MAX - 1);
        (&mut (*node).name)[..len].copy_from_slice(&new_name[..len]);
        (*node).parent = new_parent;
        insert_child(new_parent, node);
    }
}

/// Whether a node is on no parent's list: its link points at itself, which
/// is what `alloc` leaves it as and `unlink` puts it back to.
///
/// # Safety
/// `node` is a live vnode.
pub unsafe fn is_unlinked(node: *mut VNode) -> bool {
    unsafe {
        let entry = core::ptr::addr_of_mut!((*node).sibling);
        (*entry).flink == entry
    }
}
