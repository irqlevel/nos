//! `wget`: an HTTP or HTTPS GET, to the console or straight to a file.
//!
//! The body never has to exist in memory when it is going to a file: it
//! arrives in TCP-sized pieces and leaves in blocks, so a 20 MB download
//! costs one buffer. That is the whole reason the client takes a sink.

use core::fmt::Write;

use kcore::cmd::Output;
use fs::vfs::{Open, MAX_PATH, OPEN_CREATE, OPEN_TRUNCATE, OPEN_WRITE};

use crate::device::DEVICES;
use crate::http::{self, Response, MAX_BODY, MAX_URL_LEN};

/// The longest URL a command line can carry (the UDP shell takes 255-byte
/// commands, the console 80). Redirect targets run far longer -- a GitHub
/// release link becomes ~900 characters -- but those never pass through
/// here: the client keeps them in its own buffers.
const MAX_ARG_URL: usize = 256;

/// What the body is written to the file in.
const WRITE_BUF: usize = 64 * 1024;

/// Progress line every this many bytes; a big download over a slow link
/// otherwise looks like a hang.
const REPORT_STEP: usize = 1024 * 1024;

/// How much of an in-memory body the console is given.
const MAX_DISPLAY: usize = 4096;

/// What a redirect that was not followed is reported as.
const MAX_LOCATION: usize = 256;

/* ---- a body that goes to a file ---- */

/// The sink's state between calls.
struct FileSink<'a> {
    file: &'a Open<'static>,
    out: &'a mut Output,
    buf: alloc::boxed::Box<[u8]>,
    used: usize,
    written: usize,
    reported: usize,
}

impl http::Sink for FileSink<'_> {
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

        let total = self.written + self.used;
        if total - self.reported >= REPORT_STEP {
            self.reported = total - (total % REPORT_STEP);
            let _ = writeln!(self.out, "wget: {} KB", total / 1024);
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
            let _ = writeln!(self.out, "wget: write failed after {} bytes", self.written);
            return false;
        }

        self.written += len;
        true
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

    let mut buf = alloc::vec::Vec::new();
    if buf.try_reserve_exact(WRITE_BUF).is_err() {
        let _ = writeln!(out, "wget: out of memory");
        return;
    }
    buf.resize(WRITE_BUF, 0);

    let mut location = [0u8; MAX_LOCATION];
    let (resp, written, flushed) = {
        let mut sink = FileSink {
            file: &file, out, buf: buf.into_boxed_slice(),
            used: 0, written: 0, reported: 0,
        };
        let resp = http::get_with_location(nic, url.as_bytes(), &mut sink, &mut location);
        let flushed = sink.flush();
        (resp, sink.written, flushed)
    };

    /* The handle goes before the file may be taken away: an open one keeps
     * the filesystem from removing it. */
    drop(file);

    /* Nothing landed -- a failed request, or a body refused before the first
     * byte: do not leave an empty file behind. */
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
        if resp.body_len >= MAX_BODY {
            let _ = writeln!(out, "wget: body over the {} MB limit", MAX_BODY / (1024 * 1024));
        } else {
            let _ = writeln!(out, "wget: incomplete, {} bytes saved to {}", written, path);
        }
        return;
    }

    let _ = writeln!(out, "saved {} bytes to {}", written, path);
}
