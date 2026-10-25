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
    /// a page, on a page that is on the free list already, and on a page of
    /// the huge pool.
    pub fn kernel_frame_free(phys: u64);
    /// A block of the huge pool (`hugepages=`): 512 pages, physically
    /// contiguous and aligned to their 2 MiB, zeroed -- its physical
    /// address, or 0 when the pool has none free or there is no pool. Sound
    /// anywhere, as `kernel_frame_alloc` is.
    pub safe fn kernel_frame_alloc_huge() -> u64;
    /// A block back into the pool. The kernel panics on an address that is
    /// no block of the pool, and on a block that is back already.
    pub fn kernel_frame_free_huge(phys: u64);
    /// How many blocks the pool has -- 0 with no `hugepages=` -- and how
    /// many of them are free. Values only.
    pub safe fn kernel_frame_huge_blocks() -> u64;
    pub safe fn kernel_frame_huge_free() -> u64;
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
    /// The `count` frames at `phys`, four at most, mapped into the kernel's
    /// space together in that order, read-write and never executable: their
    /// address, or 0 when one is no frame or there is no address left. The
    /// frames must stay allocated until `kernel_frames_unmap` has run.
    pub fn kernel_frames_map(phys: *const u64, count: usize) -> usize;
    /// A `kernel_frames_map` undone, `va` and `count` as it returned and was
    /// given: shot down on every CPU, waiting for each -- never with a spin
    /// lock held.
    pub fn kernel_frames_unmap(va: usize, count: usize);
}
