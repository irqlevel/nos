//! Paths as the shell and the file helpers pass them around: a fixed buffer,
//! because the VFS takes a byte slice and nothing here may depend on an
//! allocation succeeding on a path that is at most 256 bytes anyway.

use crate::vfs::MAX_PATH;

/// A path being built. Longer than `MAX_PATH - 1` is refused rather than cut:
/// half a path is a different path, and `/etc/rc` cut short is `/etc/r`.
pub struct Path {
    buf: [u8; MAX_PATH],
    len: usize,
}

impl Path {
    pub const fn new() -> Self {
        Self { buf: [0; MAX_PATH], len: 0 }
    }

    pub fn from(text: &str) -> Option<Self> {
        let mut path = Self::new();
        path.push(text)?;
        Some(path)
    }

    /// `dir` and `name` with exactly one slash between them.
    pub fn join(dir: &str, name: &str) -> Option<Self> {
        let mut path = Self::from(dir)?;
        if !dir.ends_with('/') {
            path.push("/")?;
        }
        path.push(name)?;
        Some(path)
    }

    pub fn push(&mut self, text: &str) -> Option<()> {
        if self.len + text.len() >= MAX_PATH {
            return None;
        }
        self.buf[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len();
        Some(())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn as_str(&self) -> &str {
        /* Everything put in came from a &str. */
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The name after the last slash: what a copy into a directory is called.
pub fn base_name(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(at) => &path[at + 1..],
        None => path,
    }
}

/// True when `path` is inside `dir`, or is `dir` itself.
pub fn is_under(dir: &str, path: &str) -> bool {
    let trimmed = {
        let mut end = dir.len();
        while end > 1 && dir.as_bytes()[end - 1] == b'/' {
            end -= 1;
        }
        &dir[..end]
    };

    if !path.starts_with(trimmed) {
        return false;
    }
    trimmed.len() == 1
        || path.len() == trimmed.len()
        || path.as_bytes()[trimmed.len()] == b'/'
}
