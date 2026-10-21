#include "microcode.h"

#include <arch/x86_64/asm.h>
#include <arch/x86_64/cpuid.h>
#include <kernel/parameters.h>
#include <lib/stdlib.h>

namespace Kernel
{
namespace Microcode
{

namespace
{

const u32 LeafVendor = 0;
const u32 LeafFeatures = 1;
const u32 FeaturesEcxHypervisor = 1U << 31;
/* "GenuineIntel", as EBX, EDX and ECX give it */
const u32 IntelEbx = 0x756E6547;
const u32 IntelEdx = 0x49656E69;
const u32 IntelEcx = 0x6C65746E;

const u32 MsrPlatformId = 0x17;
const u32 PlatformIdShift = 50;
const u64 PlatformIdMask = 0x7;
/* IA32_BIOS_UPDT_TRIG: written the linear address of an update's data */
const u32 MsrUpdateTrigger = 0x79;
/* IA32_BIOS_SIGN_ID: the revision the CPU runs, in its high half */
const u32 MsrUpdateRevision = 0x8B;
const u32 RevisionShift = 32;

/* An update's data starts past its header */
const ulong HeaderSize = 48;

/* The update the BSP chose, copied out of the module while the bootstrap map
   still reached it, for the APs to load from after it has gone: aligned as
   the CPU wants an update's data (16), and in the image, so mapped wherever
   the kernel is. A client part's update is a few hundred KiB; one past this
   is said, not loaded. */
const ulong KeptMax = 1024 * 1024;
alignas(16) u8 KeptUpdate[KeptMax];
ulong KeptSize;

/* Why this CPU takes no microcode from here, or nullptr */
const char* NotHere()
{
    if (Parameters::GetInstance().IsUcodeOff())
        return "not loaded: ucode=off";

    CpuidResult v = Cpuid(LeafVendor);
    if (v.Ebx != IntelEbx || v.Edx != IntelEdx || v.Ecx != IntelEcx)
        return "not loaded: not an Intel CPU";

    /* Under a hypervisor the microcode is the host's, and Linux leaves it
       alone there too */
    if (Cpuid(LeafFeatures).Ecx & FeaturesEcxHypervisor)
        return "not loaded: a hypervisor's CPU";

    return nullptr;
}

/* The revision this CPU runs: IA32_BIOS_SIGN_ID's high half, which a CPUID
   fills in after a write of 0 (SDM Vol. 3A, 10.11.7) */
u32 Revision()
{
    WriteMsr(MsrUpdateRevision, 0);
    (void)Cpuid(LeafFeatures);
    return (u32)(ReadMsr(MsrUpdateRevision) >> RevisionShift);
}

/* This CPU's signature, and its platform as the mask an update names */
void Identify(u32* sig, u32* pf)
{
    static const u32 FamilyShift = 8, ModelShift = 4, ExtModelShift = 16, ExtFamilyShift = 20;
    static const u32 NibbleMask = 0xF, ByteMask = 0xFF, NibbleBits = 4;
    static const u32 FamilyExtended = 0xF;
    static const u32 FamilyP6 = 6;
    static const u32 ModelFirstPlatformId = 5;

    u32 eax = Cpuid(LeafFeatures).Eax;
    u32 family = (eax >> FamilyShift) & NibbleMask;
    u32 model = (eax >> ModelShift) & NibbleMask;
    if (family == FamilyExtended)
        family += (eax >> ExtFamilyShift) & ByteMask;
    if (family >= FamilyP6)
        model |= ((eax >> ExtModelShift) & NibbleMask) << NibbleBits;

    *sig = eax;
    *pf = 0;
    /* IA32_PLATFORM_ID is there from the P6's model 5, and in every family
       after */
    if (family > FamilyP6 || (family == FamilyP6 && model >= ModelFirstPlatformId))
        *pf = 1U << ((ReadMsr(MsrPlatformId) >> PlatformIdShift) & PlatformIdMask);
}

/* Load the kept update on this CPU, where it is for this CPU and newer than
   what it runs -- held to the format again, as every CPU holds it for
   itself: an AP of another signature takes nothing */
void Apply(Result* out)
{
    u32 sig = 0, pf = 0;
    Identify(&sig, &pf);
    out->Asked = true;
    out->Before = Revision();
    out->After = out->Before;

    Choice c;
    if (!Choose(KeptUpdate, KeptSize, sig, pf, 0, &c))
    {
        out->Skipped = "not loaded: the update is not for this CPU";
        return;
    }
    out->Date = c.Date;
    /* A CPU shares its microcode with others -- a core's two threads, an
       E-core module's four cores -- and the first of them to load it loads
       it for all */
    if (c.Revision <= out->Before)
    {
        out->Skipped = "not loaded: runs it already, or a newer one";
        return;
    }

    WriteMsr(MsrUpdateTrigger, (u64)(ulong)&KeptUpdate[c.Offset + HeaderSize]);

    out->After = Revision();
    out->Skipped = (out->After == c.Revision) ? nullptr : "the CPU did not take it";
}

}

void LoadOnBsp(const u8* file, ulong len, Result* out)
{
    *out = Result{};
    const char* why = NotHere();
    if (why != nullptr)
    {
        out->Skipped = why;
        return;
    }

    u32 sig = 0, pf = 0;
    Identify(&sig, &pf);
    out->Asked = true;
    out->Before = Revision();
    out->After = out->Before;

    Choice c;
    if (!Choose(file, len, sig, pf, 0, &c))
    {
        out->Skipped = "not loaded: the file has no update for this CPU";
        return;
    }
    out->Date = c.Date;
    if (c.Revision <= out->Before)
    {
        out->Skipped = "not loaded: this CPU runs the file's newest already, or a newer one";
        return;
    }
    if (c.Size > KeptMax)
    {
        out->Skipped = "not loaded: the update is larger than the room kept for it";
        return;
    }

    Stdlib::MemCpy(KeptUpdate, file + c.Offset, c.Size);
    KeptSize = c.Size;
    Apply(out);
}

void LoadOnAp(Result* out)
{
    *out = Result{};
    if (KeptSize == 0)
    {
        out->Skipped = "not loaded: the BSP kept none";
        return;
    }

    const char* why = NotHere();
    if (why != nullptr)
    {
        out->Skipped = why;
        return;
    }
    Apply(out);
}

bool Kept()
{
    return KeptSize != 0;
}

}
}
