//! The server: a request in, its answer out.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use ssh::wire::{put_string, put_u32, put_u64, Reader};

use crate::path;
use crate::{Entry, Error, Files, Kind, Open, Stat};

/// The version served: draft-ietf-secsh-filexfer-02's, OpenSSH's.
pub const VERSION: u32 = 3;
/// The longest message taken, as its length says it: what every request
/// OpenSSH sends a server that gives these limits fits in, and more than
/// the 34000 bytes the draft has every server take. A longer one ends the
/// session: what follows it cannot be found.
pub const MAX_PACKET: usize = 64 * 1024;
/// The most a READ answers and a WRITE carries: a message's room less 1 KiB
/// for the rest of it, as OpenSSH reckons it.
pub const MAX_DATA: usize = MAX_PACKET - 1024;
/// Handles open at once, files and directories: what `limits@openssh.com`
/// says, which OpenSSH's client also keeps its requests in flight to.
pub const MAX_HANDLES: usize = 32;
/// The channel's window a server is to be given (`ssh::Subsystem::window`):
/// four messages, so that a client has a few requests on their way while
/// one is answered -- and a whole one always fits beside what waits.
pub const WINDOW: u32 = 4 * MAX_PACKET as u32;

/* What a message is (draft-02 3) */
const FXP_INIT: u8 = 1;
const FXP_VERSION: u8 = 2;
const FXP_OPEN: u8 = 3;
const FXP_CLOSE: u8 = 4;
const FXP_READ: u8 = 5;
const FXP_WRITE: u8 = 6;
const FXP_LSTAT: u8 = 7;
const FXP_FSTAT: u8 = 8;
const FXP_SETSTAT: u8 = 9;
const FXP_FSETSTAT: u8 = 10;
const FXP_OPENDIR: u8 = 11;
const FXP_READDIR: u8 = 12;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_REALPATH: u8 = 16;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_READLINK: u8 = 19;
const FXP_SYMLINK: u8 = 20;
const FXP_STATUS: u8 = 101;
const FXP_HANDLE: u8 = 102;
const FXP_DATA: u8 = 103;
const FXP_NAME: u8 = 104;
const FXP_ATTRS: u8 = 105;
const FXP_EXTENDED: u8 = 200;
const FXP_EXTENDED_REPLY: u8 = 201;

/* How an OPEN opens (6.3); the file is read in any case */
const FXF_WRITE: u32 = 0x02;
const FXF_APPEND: u32 = 0x04;
const FXF_CREAT: u32 = 0x08;
const FXF_TRUNC: u32 = 0x10;
const FXF_EXCL: u32 = 0x20;

/* What an ATTRS carries (5) */
const ATTR_SIZE: u32 = 0x01;
const ATTR_UIDGID: u32 = 0x02;
const ATTR_PERMISSIONS: u32 = 0x04;
const ATTR_ACMODTIME: u32 = 0x08;
const ATTR_EXTENDED: u32 = 0x8000_0000;

/* A STATUS's code (7) */
const FX_OK: u32 = 0;
const FX_EOF: u32 = 1;
const FX_NO_SUCH_FILE: u32 = 2;
const FX_FAILURE: u32 = 4;
const FX_BAD_MESSAGE: u32 = 5;
const FX_OP_UNSUPPORTED: u32 = 8;

/* A file's and a directory's mode, as stat(2) has them -- the type bits a
   client tells a directory from a file by, and permissions nos does not
   keep: what a file of anyone's is, and a directory */
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const DIR_MODE: u32 = S_IFDIR | 0o755;
const FILE_MODE: u32 = S_IFREG | 0o644;

/// Entries one READDIR answers at most, as OpenSSH's server does -- fewer
/// once the answer passes `MAX_DATA`.
const DIR_BATCH: usize = 100;
/// What statvfs@openssh.com counts a filesystem's bytes in.
const SPACE_BLOCK: u64 = 4096;
/// A handle, as the client is given it: the slot's index and generation.
const HANDLE_LEN: usize = 8;

