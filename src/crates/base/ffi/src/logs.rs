/* The kernel's file logs (layers/fs/src/logs.rs, docs/logs.md): a line handed
   to one is queued, and written to /logs/<name>.log by a task of the
   kernel's a moment later. What both sides of the seam hold a log and a line
   to is here, once. */

/// A log's name at most, in bytes: letters, digits, '-' and '_', a
/// lowercase letter first -- its file's name with `.log` after it.
pub const LOG_NAME_MAX: usize = 15;
/// A line's text at most, in bytes, as it is queued: a longer one is cut,
/// and its file says so.
pub const LOG_LINE_MAX: usize = 240;
/// What a log's file is held to, and all its files together, unless whoever
/// makes it says otherwise: a megabyte, and eight.
pub const LOG_FILE_DEFAULT: u64 = 1 << 20;
pub const LOG_TOTAL_DEFAULT: u64 = 8 << 20;
/// The least a file may be held to -- some dozens of lines -- and the most.
pub const LOG_FILE_MIN: u64 = 4096;
pub const LOG_FILE_LIMIT: u64 = 64 << 20;
/// The most all a log's files may be held to together.
pub const LOG_TOTAL_LIMIT: u64 = 1 << 30;
/// A log's old files at most -- `<name>.log.1` to `.99` -- and so its total
/// at most this many and one of its file's limit.
pub const LOG_OLD_MAX: u64 = 99;
/// `kernel_log_write`'s `cut`: the line was longer than it carries.
pub const LOG_CUT: u32 = 1;

/* Each from anywhere -- a task, a soft IRQ, an interrupt handler, under a
   spin lock -- and none waits: a line goes on a queue, or is counted lost
   when the queue has no room. */
unsafe extern "C" {
    /// The log called `name`, made the first time it is asked for with these
    /// limits -- 0 and 0 for the defaults -- and as it was made every time
    /// after: its handle, or 0 for a name or limits it refuses, or no room
    /// for another log.
    pub fn kernel_log_open(name: *const u8, len: usize, file_max: u64, total_max: u64) -> u64;
    /// A line onto the log's queue: `len` bytes of it, up to
    /// LOG_LINE_MAX, and `cut` LOG_CUT when it was longer. A handle that is
    /// no log's -- one refused -- has its line said in the kernel log
    /// instead.
    pub fn kernel_log_write(log: u64, line: *const u8, len: usize, cut: u32);
}
