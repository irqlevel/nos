/// One asynchronous I/O straight to or from physical memory: what a caller
/// that must not block hands a device. The zero-copy block server (the netblk
/// module) has the disk DMA a read into the frame it is about to transmit,
/// and a write out of the frame it received.
#[repr(C)]
pub struct BlockIo {
    /// 0 read, 1 write, 2 flush
    pub op: u8,
    pub fua: u8,
    pub reserved: u16,
    /// Sectors; 0 for a flush.
    pub count: u32,
    pub sector: u64,
    /// Physically contiguous, dword aligned.
    pub phys: u64,
    /// Called exactly once, from interrupt context: 0, or the device's error.
    pub done: extern "C" fn(ctx: *mut u8, status: i32),
    pub ctx: *mut u8,
}

/// A request a device's queue is done with (`kernel_blockq_reap`): its
/// buffer, and 1 when it went well.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct QueueDone {
    pub buffer: u32,
    pub ok: u32,
}

/* A block device already in the kernel's table -- a disk, or a partition of
   one -- read and written by name: the block layer as a loadable module
   reaches it (src/crates/layers/block/src/table.rs defines these). A module is linked
   on its own, so a C ABI is the only seam it and the layer can share; code
   inside the kernel image calls the layer itself, and a driver registers
   with it as a `block::BlockDriver`. */