/// The extensions served, each with its version, as OpenSSH names them.
const EXT_LIMITS: &[u8] = b"limits@openssh.com";
const EXT_STATVFS: &[u8] = b"statvfs@openssh.com";
const EXT_FSYNC: &[u8] = b"fsync@openssh.com";
const EXT_EXPAND_PATH: &[u8] = b"expand-path@openssh.com";
const EXTENSIONS: [(&[u8], &[u8]); 4] =
    [(EXT_LIMITS, b"1"), (EXT_STATVFS, b"2"), (EXT_FSYNC, b"1"), (EXT_EXPAND_PATH, b"1")];

/// A request not done: the STATUS it is answered with, and why.
struct Status(u32, &'static str);

const BAD: Status = Status(FX_BAD_MESSAGE, "a malformed request");
const NO_HANDLE: Status = Status(FX_FAILURE, "no such handle");
const NO_MEMORY: Status = Status(FX_FAILURE, "no memory for it");
const NO_FILE: Status = Status(FX_NO_SUCH_FILE, "no such file or directory");
const NO_ANSWER: &str = "no memory for an SFTP answer";

/// A request's answer, made in the output -- or the STATUS it is to be
/// answered with instead.
type Reply = Result<(), Status>;

/// What a handle holds.
enum Held<T> {
    File { file: T, write: bool, append: bool },
    /// A directory, and the index of its next entry.
    Dir { path: String, next: usize },
}

struct Slot<T> {
    /// Counted on as each handle the slot gives out is closed, so that one
    /// closed never names what the slot holds next.
    generation: u32,
    held: Option<Held<T>>,
}

/// What a server has done: what its session's last line says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Requests answered, and of them those refused or failed -- an end of
    /// a file or of a directory, which every read comes to, not counted.
    pub requests: u64,
    pub refused: u64,
    /// Files opened, and opened to be written.
    pub opened: u64,
    pub opened_to_write: u64,
    /// Bytes read out of files, and written into them.
    pub read: u64,
    pub written: u64,
}

/// An SFTP server over the files `F` serves.
pub struct Server<F: Files> {
    files: F,
    /// The client's INIT has come, and been answered.
    started: bool,
    slots: Vec<Slot<F::File>>,
    stats: Stats,
}

impl<F: Files> Server<F> {
    pub fn new(files: F) -> Self {
        Self { files, started: false, slots: Vec::new(), stats: Stats::default() }
    }

    /// What it has done so far.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The files it serves -- for a fuzzer's model to be looked at.
    pub fn files(&mut self) -> &mut F {
        &mut self.files
    }

    /// Takes the first whole message at the front of `input` and appends its
    /// answer to `out`: how many bytes of `input` it took -- 0 while what is
    /// there is no whole message. Err for a stream there is no reading on
    /// from: a message of no length, or longer than `MAX_PACKET`; anything
    /// but an INIT first, or an INIT again; or no memory for an answer.
    pub fn input(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, &'static str> {
        let Some(head) = input.get(..4) else { return Ok(0) };
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if len == 0 {
            return Err("an SFTP message of no length");
        }
        if len > MAX_PACKET {
            return Err("an SFTP message longer than the server takes");
        }
        let Some(message) = input.get(4..4 + len) else { return Ok(0) };
        self.message(message, out)?;
        Ok(4 + len)
    }

