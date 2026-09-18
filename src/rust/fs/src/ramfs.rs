//! ramfs: a filesystem that is only ever in memory.
//!
//! What the ISO boots on when there is no root on disk, and what `/tmp` and
//! the boot self-test use. A file is one growing buffer, a directory is a
//! list of children, and nothing survives a reboot.
//!
//! Every call arrives with the VFS lock held (see `FsOps`), so there is no
//! locking here.

use alloc::boxed::Box;
use core::alloc::Layout;

use kcore::trace;

use crate::vfs::FsOps;
use crate::vnode::{self, VNode, FLAG_DIR_LOADED, NAME_MAX, TYPE_DIR, TYPE_FILE};

/// The smallest buffer a file gets; it doubles from there.
const MIN_CAPACITY: usize = 64;

pub struct RamFs {
    root: *mut VNode,
}

/// A file's buffer, as the layout it was taken with. A zero capacity is no
/// buffer at all.
fn buffer_layout(capacity: usize) -> Layout {
    /* Bytes, so any size is a valid layout and `dealloc` needs nothing but
     * the capacity the vnode already carries. */
    unsafe { Layout::from_size_align_unchecked(capacity, 1) }
}

impl RamFs {
    pub fn new() -> Option<Box<RamFs>> {
        let root = vnode::alloc();
        if root.is_null() {
            trace!(0, "ramfs: no memory for the root");
            return None;
        }

        unsafe {
            (*root).name[0] = b'/';
            (*root).node_type = TYPE_DIR;
            /* In-memory directories are born complete */
            (*root).flags = FLAG_DIR_LOADED;
        }
        Some(Box::new(RamFs { root }))
    }

    pub fn root(&self) -> *mut VNode {
        self.root
    }

    pub fn unmount(&mut self) {
        if !self.root.is_null() {
            unsafe { free_tree(self.root) };
            self.root = core::ptr::null_mut();
        }
    }

    fn new_node(name: &[u8], node_type: i32) -> *mut VNode {
        let node = vnode::alloc();
        if node.is_null() {
            trace!(0, "ramfs: no memory for a node");
            return node;
        }

        unsafe {
            let len = name.len().min(NAME_MAX - 1);
            (&mut (*node).name)[..len].copy_from_slice(&name[..len]);
            (*node).node_type = node_type;
            if node_type == TYPE_DIR {
                (*node).flags = FLAG_DIR_LOADED;
            }
        }
        node
    }

    pub fn lookup(&self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        if dir.is_null() || !unsafe { (*dir).is_dir() } {
            return core::ptr::null_mut();
        }

        for child in unsafe { vnode::children(dir) } {
            if unsafe { (*child).name_is(name) } {
                return child;
            }
        }
        core::ptr::null_mut()
    }

    fn create(&self, dir: *mut VNode, name: &[u8], node_type: i32) -> *mut VNode {
        let null = core::ptr::null_mut();
        if dir.is_null() || name.is_empty() {
            trace!(0, "ramfs: something made with no name");
            return null;
        }
        if !unsafe { (*dir).is_dir() } {
            trace!(0, "ramfs: something made somewhere that is not a directory");
            return null;
        }
        if !self.lookup(dir, name).is_null() {
            trace!(0, "ramfs: there is something by that name already");
            return null;
        }

        let node = RamFs::new_node(name, node_type);
        if !node.is_null() {
            unsafe {
                (*node).parent = dir;
                vnode::insert_child(dir, node);
            }
        }
        node
    }

    pub fn create_file(&self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        self.create(dir, name, TYPE_FILE)
    }

    pub fn create_dir(&self, dir: *mut VNode, name: &[u8]) -> *mut VNode {
        self.create(dir, name, TYPE_DIR)
    }

    /// Room for `size` bytes, keeping what the file holds. The buffer starts
    /// at MIN_CAPACITY and doubles, so a file written a line at a time costs
    /// a logarithmic number of copies rather than one per write.
    fn reserve(file: *mut VNode, size: usize) -> bool {
        let (data, used, capacity) = unsafe { ((*file).data, (*file).size, (*file).capacity) };
        if size <= capacity {
            return true;
        }

        let mut want = if capacity != 0 { capacity } else { MIN_CAPACITY };
        while want < size {
            match want.checked_mul(2) {
                Some(doubled) => want = doubled,
                None => {
                    trace!(0, "ramfs: a file of {} bytes is more than memory holds", size);
                    return false;
                }
            }
        }

        let fresh = unsafe { alloc::alloc::alloc(buffer_layout(want)) };
        if fresh.is_null() {
            trace!(0, "ramfs: no memory for {} bytes", want);
            return false;
        }

        unsafe {
            if !data.is_null() {
                core::ptr::copy_nonoverlapping(data, fresh, used);
                alloc::alloc::dealloc(data, buffer_layout(capacity));
            }
            (*file).data = fresh;
            (*file).capacity = want;
        }
        true
    }

