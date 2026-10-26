#include <hal/random.h>
#include <hal/cpu.h>

#include <arch/x86_64/cpuid.h>

#include <kernel/parameters.h>
#include <kernel/trace.h>

/* RDRAND and RDSEED: the x86-64 backend of hal/random.h.

   Every machine nos runs on has them -- the Skylake-U laptop, both Hetzner
   boxes, the i5-13500 desktop -- and a KVM guest sees them too, since neither
   instruction is trapped. That is what makes this the fix for a bare-metal
   box with no virtio-rng: on such a machine the CPU itself is the only
   hardware entropy source there is.

   The difference between the two instructions is where in the CPU's DRBG the
   value is taken from. RDSEED taps the conditioned output of the physical
   entropy source, which is what another generator wants for its seed, and can
   report itself empty when several cores draw at once. RDRAND is the AES-CTR
   DRBG downstream of that, which is fast and effectively never dry, but one
   more deterministic step away from the noise. */

namespace
{

/* CPUID.01H:ECX[30] = RDRAND, CPUID.(EAX=07H,ECX=0):EBX[18] = RDSEED */
const u32 CPUID_LEAF_FEATURES = 1;
const u32 CPUID_BIT_RDRAND = 1U << 30;
const u32 CPUID_LEAF_STRUCTURED_EXT = 7;
const u32 CPUID_BIT_RDSEED = 1U << 18;

/* Intel's own guidance for RDRAND is that ten attempts are enough to tell a
   broken DRBG from a busy one. RDSEED draws straight on the noise source and
   is expected to come up empty under contention, so it gets more patience: the
   kernel asks it for a seed a handful of times per boot, and waiting there is
   cheaper than reporting a working source as a missing one. */
const ulong RDRAND_ATTEMPTS = 10;
const ulong RDSEED_ATTEMPTS = 128;

bool have_rdrand;
bool have_rdseed;

bool rdrand_step(u64& out)
{
    u64 value = 0;
    u8 ok = 0;

    /* CF set means the value is good. Nothing else about the flags survives
       the instruction, hence the "cc" clobber. */
    asm volatile("rdrand %0; setc %1" : "=r"(value), "=qm"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool rdseed_step(u64& out)
{
    u64 value = 0;
    u8 ok = 0;

    asm volatile("rdseed %0; setc %1" : "=r"(value), "=qm"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool rdrand_retry(u64& out)
{
    for (ulong i = 0; i < RDRAND_ATTEMPTS; i++)
    {
        if (rdrand_step(out))
            return true;
        cpu_relax();
    }

    return false;
}

bool rdseed_retry(u64& out)
{
    for (ulong i = 0; i < RDSEED_ATTEMPTS; i++)
    {
        if (rdseed_step(out))
            return true;
        cpu_relax();
    }

    return false;
}

}

namespace hal
{

void probe_hw_random()
{
    CpuidResult features = cpuid(CPUID_LEAF_FEATURES);
    if (!(features.ecx & CPUID_BIT_RDRAND))
    {
        trace(0, "HwRandom: this cpu has no RDRAND");
        return;
    }

    /* Leaf 7 has to be asked for only if leaf 0 says it exists: CPUID answers
       a leaf above the maximum with the highest leaf's data, not with zeroes,
       so an unguarded read here invents an RDSEED bit out of unrelated
       feature words. */
    CpuidResult leaf0 = cpuid(0);
    bool rdseed = false;
    if (leaf0.eax >= CPUID_LEAF_STRUCTURED_EXT)
    {
        CpuidResult ext = cpuid(CPUID_LEAF_STRUCTURED_EXT, 0);
        rdseed = (ext.ebx & CPUID_BIT_RDSEED) != 0;
    }

    if (kernel::Parameters::get_instance().is_hw_rng_off())
    {
        /* Not a diagnostic for its own sake: it is how the timing-jitter path
           gets exercised on a machine that does have RDRAND. */
        trace(0, "HwRandom: rdrand present (rdseed %s), disabled by hwrng=off",
            rdseed ? "yes" : "no");
        return;
    }

    have_rdrand = true;
    have_rdseed = rdseed;

    trace(0, "HwRandom: rdrand yes, rdseed %s", have_rdseed ? "yes" : "no");
}

HwRandomKind get_hw_random_kind()
{
    if (!have_rdrand)
        return HwRandomKind::None;

    return have_rdseed ? HwRandomKind::RdSeed : HwRandomKind::RdRand;
}

bool hw_random(u64& out)
{
    if (!have_rdrand)
        return false;

    return rdrand_retry(out);
}

bool hw_random_seed(u64& out)
{
    if (!have_rdrand)
        return false;

    if (have_rdseed && rdseed_retry(out))
        return true;

    /* The noise source is drained, or there is no RDSEED here. RDRAND is
       reseeded from that same noise source, so it remains the best answer
       available -- refusing would leave the pool with nothing. */
    return rdrand_retry(out);
}

}
