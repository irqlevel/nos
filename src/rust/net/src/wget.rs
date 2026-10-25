//! `wget`: an HTTP or HTTPS GET, to the console or straight to a file.
//!
//! The body never has to exist in memory when it is going to a file: it
//! arrives in TCP-sized pieces and leaves in blocks, so a download of
//! gigabytes -- a VM's disk image -- costs one buffer, as a small one does.
//! That is the whole reason the client takes a sink, and why a file has no
//! cap but the filesystem's: a disk that fills ends the download as a write
//! that failed.

use core::fmt::Write;

use kcore::cmd::Output;
use kcore::consts::NS_PER_SEC;
use fs::vfs::{Open, MAX_PATH, OPEN_CREATE, OPEN_TRUNCATE, OPEN_WRITE};

use crate::device::DEVICES;
use crate::http::{self, Response, MAX_URL_LEN};

/// The longest URL a command line can carry (the UDP shell takes 255-byte
/// commands, the console 80). Redirect targets run far longer -- a GitHub
/// release link becomes ~900 characters -- but those never pass through
/// here: the client keeps them in its own buffers.
const MAX_ARG_URL: usize = 256;

/// What the body is written to the file in.
const WRITE_BUF: usize = 64 * 1024;

/// How far apart the progress lines are: a tenth of the time the download
/// has taken so far, from two seconds to thirty -- often at first, when
/// someone is watching whether it goes at all, and seldom an hour in. A line
/// a megabyte, what there was, is thousands of them for an image of
/// gigabytes, and the UDP shell, which sends a command's output once it is
/// over, keeps 32 KiB of it: past that the line saying how it ended was lost.
const REPORT_MIN_NS: u64 = 2 * NS_PER_SEC;
const REPORT_MAX_NS: u64 = 30 * NS_PER_SEC;
const REPORT_SHARE: u64 = 10;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// How much of an in-memory body the console is given.
const MAX_DISPLAY: usize = 4096;

/// What a redirect that was not followed is reported as.
const MAX_LOCATION: usize = 256;

/* ---- a body that goes to a file ---- */

/// The sink's state between calls.
struct FileSink<'a> {
    file: &'a Open<'static>,
    out: &'a mut Output,
    /// What its lines begin with: the command's name.
    who: &'static str,
    buf: alloc::boxed::Box<[u8]>,
    used: usize,
    written: usize,
    /// The body's length, when the server said it; 0 when it did not
    length: usize,
    /// When the download began, and when progress was last told, in
    /// nanoseconds since boot
    started: u64,
    reported: u64,
}

impl http::Sink for FileSink<'_> {
    fn length(&mut self, len: usize) {
        self.length = len;
    }

    fn take(&mut self, data: &[u8]) -> usize {
        let before = self.written;
        let mut taken = 0;

        while taken < data.len() {
            let room = WRITE_BUF - self.used;
            let take = (data.len() - taken).min(room);
            self.buf[self.used..self.used + take].copy_from_slice(&data[taken..taken + take]);
            self.used += take;
            taken += take;

            /* A failed block never reached the disk, and neither did
             * anything still buffered: only what flush committed counts. */
            if self.used == WRITE_BUF && !self.flush() {
                return self.written - before;
            }
        }

        let now = kcore::time::boot_time_ns();
        let every = (now.saturating_sub(self.started) / REPORT_SHARE)
            .clamp(REPORT_MIN_NS, REPORT_MAX_NS);
        if now.saturating_sub(self.reported) >= every {
            self.reported = now;
            self.progress(now);
        }

        taken
    }
}

