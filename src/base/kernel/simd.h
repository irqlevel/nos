#pragma once

#include <include/types.h>

/* The kernel's SIMD sections: where code that runs vector instructions --
   CRYPTOGAMS' ChaCha20 and Poly1305, through the crypto crate -- may use the
   CPU's vector registers, which nothing else in the kernel touches and no
   context switch saves (hal/simd.h).

   A section is this CPU's for as long as it is open: preemption off, so no
   other task's code can run on the CPU and none can come to find the
   registers half used; one at a time, so that an interrupt handler that
   asks for one while the code it cut into has one open is refused -- and
   runs the integer code instead -- rather than handed registers in use; and
   every register zeroed at its end, so that nothing a section computed is
   left behind in them.

   And it is what keeps a guest's registers its own. hv puts a guest's x87,
   SSE and AVX registers into a CPU at its entry and takes them back at its
   exit, and skips the putting when the CPU still holds them from its last
   exit there: the table below says whose registers each CPU holds. A
   section marks the CPU's nobody's before it uses them, so the next entry
   there puts the guest's back -- no guest resumes with a section's
   values in its registers. */

namespace kernel
{
namespace simd
{

/* This CPU's vector unit on for sections, at its bring-up, before anything
   can open one on it; what a section may use folded into what every CPU so
   far has. The boot CPU's says what it found. */
void setup_cpu(bool boot_cpu);

/* What a section may use, on every CPU that is up: hal::SIMD_* bits, 0 for
   nothing. */
u32 features();

/* A section opened on this CPU: true, and the vector registers the caller's
   until end(); false, with nothing changed, where it cannot be had -- one
   open on this CPU already, or no vector unit to use. From any context. */
bool begin();

/* The section begin() opened on this CPU, closed: its registers zeroed and
   preemption back as it was. */
void end();

/* Whether CPU `cpu`'s registers are those guest `id`'s last exit there left
   -- hv's to ask at an entry, with interrupts off on `cpu`. */
bool guest_registers_are(ulong cpu, u64 id);

/* Guest `id`'s registers have just been put into CPU `cpu`, by its entry
   there with interrupts off: they are its on `cpu`, and its on no other CPU
   any more -- what another CPU holds of it is older. */
void guest_registers_loaded(ulong cpu, u64 id);

/* CPU `cpu`'s registers are nobody's: hv's extension going off for it. */
void guest_registers_lost(ulong cpu);

}
}
