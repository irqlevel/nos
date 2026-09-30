//! What a filesystem holds, as the fuzzer keeps it: a tree of directories
//! and files with their content -- what an image is made from
//! (`image::ext2::mkfs`), what one is read back into (`image::ext2::check`),
//! and what a run of operations should have left (the targets' models).

use std::collections::BTreeMap;

/// A file's content: its length, and what of it is not zeros -- kept a
/// page at a time, so that a file of gigabytes with a few pages written
/// costs a few pages.
#[derive(Clone, Debug, Default)]
pub struct Data {
    len: u64,
    chunks: BTreeMap<u64, Vec<u8>>,
}

const CHUNK: u64 = 4096;

impl Data {
    pub fn new() -> Data {
        Data::default()
    }

    pub fn from(bytes: &[u8]) -> Data {
        let mut d = Data::new();
        d.write(0, bytes);
        d
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `n` bytes from `at`, or as many as there are.
    pub fn read(&self, at: u64, n: u64) -> Vec<u8> {
        let end = at.saturating_add(n).min(self.len);
        if at >= end {
            return Vec::new();
        }
        let mut v = vec![0u8; (end - at) as usize];
        for (&i, chunk) in self.chunks.range(at / CHUNK..=(end - 1) / CHUNK) {
            let base = i * CHUNK;
            let from = base.max(at);
            let to = (base + CHUNK).min(end);
            v[(from - at) as usize..(to - at) as usize].copy_from_slice(&chunk[(from - base) as usize..(to - base) as usize]);
        }
        v
    }

    /// `bytes` at `at`, the file grown to reach past them if it must.
    pub fn write(&mut self, at: u64, bytes: &[u8]) {
        let mut done = 0usize;
        while done < bytes.len() {
            let pos = at + done as u64;
            let (i, off) = (pos / CHUNK, (pos % CHUNK) as usize);
            let take = (CHUNK as usize - off).min(bytes.len() - done);
            let piece = &bytes[done..done + take];
            if piece.iter().any(|b| *b != 0) || self.chunks.contains_key(&i) {
                let chunk = self.chunks.entry(i).or_insert_with(|| vec![0u8; CHUNK as usize]);
                chunk[off..off + take].copy_from_slice(piece);
            }
            done += take;
        }
        self.len = self.len.max(at + bytes.len() as u64);
    }

    /// Cut to `len`, or grown to it with zeros.
    pub fn truncate(&mut self, len: u64) {
        if len < self.len {
            self.chunks.retain(|&i, _| i * CHUNK < len);
            if let Some(chunk) = self.chunks.get_mut(&(len / CHUNK)) {
                chunk[(len % CHUNK) as usize..].fill(0);
            }
        }
        self.len = len;
    }

    /// The indices of the `block`-byte blocks that are not all zeros.
    pub fn nonzero_blocks(&self, block: u64) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        for (&i, chunk) in &self.chunks {
            for (k, piece) in chunk.chunks(block.min(CHUNK) as usize).enumerate() {
                let b = (i * CHUNK + k as u64 * block.min(CHUNK)) / block;
                if b * block < self.len && piece.iter().any(|x| *x != 0) && out.last() != Some(&b) {
                    out.push(b);
                }
            }
        }
        out.dedup();
        out
    }

    /// The whole of it, for a file small enough to have whole.
    pub fn to_vec(&self) -> Vec<u8> {
        self.read(0, self.len)
    }

    /// Where two contents first differ, if they do.
    pub fn first_difference(&self, other: &Data) -> Option<u64> {
        if self.len != other.len {
            return Some(self.len.min(other.len));
        }
        let indices: std::collections::BTreeSet<u64> = self.chunks.keys().chain(other.chunks.keys()).copied().collect();
        for i in indices {
            let a = self.read(i * CHUNK, CHUNK);
            let b = other.read(i * CHUNK, CHUNK);
            if let Some(k) = a.iter().zip(&b).position(|(x, y)| x != y) {
                return Some(i * CHUNK + k as u64);
            }
        }
        None
    }
}

impl PartialEq for Data {
    fn eq(&self, other: &Data) -> bool {
        self.first_difference(other).is_none()
    }
}

impl Eq for Data {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Dir(BTreeMap<Vec<u8>, Node>),
    File(Data),
}

impl Node {
    pub fn dir() -> Node {
        Node::Dir(BTreeMap::new())
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Node::Dir(_))
    }

    pub fn children(&self) -> Option<&BTreeMap<Vec<u8>, Node>> {
        match self {
            Node::Dir(c) => Some(c),
            Node::File(_) => None,
        }
    }

    pub fn children_mut(&mut self) -> Option<&mut BTreeMap<Vec<u8>, Node>> {
        match self {
            Node::Dir(c) => Some(c),
            Node::File(_) => None,
        }
    }

    /// The node at `path`, components from the root.
    pub fn get(&self, path: &[Vec<u8>]) -> Option<&Node> {
        let mut at = self;
        for c in path {
            at = at.children()?.get(c)?;
        }
        Some(at)
    }

    pub fn get_mut(&mut self, path: &[Vec<u8>]) -> Option<&mut Node> {
        let mut at = self;
        for c in path {
            at = at.children_mut()?.get_mut(c)?;
        }
        Some(at)
    }

    /// The first difference between two trees, said as a path and what
    /// each has there; None when they are the same.
    pub fn diff(&self, other: &Node) -> Option<String> {
        fn walk(a: &Node, b: &Node, at: &str) -> Option<String> {
            match (a, b) {
                (Node::File(x), Node::File(y)) => {
                    if x.len() != y.len() {
                        return Some(format!("{}: {} bytes, and {}", at, x.len(), y.len()));
                    }
                    let first = x.first_difference(y)?;
                    Some(format!("{}: {} bytes, differing first at {} ({:#04x}, and {:#04x})", at, x.len(), first,
                                 x.read(first, 1)[0], y.read(first, 1)[0]))
                }
                (Node::Dir(x), Node::Dir(y)) => {
                    for (name, child) in x {
                        let path = format!("{}/{}", at, show(name));
                        match y.get(name) {
                            Some(other) => {
                                if let Some(d) = walk(child, other, &path) {
                                    return Some(d);
                                }
                            }
                            None => return Some(format!("{}: there, and not", path)),
                        }
                    }
                    for name in y.keys() {
                        if !x.contains_key(name) {
                            return Some(format!("{}/{}: not there, and there", at, show(name)));
                        }
                    }
                    None
                }
                (Node::Dir(_), Node::File(_)) => Some(format!("{}: a directory, and a file", at)),
                (Node::File(_), Node::Dir(_)) => Some(format!("{}: a file, and a directory", at)),
            }
        }
        walk(self, other, "")
    }
}

/// A name as it can be printed.
pub fn show(name: &[u8]) -> String {
    String::from_utf8_lossy(name).escape_debug().to_string()
}

/// Components of a path as a string: "/a/b".
pub fn join(path: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in path {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}
