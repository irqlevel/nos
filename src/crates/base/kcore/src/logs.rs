//! The kernel's file logs (docs/logs.md), as a module writes to one: lines
//! kept in `/logs/<name>.log` for a person to read later -- sshd's sessions
//! in `net.log` -- and not in the kernel log, whose ring and consoles they
//! would crowd. A line is formatted where it is written and handed to the
//! kernel's queue, from anywhere and without waiting; the kernel's own task
//! writes it to its file a moment later (`fs::logs`, which the image's code
//! writes through directly).
//!
//! ```ignore
//! static NET: kcore::logs::Log = kcore::logs::Log::new("net");
//! kcore::flog!(NET, "sshd: {} logged in from {}", user, peer);
//! ```

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use ffi::logs as abi;

pub use ffi::logs::{LOG_FILE_DEFAULT, LOG_LINE_MAX, LOG_NAME_MAX, LOG_TOTAL_DEFAULT};

/// A line written to a file log, `flog!(LOG, "fmt {}", arg)`: formatted into
/// a line's room on the stack and handed to `LOG` -- a `kcore::logs::Log`
/// in a module, an `fs::logs::Log` in the image -- which queues it. From
/// anywhere: nothing here waits or allocates.
#[macro_export]
macro_rules! flog {
    ($log:expr, $($arg:tt)*) => {
        $log.line(core::format_args!($($arg)*))
    };
}

/// A line's room: `LOG_LINE_MAX` bytes, what a queued line carries. What
/// does not fit is cut where a character begins, and said (`cut`).
pub struct LineBuf {
    buf: [u8; LOG_LINE_MAX],
    len: usize,
    cut: bool,
}

impl LineBuf {
    pub const fn new() -> LineBuf {
        LineBuf { buf: [0; LOG_LINE_MAX], len: 0, cut: false }
    }

    /// What was written, as far as it fits: whole characters.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Whether anything did not fit.
    pub fn cut(&self) -> bool {
        self.cut
    }
}

impl Default for LineBuf {
    fn default() -> LineBuf {
        LineBuf::new()
    }
}

impl fmt::Write for LineBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.cut {
            return Ok(());
        }
        let room = self.buf.len() - self.len;
        let mut n = s.len().min(room);
        if n < s.len() {
            /* Whole characters only: the file gets text, not half of one */
            while n > 0 && !s.is_char_boundary(n) {
                n -= 1;
            }
            self.cut = true;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// A file log of a module's, by its name -- `Log::new("net")` in a
/// `static` -- opened the first time a line is written to it and named by
/// the handle from then on. Every `Log` of one name, the image's and any
/// module's, writes the same files. A name the kernel refuses, or a table
/// with no room for another log, sends the line to the kernel log instead,
/// as the kernel says it did.
pub struct Log {
    name: &'static str,
    file_max: u64,
    total_max: u64,
    /// `kernel_log_open`'s handle, 0 until it answered one
    handle: AtomicU64,
}

impl Log {
    /// The log `name`, with the default limits if it is the first to ask
    /// for it.
    pub const fn new(name: &'static str) -> Log {
        Log::with_limits(name, 0, 0)
    }

    /// The log `name`, held to these limits -- its file's size and all its
    /// files' together, in bytes -- if it is the first to ask for it; the
    /// kernel refuses limits outside its own (`LOG_FILE_MIN` and the rest).
    pub const fn with_limits(name: &'static str, file_max: u64, total_max: u64) -> Log {
        Log { name, file_max, total_max, handle: AtomicU64::new(0) }
    }

    /// A line formatted -- what `flog!` calls.
    pub fn line(&self, args: fmt::Arguments) {
        let mut buf = LineBuf::new();
        let _ = fmt::Write::write_fmt(&mut buf, args);
        self.write(buf.bytes(), buf.cut());
    }

    /// A line as it is: `LOG_LINE_MAX` bytes of it at most, the rest cut.
    pub fn write(&self, line: &[u8], cut: bool) {
        let mut handle = self.handle.load(Ordering::Relaxed);
        if handle == 0 {
            /* A race here opens twice and is answered the same handle. A
             * refusal -- said by the kernel as it refuses -- is kept, and
             * not asked again at every line. */
            // SAFETY: the name is `len` readable bytes of a str.
            handle = unsafe {
                abi::kernel_log_open(self.name.as_ptr(), self.name.len(), self.file_max, self.total_max)
            };
            if handle == 0 {
                handle = REFUSED;
            }
            self.handle.store(handle, Ordering::Relaxed);
        }
        let len = line.len().min(LOG_LINE_MAX);
        let cut = if cut || len < line.len() { abi::LOG_CUT } else { 0 };
        /* A refused log's handle is no log's: the kernel writes its line to
         * the kernel log instead (`kernel_log_write`). */
        // SAFETY: the line is `len` readable bytes of a slice.
        unsafe { abi::kernel_log_write(handle, line.as_ptr(), len, cut) }
    }
}

/// What a `Log` keeps for a log the kernel refused: no log's handle -- the
/// kernel's are a table's slot and one -- and not 0, which asks again.
const REFUSED: u64 = u64::MAX;
