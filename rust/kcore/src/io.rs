use core::marker::PhantomData;
use ffi::io;

pub trait PortWidth: Copy {
    fn read_port(port: u16) -> Self;
    fn write_port(port: u16, val: Self);
}

impl PortWidth for u8 {
    fn read_port(port: u16) -> Self {
        unsafe { io::Inb(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::Outb(port, val) }
    }
}

impl PortWidth for u16 {
    fn read_port(port: u16) -> Self {
        unsafe { io::Inw(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::Outw(port, val) }
    }
}

impl PortWidth for u32 {
    fn read_port(port: u16) -> Self {
        unsafe { io::In(port) }
    }

    fn write_port(port: u16, val: Self) {
        unsafe { io::Out(port, val) }
    }
}

pub struct Port<T: PortWidth> {
    port: u16,
    _p: PhantomData<T>,
}

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

pub unsafe fn read_msr(msr: u32) -> u64 {
    io::ReadMsr(msr)
}

pub unsafe fn write_msr(msr: u32, value: u64) {
    io::WriteMsr(msr, value);
}

pub struct MmioRegion {
    base: *mut u8,
    size: usize,
}

impl MmioRegion {
    pub fn new(base: *mut u8, size: usize) -> Self {
        Self { base, size }
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
