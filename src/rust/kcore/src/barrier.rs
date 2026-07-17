//! DMA memory barriers, the Rust half of the C++ `Hal::DmaWmb`/`Hal::DmaRmb`
//! contract (src/cpp/hal/barrier.h): compiler-only on x86 (TSO + coherent
//! DMA), `dmb oshst`/`dmb oshld` on arm64. `core::sync::atomic::fence`
//! is NOT a substitute there — it lowers to `dmb ish`, whose inner-shareable
//! domain does not order stores/loads against a PCIe master.

/// Order CPU stores (DMA descriptors, ring entries) before a subsequent
/// store the device observes (doorbell write, OWN bit).
#[inline(always)]
pub fn dma_wmb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dmb oshst", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// Order a load of device-written state (phase bit, OWN bit, used index)
/// before subsequent loads of the payload it guards.
#[inline(always)]
pub fn dma_rmb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dmb oshld", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
