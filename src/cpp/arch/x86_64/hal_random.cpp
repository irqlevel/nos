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
const u32 CpuidLeafFeatures = 1;
const u32 CpuidBitRdRand = 1U << 30;
const u32 CpuidLeafStructuredExt = 7;
const u32 CpuidBitRdSeed = 1U << 18;

/* Intel's own guidance for RDRAND is that ten attempts are enough to tell a
   broken DRBG from a busy one. RDSEED draws straight on the noise source and
   is expected to come up empty under contention, so it gets more patience: the
   kernel asks it for a seed a handful of times per boot, and waiting there is
   cheaper than reporting a working source as a missing one. */
const ulong RdRandAttempts = 10;
const ulong RdSeedAttempts = 128;

bool HaveRdRand;
bool HaveRdSeed;

bool RdRandStep(u64& out)
{
    u64 value = 0;
    u8 ok = 0;

    /* CF set means the value is good. Nothing else about the flags survives
       the instruction, hence the "cc" clobber. */
    asm volatile("rdrand %0; setc %1" : "=r"(value), "=qm"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool RdSeedStep(u64& out)
{
    u64 value = 0;
    u8 ok = 0;

    asm volatile("rdseed %0; setc %1" : "=r"(value), "=qm"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool RdRandRetry(u64& out)
{
    for (ulong i = 0; i < RdRandAttempts; i++)
    {
        if (RdRandStep(out))
            return true;
        Pause();
    }

    return false;
}

bool RdSeedRetry(u64& out)
{
    for (ulong i = 0; i < RdSeedAttempts; i++)
    {
        if (RdSeedStep(out))
            return true;
        Pause();
    }

    return false;
}

}

namespace Hal
{

void ProbeHwRandom()
{
    CpuidResult features = Cpuid(CpuidLeafFeatures);
    if (!(features.Ecx & CpuidBitRdRand))
    {
        Trace(0, "HwRandom: this cpu has no RDRAND");
        return;
    }

    /* Leaf 7 has to be asked for only if leaf 0 says it exists: CPUID answers
       a leaf above the maximum with the highest leaf's data, not with zeroes,
       so an unguarded read here invents an RDSEED bit out of unrelated
       feature words. */
    CpuidResult leaf0 = Cpuid(0);
    bool rdseed = false;
    if (leaf0.Eax >= CpuidLeafStructuredExt)
    {
        CpuidResult ext = Cpuid(CpuidLeafStructuredExt, 0);
        rdseed = (ext.Ebx & CpuidBitRdSeed) != 0;
    }

    if (Kernel::Parameters::GetInstance().IsHwRngOff())
    {
        /* Not a diagnostic for its own sake: it is how the timing-jitter path
           gets exercised on a machine that does have RDRAND. */
        Trace(0, "HwRandom: rdrand present (rdseed %s), disabled by hwrng=off",
            rdseed ? "yes" : "no");
        return;
    }

    HaveRdRand = true;
    HaveRdSeed = rdseed;

    Trace(0, "HwRandom: rdrand yes, rdseed %s", HaveRdSeed ? "yes" : "no");
}

bool HasHwRandom()
{
    return HaveRdRand;
}

const char* HwRandomName()
{
    if (!HaveRdRand)
        return "none";

    return HaveRdSeed ? "rdseed" : "rdrand";
}

bool HwRandom(u64& out)
{
    if (!HaveRdRand)
        return false;

    return RdRandRetry(out);
}

bool HwRandomSeed(u64& out)
{
    if (!HaveRdRand)
        return false;

    if (HaveRdSeed && RdSeedRetry(out))
        return true;

    /* The noise source is drained, or there is no RDSEED here. RDRAND is
       reseeded from that same noise source, so it remains the best answer
       available -- refusing would leave the pool with nothing. */
    return RdRandRetry(out);
}

}
