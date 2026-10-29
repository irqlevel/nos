#include "trace.h"
#include "input.h"
#include "panic.h"
#include "time.h"
#include "mutex.h"
#include "wait_group.h"
#include "event.h"
#include "spin_lock.h"
#include "raw_spin_lock.h"
#include "raw_rw_spin_lock.h"
#include "rw_mutex.h"
#include "task.h"
#include "sched.h"
#include "preempt.h"
#include "cpu.h"
#include "interrupt.h"
#include <hal/cpu.h>
#include <hal/random.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>
#include "softirq.h"
#include "timer.h"
#include "cmd.h"
#include <mm/new.h>
#include <mm/page_allocator.h>
#include <mm/page_table.h>
#include <lib/stdlib.h>
#include <drivers/pci.h>
#include <drivers/msix.h>
#include <hal/irqchip.h>
#include <drivers/hpet.h>
#include <drivers/acpi.h>
#include "parameters.h"
#include "version_gen.h"
#include "dmesg.h"
#include "module.h"
#include "object_table.h"
#include <hal/console.h>

static const ulong RUST_ALLOC_TAG = 'rust';

/* kernel_printer_write hands a Printer at most this much at a time */
static const unsigned long PRINTER_CHUNK_SIZE = 128;

/* Longer than any net device's name: VirtioNet's are at most 7 */

/* kernel_cmd_dispatch: the longest command line it runs -- as long as a line
   of /etc/rc (cmd.cpp's SCRIPT_LINE_MAX): a guest's `hv start` over SSH with a
   distribution's kernel command line runs past two hundred characters before
   it says anything of its own, and was refused here at 255 on the AX41. The
   line is copied through a buffer this long on the stack of the task that
   dispatches it -- an SSH session's or the UDP shell's, 64 KiB each. The UDP
   shell's own datagrams stay at its CMD_MAX. */
static const unsigned long DISPATCH_LINE_MAX = 1023;

/* kernel_cmd_dispatch: how much of a command's output waits in memory for a
   moment it may be sent */
static const unsigned long SINK_BUFFER_SIZE = 64 * 1024;

/* What a command prints, for Rust -- an SSH session's channel -- instead of
   a console. A command may print holding a spinlock with interrupts off
   (ps, stacks, arp) or a mutex (ls holds the Vfs's), and passing text on
   means the network: allocating, and waiting for room as long as the client
   takes. So the text goes into Buf, and on to Fn only where the caller may
   block (preempt_can_block): when Buf is full; at a line end once its oldest
   byte has waited FLUSH_DELAY_NS, so a command printing a line a second is
   seen as it prints while one printing everything at once goes out after it
   returns, its locks let go; and whatever is left when it has. What does
   not fit while nothing may be sent is dropped, and said so at the end. */
class SinkPrinter final : public stdlib::Printer
{
public:
    typedef void (*SinkFn)(void* ctx, const unsigned char* buf, unsigned long len);
    typedef long (*SourceFn)(void* ctx, unsigned char* buf, unsigned long len,
        unsigned long long timeout_ns);
    typedef bool (*TerminalFn)(void* ctx, stdlib::TerminalInfo* info);

    SinkPrinter(SinkFn fn, SourceFn src, TerminalFn term, void* ctx, unsigned char* buf, unsigned long size)
        : fn_(fn)
        , src_(src)
        , term_(term)
        , ctx_(ctx)
        , buf_(buf)
        , size_(size)
        , len_(0)
        , oldest_(0)
        , dropped_(0)
    {
    }

    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        v_printf(fmt, args);
        va_end(args);
    }

    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) override
    {
        char text[FORMAT_MAX];
        if (stdlib::vsn_printf(text, sizeof(text), fmt, args) < 0)
            return;
        put(text);
    }

    virtual void print_string(const char *s) override
    {
        if (s != nullptr)
            put(s);
    }

    virtual void backspace() override
    {
    }

    /* From the session's source, for a command that asks -- after what it
       has printed has gone: a prompt, or the echo of what was typed, has no
       line end to send it on its way, and a command reading its answer must
       not leave the question waiting in Buf. Not where the caller may not
       block: the source waits for the network. */
    virtual long read_input(unsigned char* buf, unsigned long len, unsigned long long timeout_ns) override
    {
        if (src_ == nullptr || buf == nullptr || len == 0 || !kernel::preempt_can_block())
            return -1;

        flush();
        return src_(ctx_, buf, len, timeout_ns);
    }

    /* From the session's own, for a command that asks: the session keeps
       it under a lock that sleeps, so not where the caller may not */
    virtual bool terminal(stdlib::TerminalInfo& info) override
    {
        if (term_ == nullptr || !kernel::preempt_can_block())
            return false;

        return term_(ctx_, &info);
    }

    /* The command has returned: what is left goes, and what was lost is
       said */
    void finish()
    {
        flush();
        if (dropped_ == 0)
            return;

        char note[128];
        stdlib::BufferPrinter bp(note, sizeof(note));
        bp.printf("\n[%lu bytes of output dropped: printed with a lock held, past the %lu that can wait]\n",
            dropped_, size_);
        fn_(ctx_, reinterpret_cast<const unsigned char*>(note), stdlib::str_len(note));
        dropped_ = 0;
    }

private:
    SinkPrinter(const SinkPrinter& other) = delete;
    SinkPrinter& operator=(const SinkPrinter& other) = delete;

    void put(const char* s)
    {
        bool line_end = false;
        for (; *s != '\0'; s++)
        {
            if (len_ == size_)
            {
                if (!kernel::preempt_can_block())
                {
                    dropped_++;
                    continue;
                }
                flush();
            }
            if (len_ == 0)
                oldest_ = kernel::get_boot_time().get_value();
            buf_[len_++] = (unsigned char)*s;
            if (*s == '\n' || *s == '\r')
                line_end = true;
        }

        if (line_end && len_ != 0 &&
            kernel::get_boot_time().get_value() - oldest_ >= FLUSH_DELAY_NS &&
            kernel::preempt_can_block())
            flush();
    }

    void flush()
    {
        if (len_ != 0)
        {
            fn_(ctx_, buf_, len_);
            len_ = 0;
        }
    }

    static const unsigned long FORMAT_MAX = 512;
    static const unsigned long long FLUSH_DELAY_NS = 50ULL * 1000 * 1000;

    SinkFn fn_;
    SourceFn src_;
    TerminalFn term_;
    void* ctx_;
    unsigned char* buf_;
    unsigned long size_;
    unsigned long len_;
    unsigned long long oldest_;
    unsigned long dropped_;
};

extern "C" {

void kernel_trace(unsigned int level, const unsigned char* msg, unsigned long len)
{
    /* The level, as trace() has it. A trace!(3, ...) used to be printed at
       every level: the NVMe interrupt handler's "IRQ spurious (no CQEs)",
       forty thousand times in a few seconds of netblk load, each one
       formatted and logged from the handler. */
    if (level > (unsigned int)kernel::Tracer::get_instance().get_level())
        return;

    char buf[512];
    unsigned long n = (len < sizeof(buf) - 1) ? len : sizeof(buf) - 1;
    stdlib::mem_cpy(buf, msg, n);
    buf[n] = '\0';
    auto time = kernel::get_boot_time();
    kernel::Tracer::get_instance().output("%u:%lu.%06lu:%s\n",
        level, time.get_secs(), time.get_usecs(), buf);
}

void* kernel_alloc(unsigned long size, unsigned long align)
{
    if (align == 0 || (align & (align - 1)) != 0)
        PANIC("kernel_alloc: bad align %lu", align);

    /* mm::alloc guarantees 8-byte alignment. alloc_uninit: GlobalAlloc::alloc
       promises nothing about the contents, and alloc_zeroed zeroes them on
       the Rust side. */
    if (align <= 8)
        return kernel::mm::alloc_uninit(size, RUST_ALLOC_TAG);

    /* Over-aligned (#[repr(align)] / SIMD types): over-allocate and stash
       the original pointer just below the aligned address for kernel_free */
    if (size > (unsigned long)-1 - align - sizeof(void*))
        return nullptr;
    void* raw = kernel::mm::alloc_uninit(size + align + sizeof(void*), RUST_ALLOC_TAG);
    if (raw == nullptr)
        return nullptr;
    unsigned long aligned =
        stdlib::round_up((unsigned long)raw + sizeof(void*), align);
    ((void**)aligned)[-1] = raw;
    return (void*)aligned;
}

void kernel_free(void* ptr, unsigned long size, unsigned long align)
{
    (void)size;
    if (ptr == nullptr)
        return;
    if (align > 8)
        ptr = ((void**)ptr)[-1];
    kernel::mm::free(ptr);
}

[[noreturn]] void kernel_panic(const unsigned char* msg, unsigned long len)
{
    /* Big enough for a panic from a UB check, whose message alone runs past
       200 characters; what does not fit is truncated here and marked by
       vsn_printf below. */
    char buf[512];
    unsigned long n = (len < sizeof(buf) - 1) ? len : sizeof(buf) - 1;
    stdlib::mem_cpy(buf, msg, n);
    buf[n] = '\0';
    kernel::Panicker::get_instance().do_panic("RUST PANIC: %s\n", buf);
    /* Not an empty loop: one with no side effect is one the compiler may
       assume ends, and take out -- and this function would fall through
       into whatever follows it */
    for (;;)
        cpu_relax();
}

void kernel_get_boot_time(unsigned long* secs, unsigned long* usecs)
{
    if (!secs || !usecs)
        return;
    auto t = kernel::get_boot_time();
    *secs = t.get_secs();
    *usecs = t.get_usecs();
}

unsigned long long kernel_get_boot_time_ns()
{
    return kernel::get_boot_time().get_value();
}

unsigned long long kernel_cycle_counter_hz()
{
    return hal::cycle_counter_hz();
}

/* The cycle counter as it stands: the TSC on x86-64, the generic timer's
   virtual count on arm64 */
unsigned long long kernel_read_cycle_counter()
{
    return hal::read_cycle_counter();
}

unsigned long kernel_get_wall_time_secs()
{
    return kernel::get_wall_time_secs();
}

unsigned long kernel_mutex_create()
{
    kernel::Mutex* m = kernel::mm::talloc<kernel::Mutex, RUST_ALLOC_TAG>();
    return (unsigned long)m;
}

void kernel_mutex_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    kernel::Mutex* m = reinterpret_cast<kernel::Mutex*>(handle);
    m->~Mutex();
    kernel::mm::free(m);
}

