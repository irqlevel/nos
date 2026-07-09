extern "C" {
    fn kernel_trace(level: u32, msg: *const u8, len: usize);
}

pub fn trace(level: u32, s: &str) {
    unsafe {
        kernel_trace(level, s.as_ptr(), s.len());
    }
}

pub struct TraceBuf {
    buf: [u8; 512],
    pos: usize,
}

impl TraceBuf {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; 512],
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

impl core::fmt::Write for TraceBuf {
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
macro_rules! trace {
    ($level:expr, $($arg:tt)*) => {{
        use core::fmt::Write;
        let mut buf = $crate::trace::__TraceBuf::new();
        let f = file!();
        let fname = match f.rfind('/') {
            Some(i) => &f[i + 1..],
            None => f,
        };
        let _ = write!(buf, "{}(),{},{}: ", module_path!(), fname, line!());
        let _ = write!(buf, $($arg)*);
        $crate::trace::trace($level, buf.as_str());
    }};
}

pub use TraceBuf as __TraceBuf;
