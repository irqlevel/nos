unsafe extern "C" {
    /// One decoded byte of a keystroke to every observer: what a terminal
    /// sends for the key, and its PS/2 set-1 make code. A zero character
    /// means the key produced none.
    pub safe fn kernel_input_key(c: u8, code: u8);
}