    fn message(&mut self, message: &[u8], out: &mut Vec<u8>) -> Result<(), &'static str> {
        let kind = message[0];
        let mut r = Reader::new(&message[1..]);
        if !self.started {
            if kind != FXP_INIT {
                return Err("an SFTP request before the INIT");
            }
            /* The client's version, and whatever extensions it names after
               it: version 3 is the answer whatever it is */
            r.u32().ok_or("a malformed SFTP INIT")?;
            self.started = true;
            return version(out);
        }
        if kind == FXP_INIT {
            return Err("a second SFTP INIT");
        }
        /* Every request has its id first; one too short to have it is
           answered as id 0, whose client has no other to match it to */
        let Some(id) = r.u32() else { return self.answer(out, 0, Err(BAD)) };
        let start = out.len();
        let reply = self.request(kind, id, &mut r, out);
        if reply.is_err() {
            out.truncate(start);
        }
        self.answer(out, id, reply)
    }

    /// A request answered: as its handler did, or with the STATUS it says.
    fn answer(&mut self, out: &mut Vec<u8>, id: u32, reply: Reply) -> Result<(), &'static str> {
        self.stats.requests += 1;
        match reply {
            Ok(()) => Ok(()),
            Err(Status(code, why)) => {
                if code != FX_EOF {
                    self.stats.refused += 1;
                }
                status(out, id, code, why)
            }
        }
    }

    fn request(&mut self, kind: u8, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        match kind {
            FXP_OPEN => self.open(id, r, out),
            FXP_CLOSE => self.close(id, r, out),
            FXP_READ => self.read(id, r, out),
            FXP_WRITE => self.write(id, r, out),
            FXP_LSTAT | FXP_STAT => self.stat(id, r, out),
            FXP_FSTAT => self.fstat(id, r, out),
            FXP_SETSTAT => self.setstat(id, r, out),
            FXP_FSETSTAT => self.fsetstat(id, r, out),
            FXP_OPENDIR => self.opendir(id, r, out),
            FXP_READDIR => self.readdir(id, r, out),
            FXP_REMOVE => self.remove(id, r, out),
            FXP_MKDIR => self.mkdir(id, r, out),
            FXP_RMDIR => self.rmdir(id, r, out),
            FXP_REALPATH => realpath(id, r, out, path::normalize),
            FXP_RENAME => self.rename(id, r, out),
            FXP_READLINK => self.readlink(id, r),
            FXP_SYMLINK => Err(Status(FX_OP_UNSUPPORTED, "nos has no symbolic links")),
            FXP_EXTENDED => self.extended(id, r, out),
            _ => Err(Status(FX_OP_UNSUPPORTED, "a request this server does not have")),
        }
    }

    /* ---- paths and handles ---- */

    /// The path a request names, made one the files take: absolute, its
    /// `.` and `..` taken out. Refused -- as nothing there (`makes` false:
    /// nothing can be at a path the files cannot take), or as a failure for
    /// a request that would make something there -- when it is not UTF-8,
    /// has a NUL in it, or has a name or a length past the files' own.
    fn path(&self, raw: &[u8], makes: bool) -> Result<String, Status> {
        let refuse = |why| Status(if makes { FX_FAILURE } else { FX_NO_SUCH_FILE }, why);
        let text = core::str::from_utf8(raw).map_err(|_| refuse("a path that is not UTF-8"))?;
        if text.contains('\0') {
            return Err(refuse("a path with a NUL in it"));
        }
        let p = path::normalize(text).ok_or(NO_MEMORY)?;
        if p.len() > F::PATH_MAX {
            return Err(refuse("a path longer than the filesystem takes"));
        }
        if p.split('/').any(|name| name.len() > F::NAME_MAX) {
            return Err(refuse("a name longer than the filesystem keeps"));
        }
        Ok(p)
    }

    /// The slot a request's handle names: one open, of the generation it
    /// was given out in.
    fn handle(&self, r: &mut Reader) -> Result<usize, Status> {
        let raw = r.string().ok_or(BAD)?;
        let raw: [u8; HANDLE_LEN] = raw.try_into().map_err(|_| NO_HANDLE)?;
        let index = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let generation = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
        match self.slots.get(index) {
            Some(slot) if slot.generation == generation && slot.held.is_some() => Ok(index),
            _ => Err(NO_HANDLE),
        }
    }

    /// A slot for a new handle: one closed before, or a new one while there
    /// are fewer than `MAX_HANDLES` -- found before anything is opened, so
    /// that nothing is opened for want of one.
    fn free_slot(&mut self) -> Result<usize, Status> {
        if let Some(index) = self.slots.iter().position(|s| s.held.is_none()) {
            return Ok(index);
        }
        if self.slots.len() >= MAX_HANDLES {
            return Err(Status(FX_FAILURE, "too many handles open"));
        }
        self.slots.try_reserve(1).map_err(|_| NO_MEMORY)?;
        self.slots.push(Slot { generation: 0, held: None });
        Ok(self.slots.len() - 1)
    }

    /// `held` put in slot `index`, and its handle the answer -- or, with no
    /// memory for the answer, let go of again.
    fn hand_out(&mut self, id: u32, out: &mut Vec<u8>, index: usize, held: Held<F::File>) -> Reply {
        let generation = self.slots[index].generation;
        let at = begin(out, FXP_HANDLE, id, 4 + HANDLE_LEN)?;
        put_u32(out, HANDLE_LEN as u32);
        put_u32(out, index as u32);
        put_u32(out, generation);
        end(out, at);
        self.slots[index].held = Some(held);
        Ok(())
    }

    /* ---- the requests ---- */

    fn open(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let name = r.string().ok_or(BAD)?;
        let pflags = r.u32().ok_or(BAD)?;
        /* A file's owner, mode and times, which nos does not keep: read
           past, and nothing made of them */
        attrs(r).ok_or(BAD)?;
        let create = pflags & FXF_CREAT != 0;
        let how = Open {
            write: pflags & (FXF_WRITE | FXF_APPEND) != 0,
            create,
            truncate: pflags & FXF_TRUNC != 0,
            exclusive: create && pflags & FXF_EXCL != 0,
        };
        let path = self.path(name, create)?;
        let index = self.free_slot()?;
        let file = match self.files.open(&path, how) {
            Ok(file) => file,
            Err(e) => return Err(self.why_not_opened(&path, how, e)),
        };
        self.stats.opened += 1;
        if how.write {
            self.stats.opened_to_write += 1;
        }
        self.hand_out(id, out, index, Held::File { file, write: how.write, append: pflags & FXF_APPEND != 0 })
    }

    fn why_not_opened(&mut self, path: &str, how: Open, e: Error) -> Status {
        if e == Error::NoMemory {
            return NO_MEMORY;
        }
        match self.files.stat(path) {
            Ok(Stat { kind: Kind::Dir, .. }) => Status(FX_FAILURE, "a directory"),
            Ok(_) if how.exclusive => Status(FX_FAILURE, "there is a file there already"),
            Ok(_) => Status(FX_FAILURE, "the file could not be opened: a disk's image held, or a read-only filesystem"),
            Err(_) if !how.create => NO_FILE,
            Err(_) => self.why_not_made(path),
        }
    }

    /// Why nothing could be made at `path`, where nothing is: no directory
    /// to make it in, or the filesystem refused.
    fn why_not_made(&mut self, path: &str) -> Status {
        match self.files.stat(parent(path)) {
            Ok(Stat { kind: Kind::Dir, .. }) => {
                Status(FX_FAILURE, "it could not be made: a read-only filesystem, or no room")
            }
            _ => Status(FX_NO_SUCH_FILE, "no such directory to make it in"),
        }
    }

    fn close(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let index = self.handle(r)?;
        let slot = &mut self.slots[index];
        /* A file is closed as it is dropped */
        slot.held = None;
        slot.generation = slot.generation.wrapping_add(1);
        ok(out, id)
    }

    fn read(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let index = self.handle(r)?;
        let offset = r.u64().ok_or(BAD)?;
        let len = core::cmp::min(r.u32().ok_or(BAD)? as usize, MAX_DATA);
        let Some(Held::File { file, .. }) = self.slots[index].held.as_ref() else {
            return Err(Status(FX_FAILURE, "a directory's handle: READDIR reads those"));
        };
        let at = begin(out, FXP_DATA, id, 4 + len)?;
        let data_at = out.len() + 4;
        put_u32(out, 0);
        out.resize(data_at + len, 0);
        let got = self.files.read(file, offset, &mut out[data_at..]).map_err(|e| failed(e, "the file could not be read"))?;
        if got > len {
            return Err(Status(FX_FAILURE, "the file could not be read"));
        }
        /* A read of nothing is answered as it was asked; one of something
           that found nothing is at the end */
        if got == 0 && len != 0 {
            return Err(Status(FX_EOF, "end of file"));
        }
        out.truncate(data_at + got);
        out[data_at - 4..data_at].copy_from_slice(&(got as u32).to_be_bytes());
        end(out, at);
        self.stats.read += got as u64;
        Ok(())
    }

    fn write(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let index = self.handle(r)?;
        let offset = r.u64().ok_or(BAD)?;
        let data = r.string().ok_or(BAD)?;
        let Some(Held::File { file, write, append }) = self.slots[index].held.as_ref() else {
            return Err(Status(FX_FAILURE, "a directory's handle"));
        };
        if !*write {
            return Err(Status(FX_FAILURE, "the file was not opened to be written"));
        }
        /* Opened to append: every write at the end, wherever it says */
        let offset = if *append {
            self.files.size(file).map_err(|e| failed(e, "the file could not be written"))?
        } else {
            offset
        };
        self.files.write(file, offset, data).map_err(|e| failed(e, "the file could not be written: the disk may be full"))?;
        self.stats.written += data.len() as u64;
        ok(out, id)
    }

    fn stat(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let raw = r.string().ok_or(BAD)?;
        let path = self.path(raw, false)?;
        let st = self.files.stat(&path).map_err(missing)?;
        attrs_answer(out, id, &st)
    }

    fn fstat(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let index = self.handle(r)?;
        let st = match self.slots[index].held.as_ref() {
            Some(Held::File { file, .. }) => {
                let size = self.files.size(file).map_err(|e| failed(e, "the file could not be looked at"))?;
                Stat { kind: Kind::File, size }
            }
            Some(Held::Dir { path, .. }) => self.files.stat(path).map_err(missing)?,
            None => return Err(NO_HANDLE),
        };
        attrs_answer(out, id, &st)
    }

    fn setstat(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let raw = r.string().ok_or(BAD)?;
        let size = settable(r)?;
        let path = self.path(raw, false)?;
        let kind = self.files.stat(&path).map_err(missing)?.kind;
        let Some(size) = size else { return ok(out, id) };
        if kind == Kind::Dir {
            return Err(Status(FX_FAILURE, "a directory has no size to set"));
        }
        let file = self
            .files
            .open(&path, Open { write: true, ..Open::default() })
            .map_err(|e| failed(e, "the file could not be opened to be cut or grown"))?;
        self.files.set_len(&file, size).map_err(|e| failed(e, "the file could not be cut or grown"))?;
        drop(file);
        ok(out, id)
    }

    fn fsetstat(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let index = self.handle(r)?;
        let size = settable(r)?;
        match (self.slots[index].held.as_ref(), size) {
            (_, None) => {}
            (Some(Held::File { file, write: true, .. }), Some(size)) => {
                self.files.set_len(file, size).map_err(|e| failed(e, "the file could not be cut or grown"))?;
            }
            (Some(Held::File { .. }), Some(_)) => {
                return Err(Status(FX_FAILURE, "the file was not opened to be written"));
            }
            (_, Some(_)) => return Err(Status(FX_FAILURE, "a directory has no size to set")),
        }
        ok(out, id)
    }

    fn opendir(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let raw = r.string().ok_or(BAD)?;
        let path = self.path(raw, false)?;
        if self.files.stat(&path).map_err(missing)?.kind != Kind::Dir {
            return Err(Status(FX_FAILURE, "not a directory"));
        }
        let index = self.free_slot()?;
        self.hand_out(id, out, index, Held::Dir { path, next: 0 })
    }

    fn readdir(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let index = self.handle(r)?;
        let Some(Held::Dir { path, next }) = self.slots[index].held.as_mut() else {
            return Err(Status(FX_FAILURE, "a file's handle: READ reads those"));
        };
        let at = begin(out, FXP_NAME, id, 4)?;
        let count_at = out.len();
        put_u32(out, 0);
        let mut count = 0u32;
        while (count as usize) < DIR_BATCH && out.len() - at < MAX_DATA {
            let entry = match self.files.entry(path, *next) {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => return Err(failed(e, "the directory could not be read")),
            };
            *next += 1;
            name_entry(out, &entry)?;
            count += 1;
        }
        if count == 0 {
            return Err(Status(FX_EOF, "no more entries"));
        }
        out[count_at..count_at + 4].copy_from_slice(&count.to_be_bytes());
        end(out, at);
        Ok(())
    }

    fn remove(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let raw = r.string().ok_or(BAD)?;
        let path = self.path(raw, false)?;
        match self.files.remove_file(&path) {
            Ok(()) => ok(out, id),
            Err(Error::NoMemory) => Err(NO_MEMORY),
            Err(_) => Err(match self.files.stat(&path) {
                Err(_) => NO_FILE,
                Ok(Stat { kind: Kind::Dir, .. }) => Status(FX_FAILURE, "a directory: RMDIR removes those"),
                Ok(_) => Status(FX_FAILURE, "the file could not be removed: open, or on a read-only filesystem"),
            }),
        }
    }

    fn mkdir(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let raw = r.string().ok_or(BAD)?;
        /* A mode, which nos does not keep: read past */
        attrs(r).ok_or(BAD)?;
        let path = self.path(raw, true)?;
        /* Made where nothing is, or refused -- the files' own word, never a
           look of this server's before it, which a failed look would take
           for nothing there */
        match self.files.make_dir(&path) {
            Ok(()) => ok(out, id),
            Err(Error::NoMemory) => Err(NO_MEMORY),
            Err(_) if self.files.stat(&path).is_ok() => Err(Status(FX_FAILURE, "there is something there already")),
            Err(_) => Err(self.why_not_made(&path)),
        }
    }

    fn rmdir(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let raw = r.string().ok_or(BAD)?;
        let path = self.path(raw, false)?;
        match self.files.remove_dir(&path) {
            Ok(()) => ok(out, id),
            Err(Error::NoMemory) => Err(NO_MEMORY),
            Err(_) => Err(match self.files.stat(&path) {
                Err(_) => NO_FILE,
                Ok(Stat { kind: Kind::File, .. }) => Status(FX_FAILURE, "not a directory: REMOVE removes files"),
                Ok(_) => Status(FX_FAILURE, "the directory could not be removed: not empty, the root, or something in it open"),
            }),
        }
    }

    fn rename(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let from = r.string().ok_or(BAD)?;
        let to = r.string().ok_or(BAD)?;
        let from = self.path(from, false)?;
        let to = self.path(to, true)?;
        match self.files.rename(&from, &to) {
            Ok(()) => ok(out, id),
            Err(Error::NoMemory) => Err(NO_MEMORY),
            Err(_) => Err(if self.files.stat(&from).is_err() {
                NO_FILE
            } else if self.files.stat(&to).is_ok() {
                Status(FX_FAILURE, "the new name is taken")
            } else {
                Status(FX_FAILURE, "it could not be renamed: to another filesystem, into itself, or something under it open")
            }),
        }
    }

    fn readlink(&mut self, _id: u32, r: &mut Reader) -> Reply {
        let raw = r.string().ok_or(BAD)?;
        let path = self.path(raw, false)?;
        self.files.stat(&path).map_err(missing)?;
        Err(Status(FX_FAILURE, "not a symbolic link: nos has none"))
    }

    fn extended(&mut self, id: u32, r: &mut Reader, out: &mut Vec<u8>) -> Reply {
        let name = r.string().ok_or(BAD)?;
        match name {
            EXT_LIMITS => {
                let at = begin(out, FXP_EXTENDED_REPLY, id, 32)?;
                for limit in [MAX_PACKET, MAX_DATA, MAX_DATA, MAX_HANDLES] {
                    put_u64(out, limit as u64);
                }
                end(out, at);
                Ok(())
            }
            EXT_STATVFS => {
                let raw = r.string().ok_or(BAD)?;
                let path = self.path(raw, false)?;
                let space = self.files.space(&path).map_err(missing)?;
                /* f_bsize, f_frsize, f_blocks, f_bfree, f_bavail, f_files,
                   f_ffree, f_favail, f_fsid, f_flag, f_namemax: no inodes
                   counted, no id, no flags */
                let blocks = [SPACE_BLOCK, SPACE_BLOCK, space.total / SPACE_BLOCK, space.free / SPACE_BLOCK,
                              space.free / SPACE_BLOCK, 0, 0, 0, 0, 0, F::NAME_MAX as u64];
                let at = begin(out, FXP_EXTENDED_REPLY, id, 8 * blocks.len())?;
                for v in blocks {
                    put_u64(out, v);
                }
                end(out, at);
                Ok(())
            }
            EXT_FSYNC => {
                let index = self.handle(r)?;
                let Some(Held::File { file, .. }) = self.slots[index].held.as_ref() else {
                    return Err(Status(FX_FAILURE, "a directory's handle"));
                };
                self.files.sync(file).map_err(|e| failed(e, "the file could not be synced"))?;
                ok(out, id)
            }
            EXT_EXPAND_PATH => realpath(id, r, out, path::expand),
            _ => Err(Status(FX_OP_UNSUPPORTED, "an extension this server does not have")),
        }
    }
}

