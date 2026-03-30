extern "C" {
    pub fn kernel_mutex_create() -> usize;
    pub fn kernel_mutex_destroy(handle: usize);
    pub fn kernel_mutex_lock(handle: usize);
    pub fn kernel_mutex_unlock(handle: usize);
    pub fn kernel_spinlock_create() -> usize;
    pub fn kernel_spinlock_destroy(handle: usize);
    pub fn kernel_spinlock_lock(handle: usize) -> u64;
    pub fn kernel_spinlock_unlock(handle: usize, flags: u64);
    pub fn kernel_rw_spinlock_create() -> usize;
    pub fn kernel_rw_spinlock_destroy(handle: usize);
    pub fn kernel_rw_spinlock_read_lock(handle: usize);
    pub fn kernel_rw_spinlock_read_unlock(handle: usize);
    pub fn kernel_rw_spinlock_write_lock(handle: usize) -> u64;
    pub fn kernel_rw_spinlock_write_unlock(handle: usize, flags: u64);
    pub fn kernel_rw_mutex_create() -> usize;
    pub fn kernel_rw_mutex_destroy(handle: usize);
    pub fn kernel_rw_mutex_read_lock(handle: usize);
    pub fn kernel_rw_mutex_read_unlock(handle: usize);
    pub fn kernel_rw_mutex_write_lock(handle: usize);
    pub fn kernel_rw_mutex_write_unlock(handle: usize);
    pub fn kernel_waitgroup_create() -> usize;
    pub fn kernel_waitgroup_destroy(handle: usize);
    pub fn kernel_waitgroup_add(handle: usize, delta: isize);
    pub fn kernel_waitgroup_done(handle: usize);
    pub fn kernel_waitgroup_wait(handle: usize);
}