impl FileSink<'_> {
    /// Pushes what the buffer still holds; called once the body is over.
    fn flush(&mut self) -> bool {
        if self.used == 0 {
            return true;
        }
        let len = self.used;
        self.used = 0;

        if !self.file.write(&self.buf[..len]) {
            let _ = writeln!(self.out, "{}: write failed after {} bytes", self.who, self.written);
            return false;
        }

        self.written += len;
        true
    }

    /// The progress line: how much, of how much when the server said, and
    /// how fast since the start.
    fn progress(&mut self, now: u64) {
        let total = self.written + self.used;
        let _ = write!(self.out, "{}: {} MiB", self.who, total as u64 / MIB);
        if self.length != 0 {
            let percent = (total as u128 * 100 / self.length as u128).min(100);
            let _ = write!(self.out, " of {} MiB, {}%", self.length as u64 / MIB, percent);
        }
        let _ = writeln!(self.out, ", {}", Rate::of(total, now.saturating_sub(self.started)));
    }
}

/// Bytes over a time, as a rate: MiB/s to a tenth from one up, KiB/s below.
struct Rate(u64);

impl Rate {
    fn of(bytes: usize, ns: u64) -> Rate {
        if ns == 0 {
            return Rate(0);
        }
        let per_sec = bytes as u128 * NS_PER_SEC as u128 / ns as u128;
        Rate(u64::try_from(per_sec).unwrap_or(u64::MAX))
    }
}

impl core::fmt::Display for Rate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 >= MIB {
            write!(f, "{}.{} MiB/s", self.0 / MIB, self.0 % MIB * 10 / MIB)
        } else {
            write!(f, "{} KiB/s", self.0 / KIB)
        }
    }
}

/* ---- a body that goes to the console ---- */

struct MemSink<'a> {
    body: &'a mut alloc::vec::Vec<u8>,
}

impl http::Sink for MemSink<'_> {
    fn take(&mut self, data: &[u8]) -> usize {
        /* Capped: without a path to save it to, a body is only ever looked
         * at. */
        let room = http::MAX_RESPONSE.saturating_sub(self.body.len());
        let take = data.len().min(room);
        if take == 0 {
            return 0;
        }
        if self.body.try_reserve(take).is_err() {
            return 0;
        }
        self.body.extend_from_slice(&data[..take]);
        take
    }
}

/* ---- why a request produced nothing ---- */

fn report_failure(resp: &Response, out: &mut Output) {
    /* TLS gets its own line: "failed" for a rejected certificate would send
     * the reader looking in the wrong place. */
    if resp.tls_failed != 0 {
        let _ = writeln!(out, "wget: TLS handshake refused -- bad certificate, or no \
protocol in common (dmesg has the reason)");
    } else if resp.url_too_long != 0 {
        let _ = writeln!(out, "wget: URL, or a redirect's target, longer than {} characters",
            MAX_URL_LEN - 1);
    } else {
        let _ = writeln!(out, "wget: failed");
    }
}

fn report_head(resp: &Response, location: &[u8], out: &mut Output) {
    let _ = writeln!(out, "HTTP {}, {} bytes", resp.status, resp.body_len);

    let len = location.iter().position(|b| *b == 0).unwrap_or(location.len());
    if len != 0 {
        let _ = writeln!(out, "Location: {}",
            core::str::from_utf8(&location[..len]).unwrap_or("?"));
    }
}

/* ---- the command ---- */

