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