    pub fn read(&self, file: *mut VNode, buf: &mut [u8], offset: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "ramfs: a read of something that is not a file");
            return false;
        }

        let (data, size) = unsafe { ((*file).data, (*file).size) };
        if offset >= size {
            trace!(0, "ramfs: a read at {} is past the {} bytes there are", offset, size);
            return false;
        }

        let take = buf.len().min(size - offset);
        unsafe { core::ptr::copy_nonoverlapping(data.add(offset), buf.as_mut_ptr(), take) };
        true
    }

    pub fn write(&self, file: *mut VNode, data: &[u8], offset: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "ramfs: a write to something that is not a file");
            return false;
        }
        if data.is_empty() {
            return true;
        }

        let end = match offset.checked_add(data.len()) {
            Some(end) => end,
            None => {
                trace!(0, "ramfs: a write of {} bytes at {} is past memory",
                    data.len(), offset);
                return false;
            }
        };

        if !RamFs::reserve(file, end) {
            return false;
        }

        unsafe {
            let buf = (*file).data;
            let size = (*file).size;
            /* Writing past the end leaves a hole that reads as zeros */
            if offset > size {
                core::ptr::write_bytes(buf.add(size), 0, offset - size);
            }
            core::ptr::copy_nonoverlapping(data.as_ptr(), buf.add(offset), data.len());
            if end > size {
                (*file).size = end;
            }
        }
        true
    }

    pub fn truncate(&self, file: *mut VNode, size: usize) -> bool {
        if file.is_null() || !unsafe { (*file).is_file() } {
            trace!(0, "ramfs: a truncate of something that is not a file");
            return false;
        }

        let was = unsafe { (*file).size };
        if size > was {
            if !RamFs::reserve(file, size) {
                return false;
            }
            unsafe { core::ptr::write_bytes((*file).data.add(was), 0, size - was) };
        }

        unsafe { (*file).size = size };
        true
    }

    pub fn rename(&self, node: *mut VNode, new_dir: *mut VNode, new_name: &[u8]) -> bool {
        if node.is_null() || new_dir.is_null() || new_name.is_empty() {
            trace!(0, "ramfs: a rename of nothing");
            return false;
        }
        if unsafe { (*node).parent }.is_null() {
            trace!(0, "ramfs: the root cannot be renamed");
            return false;
        }
        if !unsafe { (*new_dir).is_dir() } {
            trace!(0, "ramfs: the target of a rename is not a directory");
            return false;
        }
        if new_name.len() >= NAME_MAX {
            trace!(0, "ramfs: a name of {} bytes is too long", new_name.len());
            return false;
        }
        if !self.lookup(new_dir, new_name).is_null() {
            trace!(0, "ramfs: there is something by that name already");
            return false;
        }

        unsafe { vnode::rename(node, new_dir, new_name) };
        true
    }

    pub fn remove(&self, node: *mut VNode) -> bool {
        if node.is_null() {
            trace!(0, "ramfs: a remove of nothing");
            return false;
        }
        if unsafe { (*node).parent }.is_null() {
            trace!(0, "ramfs: the root cannot be removed");
            return false;
        }

        unsafe {
            vnode::unlink(node);
            free_tree(node);
        }
        true
    }
}

/// Free a node, its buffer, and everything under it.
///
/// # Safety
/// `node` is a ramfs node, off its parent's list already.
unsafe fn free_tree(node: *mut VNode) {
    loop {
        let child = unsafe { vnode::first_child(node) };
        if child.is_null() {
            break;
        }
        unsafe {
            vnode::unlink(child);
            free_tree(child);
        }
    }

    unsafe {
        let (data, capacity) = ((*node).data, (*node).capacity);
        if !data.is_null() {
            alloc::alloc::dealloc(data, buffer_layout(capacity));
        }
        vnode::free(node);
    }
}

/* ---- the ops table the VFS drives it by ---- */

/// # Safety
/// `ctx` is the pointer a mount was made with, and the filesystem is alive.
unsafe fn fs<'a>(ctx: *mut u8) -> &'a mut RamFs {
    unsafe { &mut *(ctx as *mut RamFs) }
}

