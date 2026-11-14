// memsites' tables (mm/alloc_sites.cpp): the stacks Rust's allocations are
// given sites by, interned with no lock -- the input's stacks of a few
// shapes, repeated, sharing frames, some with none, and thousands of them at
// once, past where the table stops taking new ones -- and the big blocks'
// table, its lines noted and forgotten in any order and by the thousand,
// past full, each forgotten by a backward shift with no tombstone.
//
// Held to a model: every site_tag of a stack gives the one site whose frames
// are that stack's -- the same one each time, one task alone -- and never a
// site of other frames; a stack refused one is a stack the table had no
// room for, and counted; the big blocks' table holds exactly the lines noted
// and not forgotten, every one reachable from its home slot without an empty
// slot between, none twice, its count the model's; collect_big counts each
// site's blocks, asked and taken as the model does; and the Collector, a
// heap walk's visitor, counts the blocks of sites into their sites and no
// other blocks at all.
#include "host.h"

#define private public
#include <mm/alloc_sites.h>
#undef private

#include <kernel/spin_lock.h>
#include <kernel/stack_trace.h>
#include <mm/new.h>

#include "fuzz.h"
#include "kernel.h"

#include <map>
#include <set>
#include <vector>

using kernel::mm::AllocSites;

namespace
{

/* The input's stacks, by index: site_tag's fp is the index of the one the
   stand-in StackTrace hands back */
std::vector<std::vector<ulong>> stacks;

}

namespace kernel
{

size_t StackTrace::capture_from(ulong rbp, ulong* frames, size_t max_frames)
{
    INVARIANT(rbp < stacks.size(), "site_tag walked a stack the target never made: %lu", rbp);
    const std::vector<ulong>& s = stacks[rbp];
    size_t n = 0;
    for (; n < s.size() && n < max_frames; n++)
        frames[n] = s[n];
    return n;
}

}

