#pragma once

#include <include/types.h>

/* The kernel functions a module may bind to: every function the ffi crate
   declares that this kernel defines, by name. The Makefile generates the table
   from pass1.elf (module_exports.S) and links it into the final kernel only;
   pass1_tables.cpp stands in for it in pass 1. */
struct ModuleExport
{
    const char* Name;
    ulong Addr;
};

extern "C" const ModuleExport nos_module_exports[];
extern "C" const ulong nos_module_export_count;