void kernel_mutex_lock(unsigned long handle)
{
    reinterpret_cast<kernel::Mutex*>(handle)->lock();
}

void kernel_mutex_unlock(unsigned long handle)
{
    reinterpret_cast<kernel::Mutex*>(handle)->unlock();
}

unsigned long kernel_spinlock_create()
{
    kernel::SpinLock* s = kernel::mm::talloc<kernel::SpinLock, RUST_ALLOC_TAG>();
    return (unsigned long)s;
}

void kernel_spinlock_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    kernel::SpinLock* s = reinterpret_cast<kernel::SpinLock*>(handle);
    s->~SpinLock();
    kernel::mm::free(s);
}

unsigned long long kernel_spinlock_lock(unsigned long handle)
{
    unsigned long flags = 0;
    reinterpret_cast<kernel::SpinLock*>(handle)->lock(flags);
    return (unsigned long long)flags;
}

void kernel_spinlock_unlock(unsigned long handle, unsigned long long flags)
{
    reinterpret_cast<kernel::SpinLock*>(handle)->unlock((unsigned long)flags);
}

unsigned long kernel_waitgroup_create()
{
    kernel::WaitGroup* wg = kernel::mm::talloc<kernel::WaitGroup, RUST_ALLOC_TAG>();
    return (unsigned long)wg;
}

void kernel_waitgroup_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    kernel::WaitGroup* wg = reinterpret_cast<kernel::WaitGroup*>(handle);
    wg->~WaitGroup();
    kernel::mm::free(wg);
}

void kernel_waitgroup_add(unsigned long handle, long delta)
{
    reinterpret_cast<kernel::WaitGroup*>(handle)->add(delta);
}

void kernel_waitgroup_done(unsigned long handle)
{
    reinterpret_cast<kernel::WaitGroup*>(handle)->done();
}

void kernel_waitgroup_wait(unsigned long handle)
{
    reinterpret_cast<kernel::WaitGroup*>(handle)->wait();
}

unsigned long kernel_event_create()
{
    kernel::Event* e = kernel::mm::talloc<kernel::Event, RUST_ALLOC_TAG>();
    return (unsigned long)e;
}

void kernel_event_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    kernel::Event* e = reinterpret_cast<kernel::Event*>(handle);
    e->~Event();
    kernel::mm::free(e);
}

void kernel_event_wait(unsigned long handle)
{
    reinterpret_cast<kernel::Event*>(handle)->wait();
}

int kernel_event_wait_for(unsigned long handle, unsigned long long timeout_ns)
{
    return reinterpret_cast<kernel::Event*>(handle)->wait_for(timeout_ns) ? 1 : 0;
}

void kernel_event_signal(unsigned long handle)
{
    reinterpret_cast<kernel::Event*>(handle)->signal();
}

/* The name ps and top show for a task Rust starts: the caller's, a Rust
   string with no terminator, cut to the room a task has for one */
static kernel::Task* new_rust_task(const unsigned char* name, unsigned long name_len)
{
    char buf[kernel::TASK_NAME_LEN];
    const unsigned long len = stdlib::min(name_len, (unsigned long)sizeof(buf) - 1);

    stdlib::mem_cpy(buf, name, len);
    buf[len] = '\0';
    /* A name, not a format: a '%' in it is the caller's */
    return kernel::mm::talloc<kernel::Task, RUST_ALLOC_TAG>(buf);
}

unsigned long kernel_task_spawn(const unsigned char* name, unsigned long name_len,
    void (*func)(void*), void* ctx)
{
    kernel::Task* t = new_rust_task(name, name_len);
    if (!t)
        return 0;
    if (!t->start(func, ctx))
    {
        t->put();
        return 0;
    }
    return (unsigned long)t;
}

unsigned long kernel_task_spawn_on(const unsigned char* name, unsigned long name_len,
    void (*func)(void*), void* ctx,
    unsigned long affinity_mask)
{
    kernel::Task* t = new_rust_task(name, name_len);
    if (!t)
        return 0;
    t->set_cpu_affinity(affinity_mask);
    if (!t->start(func, ctx))
    {
        t->put();
        return 0;
    }
    return (unsigned long)t;
}

void kernel_task_wait(unsigned long handle)
{
    reinterpret_cast<kernel::Task*>(handle)->wait();
}

void kernel_task_set_stopping(unsigned long handle)
{
    reinterpret_cast<kernel::Task*>(handle)->set_stopping();
}

/* Whether the calling task has been asked to stop: what a loop that runs
   for the life of the kernel checks each time round. */
int kernel_task_stopping()
{
    kernel::Task* task = kernel::Task::get_current_task();
    return (task != nullptr && task->is_stopping()) ? 1 : 0;
}

void kernel_task_put(unsigned long handle)
{
    reinterpret_cast<kernel::Task*>(handle)->put();
}

void kernel_sleep_ns(unsigned long long ns)
{
    kernel::sleep((ulong)ns);
}

void kernel_task_yield_to_runnable()
{
    kernel::yield_to_runnable();
}

/* The calling task -- a handle to compare with a TaskHandle's, never to
   wait on or put */
unsigned long kernel_task_current()
{
    return (unsigned long)kernel::Task::get_current_task();
}

/* The same, but 0 rather than a complaint when the stack is not a task's.
   For code that runs wherever it is called from -- the tracer's side of the
   disk log above all, where the complaint would itself trace and recurse. */
unsigned long kernel_task_current_or_none()
{
    return (unsigned long)kernel::Task::try_get_current_task();
}

unsigned int kernel_get_cpu_id()
{
    return (unsigned int)kernel::get_cpu().get_index();
}

unsigned int kernel_cpu_count()
{
    ulong mask = kernel::CpuTable::get_instance().get_running_cpus();
    unsigned int count = 0;
    while (mask) { count += mask & 1; mask >>= 1; }
    return count;
}

unsigned long kernel_cpu_online_mask()
{
    return kernel::CpuTable::get_instance().get_running_cpus();
}

} /* extern "C" */

struct RustIPIAdapter
{
    void (*handler)(void*);
    void* ctx;
};

static void rust_ipi_trampoline(void* actx, kernel::Context*)
{
    auto* a = static_cast<RustIPIAdapter*>(actx);
    a->handler(a->ctx);
}

