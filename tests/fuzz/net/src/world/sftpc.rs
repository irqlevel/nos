//! The SFTP half of the world's SSH client (`sshc`): what it runs over the
//! session's `sftp` subsystem once the plan says so -- a directory made, a
//! file written into it a piece at a time, every piece sent at once, as fast
//! as the server's window lets them go, the file closed and looked at, read
//! back in pieces asked for all at once and one past its end, the directory
//! listed, the file renamed and removed, the directory removed: OpenSSH's
//! `put` and `get`, roughly, and what its `ls`, `rename`, `rm` and `rmdir`
//! do. Every answer is held to what it should be as it comes -- this
//! client's own reading of SFTP version 3 -- and the file read back has to be
//! the file written.

use std::collections::BTreeMap;

const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const STAT: u8 = 17;
const RENAME: u8 = 18;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;

const F_READ: u32 = 0x01;
const F_WRITE: u32 = 0x02;
const F_CREAT: u32 = 0x08;
const F_TRUNC: u32 = 0x10;
const FX_OK: u32 = 0;
const FX_EOF: u32 = 1;
const FILE_MODE: u32 = 0o100644;
/// The longest answer a server sends: a READ's of 63 KiB, and its head.
const ANSWER_MAX: usize = 64 * 1024 + 1024;

/// What the plan copies: into which directory, what, and in pieces of
/// how much.
#[derive(Clone, Debug)]
pub struct SftpPlan {
    pub dir: Vec<u8>,
    pub data: Vec<u8>,
    pub piece: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Version,
    Mkdir,
    OpenToWrite,
    Write,
    CloseWritten,
    Stat,
    OpenToRead,
    Read,
    CloseRead,
    OpenDir,
    ReadDir,
    CloseDir,
    Rename,
    Remove,
    Rmdir,
    Done,
}

/// What a request in flight is to be answered with.
#[derive(Debug)]
enum Want {
    Ok,
    Handle,
    /// A piece of the file: where, and how much was asked.
    Data(u64, usize),
    Eof,
    Attrs(u64),
    /// A READDIR's names, or its end.
    Names,
}

pub struct Script {
    plan: SftpPlan,
    stage: Stage,
    next_id: u32,
    waiting: BTreeMap<u32, Want>,
    handle: Vec<u8>,
    rx: Vec<u8>,
    /// The file as it came back, and the directory's names as listed.
    pub back: Vec<u8>,
    pub names: Vec<Vec<u8>>,
    /// Every step done, each answered as it should be.
    pub finished: bool,
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

fn frame(out: &mut Vec<u8>, body: &[u8]) {
    put_u32(out, body.len() as u32);
    out.extend_from_slice(body);
}

impl Script {
    /// The script, and what it sends first: the INIT.
    pub fn start(plan: SftpPlan) -> (Script, Vec<u8>) {
        let mut out = Vec::new();
        let mut init = vec![INIT];
        put_u32(&mut init, 3);
        frame(&mut out, &init);
        let s = Script { plan, stage: Stage::Version, next_id: 1, waiting: BTreeMap::new(), handle: Vec::new(),
                         rx: Vec::new(), back: Vec::new(), names: Vec::new(), finished: false };
        (s, out)
    }

    fn file(&self, name: &[u8]) -> Vec<u8> {
        let mut p = self.plan.dir.clone();
        p.push(b'/');
        p.extend_from_slice(name);
        p
    }

    /// A request of `kind` whose fields follow its id, framed onto `out`.
    fn send(&mut self, out: &mut Vec<u8>, kind: u8, want: Want, fields: &[u8]) {
        let id = self.next_id;
        self.next_id += 1;
        let mut body = vec![kind];
        put_u32(&mut body, id);
        body.extend_from_slice(fields);
        frame(out, &body);
        self.waiting.insert(id, want);
    }

