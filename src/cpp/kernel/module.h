#pragma once

#include "raw_spin_lock.h"

#include <lib/stdlib.h>
#include <lib/error.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

namespace kernel
{

struct LoadedModule;

/* Loadable kernel modules, written in Rust (docs/modules.md). A module is a
   crate built on src/rust/kmod and linked into a position-independent ELF
   shared object, a .ko: code that runs in the kernel, reaches it only through
   the functions the ffi crate declares, and can be put into a running kernel
   and taken out of it again.

   Loading maps the image into pages of its own, applies its dynamic
   relocations -- binding each kernel function it imports by name, against
   the export table the build generates -- checks the header the kmod crate's
   module! macro puts in it, gives each segment the page permissions its
   program header asks for (a segment may be writable or executable, never
   both), keeps the names of its functions the build put in it, for
   backtraces, and runs its init. Unloading runs its exit, which drops the
   module's state and with it everything the module holds, and frees the
   pages.

   Loads and unloads run in task context: they allocate, and a module's init
   and exit are free to sleep -- an exit waits out any call of the module's
   commands still running, for as long as that takes. So the table's lock is
   held only to look a module up or move it on to its next phase, never
   across an init or an exit: a load or unload that takes its time holds up
   nothing but itself, and lsmod never waits. The shell runs each insmod and
   rmmod in a task of its own (StartLoad, StartUnload). */
class ModuleTable final
{
public:
    static ModuleTable& get_instance()
    {
        static ModuleTable instance;
        return instance;
    }

    /* Load the module in the file at path, its init handed params -- what
       insmod's line had after the path, "" for none: the module's to take
       or refuse. What goes wrong is said on out. */
    stdlib::Error load_file(const char* path, const char* params, stdlib::Printer& out);

    /* Load a module from an image in memory, 8-byte aligned, its init
       handed params. The image is only read, and not needed once this
       returns. */
    stdlib::Error load(const void* image, ulong size, const char* params, stdlib::Printer& out);

    stdlib::Error unload(const char* name, stdlib::Printer& out);

    /* Unload every module that can be, newest first. For the shutdown path,
       while the services a module's exit may use -- files, block I/O, the
       soft IRQs it completes through -- still run. A module another task is
       still loading or unloading is left to it. */
    void unload_all();

    /* insmod and rmmod: LoadFile or Unload in a task of its own, waited for
       for at most ShellWaitMs. What it says is printed on out if it is done
       by then; if not, it carries on in the background -- lsmod shows the
       module loading or unloading meanwhile -- and the kernel log says how
       it ended. insmod's args are the path, its first word, and the
       module's parameters, the rest of the line. */
    void start_load(const char* args, stdlib::Printer& out);
    void start_unload(const char* name, stdlib::Printer& out);

    bool is_loaded(const char* name);

    /* lsmod: a line per module */
    void dump(stdlib::Printer& out);

    /* addr as "function+0xoff [module]" into buf when it is inside a loaded
       module -- copied, since the module may go the moment this returns.
       Never waits, so a panic may call it: false if the table is busy.
       returnAddress: addr is a return address, named by the call before it
       (SymbolTable::DescribeReturn); the offset is still addr's. */
    bool describe(ulong addr, bool return_address, char* buf, ulong size);

    /* The loaded modules, "name base+size" each, into buf for the panic
       report. Never waits either; false if there are none, or no telling. */
    bool describe_all(char* buf, ulong size);

    /* The longest module name: kmod's NAME_LEN, less the NUL */
    static const ulong NAME_LEN_MAX = 31;
    static const ulong PATH_LEN_MAX = 255;
    /* The longest a module's parameters may be, as insmod gives them */
    static const ulong PARAMS_MAX = 255;

    /* How long insmod and rmmod wait for their task before they leave it to
       finish on its own */
    static const ulong SHELL_WAIT_MS = 5000;

private:
    ModuleTable();
    ~ModuleTable();
    ModuleTable(const ModuleTable& other) = delete;
    ModuleTable(ModuleTable&& other) = delete;
    ModuleTable& operator=(const ModuleTable& other) = delete;
    ModuleTable& operator=(ModuleTable&& other) = delete;

    void start_job(bool unload, const char* arg, const char* params, stdlib::Printer& out);
    LoadedModule* find_locked(const char* name);
    void remove(LoadedModule* module);

    /* Guards List and each module's phase. Held only for a lookup or a
       change of phase: never across a module's init or exit, an allocation,
       or printing. Not watched: a panic may be the first to want it. */
    RawSpinLock lock_;
    stdlib::ListEntry list_;
};

}
