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

    /// Which file this is: the same for two opens of one file, under one
    /// name or two, for as long as either is open -- what a claim on the
    /// file is keyed by. None for a handle the kernel no longer knows.
    pub fn identity(&self) -> Option<FileId> {
        let mount = fs::kernel_file_mount(self.handle);
        let key = fs::kernel_file_key(self.handle);
        (mount != 0 && key != 0).then_some(FileId { mount, key })
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

/// Which file an open file is (`File::identity`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileId {
    mount: u64,
    key: u64,
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

/// Room for an entry's name: more than the VFS keeps of one (`fs::vnode::NAME_MAX`, 64), so that
/// what `list` hands back is the whole name.
const NAME_MAX: usize = 255;
/// The most entries `list` reads of one directory.
const LIST_MAX: usize = 4096;
/* What `kernel_dir_entry` answers (fs/src/files.rs). */
const ENTRY_DIR: u32 = 2;
const ENTRY_END: isize = -1;

/// An entry of a directory, as `list` reads it: its name as the filesystem
/// has it -- bytes, not necessarily UTF-8 -- whether it is a directory, and a
/// file's size.
pub struct Entry {
    pub name: Vec<u8>,
    pub dir: bool,
    pub size: u64,
}

/// What the directory at `path` has, an entry each, at most `LIST_MAX` of
/// them: NotFound when the path names no directory. Read an entry at a time
/// with nothing held between them, so a directory changing meanwhile may
/// show an entry twice or not at all.
pub fn list(path: &str) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut name = [0u8; NAME_MAX];
    for index in 0..LIST_MAX {
        let (mut kind, mut size) = (0u32, 0u64);
        /* `name` is writable for its length, `kind` and `size` are locals. */
        let len = unsafe {
            fs::kernel_dir_entry(path.as_ptr(), path.len(), index, name.as_mut_ptr(), name.len(), &mut kind, &mut size)
        };
        if len == ENTRY_END {
            break;
        }
        let len = usize::try_from(len).map_err(|_| Error::NotFound)?.min(name.len());
        let mut entry = Vec::new();
        entry.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
        entry.extend_from_slice(&name[..len]);
        entries.try_reserve(1).map_err(|_| Error::NoMemory)?;
        entries.push(Entry { name: entry, dir: kind == ENTRY_DIR, size });
    }
    Ok(entries)
}

/// Appends `data` to the file, making it if it is missing. Not synced: a
/// log's lines. A write cut short -- the disk full -- may leave a first part.
pub fn append(path: &str, data: &[u8]) -> Result<()> {
    let rc = unsafe { fs::kernel_file_append(path.as_ptr(), path.len(), data.as_ptr(), data.len()) };
    if rc == 0 { Ok(()) } else { Err(Error::IoError) }
}

/// Renames a file or a directory, on one filesystem: refused when the new
/// path is taken, or anything under the old one is open.
pub fn rename(from: &str, to: &str) -> Result<()> {
    let rc = unsafe { fs::kernel_file_rename(from.as_ptr(), from.len(), to.as_ptr(), to.len()) };
    if rc == 0 { Ok(()) } else { Err(Error::IoError) }
}

/// Removes the file: NotFound when there was nothing to remove, or it could
/// not be.
pub fn remove(path: &str) -> Result<()> {
    let rc = unsafe { fs::kernel_file_remove(path.as_ptr(), path.len()) };
    if rc == 0 { Ok(()) } else { Err(Error::NotFound) }
}
