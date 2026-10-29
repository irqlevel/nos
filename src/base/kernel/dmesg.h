#pragma once

#include "spin_lock.h"
#include "atomic.h"

#include <lib/stdlib.h>
#include <lib/printer.h>
#include <mm/block_allocator.h>

namespace kernel
{

struct DmesgMsg final
{
    char str[256 - sizeof(stdlib::ListEntry) - sizeof(Atomic) - sizeof(u64)];
    stdlib::ListEntry list_entry;
    Atomic usage;
    /* Its number among every message logged since boot, from 1: set as it
       goes on the list, under the lock, so the list's are in order and a
       reader can ask for what came after the last it had (`since`) */
    u64 seq;

    void init()
    {
        list_entry.init();
        usage.set(0);
    }
};

static_assert(sizeof(DmesgMsg) == 256, "Invalid size");

class Dmesg final
{
public:

    static Dmesg& get_instance()
    {
        static Dmesg instance;
        return instance;
    }

    bool setup();

    PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args);
    PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...);
    void print_string(const char *s);

    /* Print the log. last_lines == 0 is the whole of it; otherwise only the
       newest last_lines messages, found by walking back from the tail. filter,
       when set, prints only the messages that contain it.

       The tail is what a remote shell wants: its reply buffer holds a few
       kilobytes and drops everything past that, so a dump from the head of a
       log that has been growing for an hour returns the first minute of boot
       and nothing else. */
    void dump(stdlib::Printer& printer, ulong last_lines = 0,
              const char* filter = nullptr);

    /* Step a walk one message towards the tail (Next) or the head (Prev).
       nullptr starts at the head resp. the tail; the returned message is
       pinned so VPrintf cannot recycle it under the walk, and the step
       releases the message it came from. */
    DmesgMsg* next(DmesgMsg* current);
    DmesgMsg* prev(DmesgMsg* current);

    /* Release the message a walk is standing on. A walk that stops before it
       runs off the end must call this: a pinned message is skipped by the
       recycler forever, so an abandoned pin permanently costs the log a slot. */
    void release(DmesgMsg* msg);

    /* Messages the log no longer has: recycled to make room, or dropped. */
    ulong get_lost();

    /* The number the next message logged will have */
    u64 get_next_seq();

    /* The messages numbered after `since`, oldest first, copied into buf as
       records -- the number (8 bytes, least significant first), the text's
       length (2 bytes, the same) and the text, with no NUL -- for as many as
       fit whole. The bytes written come back; `last` is the number of the
       last message copied, or `since` when none was. A gap between `since`
       and the first record's number is what the log lost meanwhile. */
    ulong read_since(u64 since, u8* buf, ulong len, u64& last);

    /* A record's number and length, before its text */
    static const ulong READ_HEADER = sizeof(u64) + sizeof(u16);

    void reset();

    /* The list cannot hold more messages than the buffer has slots, so a walk
       that visits more than this is not walking a log any more -- it is
       following a tail another CPU keeps extending, and it is not obliged to
       ever end. Every walk is capped here. */
    static const ulong BUF_SIZE = 128 * consts::PAGE_SIZE;
    static const ulong MAX_MSGS = BUF_SIZE / sizeof(DmesgMsg);

private:
    Dmesg();
    ~Dmesg();

    Dmesg(const Dmesg& other) = delete;
    Dmesg(Dmesg&& other) = delete;
    Dmesg& operator=(const Dmesg& other) = delete;
    Dmesg& operator=(Dmesg&& other) = delete;

    /* The message a last_lines dump starts printing at, pinned, or nullptr
       when the log is empty. */
    DmesgMsg* tail_start(ulong last_lines);

    /* How many times VPrintf retries when the buffer is full and every listed
       message is pinned by a reader. Retrying is a bet that a reader steps on
       within the next few instructions; past that it is an unbounded spin in
       the trace path, which costs a CPU to save one line of log. */
    static const ulong MAX_RECYCLE_RETRIES = 16;

    char buf_[BUF_SIZE]  __attribute__((aligned(sizeof(DmesgMsg))));

    mm::BlockAllocatorImpl msg_buf_;

    stdlib::ListEntry msg_list_;

    SpinLock lock_;

    volatile bool active_;

    /* Guarded by Lock. */
    ulong lost_;
    u64 next_seq_;
};

}