    /// What the server sent on the channel: each whole answer checked, and
    /// the requests of the next step once a step's are all answered.
    pub fn on_bytes(&mut self, data: &[u8]) -> Vec<u8> {
        self.rx.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if self.rx.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes([self.rx[0], self.rx[1], self.rx[2], self.rx[3]]) as usize;
            invariant!((5..=ANSWER_MAX).contains(&len), "an SFTP answer of length {}", len);
            if self.rx.len() < 4 + len {
                break;
            }
            let msg: Vec<u8> = self.rx.drain(..4 + len).skip(4).collect();
            self.answer(&msg, &mut out);
        }
        out
    }

    fn answer(&mut self, msg: &[u8], out: &mut Vec<u8>) {
        let kind = msg[0];
        let mut r = Rd(&msg[1..]);
        if self.stage == Stage::Version {
            invariant!(kind == VERSION && r.u32() == 3, "the INIT answered with a message of kind {}", kind);
            self.next(out);
            return;
        }
        let id = r.u32();
        let Some(want) = self.waiting.remove(&id) else { panic!("invariant: an SFTP answer to id {}, never asked", id) };
        match (kind, want) {
            (STATUS, Want::Ok) => {
                let code = r.u32();
                invariant!(code == FX_OK, "{:?} of {:?}, answered with STATUS {} {:?}", self.stage,
                           String::from_utf8_lossy(&self.plan.dir), code, String::from_utf8_lossy(r.str()));
            }
            (STATUS, Want::Eof) => {
                let code = r.u32();
                invariant!(code == FX_EOF, "a read past the end answered with STATUS {}", code);
            }
            (HANDLE, Want::Handle) => self.handle = r.str().to_vec(),
            (DATA, Want::Data(at, asked)) => {
                let got = r.str();
                let size = self.plan.data.len() as u64;
                let want = (asked as u64).min(size - at) as usize;
                invariant!(got.len() == want, "a read of {} at {} gave {} bytes, the file being {}", asked, at, got.len(),
                           size);
                let at = at as usize;
                if self.back.len() < at + got.len() {
                    self.back.resize(at + got.len(), 0);
                }
                self.back[at..at + got.len()].copy_from_slice(got);
            }
            (ATTRS, Want::Attrs(size)) => {
                let (flags, got_size, mode) = (r.u32(), r.u64(), r.u32());
                invariant!(flags == 5 && got_size == size && mode == FILE_MODE, "a STAT of the file written said flags \
                           {:#x}, {} bytes, mode {:o}; {} were written", flags, got_size, mode, size);
            }
            (NAME, Want::Names) => {
                for _ in 0..r.u32() {
                    self.names.push(r.str().to_vec());
                    r.str();
                    r.u32();
                    r.u64();
                    r.u32();
                }
                /* More of the directory, until its end */
                let handle = self.handle.clone();
                let mut fields = Vec::new();
                put_str(&mut fields, &handle);
                self.send(out, READDIR, Want::Names, &fields);
            }
            (STATUS, Want::Names) => {
                let code = r.u32();
                invariant!(code == FX_EOF, "a READDIR answered with STATUS {}", code);
                invariant!(self.names == [b"f".to_vec()], "the directory listed as {:?}",
                           self.names.iter().map(|n| String::from_utf8_lossy(n)).collect::<Vec<_>>());
            }
            (kind, want) => panic!("invariant: {:?} wanted {:?}, answered with a message of kind {}", self.stage, want,
                                   kind),
        }
        if self.waiting.is_empty() {
            self.next(out);
        }
    }