pub fn wget(args: &str, out: &mut Output) {
    let mut url = "";
    let mut path = "";

    /* wget [-o <path>] <url> [path] -- the flag and the trailing argument
       mean the same thing, whichever reads better. */
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "-o" {
            match tokens.next() {
                Some(next) => path = next,
                None => { let _ = writeln!(out, "wget: -o needs a path"); return; }
            }
        } else if url.is_empty() {
            /* A cut-down URL would fetch some other resource. */
            if token.len() >= MAX_ARG_URL {
                let _ = writeln!(out, "wget: URL longer than {} characters", MAX_ARG_URL - 1);
                return;
            }
            url = token;
        } else if path.is_empty() {
            path = token;
        }
    }

    if url.is_empty() {
        let _ = writeln!(out, "usage: wget [-o <path>] <url> [path]");
        return;
    }

    let dev = match DEVICES.find(b"eth0") {
        Some(dev) => dev,
        None => { let _ = writeln!(out, "eth0 not found"); return; }
    };
    let nic = dev.as_nic();

    if !path.is_empty() {
        to_file(&nic, url, path, out);
        return;
    }

    /* No file: the body is kept in memory, capped at MAX_RESPONSE. */
    let mut body = alloc::vec::Vec::new();
    let mut location = [0u8; MAX_LOCATION];
    let resp = {
        let mut sink = MemSink { body: &mut body };
        http::get_with_location(&nic, url.as_bytes(), &mut sink, &mut location)
    };

    if resp.ok == 0 {
        report_failure(&resp, out);
        return;
    }

    report_head(&resp, &location, out);

    if !body.is_empty() {
        let shown = body.len().min(MAX_DISPLAY);
        out.write_bytes(&body[..shown]);
        let _ = writeln!(out);
        if body.len() > shown {
            let _ = writeln!(out, "... ({} bytes truncated)", body.len() - shown);
        }
    }

    if resp.truncated != 0 {
        let _ = writeln!(out, "wget: body truncated, pass a path to save it to a file");
    }
}

/// What a body streamed into a file came to.
struct Download {
    resp: Response,
    /// A redirect's target that was not followed.
    location: [u8; MAX_LOCATION],
    /// What reached the file, and whether the last of it did.
    written: usize,
    flushed: bool,
    /// How long it all took.
    ns: u64,
}

/// `url`'s answer streamed into `file`, progress told as `who`'s -- or None
/// out of memory.
fn download(nic: &crate::nic::Nic, url: &str, file: &Open<'static>, who: &'static str, out: &mut Output)
            -> Option<Download> {
    let mut buf = alloc::vec::Vec::new();
    buf.try_reserve_exact(WRITE_BUF).ok()?;
    buf.resize(WRITE_BUF, 0);

    let mut location = [0u8; MAX_LOCATION];
    let started = kcore::time::boot_time_ns();
    let mut sink = FileSink {
        file, out, who, buf: buf.into_boxed_slice(),
        used: 0, written: 0, length: 0, started, reported: started,
    };
    let resp = http::get_with_location(nic, url.as_bytes(), &mut sink, &mut location);
    let flushed = sink.flush();
    Some(Download { resp, location, written: sink.written, flushed,
                    ns: kcore::time::boot_time_ns().saturating_sub(started) })
}

/// Downloads to a file, streaming. Says why on the way out when it fails.
fn to_file(nic: &crate::nic::Nic, url: &str, path: &str, out: &mut Output) {
    /* A file to write from the beginning, made if it is missing and emptied
     * if it is not -- through the filesystem layer itself: this crate is in
     * the kernel image with it. */
    let vfs = match fs::vfs_instance() {
        Some(vfs) if !path.is_empty() && path.len() < MAX_PATH => vfs,
        _ => { let _ = writeln!(out, "wget: cannot open {} for writing", path); return; }
    };
    let file = match Open::new(vfs, path.as_bytes(), OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE) {
        Some(file) => file,
        None => { let _ = writeln!(out, "wget: cannot open {} for writing", path); return; }
    };

    let Some(got) = download(nic, url, &file, "wget", out) else {
        let _ = writeln!(out, "wget: out of memory");
        return;
    };
    let Download { resp, location, written, flushed, ns } = got;

    /* The handle goes before the file may be taken away: an open one keeps
     * the filesystem from removing it. */
    drop(file);

    /* Nothing landed -- a failed request, or an answer with no body: do not
     * leave an empty file behind. */
    if written == 0 {
        vfs.remove(path.as_bytes());
    }

    if resp.ok == 0 {
        report_failure(&resp, out);
        return;
    }

    report_head(&resp, &location, out);

    if !flushed {
        return;
    }

    if resp.truncated != 0 {
        let _ = writeln!(out, "wget: incomplete, {} bytes saved to {}", written, path);
        return;
    }

    let _ = writeln!(out, "saved {} bytes to {} in {}.{} s, {}", written, path,
        ns / NS_PER_SEC, ns % NS_PER_SEC / (NS_PER_SEC / 10), Rate::of(written, ns));
}

