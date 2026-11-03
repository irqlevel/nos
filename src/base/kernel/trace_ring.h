#pragma once

#include <lib/stdlib.h>

namespace kernel
{

/* One CPU's record of tracepoint events (kernel/tracepoint.h): a ring of
   fixed slots that only its own CPU writes -- with interrupts off, so that
   nothing but an NMI can come between -- and that anyone may read at any
   time without stopping the writer.

   A slot is eight 64-bit words: its sequence word, the cycle counter when
   the event was recorded, the event's number and size, and up to five words
   of payload. Each slot is a seqlock of its own. The writer clears the
   sequence word, writes the rest, and only then writes the sequence word
   again -- the record's index plus one -- with a write barrier between each
   step; a reader takes the sequence word, the rest, and the sequence word
   again, with a read barrier before the second look, and keeps the copy only
   when both looks give the index it asked for. A slot being written over as
   it is read gives a different word, or 0, and is left out: never a record
   half of one event and half of another. Every word is read and written as
   an atomic, so the reader racing the writer is no data race in C++'s sense,
   only a copy it throws away.

   The ring wraps: once full, each record takes the slot of the oldest. A
   writer never waits and never fails for want of room; a reader asks for the
   indices it wants, from head() - capacity() up, and gets those not yet
   written over.

   The slots are in chunks of CHUNK_BYTES from the kernel's heap, as many as
   init() is told -- a power of two, so that a slot is found with a mask and
   a shift, no division on the write path. They are kept for the life of the
   kernel (release() is a test's, for a ring nothing else reaches): a CPU may
   be inside write() when another turns tracing off, and freeing under it
   would be a use-after-free for the sake of a few megabytes. */
class TraceRing final
{
public:
    static const ulong SLOT_BYTES = 64;
    static const ulong SLOT_WORDS = SLOT_BYTES / sizeof(u64);
    static const ulong HEADER_WORDS = 3;
    static const ulong PAYLOAD_WORDS = SLOT_WORDS - HEADER_WORDS;
    static const ulong PAYLOAD_BYTES = PAYLOAD_WORDS * sizeof(u64);
    static const ulong CHUNK_BYTES = 64 * 1024;
    static const ulong SLOTS_PER_CHUNK = CHUNK_BYTES / SLOT_BYTES;
    /* The most chunks a ring has: 64 MiB of them. */
    static const ulong MAX_CHUNKS = 1024;

    /* A record as read out of the ring. */
    struct Record
    {
        /* The how-manyth record its ring has written, from 0. */
        u64 index;
        /* hal::read_cycle_counter() when it was recorded. */
        u64 cycles;
        u16 id;
        u16 size;
        u32 cpu;
        u64 payload[PAYLOAD_WORDS];
    };

    constexpr TraceRing()
        : chunks_(nullptr)
        , chunk_mask_(0)
        , head_(0)
        , writing_(0)
        , dropped_(0)
    {
    }

    /* Trivial: a ring with static storage is the compiler's to initialise,
       and is never destroyed. */
    ~TraceRing() = default;

    /* Its slots: `chunks` of them, a power of two from 1 to MAX_CHUNKS, from
       the heap under `tag`. Task context, before anything writes the ring,
       and once: false for a count that is not one, no memory -- nothing
       kept of a part -- or a ring that has its slots already. */
    bool init(ulong chunks, ulong tag);

    /* The slots given back. Only for a ring nothing else can reach any more:
       a test's own. */
    void release();

    /* Whether init() has given it its slots: what write() and read() look
       at first, so that a ring without them is nothing to either. */
    bool ready() const;

    /* How many records it holds before it wraps. */
    ulong capacity() const;

    /* The record of event `id`, `size` bytes of `payload` (at most
       PAYLOAD_BYTES), taken at `cycles`. Its own CPU's, with interrupts off;
       a ring with no slots takes nothing. False when it took nothing: no
       slots, a size too large, or another write of this CPU's in progress
       -- an NMI's, dropped and counted. */
    bool write(u16 id, u64 cycles, const void* payload, ulong size);

    /* How many records it has written: the newest's index plus one. */
    u64 head() const;

    /* The records a write() found another in progress for. */
    u64 dropped() const;

    /* Record `index` into `out` (its cpu field 0): false when it is not in
       the ring -- not written yet, written over, being written over as it
       was read -- or when the ring has no slots. Any CPU, any time, the
       writer running. */
    bool read(u64 index, Record& out) const;

private:
    TraceRing(const TraceRing& other) = delete;
    TraceRing& operator=(const TraceRing& other) = delete;

    u64* slot(u64* const* table, u64 index) const;

    /* The chunks' table, published (release) once every chunk is in it:
       null until then. */
    u64** chunks_;
    ulong chunk_mask_;
    /* Written by the ring's CPU alone, read by anyone (atomics). */
    u64 head_;
    u8 writing_;
    u64 dropped_;
};

}