extern "C" {

void kernel_cpu_run_on(unsigned int cpu,
    void (*handler)(void*), void* ctx)
{
    if (!handler || cpu >= (unsigned int)kernel::MAX_CPUS)
        return;

    /* A CPU that never started or already exited will not drain its IPI task
       list; queueing would block Completion.wait() forever (send_ipi_self
       silently refuses to send in those states). */
    auto& target = kernel::CpuTable::get_instance().get_cpu(cpu);
    ulong state = target.get_state();
    if (!(state & kernel::Cpu::STATE_RUNNING) ||
        (state & kernel::Cpu::STATE_EXITING) || target.is_parked())
    {
        trace(0, "kernel_cpu_run_on: cpu %lu not running (state 0x%lX)",
              (ulong)cpu, state);
        return;
    }

    RustIPIAdapter a{handler, ctx};
    kernel::IPITask task(rust_ipi_trampoline, &a);
    target.queue_ipi_task(task);
}

/* An interrupt to `cpu`, now, and nothing waited for: what makes a vCPU that
   is running a guest there leave it -- a physical interrupt ends the guest's
   turn -- and costs any other CPU an interrupt with nothing to do. Sent the
   way SoftIrq's kick is, straight to the interrupt controller: the caller may
   have interrupts off, and CpuTable::send_ipi takes locks its handler takes
   too. A CPU that is not running holds it pending, or never sees it. */
void kernel_cpu_kick(unsigned int cpu)
{
    if (cpu >= (unsigned int)kernel::MAX_CPUS)
        return;

    hal::send_ipi(cpu, kernel::CpuTable::IPI_VECTOR);
}

/* The notification of an interrupt posted for the vCPU running on `cpu`
   (hal::POSTED_VECTOR): the CPU takes it in the guest without an exit, and
   out of one it is an interrupt that only ends itself. Sent as the kick is,
   from any context. */
void kernel_cpu_notify(unsigned int cpu)
{
    if (cpu >= (unsigned int)kernel::MAX_CPUS)
        return;

    hal::send_ipi(cpu, hal::POSTED_VECTOR);
}

/* The vector kernel_cpu_notify sends: what a VMCS names as its
   posted-interrupt notification vector. */
unsigned char kernel_posted_interrupt_vector(void)
{
    return hal::POSTED_VECTOR;
}

/* The CPU this runs on to take an interrupt from its own timer by
   deadline_ns on the boot clock, if it is not armed to fire sooner
   (Cpu::local_timer_wake): for a vCPU about to enter its guest, whose timer's
   next edge would otherwise wait for its next exit -- the host's tick, for a
   guest that computes without one. Nothing for a deadline that has passed:
   armed for one, the timer would fire at once, and again at every entry
   while the guest cannot take what is due. From any context. */
void kernel_cpu_timer_at(unsigned long long deadline_ns)
{
    ulong flags = hal::irq_save();
    if ((ulong)deadline_ns > kernel::get_boot_time().get_value())
        kernel::CpuTable::get_instance().get_current_cpu().local_timer_wake((ulong)deadline_ns);
    hal::irq_restore(flags);
}

void* kernel_alloc_dma_pages(unsigned long count,
    unsigned long* phys_out, unsigned long* actual_pages_out)
{
    if (count == 0 || !phys_out || !actual_pages_out)
        return nullptr;

    unsigned long phys = 0;
    void* p = kernel::mm::alloc_map_pages((size_t)count, &phys);
    if (!p)
    {
        *phys_out = 0;
        *actual_pages_out = 0;
        return nullptr;
    }

    *phys_out = phys;
    *actual_pages_out = (unsigned long)(1UL << stdlib::log2((size_t)count));
    return p;
}

void kernel_free_dma_pages(void* ptr)
{
    if (ptr)
        kernel::mm::unmap_free_pages(ptr);
}

void* kernel_map_phys(unsigned long phys_base, unsigned long num_pages)
{
    if (num_pages == 0)
        return nullptr;

    ulong size = num_pages * consts::PAGE_SIZE;
    ulong va = kernel::mm::PageTable::get_instance().map_mmio_region(phys_base, size);
    if (va == 0)
        return nullptr;
    return (void*)va;
}

void kernel_unmap_phys(void* virt_addr, unsigned long num_pages)
{
    /* map_mmio_region maps at phys_addr + KERNEL_SPACE_BASE via direct PTEs;
     * these mappings are permanent and cannot be unmapped in the current
     * page-table implementation. */
    (void)virt_addr;
    (void)num_pages;
}

unsigned long kernel_virt_to_phys(const void* virt_addr)
{
    if (!virt_addr)
        return 0;
    return kernel::mm::PageTable::get_instance().virt_to_phys((ulong)virt_addr);
}

/* ---- Frames: pages of RAM mapped nowhere (kcore::frame) ---- */

unsigned long kernel_frame_alloc()
{
    auto& pt = kernel::mm::PageTable::get_instance();
    /* alloc_page_keeping zeroes what it hands out, through the temp map, as
       alloc_page does; it leaves the kernel its reserve (get_kernel_reserve_pages). */
    kernel::mm::Page* page = pt.alloc_page_keeping(pt.get_kernel_reserve_pages());
    return page ? page->get_phy_address() : 0;
}

/* A block of the huge pool, zeroed: its address, or 0 when the pool has
   none free -- or is not there, hugepages= not given. */
unsigned long kernel_frame_alloc_huge()
{
    return kernel::mm::PageTable::get_instance().alloc_huge_frame();
}

/* Back into the pool: a panic for an address that is no block of it, or
   one back already. */
void kernel_frame_free_huge(unsigned long phys)
{
    kernel::mm::PageTable::get_instance().free_huge_frame(phys);
}

/* The pool's blocks, and how many of them are free. */
unsigned long kernel_frame_huge_blocks()
{
    return kernel::mm::PageTable::get_instance().get_huge_pool_blocks();
}

unsigned long kernel_frame_huge_free()
{
    return kernel::mm::PageTable::get_instance().get_huge_pool_free();
}

void kernel_frame_free(unsigned long phys)
{
    kernel::mm::PageTable::get_instance().free_frame(phys);
}

/* Copy between a frame and the caller's memory through this CPU's frame
   slot of the temp-map window (PageTable::map_frame_slot): out of the frame
   into `buf`, or `data` into it -- exactly one of the two is given.
   Preemption and interrupts are off from the map to the unmap: that is what
   makes the slot this copy's alone, and the unmap invalidates only this
   CPU's TLB, so a task moved to another CPU mid-copy would leave the slot's
   translation stale behind it. (It went through the shared slots, under
   their lock and with a reference taken and a flush at each end, until the
   lock and the flushes were as much of a busy guest's vCPU as the copy.)

   What a frame holds may be a guest's memory, written while the copy runs,
   so nothing here may hold a view of it the compiler could reason about.
   The range is one call of the architecture's memcpy -- assembly on x86, a
   function of its own on arm64 -- which the compiler sees nothing of: each
   byte is read or written once, where the copy says, and a guest racing its
   own buffer gets what it raced for. (It used to go a word at a time
   through a volatile pointer, with a mem_cpy call for each word on the
   caller's side: 512 calls a page, and two thirds of a busy guest's vCPU
   while it wrote to its disk.) */
static int frame_copy(unsigned long phys, unsigned long offset,
    unsigned char* buf, const unsigned char* data, unsigned long len)
{
    if (len == 0)
        return 0;
    if ((buf == nullptr) == (data == nullptr))
        return -1;
    if (offset >= consts::PAGE_SIZE || len > consts::PAGE_SIZE - offset)
        return -1;

    auto& pt = kernel::mm::PageTable::get_instance();
    if (!pt.is_frame_address(phys))
        return -1;

    /* Through this CPU's own slot of the window, which interrupts off makes
       the copy's alone: no lock, no slot to search for, no reference. */
    ulong flags = kernel::preempt_irq_save();
    ulong va = pt.map_frame_slot(phys);

    /* Inside the page mapped at va: offset and len were checked above. */
    void* frame = reinterpret_cast<void*>(va + offset);
    if (data != nullptr)
        stdlib::mem_cpy(frame, data, len);
    else
        stdlib::mem_cpy(buf, frame, len);

    pt.unmap_frame_slot(va);
    kernel::preempt_irq_restore(flags);
    return 0;
}

int kernel_frame_read(unsigned long phys, unsigned long offset,
    unsigned char* buf, unsigned long len)
{
    return frame_copy(phys, offset, buf, nullptr, len);
}

int kernel_frame_write(unsigned long phys, unsigned long offset,
    const unsigned char* data, unsigned long len)
{
    return frame_copy(phys, offset, nullptr, data, len);
}

/* One 16-bit word of a frame, loaded or stored whole: a single access,
   which frame_copy's mem_cpy is not -- on x86 a `rep movsb`, a byte after a
   byte. A word a guest's other CPUs write while the host reads it -- a
   virtqueue's index -- has to be read whole: the high byte of 0x0100 with
   the low byte of the 0x00FF it replaced is 0x01FF, a ring that looks
   overrun. The load is an acquire and the store a release: what a driver
   published before its index is there to be read after the index is, and
   what this side wrote before its own index is there before the index
   says so. The same slot of the window as frame_copy's, with preemption and
   interrupts off for the same reason. -1, and nothing done, for a word
   that is not an aligned one inside a page of RAM. */
static bool frame_word_inside(unsigned long phys, unsigned long offset)
{
    return offset < consts::PAGE_SIZE && offset % sizeof(unsigned short) == 0
        && kernel::mm::PageTable::get_instance().is_frame_address(phys);
}

int kernel_frame_load16(unsigned long phys, unsigned long offset, unsigned short* value)
{
    if (value == nullptr || !frame_word_inside(phys, offset))
        return -1;

    auto& pt = kernel::mm::PageTable::get_instance();
    ulong flags = kernel::preempt_irq_save();
    ulong va = pt.map_frame_slot(phys);
    /* Aligned, inside the page mapped at va: checked above. */
    *value = __atomic_load_n(reinterpret_cast<unsigned short*>(va + offset), __ATOMIC_ACQUIRE);
    pt.unmap_frame_slot(va);
    kernel::preempt_irq_restore(flags);
    return 0;
}

int kernel_frame_store16(unsigned long phys, unsigned long offset, unsigned short value)
{
    if (!frame_word_inside(phys, offset))
        return -1;

    auto& pt = kernel::mm::PageTable::get_instance();
    ulong flags = kernel::preempt_irq_save();
    ulong va = pt.map_frame_slot(phys);
    __atomic_store_n(reinterpret_cast<unsigned short*>(va + offset), value, __ATOMIC_RELEASE);
    pt.unmap_frame_slot(va);
    kernel::preempt_irq_restore(flags);
    return 0;
}

/* A few frames mapped into the kernel's space together, in the order given,
   read-write and never executable, for as long as the Rust side keeps them
   so (kcore::frame::Mapped): a guest's pages the host reads and writes at
   every frame it handles -- a virtqueue's rings -- which frame_copy maps and
   unmaps again at every access, a TLB flush each time. Through map_pages, so
   the VA is the allocator's to track. The frames stay the caller's, and
   allocated until kernel_frames_unmap has run: the mapping holds them by
   their address and nothing else. 0 when there are none or more than
   FRAMES_MAP_MAX, one is no frame, or no address is left. */
static const unsigned long FRAMES_MAP_MAX = 4;

unsigned long kernel_frames_map(const unsigned long* phys, unsigned long count)
{
    if (phys == nullptr || count == 0 || count > FRAMES_MAP_MAX)
        return 0;

    auto& pt = kernel::mm::PageTable::get_instance();
    ulong frames[FRAMES_MAP_MAX];
    for (unsigned long i = 0; i < count; i++)
    {
        if (!pt.is_frame_address(phys[i]))
            return 0;
        frames[i] = phys[i];
    }
    return reinterpret_cast<unsigned long>(kernel::mm::map_pages(count, frames));
}

/* A kernel_frames_map undone: `va` and `count` as it returned and was
   given. The unmap shoots the address down on every CPU and waits for each
   to answer, so it is never called with a spin lock held. */
void kernel_frames_unmap(unsigned long va, unsigned long count)
{
    kernel::mm::unmap_pages(reinterpret_cast<void*>(va), count);
}

/* ---- Soft IRQ ---- */

void kernel_softirq_raise(unsigned long type)
{
    kernel::SoftIrq::get_instance().raise(type);
}

/* Whether that soft IRQ is already asked for. What lets a poll tell a pass
   it caused from one an interrupt caused. */
int kernel_softirq_pending(unsigned long type)
{
    return kernel::SoftIrq::get_instance().is_pending(type) ? 1 : 0;
}

void kernel_softirq_register(unsigned long type,
    void (*handler)(void*), void* ctx)
{
    kernel::SoftIrq::get_instance().register_handler(type, handler, ctx);
}

/* ---- PCI ---- */

struct RustPciDeviceInfo
{
    unsigned short bus;
    unsigned short slot;
    unsigned short func;
    unsigned short vendor;
    unsigned short device;
    unsigned char cls;
    unsigned char subclass;
    unsigned char prog_if;
    unsigned char revision;
    unsigned char irq_line;
    unsigned char irq_pin;
};

static void fill_rust_pci_info(RustPciDeviceInfo* out, const Pci::DeviceInfo* dev)
{
    out->bus = dev->bus;
    out->slot = dev->slot;
    out->func = dev->func;
    out->vendor = dev->vendor;
    out->device = dev->device;
    out->cls = dev->base_class;
    out->subclass = dev->sub_class;
    out->prog_if = dev->prog_if;
    out->revision = dev->revision_id;
    out->irq_line = dev->interrupt_line;
    out->irq_pin = dev->interrupt_pin;
}

long kernel_pci_find_device(unsigned short vendor, unsigned short device,
    unsigned long start_index, RustPciDeviceInfo* out)
{
    if (!out)
        return -1;
    auto& pci = Pci::get_instance();
    for (unsigned long i = start_index; i < pci.get_device_count(); i++)
    {
        auto* dev = pci.get_device(i);
        if (dev && dev->valid && dev->vendor == vendor && dev->device == device)
        {
            fill_rust_pci_info(out, dev);
            return (long)i;
        }
    }
    return -1;
}

unsigned long kernel_pci_device_count()
{
    return Pci::get_instance().get_device_count();
}

int kernel_pci_get_device(unsigned long index, RustPciDeviceInfo* out)
{
    if (!out)
        return 0;
    auto* dev = Pci::get_instance().get_device(index);
    if (!dev || !dev->valid)
        return 0;
    fill_rust_pci_info(out, dev);
    return 1;
}

unsigned int kernel_pci_get_bar(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned char bar)
{
    return Pci::get_instance().get_bar(bus, slot, func, bar);
}

void kernel_pci_enable_bus_mastering(unsigned short bus, unsigned short slot,
    unsigned short func)
{
    Pci::get_instance().enable_bus_mastering(bus, slot, func);
}

unsigned char kernel_pci_find_capability(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned char cap_id, unsigned char start_offset)
{
    return Pci::get_instance().find_capability(bus, slot, func, cap_id, start_offset);
}

unsigned char kernel_pci_read_config8(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset)
{
    return Pci::get_instance().read_byte(bus, slot, func, offset);
}

unsigned short kernel_pci_read_config16(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset)
{
    return Pci::get_instance().read_word(bus, slot, func, offset);
}

unsigned int kernel_pci_read_config32(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset)
{
    return Pci::get_instance().read_dword(bus, slot, func, offset);
}

void kernel_pci_write_config8(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset, unsigned char val)
{
    Pci::get_instance().write_byte(bus, slot, func, offset, val);
}

void kernel_pci_write_config16(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset, unsigned short val)
{
    Pci::get_instance().write_word(bus, slot, func, offset, val);
}

void kernel_pci_write_config32(unsigned short bus, unsigned short slot,
    unsigned short func, unsigned short offset, unsigned int val)
{
    Pci::get_instance().write_dword(bus, slot, func, offset, val);
}

/* ---- MSI-X ---- */

static Pci::DeviceInfo* find_pci_dev_by_bdf(unsigned short bus, unsigned short slot,
    unsigned short func)
{
    auto& pci = Pci::get_instance();
    for (ulong i = 0; i < pci.get_device_count(); i++)
    {
        auto* d = pci.get_device(i);
        if (d && d->valid && d->bus == bus && d->slot == slot && d->func == func)
            return d;
    }
    return nullptr;
}

unsigned long kernel_msix_create(unsigned short bus, unsigned short slot,
    unsigned short func, const unsigned long* mapped_bars)
{
    auto* dev = find_pci_dev_by_bdf(bus, slot, func);
    if (!dev)
        return 0;

    auto* t = kernel::mm::talloc<kernel::MsixTable, RUST_ALLOC_TAG>();
    if (!t)
        return 0;

    if (!t->setup(dev, mapped_bars))
    {
        t->~MsixTable();
        kernel::mm::free(t);
        return 0;
    }
    return (unsigned long)t;
}

void kernel_msix_destroy(unsigned long handle)
{
    if (handle == 0)
        return;
    auto* t = reinterpret_cast<kernel::MsixTable*>(handle);
    t->~MsixTable();
    kernel::mm::free(t);
}

void kernel_msix_mask(unsigned long handle, unsigned short index)
{
    if (handle == 0)
        return;
    reinterpret_cast<kernel::MsixTable*>(handle)->mask(index);
}

void kernel_msix_unmask(unsigned long handle, unsigned short index)
{
    if (handle == 0)
        return;
    reinterpret_cast<kernel::MsixTable*>(handle)->unmask(index);
}

unsigned short kernel_msix_table_size(unsigned long handle)
{
    if (handle == 0)
        return 0;
    return reinterpret_cast<kernel::MsixTable*>(handle)->get_table_size();
}

int kernel_msix_is_ready(unsigned long handle)
{
    if (handle == 0)
        return 0;
    return reinterpret_cast<kernel::MsixTable*>(handle)->is_ready() ? 1 : 0;
}

} /* extern "C" */

