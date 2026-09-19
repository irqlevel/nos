unsafe extern "C" {
    /// One decoded keystroke to every observer: the character, and the PS/2
    /// set-1 make code consumers key off (0x0E is backspace). A zero
    /// character means the key produced none.
    pub safe fn kernel_input_key(c: u8, code: u8);
}
