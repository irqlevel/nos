#pragma once

#include <include/types.h>
#include <include/const.h>
#include <kernel/event.h>

namespace kernel
{

class Task;

namespace mm
{

/* The zeroing of free pages ahead of their allocation (zerofree=): tasks of
   the lowest priority that, while nothing else wants the CPU they are on,
   take free pages off the page table's other list, zero them and put them
   on its zeroed one, and zero the huge pool's free blocks -- the page
   table's zero_free_page and zero_free_huge_page, a page a call. So that
   what a guest is given was zeroed before it asked for it, and what the
   last guest left in its memory is gone from RAM within seconds of its
   end, rather than whenever the page is next handed out.

   WORKERS tasks, and no more than there are CPUs: one starting with the
   huge pool, one with the 4 KiB pages, each going on to the other's work
   when its own has none. Free to move, so that a task displaced from a
   busy CPU is weighed for an idle one (TaskQueue::schedule), and nice 19:
   a task zeroes only while its CPU runs nothing else -- the runnable
   count, looked at between pages -- in batches of at most BATCH_PAGES,
   after each of which it looks again; one whose CPU is busy yields, then
   waits BUSY_WAIT_NS; one with nothing left to zero waits IDLE_WAIT_NS,
   its CPU free to sleep deep. A batch is a record of the "mm" group
   (tp::MmZero). zerofree=off starts none, and every page is zeroed as it
   is handed out, as it always was. */
class PageZeroer final
{
public:
    static PageZeroer& get_instance()
    {
        static PageZeroer instance;
        return instance;
    }

    /* From the boot, once, in task context: the tasks started, unless
       zerofree=off. False, and none left running, when one could not be. */
    bool start();

    /* From the halt: every task told to stop, then each waited for. A task
       looks between two pages, so the wait is a page's, or the end of a
       wait the stop cuts short. */
    void stop();

    static const ulong WORKERS = 2;
    static const ulong BATCH_PAGES = 256;
    static const ulong IDLE_WAIT_NS = 1 * consts::NANO_SECS_IN_SEC;
    static const ulong BUSY_WAIT_NS = 10 * consts::NANO_SECS_IN_MS;

private:
    PageZeroer();
    ~PageZeroer();
    PageZeroer(const PageZeroer& other) = delete;
    PageZeroer& operator=(const PageZeroer& other) = delete;

    struct Worker
    {
        /* What cuts a wait short: stop() */
        Event wake;
        Task* task;
        /* The huge pool's free blocks before the 4 KiB pages */
        bool huge_first;
    };

    static void task_func(void* ctx);
    static void run(Worker& worker);
    static bool alone_on_cpu();

    static const ulong TAG = 'Zero';

    Worker workers_[WORKERS];
};

}
}
