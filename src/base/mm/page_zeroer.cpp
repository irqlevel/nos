#include "page_zeroer.h"
#include "page_table.h"
#include "new.h"

#include <kernel/task.h>
#include <kernel/sched.h>
#include <kernel/eevdf.h>
#include <kernel/cpu.h>
#include <kernel/time.h>
#include <kernel/klog.h>
#include <kernel/parameters.h>
#include <kernel/tracepoint.h>
#include <lib/stdlib.h>

namespace kernel
{
namespace mm
{

PageZeroer::PageZeroer()
{
    for (ulong i = 0; i < WORKERS; i++)
    {
        workers_[i].task = nullptr;
        workers_[i].huge_first = false;
    }
}

PageZeroer::~PageZeroer()
{
}

bool PageZeroer::start()
{
    if (Parameters::get_instance().is_zero_free_off())
    {
        klog(0, "mm: free pages are not zeroed ahead (zerofree=off): each is zeroed as it is handed out");
        return true;
    }

    /* No more tasks than CPUs: two on one CPU would each find the other
       runnable there, and neither would zero. One alone does both kinds. */
    const ulong cpus = (ulong)__builtin_popcountl(CpuTable::get_instance().get_running_cpus());
    const ulong workers = stdlib::min(WORKERS, stdlib::max(cpus, 1UL));
    for (ulong i = 0; i < workers; i++)
    {
        Worker& worker = workers_[i];
        BUG_ON(worker.task != nullptr);
        /* One a kind first: the huge pool, which holds guests' memory alone,
           and the 4 KiB pages */
        worker.huge_first = (i == 0);

        char name[TASK_NAME_LEN];
        stdlib::sn_printf(name, sizeof(name), "zerofree/%lu", i);
        Task* task = mm::talloc<Task, TAG>(name);
        if (task == nullptr)
        {
            stop();
            return false;
        }
        /* The CPU's last claimant: whatever else is runnable there has it */
        set_task_nice(task, SchedEntity::NICE_MAX);
        if (!task->start(&PageZeroer::task_func, &worker))
        {
            task->put();
            stop();
            return false;
        }
        worker.task = task;
    }

    klog(0, "mm: free pages zeroed ahead by %lu tasks of nice %ld%s", workers, SchedEntity::NICE_MAX,
        Parameters::get_instance().is_zero_free_verify() ? ", each read as it is handed out (zerofree=verify)" : "");
    return true;
}

void PageZeroer::stop()
{
    /* Every task told first, so that they stop together; then each waited
       for. The signal ends a wait at once, and a batch looks between pages. */
    for (ulong i = 0; i < WORKERS; i++)
    {
        Worker& worker = workers_[i];
        if (worker.task != nullptr)
        {
            worker.task->set_stopping();
            worker.wake.signal();
        }
    }
    for (ulong i = 0; i < WORKERS; i++)
    {
        Worker& worker = workers_[i];
        if (worker.task != nullptr)
        {
            worker.task->wait();
            worker.task->put();
            worker.task = nullptr;
        }
    }
}

void PageZeroer::task_func(void* ctx)
{
    run(*static_cast<Worker*>(ctx));
}

bool PageZeroer::alone_on_cpu()
{
    /* What could run on this CPU: this task included, the idle task not
       (TaskQueue::get_runnable_count) -- 1 is this task alone. Read with
       preemption on, it may be the count of a CPU this task has just been
       moved off: a look stale, which the next one, a page later, corrects. */
    return get_cpu().get_task_queue().get_runnable_count() <= 1;
}

void PageZeroer::run(Worker& worker)
{
    Task* self = Task::get_current_task();
    auto& pt = PageTable::get_instance();
    /* What the zeroing leaves free: the kernel's reserve, a constant once
       the free list is built */
    const ulong keep = pt.get_kernel_reserve_pages();

    while (!self->is_stopping())
    {
        if (!alone_on_cpu())
        {
            /* Displaced while it can still run, a task is weighed for a
               lighter CPU: this yield takes it to an idle one, if there is
               one. Back on a CPU still busy, it waits, and looks again. */
            yield_to_runnable();
            if (!self->is_stopping() && !alone_on_cpu())
                worker.wake.wait_for(BUSY_WAIT_NS);
            continue;
        }

        const ulong start = get_boot_time().get_value();
        ulong pages = 0;
        ulong huge_pages = 0;
        tp::MmZeroEnd end = tp::MmZeroFull;
        while (pages + huge_pages < BATCH_PAGES)
        {
            if (self->is_stopping())
            {
                end = tp::MmZeroStop;
                break;
            }
            if (!alone_on_cpu())
            {
                end = tp::MmZeroBusy;
                break;
            }
            if (worker.huge_first)
            {
                if (pt.zero_free_huge_page())
                {
                    huge_pages++;
                    continue;
                }
                if (pt.zero_free_page(keep))
                {
                    pages++;
                    continue;
                }
            }
            else
            {
                if (pt.zero_free_page(keep))
                {
                    pages++;
                    continue;
                }
                if (pt.zero_free_huge_page())
                {
                    huge_pages++;
                    continue;
                }
            }
            end = tp::MmZeroNone;
            break;
        }

        if (unlikely(tp::on(tp::Id::MmZero)) && pages + huge_pages != 0)
            tp::emit_mm_zero(CpuTable::get_instance().get_current_cpu_id(), pages, huge_pages,
                get_boot_time().get_value() - start, end);

        if (end == tp::MmZeroNone)
            worker.wake.wait_for(IDLE_WAIT_NS);
    }
}

}
}
