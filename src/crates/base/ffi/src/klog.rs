unsafe extern "C" {
    fn kernel_klog(level: u32, msg: *const u8, len: usize);
    /// Whether the log keeps a line of `level`: values only.
    pub safe fn kernel_klog_wants(level: u32) -> i32;
}

pub fn klog(level: u32, s: &str) {
    unsafe {
        kernel_klog(level, s.as_ptr(), s.len());
    }
}

/// Whether a line of `level` is worth formatting: what `klog!` asks first.
#[inline]
pub fn wants(level: u32) -> bool {
    kernel_klog_wants(level) != 0
}

/// A log line's bytes at most, as the kernel's log carries one
/// (`Klog::LINE_SIZE`, the size of its dmesg records and of the disk log's):
/// what `klog!` formats into. It was 512 here and in the bridge, and
/// whatever went past 256 was cut there anyway -- a size that said more
/// reached the log than did, in twice the stack.
pub const LINE_BYTES: usize = 256;

pub struct KlogBuf {
    buf: [u8; LINE_BYTES],
    pos: usize,
}

impl KlogBuf {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; LINE_BYTES],
            pos: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        /* write_str truncates byte-wise at the buffer boundary, which can
         * split a multi-byte UTF-8 sequence; only expose the valid prefix
         * (an unchecked conversion of the full buffer would be UB). */
        match core::str::from_utf8(&self.buf[..self.pos]) {
            Ok(s) => s,
            Err(e) => {
                let valid = e.valid_up_to();
                unsafe { core::str::from_utf8_unchecked(&self.buf[..valid]) }
            }
        }
    }
}

impl core::fmt::Write for KlogBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let n = if bytes.len() < remaining {
            bytes.len()
        } else {
            remaining
        };
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

#[macro_export]
macro_rules! klog {
    ($level:expr, $($arg:tt)*) => {{
        let level: u32 = $level;
        /* The level first: a line is formatted -- core::fmt, into
         * LINE_BYTES of stack -- only when the log keeps it. A klog!(3, ..) on a hot
         * path used to be formatted at every level and thrown away by the
         * C++ side's own look at it. */
        if $crate::klog::wants(level) {
            use core::fmt::Write;
            let mut buf = $crate::klog::__KlogBuf::new();
            let f = file!();
            let fname = match f.rfind('/') {
                Some(i) => &f[i + 1..],
                None => f,
            };
            let _ = write!(buf, "{}(),{},{}: ", module_path!(), fname, line!());
            let _ = write!(buf, $($arg)*);
            $crate::klog::klog(level, buf.as_str());
        }
    }};
}

pub use KlogBuf as __KlogBuf;