/* ---- Legacy (INTx) interrupts ---- */

static const ulong RUST_IRQ_SLOT_COUNT = 8;
static const u8 RUST_IRQ_VECTOR_BASE = 0x38;

/* Every member initialised, so that the static table is too: a default
   constructor that left one indeterminate is not a constant expression, and
   the compiler then made a runtime constructor this kernel never runs. */
struct RustIrqSlot
{
    void (*handler)(void*) = nullptr;
    void* ctx = nullptr;
    u8 vector = 0;
    bool used = false;
    kernel::Atomic in_flight; /* ISRs currently executing Handler */
};

static RustIrqSlot rust_irq_slots[RUST_IRQ_SLOT_COUNT];
static kernel::RawRwSpinLock rust_irq_lock;

static kernel::InterruptHandlerFn rust_stub_table[RUST_IRQ_SLOT_COUNT] = {
    rust_interrupt_stub0,
    rust_interrupt_stub1,
    rust_interrupt_stub2,
    rust_interrupt_stub3,
    rust_interrupt_stub4,
    rust_interrupt_stub5,
    rust_interrupt_stub6,
    rust_interrupt_stub7,
};

class RustLegacyHandler : public kernel::InterruptHandler
{
public:
    ulong slot_index_;

    void on_interrupt_register(u8 irq, u8 vector) override
    {
        (void)irq;
        rust_irq_slots[slot_index_].vector = vector;
    }

    kernel::InterruptHandlerFn get_handler_fn() override
    {
        return rust_stub_table[slot_index_];
    }

    void on_interrupt(kernel::Context* ctx) override
    {
        (void)ctx;
        kernel::Task* preempt = rust_irq_lock.read_lock();
        auto handler = rust_irq_slots[slot_index_].handler;
        auto uctx = rust_irq_slots[slot_index_].ctx;
        if (handler)
            rust_irq_slots[slot_index_].in_flight.inc();
        rust_irq_lock.read_unlock(preempt);
        if (handler)
        {
            handler(uctx);
            rust_irq_slots[slot_index_].in_flight.dec();
        }
    }
};

static char rust_legacy_handlers_buf[RUST_IRQ_SLOT_COUNT][sizeof(RustLegacyHandler)];
static bool rust_legacy_handlers_init[RUST_IRQ_SLOT_COUNT];

static RustLegacyHandler& get_legacy_handler(ulong i)
{
    if (!rust_legacy_handlers_init[i])
    {
        new (&rust_legacy_handlers_buf[i]) RustLegacyHandler();
        rust_legacy_handlers_init[i] = true;
    }
    return *reinterpret_cast<RustLegacyHandler*>(&rust_legacy_handlers_buf[i]);
}

