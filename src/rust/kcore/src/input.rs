//! The kernel's input: where a keyboard driver puts what it decoded.

/// One byte of a keystroke to every observer -- the shell above all. `c` is
/// what a terminal sends for the key: its character, BS for Backspace, the
/// control byte for Ctrl and a letter, or a byte of an arrow's escape
/// sequence (ESC [ A is Up), each sent with its own call; 0 when the key
/// produces none. `code` is the PS/2 set-1 make code, so a USB keyboard is
/// indistinguishable from the 8042 one.
pub fn key(c: u8, code: u8) {
    ffi::input::kernel_input_key(c, code)
}
