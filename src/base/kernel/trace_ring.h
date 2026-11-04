#pragma once

#include <lib/stdlib.h>
#include <hal/barrier.h>

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
   a shift, no division on the write path. A ring is released only once
   nothing can be writing into it or reading it: a CPU may be inside write()
   as another turns tracing off, so the kernel's rings are swapped out first
   and released after every CPU has taken an interrupt since
   (CpuTable::synchronize, tracepoint.cpp's swap_rings) -- write() runs with
   interrupts off -- and a ring of a caller's own, `trace bench`'s or a
   test's, when its one writer is done with it.

   The ring itself -- the chunks' table, the head and the write in
   progress -- is a cache line of its own: what its CPU writes at every
   record shares the line with nothing any other CPU writes. */
const ulong TRACE_RING_ALIGN = 64;

class alignas(TRACE_RING_ALIGN) TraceRing final
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
        /* The cycle counter when it was recorded (read unordered), no
           earlier than the ring's record before it. */
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
        , last_cycles_(0)
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

    /* The slots given back: only for a ring nothing can write into or read
       any more (the header's comment above says how the kernel's get so). */
    void release();

    /* Whether init() has given it its slots: what write() and read() look
       at first, so that a ring without them is nothing to either. */
    bool ready() const;

    /* How many records it holds before it wraps. */
    ulong capacity() const;

    /* The record of event `id`, `size` bytes of `payload` (at most
       PAYLOAD_BYTES), taken at `cycles` -- or at the ring's newest record's
       stamp, if that is later: the stamps of a ring's records never go
       back. Its own CPU's, with interrupts off;
       a ring with no slots takes nothing. False when it took nothing: no
       slots, a size too large, or another write of this CPU's in progress
       -- an NMI's, dropped and counted. */
    bool write(u16 id, u64 cycles, const void* payload, ulong size);

    /* The same, its payload already in a slot's words -- `size` bytes of
       them (at most PAYLOAD_BYTES, the caller's to hold to), the words past
       it zero: the tracepoints' write path, in line, with no copy but the
       stores into the slot. */
    inline __attribute__((always_inline)) bool write_words(u16 id, u64 cycles, ulong size,
        const u64 (&payload)[PAYLOAD_WORDS]);

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

    /* A slot's words: the sequence word, the cycle counter, the event's
       number and size; then the payload. */
    static const ulong SEQ_WORD = 0;
    static const ulong CYCLES_WORD = 1;
    static const ulong ID_WORD = 2;
    static const ulong SIZE_SHIFT = 16;
    static const ulong ID_MASK = 0xFFFF;

    inline __attribute__((always_inline)) u64* slot(u64* const* table, u64 index) const
    {
        ulong chunk = (ulong)(index / SLOTS_PER_CHUNK) & chunk_mask_;
        ulong in_chunk = (ulong)(index % SLOTS_PER_CHUNK);

        return table[chunk] + in_chunk * SLOT_WORDS;
    }

    /* The chunks' table, published (release) once every chunk is in it:
       null until then. */
    u64** chunks_;
    ulong chunk_mask_;
    /* Written by the ring's CPU alone, read by anyone (atomics). */
    u64 head_;
    u8 writing_;
    u64 dropped_;
    /* The newest record's stamp: the writer's alone (write_words). */
    u64 last_cycles_;
};

static_assert(sizeof(TraceRing) == TRACE_RING_ALIGN && alignof(TraceRing) == TRACE_RING_ALIGN,
    "a ring is a cache line of its own");

inline __attribute__((always_inline)) bool TraceRing::write_words(u16 id, u64 cycles, ulong size,
    const u64 (&payload)[PAYLOAD_WORDS])
{
    u64** table = __atomic_load_n(&chunks_, __ATOMIC_ACQUIRE);
    if (unlikely(table == nullptr))
        return false;

    /* Interrupts are off: only an NMI can come in here on this CPU. One that
       finds a write in progress takes nothing and says so in the count. No
       read-modify-write is needed for that: an NMI between the look and the
       store below finds 0, writes its record whole and puts the 0 back, and
       this write then takes the next index -- it looks at head_ only after
       its own store. */
    if (unlikely(__atomic_load_n(&writing_, __ATOMIC_RELAXED) != 0))
    {
        __atomic_store_n(&dropped_, __atomic_load_n(&dropped_, __ATOMIC_RELAXED) + 1, __ATOMIC_RELAXED);
        return false;
    }
    __atomic_store_n(&writing_, 1, __ATOMIC_RELAXED);
    hal::compiler_barrier();

    /* Never before the ring's newest stamp: a tracepoint reads the counter
       unordered (hal::read_cycle_counter_unordered), which the CPU may read
       ahead of the instructions before it, and a ring's records are in the
       order of their stamps as they are of their indices -- what its
       readers count on (the merge, a window's start). Under writing_, so
       that no NMI's record comes between the look and the store. */
    u64 last = __atomic_load_n(&last_cycles_, __ATOMIC_RELAXED);
    if (cycles < last)
        cycles = last;
    __atomic_store_n(&last_cycles_, cycles, __ATOMIC_RELAXED);

    /* Only this CPU writes head_: its own last store, read back. */
    u64 index = __atomic_load_n(&head_, __ATOMIC_RELAXED);
    u64* s = slot(table, index);

    /* Every word of a slot is an atomic: the writer's stores and a reader's
       loads are no data race, only a copy the reader may throw away (the
       seqlock's look before and after says which). Relaxed, with the
       ordering the barriers give -- a plain mov or str on both
       architectures. The slot no record while it is rewritten: the
       sequence word first. */
    __atomic_store_n(&s[SEQ_WORD], 0, __ATOMIC_RELAXED);
    hal::smp_wmb();
    __atomic_store_n(&s[CYCLES_WORD], cycles, __ATOMIC_RELAXED);
    __atomic_store_n(&s[ID_WORD], (u64)id | ((u64)size << SIZE_SHIFT), __ATOMIC_RELAXED);
    for (ulong i = 0; i < PAYLOAD_WORDS; i++)
        __atomic_store_n(&s[HEADER_WORDS + i], payload[i], __ATOMIC_RELAXED);
    /* Everything else before the word that says it is record `index`. */
    hal::smp_wmb();
    __atomic_store_n(&s[SEQ_WORD], index + 1, __ATOMIC_RELAXED);
    __atomic_store_n(&head_, index + 1, __ATOMIC_RELEASE);

    hal::compiler_barrier();
    __atomic_store_n(&writing_, 0, __ATOMIC_RELAXED);
    return true;
}

}