impl<F: Files> ssh::Subsystem for Server<F> {
    fn input(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, &'static str> {
        Server::input(self, input, out)
    }

    fn window(&self) -> u32 {
        WINDOW
    }
}

/* ---- what the requests are answered with ---- */

/// The server's VERSION: 3, and the extensions it has.
fn version(out: &mut Vec<u8>) -> Result<(), &'static str> {
    let room: usize = 4 + EXTENSIONS.iter().map(|(name, data)| 8 + name.len() + data.len()).sum::<usize>();
    out.try_reserve(5 + room).map_err(|_| NO_ANSWER)?;
    let at = out.len();
    put_u32(out, 0);
    out.push(FXP_VERSION);
    put_u32(out, VERSION);
    for (name, data) in EXTENSIONS {
        put_string(out, name);
        put_string(out, data);
    }
    end(out, at);
    Ok(())
}

/// The path, made absolute as `how` does -- nothing looked up -- as the one
/// name a NAME answers, with no attributes: REALPATH, and expand-path.
fn realpath(id: u32, r: &mut Reader, out: &mut Vec<u8>, how: fn(&str) -> Option<String>) -> Reply {
    let raw = r.string().ok_or(BAD)?;
    let text = core::str::from_utf8(raw).map_err(|_| Status(FX_NO_SUCH_FILE, "a path that is not UTF-8"))?;
    let p = how(text).ok_or(NO_MEMORY)?;
    let at = begin(out, FXP_NAME, id, 16 + 2 * p.len())?;
    put_u32(out, 1);
    put_string(out, p.as_bytes());
    put_string(out, p.as_bytes());
    put_u32(out, 0);
    end(out, at);
    Ok(())
}

