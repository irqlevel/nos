#pragma once

#include "mutex.h"

#include <lib/stdlib.h>
#include <lib/error.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

namespace Kernel
{

struct LoadedModule;

/* Loadable kernel modules, written in Rust (docs/modules.md). A module is a
   crate built on src/rust/kmod and linked into a position-independent ELF
   shared object, a .ko: code that runs in the kernel, reaches it only through
   the functions the ffi crate declares, and can be put into a running kernel
   and taken out of it again.

   Loading maps the image's segments into pages of their own, applies its
   dynamic relocations -- binding each kernel function it imports by name,
   against the export table the build generates -- checks the header the
   kmod crate's module! macro puts in it, gives each segment the page
   permissions its program header asks for (a segment may be writable or
   executable, never both), and runs its init. Unloading runs its exit, which
   drops the module's state and with it everything the module holds, and
   frees the pages.

   Load and unload run in task context: they allocate, and a module's init
   and exit are free to sleep. One at a time -- a mutex serializes them. */
class ModuleTable final
{
public:
    static ModuleTable& GetInstance()
    {
        static ModuleTable Instance;
        return Instance;
    }

    /* Load the module in the file at path. What goes wrong is said on out. */
    Stdlib::Error LoadFile(const char* path, Stdlib::Printer& out);

    /* Load a module from an image in memory, 8-byte aligned. The image is
       only read, and not needed once this returns. */
    Stdlib::Error Load(const void* image, ulong size, Stdlib::Printer& out);

    Stdlib::Error Unload(const char* name, Stdlib::Printer& out);

    /* Unload every module that can be, newest first. For the shutdown path,
       while the services a module's exit may use -- files, block I/O, the
       soft IRQs it completes through -- still run. */
    void UnloadAll();

    bool IsLoaded(const char* name);

    /* lsmod: a line per module */
    void Dump(Stdlib::Printer& out);

    /* The longest module name: kmod's NAME_LEN, less the NUL */
    static const ulong NameMax = 31;

private:
    ModuleTable();
    ~ModuleTable();
    ModuleTable(const ModuleTable& other) = delete;
    ModuleTable(ModuleTable&& other) = delete;
    ModuleTable& operator=(const ModuleTable& other) = delete;
    ModuleTable& operator=(ModuleTable&& other) = delete;

    LoadedModule* FindLocked(const char* name);
    void UnloadLocked(LoadedModule* module);

    Mutex Lock;
    Stdlib::ListEntry List;
};

}
