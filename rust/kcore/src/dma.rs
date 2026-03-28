use crate::consts::PAGE_SIZE;
use ffi::dma;

pub struct DmaBuffer {
    ptr: *mut u8,
    phys: u64,
    pages: usize,
}

impl DmaBuffer {
    pub fn new(requested_pages: usize) -> Option<Self> {
        if requested_pages == 0 {
            return None;
        }
        let mut phys: u64 = 0;
        let mut actual: usize = 0;
        let p = unsafe {
            dma::kernel_alloc_dma_pages(requested_pages, &mut phys, &mut actual)
        };
        if p.is_null() {
            None
        } else {
            Some(Self {
                ptr: p,
                phys,
                pages: actual,
            })
        }
    }

    pub fn phys(&self) -> u64 {
        self.phys
    }

    pub fn pages(&self) -> usize {
        self.pages
    }

    pub fn len(&self) -> usize {
        self.pages * PAGE_SIZE
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.len()) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len()) }
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                dma::kernel_free_dma_pages(self.ptr);
            }
            self.ptr = core::ptr::null_mut();
        }
    }
}

pub struct PhysMapping {
    ptr: *mut u8,
    pages: usize,
}

impl PhysMapping {
    pub fn map(phys_base: u64, num_pages: usize) -> Option<Self> {
        if num_pages == 0 {
            return None;
        }
        let p = unsafe { dma::kernel_map_phys(phys_base, num_pages) };
        if p.is_null() {
            None
        } else {
            Some(Self { ptr: p, pages: num_pages })
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.pages * PAGE_SIZE
    }
}

impl Drop for PhysMapping {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                dma::kernel_unmap_phys(self.ptr, self.pages);
            }
            self.ptr = core::ptr::null_mut();
        }
    }
}
