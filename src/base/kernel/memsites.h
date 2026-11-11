#pragma once

#include <lib/printer.h>
#include <mm/alloc_sites.h>

namespace kernel
{

/* memsites: what the heap's Rust blocks hold, by where they were allocated
   (docs/memsites.md). The tracker is mm::AllocSites; this is its reading --
   the heap walked, the frames named by the symbol table -- and the command
   and the boot parameter that drive it. */
namespace memsites
{

/* memsites=on at boot: the tracker on, with the heap just made and before
   the first of Rust's allocations */
void setup();

/* What every site holds now, into totals[AllocSites::SITES], zeroed first:
   the heap's pools walked and the big blocks' table read. False when the
   tracker was never on -- no sites, nothing counted. Called where an
   allocation may sleep, with no lock held: the walk takes the pools'. */
bool collect(mm::AllocSites::Totals* totals);

/* Whether a frame, as SymbolTable::describe_return names it, is the
   allocation's machinery rather than the code that asked for it: Rust's
   allocator shims, alloc's and core's code -- a Vec's growth, a Box, a
   String's -- and the kernel's GlobalAlloc */
bool is_machinery(const char* name);

/* The position in frames[count] of the first frame that is the code's own:
   past the machinery, or the last one when every frame is */
size_t own_position(const ulong* frames, size_t count);

/* The sites by bytes taken, merged by the frame that is the code's own, the
   top `top` of them, each with the two frames that called it; and what is
   tracked and what is not, of the Rust heap's total */
void report(stdlib::Printer& out, size_t top);

/* Every site with blocks, every frame of it as an address and a name: what
   scripts/memsites.py reads, to name each to a file and a line */
void dump(stdlib::Printer& out);

/* The pools' blocks by their tags: the C++ code's owners, Rust's by site
   and Rust's untracked */
void tags(stdlib::Printer& out);

}
}
