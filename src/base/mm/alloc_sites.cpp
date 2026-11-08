#include "alloc_sites.h"
#include "new.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/spin_lock.h>
#include <kernel/stack_trace.h>
#include <lib/lock.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace mm
{

/* No constructor runs for it: constexpr, every member a word */
constinit AllocSites AllocSites::instance_;

namespace
{

const ulong SITES_TAG = 'Site';

static_assert(sizeof(AllocSites::Site) * AllocSites::SITES <= AllocSites::TABLE_BYTES, "the stacks past their table");
static_assert(sizeof(AllocSites::Big) * AllocSites::BIG_SLOTS <= AllocSites::TABLE_BYTES,
    "the big blocks past their table");
static_assert(sizeof(AllocSites::Shard) == AllocSites::SHARD_BYTES, "a shard not a line of its own");
/* The shards a page: a block the page allocator gives, page-aligned, so
   that each starts a line */
static_assert(AllocSites::BIG_SHARDS * AllocSites::SHARD_BYTES == consts::PAGE_SIZE, "the shards not a page");

}

bool AllocSites::enable()
{
    /* Made once and kept for good. Two at once: the one whose
       compare-and-swap loses gives back what it made */
    if (__atomic_load_n(&sites_, __ATOMIC_ACQUIRE) == nullptr)
    {
        auto* sites = static_cast<Site*>(mm::alloc(TABLE_BYTES, SITES_TAG));
        auto* big = static_cast<Big*>(mm::alloc(TABLE_BYTES, SITES_TAG));
        auto* shards = static_cast<Shard*>(mm::alloc(BIG_SHARDS * sizeof(Shard), SITES_TAG));
        if (sites == nullptr || big == nullptr || shards == nullptr)
        {
            if (sites != nullptr)
                mm::free(sites);
            if (big != nullptr)
                mm::free(big);
            if (shards != nullptr)
                mm::free(shards);
            return false;
        }
        /* Every slot empty: zeroed here, whatever the heap hands out */
        stdlib::mem_set(sites, 0, TABLE_BYTES);
        stdlib::mem_set(big, 0, TABLE_BYTES);
        for (size_t i = 0; i < BIG_SHARDS; i++)
        {
            /* Made here, the block being raw memory: a lock's lifetime is
               its constructor's to begin */
            Shard* shard = new (&shards[i]) Shard();
            shard->live = 0;
            shard->slots = big + i * SHARD_SLOTS;
        }

        /* The shards before the stacks: whoever sees the stacks sees them */
        Shard* no_shards = nullptr;
        if (!__atomic_compare_exchange_n(&shards_, &no_shards, shards, false, __ATOMIC_RELEASE, __ATOMIC_ACQUIRE))
        {
            for (size_t i = 0; i < BIG_SHARDS; i++)
                shards[i].~Shard();
            mm::free(shards);
            mm::free(big);
        }
        Site* no_sites = nullptr;
        if (!__atomic_compare_exchange_n(&sites_, &no_sites, sites, false, __ATOMIC_RELEASE, __ATOMIC_ACQUIRE))
            mm::free(sites);
    }
    __atomic_store_n(&on_, true, __ATOMIC_RELEASE);
    return true;
}

void AllocSites::disable()
{
    __atomic_store_n(&on_, false, __ATOMIC_RELEASE);
}

ulong AllocSites::hash_of(const ulong* frames)
{
    ulong hash = 0x9E3779B97F4A7C15UL;
    for (size_t i = 0; i < FRAMES; i++)
    {
        hash = (hash ^ frames[i]) * 0xBF58476D1CE4E5B9UL;
        hash ^= hash >> 31;
    }
    /* Never what an empty slot or a claimed one holds */
    return (hash <= CLAIMED) ? hash + CLAIMED + 1 : hash;
}

ulong AllocSites::site_tag(ulong fp)
{
    Site* sites = __atomic_load_n(&sites_, __ATOMIC_ACQUIRE);
    if (sites == nullptr)
        return 0;

    ulong frames[FRAMES];
    size_t count = StackTrace::capture_from(fp, frames, FRAMES);
    if (count == 0)
    {
        __atomic_add_fetch(&missed_, 1, __ATOMIC_RELAXED);
        return 0;
    }
    for (size_t i = count; i < FRAMES; i++)
        frames[i] = 0;

    const ulong hash = hash_of(frames);
    const size_t start = hash % SITES;
    for (size_t probe = 0; probe < PROBES; probe++)
    {
        const size_t index = (start + probe) % SITES;
        Site& site = sites[index];
        ulong seen = __atomic_load_n(&site.hash, __ATOMIC_ACQUIRE);
        if (seen == 0)
        {
            /* A new stack, if the table has room left for one: a look for
               a stack ends at the first empty slot, which a table let fill
               up would take ever longer to reach */
            if (__atomic_load_n(&interned_, __ATOMIC_RELAXED) >= SITES_FULL)
                break;
            if (__atomic_compare_exchange_n(&site.hash, &seen, CLAIMED, false, __ATOMIC_ACQUIRE, __ATOMIC_ACQUIRE))
            {
                for (size_t i = 0; i < FRAMES; i++)
                    site.frames[i] = frames[i];
                __atomic_add_fetch(&interned_, 1, __ATOMIC_RELAXED);
                /* The frames before the hash: whoever reads the hash with
                   acquire reads them */
                __atomic_store_n(&site.hash, hash, __ATOMIC_RELEASE);
                return SITE_TAG | index;
            }
            /* Another took the slot first: `seen` is what it holds now */
        }
        if (seen == hash)
        {
            bool same = true;
            for (size_t i = 0; i < FRAMES && same; i++)
                same = site.frames[i] == frames[i];
            if (same)
                return SITE_TAG | index;
        }
        /* Another stack's, or one being written: the next slot */
    }
    __atomic_add_fetch(&missed_, 1, __ATOMIC_RELAXED);
    return 0;
}

namespace
{

/* A big block's page number, spread by a multiply whose high bits are the
   well-mixed ones: the low of those pick its shard, the rest its home */
ulong spread(ulong addr)
{
    return ((addr >> consts::PAGE_SHIFT) * 0x9E3779B97F4A7C15UL) >> (stdlib::size_of_in_bits<ulong>() / 2);
}

}

size_t AllocSites::shard_of(ulong addr)
{
    return static_cast<size_t>(spread(addr) % BIG_SHARDS);
}

size_t AllocSites::big_home(ulong addr)
{
    return static_cast<size_t>((spread(addr) / BIG_SHARDS) % SHARD_SLOTS);
}

void AllocSites::big_note(const void* raw, ulong tag, size_t asked, size_t taken)
{
    Shard* shards = __atomic_load_n(&shards_, __ATOMIC_ACQUIRE);
    if (shards == nullptr || !is_site(tag) || raw == nullptr)
        return;
    const ulong addr = reinterpret_cast<ulong>(raw);
    Shard& shard = shards[shard_of(addr)];

    stdlib::AutoLock lock(shard.lock);
    if (shard.live >= SHARD_FULL)
    {
        __atomic_add_fetch(&big_refused_, 1, __ATOMIC_RELAXED);
        return;
    }
    size_t i = big_home(addr);
    while (shard.slots[i].addr != 0)
    {
        /* Noted twice: a block handed out while its last owner's line was
           still here -- freed with no big_forget, or handed out twice */
        BUG_ON(shard.slots[i].addr == addr);
        i = (i + 1) % SHARD_SLOTS;
    }
    const u32 most = static_cast<u32>(-1);
    Big& line = shard.slots[i];
    line.addr = addr;
    line.site = static_cast<u32>(index_of(tag));
    line.asked = (asked > most) ? most : static_cast<u32>(asked);
    line.taken = (taken > most) ? most : static_cast<u32>(taken);
    __atomic_store_n(&shard.live, shard.live + 1, __ATOMIC_RELAXED);
}

void AllocSites::big_forget(const void* raw)
{
    Shard* shards = __atomic_load_n(&shards_, __ATOMIC_ACQUIRE);
    if (shards == nullptr)
        return;
    const ulong addr = reinterpret_cast<ulong>(raw);
    Shard& shard = shards[shard_of(addr)];
    /* Nothing noted here: nothing to look for. A block noted on another CPU
       and handed here to be freed came with whatever ordered the hand-over,
       so its note is seen */
    if (__atomic_load_n(&shard.live, __ATOMIC_RELAXED) == 0)
        return;

    stdlib::AutoLock lock(shard.lock);
    size_t i = big_home(addr);
    /* Bounded by the shard: a look ends at an empty slot, of which there
       always is one -- SHARD_FULL is short of it */
    for (size_t n = 0; n < SHARD_SLOTS; n++)
    {
        if (shard.slots[i].addr == 0)
            return;
        if (shard.slots[i].addr == addr)
        {
            big_remove_at(shard, i);
            return;
        }
        i = (i + 1) % SHARD_SLOTS;
    }
}

void AllocSites::big_remove_at(Shard& shard, size_t hole)
{
    Big* slots = shard.slots;
    size_t at = hole;
    for (;;)
    {
        at = (at + 1) % SHARD_SLOTS;
        if (slots[at].addr == 0)
            break;
        /* A line stays where it is if its home is cyclically after the hole
           and not after it: a look for it starts past the hole. Otherwise
           it goes into the hole, and the hole moves to where it was. */
        const size_t home = big_home(slots[at].addr);
        const bool stays = (hole <= at) ? (hole < home && home <= at) : (hole < home || home <= at);
        if (stays)
            continue;
        slots[hole] = slots[at];
        hole = at;
    }
    slots[hole].addr = 0;
    __atomic_store_n(&shard.live, shard.live - 1, __ATOMIC_RELAXED);
}

void AllocSites::Collector::block(ulong tag, size_t asked, size_t taken)
{
    if (!is_site(tag))
        return;
    const size_t index = index_of(tag);
    /* Not a site of the table's: a tag written over -- the heap's own
       checks are the ones to catch it */
    if (index >= SITES)
        return;
    totals_[index].blocks++;
    totals_[index].asked += asked;
    totals_[index].taken += taken;
}

void AllocSites::collect_big(Totals* totals)
{
    Shard* shards = __atomic_load_n(&shards_, __ATOMIC_ACQUIRE);
    if (shards == nullptr)
        return;

    for (size_t s = 0; s < BIG_SHARDS; s++)
    {
        Shard& shard = shards[s];
        stdlib::AutoLock lock(shard.lock);
        for (size_t i = 0; i < SHARD_SLOTS; i++)
        {
            const Big& line = shard.slots[i];
            if (line.addr == 0 || line.site >= SITES)
                continue;
            Totals& t = totals[line.site];
            t.blocks++;
            t.asked += line.asked;
            t.taken += line.taken;
        }
    }
}

size_t AllocSites::frames_of(size_t index, ulong* frames)
{
    Site* sites = __atomic_load_n(&sites_, __ATOMIC_ACQUIRE);
    if (sites == nullptr || index >= SITES)
        return 0;
    const ulong hash = __atomic_load_n(&sites[index].hash, __ATOMIC_ACQUIRE);
    if (hash <= CLAIMED)
        return 0;
    size_t count = 0;
    for (size_t i = 0; i < FRAMES; i++)
    {
        frames[i] = sites[index].frames[i];
        if (frames[i] != 0)
            count = i + 1;
    }
    return count;
}

AllocSites::Counts AllocSites::counts()
{
    Counts c;
    c.sites = __atomic_load_n(&interned_, __ATOMIC_RELAXED);
    c.missed = __atomic_load_n(&missed_, __ATOMIC_RELAXED);
    c.big = 0;
    Shard* shards = __atomic_load_n(&shards_, __ATOMIC_ACQUIRE);
    for (size_t s = 0; shards != nullptr && s < BIG_SHARDS; s++)
        c.big += __atomic_load_n(&shards[s].live, __ATOMIC_RELAXED);
    c.big_refused = __atomic_load_n(&big_refused_, __ATOMIC_RELAXED);
    return c;
}

}
}
