#include "microcode.h"

#include <arch/x86_64/asm.h>
#include <arch/x86_64/cpuid.h>
#include <kernel/parameters.h>
#include <lib/stdlib.h>

namespace kernel
{
namespace microcode
{

namespace
{

const u32 LEAF_VENDOR = 0;
const u32 LEAF_FEATURES = 1;
const u32 FEATURES_ECX_HYPERVISOR = 1U << 31;
/* "GenuineIntel", as EBX, EDX and ECX give it */
const u32 INTEL_EBX = 0x756E6547;
const u32 INTEL_EDX = 0x49656E69;
const u32 INTEL_ECX = 0x6C65746E;

const u32 MSR_PLATFORM_ID = 0x17;
const u32 PLATFORM_ID_SHIFT = 50;
const u64 PLATFORM_ID_MASK = 0x7;
/* IA32_BIOS_UPDT_TRIG: written the linear address of an update's data */
const u32 MSR_UPDATE_TRIGGER = 0x79;
/* IA32_BIOS_SIGN_ID: the revision the CPU runs, in its high half */
const u32 MSR_UPDATE_REVISION = 0x8B;
const u32 REVISION_SHIFT = 32;

/* An update's data starts past its header */
const ulong HEADER_SIZE = 48;

/* The update the BSP chose, copied out of the module while the bootstrap map
   still reached it, for the APs to load from after it has gone: aligned as
   the CPU wants an update's data (16), and in the image, so mapped wherever
   the kernel is. A client part's update is a few hundred KiB; one past this
   is said, not loaded. */
const ulong KEPT_MAX = 1024 * 1024;
alignas(16) u8 kept_update[KEPT_MAX];
ulong kept_size;

/* Why this CPU takes no microcode from here, or nullptr */
const char* not_here()
{
    if (Parameters::get_instance().is_ucode_off())
        return "not loaded: ucode=off";

    CpuidResult v = cpuid(LEAF_VENDOR);
    if (v.ebx != INTEL_EBX || v.edx != INTEL_EDX || v.ecx != INTEL_ECX)
        return "not loaded: not an Intel CPU";

    /* Under a hypervisor the microcode is the host's, and Linux leaves it
       alone there too */
    if (cpuid(LEAF_FEATURES).ecx & FEATURES_ECX_HYPERVISOR)
        return "not loaded: a hypervisor's CPU";

    return nullptr;
}

/* The revision this CPU runs: IA32_BIOS_SIGN_ID's high half, which a CPUID
   fills in after a write of 0 (SDM Vol. 3A, 10.11.7) */
u32 revision()
{
    write_msr(MSR_UPDATE_REVISION, 0);
    (void)cpuid(LEAF_FEATURES);
    return (u32)(read_msr(MSR_UPDATE_REVISION) >> REVISION_SHIFT);
}

/* This CPU's signature, and its platform as the mask an update names */
void identify(u32* sig, u32* pf)
{
    static const u32 FAMILY_SHIFT = 8, MODEL_SHIFT = 4, EXT_MODEL_SHIFT = 16, EXT_FAMILY_SHIFT = 20;
    static const u32 NIBBLE_MASK = 0xF, BYTE_MASK = 0xFF, NIBBLE_BITS = 4;
    static const u32 FAMILY_EXTENDED = 0xF;
    static const u32 FAMILY_P6 = 6;
    static const u32 MODEL_FIRST_PLATFORM_ID = 5;

    u32 eax = cpuid(LEAF_FEATURES).eax;
    u32 family = (eax >> FAMILY_SHIFT) & NIBBLE_MASK;
    u32 model = (eax >> MODEL_SHIFT) & NIBBLE_MASK;
    if (family == FAMILY_EXTENDED)
        family += (eax >> EXT_FAMILY_SHIFT) & BYTE_MASK;
    if (family >= FAMILY_P6)
        model |= ((eax >> EXT_MODEL_SHIFT) & NIBBLE_MASK) << NIBBLE_BITS;

    *sig = eax;
    *pf = 0;
    /* IA32_PLATFORM_ID is there from the P6's model 5, and in every family
       after */
    if (family > FAMILY_P6 || (family == FAMILY_P6 && model >= MODEL_FIRST_PLATFORM_ID))
        *pf = 1U << ((read_msr(MSR_PLATFORM_ID) >> PLATFORM_ID_SHIFT) & PLATFORM_ID_MASK);
}

/* Load the kept update on this CPU, where it is for this CPU and newer than
   what it runs -- held to the format again, as every CPU holds it for
   itself: an AP of another signature takes nothing */
void apply(Result* out)
{
    u32 sig = 0, pf = 0;
    identify(&sig, &pf);
    out->asked = true;
    out->before = revision();
    out->after = out->before;

    Choice c;
    if (!choose(kept_update, kept_size, sig, pf, 0, &c))
    {
        out->skipped = "not loaded: the update is not for this CPU";
        return;
    }
    out->date = c.date;
    /* A CPU shares its microcode with others -- a core's two threads, an
       E-core module's four cores -- and the first of them to load it loads
       it for all */
    if (c.revision <= out->before)
    {
        out->skipped = "not loaded: runs it already, or a newer one";
        return;
    }

    write_msr(MSR_UPDATE_TRIGGER, (u64)(ulong)&kept_update[c.offset + HEADER_SIZE]);

    out->after = revision();
    out->skipped = (out->after == c.revision) ? nullptr : "the CPU did not take it";
}

}

void load_on_bsp(const u8* file, ulong len, Result* out)
{
    *out = Result{};
    const char* why = not_here();
    if (why != nullptr)
    {
        out->skipped = why;
        return;
    }

    u32 sig = 0, pf = 0;
    identify(&sig, &pf);
    out->asked = true;
    out->before = revision();
    out->after = out->before;

    Choice c;
    if (!choose(file, len, sig, pf, 0, &c))
    {
        out->skipped = "not loaded: the file has no update for this CPU";
        return;
    }
    out->date = c.date;
    if (c.revision <= out->before)
    {
        out->skipped = "not loaded: this CPU runs the file's newest already, or a newer one";
        return;
    }
    if (c.size > KEPT_MAX)
    {
        out->skipped = "not loaded: the update is larger than the room kept for it";
        return;
    }

    stdlib::mem_cpy(kept_update, file + c.offset, c.size);
    kept_size = c.size;
    apply(out);
}

void load_on_ap(Result* out)
{
    *out = Result{};
    if (kept_size == 0)
    {
        out->skipped = "not loaded: the BSP kept none";
        return;
    }

    const char* why = not_here();
    if (why != nullptr)
    {
        out->skipped = why;
        return;
    }
    apply(out);
}

bool kept()
{
    return kept_size != 0;
}

}
}
