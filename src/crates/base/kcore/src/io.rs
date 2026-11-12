#[cfg(target_arch = "x86_64")]
use core::marker::PhantomData;
#[cfg(target_arch = "x86_64")]
use ffi::io;

#[cfg(target_arch = "x86_64")]
pub trait PortWidth: Copy {
    fn read_port(port: u16) -> Self;
    fn write_port(port: u16, val: Self);
}

#[cfg(target_arch = "x86_64")]
impl PortWidth for u8 {
    fn read_port(port: u16) -> Self {
        unsafe { io::inb(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::outb(port, val) }
    }
}

#[cfg(target_arch = "x86_64")]
impl PortWidth for u16 {
    fn read_port(port: u16) -> Self {
        unsafe { io::inw(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::outw(port, val) }
    }
}

#[cfg(target_arch = "x86_64")]
impl PortWidth for u32 {
    fn read_port(port: u16) -> Self {
        unsafe { io::inl(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::outl(port, val) }
    }
}

#[cfg(target_arch = "x86_64")]
pub struct Port<T: PortWidth> {
    port: u16,
    _p: PhantomData<T>,
}

#[cfg(target_arch = "x86_64")]
impl<T: PortWidth> Port<T> {
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            _p: PhantomData,
        }
    }

    pub fn read(&self) -> T {
        T::read_port(self.port)
    }

    pub fn write(&self, val: T) {
        T::write_port(self.port, val);
    }
}

pub struct MmioRegion {
    base: *mut u8,
    size: usize,
}

/* A window of device registers. Every access is a volatile read or write
 * through `&self`, which is as much as the hardware itself promises of two
 * CPUs at once: what has to be one at a time -- an index register and its
 * data -- is the driver's to keep so, with a lock, as it would be anyway. */
unsafe impl Send for MmioRegion {}
unsafe impl Sync for MmioRegion {}

impl MmioRegion {
    pub fn new(base: *mut u8, size: usize) -> Self {
        Self { base, size }
    }

    /// The part of this window from `offset` on, as a window of its own: a
    /// block of registers that starts where a capability register says it
    /// does. It ends where this one ends, so an access past the mapping is
    /// still an assert rather than a wild read -- and one the new window's
    /// start leaves misaligned, too (`at`). None when `offset` is not in the
    /// window at all.
    pub fn window(&self, offset: usize) -> Option<MmioRegion> {
        if offset >= self.size {
            return None;
        }
        Some(MmioRegion { base: self.base.wrapping_add(offset), size: self.size - offset })
    }

    /// Where an access of a `T` at `offset` goes, once it is known to be
    /// one the window can take: every byte of it inside -- the end summed
    /// without wrapping, as an offset near the top of the address space
    /// once wrapped round to pass and reach before the mapping -- and the
    /// address itself aligned for a `T`, not the offset alone, as a window
    /// may start anywhere (`window`, a capability register's value). A
    /// misaligned volatile access is undefined behaviour, and a fault on
    /// arm64's device memory.
    #[inline]
    fn at<T>(&self, offset: usize) -> *mut T {
        let width = core::mem::size_of::<T>();
        assert!(offset.checked_add(width).is_some_and(|end| end <= self.size),
            "mmio: {} bytes at {:#x}, past a window of {:#x}", width, offset, self.size);
        let p = self.base.wrapping_add(offset).cast::<T>();
        assert!(p.is_aligned(), "mmio: {} bytes at {:#x}, misaligned at {:p}", width, offset, p);
        p
    }

    /* Each access: `at` has put the whole of it inside the window, which
     * `new`'s caller mapped as device memory, at an address aligned for its
     * width. */

    pub fn read8(&self, offset: usize) -> u8 {
        unsafe { self.at::<u8>(offset).read_volatile() }
    }

    pub fn write8(&self, offset: usize, val: u8) {
        unsafe { self.at::<u8>(offset).write_volatile(val) }
    }

    pub fn read16(&self, offset: usize) -> u16 {
        unsafe { self.at::<u16>(offset).read_volatile() }
    }

    pub fn write16(&self, offset: usize, val: u16) {
        unsafe { self.at::<u16>(offset).write_volatile(val) }
    }

    pub fn read32(&self, offset: usize) -> u32 {
        unsafe { self.at::<u32>(offset).read_volatile() }
    }

    pub fn write32(&self, offset: usize, val: u32) {
        unsafe { self.at::<u32>(offset).write_volatile(val) }
    }

    pub fn read64(&self, offset: usize) -> u64 {
        unsafe { self.at::<u64>(offset).read_volatile() }
    }

    pub fn write64(&self, offset: usize, val: u64) {
        unsafe { self.at::<u64>(offset).write_volatile(val) }
    }
}