/* ---- a download a module asks for ---- */

/// Why `fetch` left no file: the request's failure (`http::Failure`), an
/// answer that is not a body, or the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// The URL is none this client fetches, or too long.
    Url,
    Resolve,
    Connect,
    Tls,
    /// The request did not go, or no whole answer came back.
    Answer,
    Redirects,
    NoMemory,
    /// No network device to fetch through.
    NoDevice,
    /// An answer other than 2xx -- a 404's page is no body of the URL's --
    /// or a redirect that leads to no http: the status.
    Status(u16),
    /// There is a file at the path already, which is not written over.
    Exists,
    /// The file could not be made.
    Create,
    /// A write failed: a full filesystem, most likely.
    Write,
    /// The body was cut short: an idle timeout, or a peer that closed early.
    Cut,
}

impl From<http::Failure> for Fetch {
    fn from(f: http::Failure) -> Fetch {
        match f {
            http::Failure::Url | http::Failure::None => Fetch::Url,
            http::Failure::Resolve => Fetch::Resolve,
            http::Failure::Connect => Fetch::Connect,
            http::Failure::Tls => Fetch::Tls,
            http::Failure::Answer => Fetch::Answer,
            http::Failure::Redirects => Fetch::Redirects,
            http::Failure::NoMemory => Fetch::NoMemory,
        }
    }
}

/// `url`'s body, by HTTP or HTTPS, in a new file at `path` -- made here, never
/// one that is there -- its progress told on `out` as wget's is, but as
/// `who`'s: the body's length, or why not, and then no file. Only a 2xx
/// answer is a body: what a server says with a 404 is no image.
pub fn fetch(url: &str, path: &str, who: &'static str, out: &mut Output) -> Result<u64, Fetch> {
    if url.is_empty() || url.len() >= MAX_URL_LEN {
        return Err(Fetch::Url);
    }
    let dev = DEVICES.find(b"eth0").ok_or(Fetch::NoDevice)?;
    let nic = dev.as_nic();
    let vfs = fs::vfs_instance().ok_or(Fetch::Create)?;
    if path.is_empty() || path.len() >= MAX_PATH {
        return Err(Fetch::Create);
    }
    if vfs.stat(path.as_bytes()).is_some() {
        return Err(Fetch::Exists);
    }
    /* Made, then opened: `create` refuses a path that is there, so what is
     * written is this download's own file, and its to remove. */
    if !vfs.create(path.as_bytes(), false) {
        return Err(Fetch::Create);
    }
    let Some(file) = Open::new(vfs, path.as_bytes(), OPEN_WRITE) else {
        vfs.remove(path.as_bytes());
        return Err(Fetch::Create);
    };

    let got = download(&nic, url, &file, who, out);
    /* Closed before it may be removed. */
    drop(file);
    let result = match got {
        None => Err(Fetch::NoMemory),
        Some(d) if d.resp.ok == 0 => Err(Fetch::from(d.resp.failure)),
        Some(d) if d.resp.url_too_long != 0 => Err(Fetch::Url),
        Some(d) if !(200..300).contains(&d.resp.status) => Err(Fetch::Status(d.resp.status.clamp(0, 999) as u16)),
        Some(d) if !d.flushed => Err(Fetch::Write),
        Some(d) if d.resp.truncated != 0 => Err(Fetch::Cut),
        Some(d) => {
            let _ = writeln!(out, "{}: {} bytes in {}.{} s, {}", who, d.written, d.ns / NS_PER_SEC,
                             d.ns % NS_PER_SEC / (NS_PER_SEC / 10), Rate::of(d.written, d.ns));
            Ok(d.written as u64)
        }
    };
    if result.is_err() {
        vfs.remove(path.as_bytes());
    }
    result
}
