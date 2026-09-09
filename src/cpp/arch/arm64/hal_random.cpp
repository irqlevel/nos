#include <hal/random.h>
#include <hal/cpu.h>

#include <kernel/parameters.h>
#include <kernel/trace.h>

/* RNDR and RNDRRS -- FEAT_RNG, optional from Armv8.5 -- the arm64 backend of
   hal/random.h.

   Nothing nos runs on arm64 today implements them: Apple's M-series cores do
   not have FEAT_RNG, so a QEMU virt guest under HVF sees no RNDR, and TCG
   offers it only with -cpu max. That is precisely why this reports its absence
   cleanly instead of assuming a random instruction exists -- on that machine
   the entropy comes from virtio-rng and the timing collector, and
   Kernel::Random does not care which of the three it got. On an Ampere or
   Graviton core, which do implement FEAT_RNG, this becomes the primary source
   with nothing else to change. */

namespace
{

/* ID_AA64ISAR0_EL1.RNDR, bits [63:60]: 1 means RNDR and RNDRRS exist. */
const ulong IdIsar0RndrShift = 60;
const ulong IdIsar0RndrMask = 0xFULL;

/* Arm specifies no retry count. A read that cannot be answered "in a
   reasonable period of time" fails rather than blocking, so a retry rides out
   a reseed in progress -- it is not there to paper over a broken generator.
   RNDRRS forces that reseed and so fails more readily than RNDR. */
const ulong RndrAttempts = 10;
const ulong RndrrsAttempts = 128;

bool HaveRndr;

/* Both are read as system registers, and both set PSTATE.NZCV: on failure the
   register reads 0 and Z is set. The cset has to be in the same asm block as
   the mrs -- anything the compiler put between them would clobber the flags.

   Spelled as raw encodings (S3_3_C2_C4_0 = RNDR, S3_3_C2_C4_1 = RNDRRS) so the
   build needs no +rand in its target features: this file compiles for a
   baseline armv8-a and decides at run time. */
bool RndrStep(u64& out)
{
    u64 value = 0;
    ulong ok = 0;

    asm volatile("mrs %0, s3_3_c2_c4_0\n\tcset %1, ne"
        : "=r"(value), "=r"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool RndrrsStep(u64& out)
{
    u64 value = 0;
    ulong ok = 0;

    asm volatile("mrs %0, s3_3_c2_c4_1\n\tcset %1, ne"
        : "=r"(value), "=r"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool RndrRetry(u64& out)
{
    for (ulong i = 0; i < RndrAttempts; i++)
    {
        if (RndrStep(out))
            return true;
        Pause();
    }

    return false;
}

bool RndrrsRetry(u64& out)
{
    for (ulong i = 0; i < RndrrsAttempts; i++)
    {
        if (RndrrsStep(out))
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
    ulong isar0 = 0;
    asm volatile("mrs %0, id_aa64isar0_el1" : "=r"(isar0));

    if (((isar0 >> IdIsar0RndrShift) & IdIsar0RndrMask) == 0)
    {
        /* Reading RNDR on a core without FEAT_RNG is an undefined
           instruction, so this check is the whole reason the probe is a trace
           line and not a synchronous exception. */
        Trace(0, "HwRandom: no FEAT_RNG on this cpu (id_aa64isar0_el1 0x%p)",
            isar0);
        return;
    }

    if (Kernel::Parameters::GetInstance().IsHwRngOff())
    {
        Trace(0, "HwRandom: rndr present, disabled by hwrng=off");
        return;
    }

    HaveRndr = true;

    Trace(0, "HwRandom: rndr yes");
}

bool HasHwRandom()
{
    return HaveRndr;
}

const char* HwRandomName()
{
    return HaveRndr ? "rndr" : "none";
}

bool HwRandom(u64& out)
{
    if (!HaveRndr)
        return false;

    return RndrRetry(out);
}

bool HwRandomSeed(u64& out)
{
    if (!HaveRndr)
        return false;

    /* RNDRRS reseeds the generator from the noise source before answering,
       which is what another generator wants for its own seed. */
    if (RndrrsRetry(out))
        return true;

    return RndrRetry(out);
}

}
