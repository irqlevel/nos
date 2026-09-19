//! The kernel's input: where a keyboard driver puts what it decoded.

/// One keystroke to every observer -- the shell above all. `c` is the
/// character, or 0 when the key produces none; `code` is the PS/2 set-1 make
/// code, which is what consumers key off (0x0E is backspace), so a USB
/// keyboard is indistinguishable from the 8042 one.
pub fn key(c: u8, code: u8) {
    ffi::input::kernel_input_key(c, code)
}
