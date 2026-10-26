#pragma once

#include "spin_lock.h"
#include "mutex.h"
#include "task.h"

#include "input.h"
#include "line_editor.h"

#include <drivers/serial.h>
#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/printer.h>

namespace kernel
{

class Cmd final
    : public KeyboardObserver
    , public SerialObserver
{
public:
    static Cmd& get_instance()
    {
        static Cmd instance;
        return instance;
    }

    virtual void on_char(char c, u8 code) override;

    bool should_shutdown();
    bool should_reboot();

    void request_shutdown();
    void request_reboot();

    bool start();

    void stop();
    void stop_dhcp();

    static void dispatch(const char *cmd, stdlib::Printer& out);

    typedef void (*ScriptStep)(void* ctx);

    /* Runs the shell commands in the file at path a line at a time -- blank
       lines and #comments skipped -- with what they print on out, and step,
       if there is one, called after each (the boot script's logging). /etc/rc
       is run so at boot (run_boot_script). False if the file cannot be read,
       or a script is running already: a script that ran itself would never
       end. */
    bool run_script(const char* path, stdlib::Printer& out, ScriptStep step = nullptr,
        void* step_ctx = nullptr);

    /* Held while /etc/rc is rewritten, one edit at a time */
    Mutex& get_rc_lock() { return rc_lock_; }

    /* Shell commands added at run time, by a loadable module through
       kernel_cmd_register. A call hands the handler its ctx, the rest of the
       command line after the name, and the stdlib::Printer to answer on --
       opaque to the module, which writes to it with kernel_printer_write. */
    typedef void (*DynamicHandler)(void* ctx, const char* args, ulong args_len, void* out);

    /* A handle for unregister_dynamic, or 0: the name is empty, too long or
       not one printable word, it is taken -- by a built-in command or by
       another module's -- or the table is full. The name and help are
       copied. */
    ulong register_dynamic(const char* name, ulong name_len, const char* help, ulong help_len,
        DynamicHandler handler, void* ctx);

    /* Returns once no call to the command is running and none can start:
       from then on its handler and ctx are the caller's to free. Sleeps while
       a call runs, so task context only -- and never from that command's own
       handler, which would wait for itself. */
    void unregister_dynamic(ulong handle);

    /* The help lines of the commands added at run time */
    void dynamic_help(stdlib::Printer& out);

private:
    bool dispatch_dynamic(const char* cmd, stdlib::Printer& out);

    Cmd();
    ~Cmd();
    Cmd(const Cmd& other) = delete;
    Cmd(Cmd&& other) = delete;
    Cmd& operator=(const Cmd& other) = delete;
    Cmd& operator=(Cmd&& other) = delete;

    void show_banner(stdlib::Printer& out);

    /* /etc/rc, run once the network is set up, what it prints going to the
       kernel log; rc=off skips it */
    void run_boot_script();

    void run();
    static void run_func(void *ctx);

    /* What is typed, a terminal's bytes, until the shell's task takes it */
    stdlib::RingBuffer<char, consts::PAGE_SIZE> buf_;
    LineEditor editor_;
    SpinLock lock_;
    Task *task_ptr_;
    bool shutdown_;
    bool reboot_;
    bool active_;
    bool script_running_;     /* under Lock */
    Mutex rc_lock_;

    /* Room for every command the kernel's own layers register -- the block,
       filesystem and network layers are Rust and put their own commands in
       front of whoever runs one -- and for a module's on top of those. */
    static const ulong DYNAMIC_MAX = 128;
    static const ulong DYNAMIC_NAME_MAX = 31;
    static const ulong DYNAMIC_HELP_MAX = 95;
    /* A handle is the slot index + 1 in its low bits, and a registration
       count above them, so a stale handle never names the slot's next user */
    static const ulong DYNAMIC_SLOT_BITS = 8;
    static_assert(DYNAMIC_MAX < (1UL << DYNAMIC_SLOT_BITS), "slot must fit a handle");
    static const ulong DYNAMIC_POLL_NS = consts::NANO_SECS_IN_MS;

    struct DynamicCmd
    {
        ulong handle;       /* 0: the slot is free */
        char name[DYNAMIC_NAME_MAX + 1];
        char help[DYNAMIC_HELP_MAX + 1];
        DynamicHandler handler;
        void* ctx;
        ulong running;      /* calls in progress */
        bool removing;      /* no call may start */
    };

    DynamicCmd dynamic_[DYNAMIC_MAX];
    ulong dynamic_generation_;
    SpinLock dynamic_lock_;

    static const ulong TAG = 'Cmd ';
};

}