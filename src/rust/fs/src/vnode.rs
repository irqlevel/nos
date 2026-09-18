//! The vnode tree: names, types and places, in an arena.
//!
//! A filesystem makes the nodes and owns the tree; the VFS walks it and never
//! makes one. A node is named by a `NodeId` -- a slot and the generation of
//! what is in it -- so a parent, a child and an open file all refer to a node
//! by a number rather than by an address. What that buys: a reference to a
//! node that has been freed is a `None`, not a read of whatever the allocator
//! put there next, and the whole tree is ordinary owned data with no
//! `unsafe` in it or in any filesystem built on it.
//!
//! The slots come in chunks taken fallibly, so that a tree bigger than the
//! memory for it is a failure to report rather than a panic: a mounted image
//! says how many nodes there will be, and an image is not to be trusted with
//! that.

use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::ops::{Index, IndexMut};

/// What a name fits in, its terminator included: a name is at most one less.
pub const NAME_MAX: usize = 64;

/// Slots in one chunk of the arena: what is asked of the allocator at a time.
/// A chunk stays within a page, the one size the allocator never has to find
/// neighbouring pages for -- on a machine that has been up for a while, a
/// tree that grows a page at a time keeps growing where one that asked for
/// three together would stop.
const CHUNK: usize = 16;
const PAGE: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
}

/// Which node of a tree. Only a `Tree` makes one, for a node it has just
/// made; it stops naming anything when that node is freed, and never comes
/// to name another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeId {
    /// The slot, plus one -- so that an `Option<NodeId>` costs nothing more
    slot: NonZeroU32,
    generation: u32,
}

impl NodeId {
    fn new(index: usize, generation: u32) -> Option<NodeId> {
        let slot = NonZeroU32::new(u32::try_from(index).ok()?.checked_add(1)?)?;
        Some(NodeId { slot, generation })
    }

    fn index(self) -> usize {
        self.slot.get() as usize - 1
    }
}

pub struct Node {
    name: [u8; NAME_MAX],
    name_len: u8,
    pub kind: Kind,

    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    next: Option<NodeId>,
    prev: Option<NodeId>,

    /// A file's contents, for a filesystem that keeps them in memory
    pub data: Vec<u8>,
    /// A file's size; a directory's is 0
    pub size: usize,
    /// The on-disk inode number (nanofs, ext2); 0 where there is none
    pub ino: usize,
    /// Set on a directory once its entries are its children: the in-memory
    /// filesystems are born complete, ext2 reads a directory on first use.
    pub dir_loaded: bool,
    /// Handles open on this node, which the VFS keeps
    pub open_count: usize,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn is_file(&self) -> bool {
        self.kind == Kind::File
    }

    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    fn set_name(&mut self, name: &[u8]) {
        let len = name.len().min(NAME_MAX - 1);
        self.name = [0; NAME_MAX];
        self.name[..len].copy_from_slice(&name[..len]);
        self.name_len = len as u8;
    }
}

enum Slot {
    Free { next: Option<u32>, generation: u32 },
    Used { node: Node, generation: u32 },
}

const _: () = assert!(core::mem::size_of::<Slot>() * CHUNK <= PAGE, "a chunk is a page at most");

pub struct Tree {
    chunks: Vec<Vec<Slot>>,
    /// The first free slot, the rest threaded from it
    free: Option<u32>,
    live: usize,
}

impl Tree {
    pub const fn new() -> Tree {
        Tree { chunks: Vec::new(), free: None, live: 0 }
    }

