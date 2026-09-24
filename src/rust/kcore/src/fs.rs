//! Files, for a module to keep its configuration in -- a host key, the keys
//! allowed to log in -- by paths that are absolute, with nothing held open
//! between calls; and a file held open (`File`), for a guest's disk.

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

/// The file's size in bytes: NotFound when there is no such file.
pub fn size(path: &str) -> Result<u64> {
    let size = unsafe { fs::kernel_file_size(path.as_ptr(), path.len()) };
    if size < 0 {
        return Err(Error::NotFound);
    }
    Ok(size as u64)
}

/// As much of `buf` as the file has from `offset`: the count read, 0 at or
/// past its end. For a file too large to read whole -- a guest's kernel --
/// a piece at a time.
pub fn read_at(path: &str, offset: u64, buf: &mut [u8]) -> Result<usize> {
    let got = unsafe {
        fs::kernel_file_read_at(path.as_ptr(), path.len(), offset, buf.as_mut_ptr(), buf.len())
    };
    if got < 0 {
        return Err(Error::IoError);
    }
    Ok(got as usize)
}

/// A file held open -- a guest's disk image, read and written where the guest
/// asks for as long as it runs -- and closed when this goes. While it is
/// open the file cannot be removed or renamed from under whoever has it, nor
/// its filesystem unmounted. Every call takes the kernel's handle to the
/// file, which the kernel looks up rather than follows, and positions are
/// the caller's: two tasks may share one.
pub struct File {
    handle: usize,
}

impl File {
    /// The regular file at `path`, for reading -- and with `write` for
    /// writing too. NotFound when there is none, or it cannot be opened so
    /// (a directory, a read-only mount).
    pub fn open(path: &str, write: bool) -> Result<File> {
        let handle = unsafe { fs::kernel_file_open(path.as_ptr(), path.len(), i32::from(write)) };
        if handle == 0 {
            return Err(Error::NotFound);
        }
        Ok(File { handle })
    }

    /// Its size in bytes.
    pub fn size(&self) -> Result<u64> {
        u64::try_from(fs::kernel_file_length(self.handle)).map_err(|_| Error::IoError)
    }

    /// As much of `buf` as the file has from `offset`: the count read, 0 at
    /// or past its end.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        /* `buf` is writable for its length, and the handle is looked up. */
        let got = unsafe { fs::kernel_file_pread(self.handle, offset, buf.as_mut_ptr(), buf.len()) };
        usize::try_from(got).map_err(|_| Error::IoError)
    }

    /// Writes `data` into the file at `offset`, within the size the file has
    /// -- a disk image, whose size is its disk's: a write that would reach
    /// past the end is refused whole, and the file never grows. Not synced:
    /// `sync` is.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        /* `data` is readable for its length, and the handle is looked up. */
        let put = unsafe { fs::kernel_file_pwrite(self.handle, offset, data.as_ptr(), data.len()) };
        if usize::try_from(put) != Ok(data.len()) {
            return Err(Error::IoError);
        }
        Ok(())
    }

    /// Everything written to the file's filesystem, on its disk's medium: a
    /// guest's flush.
    pub fn sync(&self) -> Result<()> {
        if fs::kernel_file_fsync(self.handle) == 0 { Ok(()) } else { Err(Error::IoError) }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        fs::kernel_file_close(self.handle);
    }
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
