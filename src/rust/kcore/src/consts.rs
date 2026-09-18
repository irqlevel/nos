pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;
pub const SECTOR_SIZE: usize = 512;
pub const KB: usize = 1024;
pub const MB: usize = 1024 * 1024;
pub const GB: usize = 1024 * 1024 * 1024;
pub const NS_PER_SEC: u64 = 1_000_000_000;
pub const NS_PER_MS: u64 = 1_000_000;
pub const NS_PER_US: u64 = 1_000;

/// The most CPUs the kernel tracks, as `Kernel::MaxCpus` has it: what a
/// per-CPU array in a driver or a server is sized by.
pub const MAX_CPUS: usize = 64;
