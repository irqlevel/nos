use crate::consts::PAGE_SIZE;
use ffi::dma;

pub struct DmaBuffer {
    ptr: *mut u8,
    phys: u64,
    pages: usize,
}

/* Pages the buffer owns outright, as a `Box` owns its allocation: it may go
 * to another CPU, and what a shared reference gives -- `as_slice` -- is
 * reading. */
unsafe impl Send for DmaBuffer {}
unsafe impl Sync for DmaBuffer {}

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

    /// `len` bytes of the buffer from `offset`, and no more of it: what a
    /// driver that keeps several things in one page -- a header per request,
    /// a status byte per request -- takes to look at one of them, without
    /// laying claim to the rest of the page while the device is at work in
    /// it. None when that is not inside the buffer.
    pub fn bytes(&self, offset: usize, len: usize) -> Option<&[u8]> {
        if offset.checked_add(len)? > self.len() {
            return None;
        }
        /* Inside the allocation, by the check above. */
        Some(unsafe { core::slice::from_raw_parts(self.ptr.add(offset), len) })
    }

    pub fn bytes_mut(&mut self, offset: usize, len: usize) -> Option<&mut [u8]> {
        if offset.checked_add(len)? > self.len() {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts_mut(self.ptr.add(offset), len) })
    }

    /// Whether a `T` lies whole and aligned at `offset`.
    fn holds<T>(&self, offset: usize) -> bool {
        match offset.checked_add(core::mem::size_of::<T>()) {
            Some(end) if end <= self.len() => {
                (self.ptr as usize + offset) % core::mem::align_of::<T>() == 0
            }
            _ => false,
        }
    }

    /// The `T` at `offset`, as it is this instant: a volatile read, for
    /// memory a device writes while the driver looks -- a descriptor's
    /// status, a completion entry. What comes back is whatever was there; a
    /// driver decides what it means by the order it reads things in, and a
    /// `kcore::barrier` between them where the order matters. None when no
    /// `T` lies there.
    pub fn load<T: crate::pod::Pod>(&self, offset: usize) -> Option<T> {
        if !self.holds::<T>(offset) {
            return None;
        }
        /* Inside the allocation and aligned, by the check; any bytes are a
         * `T`. */
        Some(unsafe { (self.ptr.add(offset) as *const T).read_volatile() })
    }

    /// Put `value` at `offset` with a volatile write: a descriptor, a
    /// submission entry -- something a device will read, and whose last word
    /// may be what hands it over. False, and nothing written, when no `T`
    /// lies there.
    pub fn store<T: crate::pod::Pod>(&mut self, offset: usize, value: T) -> bool {
        if !self.holds::<T>(offset) {
            return false;
        }
        unsafe { (self.ptr.add(offset) as *mut T).write_volatile(value) };
        true
    }

    /// The buffer as one `T`, from its first byte: a control block the
    /// hardware reads and writes only inside an instruction the owner
    /// executes -- an SVM VMCB, which `vmrun` takes and `#vmexit` hands back
    /// before the next instruction -- and never behind the owner's back, the
    /// way a device works a descriptor ring. What that asks of the caller is
    /// what [`as_mut_slice`](Self::as_mut_slice) asks: that nothing else
    /// writes the pages while the reference is held. None when a `T` does
    /// not fit, or would not be aligned.
    pub fn as_pod<T: crate::pod::Pod>(&self) -> Option<&T> {
        if !self.holds::<T>(0) {
            return None;
        }
        /* Inside the allocation and aligned, by the check; any bytes are a
         * `T`; and the borrow of `self` is the borrow of the pages. */
        Some(unsafe { &*(self.ptr as *const T) })
    }

    pub fn as_pod_mut<T: crate::pod::Pod>(&mut self) -> Option<&mut T> {
        if !self.holds::<T>(0) {
            return None;
        }
        /* As above, and `&mut self` makes it the only reference. */
        Some(unsafe { &mut *(self.ptr as *mut T) })
    }

    /// Raw pointer access for buffers that a device writes concurrently
    /// (descriptor rings, completion queues).  Forming a `&`/`&mut` slice
    /// over such memory is unsound; use these with volatile reads/writes.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
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

/// One word of memory a device shares with the driver: a field of a
/// descriptor. Every access is volatile -- it happens where it is written,
/// once, and what is read is whatever is there -- and goes through `&self`,
/// because a descriptor is shared by construction: the device writes it while
/// the driver looks. What order things happen in is the driver's to say, by
/// the order it writes them in and a `kcore::barrier` where the device has to
/// see one thing before another.
#[repr(transparent)]
pub struct Volatile<T: crate::pod::Pod>(core::cell::UnsafeCell<T>);

/* As a register is: what two CPUs at once make of one word is what the
 * hardware makes of it, and a driver that needs more than that holds a lock,
 * or is handed the ring by whoever serialises its calls. */
unsafe impl<T: crate::pod::Pod> Sync for Volatile<T> {}
unsafe impl<T: crate::pod::Pod> Send for Volatile<T> {}

impl<T: crate::pod::Pod> Volatile<T> {
    #[inline]
    pub fn read(&self) -> T {
        unsafe { self.0.get().read_volatile() }
    }

    #[inline]
    pub fn write(&self, value: T) {
        unsafe { self.0.get().write_volatile(value) }
    }
}

/// A descriptor: what a ring shared with a device is a table of.
///
/// # Safety
/// The type is `#[repr(C)]` and every field of it is a `Volatile` of an
/// integer, so that any bytes make one and nothing about one is assumed to
/// stay put.
pub unsafe trait Descriptor: Sized + Sync + 'static {}

impl DmaBuffer {
    /// The buffer, zeroed and for good, as a table of descriptors: what a
    /// ring's base register is pointed at. For good, because a device that
    /// has been given an address keeps it; shared, because that is what a
    /// ring is -- the driver's two halves, the device, and whoever dumps its
    /// state all look at the same table. Its physical address goes with it.
    pub fn leak_ring<D: Descriptor>(mut self) -> (&'static [D], u64) {
        self.as_mut_slice().fill(0);

        let phys = self.phys;
        let count = match core::mem::size_of::<D>() {
            0 => 0,
            size if core::mem::align_of::<D>() <= PAGE_SIZE => self.len() / size,
            _ => 0,
        };
        let base = self.ptr as *const D;
        core::mem::forget(self);

        /* Page-aligned, `count` whole descriptors inside the allocation,
         * never freed -- and a `D` is made of cells, so sharing it says
         * nothing about what is in it. */
        (unsafe { core::slice::from_raw_parts(base, count) }, phys)
    }
}

pub struct PhysMapping {
    ptr: *mut u8,
    pages: usize,
}

/* A mapping of device memory, owned: it may go to another CPU, and what a
 * shared reference gives is its address. */
unsafe impl Send for PhysMapping {}
unsafe impl Sync for PhysMapping {}

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

pub fn virt_to_phys(ptr: *const u8) -> u64 {
    unsafe { dma::kernel_virt_to_phys(ptr) }
}
