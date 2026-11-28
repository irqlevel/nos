use core::ptr::NonNull;

use kcore::dma::DmaBuffer;

use crate::pci::PciIo;

pub const PAGE: usize = 4096;

#[derive(Clone, Copy)]
pub struct Mem {
    host: NonNull<u8>,
    len: usize,
    pub dev: u64,
}

/* Every view is into a DmaBuffer retained by Hca for at least as long as the
 * device can use it. Accessors are volatile because the device shares it. */
unsafe impl Send for Mem {}
unsafe impl Sync for Mem {}

pub struct DmaBuf {
    mem: Mem,
    _buf: DmaBuffer,
}

impl core::ops::Deref for DmaBuf {
    type Target = Mem;

    fn deref(&self) -> &Mem {
        &self.mem
    }
}

impl DmaBuf {
    pub fn new(_pci: &mut PciIo, bytes: usize) -> Result<Self, crate::pci::Error> {
        let pages = bytes.div_ceil(PAGE).max(1);
        let mut buf = DmaBuffer::new(pages).ok_or(crate::pci::Error)?;
        if buf.pages() < pages {
            return Err(crate::pci::Error);
        }
        buf.as_mut_slice().fill(0);
        let mem = Mem {
            host: NonNull::new(buf.as_mut_ptr()).ok_or(crate::pci::Error)?,
            len: buf.len(),
            dev: buf.phys(),
        };
        Ok(Self { mem, _buf: buf })
    }

    pub fn mem(&self) -> Mem {
        self.mem
    }

    /// # Safety
    /// The device must have finished with the buffer or have been reset.
    pub unsafe fn free(self, _pci: &mut PciIo) {
        drop(self);
    }
}

impl Mem {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn zero(&self) {
        self.fill(0);
    }

    pub fn fill(&self, b: u8) {
        unsafe { self.host.as_ptr().write_bytes(b, self.len) };
    }

    fn at<T>(&self, off: usize) -> *mut T {
        assert!(off.checked_add(core::mem::size_of::<T>()).is_some_and(|end| end <= self.len));
        unsafe { self.host.as_ptr().add(off).cast() }
    }

    pub fn u8(&self, off: usize) -> u8 {
        unsafe { self.at::<u8>(off).read_volatile() }
    }

    pub fn be16(&self, off: usize) -> u16 {
        u16::from_be_bytes([self.u8(off), self.u8(off + 1)])
    }

    pub fn be32(&self, off: usize) -> u32 {
        (u32::from(self.be16(off)) << 16) | u32::from(self.be16(off + 2))
    }

    pub fn be64(&self, off: usize) -> u64 {
        (u64::from(self.be32(off)) << 32) | u64::from(self.be32(off + 4))
    }

    pub fn set_u8(&self, off: usize, v: u8) {
        unsafe { self.at::<u8>(off).write_volatile(v) }
    }

    pub fn set_be16(&self, off: usize, v: u16) {
        self.write(off, &v.to_be_bytes());
    }

    pub fn set_be32(&self, off: usize, v: u32) {
        self.write(off, &v.to_be_bytes());
    }

    pub fn set_be64(&self, off: usize, v: u64) {
        self.set_be32(off, (v >> 32) as u32);
        self.set_be32(off + 4, v as u32);
    }

    pub fn write(&self, off: usize, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.set_u8(off + i, b);
        }
    }

    pub fn read(&self, off: usize, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.u8(off + i);
        }
    }

    pub fn slice(&self, off: usize, len: usize) -> Mem {
        assert!(off.checked_add(len).is_some_and(|end| end <= self.len));
        Mem { host: unsafe { self.host.add(off) }, len, dev: self.dev + off as u64 }
    }
}

#[derive(Clone, Copy)]
pub struct Block {
    pub virt: u64,
    pub phys: u64,
    pub log: u32,
}

pub fn blocks(virt: Option<u64>, phys: u64, size: u64) -> impl Iterator<Item = Block> {
    let mut off = 0u64;
    core::iter::from_fn(move || {
        if off >= size {
            return None;
        }
        let p = phys + off;
        let v = virt.map(|v| v + off);
        let align = (p | v.unwrap_or(0) | (1 << 63)).trailing_zeros();
        let left = size - off;
        let log = align.min(63 - left.leading_zeros());
        off += 1 << log;
        Some(Block { virt: v.unwrap_or(0), phys: p, log })
    })
}
