#pragma once

#include "allocator.h"

#include <include/types.h>
#include <kernel/spin_lock.h>

namespace kernel
{

namespace mm
{

/* Where the heap's Rust blocks were allocated: memsites
   (docs/memsites.md; the report is kernel/memsites.h's).

   Rust's allocator asks the heap for every block through one function,
   kernel_alloc. While the tracker is on, that function gives each block a
   site: the return addresses above it, FRAMES of them, interned in a table
   of the stacks seen so far, which only grows. The site's number goes where
   the heap keeps a block's owner -- a pool block's tag (Pool::Block), which
   every Rust block carried as one 'rust' before -- and a block of the page
   allocator's, half a page and more, which has no tag, is noted in a table
   of the big blocks from its allocation to its free. What a site holds is
   not counted on the way: it is read off the heap when asked, each pool's
   pages walked under the pool's lock (Collector) and the big blocks' table
   under its own (collect_big) -- what is live at that moment, whatever came
   and went before. A free costs a pool's block nothing more, and a big
   block its line out of the table.

   Off, the allocator pays a load and a branch for it. Turned off, the
   blocks given a site keep it until they are freed; turned on, the blocks
   from before are counted as nobody's site -- untracked.

   The tables are made the first time it is turned on and kept for good: a
   block's tag names a stack in them for as long as the block lives. No
   lock is taken to find a stack or to add one -- a slot is claimed by a
   compare-and-swap and published by a release store of its hash -- so two
   CPUs adding the same new stack at once may each get a slot of its own,
   which the report merges by the frames. */
class AllocSites final
{
public:
    /* The return addresses a site keeps: Rust's shims and alloc's growth
       paths take up to five of them, the rest are the code's */
    static constexpr size_t FRAMES = 8;

    struct Site
    {
        /* 0 empty, CLAIMED while its frames are being written, then their
           hash, which is never either */
        ulong hash;
        ulong frames[FRAMES];
    };

    /* Each table a run of pages the page allocator gives as it is */
    static constexpr size_t TABLE_BYTES = 256 * 1024;
    static constexpr size_t SITES = TABLE_BYTES / sizeof(Site);

    /* A big block's line: where it is, its site, what it was asked for and
       what it takes of RAM */
    struct Big
    {
        ulong addr;
        u32 site;
        u32 asked;
        u32 taken;
        u32 unused;
    };
    static constexpr size_t BIG_SLOTS = TABLE_BYTES / sizeof(Big);

    /* The big blocks' table in shards by address, a lock each: CPUs taking
       pages at once take a shard each, rather than all of them the one lock
       -- which cost a pair of a page's allocation and free 1.4 us on four
       CPUs where it cost 0.1 us (heapbench) */
    static constexpr size_t BIG_SHARDS = 32;
    static constexpr size_t SHARD_SLOTS = BIG_SLOTS / BIG_SHARDS;
    /* A shard's lock and count on a line of their own: 128 bytes, Apple's
       line and two of x86's */
    static constexpr size_t SHARD_BYTES = 128;

    struct ShardHead
    {
        SpinLock lock;
        /* Under lock, and stored atomically so that big_forget can see
           there is nothing to look for without it */
        ulong live;
        Big* slots;
    };
    struct Shard : ShardHead
    {
        u8 unused[SHARD_BYTES - sizeof(ShardHead)];
    };

    /* What a site holds: its blocks, what they were asked for and what
       they take of RAM */
    struct Totals
    {
        ulong blocks;
        ulong asked;
        ulong taken;
    };

    /* Constant-initialised, nothing to destroy: the one the allocator uses
       is a global no constructor has to run for, and the fuzzer makes its
       own */
    constexpr AllocSites()
        : on_(false)
        , sites_(nullptr)
        , shards_(nullptr)
        , interned_(0)
        , missed_(0)
        , big_refused_(0)
    {
    }

    static AllocSites& get_instance()
    {
        return instance_;
    }

    /* On, its tables made the first time; false, and off, when there is no
       memory for them. Called where an allocation may sleep. */
    bool enable();
    void disable();

    bool enabled() const
    {
        return __atomic_load_n(&on_, __ATOMIC_RELAXED);
    }

    /* Whether it was ever on: whether there are sites to report */
    bool made() const
    {
        return __atomic_load_n(&sites_, __ATOMIC_ACQUIRE) != nullptr;
    }

    /* The tag for a block allocated now by the function whose frame pointer
       is fp -- its site's -- or 0 for none: not on, no frames to read, or
       no room for a new stack (counted: missed) */
    ulong site_tag(ulong fp);

    /* A block of the page allocator's at raw, given the site tag `tag`
       (site_tag's): noted until big_forget, unless its shard is full
       (counted: big_refused). */
    void big_note(const void* raw, ulong tag, size_t asked, size_t taken);

    /* The block at raw of the page allocator's, about to be freed: out of
       the table if it is in it. A load and a branch while its shard holds
       nothing. */
    void big_forget(const void* raw);

    static constexpr bool is_site(ulong tag)
    {
        return (tag & TAG_MASK) == SITE_TAG;
    }

    static constexpr size_t index_of(ulong tag)
    {
        return static_cast<size_t>(tag & ~TAG_MASK);
    }

    /* A walk of the heap's pools (mm::walk_heap) counting each block that
       carries a site into totals[SITES] */
    class Collector final : public HeapVisitor
    {
    public:
        explicit Collector(Totals* totals)
            : totals_(totals)
        {
        }

        void block(ulong tag, size_t asked, size_t taken) override;

    private:
        Totals* totals_;
    };

    /* The big blocks noted, each counted into totals[SITES] */
    void collect_big(Totals* totals);

    /* The frames of site index, into frames[FRAMES]: how many, 0 for a site
       that is not there (or not yet written) */
    size_t frames_of(size_t index, ulong* frames);

    struct Counts
    {
        /* Stacks interned, of SITES */
        ulong sites;
        /* Allocations no site was given while on */
        ulong missed;
        /* Big blocks noted now, and those the table had no room for */
        ulong big;
        ulong big_refused;
    };
    Counts counts();

private:
    /* 'SITE' in a tag's high half, the site's index in its low one: no
       tag of the C++ code's (four characters, the low half) and not the
       pools' FREED_TAG */
    static constexpr ulong SITE_TAG = 0x5349544500000000UL;
    static constexpr ulong TAG_MASK = 0xFFFFFFFF00000000UL;
    static constexpr ulong CLAIMED = 1;

    /* How far a stack is looked for past its slot before it is given up
       on, and how full the table is let get: past it a new stack is not
       added, so that a probe always ends at an empty slot soon */
    static constexpr size_t PROBES = 32;
    static constexpr size_t SITES_FULL = SITES * 7 / 8;
    static constexpr size_t SHARD_FULL = SHARD_SLOTS * 7 / 8;

    static ulong hash_of(const ulong* frames);
    /* A big block's shard, and its home slot in it: by its page number,
       spread */
    static size_t shard_of(ulong addr);
    static size_t big_home(ulong addr);
    /* Slot `hole` of a shard emptied, the lines after it moved back to
       where a look for them reaches them again: no tombstones. The shard's
       lock held. */
    static void big_remove_at(Shard& shard, size_t hole);

    bool on_;
    Site* sites_;
    /* BIG_SHARDS of them, made with the stacks' table and kept with it,
       each a run of SHARD_SLOTS lines of one table of TABLE_BYTES */
    Shard* shards_;
    ulong interned_;
    ulong missed_;
    ulong big_refused_;

    static AllocSites instance_;
};

}
}
