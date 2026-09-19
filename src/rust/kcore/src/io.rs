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
        unsafe { io::Inb(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::Outb(port, val) }
    }
}

#[cfg(target_arch = "x86_64")]
impl PortWidth for u16 {
    fn read_port(port: u16) -> Self {
        unsafe { io::Inw(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::Outw(port, val) }
    }
}

#[cfg(target_arch = "x86_64")]
impl PortWidth for u32 {
    fn read_port(port: u16) -> Self {
        unsafe { io::In(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::Out(port, val) }
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
    /// still an assert rather than a wild read. None when `offset` is not in
    /// the window at all.
    pub fn window(&self, offset: usize) -> Option<MmioRegion> {
        if offset >= self.size {
            return None;
        }
        Some(MmioRegion { base: self.base.wrapping_add(offset), size: self.size - offset })
    }

    pub fn read8(&self, offset: usize) -> u8 {
        assert!(offset < self.size);
        unsafe { self.base.add(offset).read_volatile() }
    }

    pub fn write8(&self, offset: usize, val: u8) {
        assert!(offset < self.size);
        unsafe { self.base.add(offset).write_volatile(val) }
    }

    pub fn read16(&self, offset: usize) -> u16 {
        assert!(offset % 2 == 0 && offset + 2 <= self.size);
        unsafe { (self.base.add(offset) as *const u16).read_volatile() }
    }

    pub fn write16(&self, offset: usize, val: u16) {
        assert!(offset % 2 == 0 && offset + 2 <= self.size);
        unsafe { (self.base.add(offset) as *mut u16).write_volatile(val) }
    }

    pub fn read32(&self, offset: usize) -> u32 {
        assert!(offset % 4 == 0 && offset + 4 <= self.size);
        unsafe { (self.base.add(offset) as *const u32).read_volatile() }
    }

    pub fn write32(&self, offset: usize, val: u32) {
        assert!(offset % 4 == 0 && offset + 4 <= self.size);
        unsafe {
            (self.base.add(offset) as *mut u32).write_volatile(val);
        }
    }

    pub fn read64(&self, offset: usize) -> u64 {
        assert!(offset % 8 == 0 && offset + 8 <= self.size);
        unsafe { (self.base.add(offset) as *const u64).read_volatile() }
    }

    pub fn write64(&self, offset: usize, val: u64) {
        assert!(offset % 8 == 0 && offset + 8 <= self.size);
        unsafe {
            (self.base.add(offset) as *mut u64).write_volatile(val);
        }
    }
}
