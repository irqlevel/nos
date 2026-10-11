/* Frames: pages of RAM handed out by physical address and mapped nowhere --
   not into the kernel's address space, nor into any page table but whatever
   their owner puts them in. What a guest's memory is made of: its nested
   page table is the only mapping of it there is. */
unsafe extern "C" {
    /// A zeroed page off the free list: its physical address, 0 when there
    /// is none past the reserve the kernel keeps for itself (a sixteenth of
    /// the machine, at least 64 MiB). Sound anywhere -- a frame nobody frees
    /// is a leak, not undefined behaviour.
    pub safe fn kernel_frame_alloc() -> u64;
    /// Back onto the free list. The kernel panics on an address that is not
    /// a page, and on a page that is on the free list already.
    pub fn kernel_frame_free(phys: u64);
    /// Copy `len` bytes out of the frame at `offset` into `buf`: 0, or -1
    /// with nothing copied when that is not inside one page of RAM.
    pub fn kernel_frame_read(phys: u64, offset: usize, buf: *mut u8, len: usize) -> i32;
    /// Copy `len` bytes from `data` into the frame at `offset`: 0, or -1
    /// with nothing copied, as `kernel_frame_read`.
    pub fn kernel_frame_write(phys: u64, offset: usize, data: *const u8, len: usize) -> i32;
    /// The 16-bit word at `offset` in the frame, loaded as one access with
    /// acquire order into `value`: 0, or -1 with nothing loaded when that is
    /// not an aligned word inside one page of RAM.
    pub fn kernel_frame_load16(phys: u64, offset: usize, value: *mut u16) -> i32;
    /// `value` stored at `offset` in the frame as one access with release
    /// order: 0, or -1 with nothing stored, as `kernel_frame_load16`.
    pub fn kernel_frame_store16(phys: u64, offset: usize, value: u16) -> i32;
}