/// # Safety
/// `name` points at a NUL-terminated string.
unsafe fn cstr<'a>(name: *const u8) -> &'a [u8] {
    if name.is_null() {
        return &[];
    }
    let mut len = 0;
    while len < NAME_MAX && unsafe { *name.add(len) } != 0 {
        len += 1;
    }
    unsafe { core::slice::from_raw_parts(name, len) }
}

extern "C" fn op_root(ctx: *mut u8) -> *mut VNode {
    unsafe { fs(ctx) }.root()
}

extern "C" fn op_load_dir(_ctx: *mut u8, _dir: *mut VNode) -> i32 {
    /* Born complete: there is nowhere else the entries could be */
    0
}

extern "C" fn op_lookup(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode {
    unsafe { fs(ctx) }.lookup(dir, unsafe { cstr(name) })
}

extern "C" fn op_create_file(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode {
    unsafe { fs(ctx) }.create_file(dir, unsafe { cstr(name) })
}

extern "C" fn op_create_dir(ctx: *mut u8, dir: *mut VNode, name: *const u8) -> *mut VNode {
    unsafe { fs(ctx) }.create_dir(dir, unsafe { cstr(name) })
}

extern "C" fn op_read(
    ctx: *mut u8, file: *mut VNode, buf: *mut u8, len: usize, off: usize,
) -> i32 {
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        return -1;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    if unsafe { fs(ctx) }.read(file, buf, off) { 0 } else { -1 }
}

extern "C" fn op_write(
    ctx: *mut u8, file: *mut VNode, data: *const u8, len: usize, off: usize,
) -> i32 {
    if len == 0 {
        return 0;
    }
    if data.is_null() {
        return -1;
    }
    let data = unsafe { core::slice::from_raw_parts(data, len) };
    if unsafe { fs(ctx) }.write(file, data, off) { 0 } else { -1 }
}

extern "C" fn op_truncate(ctx: *mut u8, file: *mut VNode, size: usize) -> i32 {
    if unsafe { fs(ctx) }.truncate(file, size) { 0 } else { -1 }
}

extern "C" fn op_rename(ctx: *mut u8, node: *mut VNode, dir: *mut VNode, name: *const u8) -> i32 {
    if unsafe { fs(ctx) }.rename(node, dir, unsafe { cstr(name) }) { 0 } else { -1 }
}

extern "C" fn op_remove(ctx: *mut u8, node: *mut VNode) -> i32 {
    if unsafe { fs(ctx) }.remove(node) { 0 } else { -1 }
}

extern "C" fn op_sync(_ctx: *mut u8) -> i32 {
    /* Nowhere to push it to */
    0
}

extern "C" fn op_device(_ctx: *mut u8) -> usize {
    0
}

extern "C" fn op_mount(_ctx: *mut u8, read_only: i32) -> i32 {
    if read_only != 0 { 1 } else { 0 }
}

extern "C" fn op_unmount(ctx: *mut u8) {
    unsafe { fs(ctx) }.unmount();
}

extern "C" fn op_destroy(ctx: *mut u8) {
    drop(unsafe { Box::from_raw(ctx as *mut RamFs) });
}

fn ops_for(fs: *mut RamFs) -> FsOps {
    FsOps {
        name: b"ramfs\0".as_ptr(),
        info: None,
        root: op_root,
        load_dir: op_load_dir,
        lookup: op_lookup,
        create_file: op_create_file,
        create_dir: op_create_dir,
        read: op_read,
        write: op_write,
        truncate: op_truncate,
        rename: op_rename,
        remove: op_remove,
        sync: op_sync,
        device: op_device,
        mount: op_mount,
        unmount: op_unmount,
        destroy: Some(op_destroy),
        ctx: fs as *mut u8,
    }
}

/* ---- what the kernel calls ---- */

/// Mount a fresh ramfs at `path`: 0 mounted, -1 not.
///
/// # Safety
/// `path` points at `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_ramfs_mount(
    path: *const u8, path_len: usize, read_only: i32,
) -> i32 {
    match unsafe { crate::path(path, path_len) } {
        Some(at) if mount_bytes(at, read_only != 0) => 0,
        _ => -1,
    }
}

/// A fresh ramfs at `path`.
pub fn mount_at(path: &str, read_only: bool) -> bool {
    mount_bytes(path.as_bytes(), read_only)
}

fn mount_bytes(at: &[u8], read_only: bool) -> bool {
    let vfs = match crate::vfs_instance() {
        Some(vfs) => vfs,
        None => return false,
    };

    let fs = match RamFs::new() {
        Some(fs) => Box::into_raw(fs),
        None => return false,
    };

    let ops = ops_for(fs);
    if !vfs.mount(at, &ops, read_only) {
        drop(unsafe { Box::from_raw(fs) });
        return false;
    }
    true
}