    /// How many nodes there are.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    fn slot(&self, index: usize) -> Option<&Slot> {
        self.chunks.get(index / CHUNK)?.get(index % CHUNK)
    }

    fn slot_mut(&mut self, index: usize) -> Option<&mut Slot> {
        self.chunks.get_mut(index / CHUNK)?.get_mut(index % CHUNK)
    }

    /// One more chunk of free slots. False when there is no memory for it.
    fn grow(&mut self) -> bool {
        let base = self.chunks.len() * CHUNK;
        if u32::try_from(base + CHUNK).is_err() {
            return false;
        }

        let mut chunk = Vec::new();
        if chunk.try_reserve_exact(CHUNK).is_err() || self.chunks.try_reserve(1).is_err() {
            return false;
        }

        /* Threaded so that the lowest slot is handed out first */
        for i in 0..CHUNK {
            let next = if i + 1 < CHUNK { Some((base + i + 1) as u32) } else { self.free };
            chunk.push(Slot::Free { next, generation: 0 });
        }
        self.chunks.push(chunk);
        self.free = Some(base as u32);
        true
    }

    /// A node with this name, on nobody's list yet and with everything else
    /// the caller's to fill. None when there is no memory for one.
    pub fn alloc(&mut self, name: &[u8], kind: Kind) -> Option<NodeId> {
        if self.free.is_none() && !self.grow() {
            return None;
        }
        let index = self.free? as usize;

        let (next, generation) = match self.slot(index)? {
            Slot::Free { next, generation } => (*next, *generation),
            Slot::Used { .. } => return None,
        };
        let id = NodeId::new(index, generation)?;

        let mut node = Node {
            name: [0; NAME_MAX],
            name_len: 0,
            kind,
            parent: None,
            first_child: None,
            last_child: None,
            next: None,
            prev: None,
            data: Vec::new(),
            size: 0,
            ino: 0,
            dir_loaded: false,
            open_count: 0,
        };
        node.set_name(name);

        *self.slot_mut(index)? = Slot::Used { node, generation };
        self.free = next;
        self.live += 1;
        Some(id)
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        match self.slot(id.index())? {
            Slot::Used { node, generation } if *generation == id.generation => Some(node),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        match self.slot_mut(id.index())? {
            Slot::Used { node, generation } if *generation == id.generation => Some(node),
            _ => None,
        }
    }

    /// The node's slot, free again. Its links are the caller's business:
    /// `free_tree` is what takes a node away properly.
    fn release(&mut self, id: NodeId) {
        let free = self.free;
        if self.get(id).is_none() {
            return;
        }
        if let Some(slot) = self.slot_mut(id.index()) {
            /* A new generation: whatever still holds `id` names nothing. */
            *slot = Slot::Free { next: free, generation: id.generation.wrapping_add(1) };
            self.free = Some(id.index() as u32);
            self.live -= 1;
        }
    }

    /// Everything, gone: what an unmount leaves.
    pub fn clear(&mut self) {
        *self = Tree::new();
    }

    /* ---- the shape of the tree ---- */

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.get(id)?.parent
    }

    pub fn first_child(&self, dir: NodeId) -> Option<NodeId> {
        self.get(dir)?.first_child
    }

    /// A directory's children, in the order they were added.
    pub fn children(&self, dir: NodeId) -> Children<'_> {
        Children { tree: self, at: self.first_child(dir) }
    }

    /// The child of that name, if the directory has one.
    pub fn find_child(&self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        self.children(dir).find(|child| self.get(*child).is_some_and(|node| node.name() == name))
    }

    /// Whether a node is somebody's child. The root of a tree is not, and
    /// neither is a node that has been made and not yet placed.
    pub fn is_linked(&self, id: NodeId) -> bool {
        self.parent(id).is_some()
    }

    /// Put `child` at the end of `dir`'s children. False -- and nothing
    /// changed -- when either is not a node, or the child is placed already.
    pub fn insert_child(&mut self, dir: NodeId, child: NodeId) -> bool {
        if dir == child || self.get(child).map_or(true, |node| node.parent.is_some()) {
            return false;
        }
        let tail = match self.get(dir) {
            Some(node) => node.last_child,
            None => return false,
        };

        if let Some(node) = self.get_mut(child) {
            node.parent = Some(dir);
            node.prev = tail;
            node.next = None;
        }
        match tail.and_then(|tail| self.get_mut(tail)) {
            Some(tail) => tail.next = Some(child),
            None => {
                if let Some(node) = self.get_mut(dir) {
                    node.first_child = Some(child);
                }
            }
        }
        if let Some(node) = self.get_mut(dir) {
            node.last_child = Some(child);
        }
        true
    }

    /// Take a node off its parent's children. It keeps its own.
    pub fn unlink(&mut self, id: NodeId) {
        let (parent, prev, next) = match self.get(id) {
            Some(node) => (node.parent, node.prev, node.next),
            None => return,
        };
        let parent = match parent {
            Some(parent) => parent,
            None => return,
        };

        match prev.and_then(|prev| self.get_mut(prev)) {
            Some(prev) => prev.next = next,
            None => {
                if let Some(dir) = self.get_mut(parent) {
                    dir.first_child = next;
                }
            }
        }
        match next.and_then(|next| self.get_mut(next)) {
            Some(next) => next.prev = prev,
            None => {
                if let Some(dir) = self.get_mut(parent) {
                    dir.last_child = prev;
                }
            }
        }

        if let Some(node) = self.get_mut(id) {
            node.parent = None;
            node.prev = None;
            node.next = None;
        }
    }

    /// Give a node a new name and a new parent -- what a rename leaves behind
    /// in memory once the directories on disk say so.
    pub fn rename(&mut self, id: NodeId, new_parent: NodeId, new_name: &[u8]) {
        self.unlink(id);
        if let Some(node) = self.get_mut(id) {
            node.set_name(new_name);
        }
        self.insert_child(new_parent, id);
    }

    /// The node after `at` in a walk of everything under `top`, parents
    /// before their children. No recursion and no list of its own: the links
    /// are the stack.
    fn next_under(&self, top: NodeId, at: NodeId) -> Option<NodeId> {
        if let Some(child) = self.first_child(at) {
            return Some(child);
        }

        let mut up = at;
        while up != top {
            let node = self.get(up)?;
            if let Some(next) = node.next {
                return Some(next);
            }
            up = node.parent?;
        }
        None
    }

    /// Whether a node, or anything under it, has an open handle.
    pub fn has_open_files(&self, top: NodeId) -> bool {
        let mut at = Some(top);
        while let Some(id) = at {
            if self.get(id).is_some_and(|node| node.open_count != 0) {
                return true;
            }
            at = self.next_under(top, id);
        }
        false
    }

    /// Whether `node` is `other` or one of its ancestors -- what says a
    /// directory is being moved inside itself.
    pub fn is_ancestor(&self, node: NodeId, other: NodeId) -> bool {
        let mut at = Some(other);
        while let Some(id) = at {
            if id == node {
                return true;
            }
            at = self.parent(id);
        }
        false
    }

    /// Take a node away, with everything under it: off its parent's list,
    /// and every slot free again.
    pub fn free_tree(&mut self, top: NodeId) {
        self.unlink(top);

        /* Leaves first: down to one, free it, and back to where it hung. */
        let mut at = top;
        loop {
            if let Some(child) = self.first_child(at) {
                at = child;
                continue;
            }

            let parent = self.parent(at);
            self.unlink(at);
            self.release(at);
            if at == top {
                return;
            }
            match parent {
                Some(parent) => at = parent,
                None => return,
            }
        }
    }
}

/// A node by its id, for code that holds one it knows to be live: a stale id
/// here is a bug in a filesystem, and panics like any other broken invariant.
impl Index<NodeId> for Tree {
    type Output = Node;

    fn index(&self, id: NodeId) -> &Node {
        self.get(id).expect("a vnode id that names no node")
    }
}

impl IndexMut<NodeId> for Tree {
    fn index_mut(&mut self, id: NodeId) -> &mut Node {
        self.get_mut(id).expect("a vnode id that names no node")
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    at: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let id = self.at?;
        self.at = self.tree.get(id)?.next;
        Some(id)
    }
}