unsafe extern "C" {
    /// The device `disks` lists under this name, or 0. Devices live as long
    /// as the kernel does: there is nothing to release.
    pub fn kernel_blockdev_find(name: *const u8, name_len: usize) -> usize;

    /// Its size, in sectors
    pub safe fn kernel_blockdev_capacity(handle: usize) -> u64;

    pub safe fn kernel_blockdev_sector_size(handle: usize) -> u64;

    /// Synchronous, count in sectors: 0 once the data is in buf.
    pub fn kernel_blockdev_read(handle: usize, sector: u64, buf: *mut u8, count: u32) -> i32;

    /// Synchronous, count in sectors: 0 once the device has the data.
    pub fn kernel_blockdev_write(
        handle: usize, sector: u64, buf: *const u8, count: u32, fua: i32,
    ) -> i32;

    pub safe fn kernel_blockdev_flush(handle: usize) -> i32;

    /// Claims the device against mounts, the disk log and other writers for
    /// the holder the `name_len` bytes at `name` name -- copied, cut at
    /// HOLDER_MAX bytes; "a module writing to it" for none: a claim for
    /// kernel_blockdev_release, or 0 -- with who holds it copied into the
    /// `cap` bytes at `refused` and its length into *refused_len -- while
    /// one of those is on it, on the disk it is a partition of, or on a
    /// partition of it.
    pub fn kernel_blockdev_claim(
        handle: usize, name: *const u8, name_len: usize, refused: *mut u8, cap: usize, refused_len: *mut usize,
    ) -> usize;

    pub safe fn kernel_blockdev_release(claim: usize);

    /// How many partitions of the device the kernel found.
    pub safe fn kernel_blockdev_partitions(handle: usize) -> u32;

    /// 1 if the device has the asynchronous path: NVMe and its partitions.
    pub safe fn kernel_blockdev_can_submit(handle: usize) -> i32;

    /// Never blocks. 0 once submitted -- io.done is then called exactly
    /// once, from interrupt context -- 1 when the device has no room right
    /// now, 2 for an io it cannot take, 3 when it has no asynchronous path.
    /// io is read before this returns. kick 0 leaves the doorbell for
    /// kernel_blockdev_kick.
    pub fn kernel_blockdev_submit(handle: usize, io: *const BlockIo, kick: i32) -> i32;

    pub safe fn kernel_blockdev_kick(handle: usize);

    /// How many devices the table holds, and the index'th one's handle or
    /// 0: the table only grows, so an index once valid stays so.
    pub safe fn kernel_blockdev_count() -> u32;
    pub safe fn kernel_blockdev_at(index: u32) -> usize;

    /// The disk a partition is on, or 0 for a whole disk; the sector of it
    /// the partition starts at, u64::MAX for a whole disk.
    pub safe fn kernel_blockdev_parent(handle: usize) -> usize;
    pub safe fn kernel_blockdev_start(handle: usize) -> u64;

    /// The device's name -- what it is found by again at the next boot
    /// (`part:...`, `disk:...`) -- who holds a claim on it: into the `cap`
    /// bytes at `out`, as much as fits, no NUL; how long it is, 0 for none.
    pub fn kernel_blockdev_name(handle: usize, out: *mut u8, cap: usize) -> usize;
    pub fn kernel_blockdev_ident(handle: usize, out: *mut u8, cap: usize) -> usize;
    pub fn kernel_blockdev_holder(handle: usize, out: *mut u8, cap: usize) -> usize;

    /// Who holds a claim on the device or anything overlapping it -- what a
    /// claim of it would be refused for -- as kernel_blockdev_holder's;
    /// nothing is claimed by asking.
    pub fn kernel_blockdev_in_the_way(handle: usize, out: *mut u8, cap: usize) -> usize;

    /// The device whose id is the `len` bytes at `id`, or 0 -- with *count
    /// set to how many have it: none, or several, none taken for it.
    pub fn kernel_blockdev_find_ident(id: *const u8, len: usize, count: *mut u32) -> usize;

    /// What the partition table says the device is, into the 16 bytes at
    /// `out`: 1 a GPT type GUID, 2 an MBR type byte (the first), 0 a whole
    /// disk, -1 no device.
    pub fn kernel_blockdev_kind(handle: usize, out: *mut u8) -> i32;

    /// 1 when writing to one of the two can touch the other.
    pub safe fn kernel_blockdev_overlaps(a: usize, b: usize) -> i32;

    /// 1 when the device gives sectors back; and so its `count` sectors
    /// from `sector` given back -- inside it, or refused: 0 once it has been
    /// told, -1 otherwise. A writer's call, under its claim.
    pub safe fn kernel_blockdev_can_discard(handle: usize) -> i32;
    pub safe fn kernel_blockdev_discard(handle: usize, sector: u64, count: u64) -> i32;

    /// A queue on the device: `buffers` buffers of `bytes` each, a whole
    /// number of pages, their addresses into `out` -- the device claimed for
    /// the holder the `name_len` bytes at `name` name, for writing with
    /// `write` not 0: the queue's word, or 0 -- with who is in the way
    /// copied into the `cap` bytes at `refused` and its length into
    /// *refused_len when that is why (`layers/block/src/devq.rs`). `event`, unless
    /// 0, is a task's event (kernel_shared_event_open) for the queue to be
    /// made on -- 0 returned for one that is no event's: its completions and
    /// wakes signal that event too, which the queue holds until it is closed.
    pub fn kernel_blockq_open(
        device: usize, write: i32, buffers: usize, bytes: usize, name: *const u8, name_len: usize,
        out: *mut *mut u8, refused: *mut u8, cap: usize, refused_len: *mut usize, event: u64,
    ) -> usize;

    /// Buffer `buffer`'s request -- `len` bytes from byte `offset` of the
    /// device read into it, or written from it -- started: 1 while the
    /// device has it, 0 done now, -1 done and failed. The buffer is the
    /// device's until kernel_blockq_reap gives it back: the caller keeps off
    /// it. Not a `safe fn`: the buffers are lent to the caller as slices
    /// (kcore's `Lent`), and the caller is to be the queue's one owner and
    /// hold no slice of `buffer`.
    pub fn kernel_blockq_start(q: usize, buffer: usize, write: i32, offset: u64, len: usize) -> i32;
    pub safe fn kernel_blockq_kick(q: usize);

    /// Requests done, `max` at most, into `out`: how many, -1 for no queue.
    pub fn kernel_blockq_reap(q: usize, out: *mut QueueDone, max: usize) -> isize;

    /// A wait for a request done or a wake, up to `timeout_ns`; the same
    /// spun for; a wake, from any context: 1 when one came, 0 when the time
    /// ran out, -1 for no queue.
    pub safe fn kernel_blockq_wait(q: usize, timeout_ns: u64) -> i32;
    pub safe fn kernel_blockq_poll(q: usize, poll_ns: u64) -> i32;
    pub safe fn kernel_blockq_wake(q: usize);

    /// The device's write cache pushed out; `len` bytes of it from byte
    /// `offset` given back -- for a queue opened for writing: 0, or -1.
    pub safe fn kernel_blockq_flush(q: usize) -> i32;
    pub safe fn kernel_blockq_discard(q: usize, offset: u64, len: u64) -> i32;

    /// The queue closed: what the device still has waited for, its buffers
    /// freed, its claim given back. The buffers' addresses name nothing
    /// after. Not a `safe fn`: the caller is the queue's one owner and holds
    /// no slice of its buffers, which are freed here.
    pub fn kernel_blockq_close(q: usize);
}
