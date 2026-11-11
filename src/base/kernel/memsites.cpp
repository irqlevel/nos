#include "memsites.h"
#include "klog.h"
#include "parameters.h"
#include "symtab.h"

#include <include/const.h>
#include <lib/stdlib.h>
#include <mm/heap_usage.h>
#include <mm/new.h>

namespace kernel
{

namespace memsites
{

using mm::AllocSites;

namespace
{

const ulong MEMSITES_TAG = 'MemS';

/* The row the tags view counts every site's blocks in: no tag of the
   heap's is all ones */
const ulong SITE_ROW = ~0UL;

/* A line of the report: the frame that is the code's own, which the sites
   of other callers above it are merged by; the site of them holding the
   most, whose callers are printed; how many were merged */
struct Group
{
    ulong own;
    size_t index;
    ulong index_taken;
    size_t paths;
    bool shown;
    AllocSites::Totals totals;
};

void add(AllocSites::Totals& to, const AllocSites::Totals& from)
{
    to.blocks += from.blocks;
    to.asked += from.asked;
    to.taken += from.taken;
}

bool starts_with(const char* s, const char* prefix)
{
    for (; *prefix != '\0'; s++, prefix++)
    {
        if (*s != *prefix)
            return false;
    }
    return true;
}

/* Bytes as "123", "12.3K" or "4.5M": exact below 10 KiB, to a tenth past */
void size_text(ulong bytes, char* buf, size_t size)
{
    const ulong exact = 10 * consts::KB;
    if (bytes < exact)
        stdlib::sn_printf(buf, size, "%lu", bytes);
    else if (bytes < 10 * consts::MB)
        stdlib::sn_printf(buf, size, "%lu.%luK", bytes / consts::KB, bytes % consts::KB * 10 / consts::KB);
    else
        stdlib::sn_printf(buf, size, "%lu.%luM", bytes / consts::MB, bytes % consts::MB * 10 / consts::MB);
}

/* A frame as the symbol table names it, or its address where it names none
   -- a module gone since, or one whose table is busy this moment */
void frame_name(ulong frame, char* buf, size_t size)
{
    if (!SymbolTable::get_instance().describe_return(frame, buf, size))
        stdlib::sn_printf(buf, size, "0x%lx", frame);
}

/* By own frame, a gap sequence's insertion sorts: the report merges the
   sites of one frame */
void sort_by_own(Group* groups, size_t count)
{
    static const size_t GAPS[] = { 701, 301, 132, 57, 23, 10, 4, 1 };
    for (size_t gap : GAPS)
    {
        for (size_t i = gap; i < count; i++)
        {
            const Group g = groups[i];
            size_t j = i;
            for (; j >= gap && groups[j - gap].own > g.own; j -= gap)
                groups[j] = groups[j - gap];
            groups[j] = g;
        }
    }
}

/* What the Rust heap holds, by its own counters (kernel_alloc's): what the
   sites hold is a part of it, the rest the blocks with none */
void untracked_line(stdlib::Printer& out, const AllocSites::Totals& tracked)
{
    mm::HeapUsage::Totals rust;
    mm::rust_heap_usage().read(rust);
    /* Two readings of a heap that went on meanwhile, the counters to within
       a CPU's fold: a difference below nothing is none */
    const ulong taken = (rust.taken > static_cast<long>(tracked.taken)) ? rust.taken - tracked.taken : 0;
    const ulong blocks = (rust.blocks > static_cast<long>(tracked.blocks)) ? rust.blocks - tracked.blocks : 0;
    char held[16];
    char rest[16];
    size_text(tracked.taken, held, sizeof(held));
    size_text(taken, rest, sizeof(rest));
    out.printf("memsites: the sites hold %s in %lu blocks; the rest of Rust's heap, about %s in %lu blocks, "
        "has none: allocated while off, or no room for its stack\n", held, tracked.blocks, rest, blocks);
}

/* Counts the pools' blocks by tag, every site's under SITE_ROW: the tags
   view. Under a pool's lock: nothing allocated, rows made beforehand. */
class TagCount final : public mm::HeapVisitor
{
public:
    struct Row
    {
        ulong tag;
        AllocSites::Totals totals;
    };
    static constexpr size_t ROWS = 64;

    explicit TagCount(Row* rows)
        : rows_(rows)
        , count_(0)
        , other_{0, {0, 0, 0}}
    {
    }

    void block(ulong tag, size_t asked, size_t taken) override
    {
        if (AllocSites::is_site(tag))
            tag = SITE_ROW;
        Row* row = nullptr;
        for (size_t i = 0; i < count_ && row == nullptr; i++)
        {
            if (rows_[i].tag == tag)
                row = &rows_[i];
        }
        if (row == nullptr && count_ < ROWS)
        {
            row = &rows_[count_++];
            row->tag = tag;
            row->totals = {0, 0, 0};
        }
        if (row == nullptr)
            row = &other_;
        row->totals.blocks++;
        row->totals.asked += asked;
        row->totals.taken += taken;
    }

