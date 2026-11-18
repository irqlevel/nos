#![no_std]
#![forbid(unsafe_code)]

//! An SFTP server: the protocol, and nothing of the machine it runs on.
//!
//! SFTP is what OpenSSH's `sftp`, and since OpenSSH 9.0 its `scp`, speak to
//! copy files over an SSH session: a subsystem of the session's channel, its
//! messages SSH's data types, each request answered by its id. This is
//! version 3 (draft-ietf-secsh-filexfer-02), the one OpenSSH speaks -- the
//! later drafts were never taken up -- with four of OpenSSH's extensions:
//! `limits@openssh.com` (the longest message and the most a read or a
//! write moves, which a client sizes its requests by), `statvfs@openssh.com`
//! (`df`), `fsync@openssh.com` (`put -f`) and `expand-path@openssh.com`
//! (`~`).
//!
//! The files are the machine's, through the `Files` trait: the sshd module
//! (modules/net/sshd) gives them as `kcore::fs` has them, the net fuzzer a
//! model it holds the answers to. What a filesystem there keeps -- names and
//! sizes, files and directories -- is what a client is told; it keeps no
//! owners, no permissions and no times, so every file is said to be
//! `-rw-r--r--` and every directory `drwxr-xr-x`, a client's change of any
//! of those is refused with `SSH_FX_OP_UNSUPPORTED` rather than taken and
//! dropped, and there are no symbolic links. Paths are the client's, made
//! absolute against `/`, its home, with `.` and `..` taken out; a remove
//! takes one file, and an rmdir one empty directory, never a tree.
//!
//! A server is fed the channel's bytes (`Server::input`) and answers a
//! whole request at a time, as `ssh::Subsystem` has it: what one request
//! answers is bounded (`MAX_DATA` and a little), and so is everything it
//! keeps -- `MAX_HANDLES` open at once, a directory read a few entries at a
//! time with nothing of it held between READDIRs.

extern crate alloc;

mod path;
mod server;

pub use server::{Server, Stats, MAX_DATA, MAX_HANDLES, MAX_PACKET, VERSION, WINDOW};

/// What is at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

/// What `Files::stat` says of a path: what is there, and a file's size --
/// 0 for a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
}

/// An entry of a directory: its name as the filesystem has it -- bytes, not
/// necessarily UTF-8 -- what it is, and a file's size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: alloc::vec::Vec<u8>,
    pub kind: Kind,
    pub size: u64,
}

/// What a filesystem has room for, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

/// How `Files::open` opens a regular file, which it reads in any case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Open {
    /// Written too.
    pub write: bool,
    /// Made, empty, when there is none.
    pub create: bool,
    /// Cut to nothing.
    pub truncate: bool,
    /// Made by this open, or refused when anything is at the path already.
    pub exclusive: bool,
}

/// Why a call of `Files` was not done, as far as its caller can tell: the
/// server asks `stat` for the rest of the story when it tells the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Nothing there.
    NotFound,
    /// No memory for it.
    NoMemory,
    /// Refused or failed, for whatever reason: an I/O error, a full disk, a
    /// read-only mount, something open, the wrong kind of thing there.
    Failed,
}

/// The files a server serves, every path absolute -- `/` and below, with no
/// `.`, `..` or empty name in it, no name longer than `NAME_MAX` and no
/// path longer than `PATH_MAX` -- and exactly the one named: nothing else
/// is looked for at it, and nothing else is taken away with it.
pub trait Files {
    /// A regular file held open, its reads and writes at the offsets the
    /// client says; closed when it is dropped.
    type File;
    /// The longest name a directory's entry may have, in bytes.
    const NAME_MAX: usize;
    /// The longest path, in bytes.
    const PATH_MAX: usize;

    /// What is at `path`: NotFound when nothing is.
    fn stat(&mut self, path: &str) -> Result<Stat, Error>;
    /// The regular file at `path`, opened as `how` says: NotFound when
    /// there is none and none is to be made, Failed for the rest -- a
    /// directory there, a file there for `exclusive`, nowhere to make it.
    fn open(&mut self, path: &str, how: Open) -> Result<Self::File, Error>;
    /// As much of `buf` as the file has from `offset`: the count, 0 at or
    /// past its end.
    fn read(&mut self, file: &Self::File, offset: u64, buf: &mut [u8]) -> Result<usize, Error>;
    /// `data` into the file, open for writing, at `offset` -- the file grown
    /// to reach past it where it must, with a hole between its end and
    /// `offset`.
    fn write(&mut self, file: &Self::File, offset: u64, data: &[u8]) -> Result<(), Error>;
    /// The file's size.
    fn size(&mut self, file: &Self::File) -> Result<u64, Error>;
    /// The file, open for writing, cut or grown to `size` bytes.
    fn set_len(&mut self, file: &Self::File, size: u64) -> Result<(), Error>;
    /// What has been written to the file on its disk's medium.
    fn sync(&mut self, file: &Self::File) -> Result<(), Error>;
    /// The `index`'th entry of the directory at `dir`: None past the last,
    /// NotFound when `dir` is no directory. Nothing is held between two
    /// calls, so a directory that changes meanwhile may show an entry twice,
    /// or not at all.
    fn entry(&mut self, dir: &str, index: usize) -> Result<Option<Entry>, Error>;
    /// A directory made at `path`, where nothing is -- refused when
    /// anything is there, a directory too.
    fn make_dir(&mut self, path: &str) -> Result<(), Error>;
    /// The regular file at `path` removed -- never a directory.
    fn remove_file(&mut self, path: &str) -> Result<(), Error>;
    /// The empty directory at `path` removed -- never one with anything in
    /// it, nor a file.
    fn remove_dir(&mut self, path: &str) -> Result<(), Error>;
    /// What is at `from` renamed `to`, where nothing is.
    fn rename(&mut self, from: &str, to: &str) -> Result<(), Error>;
    /// The space of the filesystem `path` is on.
    fn space(&mut self, path: &str) -> Result<Space, Error>;
}
