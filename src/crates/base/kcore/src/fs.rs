//! Files, for a module to keep its configuration in -- a host key, the keys
//! allowed to log in -- by paths that are absolute, with nothing held open
//! between calls; and a file held open (`File`), for a guest's disk.

use alloc::vec::Vec;
use ffi::fs;

use crate::error::{Error, Result};
use crate::lent::Buffers;

/// How many times `read` sizes a file again that grew between its look and
/// its read -- replaced by a longer one -- before it says Busy.
const READ_TRIES: usize = 4;

/// The whole of a regular file: NotFound when there is none, InvalidValue
/// when it is longer than `max`, Busy when it grew under every one of a few
/// tries. A `write` cut short between its two steps is read from where it
/// left the content.
///
/// Sized, then read in one call that holds the file open throughout --
/// asked for one byte more than the size, which comes only when the file
/// grew between the two: sized again then, rather than taken cut short for
/// Ok, as a file replaced by a longer one once was. A read of the size or
/// less is a whole file, the one the call opened.
pub fn read(path: &str, max: usize) -> Result<Vec<u8>> {
    for _ in 0..READ_TRIES {
        let size = unsafe { fs::kernel_file_size(path.as_ptr(), path.len()) };
        if size < 0 {
            return Err(Error::NotFound);
        }
        let size = size as usize;
        if size > max {
            return Err(Error::InvalidValue);
        }

        let want = size + 1;
        let mut buf = Vec::new();
        buf.try_reserve_exact(want).map_err(|_| Error::NoMemory)?;
        buf.resize(want, 0);
        let got = unsafe { fs::kernel_file_read(path.as_ptr(), path.len(), buf.as_mut_ptr(), want) };
        if got < 0 {
            return Err(Error::IoError);
        }
        let got = got as usize;
        if got <= size {
            buf.truncate(got);
            return Ok(buf);
        }
    }
    Err(Error::Busy)
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

    /// The file, open for writing, cut to `size` bytes or grown to them --
    /// with a hole, on a filesystem that has them: a disk image given its
    /// disk's size before anything is written into it, so that what is
    /// never written costs nothing and reads as zeros.
    pub fn set_len(&self, size: u64) -> Result<()> {
        if fs::kernel_file_set_length(self.handle, size) == 0 { Ok(()) } else { Err(Error::IoError) }
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

/// A file held as a guest's disk image, and let go of when this goes: while
/// it is held nothing else may write the file, cut it, grow it, rename it or
/// remove it -- and its reads and writes go to the device off the VFS's
/// lock, where the filesystem keeps the file's blocks on one (`fs::image`),
/// so one guest's disk does not wait behind another's or behind nos's own
/// files. Held to read, any number may hold a file nobody writes; held to
/// write, one alone. Every call takes the kernel's word for it, looked up.
pub struct Image {
    handle: usize,
}

impl Image {
    /// The regular file at `path`, held for reading -- and with `write` for
    /// writing too. NotFound when there is none, or it cannot be held so: a
    /// directory, a read-only mount, someone else writing it or holding it
    /// for writing, no memory.
    pub fn open(path: &str, write: bool) -> Result<Image> {
        let handle = unsafe { fs::kernel_image_open(path.as_ptr(), path.len(), i32::from(write)) };
        if handle == 0 {
            return Err(Error::NotFound);
        }
        Ok(Image { handle })
    }

    /// Its size in bytes: the disk's, for as long as it is held.
    pub fn size(&self) -> Result<u64> {
        u64::try_from(fs::kernel_image_length(self.handle)).map_err(|_| Error::IoError)
    }

    /// Which file this is, as `File::identity` says it: what a claim on it
    /// is keyed by.
    pub fn identity(&self) -> Option<FileId> {
        let mount = fs::kernel_image_mount(self.handle);
        let key = fs::kernel_image_key(self.handle);
        (mount != 0 && key != 0).then_some(FileId { mount, key })
    }

    /// Fills `buf` from `offset`, all of it inside the image.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        /* `buf` is writable for its length, and the handle is looked up. */
        let rc = unsafe { fs::kernel_image_read(self.handle, offset, buf.as_mut_ptr(), buf.len()) };
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// Writes `data` at `offset`, all of it inside the image, which never
    /// grows. Not synced: `sync` is.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        /* `data` is readable for its length, and the handle is looked up. */
        let rc = unsafe { fs::kernel_image_write(self.handle, offset, data.as_ptr(), data.len()) };
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// Where the image has data from `offset` on, as `(start, end)`: holes
    /// from `offset` to `start`, and what its filesystem keeps on the device
    /// from `start` to `end`, where the next hole begins -- at most a few
    /// thousand blocks looked at, and never past the end. `start == end`
    /// where the blocks looked at are all holes: holes up to there, and the
    /// next call goes on from it. What a copy that leaves the holes out
    /// reads, a call at a time; `(size, size)` from the end on.
    pub fn extent(&self, offset: u64) -> Result<(u64, u64)> {
        let mut out = [0u64; 2];
        /* `out` takes two u64s, and the handle is looked up. */
        let rc = unsafe { fs::kernel_image_extent(self.handle, offset, out.as_mut_ptr()) };
        if rc == 0 { Ok((out[0], out[1])) } else { Err(Error::IoError) }
    }

    /// The whole chunks inside `len` bytes from `offset` given back to the
    /// filesystem -- holes from here on, reading as zeros, the image's size
    /// as it was: a guest's discard. What it covers of a chunk only in part
    /// is left as it is (`discard_unit`).
    pub fn discard(&self, offset: u64, len: u64) -> Result<()> {
        if fs::kernel_image_discard(self.handle, offset, len) == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// What a discard gives back whole, in bytes -- the image's chunk -- or 0
    /// where it gives back nothing: what a guest's disk tells its driver to
    /// align its discards to.
    pub fn discard_unit(&self) -> u64 {
        fs::kernel_image_discard_unit(self.handle)
    }

    /// `len` bytes from `offset` made zeros: with `unmap` the whole blocks
    /// among them given back, the rest written over.
    pub fn write_zeroes(&self, offset: u64, len: u64, unmap: bool) -> Result<()> {
        let rc = fs::kernel_image_write_zeroes(self.handle, offset, len, i32::from(unmap));
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// Everything written to the image, on its disk's medium: a guest's
    /// flush.
    pub fn sync(&self) -> Result<()> {
        if fs::kernel_image_sync(self.handle) == 0 { Ok(()) } else { Err(Error::IoError) }
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        /* This image's alone; its queue, which borrows it, is gone, and with
         * it every `Lent` of the queue's buffers */
        unsafe { fs::kernel_image_close(self.handle) };
    }
}

pub use crate::lent::{Lent, Started};

/// An image held with a queue of its own (`fs::image`'s): buffers of the
/// image's, one a request -- filled with a write's data before the request
/// is started, a read's taken out once it is reaped -- and requests with
/// the device as many at once as are started, each given back as it is
/// done. Shared: whoever fills and empties the buffers (a guest's vCPU)
/// and whoever starts and reaps their requests (its disk's task) each hold
/// it; every call but `lend` and `wake` from one task. A buffer is reached
/// as `crate::lent` says: through one guard at a time, never while its
/// request is the image's.
pub struct ImageQueue {
    /* Its buffers before the image, which keeps them: dropped first. */
    buffers: Buffers,
    image: Image,
}

impl Image {
    /// The image held with a queue: `buffers` buffers of `bytes` each, a
    /// whole number of pages. NoMemory when it cannot have them.
    pub fn into_queue(self, buffers: usize, bytes: usize) -> Result<ImageQueue> {
        self.queue_on(0, buffers, bytes)
    }

    /// `into_queue`, made on `event` -- the event of a task that waits for
    /// this and more: the queue's completions and wakes signal that event as
    /// well as the queue's own, which `wait` and `poll` still sleep on. The
    /// kernel holds the event for as long as the queue is open, so `event`
    /// may be dropped first.
    pub fn into_queue_on(self, event: &crate::events::SharedEvent, buffers: usize, bytes: usize)
        -> Result<ImageQueue>
    {
        self.queue_on(event.handle(), buffers, bytes)
    }

    fn queue_on(self, event: u64, buffers: usize, bytes: usize) -> Result<ImageQueue> {
        let mut at = Vec::new();
        at.try_reserve_exact(buffers).map_err(|_| Error::NoMemory)?;
        at.resize(buffers, core::ptr::null_mut());
        /* `at` takes `buffers` pointers; the handle and the event are looked
         * up. */
        if unsafe { fs::kernel_image_queue(self.handle, buffers, bytes, at.as_mut_ptr(), event) } != 0 {
            return Err(Error::NoMemory);
        }
        /* The kernel's buffers, `bytes` each, kept for as long as the image
         * is held -- the queue holds it, and lets go of it after these. */
        let buffers = unsafe { Buffers::new(at, bytes)? };
        Ok(ImageQueue { buffers, image: self })
    }
}

impl ImageQueue {
    /// The image, for what is not a request: its size, a flush, a discard.
    pub fn image(&self) -> &Image {
        &self.image
    }

    pub fn buffers(&self) -> usize {
        self.buffers.count()
    }

    /// A buffer's bytes.
    pub fn buffer_len(&self) -> usize {
        self.buffers.len()
    }

    /// Buffer `k`'s bytes, while nobody else has them: None while they are
    /// lent already, or the image's.
    pub fn lend(&self, k: usize) -> Option<Lent<'_>> {
        self.buffers.lend(k)
    }

    /// Buffer `k`'s request -- `len` bytes from `offset` read into it, or
    /// with `write` written from it -- to the image: `InFlight` until `reap`
    /// gives it back, or `Done`. IoError for one that failed, Busy for a
    /// buffer lent or started already. Its doorbell waits for `kick`.
    pub fn start(&self, k: usize, write: bool, offset: u64, len: usize) -> Result<Started> {
        self.buffers.take(k)?;
        /* `take` made buffer `k` this call's: no `Lent` of it is out, and
         * the queue is this one's alone */
        match unsafe { fs::kernel_image_start(self.image.handle, k, i32::from(write), offset, len) } {
            ffi::block::QUEUE_IN_FLIGHT => Ok(Started::InFlight),
            rc => {
                self.buffers.untake(k);
                if rc == ffi::block::QUEUE_DONE { Ok(Started::Done) } else { Err(Error::IoError) }
            }
        }
    }

    /// The doorbell for the requests started.
    pub fn kick(&self) {
        fs::kernel_image_kick(self.image.handle);
    }

    /// Each request done, as `done` is told -- its buffer, nobody's again,
    /// and whether it went well -- until there are no more: how many.
    pub fn reap(&self, mut done: impl FnMut(usize, bool)) -> usize {
        const BATCH: usize = 32;
        let mut total = 0;
        loop {
            let mut out = [fs::ImageDone::default(); BATCH];
            /* `out` takes BATCH of them, and the handle is looked up. */
            let n = unsafe { fs::kernel_image_reap(self.image.handle, out.as_mut_ptr(), BATCH) };
            let Ok(n) = usize::try_from(n) else { return total };
            for d in &out[..n.min(BATCH)] {
                let k = d.buffer as usize;
                /* The image gives back only what was started: anything else
                 * is a broken promise of the kernel's, which no request of
                 * the caller's may be taken for. */
                assert!(self.buffers.give_back(k), "an image gave back buffer {}, which it did not have", k);
                done(k, d.ok != 0);
            }
            total += n;
            if n < BATCH {
                return total;
            }
        }
    }

    /// Sleeps until one of the image's commands is done, or a `wake`, or
    /// `timeout`: whether one came.
    pub fn wait(&self, timeout: crate::time::Duration) -> bool {
        fs::kernel_image_wait(self.image.handle, timeout.as_nanos()) == 1
    }

    /// What `wait` sleeps for, looked for for up to `poll` with this CPU
    /// spinning: whether it came. Before a `wait` that is likely to be short
    /// -- a command out on a fast disk, a request expected -- so as not to
    /// pay a sleep and a wake for it.
    pub fn poll(&self, poll: crate::time::Duration) -> bool {
        fs::kernel_image_poll(self.image.handle, poll.as_nanos()) == 1
    }

    /// Wakes whoever waits (`wait`), from anywhere.
    pub fn wake(&self) {
        fs::kernel_image_wake(self.image.handle);
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
/// one at the path already -- also when `read` could not see it; IoError,
/// and no file left, for anything else that refused it. For a file that
/// must never be written over by mistake.
pub fn create(path: &str, data: &[u8]) -> Result<()> {
    let rc = unsafe { fs::kernel_file_create(path.as_ptr(), path.len(), data.as_ptr(), data.len()) };
    match rc {
        fs::FILE_CREATED => Ok(()),
        fs::FILE_THERE => Err(Error::Busy),
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
/* What `kernel_dir_entry` answers (layers/fs/src/files.rs) */
use ffi::fs::{FILE_ENTRY_DIR as ENTRY_DIR, FILE_ENTRY_END as ENTRY_END};

/// An entry of a directory, as `list` reads it: its name as the filesystem
/// has it -- bytes, not necessarily UTF-8 -- whether it is a directory, and a
/// file's size.
pub struct Entry {
    pub name: Vec<u8>,
    pub dir: bool,
    pub size: u64,
}

/// What the directory at `path` has, an entry each: NotFound when the path
/// names no directory, InvalidValue when it has more than `LIST_MAX` --
/// which once came back as the first `LIST_MAX`, Ok. Read an entry at a time
/// with nothing held between them, so a directory changing meanwhile may
/// show an entry twice or not at all; and each entry is a walk of the
/// directory to its place, so a list costs its length squared: what
/// `LIST_MAX` bounds.
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
    if entries.len() == LIST_MAX {
        /* One more look says whether that was all */
        let (mut kind, mut size) = (0u32, 0u64);
        /* `name` is writable for its length, `kind` and `size` are locals. */
        let more = unsafe {
            fs::kernel_dir_entry(path.as_ptr(), path.len(), LIST_MAX, name.as_mut_ptr(), name.len(), &mut kind,
                                 &mut size)
        };
        if more != ENTRY_END {
            return Err(Error::InvalidValue);
        }
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

/// What a filesystem has room for, in bytes: what a file's data may take on
/// it, and how much of that no file has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// The space of the filesystem `path` is on, whether the path is there yet
/// or not: NotFound when no mount holds it, or its filesystem does not say.
pub fn space(path: &str) -> Result<Space> {
    let mut out = [0u64; 2];
    /* `path` is readable for its length, and `out` takes two u64s. */
    let rc = unsafe { fs::kernel_fs_space(path.as_ptr(), path.len(), out.as_mut_ptr()) };
    if rc == 0 { Ok(Space { total_bytes: out[0], free_bytes: out[1] }) } else { Err(Error::NotFound) }
}

/// Removes the file: NotFound when there was nothing to remove, or it could
/// not be.
pub fn remove(path: &str) -> Result<()> {
    let rc = unsafe { fs::kernel_file_remove(path.as_ptr(), path.len()) };
    if rc == 0 { Ok(()) } else { Err(Error::NotFound) }
}
