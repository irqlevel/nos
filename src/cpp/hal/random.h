#pragma once

#include <include/types.h>

// The CPU's own random-number instruction, where the machine has one:
// RDRAND/RDSEED on x86-64, RNDR/RNDRRS on arm64 (FEAT_RNG). Backends live in
// arch/x86_64/hal_random.cpp and arch/arm64/hal_random.cpp.
//
// This is a source of entropy, not the kernel's generator: everything reads
// randomness from the pool (src/rust/random), which mixes this with the other
// sources and reaches it through rust_ffi.cpp. A CPU instruction is the only
// source the bare-metal machines this kernel boots on all share, which is why
// it gets a HAL seam of its own.
//
// Two functions because the hardware makes the distinction: Seed() asks for
// raw conditioned entropy (RDSEED, RNDRRS) and is the right thing to key a
// pool from, Random() asks the CPU's own whitening DRBG (RDRAND, RNDR) and is
// the cheap one. Both can legitimately fail -- the entropy behind them is
// finite and shared with every other core -- so both report it.

namespace Hal
{

/* Once, on the BSP, before the calls below: reads the capability bits and
   honors hwrng=off. Reading a random instruction the CPU does not implement
   is an undefined-instruction trap, so nothing here may run before it. */
void ProbeHwRandom();

/* Which instruction this CPU has, as the probe found it: what the pool names
   the source by. The values cross to Rust (kernel_hw_random_kind). */
enum class HwRandomKind : u32
{
    None = 0,
    RdRand = 1,     /* x86-64, RDRAND alone */
    RdSeed = 2,     /* x86-64, RDSEED and RDRAND */
    Rndr = 3,       /* arm64, RNDR and RNDRRS */
};

HwRandomKind GetHwRandomKind();

/* Whitened output; false if the CPU would not produce one. */
bool HwRandom(u64& out);

/* Raw conditioned entropy where the CPU has a separate instruction for it,
   otherwise the whitened output. */
bool HwRandomSeed(u64& out);

}
