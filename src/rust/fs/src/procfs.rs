//! procfs: what the kernel says about itself, as files.
//!
//! `/proc/version` and `/proc/cmdline` are written once at mount;
//! `/proc/interrupts` is written again on every lookup, because the VFS
//! looks a file up before it reads it or reports its size, so refreshing
//! there keeps the two consistent with each other.
//!
//! The files themselves are a [`RamFs`] -- procfs is that, with three of
//! them in it and one that keeps changing.

use alloc::boxed::Box;

use kcore::procinfo;
use kcore::trace;

use crate::ramfs::RamFs;
use crate::vfs::FileSystem;
use crate::vnode::{NodeId, Tree};

/// What one rendering of /proc/interrupts fits in.
const INTERRUPTS_MAX: usize = 512;

pub struct ProcFs {
    ram: RamFs,
    interrupts: Option<NodeId>,
}

impl ProcFs {
    pub fn new() -> Option<ProcFs> {
        Some(ProcFs { ram: RamFs::new()?, interrupts: None })
    }

    fn fill(&mut self) -> bool {
        let root = match self.ram.root() {
            Some(root) => root,
            None => return false,
        };

        let mut buf = [0u8; 128];
        let len = procinfo::version(&mut buf);
        self.put(root, b"version", &buf[..len]);

        let mut buf = [0u8; crate::vfs::MAX_PATH];
        let len = procinfo::cmdline(&mut buf);
        self.put(root, b"cmdline", &buf[..len]);

        self.interrupts = self.ram.create_file(root, b"interrupts");
        if self.interrupts.is_none() {
            trace!(0, "procfs: no memory for /proc/interrupts");
        } else {
            self.refresh_interrupts();
        }

        true
    }

    /// A file with this content, made once at mount.
    fn put(&mut self, root: NodeId, name: &[u8], content: &[u8]) {
        match self.ram.create_file(root, name) {
            Some(node) => { self.ram.write(node, content, 0); }
            None => trace!(0, "procfs: no memory for a file"),
        }
    }

    /// The interrupt counters as they are now. What does not fit is left
    /// out, as it was when this rendered into a fixed buffer in C++.
    fn refresh_interrupts(&mut self) {
        let file = match self.interrupts {
            Some(file) => file,
            None => return,
        };

        let mut out = [0u8; INTERRUPTS_MAX];
        let mut at = 0;

        for index in 0..procinfo::interrupt_source_count() {
            let mut name = [0u8; 32];
            let (len, count) = match procinfo::interrupt_source(index, &mut name) {
                Some(source) => source,
                None => break,
            };

            let written = line(&mut out[at..], &name[..len], count);
            if written == 0 {
                break;
            }
            at += written;
        }

        self.ram.truncate(file, 0);
        self.ram.write(file, &out[..at], 0);
    }
}

/// One "<name padded to 12> <count right-aligned in 10>\n" into `out`; the
/// bytes written, or 0 when the line does not fit.
fn line(out: &mut [u8], name: &[u8], count: i64) -> usize {
    let mut digits = [0u8; 20];
    let mut number = count.unsigned_abs();
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (number % 10) as u8;
        number /= 10;
        if number == 0 {
            break;
        }
    }
    if count < 0 && at > 0 {
        at -= 1;
        digits[at] = b'-';
    }
    let digits = &digits[at..];

    let name_width = name.len().max(12);
    let count_width = digits.len().max(10);
    let total = name_width + 1 + count_width + 1;
    if total > out.len() {
        return 0;
    }

    out[..name.len()].copy_from_slice(name);
    for byte in out[name.len()..name_width].iter_mut() {
        *byte = b' ';
    }
    out[name_width] = b' ';

    let pad = count_width - digits.len();
    for byte in out[name_width + 1..name_width + 1 + pad].iter_mut() {
        *byte = b' ';
    }
    out[name_width + 1 + pad..total - 1].copy_from_slice(digits);
    out[total - 1] = b'\n';
    total
}

/* Nothing is made, written, moved or removed in procfs from outside. */
impl FileSystem for ProcFs {
    fn name(&self) -> &'static str {
        "procfs"
    }

    /// Read-only whatever the mount asked for: these files are the kernel's.
    fn mount(&mut self, _read_only: bool) -> Option<bool> {
        if self.fill() { Some(true) } else { None }
    }

    fn unmount(&mut self) {
        self.ram.unmount();
        self.interrupts = None;
    }

    fn tree(&self) -> &Tree {
        self.ram.tree()
    }

    fn tree_mut(&mut self) -> &mut Tree {
        FileSystem::tree_mut(&mut self.ram)
    }

    fn root(&self) -> Option<NodeId> {
        self.ram.root()
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Option<NodeId> {
        let node = self.ram.lookup(dir, name)?;
        if Some(node) == self.interrupts {
            self.refresh_interrupts();
        }
        Some(node)
    }

    fn create_file(&mut self, _dir: NodeId, _name: &[u8]) -> Option<NodeId> {
        None
    }

    fn create_dir(&mut self, _dir: NodeId, _name: &[u8]) -> Option<NodeId> {
        None
    }

    fn read(&mut self, file: NodeId, buf: &mut [u8], offset: usize) -> bool {
        self.ram.read(file, buf, offset)
    }

    fn write(&mut self, _file: NodeId, _data: &[u8], _offset: usize) -> bool {
        false
    }

    fn truncate(&mut self, _file: NodeId, _size: usize) -> bool {
        false
    }

    fn rename(&mut self, _node: NodeId, _dir: NodeId, _name: &[u8]) -> bool {
        false
    }

    fn remove(&mut self, _node: NodeId) -> bool {
        false
    }
}

/// Mount procfs at `path`. Read-only: there is nothing in it to write.
pub fn mount_at(path: &str) -> bool {
    let (vfs, fs) = match (crate::vfs_instance(), ProcFs::new()) {
        (Some(vfs), Some(fs)) => (vfs, fs),
        _ => return false,
    };
    vfs.mount(path.as_bytes(), Box::new(fs), true).is_some()
}