    /// The next step's requests -- every one of a step's at once.
    fn next(&mut self, out: &mut Vec<u8>) {
        let file = self.file(b"f");
        let handle = self.handle.clone();
        let mut handle_field = Vec::new();
        put_str(&mut handle_field, &handle);
        let size = self.plan.data.len();
        let piece = self.plan.piece.max(1);
        self.stage = match self.stage {
            Stage::Version => {
                let mut f = Vec::new();
                put_str(&mut f, &self.plan.dir.clone());
                put_u32(&mut f, 0);
                self.send(out, MKDIR, Want::Ok, &f);
                Stage::Mkdir
            }
            Stage::Mkdir => {
                let mut f = Vec::new();
                put_str(&mut f, &file);
                put_u32(&mut f, F_WRITE | F_CREAT | F_TRUNC);
                put_u32(&mut f, 0);
                self.send(out, OPEN, Want::Handle, &f);
                Stage::OpenToWrite
            }
            Stage::OpenToWrite if size > 0 => {
                for at in (0..size).step_by(piece) {
                    let mut f = handle_field.clone();
                    f.extend_from_slice(&(at as u64).to_be_bytes());
                    put_str(&mut f, &self.plan.data[at..size.min(at + piece)].to_vec());
                    self.send(out, WRITE, Want::Ok, &f);
                }
                Stage::Write
            }
            Stage::OpenToWrite | Stage::Write => {
                self.send(out, CLOSE, Want::Ok, &handle_field);
                Stage::CloseWritten
            }
            Stage::CloseWritten => {
                let mut f = Vec::new();
                put_str(&mut f, &file);
                self.send(out, STAT, Want::Attrs(size as u64), &f);
                Stage::Stat
            }
            Stage::Stat => {
                let mut f = Vec::new();
                put_str(&mut f, &file);
                put_u32(&mut f, F_READ);
                put_u32(&mut f, 0);
                self.send(out, OPEN, Want::Handle, &f);
                Stage::OpenToRead
            }
            Stage::OpenToRead => {
                for at in (0..size).step_by(piece) {
                    let mut f = handle_field.clone();
                    f.extend_from_slice(&(at as u64).to_be_bytes());
                    put_u32(&mut f, piece as u32);
                    self.send(out, READ, Want::Data(at as u64, piece), &f);
                }
                let mut f = handle_field.clone();
                f.extend_from_slice(&(size as u64).to_be_bytes());
                put_u32(&mut f, piece as u32);
                self.send(out, READ, Want::Eof, &f);
                Stage::Read
            }
            Stage::Read => {
                self.send(out, CLOSE, Want::Ok, &handle_field);
                Stage::CloseRead
            }
            Stage::CloseRead => {
                let mut f = Vec::new();
                put_str(&mut f, &self.plan.dir.clone());
                self.send(out, OPENDIR, Want::Handle, &f);
                Stage::OpenDir
            }
            Stage::OpenDir => {
                self.send(out, READDIR, Want::Names, &handle_field);
                Stage::ReadDir
            }
            Stage::ReadDir => {
                self.send(out, CLOSE, Want::Ok, &handle_field);
                Stage::CloseDir
            }
            Stage::CloseDir => {
                let mut f = Vec::new();
                put_str(&mut f, &file);
                put_str(&mut f, &self.file(b"g"));
                self.send(out, RENAME, Want::Ok, &f);
                Stage::Rename
            }
            Stage::Rename => {
                let mut f = Vec::new();
                put_str(&mut f, &self.file(b"g"));
                self.send(out, REMOVE, Want::Ok, &f);
                Stage::Remove
            }
            Stage::Remove => {
                let mut f = Vec::new();
                put_str(&mut f, &self.plan.dir.clone());
                self.send(out, RMDIR, Want::Ok, &f);
                Stage::Rmdir
            }
            Stage::Rmdir | Stage::Done => {
                invariant!(self.back == self.plan.data, "the file came back {} bytes, {} written, {}", self.back.len(),
                           size, if self.back.len() == size { "different" } else { "a different size" });
                self.finished = true;
                Stage::Done
            }
        };
    }
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        invariant!(self.0.len() >= n, "an SFTP answer cut short");
        let (a, rest) = self.0.split_at(n);
        self.0 = rest;
        a
    }
    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().unwrap())
    }
    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().unwrap())
    }
    fn str(&mut self) -> &'a [u8] {
        let n = self.u32() as usize;
        self.take(n)
    }
}
