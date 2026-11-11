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
   crate built on src/crates/base/kmod and linked into a position-independent ELF
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
   rmmod in a task of its own (start_load, start_unload). */
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

    /* The halt's: unload every module that can be, newest first, and load
       none from here on. While the services a module's exit may use --
       files, block I/O, the soft IRQs it completes through -- still run,
       and while the shells do: a shell may be inside a module's command
       that returns only when its module is told to stop, and it is the
       take-down that tells it (close, stop, the calls waited out, exit --
       which is why no shell has to be gone first). A load another task is
       in the middle of is waited for, LOADING_WAIT_MS at most, and unloaded
       with the rest; a module another task is unloading is left to it. */
    void unload_all();

    /* How an insmod or rmmod ended, as start_load and start_unload saw it */
    enum class JobOutcome
    {
        /* Done, and it said so */
        Done,
        /* Done, and failed: what it said says why */
        Failed,
        /* Still going after SHELL_WAIT_MS: lsmod shows where, the log will
           say how it ended */
        Pending,
        /* Never started: too long, or no task or memory for it */
        NotStarted,
    };

    /* insmod and rmmod: load_file or Unload in a task of its own, waited for
       for at most SHELL_WAIT_MS. What it says is printed on out if it is done
       by then; if not, it carries on in the background -- lsmod shows the
       module loading or unloading meanwhile -- and the kernel log says how
       it ended. insmod's args are the path, its first word, and the
       module's parameters, the rest of the line. */
    JobOutcome start_load(const char* args, stdlib::Printer& out);
    JobOutcome start_unload(const char* name, stdlib::Printer& out);

    bool is_loaded(const char* name);

    /* lsmod: a line per module */
    void dump(stdlib::Printer& out);

    /* addr as "function+0xoff [module]" into buf when it is inside a loaded
       module -- copied, since the module may go the moment this returns.
       Never waits, so a panic may call it: false if the table is busy.
       return_address: addr is a return address, named by the call before it
       (SymbolTable::describe_return); the offset is still addr's. */
    bool describe(ulong addr, bool return_address, char* buf, ulong size);

    /* The name of the module whose image holds addr, into buf (size bytes,
       no NUL written): its length, or 0 when addr is no loaded module's. How
       the message channel knows which module a caller of its C ABI is
       (ipc::owner_at): an address in the caller's own code. A module is
       listed while its init runs, so what its init makes is its own. */
    ulong name_at(ulong addr, char* buf, ulong size);

    /* Whether addr is in the image of a module that is going -- unloaded,
       or its init failed: what a shell command and a bus provider are
       refused registration by once take_down has begun to close the
       module's, so that none is left behind it able to take a call. Never
       waits. */
    bool unloading_at(ulong addr);

    /* The loaded modules, "name base+size" each, into buf for the panic
       report. Never waits either; false if there are none, or no telling. */
    bool describe_all(char* buf, ulong size);

    /* The longest module name: kmod's NAME_LEN, less the NUL */
    static const ulong NAME_LEN_MAX = 31;
    static const ulong PATH_LEN_MAX = 255;
    /* The longest a module's parameters may be, as insmod gives them */
    static const ulong PARAMS_MAX = 255;

    /* A module as the management bus lists it (`system.modules`), copied
       out under the lock into the caller's array -- nothing allocated -- and
       not where it is: the API says no kernel address. The C ABI's,
       kernel_module_list's: ffi::system::ModuleInfo is its Rust */
    struct ModuleSnapshot
    {
        char name[NAME_LEN_MAX + 1];
        u64 size;
        u64 imports;
        /* SNAPSHOT_* */
        u32 phase;
        /* 1 when what it imports keeps it from being unloaded */
        u32 permanent;
    };

    static const u32 SNAPSHOT_LIVE = 0;
    static const u32 SNAPSHOT_LOADING = 1;
    static const u32 SNAPSHOT_UNLOADING = 2;

    ulong snapshot(ModuleSnapshot* out, ulong max);


    /* How long insmod and rmmod wait for their task before they leave it to
       finish on its own */
    static const ulong SHELL_WAIT_MS = 5000;

    /* How long the halt waits for a load in another task's hands */
    static const ulong LOADING_WAIT_MS = 10000;

private:
    ModuleTable();
    ~ModuleTable();
    ModuleTable(const ModuleTable& other) = delete;
    ModuleTable(ModuleTable&& other) = delete;
    ModuleTable& operator=(const ModuleTable& other) = delete;
    ModuleTable& operator=(ModuleTable&& other) = delete;

    JobOutcome start_job(bool unload, const char* arg, const char* params, stdlib::Printer& out);
    LoadedModule* find_locked(const char* name);
    void remove(LoadedModule* module);

    /* A module made Unloading taken down, its exit run if it has an
       instance (live), and freed: the order is module.cpp's */
    void take_down(LoadedModule* module, bool live);

    /* Guards List and each module's phase. Held only for a lookup or a
       change of phase: never across a module's init or exit, an allocation,
       or printing. Not watched: a panic may be the first to want it. */
    RawSpinLock lock_;
    stdlib::ListEntry list_;
    /* Set by unload_all, under the lock: nothing is put on the list after */
    bool halting_;
};

}