extern "C" {

void dispatch_rust_interrupt(kernel::Context* ctx, int slot)
{
    (void)ctx;

    /* The legacy line, counted for the same reason as the MSI-X path below:
       the driver falls back to INTx when MSI-X is unavailable, and which of
       the two a card ended up on was not visible from the running machine. */
    kernel::InterruptStats::inc(kernel::IrqShared);

    if (slot < 0 || (ulong)slot >= RUST_IRQ_SLOT_COUNT)
    {
        hal::irq_eoi();
        return;
    }
    /* in_flight is raised inside the read-locked section so unregister (which
       nulls Handler under the write lock) can wait out a mid-call ISR before
       its caller frees ctx. */
    kernel::Task* preempt = rust_irq_lock.read_lock();
    auto handler = rust_irq_slots[slot].handler;
    auto uctx = rust_irq_slots[slot].ctx;
    if (handler)
        rust_irq_slots[slot].in_flight.inc();
    rust_irq_lock.read_unlock(preempt);
    if (handler)
    {
        handler(uctx);
        rust_irq_slots[slot].in_flight.dec();
    }
    hal::irq_eoi();
}

unsigned long kernel_interrupt_register_level(
    unsigned char irq_line,
    void (*handler)(void*), void* ctx,
    unsigned char* out_vector)
{
    if (!handler || !out_vector)
        return 0;

    ulong flags = rust_irq_lock.write_lock_irq_save();
    for (ulong i = 0; i < RUST_IRQ_SLOT_COUNT; i++)
    {
        if (!rust_irq_slots[i].used)
        {
            rust_irq_slots[i].handler = handler;
            rust_irq_slots[i].ctx = ctx;
            rust_irq_slots[i].used = true;
            rust_irq_slots[i].vector = 0;

            auto& lh = get_legacy_handler(i);
            lh.slot_index_ = i;

            u8 vector = (u8)(RUST_IRQ_VECTOR_BASE + i);
            kernel::Interrupt::register_level(lh, irq_line, vector);

            /* register_level is void and refuses silently (GSI owned by an
               edge handler, vector already claimed, shared list full). Its
               on_interrupt_register callback sets the slot's Vector; still 0
               means no IOAPIC pin was programmed -- roll the slot back
               instead of handing the driver a dead handle it would trust
               (NIC enabled, no interrupts ever delivered). */
            if (rust_irq_slots[i].vector == 0)
            {
                rust_irq_slots[i].handler = nullptr;
                rust_irq_slots[i].ctx = nullptr;
                rust_irq_slots[i].used = false;
                rust_irq_lock.write_unlock_irq_restore(flags);
                trace(0, "kernel_interrupt_register_level: irq %lu refused",
                      (ulong)irq_line);
                return 0;
            }

            *out_vector = rust_irq_slots[i].vector;
            rust_irq_lock.write_unlock_irq_restore(flags);
            return i + 1;
        }
    }
    rust_irq_lock.write_unlock_irq_restore(flags);

    trace(0, "kernel_interrupt_register_level: no free slots");
    return 0;
}

void kernel_interrupt_unregister(unsigned long handle)
{
    if (handle == 0 || handle > RUST_IRQ_SLOT_COUNT)
        return;
    ulong i = handle - 1;
    ulong flags = rust_irq_lock.write_lock_irq_save();
    rust_irq_slots[i].handler = nullptr;
    rust_irq_slots[i].ctx = nullptr;
    rust_irq_slots[i].used = false;
    rust_irq_slots[i].vector = 0;
    rust_irq_lock.write_unlock_irq_restore(flags);

    /* Wait out an ISR mid-call on another CPU: the caller may free ctx the
       moment we return. An ISR on this CPU cannot be mid-call here --
       interrupts run to completion. */
    while (rust_irq_slots[i].in_flight.get() != 0)
        cpu_relax();
}

} /* extern "C" */

/* ---- Periodic timers ---- */

static const ulong RUST_TIMER_SLOT_COUNT = 8;

struct RustTimerSlot
{
    void (*handler)(void*);
    void* ctx;
    bool active;
};

static RustTimerSlot rust_timer_slots[RUST_TIMER_SLOT_COUNT];
static kernel::RawRwSpinLock rust_timer_lock;

class RustTimerAdapter : public kernel::TimerCallback
{
public:
    ulong slot_index_;

    void on_tick(kernel::TimerCallback& callback) override
    {
        (void)callback;
        kernel::Task* preempt = rust_timer_lock.read_lock();
        auto handler = rust_timer_slots[slot_index_].handler;
        auto ctx = rust_timer_slots[slot_index_].ctx;
        rust_timer_lock.read_unlock(preempt);
        if (handler)
            handler(ctx);
    }
};

static char rust_timer_adapters_buf[RUST_TIMER_SLOT_COUNT][sizeof(RustTimerAdapter)];
static bool rust_timer_adapters_init[RUST_TIMER_SLOT_COUNT];

static RustTimerAdapter& get_timer_adapter(ulong i)
{
    if (!rust_timer_adapters_init[i])
    {
        new (&rust_timer_adapters_buf[i]) RustTimerAdapter();
        rust_timer_adapters_init[i] = true;
    }
    return *reinterpret_cast<RustTimerAdapter*>(&rust_timer_adapters_buf[i]);
}

extern "C" {

unsigned long kernel_timer_start(
    void (*handler)(void*), void* ctx,
    unsigned long long period_ns)
{
    if (!handler || period_ns == 0)
        return 0;

    ulong flags = rust_timer_lock.write_lock_irq_save();
    for (ulong i = 0; i < RUST_TIMER_SLOT_COUNT; i++)
    {
        if (!rust_timer_slots[i].active)
        {
            rust_timer_slots[i].handler = handler;
            rust_timer_slots[i].ctx = ctx;
            rust_timer_slots[i].active = true;

            auto& ta = get_timer_adapter(i);
            ta.slot_index_ = i;

            stdlib::Time period(period_ns);
            if (!kernel::TimerTable::get_instance().start_timer(
                    ta, period))
            {
                rust_timer_slots[i].active = false;
                rust_timer_slots[i].handler = nullptr;
                rust_timer_slots[i].ctx = nullptr;
                rust_timer_lock.write_unlock_irq_restore(flags);
                return 0;
            }
            rust_timer_lock.write_unlock_irq_restore(flags);
            return i + 1;
        }
    }
    rust_timer_lock.write_unlock_irq_restore(flags);
    return 0;
}

void kernel_timer_stop(unsigned long handle)
{
    if (handle == 0 || handle > RUST_TIMER_SLOT_COUNT)
        return;
    ulong i = handle - 1;

    /* Clear the slot under the lock, then release it BEFORE stop_timer. stop_timer
       spin-waits for an in-flight on_tick on another CPU to finish, and on_tick
       needs rust_timer_lock in read mode; holding the write lock across that wait
       would deadlock (AB/BA). Clearing Handler first makes a racing on_tick a
       no-op once it acquires the read lock. */
    ulong flags = rust_timer_lock.write_lock_irq_save();
    rust_timer_slots[i].handler = nullptr;
    rust_timer_slots[i].ctx = nullptr;
    rust_timer_slots[i].active = false;
    rust_timer_lock.write_unlock_irq_restore(flags);

    kernel::TimerTable::get_instance().stop_timer(get_timer_adapter(i));
}

} /* extern "C" */

/* ---- MSI-X callback slots ---- */

static const ulong RUST_MSIX_SLOT_COUNT = 32;

/* Every member initialised, as RustIrqSlot's are. */
struct RustMsixSlot
{
    void (*handler)(void*) = nullptr;
    void* ctx = nullptr;
    bool used = false;
    kernel::Atomic in_flight; /* ISRs currently executing Handler */
};

static RustMsixSlot rust_msix_slots[RUST_MSIX_SLOT_COUNT];
static kernel::RawRwSpinLock rust_msix_lock;

static kernel::InterruptHandlerFn rust_msix_stub_table[RUST_MSIX_SLOT_COUNT] = {
    rust_msix_stub0,
    rust_msix_stub1,
    rust_msix_stub2,
    rust_msix_stub3,
    rust_msix_stub4,
    rust_msix_stub5,
    rust_msix_stub6,
    rust_msix_stub7,
    rust_msix_stub8,
    rust_msix_stub9,
    rust_msix_stub10,
    rust_msix_stub11,
    rust_msix_stub12,
    rust_msix_stub13,
    rust_msix_stub14,
    rust_msix_stub15,
    rust_msix_stub16,
    rust_msix_stub17,
    rust_msix_stub18,
    rust_msix_stub19,
    rust_msix_stub20,
    rust_msix_stub21,
    rust_msix_stub22,
    rust_msix_stub23,
    rust_msix_stub24,
    rust_msix_stub25,
    rust_msix_stub26,
    rust_msix_stub27,
    rust_msix_stub28,
    rust_msix_stub29,
    rust_msix_stub30,
    rust_msix_stub31,
};

extern "C" void dispatch_rust_msix(kernel::Context* ctx, int slot);

class RustMsixSlotHandler : public kernel::InterruptHandler
{
public:
    ulong slot_index_;

    void on_interrupt_register(u8 irq, u8 vector) override
    {
        (void)irq;
        (void)vector;
    }

    kernel::InterruptHandlerFn get_handler_fn() override
    {
        return rust_msix_stub_table[slot_index_];
    }

    /* Object-based dispatch path (arm64 GIC/ITS LPIs). On x86 MSI-X goes
       through the IDT stub -> dispatch_rust_msix instead, so this override is
       unused there. */
    void on_interrupt(kernel::Context* ctx) override
    {
        dispatch_rust_msix(ctx, (int)slot_index_);
    }
};

