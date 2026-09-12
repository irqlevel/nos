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
