//! The CPU's vector registers, for the code that runs vector instructions:
//! the crypto crate's CRYPTOGAMS assembly. Nothing else in the kernel is
//! built to use them -- its C++ without SSE, its Rust soft-float -- and no
//! context switch saves them, so they are lent a [`Section`] at a time:
//! preemption off while it is open, one open on a CPU at a time, every
//! register zeroed when it closes, and a guest's registers on that CPU
//! marked gone so that hv puts them back at the guest's next entry
//! (kernel/simd.h).

use core::marker::PhantomData;

/// What a section may use, as [`features`] answers -- hal/simd.h's bits:
/// x86-64's SSE2, SSSE3, AVX and AVX2, arm64's Advanced SIMD.
pub const SSE2: u32 = 1 << 0;
pub const SSSE3: u32 = 1 << 1;
pub const AVX: u32 = 1 << 2;
pub const AVX2: u32 = 1 << 3;
pub const NEON: u32 = 1 << 4;

/// What a section may use on every CPU: the bits above, 0 where there is
/// no vector unit to lend.
#[inline]
pub fn features() -> u32 {
    ffi::simd::kernel_simd_features()
}

/// The vector registers of the CPU this runs on, the holder's until it is
/// dropped. Preemption is off for as long as it lives, so nothing but the
/// holder runs on that CPU but interrupt handlers -- and one of those that
/// asks for a section of its own is refused one while this lives. Not
/// `Send`: it is closed by the task, and on the CPU, that opened it.
pub struct Section {
    _not_send: PhantomData<*const ()>,
}

impl Section {
    /// A section on the CPU this runs on; None where none can be had --
    /// one open there already, the code an interrupt cut into holding it,
    /// or no vector unit -- and the caller runs code that uses none.
    #[inline]
    pub fn begin() -> Option<Self> {
        if ffi::simd::kernel_simd_begin() != 0 {
            Some(Self { _not_send: PhantomData })
        } else {
            None
        }
    }
}

impl Drop for Section {
    #[inline]
    fn drop(&mut self) {
        /* The section `kernel_simd_begin` opened for this guard: made by
         * `begin` alone, when it answered 1; neither `Copy` nor `Send`;
         * dropped once, on the task that opened it -- and on its CPU,
         * preemption being off until here. */
        unsafe { ffi::simd::kernel_simd_end() }
    }
}

/// Whose x87, SSE and AVX registers a CPU holds: a guest's, by the number
/// hvarch's `GuestFp` gives each, as its last exit there left them -- or
/// nobody's, as a section leaves them. hv skips putting a guest's registers
/// back at an entry on a CPU that holds them still.
pub mod guest {
    /// Whether CPU `cpu` holds guest `id`'s registers. Asked at an entry,
    /// with interrupts off on `cpu`, before the guest's are put there.
    #[inline]
    pub fn registers_are(cpu: u32, id: u64) -> bool {
        ffi::simd::kernel_simd_guest_is(cpu, id) != 0
    }

    /// Guest `id`'s registers have just been put into CPU `cpu`: they are
    /// its there, and on no other CPU any more.
    ///
    /// # Safety
    /// They are: called by the entry that put them there, on `cpu`, with
    /// interrupts off since the putting. A mark that lies is a guest that
    /// resumes with another's registers.
    #[inline]
    pub unsafe fn registers_loaded(cpu: u32, id: u64) {
        unsafe { ffi::simd::kernel_simd_guest_loaded(cpu, id) }
    }

    /// CPU `cpu`'s registers are nobody's: its next entry puts its guest's
    /// back whoever's they were.
    #[inline]
    pub fn registers_lost(cpu: u32) {
        ffi::simd::kernel_simd_guest_lost(cpu)
    }
}
