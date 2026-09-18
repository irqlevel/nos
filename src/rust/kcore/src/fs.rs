//! Files, for a module to keep its configuration in -- a host key, the keys
//! allowed to log in. Paths are absolute, and nothing is held open between
//! calls.

use alloc::vec::Vec;
use ffi::fs;

use crate::error::{Error, Result};

/// The whole of a regular file: NotFound when there is none, InvalidValue
/// when it is longer than `max`. A `write` cut short between its two steps
/// is read from where it left the content.
pub fn read(path: &str, max: usize) -> Result<Vec<u8>> {
    let size = unsafe { fs::kernel_file_size(path.as_ptr(), path.len()) };
    if size < 0 {
        return Err(Error::NotFound);
    }
    let size = size as usize;
    if size > max {
        return Err(Error::InvalidValue);
    }

    let mut buf = Vec::new();
    buf.try_reserve_exact(size).map_err(|_| Error::NoMemory)?;
    buf.resize(size, 0);
    let got = unsafe { fs::kernel_file_read(path.as_ptr(), path.len(), buf.as_mut_ptr(), size) };
    if got < 0 {
        return Err(Error::IoError);
    }
    buf.truncate(got as usize);
    Ok(buf)
}

/// Replaces the file's content, making the file if it is missing: the new
/// content is written whole, and synced, before it takes the old one's place
/// -- a full disk or a crash midway leaves the old content, never an empty
/// file.
pub fn write(path: &str, data: &[u8]) -> Result<()> {
    let rc = unsafe { fs::kernel_file_write(path.as_ptr(), path.len(), data.as_ptr(), data.len()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::IoError)
    }
}

/// A new file with this content; Busy, and nothing written, when there is
/// one at the path already -- also when `read` could not see it. For a file
/// that must never be written over by mistake.
pub fn create(path: &str, data: &[u8]) -> Result<()> {
    let rc = unsafe { fs::kernel_file_create(path.as_ptr(), path.len(), data.as_ptr(), data.len()) };
    match rc {
        0 => Ok(()),
        1 => Err(Error::Busy),
        _ => Err(Error::IoError),
    }
}

/// Makes a directory, and is content with one already there.
pub fn create_dir(path: &str) -> Result<()> {
    let rc = unsafe { fs::kernel_dir_create(path.as_ptr(), path.len()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::IoError)
    }
}

/* ---- the streaming half ---- */

/// Open flags, as the VFS takes them.
pub const OPEN_READ: usize = 1;
pub const OPEN_WRITE: usize = 2;
pub const OPEN_CREATE: usize = 4;
pub const OPEN_TRUNCATE: usize = 8;
pub const OPEN_APPEND: usize = 16;

/// An open file, closed when it goes out of scope. For content that must not
/// exist in memory all at once -- a download written as it arrives.
///
/// An open handle holds its filesystem in place: nothing may unmount it, and
/// nothing may remove or rename the file, until this is dropped.
pub struct File {
    handle: *mut core::ffi::c_void,
}

impl File {
    pub fn open(path: &str, flags: usize) -> Option<Self> {
        let handle = unsafe { fs::kernel_vfs_open(path.as_ptr(), path.len(), flags) };
        if handle.is_null() { None } else { Some(Self { handle }) }
    }

    /// A file to write from the beginning, made if it is missing and emptied
    /// if it is not.
    pub fn create(path: &str) -> Option<Self> {
        Self::open(path, OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE)
    }

    pub fn write(&self, data: &[u8]) -> bool {
        unsafe { fs::kernel_vfs_write(self.handle, data.as_ptr(), data.len()) == 0 }
    }

    /// What was read; `Some(0)` at end of file, None on an error.
    pub fn read(&self, buf: &mut [u8]) -> Option<usize> {
        let mut got = 0usize;
        let rc = unsafe { fs::kernel_vfs_read(self.handle, buf.as_mut_ptr(), buf.len(), &mut got) };
        if rc == 0 { Some(got) } else { None }
    }

    pub fn size(&self) -> usize {
        unsafe { fs::kernel_vfs_size(self.handle) }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe { fs::kernel_vfs_close(self.handle) };
    }
}

/// Takes the file away. Unlike `read` and `write` this names the path
/// exactly, with no looking in the other place a cut-short write may have
/// left the content -- `remove_content` does that.
pub fn remove(path: &str) -> bool {
    unsafe { fs::kernel_vfs_remove(path.as_ptr(), path.len()) == 0 }
}

/// The file wherever a `write` left it -- at the path, and at `<path>.new`
/// should one have been cut short leaving both.
pub fn remove_content(path: &str) -> bool {
    unsafe { fs::kernel_file_remove(path.as_ptr(), path.len()) == 0 }
}
