#include "trace_ring.h"

#include <hal/barrier.h>
#include <mm/new.h>

namespace kernel
{

namespace
{

/* Every word of a slot is an atomic: the writer's stores and a reader's
   loads are no data race, only a copy the reader may throw away (the
   seqlock's look before and after says which). Relaxed, with the ordering
   the barriers below give -- a plain mov or ldr/str on both architectures. */
inline u64 load_word(const u64* word)
{
    return __atomic_load_n(word, __ATOMIC_RELAXED);
}

inline void store_word(u64* word, u64 value)
{
    __atomic_store_n(word, value, __ATOMIC_RELAXED);
}

/* A slot's words: the sequence word, the cycle counter, the event's number
   and size; then the payload. */
const ulong SEQ_WORD = 0;
const ulong CYCLES_WORD = 1;
const ulong ID_WORD = 2;
const ulong SIZE_SHIFT = 16;
const ulong ID_MASK = 0xFFFF;

}

bool TraceRing::init(ulong chunks, ulong tag)
{
    if (chunks == 0 || chunks > MAX_CHUNKS || (chunks & (chunks - 1)) != 0)
        return false;
    if (__atomic_load_n(&chunks_, __ATOMIC_ACQUIRE) != nullptr)
        return false;

    auto table = static_cast<u64**>(mm::alloc(chunks * sizeof(u64*), tag));
    if (table == nullptr)
        return false;
    for (ulong i = 0; i < chunks; i++)
    {
        table[i] = static_cast<u64*>(mm::alloc(CHUNK_BYTES, tag));
        if (table[i] == nullptr)
        {
            /* Nothing kept of a part: given back in the reverse order. */
            for (ulong j = i; j > 0; j--)
                mm::free(table[j - 1]);
            mm::free(table);
            return false;
        }
        /* Every sequence word 0: a slot nothing was written to yet is no
           record of any index. */
        stdlib::mem_set(table[i], 0, CHUNK_BYTES);
    }

    chunk_mask_ = chunks - 1;
    __atomic_store_n(&head_, 0, __ATOMIC_RELAXED);
    __atomic_store_n(&dropped_, 0, __ATOMIC_RELAXED);
    /* The table last, and with release: a writer or reader that finds it
       finds its chunks zeroed and the mask set. */
    __atomic_store_n(&chunks_, table, __ATOMIC_RELEASE);
    return true;
}

void TraceRing::release()
{
    u64** table = __atomic_load_n(&chunks_, __ATOMIC_ACQUIRE);
    if (table == nullptr)
        return;

    __atomic_store_n(&chunks_, nullptr, __ATOMIC_RELEASE);
    for (ulong i = chunk_mask_ + 1; i > 0; i--)
        mm::free(table[i - 1]);
    mm::free(table);
    chunk_mask_ = 0;
}

bool TraceRing::ready() const
{
    return __atomic_load_n(&chunks_, __ATOMIC_ACQUIRE) != nullptr;
}

ulong TraceRing::capacity() const
{
    return ready() ? (chunk_mask_ + 1) * SLOTS_PER_CHUNK : 0;
}

u64* TraceRing::slot(u64* const* table, u64 index) const
{
    ulong chunk = (ulong)(index / SLOTS_PER_CHUNK) & chunk_mask_;
    ulong in_chunk = (ulong)(index % SLOTS_PER_CHUNK);

    return table[chunk] + in_chunk * SLOT_WORDS;
}

bool TraceRing::write(u16 id, u64 cycles, const void* payload, ulong size)
{
    u64** table = __atomic_load_n(&chunks_, __ATOMIC_ACQUIRE);
    if (table == nullptr || size > PAYLOAD_BYTES)
        return false;

    /* Interrupts are off: only an NMI can come in here on this CPU. One that
       finds a write in progress takes nothing and says so in the count. No
       read-modify-write is needed for that: an NMI between the look and the
       store below finds 0, writes its record whole and puts the 0 back, and
       this write then takes the next index -- it looks at head_ only after
       its own store. */
    if (__atomic_load_n(&writing_, __ATOMIC_RELAXED) != 0)
    {
        __atomic_store_n(&dropped_, __atomic_load_n(&dropped_, __ATOMIC_RELAXED) + 1, __ATOMIC_RELAXED);
        return false;
    }
    __atomic_store_n(&writing_, 1, __ATOMIC_RELAXED);
    hal::compiler_barrier();

    /* The payload in whole words, its tail zeroed: no byte of the stack
       copied out that the event did not give. */
    u64 words[PAYLOAD_WORDS] = {};
    if (size != 0)
        stdlib::mem_cpy(words, payload, size);

    /* Only this CPU writes head_: its own last store, read back. */
    u64 index = __atomic_load_n(&head_, __ATOMIC_RELAXED);
    u64* s = slot(table, index);

    /* The slot no record while it is rewritten: the sequence word first. */
    store_word(&s[SEQ_WORD], 0);
    hal::smp_wmb();
    store_word(&s[CYCLES_WORD], cycles);
    store_word(&s[ID_WORD], (u64)id | ((u64)size << SIZE_SHIFT));
    for (ulong i = 0; i < PAYLOAD_WORDS; i++)
        store_word(&s[HEADER_WORDS + i], words[i]);
    /* Everything else before the word that says it is record `index`. */
    hal::smp_wmb();
    store_word(&s[SEQ_WORD], index + 1);
    __atomic_store_n(&head_, index + 1, __ATOMIC_RELEASE);

    hal::compiler_barrier();
    __atomic_store_n(&writing_, 0, __ATOMIC_RELAXED);
    return true;
}

u64 TraceRing::head() const
{
    return __atomic_load_n(&head_, __ATOMIC_ACQUIRE);
}

u64 TraceRing::dropped() const
{
    return __atomic_load_n(&dropped_, __ATOMIC_RELAXED);
}

bool TraceRing::read(u64 index, Record& out) const
{
    u64** table = __atomic_load_n(&chunks_, __ATOMIC_ACQUIRE);
    if (table == nullptr)
        return false;

    const u64* s = slot(table, index);
    /* Acquire: the words below are read after it. */
    u64 before = __atomic_load_n(&s[SEQ_WORD], __ATOMIC_ACQUIRE);
    if (before != index + 1)
        return false;

    u64 cycles = load_word(&s[CYCLES_WORD]);
    u64 id_word = load_word(&s[ID_WORD]);
    u64 words[PAYLOAD_WORDS];
    for (ulong i = 0; i < PAYLOAD_WORDS; i++)
        words[i] = load_word(&s[HEADER_WORDS + i]);

    /* The copy before the second look: a writer that had begun on the slot
       meanwhile cleared the word before it touched the rest. */
    hal::smp_rmb();
    if (load_word(&s[SEQ_WORD]) != index + 1)
        return false;

    ulong size = (ulong)(id_word >> SIZE_SHIFT) & ID_MASK;
    if (size > PAYLOAD_BYTES)
        return false;

    out.index = index;
    out.cycles = cycles;
    out.id = (u16)(id_word & ID_MASK);
    out.size = (u16)size;
    out.cpu = 0;
    for (ulong i = 0; i < PAYLOAD_WORDS; i++)
        out.payload[i] = words[i];
    return true;
}

}
