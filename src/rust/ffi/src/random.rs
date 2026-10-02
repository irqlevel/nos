/// A draw from the CPU's random instruction (hal/random.h): `value`, when
/// `ok` is 1.
#[repr(C)]
pub struct HwRandomDraw {
    pub value: u64,
    pub ok: u64,
}

unsafe extern "C" {
    /// `len` random bytes from the kernel's pool (src/rust/random) at `buf`:
    /// 1, or 0 -- and nothing written -- while nothing has seeded it.
    pub fn kernel_get_random(buf: *mut u8, len: usize) -> i32;

    /// Which random instruction the CPU has, as the boot's probe found it:
    /// 0 none, 1 RDRAND, 2 RDSEED and RDRAND, 3 RNDR and RNDRRS.
    pub safe fn kernel_hw_random_kind() -> u32;

    /// The instruction's whitened output (RDRAND, RNDR). A CPU with none, or
    /// one that would not give a value, answers `ok` 0 -- never a fault.
    pub safe fn kernel_hw_random() -> HwRandomDraw;

    /// Its raw conditioned entropy where it has an instruction for that
    /// (RDSEED, RNDRRS), the whitened output where it has not; as
    /// `kernel_hw_random` when there is none.
    pub safe fn kernel_hw_random_seed() -> HwRandomDraw;
}
