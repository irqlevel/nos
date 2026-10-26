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
   the entropy comes from virtio-rng and the timing collector, and the pool
   does not care which of the three it got. On an Ampere or
   Graviton core, which do implement FEAT_RNG, this becomes the primary source
   with nothing else to change. */

namespace
{

/* ID_AA64ISAR0_EL1.RNDR, bits [63:60]: 1 means RNDR and RNDRRS exist. */
const ulong ID_ISAR0_RNDR_SHIFT = 60;
const ulong ID_ISAR0_RNDR_MASK = 0xFULL;

/* Arm specifies no retry count. A read that cannot be answered "in a
   reasonable period of time" fails rather than blocking, so a retry rides out
   a reseed in progress -- it is not there to paper over a broken generator.
   RNDRRS forces that reseed and so fails more readily than RNDR. */
const ulong RNDR_ATTEMPTS = 10;
const ulong RNDRRS_ATTEMPTS = 128;

bool have_rndr;

/* Both are read as system registers, and both set PSTATE.NZCV: on failure the
   register reads 0 and Z is set. The cset has to be in the same asm block as
   the mrs -- anything the compiler put between them would clobber the flags.

   Spelled as raw encodings (S3_3_C2_C4_0 = RNDR, S3_3_C2_C4_1 = RNDRRS) so the
   build needs no +rand in its target features: this file compiles for a
   baseline armv8-a and decides at run time. */
bool rndr_step(u64& out)
{
    u64 value = 0;
    ulong ok = 0;

    asm volatile("mrs %0, s3_3_c2_c4_0\n\tcset %1, ne"
        : "=r"(value), "=r"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool rndrrs_step(u64& out)
{
    u64 value = 0;
    ulong ok = 0;

    asm volatile("mrs %0, s3_3_c2_c4_1\n\tcset %1, ne"
        : "=r"(value), "=r"(ok) :: "cc");

    out = value;
    return ok != 0;
}

bool rndr_retry(u64& out)
{
    for (ulong i = 0; i < RNDR_ATTEMPTS; i++)
    {
        if (rndr_step(out))
            return true;
        cpu_relax();
    }

    return false;
}

bool rndrrs_retry(u64& out)
{
    for (ulong i = 0; i < RNDRRS_ATTEMPTS; i++)
    {
        if (rndrrs_step(out))
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
    ulong isar0 = 0;
    asm volatile("mrs %0, id_aa64isar0_el1" : "=r"(isar0));

    if (((isar0 >> ID_ISAR0_RNDR_SHIFT) & ID_ISAR0_RNDR_MASK) == 0)
    {
        /* Reading RNDR on a core without FEAT_RNG is an undefined
           instruction, so this check is the whole reason the probe is a trace
           line and not a synchronous exception. */
        trace(0, "HwRandom: no FEAT_RNG on this cpu (id_aa64isar0_el1 0x%lX)",
            isar0);
        return;
    }

    if (kernel::Parameters::get_instance().is_hw_rng_off())
    {
        trace(0, "HwRandom: rndr present, disabled by hwrng=off");
        return;
    }

    have_rndr = true;

    trace(0, "HwRandom: rndr yes");
}

HwRandomKind get_hw_random_kind()
{
    return have_rndr ? HwRandomKind::Rndr : HwRandomKind::None;
}

bool hw_random(u64& out)
{
    if (!have_rndr)
        return false;

    return rndr_retry(out);
}

bool hw_random_seed(u64& out)
{
    if (!have_rndr)
        return false;

    /* RNDRRS reseeds the generator from the noise source before answering,
       which is what another generator wants for its own seed. */
    if (rndrrs_retry(out))
        return true;

    return rndr_retry(out);
}

}