    size_t count() const
    {
        return count_;
    }

    const Row& other() const
    {
        return other_;
    }

private:
    Row* rows_;
    size_t count_;
    Row other_;
};

/* A tag as text: the four characters the C++ code's are, Rust's sites as
   one, anything else as a number */
void tag_text(ulong tag, char* buf, size_t size)
{
    if (tag == SITE_ROW)
    {
        stdlib::sn_printf(buf, size, "rust, by site");
        return;
    }
    const size_t chars = 4;
    char text[chars + 1];
    bool printable = (tag >> (chars * 8)) == 0;
    for (size_t i = 0; i < chars && printable; i++)
    {
        const char c = static_cast<char>((tag >> ((chars - 1 - i) * 8)) & 0xFF);
        printable = c >= ' ' && c <= '~';
        text[i] = c;
    }
    text[chars] = '\0';
    if (printable)
        stdlib::sn_printf(buf, size, "%s", text);
    else
        stdlib::sn_printf(buf, size, "0x%lx", tag);
}

}

void setup()
{
    if (!Parameters::get_instance().is_mem_sites())
        return;
    if (AllocSites::get_instance().enable())
        klog(0, "memsites: on from boot");
    else
        klog(0, "memsites: no memory for its tables: off");
}

bool collect(AllocSites::Totals* totals)
{
    for (size_t i = 0; i < AllocSites::SITES; i++)
        totals[i] = {0, 0, 0};
    auto& sites = AllocSites::get_instance();
    if (!sites.made())
        return false;
    AllocSites::Collector collector(totals);
    mm::walk_heap(collector);
    sites.collect_big(totals);
    return true;
}

bool is_machinery(const char* name)
{
    static const char* const MACHINERY[] = {
        /* Rust's allocator shims: __rust_alloc and the rest */
        "__rustc::",
        /* alloc's and core's own: Vec, Box, String, BTreeMap, fmt */
        "alloc::", "core::", "<alloc::", "<core::",
        /* Their methods of slices and str: to_vec, to_owned, into_boxed */
        "<[", "<str>", "<str as ",
        /* The kernel's GlobalAlloc: realloc and alloc_zeroed */
        "<ffi::alloc::KernelAllocator",
    };
    for (const char* prefix : MACHINERY)
    {
        if (starts_with(name, prefix))
            return true;
    }
    /* A trait of alloc's for a type of the code's -- to_string, to_owned:
       alloc's code, made for that type */
    return stdlib::str_str(name, " as alloc::") != nullptr;
}

size_t own_position(const ulong* frames, size_t count)
{
    for (size_t i = 0; i < count; i++)
    {
        char name[SymbolTable::DESCRIBE_MAX];
        /* A frame the table cannot name is the code's for all it knows */
        if (!SymbolTable::get_instance().describe_return(frames[i], name, sizeof(name)) || !is_machinery(name))
            return i;
    }
    return (count == 0) ? 0 : count - 1;
}

void report(stdlib::Printer& out, size_t top)
{
    auto& sites = AllocSites::get_instance();
    const AllocSites::Counts counts = sites.counts();
    out.printf("memsites: %s; %lu stacks of %lu, %lu allocations given none, %lu big blocks noted (%lu not)\n",
        sites.enabled() ? "on" : "off", counts.sites, AllocSites::SITES, counts.missed, counts.big,
        counts.big_refused);
    if (!sites.made())
    {
        out.printf("memsites: not on since boot: `memsites on`, or memsites=on at boot\n");
        return;
    }

    auto* totals = static_cast<AllocSites::Totals*>(
        mm::alloc(AllocSites::SITES * sizeof(AllocSites::Totals), MEMSITES_TAG));
    auto* groups = static_cast<Group*>(mm::alloc(AllocSites::SITES * sizeof(Group), MEMSITES_TAG));
    if (totals == nullptr || groups == nullptr)
    {
        if (totals != nullptr)
            mm::free(totals);
        if (groups != nullptr)
            mm::free(groups);
        out.printf("memsites: no memory for the report\n");
        return;
    }

    collect(totals);
    size_t count = 0;
    AllocSites::Totals tracked = {0, 0, 0};
    for (size_t i = 0; i < AllocSites::SITES; i++)
    {
        if (totals[i].blocks == 0)
            continue;
        ulong frames[AllocSites::FRAMES];
        const size_t n = sites.frames_of(i, frames);
        const ulong own = (n == 0) ? 0 : frames[own_position(frames, n)];
        groups[count++] = {own, i, totals[i].taken, 1, false, totals[i]};
        add(tracked, totals[i]);
    }

    /* One group a frame of the code's own: what its callers' sites hold,
       summed, and the one of them that holds the most kept to print */
    sort_by_own(groups, count);
    size_t merged = 0;
    for (size_t i = 0; i < count; i++)
    {
        if (merged != 0 && groups[merged - 1].own == groups[i].own)
        {
            Group& g = groups[merged - 1];
            add(g.totals, groups[i].totals);
            g.paths++;
            if (groups[i].index_taken > g.index_taken)
            {
                g.index = groups[i].index;
                g.index_taken = groups[i].index_taken;
            }
            continue;
        }
        groups[merged++] = groups[i];
    }

    out.printf("     taken      asked  blocks paths  where (and the two frames above it)\n");
    for (size_t shown = 0; shown < top && shown < merged; shown++)
    {
        size_t best = merged;
        for (size_t i = 0; i < merged; i++)
        {
            if (!groups[i].shown && (best == merged || groups[i].totals.taken > groups[best].totals.taken))
                best = i;
        }
        Group& g = groups[best];
        g.shown = true;

        char taken[16];
        char asked[16];
        char name[SymbolTable::DESCRIBE_MAX];
        size_text(g.totals.taken, taken, sizeof(taken));
        size_text(g.totals.asked, asked, sizeof(asked));
        ulong frames[AllocSites::FRAMES];
        const size_t n = sites.frames_of(g.index, frames);
        const size_t at = own_position(frames, n);
        if (n == 0)
            stdlib::sn_printf(name, sizeof(name), "?");
        else
            frame_name(frames[at], name, sizeof(name));
        out.printf("%10s %10s %7lu %5lu  %s\n", taken, asked, g.totals.blocks, g.paths, name);
        for (size_t k = at + 1; k < n && k <= at + 2; k++)
        {
            frame_name(frames[k], name, sizeof(name));
            out.printf("%37s<- %s\n", "", name);
        }
    }
    untracked_line(out, tracked);

    mm::free(groups);
    mm::free(totals);
}

void dump(stdlib::Printer& out)
{
    auto& sites = AllocSites::get_instance();
    auto* totals = static_cast<AllocSites::Totals*>(
        mm::alloc(AllocSites::SITES * sizeof(AllocSites::Totals), MEMSITES_TAG));
    if (totals == nullptr)
    {
        out.printf("memsites: no memory for the dump\n");
        return;
    }

    out.printf("memsites-dump 1\n");
    collect(totals);
    for (size_t i = 0; i < AllocSites::SITES; i++)
    {
        if (totals[i].blocks == 0)
            continue;
        out.printf("site %lu %lu %lu %lu\n", i, totals[i].blocks, totals[i].asked, totals[i].taken);
        ulong frames[AllocSites::FRAMES];
        const size_t n = sites.frames_of(i, frames);
        for (size_t k = 0; k < n; k++)
        {
            char name[SymbolTable::DESCRIBE_MAX];
            frame_name(frames[k], name, sizeof(name));
            out.printf("frame 0x%lx %s\n", frames[k], name);
        }
    }
    mm::HeapUsage::Totals rust;
    mm::rust_heap_usage().read(rust);
    out.printf("rust %ld %ld %ld\n", rust.blocks, rust.bytes, rust.taken);
    out.printf("end\n");
    mm::free(totals);
}

void tags(stdlib::Printer& out)
{
    auto* rows = static_cast<TagCount::Row*>(mm::alloc(TagCount::ROWS * sizeof(TagCount::Row), MEMSITES_TAG));
    if (rows == nullptr)
    {
        out.printf("memsites: no memory for the tags\n");
        return;
    }
    TagCount count(rows);
    mm::walk_heap(count);

    /* By bytes taken, the most first */
    const size_t n = count.count();
    for (size_t i = 1; i < n; i++)
    {
        const TagCount::Row row = rows[i];
        size_t j = i;
        for (; j > 0 && rows[j - 1].totals.taken < row.totals.taken; j--)
            rows[j] = rows[j - 1];
        rows[j] = row;
    }

    out.printf("memsites tags: the heap's pool blocks by owner (a block of half a page and more has no tag)\n");
    out.printf("     taken      asked  blocks  owner\n");
    for (size_t i = 0; i <= n; i++)
    {
        const TagCount::Row& row = (i < n) ? rows[i] : count.other();
        if (i == n && row.totals.blocks == 0)
            break;
        char taken[16];
        char asked[16];
        char tag[24];
        size_text(row.totals.taken, taken, sizeof(taken));
        size_text(row.totals.asked, asked, sizeof(asked));
        if (i < n)
            tag_text(row.tag, tag, sizeof(tag));
        else
            stdlib::sn_printf(tag, sizeof(tag), "the other tags");
        out.printf("%10s %10s %7lu  %s\n", taken, asked, row.totals.blocks, tag);
    }
    mm::free(rows);
}

}
}