/// A message of `kind` for request `id` begun in `out`, with room for `room`
/// bytes after the id: where it starts, for `end` to give it its length.
fn begin(out: &mut Vec<u8>, kind: u8, id: u32, room: usize) -> Result<usize, Status> {
    out.try_reserve(9 + room).map_err(|_| NO_MEMORY)?;
    let at = out.len();
    put_u32(out, 0);
    out.push(kind);
    put_u32(out, id);
    Ok(at)
}

/// The message begun at `at`, given its length.
fn end(out: &mut [u8], at: usize) {
    let len = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&len.to_be_bytes());
}

fn status(out: &mut Vec<u8>, id: u32, code: u32, why: &str) -> Result<(), &'static str> {
    let at = begin(out, FXP_STATUS, id, 12 + why.len()).map_err(|_| NO_ANSWER)?;
    put_u32(out, code);
    put_string(out, why.as_bytes());
    /* The language of `why`, which OpenSSH leaves empty too */
    put_string(out, b"");
    end(out, at);
    Ok(())
}

fn ok(out: &mut Vec<u8>, id: u32) -> Reply {
    status(out, id, FX_OK, "Success").map_err(|_| NO_MEMORY)
}

fn attrs_answer(out: &mut Vec<u8>, id: u32, st: &Stat) -> Reply {
    let at = begin(out, FXP_ATTRS, id, 16)?;
    put_attrs(out, st.kind, st.size);
    end(out, at);
    Ok(())
}

