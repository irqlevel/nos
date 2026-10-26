#include "panic.h"
#include "preempt.h"
#include <hal/cpu.h>
#include <hal/context.h>
#include "cpu.h"
#include "parameters.h"
#include "trace.h"
#include "stack_trace.h"
#include "symtab.h"
#include "module.h"
#include "task.h"

#include <hal/irqchip.h>
#include <hal/barrier.h>
#include <hal/console.h>
#include <hal/power.h>


/* The disk log is Rust (src/rust/block/src/disklog.rs). */
extern "C" {
void rust_disklog_log(const char* line);
void rust_netconsole_log(const char* line, unsigned long len);
void rust_netconsole_panic_mark();
void rust_netconsole_panic_flush();
void rust_disklog_panic_flush();
}

namespace kernel
{

Panicker::Panicker()
    : probe_cpu_(-1)
{
}

Panicker::~Panicker()
{
}

bool Panicker::is_active()
{
    return (active_.get() != 0) ? true : false;
}

/* After the report: on the screen, the serial port and -- flushed by the
   caller -- the netconsole and the disk log. Timed on the cycle counter,
   which takes no lock and wants no interrupt; before the counter is
   calibrated, at once. Only the CPU whose report it is: the others are
   halted. */
void Panicker::reset_if_asked()
{
    ulong secs = Parameters::get_instance().get_panic_reset_secs();
    if (secs == 0)
        return;

    char buf[96];
    stdlib::sn_printf(buf, sizeof(buf), "panic: resetting the machine in %lu s (panic=%lu)\n", secs, secs);
    print_output(buf);

    ulong hz = hal::cycle_counter_hz();
    if (hz != 0)
    {
        u64 start = hal::read_cycle_counter();
        u64 wait = (u64)secs * hz;
        while (hal::read_cycle_counter() - start < wait)
            cpu_relax();
    }
    hal::reset_now();
}

void Panicker::print_output(const char* str)
{
    hal::console_panic_write(str);

    /* Also into the netconsole ring; PanicFlush() below pushes it out while
       the machine still can. */
    rust_netconsole_log(str, stdlib::str_len(str));

    /* And into the disk log's, for its PanicFlush(): on a machine with no
       serial port and no network yet the disk is the only place the report
       can go. Log() takes no lock, which is what makes it safe here. */
    rust_disklog_log(str);
}

void Panicker::dump_context()
{
    char buf[128];

    /* CPU ID — safe if LAPIC is mapped */
    if (hal::irq_chip_ready())
    {
        ulong cpu_id = CpuTable::get_instance().get_current_cpu_id();
        stdlib::sn_printf(buf, sizeof(buf), "CPU: %lu\n", cpu_id);
        print_output(buf);
    }

    /* Task — use try_get_current_task to avoid BUG_ON/recursive panic */
    Task* task = Task::try_get_current_task();
    if (task != nullptr)
    {
        stdlib::sn_printf(buf, sizeof(buf), "Task: pid %lu name %s\n",
            task->pid_, task->get_name());
        print_output(buf);
    }
}

void Panicker::dump_backtrace(ulong* frames, size_t count)
{
    char where[SymbolTable::DESCRIBE_MAX];
    char buf[SymbolTable::DESCRIBE_MAX + 32];
    auto& symtab = SymbolTable::get_instance();

    print_output("Backtrace:\n");
    for (size_t i = 0; i < count; i++)
    {
        if (symtab.describe_return(frames[i], where, sizeof(where)))
            stdlib::sn_printf(buf, sizeof(buf), "  [%lu] 0x%lX %s\n",
                (ulong)i, frames[i], where);
        else
            stdlib::sn_printf(buf, sizeof(buf), "  [%lu] 0x%lX\n",
                (ulong)i, frames[i]);
        print_output(buf);
    }
}

/* Where each loaded module sits, so a frame nothing could name -- or one read
   off a log later -- can still be placed */
void Panicker::dump_modules()
{
    static const ulong LINE_LEN_MAX = 256;
    char list[LINE_LEN_MAX];
    char buf[LINE_LEN_MAX + 16];

    if (!ModuleTable::get_instance().describe_all(list, sizeof(list)))
        return;

    stdlib::sn_printf(buf, sizeof(buf), "Modules: %s\n", list);
    print_output(buf);
}

bool Panicker::collect_remote_stack()
{
    if (collecting_.get() == 0)
        return false;

    ulong index = hal::get_current_cpu_hw_id();
    if (index >= REMOTE_CPU_COUNT)
        return false;

    if (remote_done_[index].cmpxchg(1, 0) != 0)
        return true; /* already recorded; still not our NMI to panic about */

    /* Walked from here: the frame pointer chain crosses the interrupt entry
       (the kernel is built with -fno-omit-frame-pointer), so this stack ends
       in whatever the CPU was doing when the NMI arrived -- which is the
       whole point of asking. */
    size_t count = StackTrace::capture(remote_frame_[index], REMOTE_FRAME_COUNT);
    remote_count_[index] = (u8)count;

    hal::smp_wmb();
    remote_done_[index].set(2);
    return true;
}

void Panicker::recover_fault()
{
    long probe = probe_cpu_.get();
    if (probe < 0 || active_.get() == 0)
        return;

    /* Which CPU this is, only once it can be asked: before the interrupt
       controller is up the boot CPU is the only one, and on x86 the asking
       is a read of the local APIC, not mapped yet -- a fault of its own */
    if (hal::irq_chip_ready() && probe != (long)hal::get_current_cpu_hw_id())
        return;

    probe_cpu_.set(-1);
    long_jmp(&probe_jump_, 1);
}

void Panicker::collect_remote_stacks()
{
    static_assert(REMOTE_CPU_COUNT == MAX_CPUS, "REMOTE_CPU_COUNT is out of step");

    if (!hal::irq_chip_ready() || !hal::nmi_ipi_supported())
        return;

    auto& table = CpuTable::get_instance();
    ulong self = table.get_current_cpu().get_index();
    ulong mask = table.get_running_cpus();

    for (ulong i = 0; i < REMOTE_CPU_COUNT; i++)
        remote_done_[i].set(0);

    collecting_.set(1);

    ulong asked = 0;
    ulong parked = 0;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(mask & (1UL << i)) || i == self)
            continue;

