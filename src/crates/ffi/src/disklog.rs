unsafe extern "C" {
    /// Whether the kernel log is to be written to a prepared disk area: 1 for
    /// `disklog=on`, 0 without it, and -1 before the command line has been
    /// read at all -- when nobody knows yet, and every line is kept because
    /// the first lines are part of the boot the area is meant to hold.
    pub safe fn kernel_disklog_wanted() -> i32;
}
