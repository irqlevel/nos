#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

/* Exercise the filesystem behind dir through the Vfs: create, write at an
   offset, append, truncate both ways, rename, move, a file of bigSize
   bytes written and read back in mismatched chunks, recursive remove.
   Every step reports to out (if given) and to the trace; false on the
   first failure. Runs at boot with fstest=on and from the shell as
   `fstest`. */
bool FsSelfTest(const char* dir, ulong bigSize, Stdlib::Printer* out);

}