        /* Stopped for good in the halt path (Cpu::park): it has nothing to
           tell -- what it runs is the halt, on a stack of its own -- and the
           report says so in a line. */
        if (table.get_cpu(i).is_parked())
        {
            parked |= (1UL << i);
            continue;
        }

        asked |= (1UL << i);
        hal::send_nmi_ipi(i);
    }

    /* Bounded by iterations, not by the clock: a panic must not depend on a
       clock source that may be part of what broke. */
    static const ulong SPIN_BUDGET = 20000000;
    for (ulong spin = 0; spin < SPIN_BUDGET; spin++)
    {
        ulong done = 0;
        for (ulong i = 0; i < MAX_CPUS; i++)
        {
            if ((asked & (1UL << i)) && remote_done_[i].get() == 2)
                done |= (1UL << i);
        }

        if (done == asked)
            break;

        cpu_relax();
    }

    collecting_.set(0);

    char buf[64];
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (parked & (1UL << i))
        {
            stdlib::sn_printf(buf, sizeof(buf), "Cpu %lu parked\n", i);
            print_output(buf);
            continue;
        }

        if (!(asked & (1UL << i)))
            continue;

        if (remote_done_[i].get() != 2)
        {
            stdlib::sn_printf(buf, sizeof(buf), "Cpu %lu did not answer the NMI\n", i);
            print_output(buf);
            continue;
        }

        stdlib::sn_printf(buf, sizeof(buf), "Cpu %lu backtrace:\n", i);
        print_output(buf);
        dump_backtrace(remote_frame_[i], remote_count_[i]);
    }
}

