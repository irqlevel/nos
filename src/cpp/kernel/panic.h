#pragma once

#include <lib/stdlib.h>
#include <hal/context.h>
#include "atomic.h"

namespace kernel
{

class Panicker
{
public:
    static Panicker& get_instance()
    {
        static Panicker instance;

        return instance;
    }

    PRINTF_FORMAT(2, 3) void do_panic(const char *fmt, ...);

    PRINTF_FORMAT(4, 5) void do_panic_ctx(Context* ctx, bool has_error_code, const char *fmt, ...);

    bool is_active();

    /* Called from the NMI handler. Returns true when a panic asked for this
       CPU's stack, in which case it has been recorded and the caller must
       not go on to panic about the NMI itself. */
    bool collect_remote_stack();

    /* Called first by the handlers of the faults a read can raise -- x86's
       page fault, general protection fault and stack fault, arm64's sync
       exception. When the fault is the panicking CPU's own, in a read its
       report could not be sure of (ProbeCpu, see DoPanicCtx), it does not
       return: it resumes the report past that read. Otherwise it returns,
       and the fault is reported as any other. */
    void recover_fault();

private:

    /* The CPU that wedged the machine is, by definition, one that stopped
       answering interrupts -- so an ordinary IPI never reaches it and its
       stack, the one worth having, is the one missing from the report. An
       NMI does reach it. Each CPU writes its own slot from inside the NMI
       handler and the panicking CPU prints them. */
    void collect_remote_stacks();

    /* panic=N: the machine reset N seconds after the report has gone out.
       Returns where nothing asked for it. */
    void reset_if_asked();

    static const size_t REMOTE_FRAME_COUNT = 12;

    /* Kept in step with Kernel::MaxCpus by a static_assert in panic.cpp;
       including cpu.h here would be circular. */
    static const size_t REMOTE_CPU_COUNT = 64;

    Panicker();
    virtual ~Panicker();
    Panicker(const Panicker& other) = delete;
    Panicker(Panicker&& other) = delete;
    Panicker& operator=(const Panicker& other) = delete;
    Panicker& operator=(Panicker&& other) = delete;

    void print_output(const char* str);
    void dump_context();
    void dump_backtrace(ulong* frames, size_t count);
    void dump_modules();

    /* 512, not 256: a Rust panic's text is "panicked at <file>:<line>:" and
       the message, and an undefined-behaviour check's message alone is over
       200 characters (RUSTUB=1, docs/build.md). */
    char message_[512];
    Atomic active_;

    /* A read of memory that may not be there, by the CPU making the report:
       that CPU's index in ProbeCpu while it lasts, -1 otherwise, and in
       ProbeJump where a fault in it resumes. */
    JmpContext probe_jump_;
    Atomic probe_cpu_;

    Atomic collecting_;
    ulong remote_frame_[REMOTE_CPU_COUNT][REMOTE_FRAME_COUNT];
    u8 remote_count_[REMOTE_CPU_COUNT];
    Atomic remote_done_[REMOTE_CPU_COUNT];
};

}

#define PANIC(fmt, ...)                                             \
do {                                                                \
    auto& macro_panicker = kernel::Panicker::get_instance();         \
    macro_panicker.do_panic("PANIC:%s():%s,%lu: " fmt "\n",          \
        __func__, stdlib::truncate_file_name(__FILE__),               \
        (ulong)__LINE__, ##__VA_ARGS__);                            \
} while (false)

#define PANIC_CTX(ctx, has_err_code, fmt, ...)                       \
do {                                                                \
    auto& macro_panicker = kernel::Panicker::get_instance();         \
    macro_panicker.do_panic_ctx(ctx, has_err_code,                    \
        "PANIC:%s():%s,%lu: " fmt "\n",                             \
        __func__, stdlib::truncate_file_name(__FILE__),               \
        (ulong)__LINE__, ##__VA_ARGS__);                            \
} while (false)

static inline bool do_bug_on(const char *func, const char *file, int line)
{
    auto& panicker = kernel::Panicker::get_instance();
    panicker.do_panic("PANIC:%s():%s,%lu: BUG\n", func, file, (ulong)line);
    return true;
}

#define BUG_ON(condition)    \
    (unlikely(condition)) ? do_bug_on(__func__, stdlib::truncate_file_name(__FILE__), __LINE__) : \
    false
