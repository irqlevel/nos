#include "dmesg.h"
#include "panic.h"

namespace kernel
{

Dmesg::Dmesg()
    : active_(false)
    , lost_(0)
{
}

Dmesg::~Dmesg()
{
    reset();
}

bool Dmesg::setup()
{
    if (active_)
        return false;

    lost_ = 0;
    active_ = msg_buf_.setup((ulong)&buf_[0], (ulong)&buf_[0] + sizeof(buf_), sizeof(DmesgMsg));
    return active_;
}

void Dmesg::reset()
{
    active_ = false;
    stdlib::AutoLock lock(lock_);
    while (!msg_list_.is_empty())
    {
        DmesgMsg *msg = CONTAINING_RECORD(msg_list_.remove_head(), DmesgMsg, list_entry);
        msg_buf_.free(msg);
    }
}

void Dmesg::v_printf(const char *fmt, va_list args)
{
    if (!active_)
        return;

    ulong retries = 0;

restart:
    DmesgMsg* msg = (DmesgMsg*)msg_buf_.alloc();
    if (msg == nullptr)
    {
        stdlib::AutoLock lock(lock_);
        if (msg_list_.is_empty())
        {
            lost_++;
            return;
        }

        for (auto list_entry = msg_list_.flink; list_entry != &msg_list_; list_entry = list_entry->flink)
        {
            auto cand_msg = CONTAINING_RECORD(list_entry, DmesgMsg, list_entry);
            if (cand_msg->usage.get() == 0)
            {
                msg = cand_msg;
                msg->list_entry.remove_init();
                break;
            }
        }

        if (msg == nullptr)
        {
            /* Nothing free and nothing recyclable: every message in the list
               is pinned by a walk. Retrying bets on a walk stepping on within
               the next few instructions -- past that this is a spin in the
               trace path, and a CPU stuck printing is worse than a lost line. */
            if (++retries < MAX_RECYCLE_RETRIES)
                goto restart;

            lost_++;
            return;
        }

        lost_++;
    }
    else
    {
        msg->init();
    }

	int size = stdlib::vsn_printf(msg->str, sizeof(msg->str), fmt, args);
    if (size < 0)
    {
        msg_buf_.free(msg);
		return;
    }

    stdlib::AutoLock lock(lock_);
    BUG_ON(msg->usage.get() != 0);
    msg_list_.insert_tail(&msg->list_entry);
}

void Dmesg::printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    v_printf(fmt, args);
    va_end(args);
}

void Dmesg::print_string(const char *s)
{
    printf("%s", s);
}

DmesgMsg* Dmesg::next(DmesgMsg* current)
{
    BUG_ON(current != nullptr && current->usage.get() == 0);

    stdlib::AutoLock lock(lock_);

    stdlib::ListEntry* next_list_entry;
    if (current == nullptr)
    {
        next_list_entry = msg_list_.flink;
    }
    else
    {
        next_list_entry = current->list_entry.flink;
        BUG_ON(current->usage.get() <= 0);
        current->usage.dec();
    }

    if (next_list_entry == &msg_list_)
    {
        return nullptr;
    }

    DmesgMsg* next = CONTAINING_RECORD(next_list_entry, DmesgMsg, list_entry);
    next->usage.inc();
    return next;
}

DmesgMsg* Dmesg::prev(DmesgMsg* current)
{
    BUG_ON(current != nullptr && current->usage.get() == 0);

    stdlib::AutoLock lock(lock_);

    stdlib::ListEntry* prev_list_entry;
    if (current == nullptr)
    {
        prev_list_entry = msg_list_.blink;
    }
    else
    {
        prev_list_entry = current->list_entry.blink;
        BUG_ON(current->usage.get() <= 0);
        current->usage.dec();
    }

    if (prev_list_entry == &msg_list_)
    {
        return nullptr;
    }

    DmesgMsg* prev = CONTAINING_RECORD(prev_list_entry, DmesgMsg, list_entry);
    prev->usage.inc();
    return prev;
}

void Dmesg::release(DmesgMsg* msg)
{
    if (msg == nullptr)
        return;

    stdlib::AutoLock lock(lock_);
    BUG_ON(msg->usage.get() <= 0);
    msg->usage.dec();
}

ulong Dmesg::get_lost()
{
    stdlib::AutoLock lock(lock_);
    return lost_;
}

DmesgMsg* Dmesg::tail_start(ulong last_lines)
{
    DmesgMsg* msg = prev(nullptr);

    for (ulong i = 1; msg != nullptr && i < last_lines; i++)
    {
        DmesgMsg* earlier = prev(msg);
        if (earlier == nullptr)
        {
            /* The log is shorter than the walk asked for. Prev() released the
               pin on its way off the head, so start over from the head rather
               than try to resurrect it. */
            return next(nullptr);
        }

        msg = earlier;
    }

    return msg;
}

void Dmesg::dump(stdlib::Printer& printer, ulong last_lines, const char* filter)
{
    if (last_lines > MAX_MSGS)
        last_lines = MAX_MSGS;

    /* A walk longer than the list can be is following a tail another CPU keeps
       extending, so it would never return. Bound both flavours. */
    const ulong limit = (last_lines != 0) ? last_lines : MAX_MSGS;

    ulong lost = get_lost();
    if (lost != 0)
        printer.printf("dmesg: %lu earlier lines lost\n", lost);

    DmesgMsg* msg = (last_lines != 0) ? tail_start(last_lines) : next(nullptr);

    for (ulong visited = 0; msg != nullptr; visited++)
    {
        if (filter == nullptr || stdlib::str_str(msg->str, filter) != nullptr)
            printer.print_string(msg->str);

        if (visited + 1 == limit)
        {
            release(msg);
            break;
        }

        msg = next(msg);
    }
}

}