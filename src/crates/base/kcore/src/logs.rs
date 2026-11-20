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

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

use ffi::logs as abi;

use crate::consts::NS_PER_MS;
use crate::sync::IrqSpinLock;

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

/// A line from a place that may say it a thousand times a second -- a write
/// that fails, a peer that does not answer -- `flog_limited!(LOG, "fmt",
/// ..)`: written as `flog!` writes one, but held to the place's `Limiter`
/// (`LIMIT_ALIKE` lines alike and `LIMIT_PLACE` in all a `LIMIT_WINDOW_MS`
/// window), the rest counted, and the next line that goes from the place
/// saying how many before it: `[held back: 1834] ...`. Each use of the macro
/// is a place with a limiter of its own. From anywhere, as `flog!` is; a
/// line held while its place is closed costs a clock read and an atomic
/// add -- it is not even formatted.
#[macro_export]
macro_rules! flog_limited {
    ($log:expr, $($arg:tt)*) => {{
        static LIMITER: $crate::logs::Limiter = $crate::logs::Limiter::new();
        let now = $crate::time::boot_time_ns();
        if LIMITER.open_at(now) {
            let mut line = $crate::logs::LineBuf::new();
            let _ = core::fmt::Write::write_fmt(&mut line, core::format_args!($($arg)*));
            if let Some(held) = LIMITER.admit_at(line.bytes(), now) {
                let line = $crate::logs::held_back(held, line);
                $log.write(line.bytes(), line.cut());
            }
        }
    }};
}

/// A `Limiter`'s window, and what it lets through in each: lines alike --
/// the same but for their digits, a volume's number or a count -- and lines
/// in all, from its place.
pub const LIMIT_WINDOW_MS: u64 = 60_000;
pub const LIMIT_ALIKE: u32 = 5;
pub const LIMIT_PLACE: u32 = 30;
/// Kinds of line a place's limiter tells apart at once: one more takes the
/// slot of the one whose window began longest ago, which starts counting
/// again -- the place's own count still holds them all.
pub const LIMIT_SHAPES: usize = 8;

/// One kind of line a `Limiter` counts: its shape (`alike`), when its window
/// began, and how many of it went in it.
#[derive(Clone, Copy)]
struct Shape {
    key: u64,
    start: u64,
    went: u32,
}

struct Place {
    /// The place's window -- 0 for none begun -- and its lines that went
    /// in it
    start: u64,
    went: u32,
    /// Lines held back under the lock since the last one that went
    held: u64,
    shapes: [Shape; LIMIT_SHAPES],
}

/// What holds a place's lines to a rate (`flog_limited!`): a few lines of
/// each kind a window, a few dozen in all, and how many it held back since
/// the last that went. A failure a thousand times a second is then a few
/// lines a minute and a count, and a line of another kind from the same
/// place still goes. The count is said by the place's next line that goes:
/// a flood that stops leaves its last count to the next line of the place,
/// whenever that comes.
///
/// Once a window's `LIMIT_PLACE` lines have gone the place is closed until
/// the window ends (`closed_until`): a line then is counted in `held_closed`
/// with no lock and not formatted, so that a flood costs the place's callers
/// next to nothing. Every line held is counted once, in one of the two, and
/// taken by one line that goes.
pub struct Limiter {
    closed_until: AtomicU64,
    held_closed: AtomicU64,
    place: IrqSpinLock<Place>,
}

impl Limiter {
    pub const fn new() -> Limiter {
        Limiter {
            closed_until: AtomicU64::new(0),
            held_closed: AtomicU64::new(0),
            place: IrqSpinLock::new(Place {
                start: 0,
                went: 0,
                held: 0,
                shapes: [Shape { key: 0, start: 0, went: 0 }; LIMIT_SHAPES],
            }),
        }
    }

    /// Whether a line at `now` (`time::boot_time_ns`) is to be formatted and
    /// asked about (`admit_at`): not while the place is closed, when it is
    /// counted held here.
    #[inline]
    pub fn open_at(&self, now: u64) -> bool {
        if now < self.closed_until.load(Ordering::Relaxed) {
            self.held_closed.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Whether `line` may go at `now`: `Some` with how many lines of the
    /// place's were held back since the last one that went, or `None` --
    /// held back itself, and counted.
    pub fn admit_at(&self, line: &[u8], now: u64) -> Option<u64> {
        let key = alike(line);
        let window = LIMIT_WINDOW_MS * NS_PER_MS;
        /* 0 is a window never begun */
        let now = now.max(1);
        let mut guard = self.place.lock();
        let place = &mut *guard;
        if place.start == 0 || now.saturating_sub(place.start) >= window {
            place.start = now;
            place.went = 0;
            self.closed_until.store(0, Ordering::Relaxed);
        }
        let at = match place.shapes.iter().position(|s| s.start != 0 && s.key == key) {
            Some(at) => at,
            None => {
                let at = place.shapes.iter().enumerate().min_by_key(|(_, s)| s.start).map_or(0, |(at, _)| at);
                place.shapes[at] = Shape { key, start: now, went: 0 };
                at
            }
        };
        let shape = &mut place.shapes[at];
        if now.saturating_sub(shape.start) >= window {
            shape.start = now;
            shape.went = 0;
        }
        if shape.went >= LIMIT_ALIKE || place.went >= LIMIT_PLACE {
            place.held = place.held.saturating_add(1);
            return None;
        }
        shape.went += 1;
        place.went += 1;
        if place.went >= LIMIT_PLACE {
            self.closed_until.store(place.start.saturating_add(window), Ordering::Relaxed);
        }
        let held = core::mem::take(&mut place.held);
        Some(held.saturating_add(self.held_closed.swap(0, Ordering::Relaxed)))
    }
}

impl Default for Limiter {
    fn default() -> Limiter {
        Limiter::new()
    }
}

/// What makes two lines alike: their bytes but for their digits -- FNV-1a of
/// the rest.
fn alike(line: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    line.iter().filter(|b| !b.is_ascii_digit()).fold(OFFSET, |h, &b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

/// `line` as a place under a limiter writes it: with how many of its lines
/// were held back before it, when any were.
pub fn held_back(held: u64, line: LineBuf) -> LineBuf {
    if held == 0 {
        return line;
    }
    let mut out = LineBuf::new();
    let _ = write!(out, "[held back: {}] ", held);
    /* A LineBuf holds whole characters only */
    let _ = out.write_str(core::str::from_utf8(line.bytes()).unwrap_or(""));
    out.cut |= line.cut;
    out
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
