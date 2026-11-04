#include "trace_ring.h"

#include <hal/barrier.h>
#include <mm/new.h>

namespace kernel
{

namespace
{

/* Every word of a slot is an atomic: the writer's stores (write_words, in
   trace_ring.h) and a reader's loads are no data race, only a copy the
   reader may throw away (the seqlock's look before and after says which).
   Relaxed, with the ordering the barriers give -- a plain mov or ldr on
   both architectures. */
inline u64 load_word(const u64* word)
{
    return __atomic_load_n(word, __ATOMIC_RELAXED);
}

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
        /* Zeroed below, once: mm::alloc would zero it too */
        table[i] = static_cast<u64*>(mm::alloc_uninit(CHUNK_BYTES, tag));
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
    __atomic_store_n(&last_cycles_, 0, __ATOMIC_RELAXED);
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

bool TraceRing::write(u16 id, u64 cycles, const void* payload, ulong size)
{
    if (size > PAYLOAD_BYTES)
        return false;

    /* The payload in whole words, its tail zeroed: no byte of the stack
       copied out that the event did not give. */
    u64 words[PAYLOAD_WORDS] = {};
    if (size != 0)
        stdlib::mem_cpy(words, payload, size);
    return write_words(id, cycles, size, words);
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