/// What the files keep of a file or a directory, as an ATTRS: its size and
/// its mode -- no owner and no times, which they do not keep.
fn put_attrs(out: &mut Vec<u8>, kind: Kind, size: u64) {
    put_u32(out, ATTR_SIZE | ATTR_PERMISSIONS);
    put_u64(out, size);
    put_u32(out, if kind == Kind::Dir { DIR_MODE } else { FILE_MODE });
}

/// A directory's entry, as a NAME carries it: its name, the line `ls -l`
/// would show of it -- which OpenSSH's `ls -l` shows as it is -- and its
/// attributes. No time is kept, so the time shown is none: the epoch.
fn name_entry(out: &mut Vec<u8>, entry: &Entry) -> Reply {
    /* The line's start is short and bounded: within this room nothing
       written below grows `out`, so nothing allocates */
    const LONG_HEAD_MAX: usize = 96;
    out.try_reserve(4 + entry.name.len() + 4 + LONG_HEAD_MAX + entry.name.len() + 16).map_err(|_| NO_MEMORY)?;
    put_string(out, &entry.name);
    let long_at = out.len();
    put_u32(out, 0);
    let (mode, links) = if entry.kind == Kind::Dir { ("drwxr-xr-x", 2) } else { ("-rw-r--r--", 1) };
    let _ = write!(Text(out), "{} {:>3} {:<8} {:<8} {:>8} Jan  1  1970 ", mode, links, "root", "root", entry.size);
    out.extend_from_slice(&entry.name);
    let long_len = (out.len() - long_at - 4) as u32;
    out[long_at..long_at + 4].copy_from_slice(&long_len.to_be_bytes());
    put_attrs(out, entry.kind, entry.size);
    Ok(())
}

