/* Files, for a module to keep its configuration in: paths are absolute, and
   nothing is held open between calls. Each returns -1 when the path names
   nothing of the kind, or the filesystem refuses. */
unsafe extern "C" {
    /// A regular file's size in bytes.
    pub fn kernel_file_size(path: *const u8, path_len: usize) -> isize;
    /// Up to cap bytes from the start of the file: the count read.
    pub fn kernel_file_read(path: *const u8, path_len: usize, buf: *mut u8, cap: usize) -> isize;
    /// Up to cap bytes from `offset`: the count read, 0 at or past the end.
    pub fn kernel_file_read_at(
        path: *const u8, path_len: usize, offset: u64, buf: *mut u8, cap: usize,
    ) -> isize;
    /// Replaces the file's content -- making the file if it is missing --
    /// without ever leaving it empty or half written.
    pub fn kernel_file_write(path: *const u8, path_len: usize, data: *const u8, len: usize) -> i32;
    /// A new file with this content: 1, and nothing written, when there is
    /// one at the path already.
    pub fn kernel_file_create(path: *const u8, path_len: usize, data: *const u8, len: usize) -> i32;
    /// Makes a directory; 0 also when there is one by that name already.
    pub fn kernel_dir_create(path: *const u8, path_len: usize) -> i32;
}

/* A file held open -- a guest's disk image, for as long as the guest runs:
   its handle is looked up on every call, so any word is safe to pass back,
   and one that is no open file's reads as no file. All of them sleep. */
unsafe extern "C" {
    /// Opens a regular file for reading, and with `write` not 0 for writing
    /// too: its handle, or 0.
    pub fn kernel_file_open(path: *const u8, path_len: usize, write: i32) -> usize;
    /// Closes it; a word that is no open file's is nothing to close.
    pub safe fn kernel_file_close(file: usize);
    /// Its size in bytes, or -1.
    pub safe fn kernel_file_length(file: usize) -> i64;
    /// Up to cap bytes from `offset`: the count read, 0 at or past the end,
    /// or -1.
    pub fn kernel_file_pread(file: usize, offset: u64, buf: *mut u8, cap: usize) -> isize;
    /// Writes `len` bytes at `offset`, within the file's size -- never
    /// growing it: `len`, or -1. Not synced.
    pub fn kernel_file_pwrite(file: usize, offset: u64, data: *const u8, len: usize) -> isize;
    /// Everything written to the file's filesystem on its disk's medium: 0,
    /// or -1.
    pub safe fn kernel_file_fsync(file: usize) -> i32;
}

/* What procfs puts in its files. Each writes into the buffer given and
   answers how many bytes it wrote. */
unsafe extern "C" {
    /// "nos <version> (<git rev>)".
    pub fn kernel_version_string(buf: *mut u8, len: usize) -> usize;
    /// The command line the kernel was booted with.
    pub fn kernel_cmdline_string(buf: *mut u8, len: usize) -> usize;
    /// How many interrupt sources there are to ask about.
    pub safe fn kernel_interrupt_source_count() -> usize;
    /// What the index'th source is called, into `name`, and what it has
    /// counted; -1 past the end.
    pub fn kernel_interrupt_source(index: usize, name: *mut u8, name_len: usize) -> isize;
}

/* What the root filesystem is to be, off the kernel command line, and the
   self-test that runs on it. */
unsafe extern "C" {
    /// The root mode (0 none, 1 auto, 2 device, 3 label, 4 uuid), with the
    /// device name or label into `value` and the parsed UUID into `uuid`.
    pub fn kernel_root_spec(
        value: *mut u8, value_len: usize, uuid: *mut u8, uuid_len: usize,
    ) -> i32;
    /// `ro`: the root is to be mounted read-only.
    pub safe fn kernel_root_read_only() -> i32;
    /// `fstest=on`: run the self-test on / once it is mounted.
    pub safe fn kernel_root_fstest() -> i32;
}