namespace
{

const ulong PAGE_BYTES = 4096;

/* A frame: a return address of the kernel's, from few enough that stacks
   share them */
ulong frame(fuzz::Input& in)
{
    return 0xffffffff80100000UL + in.below(64) * 0x40 + in.below(4);
}

/* A stack of 0..12 frames: past FRAMES the walk cuts it, and the site keeps
   its first FRAMES */
std::vector<ulong> stack(fuzz::Input& in)
{
    std::vector<ulong> s(in.below(13));
    for (auto& f : s)
        f = frame(in);
    return s;
}

/* The stack as a site keeps it: its first FRAMES, zeros past its end */
std::vector<ulong> kept(const std::vector<ulong>& s)
{
    std::vector<ulong> k(AllocSites::FRAMES, 0);
    for (size_t i = 0; i < s.size() && i < AllocSites::FRAMES; i++)
        k[i] = s[i];
    return k;
}

struct Line
{
    size_t site;
    ulong asked;
    ulong taken;
};

struct Model
{
    /* Each stack's site, as the table gave it */
    std::map<std::vector<ulong>, size_t> sites;
    ulong missed = 0;
    std::map<ulong, Line> big;
    ulong big_refused = 0;
};

/* site_tag for stack index k, held to the model */
void ask(AllocSites& t, Model& m, size_t k)
{
    const std::vector<ulong> want = kept(stacks[k]);
    const ulong tag = t.site_tag(k);
    if (tag == 0)
    {
        /* No frames, or no room for a stack the table has not got. One it
           has is always found: slots never empty again, so the look takes
           no more probes than the stack's own insertion did */
        INVARIANT(stacks[k].empty() || m.sites.count(want) == 0, "stack %zu, which has site %zu, given none", k,
                  m.sites.count(want) != 0 ? m.sites[want] : 0);
        m.missed++;
        fuzz::reached(stacks[k].empty() ? "a stack of no frames" : "a stack refused a site");
        return;
    }
    INVARIANT(AllocSites::is_site(tag), "site_tag gave 0x%lx, no site's tag", tag);
    const size_t index = AllocSites::index_of(tag);
    INVARIANT(index < AllocSites::SITES, "site_tag gave site %zu of %zu", index, AllocSites::SITES);
    ulong frames[AllocSites::FRAMES];
    const size_t n = t.frames_of(index, frames);
    INVARIANT(n <= AllocSites::FRAMES && std::vector<ulong>(frames, frames + AllocSites::FRAMES) == want,
              "site %zu holds frames other than the stack's it was given for", index);
    auto it = m.sites.find(want);
    if (it == m.sites.end())
    {
        m.sites[want] = index;
        fuzz::reached("a new stack interned");
    }
    else
    {
        INVARIANT(it->second == index, "one stack given sites %zu and %zu", it->second, index);
        fuzz::reached("a stack found again");
    }
}

/* The big blocks' lines the shards count */
ulong big_lines(AllocSites& t)
{
    ulong n = 0;
    for (size_t s = 0; s < AllocSites::BIG_SHARDS; s++)
        n += t.shards_[s].live;
    return n;
}

/* Every line of the big table in its own address's shard, where a look for
   it from its home finds it, none twice; each shard counting the lines it
   holds; as many as the model has */
void check_big(AllocSites& t, const Model& m)
{
    std::set<ulong> seen;
    for (size_t s = 0; s < AllocSites::BIG_SHARDS; s++)
    {
        const AllocSites::Shard& shard = t.shards_[s];
        ulong held = 0;
        for (size_t i = 0; i < AllocSites::SHARD_SLOTS; i++)
        {
            const AllocSites::Big& b = shard.slots[i];
            if (b.addr == 0)
                continue;
            held++;
            INVARIANT(seen.insert(b.addr).second, "0x%lx in the big table twice", b.addr);
            INVARIANT(AllocSites::shard_of(b.addr) == s, "0x%lx in shard %zu, not its own %zu", b.addr, s,
                      AllocSites::shard_of(b.addr));
            auto it = m.big.find(b.addr);
            INVARIANT(it != m.big.end(), "0x%lx in the big table, forgotten or never noted", b.addr);
            INVARIANT(b.site == it->second.site && b.asked == it->second.asked && b.taken == it->second.taken,
                      "0x%lx's line is not what was noted", b.addr);
            /* Reachable: from its home to it, no empty slot */
            size_t at = AllocSites::big_home(b.addr);
            while (at != i)
            {
                INVARIANT(shard.slots[at].addr != 0,
                          "0x%lx past an empty slot from its home: a look stops short of it", b.addr);
                at = (at + 1) % AllocSites::SHARD_SLOTS;
            }
        }
        INVARIANT(shard.live == held, "shard %zu counts %lu lines and holds %lu", s, shard.live, held);
    }
    INVARIANT(seen.size() == m.big.size(), "the big table has %zu lines, the model %zu", seen.size(), m.big.size());
}

/* collect_big, and a Collector fed blocks: what each site holds is the
   model's */
void check_totals(AllocSites& t, const Model& m, fuzz::Input& in)
{
    std::vector<AllocSites::Totals> got(AllocSites::SITES, {0, 0, 0});
    std::vector<AllocSites::Totals> want(AllocSites::SITES, {0, 0, 0});
    t.collect_big(got.data());
    for (const auto& l : m.big)
    {
        want[l.second.site].blocks++;
        want[l.second.site].asked += l.second.asked;
        want[l.second.site].taken += l.second.taken;
    }

    /* A walk's blocks: of sites the model has, and of other tags, which
       count nowhere */
    AllocSites::Collector collector(got.data());
    std::vector<size_t> known;
    for (const auto& s : m.sites)
        known.push_back(s.second);
    for (ulong b = in.below(40); b > 0; b--)
    {
        const ulong asked = 1 + in.below(2000);
        const ulong taken = asked + 24 + in.below(40);
        if (!known.empty() && in.boolean())
        {
            const size_t site = known[in.below(known.size())];
            collector.block(AllocSites::SITE_TAG | site, asked, taken);
            want[site].blocks++;
            want[site].asked += asked;
            want[site].taken += taken;
        }
        else
        {
            /* 'rust', 'Test', none, the pools' FREED_TAG, and one a letter
               short of a site's */
            const ulong tags[] = {0x72757374UL, 0x54657374UL, 0, 0xF7EEF7EEF7EEF7EEUL, 0x5349544400000001UL};
            collector.block(tags[in.below(5)], asked, taken);
        }
    }
    for (size_t i = 0; i < AllocSites::SITES; i++)
    {
        INVARIANT(got[i].blocks == want[i].blocks && got[i].asked == want[i].asked && got[i].taken == want[i].taken,
                  "site %zu: counted %lu blocks, %lu asked, %lu taken; the model %lu, %lu, %lu", i, got[i].blocks,
                  got[i].asked, got[i].taken, want[i].blocks, want[i].asked, want[i].taken);
    }
    fuzz::reached("the sites' totals read");
}

/* A page-aligned address of the big blocks', from a range the input's
   notes reuse */
ulong big_addr(fuzz::Input& in, ulong spread)
{
    return 0xffff800001000000UL + in.below(spread) * PAGE_BYTES;
}

void run(fuzz::Input& in)
{
    stacks.clear();
    for (ulong n = 1 + in.below(24); n > 0; n--)
        stacks.push_back(stack(in));

    AllocSites t;
    INVARIANT(t.enable(), "no memory for the tables");
    Model m;

    for (int ops = 0; ops < 300 && in.more(); ops++)
    {
        switch (in.u8() % 10)
        {
        case 0:
        case 1:
        case 2:
            ask(t, m, in.below(stacks.size()));
            break;
        case 3:
        {
            /* Stacks by the thousand: the table past where it takes no
               new one */
            if (!in.chance(16))
                break;
            const ulong count = 1 + in.below(AllocSites::SITES + 200);
            const ulong seed = in.u32();
            for (ulong k = 0; k < count; k++)
            {
                stacks.push_back({0xffffffff80800000UL + seed * 7919 + k * 16, 0xffffffff80900000UL + k});
                ask(t, m, stacks.size() - 1);
            }
            if (t.interned_ >= AllocSites::SITES_FULL)
                fuzz::reached("the stacks' table full");
            break;
        }
        case 4:
        case 5:
        {
            /* A big block noted: a fresh address, of a site the model has */
            if (m.sites.empty())
                break;
            const ulong addr = big_addr(in, 4096);
            if (m.big.count(addr) != 0)
                break;
            auto it = m.sites.begin();
            std::advance(it, in.below(m.sites.size()));
            const ulong asked = PAGE_BYTES / 2 + in.below(64 * PAGE_BYTES);
            const ulong taken = PAGE_BYTES << in.below(8);
            const ulong before = big_lines(t);
            const ulong in_shard = t.shards_[AllocSites::shard_of(addr)].live;
            t.big_note(reinterpret_cast<void*>(addr), AllocSites::SITE_TAG | it->second, asked, taken);
            if (big_lines(t) == before)
            {
                INVARIANT(in_shard >= AllocSites::SHARD_FULL, "a big block refused a line, its shard holding %lu of %zu",
                          in_shard, AllocSites::SHARD_SLOTS);
                m.big_refused++;
                fuzz::reached("a big block refused: its shard full");
                break;
            }
            m.big[addr] = {it->second, asked, taken};
            fuzz::reached("a big block noted");
            break;
        }
        case 6:
        {
            /* A free: a block noted, or one that never was */
            ulong addr;
            if (!m.big.empty() && !in.chance(64))
            {
                auto it = m.big.begin();
                std::advance(it, in.below(m.big.size()));
                addr = it->first;
                m.big.erase(it);
                fuzz::reached("a noted big block forgotten");
            }
            else
            {
                addr = big_addr(in, 8192);
                if (m.big.erase(addr) != 0)
                    fuzz::reached("a noted big block forgotten");
            }
            t.big_forget(reinterpret_cast<void*>(addr));
            break;
        }
        case 7:
        {
            /* Big blocks by the thousand, noted and then most forgotten:
               the shifts back over long runs */
            if (!in.chance(16) || m.sites.empty())
                break;
            const size_t site = m.sites.begin()->second;
            const ulong count = 1 + in.below(AllocSites::BIG_SLOTS + 100);
            const ulong base = 0xffff900000000000UL + in.below(1 << 20) * PAGE_BYTES;
            for (ulong k = 0; k < count; k++)
            {
                const ulong addr = base + k * PAGE_BYTES * (1 + in.below(3));
                if (m.big.count(addr) != 0)
                    continue;
                const ulong before = big_lines(t);
                const ulong in_shard = t.shards_[AllocSites::shard_of(addr)].live;
                t.big_note(reinterpret_cast<void*>(addr), AllocSites::SITE_TAG | site, PAGE_BYTES, PAGE_BYTES);
                if (big_lines(t) != before)
                {
                    m.big[addr] = {site, PAGE_BYTES, PAGE_BYTES};
                }
                else
                {
                    INVARIANT(in_shard >= AllocSites::SHARD_FULL, "a big block refused, its shard holding %lu",
                              in_shard);
                    m.big_refused++;
                    fuzz::reached("a big block refused: its shard full");
                }
            }
            for (auto it = m.big.begin(); it != m.big.end();)
            {
                if (in.chance(8))
                {
                    ++it;
                    continue;
                }
                t.big_forget(reinterpret_cast<void*>(it->first));
                it = m.big.erase(it);
            }
            fuzz::reached("big blocks by the thousand");
            break;
        }
        case 8:
            check_totals(t, m, in);
            break;
        default:
            /* Off and on: the tables kept, the sites theirs */
            t.disable();
            INVARIANT(!t.enabled(), "off, and on");
            INVARIANT(t.enable(), "on again refused");
            break;
        }
        if (m.big.size() < 2000 || ops % 32 == 0)
            check_big(t, m);
        fuzz::check_no_locks_held("an operation");
    }
    check_big(t, m);
    const AllocSites::Counts c = t.counts();
    INVARIANT(c.missed == m.missed && c.big_refused == m.big_refused && c.big == m.big.size(),
              "counts: %lu missed, %lu big, %lu refused; the model %lu, %zu, %lu", c.missed, c.big, c.big_refused,
              m.missed, m.big.size(), m.big_refused);

    /* The tables are kept for good in the kernel; here, each input's own:
       the big blocks' table is the first shard's run of it */
    kernel::mm::free(t.sites_);
    kernel::mm::free(t.shards_[0].slots);
    for (size_t s = 0; s < AllocSites::BIG_SHARDS; s++)
        t.shards_[s].~Shard();
    kernel::mm::free(t.shards_);
}

void reset()
{
    stacks.clear();
}

}

const fuzz::Target fuzz::THE_TARGET = {"allocsites", reset, run, 2048, 3000};