static char rust_msix_slot_handlers_buf[RUST_MSIX_SLOT_COUNT][sizeof(RustMsixSlotHandler)];
static bool rust_msix_slot_handlers_init[RUST_MSIX_SLOT_COUNT];

static RustMsixSlotHandler& get_msix_slot_handler(ulong i)
{
    if (!rust_msix_slot_handlers_init[i])
    {
        new (&rust_msix_slot_handlers_buf[i]) RustMsixSlotHandler();
        rust_msix_slot_handlers_init[i] = true;
    }
    return *reinterpret_cast<RustMsixSlotHandler*>(&rust_msix_slot_handlers_buf[i]);
}

extern "C" {

void dispatch_rust_msix(kernel::Context* ctx, int slot)
{
    (void)ctx;

    /* Counted here because nothing counted it anywhere: IrqMsix existed only
       to name a row in `irqstat`, so that row read zero for the life of every
       boot -- including on a machine whose NIC had just delivered thirteen
       thousand packets. Asking whether the card's interrupts had stopped was
       therefore unanswerable, which is most of why its receive stall took so
       long to corner. */
    kernel::InterruptStats::inc(kernel::IrqMsix);

    if (slot < 0 || (ulong)slot >= RUST_MSIX_SLOT_COUNT)
    {
        hal::irq_eoi();
        return;
    }
    /* Same in-flight discipline as dispatch_rust_interrupt */
    kernel::Task* preempt = rust_msix_lock.read_lock();
    auto handler = rust_msix_slots[slot].handler;
    auto uctx = rust_msix_slots[slot].ctx;
    if (handler)
        rust_msix_slots[slot].in_flight.inc();
    rust_msix_lock.read_unlock(preempt);
    if (handler)
    {
        handler(uctx);
        rust_msix_slots[slot].in_flight.dec();
    }
    hal::irq_eoi();
}

unsigned long kernel_msix_register_handler(
    unsigned long msix_handle, unsigned short msix_index,
    void (*handler)(void*), void* ctx,
    unsigned char* out_vector)
{
    if (!handler || !out_vector || msix_handle == 0)
        return 0;

    ulong flags = rust_msix_lock.write_lock_irq_save();
    for (ulong i = 0; i < RUST_MSIX_SLOT_COUNT; i++)
    {
        if (!rust_msix_slots[i].used)
        {
            rust_msix_slots[i].handler = handler;
            rust_msix_slots[i].ctx = ctx;
            rust_msix_slots[i].used = true;

            auto& mh = get_msix_slot_handler(i);
            mh.slot_index_ = i;

            auto* t = reinterpret_cast<kernel::MsixTable*>(msix_handle);
            u8 vector = t->enable_vector(msix_index, mh);
            if (vector == 0)
            {
                rust_msix_slots[i].handler = nullptr;
                rust_msix_slots[i].ctx = nullptr;
                rust_msix_slots[i].used = false;
                rust_msix_lock.write_unlock_irq_restore(flags);
                return 0;
            }
            *out_vector = vector;
            rust_msix_lock.write_unlock_irq_restore(flags);
            return i + 1;
        }
    }
    rust_msix_lock.write_unlock_irq_restore(flags);
    trace(0, "kernel_msix_register_handler: no free slots");
    return 0;
}

void kernel_msix_unregister_handler(unsigned long handle)
{
    if (handle == 0 || handle > RUST_MSIX_SLOT_COUNT)
        return;
    ulong i = handle - 1;
    ulong flags = rust_msix_lock.write_lock_irq_save();
    rust_msix_slots[i].handler = nullptr;
    rust_msix_slots[i].ctx = nullptr;
    rust_msix_slots[i].used = false;
    rust_msix_lock.write_unlock_irq_restore(flags);

    /* Wait out an ISR mid-call on another CPU (see kernel_interrupt_unregister) */
    while (rust_msix_slots[i].in_flight.get() != 0)
        cpu_relax();
}

} /* extern "C" */

/* ---- The CPU's random instruction ---- */

/* hal/random.h for the pool, which is Rust (src/crates/random): which
   instruction the CPU has, and one draw from it. Values in and values out,
   and safe at any time: a draw reads only the flags probe_hw_random sets, so on
   a CPU with no instruction, or before the probe, it fails rather than
   faulting. */
struct HwRandomDraw
{
    unsigned long long value;
    unsigned long long ok;      /* 1 when Value is a draw */
};

extern "C" {

unsigned int kernel_hw_random_kind()
{
    return (unsigned int)hal::get_hw_random_kind();
}

/* The CPU's whitened output: RDRAND, RNDR */
HwRandomDraw kernel_hw_random()
{
    HwRandomDraw draw = { 0, 0 };
    u64 value = 0;
    if (hal::hw_random(value))
    {
        draw.value = value;
        draw.ok = 1;
    }
    return draw;
}

/* Its raw conditioned entropy where it has an instruction for that (RDSEED,
   RNDRRS), the whitened output where it has not */
HwRandomDraw kernel_hw_random_seed()
{
    HwRandomDraw draw = { 0, 0 };
    u64 value = 0;
    if (hal::hw_random_seed(value))
    {
        draw.value = value;
        draw.ok = 1;
    }
    return draw;
}

} /* extern "C" */

/* ---- Net device bridge ---- */

/* The net devices, the frames and the queues between them are Rust
   (src/crates/net/src/device.rs and frame.rs), and so is the C ABI a driver
   registers through: the kernel_netdev_* and kernel_netframe_* names are
   defined there now. Nothing of them is left here. */

