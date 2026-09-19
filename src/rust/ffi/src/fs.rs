/* Files, for a module to keep its configuration in: paths are absolute, and
   nothing is held open between calls. Each returns -1 when the path names
   nothing of the kind, or the filesystem refuses. */
extern "C" {
    /// A regular file's size in bytes.
    pub fn kernel_file_size(path: *const u8, path_len: usize) -> isize;
    /// Up to cap bytes from the start of the file: the count read.
    pub fn kernel_file_read(path: *const u8, path_len: usize, buf: *mut u8, cap: usize) -> isize;
    /// Replaces the file's content -- making the file if it is missing --
    /// without ever leaving it empty or half written.
    pub fn kernel_file_write(path: *const u8, path_len: usize, data: *const u8, len: usize) -> i32;
    /// A new file with this content: 1, and nothing written, when there is
    /// one at the path already.
    pub fn kernel_file_create(path: *const u8, path_len: usize, data: *const u8, len: usize) -> i32;
    /// Makes a directory; 0 also when there is one by that name already.
    pub fn kernel_dir_create(path: *const u8, path_len: usize) -> i32;
}

/* What procfs puts in its files. Each writes into the buffer given and
   answers how many bytes it wrote. */
extern "C" {
    /// "nos <version> (<git rev>)".
    pub fn kernel_version_string(buf: *mut u8, len: usize) -> usize;
    /// The command line the kernel was booted with.
    pub fn kernel_cmdline_string(buf: *mut u8, len: usize) -> usize;
    /// How many interrupt sources there are to ask about.
    pub fn kernel_interrupt_source_count() -> usize;
    /// What the index'th source is called, into `name`, and what it has
    /// counted; -1 past the end.
    pub fn kernel_interrupt_source(index: usize, name: *mut u8, name_len: usize) -> isize;
}

/* What the root filesystem is to be, off the kernel command line, and the
   self-test that runs on it. */
extern "C" {
    /// The root mode (0 none, 1 auto, 2 device, 3 label, 4 uuid), with the
    /// device name or label into `value` and the parsed UUID into `uuid`.
    pub fn kernel_root_spec(
        value: *mut u8, value_len: usize, uuid: *mut u8, uuid_len: usize,
    ) -> i32;
    /// `ro`: the root is to be mounted read-only.
    pub fn kernel_root_read_only() -> i32;
    /// `fstest=on`: run the self-test on / once it is mounted.
    pub fn kernel_root_fstest() -> i32;
}