void Panicker::do_panic(const char *fmt, ...)
{
    interrupt_disable();
    if (active_.cmpxchg(1, 0) == 0)
    {
        /* Before the first print_output: the report is appended behind whatever
           backlog the drain task still owes the collector, and PanicFlush()
           needs to know how much of it to skip. */
        rust_netconsole_panic_mark();

        va_list args;

        va_start(args, fmt);
        stdlib::vsn_printf(message_, sizeof(message_), fmt, args);
        va_end(args);

        print_output(message_);
        dump_context();

        ulong frames[16];
        size_t count = StackTrace::capture(frames, stdlib::array_size(frames));
        dump_backtrace(frames, count);
        dump_modules();

        /* Before the halting IPI: the CPU worth asking has interrupts off and
           would never take that one. */
        collect_remote_stacks();

        if (hal::irq_chip_ready())
        {
            Cpu& cpu = CpuTable::get_instance().get_current_cpu();
            CpuTable::get_instance().send_ipi_all_exclude(cpu.get_index());
        }

        /* Last thing done, and best-effort by construction: the console
           already has the whole report, so a TX path that turns out to be
           wedged costs nothing that was still needed. */
        rust_netconsole_panic_flush();
        rust_disklog_panic_flush();

        reset_if_asked();
    }

    for (;;)
    {
        cpu_relax();
    }
}

void Panicker::do_panic_ctx(Context* ctx, bool has_error_code, const char *fmt, ...)
{
    interrupt_disable();
    if (active_.cmpxchg(1, 0) == 0)
    {
        /* Before the first print_output: the report is appended behind whatever
           backlog the drain task still owes the collector, and PanicFlush()
           needs to know how much of it to skip. */
        rust_netconsole_panic_mark();

        va_list args;

        va_start(args, fmt);
        stdlib::vsn_printf(message_, sizeof(message_), fmt, args);
        va_end(args);

        print_output(message_);
        dump_context();

        /* Walk from the faulting code's RBP, over the stack its own RSP
           names: #DF arrives on a separate IST stack, and the report wanted
           is about the stack it came from. Which is the likeliest thing
           there to be broken -- a double fault is a fault the CPU could not
           push onto that stack -- so the walk reads it as memory that may
           not be there: a fault in it comes back here (recover_fault), and
           the report goes on without it. It used to fault itself, a panic
           inside the panic, and stop: no other CPU's stack, no netconsole,
           no reset. */
        if (set_jmp(&probe_jump_) == 0)
        {
            probe_cpu_.set(hal::irq_chip_ready() ? (long)hal::get_current_cpu_hw_id() : 0);
            ulong frames[16];
            size_t count = StackTrace::capture_from_sp(ctx->get_frame_pointer(),
                ctx->get_orig_rsp(has_error_code), frames, stdlib::array_size(frames));
            probe_cpu_.set(-1);
            dump_backtrace(frames, count);
        }
        else
        {
            /* Then at least where it was, which needs no stack */
            char where[SymbolTable::DESCRIBE_MAX];
            char buf[SymbolTable::DESCRIBE_MAX + 64];
            if (SymbolTable::get_instance().describe(ctx->get_ret_rip(has_error_code), where, sizeof(where)))
                stdlib::sn_printf(buf, sizeof(buf),
                    "Backtrace: none, the stack it came from cannot be read; at %s\n", where);
            else
                stdlib::sn_printf(buf, sizeof(buf),
                    "Backtrace: none, the stack it came from cannot be read\n");
            print_output(buf);
        }
        dump_modules();

        collect_remote_stacks();

        if (hal::irq_chip_ready())
        {
            Cpu& cpu = CpuTable::get_instance().get_current_cpu();
            CpuTable::get_instance().send_ipi_all_exclude(cpu.get_index());
        }

        /* Last thing done, and best-effort by construction: the console
           already has the whole report, so a TX path that turns out to be
           wedged costs nothing that was still needed. */
        rust_netconsole_panic_flush();
        rust_disklog_panic_flush();

        reset_if_asked();
    }

    for (;;)
    {
        cpu_relax();
    }
}

}