extern "C" {

unsigned long long kernel_hpet_read_ns()
{
    auto& hpet = kernel::Hpet::get_instance();
    if (!hpet.is_available()) return 0;
    return hpet.get_time().get_value();
}

bool kernel_hpet_is_available()
{
    return kernel::Hpet::get_instance().is_available();
}

bool kernel_acpi_has_firmware_watchdog()
{
    return kernel::Acpi::get_instance().has_firmware_watchdog();
}

/* Shell commands from Rust, a loadable module's in particular (kcore::cmd).
   Rust's handler takes the arguments as *const u8, which is the same thing
   to the ABI as the const char* DynamicHandler is declared with. */
unsigned long kernel_cmd_register(const unsigned char* name, unsigned long name_len,
    const unsigned char* help, unsigned long help_len,
    kernel::Cmd::DynamicHandler handler, void* ctx)
{
    return kernel::Cmd::get_instance().register_dynamic(reinterpret_cast<const char*>(name),
        name_len, reinterpret_cast<const char*>(help), help_len, handler, ctx);
}

void kernel_cmd_unregister(unsigned long handle)
{
    kernel::Cmd::get_instance().unregister_dynamic(handle);
}

/* out is the stdlib::Printer a command handler was given */
void kernel_printer_write(void* out, const unsigned char* buf, unsigned long len)
{
    if (out == nullptr || buf == nullptr)
        return;

    auto* printer = static_cast<stdlib::Printer*>(out);
    char chunk[PRINTER_CHUNK_SIZE];
    while (len != 0)
    {
        unsigned long n = (len < sizeof(chunk) - 1) ? len : sizeof(chunk) - 1;
        stdlib::mem_cpy(chunk, buf, n);
        chunk[n] = '\0';
        printer->print_string(chunk);
        buf += n;
        len -= n;
    }
}

/* kernel_cmd_dispatch and kernel_cmd_dispatch_io: a command line run as the
   console would run it, what it prints handed to sink(ctx, ...) as
   SinkPrinter passes it on, and -- given a source -- what is typed while it
   runs read from source(ctx, ...), and the terminal it is typed at from
   term(ctx, ...). Sleeps as long as the command runs: task context only,
   with no lock held. */
static void dispatch_to(const unsigned char* line, unsigned long len,
    SinkPrinter::SinkFn sink, SinkPrinter::SourceFn source, SinkPrinter::TerminalFn term, void* ctx)
{
    if (line == nullptr || sink == nullptr)
        return;

    if (len > DISPATCH_LINE_MAX)
    {
        char note[96];
        stdlib::BufferPrinter bp(note, sizeof(note));
        bp.printf("command too long: %lu characters, the most is %lu\n", len, DISPATCH_LINE_MAX);
        sink(ctx, reinterpret_cast<const unsigned char*>(note), stdlib::str_len(note));
        return;
    }

    /* Before the command runs: the one moment sure to be allowed to */
    auto* buf = static_cast<unsigned char*>(kernel::mm::alloc(SINK_BUFFER_SIZE, RUST_ALLOC_TAG));
    if (buf == nullptr)
    {
        static const char NO_MEMORY[] = "no memory for the command's output\n";
        sink(ctx, reinterpret_cast<const unsigned char*>(NO_MEMORY), sizeof(NO_MEMORY) - 1);
        return;
    }

    char cmd[DISPATCH_LINE_MAX + 1];
    stdlib::mem_cpy(cmd, line, len);
    cmd[len] = '\0';

    SinkPrinter out(sink, source, term, ctx, buf, SINK_BUFFER_SIZE);
    kernel::Cmd::dispatch(cmd, out);
    out.finish();
    kernel::mm::free(buf);
}

/* A shell command line run for Rust -- an SSH session's -- with nobody to
   type at it */
void kernel_cmd_dispatch(const unsigned char* line, unsigned long len,
    SinkPrinter::SinkFn sink, void* ctx)
{
    dispatch_to(line, len, sink, nullptr, nullptr, ctx);
}

/* The same with someone to type: an SSH session's command can read what the
   client types while it runs (kernel_printer_read), and ask what terminal
   it is typed at (kernel_printer_terminal) */
void kernel_cmd_dispatch_io(const unsigned char* line, unsigned long len,
    SinkPrinter::SinkFn sink, SinkPrinter::SourceFn source, SinkPrinter::TerminalFn term, void* ctx)
{
    dispatch_to(line, len, sink, source, term, ctx);
}

/* What is typed at a command, for one that asks: see Printer::read_input */
long kernel_printer_read(void* out, unsigned char* buf, unsigned long len,
    unsigned long long timeout_ns)
{
    if (out == nullptr || buf == nullptr || len == 0)
        return -1;

    return static_cast<stdlib::Printer*>(out)->read_input(buf, len, timeout_ns);
}

/* The terminal a command is typed at, for one that draws on it: see
   Printer::terminal. info is untouched when there is none */
bool kernel_printer_terminal(void* out, stdlib::TerminalInfo* info)
{
    if (out == nullptr || info == nullptr)
        return false;

    return static_cast<stdlib::Printer*>(out)->terminal(*info);
}

/* What procfs puts in its files: the kernel's version, the command line it
   was booted with, and the interrupt counters. Each answers how many bytes
   it wrote, which is never more than the buffer holds. */
unsigned long kernel_version_string(char* buf, unsigned long len)
{
    if (buf == nullptr || len == 0)
        return 0;

    int n = stdlib::sn_printf(buf, len, "nos %s (%s)", KERNEL_VERSION, KERNEL_GIT_REV);
    return (n > 0) ? (unsigned long)n : 0;
}

unsigned long kernel_cmdline_string(char* buf, unsigned long len)
{
    if (buf == nullptr || len == 0)
        return 0;

    const char* cmdline = kernel::Parameters::get_instance().get_cmdline();
    int n = stdlib::sn_printf(buf, len, "%s", cmdline);
    return (n > 0) ? (unsigned long)n : 0;
}

/* How many interrupt sources there are, and what the index'th one is
   called and has counted. -1 past the end. */
unsigned long kernel_interrupt_source_count()
{
    return kernel::InterruptStats::COUNT;
}

long kernel_interrupt_source(unsigned long index, char* name, unsigned long name_len)
{
    if (index >= kernel::InterruptStats::COUNT)
        return -1;

    kernel::InterruptSource src = static_cast<kernel::InterruptSource>(index);
    if (name != nullptr && name_len != 0)
        stdlib::sn_printf(name, name_len, "%s", kernel::InterruptStats::get_name(src));
    return kernel::InterruptStats::get(src);
}

/* ---- What the management bus says of the system ---- */

/* src/crates/kernel's `system` provider: what the C++'s own tables hold,
   copied into the caller's buffers under the lock each table keeps and with
   nothing allocated (TaskTable::snapshot, ModuleTable::snapshot,
   Dmesg::read_since), and the few things it changes, each checked here
   first. The structs are the C ABI's; ffi::system has their Rust. */

static_assert(sizeof(kernel::TaskTable::TaskSnapshot) == 64, "ffi::system::TaskInfo");
static_assert(sizeof(kernel::ModuleTable::ModuleSnapshot) == 56, "ffi::system::ModuleInfo");

/* Every task, at most max of them, into out: how many were copied */
unsigned long kernel_task_snapshot(kernel::TaskTable::TaskSnapshot* out, unsigned long max)
{
    if (out == nullptr)
        return 0;

    return kernel::TaskTable::get_instance().snapshot(out, max);
}

/* How long the CPU numbered `cpu` has spent in its idle task, the stretch it
   is in now included: 0 for one that is not running */
unsigned long long kernel_cpu_idle_ns(unsigned int cpu)
{
    auto& cpus = kernel::CpuTable::get_instance();
    if (cpu >= kernel::MAX_CPUS || (cpus.get_running_cpus() & (1UL << cpu)) == 0)
        return 0;

    kernel::Task* idle = cpus.get_cpu(cpu).get_idle_task();
    return (idle != nullptr) ? idle->get_runtime_ns() : 0;
}

/* The page allocator's pages and the huge pool's blocks: ffi::system::MemInfo */
struct RustMemInfo
{
    unsigned long long page_bytes;
    unsigned long long total_pages;
    unsigned long long free_pages;
    unsigned long long huge_block_bytes;
    unsigned long long huge_blocks;
    unsigned long long huge_free;
};

void kernel_mem_info(RustMemInfo* out)
{
    if (out == nullptr)
        return;

    auto& pt = kernel::mm::PageTable::get_instance();
    out->page_bytes = consts::PAGE_SIZE;
    out->total_pages = pt.get_total_pages_count();
    out->free_pages = pt.get_free_pages_count();
    out->huge_block_bytes = kernel::mm::PageTable::HUGE_FRAME_SIZE;
    out->huge_blocks = pt.get_huge_pool_blocks();
    out->huge_free = pt.get_huge_pool_free();
}

/* The length of what buf holds up to its NUL, never past len */
static unsigned long bounded_len(const char* buf, unsigned long len)
{
    unsigned long n = 0;
    while (n < len && buf[n] != '\0')
        n++;
    return n;
}

/* What the CPU is called (hal::cpu_model), into buf: the bytes written,
   without the NUL */
unsigned long kernel_cpu_model(char* buf, unsigned long len)
{
    if (buf == nullptr || len == 0)
        return 0;

    hal::cpu_model(buf, len);
    return bounded_len(buf, len);
}

/* The version (0) or the git revision it was built from (1), into buf: the
   bytes written, without the NUL; 0 for anything else */
unsigned long kernel_version_field(unsigned int which, char* buf, unsigned long len)
{
    static const unsigned int VERSION = 0;
    static const unsigned int REVISION = 1;
    if (buf == nullptr || len == 0 || (which != VERSION && which != REVISION))
        return 0;

    stdlib::sn_printf(buf, len, "%s", (which == VERSION) ? KERNEL_VERSION : KERNEL_GIT_REV);
    return bounded_len(buf, len);
}

unsigned int kernel_trace_level()
{
    const int level = kernel::Tracer::get_instance().get_level();
    return (level > 0) ? (unsigned int)level : 0;
}

/* The trace level set, as `loglevel N` sets it: 1, or 0 -- and nothing
   changed -- for a level past MAX_TRACE_LEVEL */
int kernel_trace_set_level(unsigned int level)
{
    if (level > (unsigned int)kernel::MAX_TRACE_LEVEL)
        return 0;

    kernel::Tracer::get_instance().set_level((int)level);
    return 1;
}

/* A task's nice, as `sched nice` sets it: 0 done, 1 no such task, 2 a nice
   out of range */
int kernel_task_set_nice(unsigned long pid, long nice)
{
    if (nice < kernel::SchedEntity::NICE_MIN || nice > kernel::SchedEntity::NICE_MAX)
        return 2;

    kernel::ObjectPtr<kernel::Task> task(kernel::TaskTable::get_instance().lookup(pid));
    if (!task)
        return 1;

    kernel::set_task_nice(task.get(), nice);
    return 0;
}

/* Every loaded module, at most max of them, into out: how many were copied */
unsigned long kernel_module_list(kernel::ModuleTable::ModuleSnapshot* out, unsigned long max)
{
    if (out == nullptr)
        return 0;

    return kernel::ModuleTable::get_instance().snapshot(out, max);
}

/* What kernel_module_command collects of what insmod or rmmod says: their
   job's output and the line after it */
static const unsigned long MODULE_COMMAND_OUTPUT = 4 * consts::KB;

/* Whether the len bytes at s are printable ASCII, a blank allowed or not:
   no NUL to cut a C string short, and nothing that would end a line */
static bool printable(const unsigned char* s, unsigned long len, bool blanks)
{
    for (unsigned long i = 0; i < len; i++)
    {
        if (s[i] < ' ' || s[i] > '~' || (!blanks && s[i] == ' '))
            return false;
    }
    return true;
}

/* insmod (unload 0) of the module at the path arg, its init handed params,
   or rmmod (1) of the module named arg -- as the shell runs them, in a task
   of their own waited for SHELL_WAIT_MS -- what they said handed to
   sink(ctx, ...) once they have said it. Answers how it ended: 0 done, 1
   failed, 2 still going, 3 never started -- a path, name or parameters too
   long, empty where they may not be, or with anything in them but printable
   ASCII (a blank in params alone), or no memory. Task context only: it
   sleeps. */
int kernel_module_command(int unload, const unsigned char* arg, unsigned long arg_len,
    const unsigned char* params, unsigned long params_len, SinkPrinter::SinkFn sink, void* ctx)
{
    static const int DONE = 0;
    static const int FAILED = 1;
    static const int PENDING = 2;
    static const int NOT_STARTED = 3;
    using kernel::ModuleTable;

    if (arg == nullptr || sink == nullptr || arg_len == 0 || arg_len > ModuleTable::PATH_LEN_MAX ||
        !printable(arg, arg_len, false))
        return NOT_STARTED;
    if (params_len != 0 && (params == nullptr || unload != 0 || params_len > ModuleTable::PARAMS_MAX ||
        !printable(params, params_len, true)))
        return NOT_STARTED;

    /* insmod's line: the path, a blank, the parameters */
    char line[ModuleTable::PATH_LEN_MAX + 1 + ModuleTable::PARAMS_MAX + 1];
    stdlib::mem_cpy(line, arg, arg_len);
    unsigned long at = arg_len;
    if (params_len != 0)
    {
        line[at++] = ' ';
        stdlib::mem_cpy(line + at, params, params_len);
        at += params_len;
    }
    line[at] = '\0';

    auto* text = static_cast<char*>(kernel::mm::alloc(MODULE_COMMAND_OUTPUT, RUST_ALLOC_TAG));
    if (text == nullptr)
        return NOT_STARTED;
    text[0] = '\0';

    stdlib::BufferPrinter out(text, MODULE_COMMAND_OUTPUT);
    auto& modules = ModuleTable::get_instance();
    const auto outcome = (unload != 0) ? modules.start_unload(line, out) : modules.start_load(line, out);

    const unsigned long len = bounded_len(text, MODULE_COMMAND_OUTPUT);
    if (len != 0)
        sink(ctx, reinterpret_cast<const unsigned char*>(text), len);
    kernel::mm::free(text);

    switch (outcome)
    {
    case ModuleTable::JobOutcome::Done:
        return DONE;
    case ModuleTable::JobOutcome::Failed:
        return FAILED;
    case ModuleTable::JobOutcome::Pending:
        return PENDING;
    case ModuleTable::JobOutcome::NotStarted:
        return NOT_STARTED;
    }
    return NOT_STARTED;
}

/* The log's messages numbered after `since`, as Dmesg::read_since copies
   them into buf: the bytes written, and the number of the last copied --
   `since` for none -- into last */
unsigned long kernel_dmesg_read(unsigned long long since, unsigned char* buf, unsigned long len,
    unsigned long long* last)
{
    if (buf == nullptr || last == nullptr)
        return 0;

    u64 got = since;
    const unsigned long n = kernel::Dmesg::get_instance().read_since(since, buf, len, got);
    *last = got;
    return n;
}

/* The number the next message logged will have */
unsigned long long kernel_dmesg_next_seq()
{
    return kernel::Dmesg::get_instance().get_next_seq();
}

/* Messages the log no longer has: recycled to make room, or dropped */
unsigned long long kernel_dmesg_lost()
{
    return kernel::Dmesg::get_instance().get_lost();
}

/* A power-off (0) or a reboot (1) asked for, as the shell's commands ask
   for one: the halt runs on the shell's own watch, after this returns.
   Anything else is nothing. */
void kernel_power_request(unsigned int what)
{
    static const unsigned int POWER_OFF = 0;
    static const unsigned int REBOOT = 1;
    if (what == POWER_OFF)
        kernel::Cmd::get_instance().request_shutdown();
    else if (what == REBOOT)
        kernel::Cmd::get_instance().request_reboot();
}

/* What netconsole= asked for: the collector's address into ip, its port into
   port, and the nctail= cap in KiB into tail_kb. 1 when it was asked for at
   all, 0 when it was not. */
int kernel_netconsole_params(unsigned int* ip, unsigned short* port,
    unsigned long* tail_kb)
{
    auto& params = kernel::Parameters::get_instance();
    if (!params.is_netconsole_enabled())
        return 0;

    if (ip != nullptr)
        *ip = params.get_netconsole_ip();
    if (port != nullptr)
        *port = params.get_netconsole_port();
    if (tail_kb != nullptr)
        *tail_kb = params.get_netconsole_tail_kb();
    return 1;
}

/* Every message the log already holds, oldest first, handed to `line`. What
   primes the netconsole ring with the boot that happened before it was set
   up, so a collector still sees the whole of it. */
void kernel_dmesg_replay(void (*line)(void* ctx, const unsigned char* s,
    unsigned long len), void* ctx)
{
    if (line == nullptr)
        return;

    kernel::DmesgMsg* msg = kernel::Dmesg::get_instance().next(nullptr);
    for (ulong replayed = 0; msg != nullptr; replayed++)
    {
        ulong len = stdlib::str_len(msg->str);
        if (len != 0)
            line(ctx, reinterpret_cast<const unsigned char*>(msg->str), len);

        /* Never outlast the log itself: a walk past its capacity is chasing
           a tail someone else is still extending. */
        if (replayed + 1 == kernel::Dmesg::MAX_MSGS)
        {
            kernel::Dmesg::get_instance().release(msg);
            break;
        }

        msg = kernel::Dmesg::get_instance().next(msg);
    }
}

/* Interrupts and preemption off, and back on: what code holding a lock of
   its own uses when it cannot allocate one -- netconsole arms its capture
   ring before the page allocator exists. The order matters: reading the CPU
   first and disabling after leaves a window to be preempted onto another. */
unsigned long kernel_irq_save()
{
    return kernel::preempt_irq_save();
}

void kernel_irq_restore(unsigned long flags)
{
    kernel::preempt_irq_restore(flags);
}

/* Preemption off, and back on: what a spin lock of the kernel's own kind
   holds while it is taken. Interrupts stay on -- a lock an interrupt handler
   also takes needs kernel_irq_save instead. The holder cannot be switched
   away, which is what keeps every other taker from spinning out a whole time
   slice. */
void kernel_preempt_disable()
{
    kernel::preempt_disable();
}

void kernel_preempt_enable()
{
    kernel::preempt_enable();
}

/* The same, for code that runs on stacks that are not a task's -- the
   tracer's, an interrupt handler's, an AP on its way up. Answers with the
   task whose count went up, or 0 when there was none to raise, and that goes
   back to kernel_preempt_enable_task: the count belongs to the task that
   took it. */
unsigned long kernel_preempt_disable_task()
{
    return (unsigned long)kernel::preempt_disable_task();
}

void kernel_preempt_enable_task(unsigned long task)
{
    kernel::preempt_enable_task(reinterpret_cast<kernel::Task*>(task));
}

/* Whether preemption is on at all yet: before it is, no task will ever be
   scheduled and a caller with work to hand off has to do it itself. */
int kernel_preempt_is_on()
{
    return kernel::preempt_is_on() ? 1 : 0;
}

/* May the caller wait -- for a completion, or to be scheduled away? Not with
   interrupts off, not off a task stack, and not with preemption disabled,
   which every spinlock holds for as long as it is held. */
int kernel_preempt_can_block()
{
    return kernel::preempt_can_block() ? 1 : 0;
}

/* Whether interrupts are on for this CPU. What tells a caller it may
   release something whose free waits for every other CPU to answer -- with
   interrupts off it could not answer one itself. */
int kernel_interrupts_enabled()
{
    return hal::is_interrupt_enabled() ? 1 : 0;
}

/* Whether a panic has started: what tells code to write without taking a
   lock, because another CPU may hold it and is on its way to a halt. */
int kernel_panic_active()
{
    return kernel::Panicker::get_instance().is_active() ? 1 : 0;
}

/* One decoded byte of a keystroke from a driver in Rust -- the USB
   keyboard -- to every observer: the shell, and whatever else is listening.
   What it is is KeyboardObserver's (kernel/input.h). */
void kernel_input_key(char c, unsigned char code)
{
    kernel::KeyboardInput::get_instance().emit(c, code);
}

/* dhcp=off: the shell's `dhcp` says so rather than running one. */
int kernel_param_dhcp_off()
{
    return kernel::Parameters::get_instance().is_dhcp_off() ? 1 : 0;
}

/* dns=on: a lease's DNS server is worth starting a resolver on. */
int kernel_param_dns_on()
{
    return kernel::Parameters::get_instance().is_dns_enabled() ? 1 : 0;
}

/* rxpoll=on: the tick's receive poll, which the network layer's
   DeviceTable::poll_rx asks about on every call. */
int kernel_param_rxpoll_on()
{
    return kernel::Parameters::get_instance().is_rx_poll_enabled() ? 1 : 0;
}

/* Whether the kernel log is to be written to a prepared disk area: 1 for
   disklog=on, 0 without it, and -1 before the command line has been read at
   all -- when nobody knows yet, and every line is kept because the first
   lines are part of the boot the area is meant to hold. */
int kernel_disklog_wanted()
{
    auto& params = kernel::Parameters::get_instance();
    if (!params.is_parsed())
        return -1;
    return params.is_disk_log_on() ? 1 : 0;
}

/* What the root filesystem is to be, off the kernel command line: the mode
   (Parameters::RootMode), the device name or label into `value`, and the
   parsed UUID into `uuid`. See rust_mount_root_fs. */
int kernel_root_spec(char* value, unsigned long value_len, unsigned char* uuid,
    unsigned long uuid_len)
{
    const kernel::Parameters::RootSpec& spec = kernel::Parameters::get_instance().get_root();

    if (value != nullptr && value_len != 0)
        stdlib::sn_printf(value, value_len, "%s", spec.value);
    if (uuid != nullptr && uuid_len >= sizeof(spec.uuid))
        stdlib::mem_cpy(uuid, spec.uuid, sizeof(spec.uuid));

    return (int)spec.mode;
}

/* ro: the root is to be mounted read-only */
int kernel_root_read_only()
{
    return kernel::Parameters::get_instance().is_root_read_only() ? 1 : 0;
}

/* fstest=on: run the filesystem self-test on / once it is mounted */
int kernel_root_fstest()
{
    return kernel::Parameters::get_instance().is_fs_test() ? 1 : 0;
}

/* Net devices from Rust, both sides of them -- the driver's and the
   consumer's -- are defined in src/crates/net/src/device.rs, alongside the
   devices themselves. Nothing of them is left here. */

} /* extern "C" */
