#pragma once

#include <include/types.h>

namespace kernel
{

/* Intel's microcode, loaded by the kernel from the file GRUB put in memory
   beside it (`module2 /boot/ucode ucode`): what firmware's own microcode
   lacks of the fixes since it was built. Under nos the EX44 ran its
   firmware's 0x3a, where its Linux loads 0x3e, of October 2025 -- and a
   mitigation is only as good as the microcode under it. Loaded early, on
   the BSP and on each AP as it comes up, as Linux loads it from its
   initramfs. */
namespace microcode
{

/* An update for a CPU, as Choose found it in a file. */
struct Choice
{
    ulong offset;   /* where it starts in the file */
    ulong size;     /* all of it: header, data, any extended table */
    u32 revision;
    u32 date;       /* as the header has it: mmddyyyy, in BCD */
};

/* The parts of an update's date, each of which printed in hex reads as the
   decimal it is: (Date & DATE_YEAR_MASK), (Date >> DATE_MONTH_SHIFT) and
   ((Date >> DATE_DAY_SHIFT) & DATE_DAY_MASK) */
const u32 DATE_YEAR_MASK = 0xFFFF;
const u32 DATE_MONTH_SHIFT = 24;
const u32 DATE_DAY_SHIFT = 16;
const u32 DATE_DAY_MASK = 0xFF;

/* The newest update in `file` -- `len` bytes of Intel's updates one after
   another (SDM Vol. 3A, 10.11.1) -- for a CPU of signature `sig`
   (CPUID.1:EAX) and platform mask `pf` (1 << IA32_PLATFORM_ID[52:50], 0 on
   a CPU without the MSR), of a revision above `current`. Each update is
   held to its format before anything in it is believed -- the header's and
   the loader's versions, its sizes, its checksum, an extended signature
   table's -- and the walk ends at the first that is not sound, past which
   where the next one starts is unknown. False when none fits. Reads
   nothing outside [file, file + len). */
bool choose(const u8* file, ulong len, u32 sig, u32 pf, u32 current, Choice* out);

/* What loading did on a CPU: whether it was asked its revision -- not where
   the microcode is not the kernel's to load, as under a hypervisor -- the
   revision it ran before and after, the update's date, and why nothing was
   loaded: nullptr when an update was. */
struct Result
{
    bool asked;
    u32 before;
    u32 after;
    u32 date;
    const char* skipped;
};

/* On the BSP, interrupts off, while the bootstrap map still reaches the
   module (`file`, `len`): choose this CPU's update, keep it for the APs and
   load it here. */
void load_on_bsp(const u8* file, ulong len, Result* out);

/* On an AP, interrupts off: the update the BSP kept, loaded where it is for
   this CPU and newer than what it runs. Nothing where the BSP kept none. */
void load_on_ap(Result* out);

/* Whether the BSP kept an update, for the APs to say what they did with it. */
bool kept();

}
}
