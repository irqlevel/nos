//! A raft log's folder over kcore::fs, as `raft::log::Files` has one.

use alloc::string::String;
use alloc::vec::Vec;

use kcore::error::Error;
use kcore::fs::File;
use raft::log::{FileError, Files};

/// The log's folder: its calls are kcore::fs's, its paths under `dir`.
pub struct KFiles {
    dir: String,
    /// What a sync of the folder syncs: an empty file of the root's.
    mark: String,
}

/// Every folder of `path` that is not there made, from the root down:
/// `/nbs/mds` on a host with no `/nbs`. One that cannot be made is the
/// next call's to say.
pub fn create_dirs(path: &str) {
    let mut at = 0;
    while let Some(i) = path[at + 1..].find('/').map(|i| i + at + 1) {
        let _ = kcore::fs::create_dir(&path[..i]);
        at = i;
    }
    let _ = kcore::fs::create_dir(path);
}

impl KFiles {
    /// The log's folder, `<root>/<sub>`, and the root, made where they are
    /// not there; `<root>/.sync`, an empty file a sync of the folder syncs --
    /// nos's file system makes everything durable at any file's sync.
    pub fn new(root: &str, sub: &str) -> Result<KFiles, String> {
        let root = root.trim_end_matches('/');
        create_dirs(root);
        let dir = alloc::format!("{}/{}", root, sub);
        kcore::fs::create_dir(&dir).map_err(|_| alloc::format!("cannot make {}", dir))?;
        let mark = alloc::format!("{}/.sync", root);
        match kcore::fs::create(&mark, b"") {
            Ok(()) | Err(Error::Busy) => {}
            Err(_) => return Err(alloc::format!("cannot make {}", mark)),
        }
        Ok(KFiles { dir, mark })
    }

    /// The folder's path.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    fn path(&self, name: &str) -> String {
        alloc::format!("{}/{}", self.dir, name)
    }

    /// Every file of the folder removed, the folder synced if there were
    /// any: what a start of a member that did not finish left.
    pub fn clear(&mut self) -> Result<(), String> {
        let mut names = Vec::new();
        self.list(&mut names).map_err(|_| alloc::format!("cannot list {}", self.dir))?;
        for name in &names {
            self.remove(name).map_err(|_| alloc::format!("cannot remove {}/{}", self.dir, name))?;
        }
        if !names.is_empty() {
            self.sync_dir().map_err(|_| alloc::format!("cannot sync {}", self.dir))?;
        }
        Ok(())
    }
}

fn file_error(e: Error) -> FileError {
    if e == Error::NotFound { FileError::NotFound } else { FileError::Io }
}

impl Files for KFiles {
    fn list(&mut self, out: &mut Vec<String>) -> Result<(), FileError> {
        for e in kcore::fs::list(&self.dir).map_err(file_error)? {
            if e.dir {
                continue;
            }
            /* The log names its files in ASCII: anything else is not its */
            if let Ok(name) = String::from_utf8(e.name) {
                out.push(name);
            }
        }
        Ok(())
    }

    fn len(&mut self, name: &str) -> Result<u64, FileError> {
        kcore::fs::size(&self.path(name)).map_err(file_error)
    }

    fn read_at(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FileError> {
        kcore::fs::read_at(&self.path(name), offset, buf).map_err(file_error)
    }

    fn append(&mut self, name: &str, data: &[u8]) -> Result<(), FileError> {
        kcore::fs::append(&self.path(name), data).map_err(file_error)
    }

    fn truncate(&mut self, name: &str, len: u64) -> Result<(), FileError> {
        File::open(&self.path(name), true).and_then(|f| f.set_len(len)).map_err(file_error)
    }

    fn sync(&mut self, name: &str) -> Result<(), FileError> {
        File::open(&self.path(name), false).and_then(|f| f.sync()).map_err(file_error)
    }

    fn sync_dir(&mut self) -> Result<(), FileError> {
        /* A file's sync is the whole file system's on nos */
        File::open(&self.mark, false).and_then(|f| f.sync()).map_err(file_error)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), FileError> {
        kcore::fs::rename(&self.path(from), &self.path(to)).map_err(file_error)
    }

    fn remove(&mut self, name: &str) -> Result<(), FileError> {
        kcore::fs::remove(&self.path(name)).map_err(file_error)
    }
}
