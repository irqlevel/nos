//! The console: the 8250 at COM1, which hv's console reads -- each line of
//! hvboot's starts "hvboot: ", so that a VM's console says whose it is.

use crate::io::{inb, outb};

const COM1: u16 = 0x3F8;
/// The line status register, and its "transmitter holding register empty".
const LSR: u16 = COM1 + 5;
const LSR_THRE: u8 = 1 << 5;
/// How long a byte waits for the transmitter, at most: hv's is always
/// ready, and a port that never is must not hang the loader.
const SPINS: u32 = 1 << 16;

pub struct Console;

fn put(b: u8) {
    let mut spins = 0;
    while inb(LSR) & LSR_THRE == 0 && spins < SPINS {
        spins += 1;
    }
    outb(COM1, b);
}

impl core::fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                put(b'\r');
            }
            put(b);
        }
        Ok(())
    }
}

/// A line on the console, "hvboot: " first.
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = core::fmt::Write::write_fmt(&mut $crate::console::Console,
                                            format_args!("hvboot: {}\n", format_args!($($arg)*)));
    }};
}