/// A writer of formatted text onto the end of a message.
struct Text<'a>(&'a mut Vec<u8>);

impl Write for Text<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/* ---- what is read of the requests ---- */

/// What an ATTRS (draft-02 5) carries, read past: its flags, and its size
/// if it has one. None for one cut short.
struct Attrs {
    flags: u32,
    size: Option<u64>,
}

fn attrs(r: &mut Reader) -> Option<Attrs> {
    let flags = r.u32()?;
    let size = if flags & ATTR_SIZE != 0 { Some(r.u64()?) } else { None };
    if flags & ATTR_UIDGID != 0 {
        r.u32()?;
        r.u32()?;
    }
    if flags & ATTR_PERMISSIONS != 0 {
        r.u32()?;
    }
    if flags & ATTR_ACMODTIME != 0 {
        r.u32()?;
        r.u32()?;
    }
    if flags & ATTR_EXTENDED != 0 {
        /* Each pair takes eight bytes at least: a count past what the
           message holds runs out of it, a few thousand turns at most */
        let count = r.u32()?;
        for _ in 0..count {
            r.string()?;
            r.string()?;
        }
    }
    Some(Attrs { flags, size })
}

/// What a SETSTAT or FSETSTAT asks to set, as far as the files can: a size,
/// and nothing else -- an owner, a mode, a time or an extension asked for
/// refuses the whole request, nothing done, rather than being dropped.
fn settable(r: &mut Reader) -> Result<Option<u64>, Status> {
    let a = attrs(r).ok_or(BAD)?;
    if a.flags & !ATTR_SIZE != 0 {
        return Err(Status(FX_OP_UNSUPPORTED, "nos keeps no owners, permissions or times"));
    }
    Ok(a.size)
}

/// The directory `path`, absolute and made as `path::normalize` makes it,
/// is in.
fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(cut) => &path[..cut],
    }
}

/// A lookup's failure, as the client is told it.
fn missing(e: Error) -> Status {
    match e {
        Error::NotFound => NO_FILE,
        Error::NoMemory => NO_MEMORY,
        Error::Failed => Status(FX_FAILURE, "it could not be looked at"),
    }
}

/// A call's failure on what is there, as the client is told it.
fn failed(e: Error, why: &'static str) -> Status {
    match e {
        Error::NotFound => NO_FILE,
        Error::NoMemory => NO_MEMORY,
        Error::Failed => Status(FX_FAILURE, why),
    }
}
