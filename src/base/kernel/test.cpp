#include "klog.h"
#include "debug.h"
#include "task.h"
#include "sched.h"
#include "preempt.h"
#include "raw_rw_spin_lock.h"
#include "cpu.h"
#include "stack_trace.h"
#include "parameters.h"
#include "module.h"
#include "cmd.h"
#include "line_editor.h"
#include "elf.h"
#include "symtab.h"
#include "event.h"
#include "tracepoint.h"
#include "trace_ring.h"
#include "interrupt.h"
#include <hal/cpu.h>
#include <hal/irqchip.h>

#include <lib/error.h>
#include <lib/grub_env.h>
#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/vector.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <mm/new.h>
#include <mm/allocator.h>
#include <mm/alloc_sites.h>
#include <mm/heap_usage.h>
#include "memsites.h"

namespace kernel
{

namespace test
{

static const ulong TAG = 'Test';

stdlib::Error test_allocator()
{
    klog(0, "test_allocator: started");

    for (size_t size = 1; size <= 8 * consts::PAGE_SIZE; size++)
    {
        if (size % 500 == 0)
        {
            klog(0, "test_allocator: alloc size %lu", size);
        }

        u8 *block = new (mm::NO_THROW) u8[size];
        if (block == nullptr)
        {
            klog(0, "test_allocator: alloc failed at size %lu", size);
            return stdlib::Error::NO_MEMORY;
        }

        block[0] = 1;
        block[size / 2] = 1;
        block[size - 1] = 1;
        delete [] block;
    }

    klog(0, "test_allocator: complete");
    return make_error(stdlib::Error::SUCCESS);
}

/* What the heap takes for an allocation of size, as alloc_block takes it */
static bool check_footprint(size_t size, size_t want)
{
    const size_t got = mm::AllocatorImpl::footprint(size);
    if (got != want)
        klog(0, "test_heap_usage: an allocation of %lu takes %lu, not %lu", size, got, want);
    return got == want;
}

/* A read of usage, against what the test put into it */
static bool check_usage(mm::HeapUsage& usage, const char* when, const mm::HeapUsage::Totals& want)
{
    mm::HeapUsage::Totals got;
    usage.read(got);
    const bool ok = got.bytes == want.bytes && got.taken == want.taken && got.blocks == want.blocks &&
        got.bytes_peak == want.bytes_peak && got.taken_peak == want.taken_peak &&
        got.blocks_peak == want.blocks_peak && got.allocs == want.allocs && got.frees == want.frees &&
        got.refused == want.refused;
    if (!ok)
        klog(0, "test_heap_usage: %s: asked/taken/blocks %ld/%ld/%ld, peaks %ld/%ld/%ld, allocs/frees/refused "
             "%lu/%lu/%lu; want %ld/%ld/%ld, %ld/%ld/%ld, %lu/%lu/%lu", when,
             got.bytes, got.taken, got.blocks, got.bytes_peak, got.taken_peak, got.blocks_peak,
             got.allocs, got.frees, got.refused,
             want.bytes, want.taken, want.blocks, want.bytes_peak, want.taken_peak, want.blocks_peak,
             want.allocs, want.frees, want.refused);
    return ok;
}

/* The heap's counters (mm::HeapUsage, the Rust heap's behind memusage):
   what an allocation takes, and what goes in coming out of a read -- now,
   the peaks, the counts -- through a slot's folds by bytes and by blocks.
   On one CPU, so every read is exact; test_heap_usage_cpus is all of them
   at once. */
stdlib::Error test_heap_usage()
{
    const size_t page = consts::PAGE_SIZE;
    const size_t largest = mm::PageTable::MAX_CONTIGUOUS_PAGES * page;
    bool ok = true;

    /* Below half a page a pool's block -- the allocator's 8-byte header in
       it, 16 bytes the least -- and the pool's header in front of it */
    ok = check_footprint(1, mm::Pool::footprint(16)) && ok;
    ok = check_footprint(8, mm::Pool::footprint(16)) && ok;
    ok = check_footprint(9, mm::Pool::footprint(32)) && ok;
    ok = check_footprint(page / 2 - 9, mm::Pool::footprint(page / 2)) && ok;
    /* From there a run of pages, a power of two of them */
    ok = check_footprint(page / 2 - 8, page) && ok;
    ok = check_footprint(page, page) && ok;
    ok = check_footprint(page + 1, 2 * page) && ok;
    ok = check_footprint(5 * page, 8 * page) && ok;
    ok = check_footprint(largest, largest) && ok;
    /* Nothing past the largest class, or for nothing */
    ok = check_footprint(largest + 1, 0) && ok;
    ok = check_footprint(0, 0) && ok;

    /* log2_clz, which footprint takes its class by, is log2 -- which
       alloc_block takes it by -- by another road: every value to four
       pages, and either side of every power of two */
    for (size_t v = 0; v <= 4 * page && ok; v++)
    {
        ok = (stdlib::log2_clz(v) == stdlib::log2(v));
        if (!ok)
            klog(0, "test_heap_usage: log2_clz(%lu) %lu, log2 %lu", v, stdlib::log2_clz(v), stdlib::log2(v));
    }
    for (size_t shift = 1; shift < stdlib::size_of_in_bits<size_t>() && ok; shift++)
    {
        const size_t p = static_cast<size_t>(1) << shift;
        ok = stdlib::log2_clz(p - 1) == stdlib::log2(p - 1) && stdlib::log2_clz(p) == stdlib::log2(p) &&
            stdlib::log2_clz(p + 1) == stdlib::log2(p + 1);
        if (!ok)
            klog(0, "test_heap_usage: log2_clz and log2 part at 2^%lu", shift);
    }
    /* And so the footprint of every size alloc_block puts in a pool -- its
       8-byte header added, 16 bytes the least -- and of every run of pages
       to the largest class */
    for (size_t size = 1; size + 8 < page / 2 && ok; size++)
        ok = check_footprint(size, mm::Pool::footprint(static_cast<size_t>(1) << stdlib::max<size_t>(4, stdlib::log2(size + 8))));
    for (size_t pages = 1; pages <= mm::PageTable::MAX_CONTIGUOUS_PAGES && ok; pages++)
        ok = check_footprint(pages * page, (static_cast<size_t>(1) << stdlib::log2(pages)) * page);

    /* Counters of the test's own: the Rust heap's go on counting under it */
    static mm::HeapUsage usage;
    const long fold = mm::HeapUsage::FOLD_BYTES;
    const long fold_blocks = mm::HeapUsage::FOLD_BLOCKS;
    ok = check_usage(usage, "at first", {0, 0, 0, 0, 0, 0, 0, 0, 0}) && ok;

    for (ulong i = 0; i < 10; i++)
        usage.add(100, 136);
    ok = check_usage(usage, "ten in", {1000, 1360, 10, 1000, 1360, 10, 10, 0, 0}) && ok;

    /* Down, the peaks where they were */
    for (ulong i = 0; i < 4; i++)
        usage.sub(100, 136);
    ok = check_usage(usage, "four out", {600, 816, 6, 1000, 1360, 10, 10, 4, 0}) && ok;

    /* A block of a slot's worth: folded in, and out again -- the peaks of
       the bytes from the fold, the blocks' still the ten */
    usage.add(fold, fold);
    usage.sub(fold, fold);
    ok = check_usage(usage, "a fold in and out",
        {600, 816, 6, 600 + fold, 816 + fold, 10, 11, 5, 0}) && ok;

    /* A slot's worth of blocks of a byte: folded by their count */
    for (long i = 0; i < fold_blocks; i++)
        usage.add(1, 1);
    ok = check_usage(usage, "a fold of blocks",
        {600 + fold_blocks, 816 + fold_blocks, 6 + fold_blocks, 600 + fold, 816 + fold, 6 + fold_blocks,
         11 + static_cast<ulong>(fold_blocks), 5, 0}) && ok;
    for (long i = 0; i < fold_blocks; i++)
        usage.sub(1, 1);

    /* All out, and two the heap had nothing for */
    for (ulong i = 0; i < 6; i++)
        usage.sub(100, 136);
    usage.refuse();
    usage.refuse();
    const ulong all = 11 + static_cast<ulong>(fold_blocks);
    ok = check_usage(usage, "all out", {0, 0, 0, 600 + fold, 816 + fold, 6 + fold_blocks, all, all, 2}) && ok;

    if (!ok)
        return make_error(stdlib::Error::UNSUCCESSFUL);
    klog(0, "test_heap_usage: complete");
    return make_success();
}

/* The Rust half of test_alloc_sites (kernel crate, memsites.rs): blocks
   allocated by its memsites_probe and kept, and freed */
extern "C" bool rust_memsites_probe(unsigned long count, unsigned long size);
extern "C" void rust_memsites_release();

/* What the sites whose own frame is the probe's function hold, as the
   report would merge them, read off the heap now */
static mm::AllocSites::Totals probe_totals(mm::AllocSites::Totals* totals)
{
    mm::AllocSites::Totals sum = {0, 0, 0};
    memsites::collect(totals);
    for (size_t i = 0; i < mm::AllocSites::SITES; i++)
    {
        if (totals[i].blocks == 0)
            continue;
        ulong frames[mm::AllocSites::FRAMES];
        const size_t n = mm::AllocSites::get_instance().frames_of(i, frames);
        char name[SymbolTable::DESCRIBE_MAX];
        if (n == 0 || !SymbolTable::get_instance().describe_return(frames[memsites::own_position(frames, n)],
            name, sizeof(name)))
            continue;
        if (stdlib::str_str(name, "memsites_probe") == nullptr)
            continue;
        sum.blocks += totals[i].blocks;
        sum.asked += totals[i].asked;
        sum.taken += totals[i].taken;
    }
    return sum;
}

/* memsites, the allocation-site tracker (mm/alloc_sites.h): blocks Rust
   allocates from a function of a known name, through Vec's growth, Rust's
   allocator shims and the kernel's GlobalAlloc, must be that function's
   site's when read off the heap -- the pools' blocks by their tags, the
   page runs by the big blocks' table -- every one of them and what each was
   asked for; none once freed; and none given a site while it is off. */
stdlib::Error test_alloc_sites()
{
    static const ulong TAG = 'TSit';
    /* A Vec's own three words: the vector of the probe's blocks holds one a
       block */
    static const ulong VEC_BYTES = 3 * sizeof(ulong);
    struct Case
    {
        ulong count;
        ulong size;
        const char* what;
    };
    static const Case CASES[] = {
        {5, 200, "pool blocks"},
        {3, 3 * consts::PAGE_SIZE, "page runs"},
    };

    auto& sites = mm::AllocSites::get_instance();
    const bool was_on = sites.enabled();
    auto* totals = static_cast<mm::AllocSites::Totals*>(
        mm::alloc(mm::AllocSites::SITES * sizeof(mm::AllocSites::Totals), TAG));
    if (totals == nullptr || !sites.enable())
    {
        if (totals != nullptr)
            mm::free(totals);
        klog(0, "test_alloc_sites: no memory for the tables");
        return make_error(stdlib::Error::NO_MEMORY);
    }

    bool ok = true;
    for (const Case& c : CASES)
    {
        if (!rust_memsites_probe(c.count, c.size))
        {
            klog(0, "test_alloc_sites: %s: the probe got no memory", c.what);
            ok = false;
            break;
        }
        /* Its blocks, and the vector holding them */
        const ulong want_blocks = c.count + 1;
        const ulong want_asked = c.count * c.size + c.count * VEC_BYTES;
        mm::AllocSites::Totals got = probe_totals(totals);
        if (got.blocks != want_blocks || got.asked != want_asked || got.taken < got.asked)
        {
            klog(0, "test_alloc_sites: %s: the probe's site holds %lu blocks of %lu bytes (taking %lu), not %lu of %lu",
                c.what, got.blocks, got.asked, got.taken, want_blocks, want_asked);
            ok = false;
        }
        rust_memsites_release();
        got = probe_totals(totals);
        if (got.blocks != 0)
        {
            klog(0, "test_alloc_sites: %s: %lu blocks still the probe's once freed", c.what, got.blocks);
            ok = false;
        }
    }

    /* Off: what is allocated then is nobody's site */
    sites.disable();
    if (ok && rust_memsites_probe(CASES[0].count, CASES[0].size))
    {
        const mm::AllocSites::Totals got = probe_totals(totals);
        rust_memsites_release();
        if (got.blocks != 0)
        {
            klog(0, "test_alloc_sites: off, and %lu blocks given the probe's site", got.blocks);
            ok = false;
        }
    }
    if (was_on)
        sites.enable();
    mm::free(totals);

    if (!ok)
        return make_error(stdlib::Error::UNSUCCESSFUL);
    klog(0, "test_alloc_sites: complete");
    return make_success();
}

stdlib::Error test_ring_buffer()
{
    klog(0, "test_ring_buffer: started");

    stdlib::RingBuffer<u8, 3> rb;

    if (!rb.put(0x1))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (!rb.put(0x2))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (!rb.put(0x3))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (rb.put(0x4))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (!rb.is_full())
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (rb.is_empty())
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (rb.get() != 0x1)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (rb.get() != 0x2)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (rb.get() != 0x3)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (!rb.is_empty())
        return make_error(stdlib::Error::UNSUCCESSFUL);
   
    return make_error(stdlib::Error::SUCCESS);
}

/* noinline helpers to create a known call-depth difference */
static __attribute__((noinline)) size_t capture_at_depth0()
{
    ulong frames[20];
    return StackTrace::capture(frames, stdlib::array_size(frames));
}

static __attribute__((noinline)) size_t capture_at_depth1()
{
    return capture_at_depth0();
}

static __attribute__((noinline)) size_t capture_at_depth2()
{
    return capture_at_depth1();
}

static __attribute__((noinline)) size_t capture_at_depth3()
{
    return capture_at_depth2();
}

stdlib::Error test_stack_trace()
{
    klog(0, "test_stack_trace: started");

    ulong frames[20];
    size_t count;

    /* Auto-detect: must capture at least 1 frame */
    count = StackTrace::capture(frames, stdlib::array_size(frames));
    if (count == 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* All return addresses must be in kernel space */
    for (size_t i = 0; i < count; i++)
    {
        if (frames[i] < 0xFFFF800001000000UL)
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* max_frames = 0 must return 0 */
    count = StackTrace::capture(frames, 0);
    if (count != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* max_frames = 1 must return exactly 1 */
    count = StackTrace::capture(frames, 1);
    if (count != 1)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Explicit bounds: empty range (base == limit) must return 0 */
    count = StackTrace::capture(0, 0, frames, stdlib::array_size(frames));
    if (count != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Explicit bounds: range not containing our stack must return 0 */
    count = StackTrace::capture(0x1000, 0x2000, frames, stdlib::array_size(frames));
    if (count != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Explicit bounds with correct task stack range */
    ulong rsp = hal::get_sp();
    ulong base = rsp & (~(Task::STACK_SIZE - 1));
    Task::Stack* stack_ptr = reinterpret_cast<Task::Stack*>(base);
    if (stack_ptr->magic1 == Task::STACK_MAGIC1 &&
        stack_ptr->magic2 == Task::STACK_MAGIC2)
    {
        count = StackTrace::capture((ulong)&stack_ptr->stack_bottom[0], (ulong)&stack_ptr->stack_top[0],
                                    frames, stdlib::array_size(frames));
        if (count == 0)
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Deeper nesting must capture more frames:
       capture_at_depth0 adds 1 extra function on the chain,
       capture_at_depth3 adds 4.  Difference must be exactly 3. */
    size_t shallow = capture_at_depth0();
    size_t deep = capture_at_depth3();
    if (deep < shallow + 3)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Consecutive captures at the same depth must match */
    size_t count1 = StackTrace::capture(frames, stdlib::array_size(frames));
    ulong frames2[20];
    size_t count2 = StackTrace::capture(frames2, stdlib::array_size(frames2));
    if (count1 != count2)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Return addresses from consecutive captures must be identical
       (same call chain, same function, same callers above) */
    for (size_t i = 0; i < count1; i++)
    {
        /* frame[0] is the return site inside this function; it
           differs between the two calls.  Compare from frame[1] up. */
        if (i == 0)
            continue;
        if (frames[i] != frames2[i])
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    klog(0, "test_stack_trace: complete");
    return make_success();
}

stdlib::Error test_parse_ulong()
{
    klog(0, "test_parse_ulong: started");

    ulong val;

    /* Basic numbers */
    if (!stdlib::parse_ulong("0", val) || val != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (!stdlib::parse_ulong("1", val) || val != 1)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (!stdlib::parse_ulong("12345", val) || val != 12345)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (!stdlib::parse_ulong("999999", val) || val != 999999)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Invalid inputs must fail */
    if (stdlib::parse_ulong("", val))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (stdlib::parse_ulong(nullptr, val))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (stdlib::parse_ulong("abc", val))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (stdlib::parse_ulong("12x4", val))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    if (stdlib::parse_ulong(" 5", val))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_parse_ulong: complete");
    return make_success();
}

/* Nothing on the command line stops the boot: it is read before the
   netconsole, the disk log or the NIC exist, and on x86 a line that failed
   to parse used to be a panic. A value may carry an '=' of its own, what
   cannot be parsed is skipped and the rest still read, and a line too long
   for the buffer is read up to its last whole parameter. Each case parses
   into its own instance, never the one the kernel booted with. */
stdlib::Error test_parameters()
{
    klog(0, "test_parameters: started");

    Parameters label;
    label.parse("dhcp=auto root=LABEL=nosenv");
    if (label.get_root().mode != Parameters::RootLabel ||
        stdlib::str_cmp(label.get_root().value, "nosenv") != 0 ||
        !label.is_dhcp_auto())
        return make_error(stdlib::Error::UNSUCCESSFUL);

    Parameters uuid;
    uuid.parse("root=UUID=f8740651-e0b5-4ae3-84f1-a55d1ab00139 ro");
    if (uuid.get_root().mode != Parameters::RootUuid ||
        uuid.get_root().uuid[0] != 0xf8 || uuid.get_root().uuid[15] != 0x39 ||
        !uuid.is_root_read_only())
        return make_error(stdlib::Error::UNSUCCESSFUL);

    Parameters device;
    device.parse("root=nvme11");
    if (device.get_root().mode != Parameters::RootDevice ||
        stdlib::str_cmp(device.get_root().value, "nvme11") != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Skipped: no key, no value, a bare word other than ro, one character,
       and a parameter longer than any taken */
    const char* const skipped[] = {
        "=nosenv",
        "root=",
        "quiet",
        "x",
        "root=LABEL=a-label-longer-than-any-parameter-taken",
    };
    for (ulong i = 0; i < stdlib::array_size(skipped); i++)
    {
        Parameters one;
        one.parse(skipped[i]);
        if (one.get_root().mode != Parameters::RootNone || one.is_root_read_only())
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* ... and the rest of the line still read around them */
    Parameters rest;
    rest.parse("quiet root=nvme11 =nosenv dhcp=auto x");
    if (rest.get_root().mode != Parameters::RootDevice ||
        stdlib::str_cmp(rest.get_root().value, "nvme11") != 0 ||
        !rest.is_dhcp_auto())
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Too long for the buffer, padded so the root parameter starts inside it
       and ends past it: that one is dropped rather than read as a shorter
       one (a root=LABEL=nos), and everything before it is taken */
    const char* filler = " dns=on";
    const char* straddler = " root=LABEL=nosenv";
    char line[Parameters::CMDLINE_LEN * 2];
    stdlib::strn_cpy(line, "dhcp=auto", sizeof(line));
    ulong line_len = stdlib::str_len(line);
    while (line_len < Parameters::CMDLINE_LEN - stdlib::str_len(straddler))
    {
        stdlib::strn_cpy(line + line_len, filler, sizeof(line) - line_len);
        line_len += stdlib::str_len(filler);
    }
    stdlib::strn_cpy(line + line_len, straddler, sizeof(line) - line_len);

    Parameters long_line;
    long_line.parse(line);
    if (long_line.get_root().mode != Parameters::RootNone ||
        !long_line.is_dhcp_auto() || !long_line.is_dns_enabled())
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_parameters: complete");
    return make_success();
}

stdlib::Error test_hex_char_to_nibble()
{
    klog(0, "test_hex_char_to_nibble: started");

    /* Decimal digits */
    for (char c = '0'; c <= '9'; c++)
    {
        if (stdlib::hex_char_to_nibble(c) != (u8)(c - '0'))
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Lowercase hex */
    for (char c = 'a'; c <= 'f'; c++)
    {
        if (stdlib::hex_char_to_nibble(c) != (u8)(c - 'a' + 10))
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Uppercase hex */
    for (char c = 'A'; c <= 'F'; c++)
    {
        if (stdlib::hex_char_to_nibble(c) != (u8)(c - 'A' + 10))
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Invalid chars */
    if (stdlib::hex_char_to_nibble('g') != 0xFF)
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (stdlib::hex_char_to_nibble(' ') != 0xFF)
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (stdlib::hex_char_to_nibble('\0') != 0xFF)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_hex_char_to_nibble: complete");
    return make_success();
}

stdlib::Error test_hex_decode()
{
    klog(0, "test_hex_decode: started");

    u8 out[16];
    ulong written;

    /* Simple decode */
    if (!stdlib::hex_decode("48656C6C6F", 10, out, sizeof(out), written))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (written != 5)
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (out[0] != 0x48 || out[1] != 0x65 || out[2] != 0x6C ||
        out[3] != 0x6C || out[4] != 0x6F)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* All zeros */
    if (!stdlib::hex_decode("0000", 4, out, sizeof(out), written))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (written != 2 || out[0] != 0 || out[1] != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* All FF */
    if (!stdlib::hex_decode("FFFF", 4, out, sizeof(out), written))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (written != 2 || out[0] != 0xFF || out[1] != 0xFF)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Lowercase */
    if (!stdlib::hex_decode("deadbeef", 8, out, sizeof(out), written))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (written != 4 || out[0] != 0xDE || out[1] != 0xAD ||
        out[2] != 0xBE || out[3] != 0xEF)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Invalid hex must fail */
    if (stdlib::hex_decode("ZZZZ", 4, out, sizeof(out), written))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Empty string */
    if (!stdlib::hex_decode("", 0, out, sizeof(out), written))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (written != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Odd length: only complete pairs decoded */
    if (!stdlib::hex_decode("ABC", 3, out, sizeof(out), written))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (written != 1 || out[0] != 0xAB)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Output buffer limit */
    if (!stdlib::hex_decode("0102030405", 10, out, 2, written))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (written != 2)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_hex_decode: complete");
    return make_success();
}

stdlib::Error test_next_token()
{
    klog(0, "test_next_token: started");

    const char* end;
    const char* tok;

    /* Single token */
    tok = stdlib::next_token("hello", end);
    if (!tok || stdlib::strn_cmp(tok, "hello", 5) != 0 || (end - tok) != 5)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Leading spaces */
    tok = stdlib::next_token("   abc", end);
    if (!tok || stdlib::strn_cmp(tok, "abc", 3) != 0 || (end - tok) != 3)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Two tokens */
    tok = stdlib::next_token("one two", end);
    if (!tok || stdlib::strn_cmp(tok, "one", 3) != 0 || (end - tok) != 3)
        return make_error(stdlib::Error::UNSUCCESSFUL);
    tok = stdlib::next_token(end, end);
    if (!tok || stdlib::strn_cmp(tok, "two", 3) != 0 || (end - tok) != 3)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* No more tokens */
    tok = stdlib::next_token(end, end);
    if (tok != nullptr)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Empty string */
    tok = stdlib::next_token("", end);
    if (tok != nullptr)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Only spaces */
    tok = stdlib::next_token("   ", end);
    if (tok != nullptr)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* token_copy */
    const char* input = "diskread vda 42";
    tok = stdlib::next_token(input, end);
    char buf[16];
    stdlib::token_copy(tok, end, buf, sizeof(buf));
    if (stdlib::str_cmp(buf, "diskread") != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    tok = stdlib::next_token(end, end);
    stdlib::token_copy(tok, end, buf, sizeof(buf));
    if (stdlib::str_cmp(buf, "vda") != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    tok = stdlib::next_token(end, end);
    stdlib::token_copy(tok, end, buf, sizeof(buf));
    if (stdlib::str_cmp(buf, "42") != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* token_copy truncation */
    char tiny[4];
    const char* long_tok = "longtoken";
    stdlib::token_copy(long_tok, long_tok + 9, tiny, sizeof(tiny));
    if (stdlib::str_cmp(tiny, "lon") != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_next_token: complete");
    return make_success();
}

/* The block layer's table (src/crates/layers/block/src/selftest.rs). The devices are
   whatever booted, so what is checked is that the table agrees with itself:
   a name nothing carries finds nothing, and every device it counts answers
   to its own name with the handle the walk gave. */
extern "C" int rust_block_selftest();

static stdlib::Error test_block_device_table()
{
    klog(0, "test_block_device_table: started");

    if (rust_block_selftest() != 0)
    {
        klog(0, "test_block_device_table: failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    klog(0, "test_block_device_table: complete");
    return make_success();
}

stdlib::Error test_contiguous_pages()
{
    auto& pt = mm::PageTable::get_instance();

    klog(0, "test_contiguous_pages: started");

    /* count=0 must fail */
    if (pt.alloc_contiguous_pages(0) != nullptr)
    {
        klog(0, "test_contiguous_pages: count=0 should fail");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Alloc 1 page */
    ulong free_before = pt.get_free_pages_count();
    klog(0, "test_contiguous_pages: free pages before %lu", free_before);

    mm::Page* p1 = pt.alloc_contiguous_pages(1);
    if (!p1)
    {
        klog(0, "test_contiguous_pages: alloc 1 page failed");
        return make_error(stdlib::Error::NO_MEMORY);
    }

    if (pt.get_free_pages_count() != free_before - 1)
    {
        klog(0, "test_contiguous_pages: free count mismatch after alloc 1");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Write and read back via tmp_map (pages are not identity-mapped) */
    ulong va1 = pt.tmp_map_page(p1->get_phy_address());
    if (!va1)
    {
        klog(0, "test_contiguous_pages: tmp_map_page failed");
        pt.free_page(p1);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }
    stdlib::mem_set((void*)va1, 0xAB, consts::PAGE_SIZE);
    u8* ptr1 = (u8*)va1;
    for (ulong i = 0; i < consts::PAGE_SIZE; i++)
    {
        if (ptr1[i] != 0xAB)
        {
            klog(0, "test_contiguous_pages: pattern mismatch at %lu", i);
            pt.tmp_unmap_page(va1);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }
    pt.tmp_unmap_page(va1);

    pt.free_page(p1);
    if (pt.get_free_pages_count() != free_before)
    {
        klog(0, "test_contiguous_pages: free count mismatch after free 1");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Alloc 4 contiguous pages */
    ulong free_before4 = pt.get_free_pages_count();
    mm::Page* p4 = pt.alloc_contiguous_pages(4);
    if (!p4)
    {
        klog(0, "test_contiguous_pages: alloc 4 pages failed");
        return make_error(stdlib::Error::NO_MEMORY);
    }

    if (pt.get_free_pages_count() != free_before4 - 4)
    {
        klog(0, "test_contiguous_pages: free count mismatch after alloc 4");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Verify physical contiguity */
    ulong base_phys = p4->get_phy_address();
    klog(0, "test_contiguous_pages: 4 pages at phys 0x%lX", base_phys);
    mm::Page* page_array = p4;
    for (ulong j = 0; j < 4; j++)
    {
        if (page_array[j].get_phy_address() != base_phys + j * consts::PAGE_SIZE)
        {
            klog(0, "test_contiguous_pages: page %lu phys 0x%lX expected 0x%lX",
                j, page_array[j].get_phy_address(), base_phys + j * consts::PAGE_SIZE);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Write unique patterns to each page and verify via tmp_map */
    for (ulong j = 0; j < 4; j++)
    {
        ulong va = pt.tmp_map_page(page_array[j].get_phy_address());
        BUG_ON(!va);
        stdlib::mem_set((void*)va, (u8)(0xC0 + j), consts::PAGE_SIZE);
        pt.tmp_unmap_page(va);
    }

    for (ulong j = 0; j < 4; j++)
    {
        ulong va = pt.tmp_map_page(page_array[j].get_phy_address());
        BUG_ON(!va);
        u8* ptr = (u8*)va;
        u8 expected = (u8)(0xC0 + j);
        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (ptr[i] != expected)
            {
                klog(0, "test_contiguous_pages: page %lu byte %lu: 0x%lX expected 0x%lX",
                    j, i, (ulong)ptr[i], (ulong)expected);
                pt.tmp_unmap_page(va);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
        pt.tmp_unmap_page(va);
    }

    /* Free all 4 pages */
    for (ulong j = 0; j < 4; j++)
    {
        pt.free_page(&page_array[j]);
    }

    if (pt.get_free_pages_count() != free_before4)
    {
        klog(0, "test_contiguous_pages: free count mismatch after free 4: %lu expected %lu",
            pt.get_free_pages_count(), free_before4);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Alloc and free repeatedly to check stability */
    for (ulong round = 0; round < 3; round++)
    {
        ulong free_now = pt.get_free_pages_count();
        mm::Page* pages = pt.alloc_contiguous_pages(2);
        if (!pages)
        {
            klog(0, "test_contiguous_pages: round %lu alloc failed", round);
            return make_error(stdlib::Error::NO_MEMORY);
        }

        if (pages[1].get_phy_address() != pages[0].get_phy_address() + consts::PAGE_SIZE)
        {
            klog(0, "test_contiguous_pages: round %lu not contiguous", round);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        pt.free_page(&pages[0]);
        pt.free_page(&pages[1]);

        if (pt.get_free_pages_count() != free_now)
        {
            klog(0, "test_contiguous_pages: round %lu free count mismatch", round);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    klog(0, "test_contiguous_pages: complete");
    return make_success();
}

/* The zeroing of free pages (PageTable::zero_free_page and its huge pool's
   twin), before the tasks that do it exist (PageZeroer): pages zeroed onto
   the zeroed list and counted there, still counted free; a guest's
   allocation (alloc_page_keeping) taking them before any other, each read
   as zero by zerofree=verify's check; a page written to, freed, and
   allocated again by the kernel's own path (alloc_page) back without a
   byte of what was written; and the counts where they began. With a pool,
   a block zeroed page by page into the known-zero ones, handed out as it
   is and read, and back. */
stdlib::Error test_zero_free()
{
    auto& pt = mm::PageTable::get_instance();
    static const ulong PAGES = 16;
    static const u8 PATTERN = 0x5A;
    const bool check_was = pt.is_zero_check();
    const ulong reserve = pt.get_kernel_reserve_pages();
    const ulong free0 = pt.get_free_pages_count();
    const ulong zeroed0 = pt.get_zeroed_pages_count();
    mm::Page* pages[PAGES] = {};
    stdlib::Error err = make_success();

    klog(0, "test_zero_free: started, %lu free, %lu of them zeroed", free0, zeroed0);
    if (free0 <= reserve + PAGES)
    {
        klog(0, "test_zero_free: too little free to test (%lu, the reserve %lu)", free0, reserve);
        return make_error(stdlib::Error::NO_MEMORY);
    }

    for (ulong i = 0; i < PAGES; i++)
    {
        if (!pt.zero_free_page(reserve))
        {
            klog(0, "test_zero_free: zero_free_page %lu found nothing to zero", i);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }
    if (pt.get_zeroed_pages_count() != zeroed0 + PAGES || pt.get_free_pages_count() != free0)
    {
        klog(0, "test_zero_free: after zeroing %lu: %lu zeroed (want %lu), %lu free (want %lu)", PAGES,
            pt.get_zeroed_pages_count(), zeroed0 + PAGES, pt.get_free_pages_count(), free0);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* A guest's allocations take the zeroed ones first, and the check reads
       each: a word not zero is a panic */
    pt.set_zero_check(true);
    for (ulong i = 0; i < PAGES; i++)
    {
        pages[i] = pt.alloc_page_keeping(reserve);
        if (pages[i] == nullptr)
        {
            klog(0, "test_zero_free: alloc_page_keeping %lu failed", i);
            err = make_error(stdlib::Error::NO_MEMORY);
            break;
        }
    }
    pt.set_zero_check(check_was);
    if (err.ok() && pt.get_zeroed_pages_count() != zeroed0)
    {
        klog(0, "test_zero_free: the guest's allocations left %lu zeroed, not %lu: not taken from the zeroed list",
            pt.get_zeroed_pages_count(), zeroed0);
        err = make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Written to, freed -- dirty again, on the other list -- and taken back
       by the kernel's own allocation, which prefers those: zeroed again */
    for (ulong i = 0; err.ok() && i < PAGES; i++)
    {
        ulong va = pt.tmp_map_page(pages[i]->get_phy_address());
        if (va == 0)
        {
            klog(0, "test_zero_free: tmp_map_page failed");
            err = make_error(stdlib::Error::UNSUCCESSFUL);
            break;
        }
        stdlib::mem_set((void*)va, PATTERN, consts::PAGE_SIZE);
        pt.tmp_unmap_page(va);
    }
    for (ulong i = 0; i < PAGES; i++)
    {
        if (pages[i] != nullptr)
            pt.free_page(pages[i]);
        pages[i] = nullptr;
    }
    if (!err.ok())
        return err;
    if (pt.get_free_pages_count() != free0 || pt.get_zeroed_pages_count() != zeroed0)
    {
        klog(0, "test_zero_free: after the frees %lu free (want %lu), %lu zeroed (want %lu)",
            pt.get_free_pages_count(), free0, pt.get_zeroed_pages_count(), zeroed0);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    for (ulong i = 0; err.ok() && i < PAGES; i++)
    {
        pages[i] = pt.alloc_page();
        if (pages[i] == nullptr)
        {
            klog(0, "test_zero_free: alloc_page %lu failed", i);
            err = make_error(stdlib::Error::NO_MEMORY);
            break;
        }
        ulong va = pt.tmp_map_page(pages[i]->get_phy_address());
        if (va == 0)
        {
            klog(0, "test_zero_free: tmp_map_page failed");
            err = make_error(stdlib::Error::UNSUCCESSFUL);
            break;
        }
        const u8* bytes = (const u8*)va;
        for (ulong b = 0; b < consts::PAGE_SIZE; b++)
        {
            if (bytes[b] != 0)
            {
                klog(0, "test_zero_free: page 0x%lX handed out again with 0x%x at %lu",
                    pages[i]->get_phy_address(), (unsigned int)bytes[b], b);
                err = make_error(stdlib::Error::UNSUCCESSFUL);
                break;
            }
        }
        pt.tmp_unmap_page(va);
    }
    for (ulong i = 0; i < PAGES; i++)
    {
        if (pages[i] != nullptr)
            pt.free_page(pages[i]);
    }
    if (!err.ok())
        return err;
    if (pt.get_free_pages_count() != free0)
    {
        klog(0, "test_zero_free: %lu free at the end, not %lu", pt.get_free_pages_count(), free0);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* The huge pool, when there is one. First a block taken from under the
       zeroing: a block written all over and given back, the zeroing a
       hundred pages into it -- it starts on the block the last allocation
       took, the lowest free one of its word, while no block is known zero
       -- and the next allocation, with none known zero, takes that block,
       its first hundred pages read under the check and the rest zeroed: all
       of it zeros. Then a block's worth of the zeroing makes one block known
       zero -- none is, before the tasks start -- which the next allocation
       takes as it is, read under the check. */
    if (pt.get_huge_pool_blocks() != 0 && pt.get_huge_pool_free() != 0 && pt.get_huge_pool_zeroed() == 0)
    {
        static const ulong PART = 100;
        const ulong dirty = pt.alloc_huge_frame();
        if (dirty == 0)
        {
            klog(0, "test_zero_free: no block of the pool handed out with %lu free", pt.get_huge_pool_free());
            return make_error(stdlib::Error::NO_MEMORY);
        }
        for (ulong i = 0; i < mm::PageTable::HUGE_FRAME_PAGES; i++)
        {
            ulong va = pt.tmp_map_page(dirty + i * consts::PAGE_SIZE);
            if (va == 0)
            {
                klog(0, "test_zero_free: tmp_map_page failed");
                pt.free_huge_frame(dirty);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
            stdlib::mem_set((void*)va, PATTERN, consts::PAGE_SIZE);
            pt.tmp_unmap_page(va);
        }
        pt.free_huge_frame(dirty);
        for (ulong i = 0; i < PART; i++)
        {
            if (!pt.zero_free_huge_page())
            {
                klog(0, "test_zero_free: zero_free_huge_page %lu found nothing to zero", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
        pt.set_zero_check(true);
        const ulong taken = pt.alloc_huge_frame();
        pt.set_zero_check(check_was);
        if (taken != dirty)
        {
            klog(0, "test_zero_free: the allocation took 0x%lX, not 0x%lX, the block the zeroing was part way through",
                taken, dirty);
            if (taken != 0)
                pt.free_huge_frame(taken);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        for (ulong i = 0; err.ok() && i < mm::PageTable::HUGE_FRAME_PAGES; i++)
        {
            ulong va = pt.tmp_map_page(taken + i * consts::PAGE_SIZE);
            if (va == 0)
            {
                klog(0, "test_zero_free: tmp_map_page failed");
                err = make_error(stdlib::Error::UNSUCCESSFUL);
                break;
            }
            const ulong* words = (const ulong*)va;
            for (ulong w = 0; w < consts::PAGE_SIZE / sizeof(ulong); w++)
            {
                if (words[w] != 0)
                {
                    klog(0, "test_zero_free: page %lu of a block taken from under the zeroing holds 0x%lX at word %lu",
                        i, words[w], w);
                    err = make_error(stdlib::Error::UNSUCCESSFUL);
                    break;
                }
            }
            pt.tmp_unmap_page(va);
        }
        pt.free_huge_frame(taken);
        if (!err.ok())
            return err;

        const ulong huge_zeroed0 = pt.get_huge_pool_zeroed();
        for (ulong i = 0; i < mm::PageTable::HUGE_FRAME_PAGES; i++)
        {
            if (!pt.zero_free_huge_page())
            {
                klog(0, "test_zero_free: zero_free_huge_page %lu found nothing to zero", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
        if (pt.get_huge_pool_zeroed() != huge_zeroed0 + 1)
        {
            klog(0, "test_zero_free: %lu blocks known zero after a block's worth, not %lu",
                pt.get_huge_pool_zeroed(), huge_zeroed0 + 1);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        pt.set_zero_check(true);
        ulong block = pt.alloc_huge_frame();
        pt.set_zero_check(check_was);
        if (block == 0 || pt.get_huge_pool_zeroed() != huge_zeroed0)
        {
            klog(0, "test_zero_free: the block known zero was not the one handed out (0x%lX, %lu known zero)",
                block, pt.get_huge_pool_zeroed());
            if (block != 0)
                pt.free_huge_frame(block);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        pt.free_huge_frame(block);
    }

    klog(0, "test_zero_free: complete");
    return make_success();
}

stdlib::Error test_page_allocator()
{
    klog(0, "test_page_allocator: started");

    auto& pt = mm::PageTable::get_instance();

    /* Test alloc_map_pages / unmap_free_pages with 1 page */
    {
        ulong phys_addr = 0;
        void* ptr = mm::alloc_map_pages(1, &phys_addr);
        if (!ptr)
        {
            klog(0, "test_page_allocator: alloc_map_pages(1) failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        if (phys_addr == 0)
        {
            klog(0, "test_page_allocator: alloc_map_pages(1) phys_addr is 0");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        stdlib::mem_set(ptr, 0xAA, consts::PAGE_SIZE);
        u8* p = (u8*)ptr;
        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (p[i] != 0xAA)
            {
                klog(0, "test_page_allocator: alloc_map_pages(1) mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }

        mm::unmap_free_pages(ptr);
    }

    /* Test alloc_map_pages / unmap_free_pages with 2 pages */
    {
        ulong phys_addr = 0;
        void* ptr = mm::alloc_map_pages(2, &phys_addr);
        if (!ptr)
        {
            klog(0, "test_page_allocator: alloc_map_pages(2) failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        if (phys_addr == 0)
        {
            klog(0, "test_page_allocator: alloc_map_pages(2) phys_addr is 0");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        u8* p = (u8*)ptr;
        stdlib::mem_set(p, 0xBB, consts::PAGE_SIZE);
        stdlib::mem_set(p + consts::PAGE_SIZE, 0xCC, consts::PAGE_SIZE);

        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (p[i] != 0xBB)
            {
                klog(0, "test_page_allocator: alloc_map_pages(2) page0 mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (p[consts::PAGE_SIZE + i] != 0xCC)
            {
                klog(0, "test_page_allocator: alloc_map_pages(2) page1 mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }

        mm::unmap_free_pages(ptr);
    }

    /* Test map_pages / unmap_pages with pre-allocated physical pages */
    {
        ulong free_pages_before = pt.get_free_pages_count();

        mm::Page* pages = pt.alloc_contiguous_pages(2);
        if (!pages)
        {
            klog(0, "test_page_allocator: alloc_contiguous_pages(2) failed");
            return make_error(stdlib::Error::NO_MEMORY);
        }

        ulong phys_addrs[2];
        phys_addrs[0] = pages[0].get_phy_address();
        phys_addrs[1] = pages[1].get_phy_address();

        void* ptr = mm::map_pages(2, phys_addrs);
        if (!ptr)
        {
            klog(0, "test_page_allocator: map_pages(2) failed");
            pt.free_page(&pages[0]);
            pt.free_page(&pages[1]);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        /* Write via mapped VA */
        u8* p = (u8*)ptr;
        stdlib::mem_set(p, 0xDD, consts::PAGE_SIZE);
        stdlib::mem_set(p + consts::PAGE_SIZE, 0xEE, consts::PAGE_SIZE);

        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (p[i] != 0xDD)
            {
                klog(0, "test_page_allocator: map_pages page0 mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (p[consts::PAGE_SIZE + i] != 0xEE)
            {
                klog(0, "test_page_allocator: map_pages page1 mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }

        /* unmap_pages - releases VA but does NOT free physical pages */
        mm::unmap_pages(ptr, 2);

        /* Verify physical pages still contain written data via tmp_map */
        ulong va0 = pt.tmp_map_page(phys_addrs[0]);
        if (!va0)
        {
            klog(0, "test_page_allocator: tmp_map_page page0 after unmap_pages failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        u8* t0 = (u8*)va0;
        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (t0[i] != 0xDD)
            {
                klog(0, "test_page_allocator: post-unmap page0 mismatch at %lu", i);
                pt.tmp_unmap_page(va0);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
        pt.tmp_unmap_page(va0);

        ulong va1 = pt.tmp_map_page(phys_addrs[1]);
        if (!va1)
        {
            klog(0, "test_page_allocator: tmp_map_page page1 after unmap_pages failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        u8* t1 = (u8*)va1;
        for (ulong i = 0; i < consts::PAGE_SIZE; i++)
        {
            if (t1[i] != 0xEE)
            {
                klog(0, "test_page_allocator: post-unmap page1 mismatch at %lu", i);
                pt.tmp_unmap_page(va1);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
        pt.tmp_unmap_page(va1);

        /* Now free the physical pages */
        pt.free_page(&pages[0]);
        pt.free_page(&pages[1]);

        if (pt.get_free_pages_count() != free_pages_before)
        {
            klog(0, "test_page_allocator: free count mismatch: %lu expected %lu",
                pt.get_free_pages_count(), free_pages_before);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Repeated alloc/map/unmap/free cycles for stability */
    {
        for (ulong round = 0; round < 5; round++)
        {
            ulong phys_addr = 0;
            void* ptr = mm::alloc_map_pages(1, &phys_addr);
            if (!ptr)
            {
                klog(0, "test_page_allocator: round %lu alloc_map_pages failed", round);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }

            u8* p = (u8*)ptr;
            stdlib::mem_set(p, (u8)(0x10 + round), consts::PAGE_SIZE);
            if (p[0] != (u8)(0x10 + round))
            {
                klog(0, "test_page_allocator: round %lu pattern mismatch", round);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }

            mm::unmap_free_pages(ptr);
        }
    }

    /* Repeated map_pages / unmap_pages cycles */
    {
        for (ulong round = 0; round < 5; round++)
        {
            mm::Page* page = pt.alloc_contiguous_pages(1);
            if (!page)
            {
                klog(0, "test_page_allocator: map_pages round %lu alloc failed", round);
                return make_error(stdlib::Error::NO_MEMORY);
            }

            ulong pa = page->get_phy_address();
            void* ptr = mm::map_pages(1, &pa);
            if (!ptr)
            {
                klog(0, "test_page_allocator: map_pages round %lu map failed", round);
                pt.free_page(page);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }

            u8* p = (u8*)ptr;
            stdlib::mem_set(p, (u8)(0x50 + round), consts::PAGE_SIZE);
            if (p[0] != (u8)(0x50 + round))
            {
                klog(0, "test_page_allocator: map_pages round %lu pattern mismatch", round);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }

            mm::unmap_pages(ptr, 1);
            pt.free_page(page);
        }
    }

    /* Test alloc_map_pages / unmap_free_pages with 32 pages (large DMA buffer) */
    {
        /* Pre-warm: the first mapping in this VA range may allocate
           intermediate page table pages (L1/L2/L3) that are never
           reclaimed on unmap.  Do a throwaway cycle so that the page
           table structure is already in place before we snapshot.
           Without it the check fails wherever this mapping is the first
           to touch a fresh L1 -- which is what a UEFI boot does, the
           framebuffer mapping having moved the VA cursor along. */
        {
            ulong warm_phys = 0;
            void* warm_ptr = mm::alloc_map_pages(32, &warm_phys);
            if (warm_ptr)
                mm::unmap_free_pages(warm_ptr);
        }

        ulong free_pages_before = pt.get_free_pages_count();
        ulong phys_addr = 0;
        void* ptr = mm::alloc_map_pages(32, &phys_addr);
        if (!ptr)
        {
            klog(0, "test_page_allocator: alloc_map_pages(32) failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        if (phys_addr == 0)
        {
            klog(0, "test_page_allocator: alloc_map_pages(32) phys_addr is 0");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        u8* p = (u8*)ptr;
        for (ulong pg = 0; pg < 32; pg++)
        {
            stdlib::mem_set(p + pg * consts::PAGE_SIZE, (u8)(0xA0 + pg), consts::PAGE_SIZE);
        }
        for (ulong pg = 0; pg < 32; pg++)
        {
            u8 expected = (u8)(0xA0 + pg);
            u8* base = p + pg * consts::PAGE_SIZE;
            if (base[0] != expected || base[consts::PAGE_SIZE - 1] != expected)
            {
                klog(0, "test_page_allocator: alloc_map_pages(32) page %lu mismatch", pg);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }

        mm::unmap_free_pages(ptr);

        if (pt.get_free_pages_count() != free_pages_before)
        {
            klog(0, "test_page_allocator: alloc_map_pages(32) free count mismatch: %lu expected %lu",
                pt.get_free_pages_count(), free_pages_before);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Test alloc_map_pages / unmap_free_pages with 128 pages (large VirtQueue) */
    {
        /* Pre-warm: the first mapping in this VA range may allocate
           intermediate page table pages (L1/L2/L3) that are never
           reclaimed on unmap.  Do a throwaway cycle so that the page
           table structure is already in place before we snapshot. */
        {
            ulong warm_phys = 0;
            void* warm_ptr = mm::alloc_map_pages(128, &warm_phys);
            if (warm_ptr)
                mm::unmap_free_pages(warm_ptr);
        }

        ulong free_pages_before = pt.get_free_pages_count();
        ulong phys_addr = 0;
        void* ptr = mm::alloc_map_pages(128, &phys_addr);
        if (!ptr)
        {
            klog(0, "test_page_allocator: alloc_map_pages(128) failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        if (phys_addr == 0)
        {
            klog(0, "test_page_allocator: alloc_map_pages(128) phys_addr is 0");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        u8* p = (u8*)ptr;
        for (ulong pg = 0; pg < 128; pg++)
        {
            stdlib::mem_set(p + pg * consts::PAGE_SIZE, (u8)(pg & 0xFF), consts::PAGE_SIZE);
        }
        for (ulong pg = 0; pg < 128; pg++)
        {
            u8 expected = (u8)(pg & 0xFF);
            u8* base = p + pg * consts::PAGE_SIZE;
            if (base[0] != expected || base[consts::PAGE_SIZE - 1] != expected)
            {
                klog(0, "test_page_allocator: alloc_map_pages(128) page %lu mismatch", pg);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }

        mm::unmap_free_pages(ptr);

        if (pt.get_free_pages_count() != free_pages_before)
        {
            klog(0, "test_page_allocator: alloc_map_pages(128) free count mismatch: %lu expected %lu",
                pt.get_free_pages_count(), free_pages_before);
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    klog(0, "test_page_allocator: complete");
    return make_success();
}

stdlib::Error test_mem_set()
{
    klog(0, "test_mem_set: started");

    /* Basic fill */
    {
        u8 buf[128];
        stdlib::mem_set(buf, 0xAA, sizeof(buf));
        for (ulong i = 0; i < sizeof(buf); i++)
        {
            if (buf[i] != 0xAA)
            {
                klog(0, "test_mem_set: basic fill mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
    }

    /* Zero fill */
    {
        u8 buf[64];
        stdlib::mem_set(buf, 0xFF, sizeof(buf));
        stdlib::mem_set(buf, 0, sizeof(buf));
        for (ulong i = 0; i < sizeof(buf); i++)
        {
            if (buf[i] != 0)
            {
                klog(0, "test_mem_set: zero fill mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
    }

    /* Odd sizes (not multiple of 8) */
    {
        for (ulong sz = 0; sz <= 17; sz++)
        {
            u8 buf[32];
            stdlib::mem_set(buf, 0, sizeof(buf));
            stdlib::mem_set(buf, 0xCD, sz);
            for (ulong i = 0; i < sz; i++)
            {
                if (buf[i] != 0xCD)
                {
                    klog(0, "test_mem_set: odd size %lu mismatch at %lu", sz, i);
                    return make_error(stdlib::Error::UNSUCCESSFUL);
                }
            }
            /* Bytes beyond sz must be untouched */
            for (ulong i = sz; i < sizeof(buf); i++)
            {
                if (buf[i] != 0)
                {
                    klog(0, "test_mem_set: odd size %lu overflow at %lu", sz, i);
                    return make_error(stdlib::Error::UNSUCCESSFUL);
                }
            }
        }
    }

    klog(0, "test_mem_set: complete");
    return make_success();
}

stdlib::Error test_mem_cpy()
{
    klog(0, "test_mem_cpy: started");

    /* Basic copy */
    {
        u8 src[128], dst[128];
        for (ulong i = 0; i < sizeof(src); i++)
            src[i] = (u8)(i & 0xFF);
        stdlib::mem_set(dst, 0, sizeof(dst));
        stdlib::mem_cpy(dst, src, sizeof(src));
        for (ulong i = 0; i < sizeof(src); i++)
        {
            if (dst[i] != src[i])
            {
                klog(0, "test_mem_cpy: basic copy mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
    }

    /* Odd sizes (not multiple of 8) */
    {
        for (ulong sz = 0; sz <= 17; sz++)
        {
            u8 src[32], dst[32];
            for (ulong i = 0; i < sizeof(src); i++)
                src[i] = (u8)((i + 0x30) & 0xFF);
            stdlib::mem_set(dst, 0, sizeof(dst));
            stdlib::mem_cpy(dst, src, sz);
            for (ulong i = 0; i < sz; i++)
            {
                if (dst[i] != src[i])
                {
                    klog(0, "test_mem_cpy: odd size %lu mismatch at %lu", sz, i);
                    return make_error(stdlib::Error::UNSUCCESSFUL);
                }
            }
            /* Bytes beyond sz must be untouched */
            for (ulong i = sz; i < sizeof(dst); i++)
            {
                if (dst[i] != 0)
                {
                    klog(0, "test_mem_cpy: odd size %lu overflow at %lu", sz, i);
                    return make_error(stdlib::Error::UNSUCCESSFUL);
                }
            }
        }
    }

    klog(0, "test_mem_cpy: complete");
    return make_success();
}

stdlib::Error test_mem_move()
{
    klog(0, "test_mem_move: started");

    /* Non-overlapping forward */
    {
        u8 src[128], dst[128];
        for (ulong i = 0; i < sizeof(src); i++)
            src[i] = (u8)(i & 0xFF);
        stdlib::mem_set(dst, 0, sizeof(dst));
        stdlib::mem_move(dst, src, sizeof(src));
        for (ulong i = 0; i < sizeof(src); i++)
        {
            if (dst[i] != src[i])
            {
                klog(0, "test_mem_move: non-overlap mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
    }

    /* Overlapping forward: src before dst */
    {
        u8 buf[64];
        for (ulong i = 0; i < sizeof(buf); i++)
            buf[i] = (u8)(i & 0xFF);
        stdlib::mem_move(buf + 8, buf, 32);
        for (ulong i = 0; i < 32; i++)
        {
            if (buf[8 + i] != (u8)(i & 0xFF))
            {
                klog(0, "test_mem_move: overlap fwd mismatch at %lu: 0x%lX expected 0x%lX",
                    i, (ulong)buf[8 + i], (ulong)(u8)(i & 0xFF));
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
    }

    /* Overlapping backward: dst before src */
    {
        u8 buf[64];
        for (ulong i = 0; i < sizeof(buf); i++)
            buf[i] = (u8)(i & 0xFF);
        stdlib::mem_move(buf, buf + 8, 32);
        for (ulong i = 0; i < 32; i++)
        {
            if (buf[i] != (u8)((i + 8) & 0xFF))
            {
                klog(0, "test_mem_move: overlap bwd mismatch at %lu: 0x%lX expected 0x%lX",
                    i, (ulong)buf[i], (ulong)(u8)((i + 8) & 0xFF));
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
    }

    /* dst == src (no-op) */
    {
        u8 buf[32];
        for (ulong i = 0; i < sizeof(buf); i++)
            buf[i] = (u8)(i & 0xFF);
        stdlib::mem_move(buf, buf, sizeof(buf));
        for (ulong i = 0; i < sizeof(buf); i++)
        {
            if (buf[i] != (u8)(i & 0xFF))
            {
                klog(0, "test_mem_move: same ptr mismatch at %lu", i);
                return make_error(stdlib::Error::UNSUCCESSFUL);
            }
        }
    }

    /* Zero size */
    {
        u8 buf[16];
        stdlib::mem_set(buf, 0xAA, sizeof(buf));
        stdlib::mem_move(buf + 4, buf, 0);
        if (buf[4] != 0xAA)
        {
            klog(0, "test_mem_move: zero size modified data");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Odd sizes (not multiple of 8) */
    {
        for (ulong sz = 1; sz <= 17; sz++)
        {
            u8 buf[64];
            for (ulong i = 0; i < sizeof(buf); i++)
                buf[i] = (u8)(i & 0xFF);
            u8 expected[64];
            for (ulong i = 0; i < sizeof(expected); i++)
                expected[i] = (u8)(i & 0xFF);

            stdlib::mem_move(buf + 3, buf, sz);
            for (ulong i = 0; i < sz; i++)
            {
                if (buf[3 + i] != expected[i])
                {
                    klog(0, "test_mem_move: odd sz %lu overlap fwd mismatch at %lu", sz, i);
                    return make_error(stdlib::Error::UNSUCCESSFUL);
                }
            }
        }
    }

    /* Odd sizes backward overlap */
    {
        for (ulong sz = 1; sz <= 17; sz++)
        {
            u8 buf[64];
            for (ulong i = 0; i < sizeof(buf); i++)
                buf[i] = (u8)(i & 0xFF);

            stdlib::mem_move(buf, buf + 3, sz);
            for (ulong i = 0; i < sz; i++)
            {
                if (buf[i] != (u8)((i + 3) & 0xFF))
                {
                    klog(0, "test_mem_move: odd sz %lu overlap bwd mismatch at %lu", sz, i);
                    return make_error(stdlib::Error::UNSUCCESSFUL);
                }
            }
        }
    }

    /* Single byte */
    {
        u8 a = 0x42, b = 0;
        stdlib::mem_move(&b, &a, 1);
        if (b != 0x42)
        {
            klog(0, "test_mem_move: single byte failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    klog(0, "test_mem_move: complete");
    return make_success();
}

stdlib::Error test_mem_cmp()
{
    klog(0, "test_mem_cmp: started");

    /* Equal buffers */
    {
        u8 a[64], b[64];
        stdlib::mem_set(a, 0xAB, sizeof(a));
        stdlib::mem_set(b, 0xAB, sizeof(b));
        if (stdlib::mem_cmp(a, b, sizeof(a)) != 0)
        {
            klog(0, "test_mem_cmp: equal buffers returned non-zero");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* First < second */
    {
        u8 a[] = {1, 2, 3};
        u8 b[] = {1, 2, 4};
        if (stdlib::mem_cmp(a, b, 3) >= 0)
        {
            klog(0, "test_mem_cmp: a < b not detected");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* First > second */
    {
        u8 a[] = {1, 2, 5};
        u8 b[] = {1, 2, 4};
        if (stdlib::mem_cmp(a, b, 3) <= 0)
        {
            klog(0, "test_mem_cmp: a > b not detected");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Difference at first byte */
    {
        u8 a[] = {0x00, 0xFF};
        u8 b[] = {0xFF, 0x00};
        if (stdlib::mem_cmp(a, b, 2) >= 0)
        {
            klog(0, "test_mem_cmp: first byte a < b not detected");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Zero length */
    {
        u8 a[] = {1};
        u8 b[] = {2};
        if (stdlib::mem_cmp(a, b, 0) != 0)
        {
            klog(0, "test_mem_cmp: zero length not equal");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Odd sizes */
    {
        u8 a[17], b[17];
        for (ulong i = 0; i < sizeof(a); i++)
        {
            a[i] = (u8)i;
            b[i] = (u8)i;
        }
        if (stdlib::mem_cmp(a, b, sizeof(a)) != 0)
        {
            klog(0, "test_mem_cmp: odd size equal mismatch");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
        b[16] = 0xFF;
        if (stdlib::mem_cmp(a, b, sizeof(a)) >= 0)
        {
            klog(0, "test_mem_cmp: odd size diff at end not detected");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    klog(0, "test_mem_cmp: complete");
    return make_success();
}

stdlib::Error test_str_len()
{
    klog(0, "test_str_len: started");

    /* Empty string */
    if (stdlib::str_len("") != 0)
    {
        klog(0, "test_str_len: empty string failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Single char */
    if (stdlib::str_len("A") != 1)
    {
        klog(0, "test_str_len: single char failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Known string */
    if (stdlib::str_len("hello") != 5)
    {
        klog(0, "test_str_len: 'hello' failed, got %lu", stdlib::str_len("hello"));
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* String with embedded high bytes */
    {
        char buf[10] = {'\xFF', '\xFE', '\x01', '\0', 'X', '\0'};
        if (stdlib::str_len(buf) != 3)
        {
            klog(0, "test_str_len: high bytes failed, got %lu", stdlib::str_len(buf));
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Longer string (> 8 bytes, tests qword boundary) */
    if (stdlib::str_len("abcdefghijklmnop") != 16)
    {
        klog(0, "test_str_len: 16-char string failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    klog(0, "test_str_len: complete");
    return make_success();
}

stdlib::Error test_str_cmp()
{
    klog(0, "test_str_cmp: started");

    /* Equal strings */
    if (stdlib::str_cmp("abc", "abc") != 0)
    {
        klog(0, "test_str_cmp: equal strings failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Empty strings */
    if (stdlib::str_cmp("", "") != 0)
    {
        klog(0, "test_str_cmp: empty strings failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* First < second */
    if (stdlib::str_cmp("abc", "abd") >= 0)
    {
        klog(0, "test_str_cmp: abc < abd failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* First > second */
    if (stdlib::str_cmp("abd", "abc") <= 0)
    {
        klog(0, "test_str_cmp: abd > abc failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Prefix: shorter < longer */
    if (stdlib::str_cmp("ab", "abc") >= 0)
    {
        klog(0, "test_str_cmp: ab < abc failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Prefix: longer > shorter */
    if (stdlib::str_cmp("abc", "ab") <= 0)
    {
        klog(0, "test_str_cmp: abc > ab failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Single char difference */
    if (stdlib::str_cmp("a", "b") >= 0)
    {
        klog(0, "test_str_cmp: a < b failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Empty vs non-empty */
    if (stdlib::str_cmp("", "a") >= 0)
    {
        klog(0, "test_str_cmp: '' < 'a' failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }
    if (stdlib::str_cmp("a", "") <= 0)
    {
        klog(0, "test_str_cmp: 'a' > '' failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Longer strings (cross qword boundary) */
    if (stdlib::str_cmp("abcdefghij", "abcdefghij") != 0)
    {
        klog(0, "test_str_cmp: long equal failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }
    if (stdlib::str_cmp("abcdefghij", "abcdefghik") >= 0)
    {
        klog(0, "test_str_cmp: long diff at end failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    klog(0, "test_str_cmp: complete");
    return make_success();
}

stdlib::Error test_str_str()
{
    klog(0, "test_str_str: started");

    /* Empty needle always matches at start */
    {
        const char* h = "hello";
        if (stdlib::str_str(h, "") != h)
        {
            klog(0, "test_str_str: empty needle failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Match at beginning */
    {
        const char* h = "abcdef";
        const char* r = stdlib::str_str(h, "abc");
        if (r != h)
        {
            klog(0, "test_str_str: match at start failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Match in middle */
    {
        const char* h = "hello world";
        const char* r = stdlib::str_str(h, "world");
        if (r != h + 6)
        {
            klog(0, "test_str_str: match in middle failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Match at end */
    {
        const char* h = "abcxyz";
        const char* r = stdlib::str_str(h, "xyz");
        if (r != h + 3)
        {
            klog(0, "test_str_str: match at end failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* No match */
    {
        if (stdlib::str_str("abcdef", "xyz") != nullptr)
        {
            klog(0, "test_str_str: no match returned non-null");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Partial match then fail */
    {
        const char* h = "abcabd";
        const char* r = stdlib::str_str(h, "abd");
        if (r != h + 3)
        {
            klog(0, "test_str_str: partial match failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Needle longer than haystack */
    {
        if (stdlib::str_str("ab", "abcdef") != nullptr)
        {
            klog(0, "test_str_str: needle longer returned non-null");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Both empty */
    {
        const char* h = "";
        if (stdlib::str_str(h, "") != h)
        {
            klog(0, "test_str_str: both empty failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Single char match */
    {
        const char* h = "abcdef";
        const char* r = stdlib::str_str(h, "d");
        if (r != h + 3)
        {
            klog(0, "test_str_str: single char match failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* Exact match */
    {
        const char* h = "exact";
        if (stdlib::str_str(h, "exact") != h)
        {
            klog(0, "test_str_str: exact match failed");
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    klog(0, "test_str_str: complete");
    return make_success();
}

/* mem_set, mem_cpy and mem_move held to a byte at a time -- on arm64 the
   assembly of string.S, which every copy the kernel makes goes through, its
   Rust's included; on x86 stdlib_asm.asm's. Every size to two blocks past
   the 64-byte loop, at every alignment of the destination to 16 and four of
   the source; mem_move over every overlap either way; and once at a size
   past two pages. The guard bytes either side of what was written must come
   through untouched. */
namespace
{

const u8 MEM_GUARD_BYTE = 0xA5;
const u8 MEM_SET_BYTE = 0x5C;
const ulong MEM_GUARD = 16;
const ulong MEM_SMALL_MAX = 130;
const ulong MEM_EDGES[] = { 191, 192, 193, 255, 256, 257 };
const ulong MEM_SRC_OFFSETS[] = { 0, 3, 8, 13 };
const ulong MEM_ALIGN = 16;
const ulong MEM_LARGE = 2 * consts::PAGE_SIZE + 77;
const ulong MEM_BUF_SIZE = MEM_LARGE + 2 * MEM_ALIGN + 2 * MEM_GUARD;
const ulong MEM_TAG = 'memt';
/* Copies, and sets, of MEM_LARGE bytes the throughput check times: half a
   megabyte, a quarter of a millisecond at a byte a cycle */
const ulong MEM_RATE_ROUNDS = 64;
const ulong NS_PER_US = 1000;

u8 mem_pattern(ulong i)
{
    return static_cast<u8>(i * 7 + 13);
}

/* What dst holds after [at, at + n) was set from expected(i - at), the rest
   still guard bytes; false, said, otherwise */
bool mem_check(const char* what, const u8* dst, ulong cap, ulong at, ulong n, const u8* from, bool set)
{
    for (ulong i = 0; i < cap; i++)
    {
        u8 expected = MEM_GUARD_BYTE;
        if (i >= at && i < at + n)
            expected = set ? MEM_SET_BYTE : from[i - at];
        if (dst[i] != expected)
        {
            klog(0, "test_mem_routines: %s of %lu at +%lu: byte %lu is 0x%x, not 0x%x", what, n, at, i,
                static_cast<unsigned int>(dst[i]), static_cast<unsigned int>(expected));
            return false;
        }
    }
    return true;
}

bool mem_case(u8* src, u8* dst, ulong n, ulong src_off, ulong dst_off)
{
    const ulong cap = dst_off + n + MEM_GUARD;
    for (ulong i = 0; i < src_off + n; i++)
        src[i] = mem_pattern(i);

    stdlib::mem_set(dst, MEM_GUARD_BYTE, cap);
    stdlib::mem_cpy(dst + dst_off, src + src_off, n);
    if (!mem_check("mem_cpy", dst, cap, dst_off, n, src + src_off, false))
        return false;

    /* mem_move without an overlap is a copy */
    stdlib::mem_set(dst, MEM_GUARD_BYTE, cap);
    stdlib::mem_move(dst + dst_off, src + src_off, n);
    if (!mem_check("mem_move", dst, cap, dst_off, n, src + src_off, false))
        return false;

    /* The guard bytes set a byte at a time, so mem_set is held to the loop and
       not to itself */
    for (ulong i = 0; i < cap; i++)
        dst[i] = MEM_GUARD_BYTE;
    stdlib::mem_set(dst + dst_off, MEM_SET_BYTE, n);
    return mem_check("mem_set", dst, cap, dst_off, n, nullptr, true);
}

/* mem_move of n bytes within buf, from+src to from+dst, against a copy made a
   byte at a time through ref */
bool mem_overlap(u8* buf, u8* ref, ulong cap, ulong src, ulong dst, ulong n)
{
    for (ulong i = 0; i < cap; i++)
        buf[i] = mem_pattern(i);
    for (ulong i = 0; i < n; i++)
        ref[i] = buf[src + i];

    stdlib::mem_move(buf + dst, buf + src, n);
    for (ulong i = 0; i < cap; i++)
    {
        u8 expected = (i >= dst && i < dst + n) ? ref[i - dst] : mem_pattern(i);
        if (buf[i] != expected)
        {
            klog(0, "test_mem_routines: mem_move of %lu from +%lu to +%lu: byte %lu is 0x%x, not 0x%x", n, src, dst,
                i, static_cast<unsigned int>(buf[i]), static_cast<unsigned int>(expected));
            return false;
        }
    }
    return true;
}

bool mem_routines(u8* src, u8* dst, u8* ref)
{
    for (ulong n = 0; n <= MEM_SMALL_MAX + stdlib::array_size(MEM_EDGES); n++)
    {
        ulong size = (n <= MEM_SMALL_MAX) ? n : MEM_EDGES[n - MEM_SMALL_MAX - 1];
        for (ulong dst_off = 0; dst_off < MEM_ALIGN; dst_off++)
        {
            for (ulong src_off : MEM_SRC_OFFSETS)
            {
                if (!mem_case(src, dst, size, src_off, dst_off))
                    return false;
            }
        }
    }

    /* Overlaps: every shift of up to 20 either way, at a base off any
       boundary */
    const ulong base = MEM_GUARD + 2 * MEM_ALIGN + 3;
    const long shift_max = 20;
    for (ulong n = 0; n <= MEM_SMALL_MAX + stdlib::array_size(MEM_EDGES); n++)
    {
        ulong size = (n <= MEM_SMALL_MAX) ? n : MEM_EDGES[n - MEM_SMALL_MAX - 1];
        for (long shift = -shift_max; shift <= shift_max; shift++)
        {
            ulong to = static_cast<ulong>(static_cast<long>(base) + shift);
            if (!mem_overlap(dst, ref, base + size + shift_max + MEM_GUARD, base, to, size))
                return false;
        }
    }

    /* Past two pages, unaligned both ways, and moved onto itself by a block
       and a byte, up and down */
    if (!mem_case(src, dst, MEM_LARGE, 5, 3))
        return false;
    const ulong large_cap = MEM_LARGE + 2 * MEM_ALIGN + MEM_GUARD;
    if (!mem_overlap(dst, ref, large_cap, MEM_ALIGN, MEM_ALIGN + 65, MEM_LARGE - 65) ||
        !mem_overlap(dst, ref, large_cap, MEM_ALIGN + 65, MEM_ALIGN, MEM_LARGE - 65) ||
        !mem_overlap(dst, ref, large_cap, MEM_ALIGN, MEM_ALIGN + 1, MEM_LARGE - 1) ||
        !mem_overlap(dst, ref, large_cap, MEM_ALIGN + 1, MEM_ALIGN, MEM_LARGE - 1))
        return false;
    return true;
}

/* What mem_cpy and mem_set cost, said every boot: MB/s, a byte a microsecond */
void mem_rate(u8* src, u8* dst)
{
    const ulong start = get_boot_time().get_value();
    for (ulong r = 0; r < MEM_RATE_ROUNDS; r++)
        stdlib::mem_cpy(dst, src, MEM_LARGE);
    const ulong copied = get_boot_time().get_value();
    for (ulong r = 0; r < MEM_RATE_ROUNDS; r++)
        stdlib::mem_set(dst, static_cast<u8>(r), MEM_LARGE);
    const ulong set = get_boot_time().get_value();

    const ulong bytes = MEM_RATE_ROUNDS * MEM_LARGE;
    const ulong copy_ns = (copied > start) ? copied - start : 1;
    const ulong set_ns = (set > copied) ? set - copied : 1;
    klog(0, "test_mem_routines: mem_cpy %lu MB/s, mem_set %lu MB/s", bytes * NS_PER_US / copy_ns,
        bytes * NS_PER_US / set_ns);
}

}

stdlib::Error test_mem_routines()
{
    klog(0, "test_mem_routines: started");

    u8* bufs = static_cast<u8*>(mm::alloc(3 * MEM_BUF_SIZE, MEM_TAG));
    if (bufs == nullptr)
        return make_error(stdlib::Error::NO_MEMORY);

    bool ok = mem_routines(bufs, bufs + MEM_BUF_SIZE, bufs + 2 * MEM_BUF_SIZE);
    if (ok)
        mem_rate(bufs, bufs + MEM_BUF_SIZE);
    mm::free(bufs);
    if (!ok)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_mem_routines: complete");
    return make_success();
}

stdlib::Error test_sn_printf()
{
    klog(0, "test_sn_printf: started");

    char buf[128];
    int rc;

    /* Existing specifiers still work */
    rc = stdlib::sn_printf(buf, sizeof(buf), "hello %s", "world");
    if (rc < 0 || stdlib::str_cmp(buf, "hello world") != 0)
    {
        klog(0, "test_sn_printf: %%s failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%lu", (ulong)42);
    if (rc < 0 || stdlib::str_cmp(buf, "42") != 0)
    {
        klog(0, "test_sn_printf: %%u failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%c", (int)'A');
    if (rc < 0 || stdlib::str_cmp(buf, "A") != 0)
    {
        klog(0, "test_sn_printf: %%c failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%lu", (ulong)0);
    if (rc < 0 || stdlib::str_cmp(buf, "0") != 0)
    {
        klog(0, "test_sn_printf: %%u zero failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* %% literal percent */
    rc = stdlib::sn_printf(buf, sizeof(buf), "100%%");
    if (rc < 0 || stdlib::str_cmp(buf, "100%") != 0)
    {
        klog(0, "test_sn_printf: %%%% failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%%start");
    if (rc < 0 || stdlib::str_cmp(buf, "%start") != 0)
    {
        klog(0, "test_sn_printf: %%%% at start failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* %d signed decimal */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%ld", (long)42);
    if (rc < 0 || stdlib::str_cmp(buf, "42") != 0)
    {
        klog(0, "test_sn_printf: %%d positive failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%ld", (long)-1);
    if (rc < 0 || stdlib::str_cmp(buf, "-1") != 0)
    {
        klog(0, "test_sn_printf: %%d -1 failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%ld", (long)-12345);
    if (rc < 0 || stdlib::str_cmp(buf, "-12345") != 0)
    {
        klog(0, "test_sn_printf: %%d negative failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%ld", (long)0);
    if (rc < 0 || stdlib::str_cmp(buf, "0") != 0)
    {
        klog(0, "test_sn_printf: %%d zero failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* %x lowercase hex */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%lx", (ulong)0);
    if (rc < 0 || stdlib::str_cmp(buf, "0") != 0)
    {
        klog(0, "test_sn_printf: %%x zero failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%lx", (ulong)0xDEAD);
    if (rc < 0 || stdlib::str_cmp(buf, "dead") != 0)
    {
        klog(0, "test_sn_printf: %%x deadbeef failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%lx", (ulong)255);
    if (rc < 0 || stdlib::str_cmp(buf, "ff") != 0)
    {
        klog(0, "test_sn_printf: %%x 255 failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* %X uppercase hex */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%lX", (ulong)0xDEAD);
    if (rc < 0 || stdlib::str_cmp(buf, "DEAD") != 0)
    {
        klog(0, "test_sn_printf: %%X failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%lX", (ulong)255);
    if (rc < 0 || stdlib::str_cmp(buf, "FF") != 0)
    {
        klog(0, "test_sn_printf: %%X 255 failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Zero-padded width: %08x */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%08lx", (ulong)0xFF);
    if (rc < 0 || stdlib::str_cmp(buf, "000000ff") != 0)
    {
        klog(0, "test_sn_printf: %%08x failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%016lX", (ulong)0xABCD1234);
    if (rc < 0 || stdlib::str_cmp(buf, "00000000ABCD1234") != 0)
    {
        klog(0, "test_sn_printf: %%016X failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Zero-padded decimal: %05u */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%05lu", (ulong)42);
    if (rc < 0 || stdlib::str_cmp(buf, "00042") != 0)
    {
        klog(0, "test_sn_printf: %%05u failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Zero-padded signed: %05d with negative */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%05ld", (long)-42);
    if (rc < 0 || stdlib::str_cmp(buf, "-0042") != 0)
    {
        klog(0, "test_sn_printf: %%05d negative failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Zero-padded signed: value fills width */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%03ld", (long)12345);
    if (rc < 0 || stdlib::str_cmp(buf, "12345") != 0)
    {
        klog(0, "test_sn_printf: %%03d overflow failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Width with no zero-pad (space padding) */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%5lu", (ulong)42);
    if (rc < 0 || stdlib::str_cmp(buf, "   42") != 0)
    {
        klog(0, "test_sn_printf: %%5u space pad failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(buf, sizeof(buf), "%8lx", (ulong)0xFF);
    if (rc < 0 || stdlib::str_cmp(buf, "      ff") != 0)
    {
        klog(0, "test_sn_printf: %%8x space pad failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* String with width */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%10s", "hi");
    if (rc < 0 || stdlib::str_cmp(buf, "        hi") != 0)
    {
        klog(0, "test_sn_printf: %%10s failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Mixed specifiers in one format string */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%ld %lu %lx %lX %s %%",
                          (long)-7, (ulong)100, (ulong)0xAB, (ulong)0xCD, "ok");
    if (rc < 0 || stdlib::str_cmp(buf, "-7 100 ab CD ok %") != 0)
    {
        klog(0, "test_sn_printf: mixed failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Null string */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%s", (const char*)nullptr);
    if (rc < 0 || stdlib::str_cmp(buf, "(null)") != 0)
    {
        klog(0, "test_sn_printf: null string failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* %p still works (uppercase hex) */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%p", (void*)(ulong)0xABCDEF);
    if (rc < 0 || stdlib::str_cmp(buf, "ABCDEF") != 0)
    {
        klog(0, "test_sn_printf: %%p failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* More than the buffer holds is truncated and marked, never dropped: a
       %s that did not fit used to take the whole line with it, and Trace
       threw away every line that reported the overflow -- which is how a
       Rust panic reached the console as "RUST PANIC: " and nothing else. */
    char small[8];
    rc = stdlib::sn_printf(small, sizeof(small), "ab%s", "cdefghij");
    if (rc != 7 || stdlib::str_cmp(small, "abcd...") != 0)
    {
        klog(0, "test_sn_printf: long %%s not truncated: '%s' (%d)", small, rc);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(small, sizeof(small), "abcdefghij");
    if (rc != 7 || stdlib::str_cmp(small, "abcd...") != 0)
    {
        klog(0, "test_sn_printf: long text not truncated: '%s' (%d)", small, rc);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    rc = stdlib::sn_printf(small, sizeof(small), "%lu", (ulong)123456789);
    if (rc != 7 || stdlib::str_cmp(small, "1234...") != 0)
    {
        klog(0, "test_sn_printf: long %%u not truncated: '%s' (%d)", small, rc);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Exactly the buffer, terminator included: no mark, nothing lost. */
    rc = stdlib::sn_printf(small, sizeof(small), "abcdefg");
    if (rc != 7 || stdlib::str_cmp(small, "abcdefg") != 0)
    {
        klog(0, "test_sn_printf: exact fit failed: '%s' (%d)", small, rc);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Each argument read as the type it was passed as, which the length
       modifier names (PRINTF_FORMAT): an int is not read as a 64-bit slot.
       Twelve of them, so that the ones past the third are on the stack on
       x86-64 and past the fifth on arm64 (buf, size and fmt take three of
       the six and eight registers), where an int read as a long brings the
       other half of its slot with it. */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%u %lu %d %ld %x %lx %u %lu %d %ld %X %lX",
        (u32)0xFFFFFFFF, (ulong)1 << 40, (int)-5, (long)-6, (u32)0xAB, (ulong)0xCD00000000UL,
        (u32)7, (ulong)8, (int)-9, (long)-10, (u32)0xEF, (ulong)0x1200000000UL);
    if (rc < 0 || stdlib::str_cmp(buf,
        "4294967295 1099511627776 -5 -6 ab cd00000000 7 8 -9 -10 EF 1200000000") != 0)
    {
        klog(0, "test_sn_printf: widths failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* hh and h: a char or a short, passed as an int */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%hhu %hhd %hu %hd",
        (u8)200, (s8)-3, (u16)60000, (s16)-30000);
    if (rc < 0 || stdlib::str_cmp(buf, "200 -3 60000 -30000") != 0)
    {
        klog(0, "test_sn_printf: hh/h failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* z a size_t, ll a long long, their extremes included */
    rc = stdlib::sn_printf(buf, sizeof(buf), "%zu %lld %llu %llx",
        (size_t)123, (long long)-9223372036854775807LL - 1, 18446744073709551615ULL, 0xABCULL);
    if (rc < 0 || stdlib::str_cmp(buf, "123 -9223372036854775808 18446744073709551615 abc") != 0)
    {
        klog(0, "test_sn_printf: z/ll failed: '%s'", buf);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    klog(0, "test_sn_printf: complete");
    return make_success();
}

/* The filesystem self-test (src/crates/layers/fs/src/selftest.rs) on a ramfs mounted
   on / for the duration and taken down after: the whole file API, over a
   filesystem that needs no disk. */
extern "C" int kernel_fs_selftest(const char* dir, unsigned long dir_len, unsigned long size);
extern "C" int rust_ramfs_mount(const char* path, unsigned long path_len, int read_only);
extern "C" int kernel_vfs_unmount(const char* path, unsigned long path_len);

stdlib::Error test_vfs()
{
    static const ulong BIG_SIZE = 100 * 1024;

    if (rust_ramfs_mount("/", 1, 0) != 0)
    {
        klog(0, "test_vfs: mount failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    bool ok = kernel_fs_selftest("/", 1, BIG_SIZE) == 0;

    if (kernel_vfs_unmount("/", 1) != 0)
    {
        klog(0, "test_vfs: unmount failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    if (!ok)
    {
        klog(0, "test_vfs: self-test failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    klog(0, "test_vfs: complete");
    return make_success();
}

/* The network layer's packet formats (src/crates/layers/net/src/selftest.rs). A ping
   that comes back checks the sending half; what it cannot reach is the
   receiving one -- under QEMU user networking nothing ever pings this
   machine or asks for its address -- so the formats behind those answers are
   checked here instead, against packets written out by hand. */
extern "C" int rust_net_selftest();

stdlib::Error test_net_wire()
{
    klog(0, "test_net_wire: started");

    if (rust_net_selftest() != 0)
    {
        klog(0, "test_net_wire: failed");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    klog(0, "test_net_wire: complete");
    return make_success();
}

static bool count_grub_env_var(const char* name, const char* value, void* ctx)
{
    (void)name;
    (void)value;
    auto* count = static_cast<ulong*>(ctx);
    (*count)++;
    return true;
}

/* Does the block hold exactly this value for name? */
static bool grub_env_has(stdlib::GrubEnvBlock& env, const char* name, const char* expected)
{
    char value[stdlib::GrubEnvBlock::MAX_VALUE_LEN + 1];
    return env.get(name, value, sizeof(value)) && stdlib::str_cmp(value, expected) == 0;
}

stdlib::Error test_grub_env()
{
    klog(0, "test_grub_env: started");

    using stdlib::GrubEnvBlock;

    char block[GrubEnvBlock::DEFAULT_SIZE];
    GrubEnvBlock::format(block, sizeof(block));
    GrubEnvBlock env(block, sizeof(block));
    char value[GrubEnvBlock::MAX_VALUE_LEN + 1];

    if (!env.is_valid() || env.get("nos_next", value, sizeof(value)))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Set, read back, and a second variable behind the first */
    if (!env.set("nos_next", "nos-next") || !env.set("other", "x") ||
        !grub_env_has(env, "nos_next", "nos-next") || !grub_env_has(env, "other", "x"))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* A longer value shifts the line behind it, intact; so does a shorter one */
    if (!env.set("nos_next", "nos-multiboot2") ||
        !grub_env_has(env, "nos_next", "nos-multiboot2") || !grub_env_has(env, "other", "x"))
        return make_error(stdlib::Error::UNSUCCESSFUL);
    if (!env.set("nos_next", "n") ||
        !grub_env_has(env, "nos_next", "n") || !grub_env_has(env, "other", "x"))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* What GRUB leaves behind when it clears a flag: the name, nothing after '=' */
    if (!env.set("nos_next", "") || !grub_env_has(env, "nos_next", ""))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Escapes go on and come off */
    if (!env.set("esc", "a\\b") || !grub_env_has(env, "esc", "a\\b"))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    ulong count = 0;
    if (!env.for_each(count_grub_env_var, &count) || count != 3)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* Unset closes the gap; a second unset finds nothing */
    if (!env.unset("nos_next") || env.get("nos_next", value, sizeof(value)) ||
        env.unset("nos_next") || !grub_env_has(env, "other", "x") || !grub_env_has(env, "esc", "a\\b"))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* The layout GRUB's own writer insists on: lines, a newline, '#' to the end */
    {
        ulong pos = sizeof(block);
        while (pos > 0 && block[pos - 1] == '#')
            pos--;
        if (pos == sizeof(block) || block[pos - 1] != '\n')
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* A name GRUB would not take, and a value there is no room for, leave
       the block as it was */
    char copy[sizeof(block)];
    stdlib::mem_cpy(copy, block, sizeof(block));
    if (env.set("bad name", "x") || env.set("", "x") || env.set("a=b", "x") ||
        stdlib::mem_cmp(copy, block, sizeof(block)) != 0)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    char big[GrubEnvBlock::MAX_VALUE_LEN + 1];
    stdlib::mem_set(big, 'v', sizeof(big) - 1);
    big[sizeof(big) - 1] = '\0';
    bool full = false;
    for (ulong i = 0; i < 8 && !full; i++)
    {
        char name[8];
        stdlib::sn_printf(name, sizeof(name), "big%lu", i);
        stdlib::mem_cpy(copy, block, sizeof(block));
        if (!env.set(name, big))
        {
            if (stdlib::mem_cmp(copy, block, sizeof(block)) != 0)
                return make_error(stdlib::Error::UNSUCCESSFUL);
            full = true;
        }
    }
    if (!full || !grub_env_has(env, "other", "x"))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    /* A comment line, as grub-editenv writes one, is passed over */
    {
        static const char COMMENT[] = "# WARNING: Do not edit this file by hand\n";
        static const char LINE[] = "a=1\n";
        GrubEnvBlock::format(block, sizeof(block));
        stdlib::mem_cpy(block + GrubEnvBlock::SIGNATURE_LEN, COMMENT, sizeof(COMMENT) - 1);
        stdlib::mem_cpy(block + GrubEnvBlock::SIGNATURE_LEN + sizeof(COMMENT) - 1, LINE, sizeof(LINE) - 1);
        if (!grub_env_has(env, "a", "1") || !env.set("b", "2") ||
            !grub_env_has(env, "a", "1") || !grub_env_has(env, "b", "2") ||
            stdlib::mem_cmp(block + GrubEnvBlock::SIGNATURE_LEN, COMMENT, sizeof(COMMENT) - 1) != 0)
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* A line with no '=' would lend its text to the next line's name as
       for_each -- and GRUB -- read it, while Set would find "a" on a line of
       its own: the block is malformed, and Set and Unset leave it as it was */
    {
        static const char LINES[] = "b\na=1\n";
        GrubEnvBlock::format(block, sizeof(block));
        stdlib::mem_cpy(block + GrubEnvBlock::SIGNATURE_LEN, LINES, sizeof(LINES) - 1);
        stdlib::mem_cpy(copy, block, sizeof(block));
        count = 0;
        if (env.for_each(count_grub_env_var, &count) || env.set("a", "2") || env.unset("a") ||
            stdlib::mem_cmp(copy, block, sizeof(block)) != 0)
            return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Not a block at all */
    block[0] = 'x';
    if (env.is_valid() || env.set("nos_next", "nos-next") || env.get("a", value, sizeof(value)))
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_grub_env: complete");
    return make_success();
}

/* modtest.ko -- src/crates/modules/test/modtest as the Makefile builds it --
   embedded in the kernel image (modtest_blob.S) for test_modules to load */
extern "C" const u8 nos_modtest_ko[];
extern "C" const ulong nos_modtest_ko_size;

/* The command's output says where modtest's double() landed. The symbolizer
   has to name it back, out of the function table the build put in the .ko --
   and the kernel's own functions still by the kernel's table, with an
   address in neither left unnamed. */
static stdlib::Error check_symbolizer(const char* output)
{
    static const char MARKER[] = "double at ";
    static const ulong DIGITS_MAX = 24;

    const char* at = stdlib::str_str(output, MARKER);
    if (at == nullptr)
    {
        klog(0, "test_modules: no address in: %s", output);
        return make_error(stdlib::Error::NOT_FOUND);
    }

    char digits[DIGITS_MAX];
    ulong n = 0;
    at += sizeof(MARKER) - 1;
    for (; n < sizeof(digits) - 1 && at[n] >= '0' && at[n] <= '9'; n++)
        digits[n] = at[n];
    digits[n] = '\0';

    auto& symtab = SymbolTable::get_instance();
    char where[SymbolTable::DESCRIBE_MAX];
    where[0] = '\0';
    ulong addr = 0;
    if (!stdlib::parse_ulong(digits, addr) ||
        !symtab.describe(addr, where, sizeof(where)) ||
        stdlib::str_str(where, "mod_modtest::double+0x") == nullptr ||
        stdlib::str_str(where, "[modtest]") == nullptr)
    {
        klog(0, "test_modules: modtest's double() at 0x%lX named '%s'", addr, where);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    where[0] = '\0';
    if (!symtab.describe(reinterpret_cast<ulong>(&check_symbolizer), where, sizeof(where)) ||
        stdlib::str_str(where, "check_symbolizer") == nullptr ||
        symtab.describe(0, where, sizeof(where)))
    {
        klog(0, "test_modules: the kernel's own named '%s'", where);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* A backtrace's frames are return addresses, named by the call before
       them: one a byte into a function is that function's, at +0x1 -- in the
       kernel and in a module alike -- and one at a function's first byte is
       the end of whatever precedes it, which is what a call that does not
       return leaves on the stack, never the function at +0x0 */
    const ulong self = reinterpret_cast<ulong>(&check_symbolizer);
    where[0] = '\0';
    if (!symtab.describe_return(self + 1, where, sizeof(where)) ||
        stdlib::str_str(where, "check_symbolizer+0x1") == nullptr)
    {
        klog(0, "test_modules: a return address a byte into check_symbolizer named '%s'", where);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }
    where[0] = '\0';
    if (symtab.describe_return(self, where, sizeof(where)) &&
        stdlib::str_str(where, "check_symbolizer+0x0") != nullptr)
    {
        klog(0, "test_modules: a return address at check_symbolizer's start named '%s'", where);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }
    where[0] = '\0';
    if (!symtab.describe_return(addr + 1, where, sizeof(where)) ||
        stdlib::str_str(where, "mod_modtest::double+0x1 [modtest]") == nullptr ||
        symtab.describe_return(0, where, sizeof(where)))
    {
        klog(0, "test_modules: a return address a byte into modtest's double() named '%s'", where);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    return make_success();
}

/* Overwrite every occurrence of from with to, a string as long */
static void patch_all(u8* buf, ulong size, const char* from, const char* to)
{
    const ulong len = stdlib::str_len(from);
    if (stdlib::str_len(to) != len)
        return;

    for (ulong i = 0; i + len <= size; i++)
    {
        if (stdlib::mem_cmp(buf + i, from, len) == 0)
            stdlib::mem_cpy(buf + i, to, len);
    }
}

/* Flip a bit of the byte offset bytes past the first occurrence of marker */
static void flip_after(u8* buf, ulong size, const char* marker, ulong offset)
{
    const ulong len = stdlib::str_len(marker);
    for (ulong i = 0; i + len <= size; i++)
    {
        if (stdlib::mem_cmp(buf + i, marker, len) == 0)
        {
            if (i + offset < size)
                buf[i + offset] ^= 1;
            return;
        }
    }
}

/* What the console shows of the line being edited: the editor prints text
   and Backspace alone, and this is the row they make. Backspace on an empty
   row would have erased the line above it, and a row past what the editor
   ever shows is a redraw that forgot what was there: either breaks it. */
class EditorScreen final : public stdlib::Printer
{
public:
    EditorScreen()
        : len_(0)
        , broken_(false)
    {
        row_[0] = '\0';
    }

    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        v_printf(fmt, args);
        va_end(args);
    }

    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) override
    {
        char text[ROW_MAX + 1];
        if (stdlib::vsn_printf(text, sizeof(text), fmt, args) < 0)
            return;
        print_string(text);
    }

    virtual void print_string(const char *s) override
    {
        for (; *s != '\0'; s++)
        {
            if (*s == '\n')
                len_ = 0;
            else if (len_ < ROW_MAX)
                row_[len_++] = *s;
            else
                broken_ = true;
            row_[len_] = '\0';
        }
    }

    virtual void backspace() override
    {
        if (len_ == 0)
            broken_ = true;
        else
            row_[--len_] = '\0';
    }

    /* The row is want, and nothing broke on the way */
    bool shows(const char* want, ulong step) const
    {
        if (!broken_ && stdlib::str_cmp(row_, want) == 0)
            return true;
        klog(0, "test_line_editor: step %lu shows '%s'%s, not '%s'", step, row_,
            broken_ ? " (broken)" : "", want);
        return false;
    }

private:
    static const ulong ROW_MAX = 256;

    char row_[ROW_MAX + 1];
    ulong len_;
    bool broken_;
};

/* Types keys at the editor as the shell does: a line it ends is counted and
   kept in line, and the next prompt shown */
static void type_at(LineEditor& ed, EditorScreen& screen, const char* keys, ulong& lines, const char*& line)
{
    for (; *keys != '\0'; keys++)
    {
        if (ed.key(*keys, screen))
        {
            lines++;
            line = ed.get_line();
            ed.prompt(screen);
        }
    }
}

static bool test_line_editor_on(LineEditor& ed)
{
    EditorScreen screen;
    ulong lines = 0;
    const char* line = "";

    ed.prompt(screen);
    if (!screen.shows("$", 1))
        return false;

    /* Three lines run, a blank one not kept, the same line twice kept once */
    type_at(ed, screen, "echo one\necho two\n   \nls\nls\n", lines, line);
    if (lines != 5 || stdlib::str_cmp(line, "ls") != 0 || !screen.shows("$", 2))
        return false;

    /* Up goes back to the oldest and no further, Down forward to what was
       being typed; ^P and ^N are the same; an edited line runs as edited */
    type_at(ed, screen, "dr\x1b[A", lines, line);
    if (!screen.shows("$ls", 3))
        return false;
    type_at(ed, screen, "\x1b[A\x1bOA\x1b[A", lines, line);
    if (!screen.shows("$echo one", 4))
        return false;
    type_at(ed, screen, "\x0e\x0e", lines, line);
    if (!screen.shows("$ls", 5))
        return false;
    type_at(ed, screen, "\x1b[B", lines, line);
    if (!screen.shows("$dr", 6))
        return false;
    type_at(ed, screen, "\x1b[B\x10\x7f\x08ls -l\n", lines, line);
    if (lines != 6 || stdlib::str_cmp(line, "ls -l") != 0 || !screen.shows("$", 7))
        return false;

    /* ^R finds the newest line holding the query, ^R again an older one, and
       Enter runs it; history: echo one, echo two, ls, ls -l */
    type_at(ed, screen, "\x12" "ech", lines, line);
    if (!screen.shows("(reverse-i-search)'ech': echo two", 8))
        return false;
    type_at(ed, screen, "\x12", lines, line);
    if (!screen.shows("(reverse-i-search)'ech': echo one", 9))
        return false;
    type_at(ed, screen, "\x12", lines, line);
    if (!screen.shows("(failed reverse-i-search)'ech': echo one", 10))
        return false;
    type_at(ed, screen, "\n", lines, line);
    if (lines != 7 || stdlib::str_cmp(line, "echo one") != 0 || !screen.shows("$", 11))
        return false;

    /* A query that fails, made shorter, finds again from the newest line;
       ^G puts back the line from before the search */
    type_at(ed, screen, "x\x12lz", lines, line);
    if (!screen.shows("(failed reverse-i-search)'lz': ls -l", 12))
        return false;
    type_at(ed, screen, "\x08", lines, line);
    if (!screen.shows("(reverse-i-search)'l': ls -l", 13))
        return false;
    type_at(ed, screen, "\x7f", lines, line);
    if (!screen.shows("(reverse-i-search)'': x", 14))
        return false;
    type_at(ed, screen, "two\x07", lines, line);
    if (!screen.shows("$x", 15))
        return false;

    /* Any other key keeps what was found, to be edited, and Up and Down go
       on from it; Down past the newest gives back the line from before */
    type_at(ed, screen, "\x12two\x1b[D", lines, line);
    if (!screen.shows("$echo two", 16))
        return false;
    type_at(ed, screen, "\x1b[A", lines, line);
    if (!screen.shows("$echo one", 17))
        return false;
    type_at(ed, screen, "\x1b[B\x1b[B\x1b[B\x1b[B", lines, line);
    if (!screen.shows("$echo one", 18))
        return false;
    type_at(ed, screen, "\x1b[B", lines, line);
    if (!screen.shows("$x", 19))
        return false;

    /* ^C drops the line and ^U clears it; a sequence a control key cuts
       short leaves the key to be itself */
    type_at(ed, screen, "\x03", lines, line);
    if (lines != 7 || !screen.shows("$", 20))
        return false;
    type_at(ed, screen, "abc\x15" "def\x1b[\n", lines, line);
    if (lines != 8 || stdlib::str_cmp(line, "def") != 0 || !screen.shows("$", 21))
        return false;

    /* A line stops at LINE_LEN_MAX */
    for (ulong i = 0; i < LineEditor::LINE_LEN_MAX + 10; i++)
        type_at(ed, screen, "a", lines, line);
    type_at(ed, screen, "\n", lines, line);
    if (lines != 9 || stdlib::str_len(line) != LineEditor::LINE_LEN_MAX)
        return false;

    /* Of more lines than it keeps, the oldest go */
    for (ulong i = 0; i < LineEditor::HISTORY_MAX + 8; i++)
    {
        char cmd[16];
        stdlib::sn_printf(cmd, sizeof(cmd), "c%lu\n", i);
        type_at(ed, screen, cmd, lines, line);
    }
    for (ulong i = 0; i < LineEditor::HISTORY_MAX + 4; i++)
        type_at(ed, screen, "\x10", lines, line);
    if (!screen.shows("$c8", 22))
        return false;
    type_at(ed, screen, "\x15", lines, line);

    return screen.shows("$", 23);
}

/* The console's line editor (kernel/line_editor.h): history, Up and Down,
   ^R's search, all on a screen of text and Backspace. cpp-fuzz.py's
   lineedit target holds it to a model over random keys. */
stdlib::Error test_line_editor()
{
    klog(0, "test_line_editor: started");

    LineEditor* ed = new (mm::NO_THROW) LineEditor();
    if (ed == nullptr)
        return make_error(stdlib::Error::NO_MEMORY);

    bool ok = test_line_editor_on(*ed);
    delete ed;

    if (!ok)
        return make_error(stdlib::Error::UNSUCCESSFUL);

    klog(0, "test_line_editor: complete");
    return make_success();
}

/* The loader end to end, on modtest.ko. Loaded, the module runs its own
   checks in its init and adds a shell command, which answers through the
   dispatcher; a second copy is refused; unloaded, the command goes with it.
   All of it twice, the second time into memory the first gave back and
   with a parameter, which the command's answer must show reached the init.
   Then a parameter the module has not got, and one longer than insmod
   takes, refused; then damaged copies, which the loader must turn away --
   the last three only after mapping and relocating them, so the cleanup of
   a load that fails halfway runs too. */
/* The management bus's module side (src/crates/layers/mgmt), through the `mgmt`
   command: modtest's provider answers a call with its parameters, and a job
   of it that runs until it is told to stop is started -- and seen running,
   so that it is the unload, next, that has to stop it and wait it out */
static stdlib::Error check_bus_loaded(stdlib::BufferPrinter& out)
{
    static const ulong JOB_WAIT_POLLS = 2000;

    /* The bus as a module asks it -- the host API's way in */
    out.reset();
    Cmd::dispatch("modtest bus", out);
    if (stdlib::str_str(out.get(), "modtest bus: ok") == nullptr)
    {
        klog(0, "test_module_bus: the module asking the bus: %s", out.get());
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    out.reset();
    Cmd::dispatch("mgmt call modtest echo {\"n\":42}", out);
    if (stdlib::str_str(out.get(), "{\"n\":42}") == nullptr)
    {
        klog(0, "test_module_bus: the module's provider answered: %s", out.get());
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    out.reset();
    Cmd::dispatch("mgmt start modtest wait", out);
    if (stdlib::str_str(out.get(), "started") == nullptr)
    {
        klog(0, "test_module_bus: the module's job was not started: %s", out.get());
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }
    for (ulong i = 0; i < JOB_WAIT_POLLS; i++)
    {
        out.reset();
        Cmd::dispatch("mgmt jobs", out);
        if (stdlib::str_str(out.get(), "modtest.wait running") != nullptr)
            return make_success();
        sleep(consts::NANO_SECS_IN_MS);
    }
    klog(0, "test_module_bus: the module's job never ran: %s", out.get());
    return make_error(stdlib::Error::UNSUCCESSFUL);
}

/* After the unload: the job was told to stop and ended so, and the provider
   is gone with its module */
static stdlib::Error check_bus_unloaded(stdlib::BufferPrinter& out)
{
    out.reset();
    Cmd::dispatch("mgmt jobs", out);
    if (stdlib::str_str(out.get(), "modtest.wait done cancelled") == nullptr)
    {
        klog(0, "test_module_bus: the job the unload had to stop: %s", out.get());
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    out.reset();
    Cmd::dispatch("mgmt call modtest echo {}", out);
    if (stdlib::str_str(out.get(), "mgmt: no_provider") == nullptr)
    {
        klog(0, "test_module_bus: the provider outlived its module: %s", out.get());
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }
    return make_success();
}

stdlib::Error test_modules()
{
    /* kmod::ModuleInfo: the magic "NOSM", the version, then the digest of the
       kernel interface the module was built against */
    static const ulong HEADER_ABI_OFFSET = 8;
    static const ulong DAMAGE_COUNT = 6;
    static const ulong OUTPUT_SIZE = 1024;
    /* What each round hands modtest's init, and what its command answers:
       42 + 10, what its checks computed, unless answer= says otherwise */
    static const char* const ROUND_PARAMS[] = { "", "answer=7" };
    static const char* const ROUND_ANSWERS[] = {
        "modtest: answer 52 args '1 2'",
        "modtest: answer 7 args '1 2'",
    };

    auto& modules = ModuleTable::get_instance();
    char output[OUTPUT_SIZE];
    stdlib::BufferPrinter out(output, sizeof(output));

    for (ulong round = 0; round < 2; round++)
    {
        out.reset();
        stdlib::Error err = modules.load(nos_modtest_ko, nos_modtest_ko_size, ROUND_PARAMS[round], out);
        if (!err.ok())
        {
            klog(0, "test_modules: load failed: %s", out.get());
            return err;
        }

        out.reset();
        Cmd::dispatch("modtest 1 2", out);
        if (stdlib::str_str(out.get(), ROUND_ANSWERS[round]) == nullptr)
        {
            klog(0, "test_modules: the module's command answered: %s", out.get());
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        err = check_symbolizer(out.get());
        if (!err.ok())
            return err;

        out.reset();
        err = modules.load(nos_modtest_ko, nos_modtest_ko_size, "", out);
        if (err.get_code() != stdlib::Error::ALREADY_EXISTS || !modules.is_loaded("modtest"))
        {
            klog(0, "test_modules: a second copy was not refused: %s", out.get());
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        out.reset();
        err = modules.unload("modtest", out);
        if (!err.ok() || modules.is_loaded("modtest"))
        {
            klog(0, "test_modules: unload failed: %s", out.get());
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }

        out.reset();
        Cmd::dispatch("modtest", out);
        if (stdlib::str_str(out.get(), "not found") == nullptr)
        {
            klog(0, "test_modules: the command outlived its module: %s", out.get());
            return make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    /* A parameter it has not got: its init says so, and refuses the load */
    out.reset();
    stdlib::Error err = modules.load(nos_modtest_ko, nos_modtest_ko_size, "answr=7", out);
    if (err.get_code() != stdlib::Error::UNSUCCESSFUL || modules.is_loaded("modtest") ||
        stdlib::str_str(out.get(), "\"answr\" is not a parameter") == nullptr)
    {
        klog(0, "test_modules: an unknown parameter: error %lu: %s", (ulong)err.get_code(), out.get());
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    /* Longer than insmod hands on: refused before the module is looked at */
    char long_params[ModuleTable::PARAMS_MAX + 2];
    stdlib::mem_set(long_params, 'a', sizeof(long_params) - 1);
    long_params[sizeof(long_params) - 1] = '\0';
    out.reset();
    err = modules.load(nos_modtest_ko, nos_modtest_ko_size, long_params, out);
    if (err.get_code() != stdlib::Error::INVALID_VALUE || modules.is_loaded("modtest"))
    {
        klog(0, "test_modules: parameters too long: error %lu: %s", (ulong)err.get_code(), out.get());
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    u8* copy = static_cast<u8*>(mm::alloc(nos_modtest_ko_size, TAG));
    if (copy == nullptr)
        return make_error(stdlib::Error::NO_MEMORY);

    stdlib::Error result = make_success();
    for (ulong i = 0; i < DAMAGE_COUNT && result.ok(); i++)
    {
        stdlib::mem_cpy(copy, nos_modtest_ko, nos_modtest_ko_size);
        ulong size = nos_modtest_ko_size;
        const char* what;
        int expected;

        switch (i)
        {
        case 0:
            what = "a truncated header";
            size = sizeof(elf::Ehdr) - 1;
            expected = stdlib::Error::BAD_MAGIC;
            break;
        case 1:
            what = "another machine's";
            reinterpret_cast<elf::Ehdr*>(copy)->machine = 0;
            expected = stdlib::Error::INVALID_VALUE;
            break;
        case 2:
            what = "an import the kernel lacks";
            patch_all(copy, size, "kernel_klog", "kernel_kl0g");
            expected = stdlib::Error::NOT_FOUND;
            break;
        case 3:
            what = "no module header";
            patch_all(copy, size, "nos_module_info", "nos_module_inf0");
            expected = stdlib::Error::NOT_FOUND;
            break;
        case 4:
            what = "a bad header magic";
            patch_all(copy, size, "NOSM", "NOSX");
            expected = stdlib::Error::BAD_MAGIC;
            break;
        default:
            what = "another kernel interface";
            flip_after(copy, size, "NOSM", HEADER_ABI_OFFSET);
            expected = stdlib::Error::INVALID_VALUE;
            break;
        }

        out.reset();
        err = modules.load(copy, size, "", out);
        if (err.get_code() != expected || modules.is_loaded("modtest"))
        {
            klog(0, "test_modules: %s: error %lu, expected %lu: %s", what,
                (ulong)err.get_code(), (ulong)expected, out.get());
            result = make_error(stdlib::Error::UNSUCCESSFUL);
        }
    }

    mm::free(copy);
    if (!result.ok())
        return result;

    klog(0, "test_modules: complete");
    return make_success();
}

/* The management bus's module side, once there are tasks to run a job on
   (test_modules runs before there are): modtest loaded again, its provider
   called, a job of it started, and the module's unload -- which takes the
   provider off the bus -- telling the job to stop and waiting it out. */
bool test_module_bus()
{
    static const ulong OUTPUT_SIZE = 1024;

    auto& modules = ModuleTable::get_instance();
    char output[OUTPUT_SIZE];
    stdlib::BufferPrinter out(output, sizeof(output));

    stdlib::Error err = modules.load(nos_modtest_ko, nos_modtest_ko_size, "", out);
    if (!err.ok())
    {
        klog(0, "test_module_bus: load failed: %s", out.get());
        return false;
    }

    err = check_bus_loaded(out);
    out.reset();
    stdlib::Error unloaded = modules.unload("modtest", out);
    if (!unloaded.ok() || modules.is_loaded("modtest"))
    {
        klog(0, "test_module_bus: unload failed: %s", out.get());
        return false;
    }
    if (!err.ok())
        return false;

    return check_bus_unloaded(out).ok();
}

/* What test_module_unload's task runs: `modtest hold` through the
   dispatcher, what it says kept for the test to read once the task is done */
struct HoldCall
{
    static const ulong OUTPUT_SIZE = 512;

    HoldCall()
    {
        output[0] = '\0';
    }

    char output[OUTPUT_SIZE];
};

static void hold_call_task(void* ctx)
{
    auto* hold = static_cast<HoldCall*>(ctx);
    stdlib::BufferPrinter out(hold->output, sizeof(hold->output));
    Cmd::dispatch("modtest hold", out);
}

/* What test_module_unload holds the net layer's sweep to
   (rust_net_close_range): modtest loaded again, its `modtest listen` taking
   the port the unloaded copy left a listener on -- refused, that listener
   outlived its module, its callbacks in freed pages -- and unloaded again,
   the one it leaves now swept the same way */
static bool check_listener_swept(stdlib::BufferPrinter& out)
{
    auto& modules = ModuleTable::get_instance();

    out.reset();
    stdlib::Error err = modules.load(nos_modtest_ko, nos_modtest_ko_size, "", out);
    if (!err.ok())
    {
        klog(0, "test_module_unload: the load after the listener's: %s", out.get());
        return false;
    }

    out.reset();
    Cmd::dispatch("modtest listen", out);
    const bool port_free = (stdlib::str_str(out.get(), "modtest listen: left listening") != nullptr);
    if (!port_free)
        klog(0, "test_module_unload: the listener the unload had to take off: %s", out.get());

    out.reset();
    stdlib::Error unloaded = modules.unload("modtest", out);
    if (!unloaded.ok() || modules.is_loaded("modtest"))
    {
        klog(0, "test_module_unload: the unload after the listener's: %s", out.get());
        return false;
    }
    return port_free;
}

bool test_module_unload()
{
    static const ulong OUTPUT_SIZE = 1024;
    static const ulong HOLD_WAIT_POLLS = 5000;
    /* How long the unloaded module's timer is given to tick again, were it
       left running: several of its periods, and of the tick's */
    static const ulong LEFT_TIMER_NS = 50 * consts::NANO_SECS_IN_MS;

    auto& modules = ModuleTable::get_instance();
    char output[OUTPUT_SIZE];
    stdlib::BufferPrinter out(output, sizeof(output));

    stdlib::Error err = modules.load(nos_modtest_ko, nos_modtest_ko_size, "", out);
    if (!err.ok())
    {
        klog(0, "test_module_unload: load failed: %s", out.get());
        return false;
    }

    /* The call, running on a task of its own, and seen running */
    auto* hold = mm::talloc<HoldCall, TAG>();
    Task* task = (hold != nullptr) ? mm::talloc<Task, TAG>("modhold") : nullptr;
    const bool started = (task != nullptr && task->start(hold_call_task, hold));
    bool holding = false;
    for (ulong i = 0; started && !holding && i < HOLD_WAIT_POLLS; i++)
    {
        out.reset();
        Cmd::dispatch("modtest holding", out);
        holding = (stdlib::str_str(out.get(), "modtest holding: 1") != nullptr);
        if (!holding)
            sleep(consts::NANO_SECS_IN_MS);
    }

    /* A timer the module starts and leaves, its handler the module's own
       code: the unload has to stop it (sweep_module_slots). Left running,
       its next tick is a call into the pages the unload frees -- a fault
       inside the wait below, which is the check. */
    out.reset();
    Cmd::dispatch("modtest timer", out);
    const bool ticking = (stdlib::str_str(out.get(), "modtest timer: left running") != nullptr);
    if (!ticking)
        klog(0, "test_module_unload: the timer: %s", out.get());

    /* A UDP listener the module leaves, its callbacks the module's code:
       the unload has to take it off (rust_net_close_range), which the next
       load finds out by taking the port again (check_listener_swept). A
       boot with no NIC has nothing to listen on: said, and not held
       against it */
    out.reset();
    Cmd::dispatch("modtest listen", out);
    const bool listening = (stdlib::str_str(out.get(), "modtest listen: left listening") != nullptr);
    const bool no_nic = (stdlib::str_str(out.get(), "modtest listen: no ") != nullptr);
    if (!listening)
        klog(0, "test_module_unload: the listener: %s", out.get());

    /* Unloaded whatever came of that: the call returns only once the stop
       has run, and the unload, only once the call has returned */
    out.reset();
    stdlib::Error unloaded = modules.unload("modtest", out);
    bool ok = unloaded.ok() && !modules.is_loaded("modtest") && ticking && (listening || no_nic);
    if (!unloaded.ok() || modules.is_loaded("modtest"))
        klog(0, "test_module_unload: unload failed: %s", out.get());
    sleep(LEFT_TIMER_NS);
    if (!holding)
    {
        klog(0, "test_module_unload: the call never ran: started %u", started ? 1U : 0U);
        ok = false;
    }

    if (started)
    {
        task->wait();
        if (stdlib::str_str(hold->output,
                "modtest hold: released by the stop, its command and provider closed before it") == nullptr)
        {
            klog(0, "test_module_unload: the call said: %s", hold->output);
            ok = false;
        }
    }

    if (task != nullptr)
        task->put();
    if (hold != nullptr)
    {
        hold->~HoldCall();
        mm::free(hold);
    }

    if (ok && listening && !check_listener_swept(out))
        ok = false;

    if (ok)
        klog(0, "test_module_unload: complete");
    return ok;
}

/* What the shell answers `cmd` with, into out, waited for until it has
   `want` in it -- up to polls milliseconds: whether it came */
static bool answers(const char* cmd, const char* want, stdlib::BufferPrinter& out, ulong polls)
{
    for (ulong i = 0; i < polls; i++)
    {
        out.reset();
        Cmd::dispatch(cmd, out);
        if (stdlib::str_str(out.get(), want) != nullptr)
            return true;
        sleep(consts::NANO_SECS_IN_MS);
    }
    return false;
}

/* modtest loaded and its `modtest ipc` run against kernel.echo, its names
   seen, and the echo seen pestering its mailbox: whether all of it went */
static bool check_ipc_loaded(stdlib::BufferPrinter& out)
{
    static const ulong PESTER_WAIT_POLLS = 2000;

    out.reset();
    Cmd::dispatch("modtest ipc", out);
    if (stdlib::str_str(out.get(), "modtest ipc: ok") == nullptr)
    {
        klog(0, "test_module_ipc: the module's side: %s", out.get());
        return false;
    }

    out.reset();
    Cmd::dispatch("ipc names modtest.", out);
    if (stdlib::str_str(out.get(), "modtest.ipc") == nullptr || stdlib::str_str(out.get(), "modtest.leaked") == nullptr)
    {
        klog(0, "test_module_ipc: the module's names: %s", out.get());
        return false;
    }

    /* The echo takes the request in its own time: traffic before the unload */
    if (!answers("ipc echo", "pestering 1,", out, PESTER_WAIT_POLLS))
    {
        klog(0, "test_module_ipc: the echo never pestered the module: %s", out.get());
        return false;
    }
    return true;
}

/* After the unload, under the echo's traffic: the module's mailbox closed by
   its drop and the echo's monitor told, the one it left open closed by the
   sweep, its names gone, and once the echo stops, nothing queued anywhere */
static bool check_ipc_unloaded(stdlib::BufferPrinter& out)
{
    static const ulong DOWN_WAIT_POLLS = 2000;

    if (!answers("ipc echo", "pestering 0,", out, DOWN_WAIT_POLLS) || stdlib::str_str(out.get(), "downs 0,") != nullptr)
    {
        klog(0, "test_module_ipc: the echo was never told the module's mailbox went: %s", out.get());
        return false;
    }

    out.reset();
    Cmd::dispatch("ipc names modtest.", out);
    if (stdlib::str_str(out.get(), "ipc: no names under modtest.") == nullptr)
    {
        klog(0, "test_module_ipc: names that outlived their module: %s", out.get());
        return false;
    }
    return true;
}

/* The message channel's module side, once there are tasks: the echo
   started, modtest loaded and its side run, the module unloaded under the
   echo's traffic, the echo stopped -- and the host's queues empty. */
bool test_module_ipc()
{
    static const ulong OUTPUT_SIZE = 2048;

    auto& modules = ModuleTable::get_instance();
    char output[OUTPUT_SIZE];
    stdlib::BufferPrinter out(output, sizeof(output));

    Cmd::dispatch("ipc echo start", out);
    if (stdlib::str_str(out.get(), "ipc echo: started") == nullptr)
    {
        klog(0, "test_module_ipc: %s", out.get());
        return false;
    }

    out.reset();
    stdlib::Error err = modules.load(nos_modtest_ko, nos_modtest_ko_size, "", out);
    bool ok = err.ok();
    if (!ok)
        klog(0, "test_module_ipc: load failed: %s", out.get());
    else
    {
        ok = check_ipc_loaded(out);
        out.reset();
        stdlib::Error unloaded = modules.unload("modtest", out);
        if (!unloaded.ok() || modules.is_loaded("modtest"))
        {
            klog(0, "test_module_ipc: unload failed: %s", out.get());
            ok = false;
        }
        else if (ok)
            ok = check_ipc_unloaded(out);
    }

    out.reset();
    Cmd::dispatch("ipc echo stop", out);
    if (stdlib::str_str(out.get(), "ipc echo: stopped") == nullptr)
    {
        klog(0, "test_module_ipc: %s", out.get());
        return false;
    }

    out.reset();
    Cmd::dispatch("ipc", out);
    if (ok && stdlib::str_str(out.get(), " 0 bytes queued") == nullptr)
    {
        klog(0, "test_module_ipc: bytes left queued: %s", out.get());
        return false;
    }
    if (ok)
        klog(0, "test_module_ipc: complete");
    return ok;
}

stdlib::Error test()
{
    stdlib::Error err;

    klog(0, "Self-test in progress, please wait...");

    err = test_allocator();
    if (!err.ok())
        return err;

    err = test_heap_usage();
    if (!err.ok())
        return err;

    err = test_alloc_sites();
    if (!err.ok())
        return err;

    err = test_mem_routines();
    if (!err.ok())
        return err;

    err = test_ring_buffer();
    if (!err.ok())
        return err;

    err = test_stack_trace();
    if (!err.ok())
        return err;

    err = test_parse_ulong();
    if (!err.ok())
        return err;

    err = test_parameters();
    if (!err.ok())
        return err;

    err = test_hex_char_to_nibble();
    if (!err.ok())
        return err;

    err = test_hex_decode();
    if (!err.ok())
        return err;

    err = test_next_token();
    if (!err.ok())
        return err;

    err = test_block_device_table();
    if (!err.ok())
        return err;

    err = test_contiguous_pages();
    if (!err.ok())
        return err;

    err = test_page_allocator();
    if (!err.ok())
        return err;

    err = test_zero_free();
    if (!err.ok())
        return err;

    err = test_mem_set();
    if (!err.ok())
        return err;

    err = test_mem_cpy();
    if (!err.ok())
        return err;

    err = test_mem_move();
    if (!err.ok())
        return err;

    err = test_mem_cmp();
    if (!err.ok())
        return err;

    err = test_str_len();
    if (!err.ok())
        return err;

    err = test_str_cmp();
    if (!err.ok())
        return err;

    err = test_str_str();
    if (!err.ok())
        return err;

    err = test_sn_printf();
    if (!err.ok())
        return err;

    err = test_net_wire();
    if (!err.ok())
        return err;

    err = test_grub_env();
    if (!err.ok())
        return err;

    err = test_line_editor();
    if (!err.ok())
        return err;

    err = test_vfs();
    if (!err.ok())
        return err;

    err = test_modules();
    if (!err.ok())
        return err;

    return err;
}

/* A spinlock disables preemption for as long as it is held, and
   preempt_can_block() -- asked by code that has to know whether it may wait --
   follows it: through nesting, through a try_lock that fails and one that
   succeeds, through the IRQ-saving SpinLock, and through both sides of a
   RawRwSpinLock. Run first thing in test_multi_tasking, which every CPU calls
   in task context with interrupts and preemption on. */
static bool test_spin_lock_preempt()
{
    if (!preempt_can_block())
    {
        klog(0, "test_spin_lock_preempt: cannot block before any lock is taken");
        return false;
    }

    RawSpinLock outer;
    RawSpinLock inner;

    outer.lock();
    bool under_outer = !preempt_can_block();
    bool inner_taken = inner.try_lock();
    bool outer_taken_twice = outer.try_lock();
    outer.unlock();
    bool under_inner = !preempt_can_block();
    if (inner_taken)
        inner.unlock();
    bool released = preempt_can_block();

    SpinLock spin;
    bool under_spin;
    {
        stdlib::AutoLock lock(spin);
        under_spin = !preempt_can_block();
    }
    bool released_spin = preempt_can_block();

    /* The reader-writer lock, both sides: two readers nested on it, their
       counts coming back through read_unlock(), then a writer alone. */
    RawRwSpinLock rw;
    Task* reader = rw.read_lock();
    Task* nested = rw.read_lock();
    bool under_reads = !preempt_can_block();
    rw.read_unlock(nested);
    bool under_read = !preempt_can_block();
    rw.read_unlock(reader);
    bool released_read = preempt_can_block();
    rw.write_lock();
    bool under_write = !preempt_can_block();
    rw.write_unlock();
    bool released_write = preempt_can_block();

    if (!under_outer || !inner_taken || outer_taken_twice || !under_inner ||
        !released || !under_spin || !released_spin || !under_reads ||
        !under_read || !released_read || !under_write || !released_write)
    {
        klog(0, "test_spin_lock_preempt: outer %lu inner %lu twice %lu "
            "under inner %lu released %lu spin %lu released spin %lu",
            (ulong)under_outer, (ulong)inner_taken, (ulong)outer_taken_twice,
            (ulong)under_inner, (ulong)released, (ulong)under_spin,
            (ulong)released_spin);
        klog(0, "test_spin_lock_preempt: rw reads %lu read %lu released %lu "
            "write %lu released %lu", (ulong)under_reads, (ulong)under_read,
            (ulong)released_read, (ulong)under_write, (ulong)released_write);
        return false;
    }

    return true;
}

void test_multi_tasking_task_func(void *ctx)
{
    (void)ctx;

    for (size_t i = 0; i < 2; i++)
    {
        auto& cpu = get_cpu();
        auto task = Task::get_current_task();
        klog(TEST_LL, "Hello from task 0x%p pid %lu cpu %lu", task, task->pid_, cpu.get_index());
        sleep(100 * consts::NANO_SECS_IN_MS);
    }
}

/* Two tasks taking turns through a pair of events, the way the block
   server's worker and whoever feeds it do: every signal has to reach a waiter
   that is blocked, or on its way to blocking, on whichever CPU it is. Both
   are tasks of their own. This runs on each CPU's boot task, which is its
   idle task -- the scheduler's last resort, which must never block: a task
   exiting on a CPU whose idle task is blocked has nowhere to switch to, and
   spins, interrupts off, until the TLB shootdown watchdog panics. */
struct EventPingPong
{
    Event ping;
    Event pong;
    Atomic rounds;
    Atomic failed;
    Atomic abort;   /* the pinger never started: the ponger goes home */
};

static const ulong EVENT_TEST_ROUNDS = 64;

static void event_test_pinger(void* ctx)
{
    auto* pp = static_cast<EventPingPong*>(ctx);
    for (ulong i = 0; i < EVENT_TEST_ROUNDS; i++)
    {
        pp->ping.signal();
        pp->pong.wait();
        if (pp->rounds.get() != (long)(i + 1))
            pp->failed.set(1);
    }
}

static void event_test_ponger(void* ctx)
{
    auto* pp = static_cast<EventPingPong*>(ctx);
    for (ulong i = 0; i < EVENT_TEST_ROUNDS; i++)
    {
        pp->ping.wait();
        if (pp->abort.get() != 0)
            return;
        pp->rounds.inc();
        pp->pong.signal();
    }
}

static bool test_event()
{
    /* A signal before the wait is kept, and the wait takes it without
       blocking -- so this much is safe on the idle task; two are one. */
    {
        Event early;
        early.signal();
        early.signal();
        early.wait();
    }

    auto* pp = mm::talloc<EventPingPong, TAG>();
    if (pp == nullptr)
        return false;

    Task* ponger = mm::talloc<Task, TAG>("evpong");
    Task* pinger = mm::talloc<Task, TAG>("evping");
    bool ok = ponger != nullptr && pinger != nullptr;

    /* Both on this CPU: a waiter moved elsewhere as it blocks is woken at its
       new CPU's next tick, not by the IPI -- 64 rounds of that would be most
       of a second of boot */
    if (ok)
    {
        ulong self = 1UL << CpuTable::get_instance().get_current_cpu_id();
        ponger->set_cpu_affinity(self);
        pinger->set_cpu_affinity(self);
    }

    if (ok && ponger->start(event_test_ponger, pp))
    {
        if (!pinger->start(event_test_pinger, pp))
        {
            ok = false;
            pp->abort.set(1);
            pp->ping.signal();
        }
        else
        {
            pinger->wait();
        }
        ponger->wait();
    }
    else
    {
        ok = false;
    }

    ok = ok && pp->failed.get() == 0 && pp->rounds.get() == (long)EVENT_TEST_ROUNDS;

    if (pinger != nullptr)
        pinger->put();
    if (ponger != nullptr)
        ponger->put();
    pp->~EventPingPong();
    mm::free(pp);

    if (!ok)
        klog(0, "test_event: the ping-pong lost count");
    return ok;
}

/* Event::wait_for, from a task of its own -- a CPU's idle task, which runs
   this, must never block: a wait that nobody signals runs out, and not
   before its time; one that is signalled comes back true. The phase is
   what lets the signal come only after the first wait has returned. */
struct WaitForProbe
{
    Event ev;
    Atomic phase;       /* 1 once the first wait has returned */
    Atomic timed_out;    /* it returned false, having waited its time */
    Atomic signalled;   /* the second came back true */
};

static const ulong WAIT_FOR_TEST_NS = 20 * consts::NANO_SECS_IN_MS;
static const ulong WAIT_FOR_LONG_NS = 10 * consts::NANO_SECS_IN_SEC;

static void wait_for_task(void* ctx)
{
    auto* p = static_cast<WaitForProbe*>(ctx);
    ulong start = get_boot_time().get_value();
    bool got = p->ev.wait_for(WAIT_FOR_TEST_NS);
    ulong waited = get_boot_time().get_value() - start;
    p->timed_out.set((!got && waited >= WAIT_FOR_TEST_NS) ? 1 : 0);
    p->phase.set(1);
    p->signalled.set(p->ev.wait_for(WAIT_FOR_LONG_NS) ? 1 : 0);
}

static bool test_event_wait_for()
{
    auto* p = mm::talloc<WaitForProbe, TAG>();
    Task* task = mm::talloc<Task, TAG>("waitfor");
    bool ok = (p != nullptr && task != nullptr);
    if (ok)
        task->set_cpu_affinity(1UL << CpuTable::get_instance().get_current_cpu_id());
    if (ok && task->start(wait_for_task, p))
    {
        while (p->phase.get() == 0)
            sleep(consts::NANO_SECS_IN_MS);
        p->ev.signal();
        task->wait();
        ok = (p->timed_out.get() == 1 && p->signalled.get() == 1);
        if (!ok)
            klog(0, "test_event_wait_for: timed out %lu, signalled %lu",
                (ulong)p->timed_out.get(), (ulong)p->signalled.get());
    }
    else
    {
        ok = false;
    }

    if (task != nullptr)
        task->put();
    if (p != nullptr)
    {
        p->~WaitForProbe();
        mm::free(p);
    }
    return ok;
}

/* Two tasks asleep on one CPU cost it next to nothing. sleep() used to poll
   -- a loop around schedule() that left the task runnable -- so two sleepers
   on one CPU handed it to each other without end, each running about half
   the time it slept, and the idle task never ran: two idle hypervisor
   guests, their vCPUs asleep until the next timer edge, kept a CPU of the
   AX41 busy that way. A blocked sleeper runs only to find that its time has
   come. Both on this CPU, each asleep SLEEP_TEST_ROUNDS times; what the two
   ran between them must be a small part of what they slept -- well under a
   poller's half, far over a blocked one's microseconds. */
static const ulong SLEEP_TEST_ROUNDS = 5;
static const ulong SLEEP_TEST_NS = 20 * consts::NANO_SECS_IN_MS;
static const ulong SLEEP_TEST_MAX_RUN_PERCENT = 10;
static const ulong SLEEP_TEST_PERCENT = 100;

static void sleep_test_task(void* ctx)
{
    (void)ctx;
    for (ulong i = 0; i < SLEEP_TEST_ROUNDS; i++)
        sleep(SLEEP_TEST_NS);
}

static bool test_sleep_blocks()
{
    Task* task[2] = { mm::talloc<Task, TAG>("sleep0"), mm::talloc<Task, TAG>("sleep1") };
    bool ok = (task[0] != nullptr && task[1] != nullptr);

    if (ok)
    {
        ulong self = 1UL << CpuTable::get_instance().get_current_cpu_id();
        task[0]->set_cpu_affinity(self);
        task[1]->set_cpu_affinity(self);
    }

    bool started[2] = { false, false };
    for (ulong i = 0; ok && i < stdlib::array_size(task); i++)
    {
        started[i] = task[i]->start(sleep_test_task, nullptr);
        ok = started[i];
    }

    ulong run_ns = 0;
    for (ulong i = 0; i < stdlib::array_size(task); i++)
    {
        if (!started[i])
            continue;

        task[i]->wait();
        /* Under the task's lock, which every update_runtime() is made under:
           the exited task may still be in its last switch on its CPU. */
        stdlib::AutoLock lock(task[i]->lock_);
        run_ns += task[i]->runtime_.get_value();
    }

    for (ulong i = 0; i < stdlib::array_size(task); i++)
    {
        if (task[i] != nullptr)
            task[i]->put();
    }

    if (!ok)
    {
        klog(0, "test_sleep_blocks: no tasks for the test");
        return false;
    }

    ulong slept_ns = stdlib::array_size(task) * SLEEP_TEST_ROUNDS * SLEEP_TEST_NS;
    if (run_ns * SLEEP_TEST_PERCENT > slept_ns * SLEEP_TEST_MAX_RUN_PERCENT)
    {
        klog(0, "test_sleep_blocks: two sleepers on one CPU ran %lu us of the %lu us they slept",
            run_ns / consts::NANO_SECS_IN_USEC, slept_ns / consts::NANO_SECS_IN_USEC);
        return false;
    }

    return true;
}

/* A task asleep until a deadline runs at it, not at the next tick after it:
   its CPU's timer is armed for the deadline (Cpu::local_timer_wake). Before,
   a halted guest's vCPU waited for its guest's next timer edge on the host's
   tick, and a guest's `usleep 1000` took 10 ms on the EX44. A task of its
   own on this CPU sleeps a millisecond at a time while this one -- the
   CPU's idle task, which must not block -- halts between interrupts rather
   than poll the clock, which would wake the sleeper on time whatever the
   timer did. At a tick each, the sleeps took ten milliseconds; armed for,
   1.0-1.1 under HVF and 1.1-2.4 under TCG. The median is what is held to
   well under a tick: a TCG host that stalls a vCPU for tens of
   milliseconds now and then puts a sleep or two that far out, and not half
   of them. */
static const ulong SLEEP_PROMPT_ROUNDS = 21;
static const ulong SLEEP_PROMPT_NS = consts::NANO_SECS_IN_MS;
static const ulong SLEEP_PROMPT_MAX_MEDIAN_NS = 5 * consts::NANO_SECS_IN_MS;

struct SleepPromptProbe
{
    Atomic done;
    ulong took[SLEEP_PROMPT_ROUNDS];
};

static void sleep_prompt_task(void* ctx)
{
    auto* p = static_cast<SleepPromptProbe*>(ctx);
    for (ulong i = 0; i < SLEEP_PROMPT_ROUNDS; i++)
    {
        ulong start = get_boot_time().get_value();
        sleep(SLEEP_PROMPT_NS);
        p->took[i] = get_boot_time().get_value() - start;
    }
    p->done.set(1);
}

static bool test_sleep_prompt()
{
    auto& cpu = CpuTable::get_instance().get_current_cpu();

    /* A timer that could not be calibrated leaves the broadcast tick in
       charge (x86), and every sleep on its grid */
    if (!cpu.local_timer_one_shot())
        return true;

    auto* p = mm::talloc<SleepPromptProbe, TAG>();
    Task* task = mm::talloc<Task, TAG>("sleepprompt");
    bool ok = (p != nullptr && task != nullptr);

    if (ok)
        task->set_cpu_affinity(1UL << cpu.get_index());
    if (ok && task->start(sleep_prompt_task, p))
    {
        while (p->done.get() == 0)
            cpu.idle();
        task->wait();

        /* Sorted in place, an insertion at a time: the middle one is the
           median of an odd count */
        for (ulong i = 1; i < SLEEP_PROMPT_ROUNDS; i++)
        {
            ulong v = p->took[i];
            ulong j = i;
            for (; j > 0 && p->took[j - 1] > v; j--)
                p->took[j] = p->took[j - 1];
            p->took[j] = v;
        }

        ulong median = p->took[SLEEP_PROMPT_ROUNDS / 2];
        ok = (median <= SLEEP_PROMPT_MAX_MEDIAN_NS);
        if (!ok)
            klog(0, "test_sleep_prompt: sleeps of %lu us took %lu us at the median, %lu to %lu",
                SLEEP_PROMPT_NS / consts::NANO_SECS_IN_USEC, median / consts::NANO_SECS_IN_USEC,
                p->took[0] / consts::NANO_SECS_IN_USEC, p->took[SLEEP_PROMPT_ROUNDS - 1] / consts::NANO_SECS_IN_USEC);
    }
    else
    {
        ok = false;
    }

    if (task != nullptr)
        task->put();
    if (p != nullptr)
    {
        p->~SleepPromptProbe();
        mm::free(p);
    }
    return ok;
}

/* A reschedule that finds preemption off is deferred, not dropped: made by
   the unlock that brings the count back to zero. Preempt() under a spinlock
   stands in for the IPI that used to be lost that way -- refused, it leaves
   preempt_pending_ up and is counted; the unlock makes it, which clears the
   flag whether or not another task was there to take the CPU. After
   test_spin_lock_preempt, in the same task context with interrupts and
   preemption on. */
static bool test_preempt_deferred()
{
    Task* task = Task::get_current_task();
    long deferred = get_preempt_deferred_count();

    SpinLock lock;
    bool pending_under;
    {
        stdlib::AutoLock guard(lock);
        preempt();
        pending_under = (task->preempt_pending_.get() != 0);
    }
    bool pending_after = (task->preempt_pending_.get() != 0);
    bool counted = (get_preempt_deferred_count() > deferred);

    if (!pending_under || pending_after || !counted)
    {
        klog(0, "test_preempt_deferred: pending under the lock %lu, after the unlock %lu, counted %lu",
            (ulong)pending_under, (ulong)pending_after, (ulong)counted);
        return false;
    }

    return true;
}

/* Task::locate, and Wait's report of a join that goes on: a task blocked is
   read off its stack, one on another CPU with interrupts off is named as
   that CPU's, one spinning there is found there by the IPI that asks -- on
   the probe the silent CPU answered late, which has to be reusable -- and a
   wait that outlasts its report interval is reported. Each is held to
   frames that name the function the task is in. A join that never ends is
   told from a quiet one only by these. */
struct LocateProbe
{
    Event ev;
    Atomic started;
    Atomic stop;
    Atomic done;
    /* The silent task gave up waiting for Stop, interrupts off */
    Atomic gave_up;
};

static const ulong LOCATE_TEST_TRIES = 200;
static const ulong LOCATE_TEST_ANSWER_NS = 100 * consts::NANO_SECS_IN_MS;
/* The longest the silent task keeps its CPU deaf, should the test never
   tell it to stop: a failure, and not a wedge */
static const ulong LOCATE_TEST_SILENT_MAX_NS = 2 * consts::NANO_SECS_IN_SEC;
static const ulong JOIN_TEST_REPORT_NS = 20 * consts::NANO_SECS_IN_MS;
static const ulong JOIN_TEST_NAP_NS = 50 * consts::NANO_SECS_IN_MS;

static void locate_park_task(void* ctx)
{
    auto* p = static_cast<LocateProbe*>(ctx);
    p->started.set(1);
    p->ev.wait();
    p->done.set(1);
}

/* Out of line, so that the frame named is its caller's, whichever
   instruction of the loop the IPI lands on: one landing in the loop of a
   function that has the loop itself is named by Pc alone */
static __attribute__((noinline)) void locate_spinning(LocateProbe* p)
{
    while (p->stop.get() == 0)
        cpu_relax();
}

static void locate_spin_task(void* ctx)
{
    auto* p = static_cast<LocateProbe*>(ctx);
    p->started.set(1);
    locate_spinning(p);
    p->done.set(1);
}

static void locate_silent_task(void* ctx)
{
    auto* p = static_cast<LocateProbe*>(ctx);
    ulong until = get_boot_time().get_value() + LOCATE_TEST_SILENT_MAX_NS;
    ulong flags = hal::irq_save();
    p->started.set(1);
    while (p->stop.get() == 0)
    {
        if (get_boot_time().get_value() >= until)
        {
            p->gave_up.set(1);
            break;
        }
        cpu_relax();
    }
    hal::irq_restore(flags);
    p->done.set(1);
}

static void join_nap_task(void* ctx)
{
    (void)ctx;
    sleep(JOIN_TEST_NAP_NS);
}

/* Whether where's frames name fn */
static bool locate_names(Task::Where& where, const char* fn)
{
    auto& symtab = SymbolTable::get_instance();
    char name[SymbolTable::DESCRIBE_MAX];
    for (size_t i = 0; i < where.count; i++)
    {
        if (symtab.describe_return(where.frames[i], name, sizeof(name)) &&
            stdlib::str_str(name, fn) != nullptr)
            return true;
    }
    return false;
}

static void locate_trace(const char* test, Task::Where& where)
{
    char what[2 * SymbolTable::DESCRIBE_MAX];
    where.describe(what, sizeof(what));
    klog(0, "%s: found %s, %lu frames", test, what, (ulong)where.count);
}

/* A task made, started with p and on cpu (~0UL: any), or nullptr */
static Task* locate_start(const char* name, Task::Func func, LocateProbe* p, ulong cpu)
{
    Task* task = mm::talloc<Task, TAG>(name);
    if (task == nullptr)
        return nullptr;

    if (cpu != ~0UL)
        task->set_cpu_affinity(1UL << cpu);
    if (!task->start(func, p))
    {
        task->put();
        return nullptr;
    }

    while (p->started.get() == 0)
        sleep(consts::NANO_SECS_IN_MS);
    return task;
}

static bool test_locate_blocked()
{
    auto* p = mm::talloc<LocateProbe, TAG>();
    if (p == nullptr)
        return false;

    bool ok = false;
    Task* task = locate_start("locate-park", locate_park_task, p, ~0UL);
    if (task != nullptr)
    {
        Task::Where where;
        for (ulong i = 0; i < LOCATE_TEST_TRIES && !ok; i++)
        {
            task->locate(where);
            ok = (where.what == Task::Where::Switched && where.blocked &&
                  !where.asleep && locate_names(where, "locate_park_task"));
            if (!ok)
                sleep(consts::NANO_SECS_IN_MS);
        }
        if (!ok)
            locate_trace("test_locate_blocked", where);

        p->ev.signal();
        task->wait();
        task->put();
    }

    p->~LocateProbe();
    mm::free(p);
    return ok;
}

/* The silent CPU first, then the spinning task on it: the probe left
   unanswered there is the one the second has to find answered and reuse */
static bool test_locate_on_cpu(ulong cpu)
{
    auto* p = mm::talloc<LocateProbe, TAG>();
    if (p == nullptr)
        return false;

    bool ok = false;
    Task* task = locate_start("locate-silent", locate_silent_task, p, cpu);
    if (task != nullptr)
    {
        Task::Where where;
        task->locate(where, LOCATE_TEST_ANSWER_NS);
        ok = (where.what == Task::Where::Silent && (where.silent_cpus & (1UL << cpu)));
        if (!ok)
            locate_trace("test_locate_on_cpu silent", where);

        p->stop.set(1);
        task->wait();
        task->put();
        if (p->gave_up.get() != 0)
        {
            klog(0, "test_locate_on_cpu: the silent task was never told to stop");
            ok = false;
        }
    }

    p->~LocateProbe();
    mm::free(p);
    if (!ok)
        return false;

    p = mm::talloc<LocateProbe, TAG>();
    if (p == nullptr)
        return false;

    ok = false;
    task = locate_start("locate-spin", locate_spin_task, p, cpu);
    if (task != nullptr)
    {
        Task::Where where;
        for (ulong i = 0; i < LOCATE_TEST_TRIES && !ok; i++)
        {
            task->locate(where);
            ok = (where.what == Task::Where::Running && where.cpu == cpu &&
                  where.pc != 0 && locate_names(where, "locate_spin_task"));
            if (!ok)
                sleep(consts::NANO_SECS_IN_MS);
        }
        if (!ok)
            locate_trace("test_locate_on_cpu running", where);

        p->stop.set(1);
        task->wait();
        task->put();
    }

    p->~LocateProbe();
    mm::free(p);
    return ok;
}

static bool test_join_report()
{
    Task* task = mm::talloc<Task, TAG>("join-nap");
    if (task == nullptr)
        return false;

    long before = Task::get_join_report_count();
    bool ok = task->start(join_nap_task, nullptr);
    if (ok)
    {
        task->wait(JOIN_TEST_REPORT_NS);
        ok = (Task::get_join_report_count() > before);
        if (!ok)
            klog(0, "test_join_report: a wait of %lu ms went unreported",
                JOIN_TEST_NAP_NS / consts::NANO_SECS_IN_MS);
    }
    task->put();
    return ok;
}

bool test_task_locate()
{
    if (!test_locate_blocked())
        return false;

    /* Another CPU, should there be one: the highest, the one least likely
       to be the BSP's */
    auto& cpus = CpuTable::get_instance();
    ulong self = cpus.get_current_cpu_id();
    ulong mask = cpus.get_running_cpus();
    ulong other = ~0UL;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((mask & (1UL << i)) && i != self)
            other = i;
    }

    if (other != ~0UL && !test_locate_on_cpu(other))
        return false;

    return test_join_report();
}

/* The boot runs in its CPU's idle task, and the idle task is the last resort
   only while it idles (Task::set_idling): running the boot, it takes its turn
   like any other task. A task pinned to this CPU that never blocks, only
   yields, and a sleep of a millisecond here: it must come back in about that,
   and not when the other task gives up. With the idle task the last resort
   whatever it ran, the boot came back only then -- the ring selftest's
   producer, started on the boot CPU, kept the boot from starting its
   consumer for the thirty seconds it waited for one, one boot in a few
   hundred under HVF. */
static const ulong TURN_SPIN_MAX_NS = 2 * consts::NANO_SECS_IN_SEC;
static const ulong TURN_SLEEP_NS = consts::NANO_SECS_IN_MS;
static const ulong TURN_MAX_NS = 200 * consts::NANO_SECS_IN_MS;

struct TurnProbe
{
    Atomic stop;
};

static void turn_spin_task(void* ctx)
{
    auto* p = static_cast<TurnProbe*>(ctx);
    ulong until = get_boot_time().get_value() + TURN_SPIN_MAX_NS;
    while (p->stop.get() == 0 && get_boot_time().get_value() < until)
        yield_to_runnable();
}

bool test_idle_takes_turns()
{
    Task* self = Task::get_current_task();
    if (!self->is_idle())
        return true;

    auto* p = mm::talloc<TurnProbe, TAG>();
    Task* task = mm::talloc<Task, TAG>("turn-spin");
    bool ok = (p != nullptr && task != nullptr);
    if (ok)
        task->set_cpu_affinity(1UL << CpuTable::get_instance().get_current_cpu_id());
    if (ok && task->start(turn_spin_task, p))
    {
        ulong start = get_boot_time().get_value();
        sleep(TURN_SLEEP_NS);
        ulong took = get_boot_time().get_value() - start;

        p->stop.set(1);
        task->wait();

        ok = (took <= TURN_MAX_NS);
        if (!ok)
            klog(0, "test_idle_takes_turns: a sleep of %lu us in the idle task took %lu us beside a task that yields",
                TURN_SLEEP_NS / consts::NANO_SECS_IN_USEC, took / consts::NANO_SECS_IN_USEC);
    }
    else
    {
        ok = false;
    }

    if (task != nullptr)
        task->put();
    if (p != nullptr)
    {
        p->~TurnProbe();
        mm::free(p);
    }
    return ok;
}

/* EEVDF's two promises, held at every boot on a CPU that is not the boot's:
   a task that wakes beside tasks that never block runs at once -- it comes
   back owed what it waited for the last time, and its deadline is the
   soonest -- and tasks that never block share the CPU by their weights.
   The round robin before failed both: a woken task waited a tick behind
   each task ahead of it (8 ms at the median behind two), and two spinners
   of nice 0 and 5 shared it half and half. And a lag reckoned wrong once --
   the waiter's, from a V that left out the task about to run -- left a
   waiter beside one spinner owed nothing at any wakeup, to wait out what
   was left of the spinner's slice: the bound for one spinner is there for
   that. The signals come at random points of the spinners' slices, which
   it takes: sent at a fixed rhythm, they settled just before a slice ended,
   and the waiter owed nothing waited 153us at the median, under the bound,
   where at random points it waits half a slice. Measured: under HVF a
   wakeup waits <16us at the median, under TCG <64us; the shares are 3.0
   for 3.06. */
static const ulong EEVDF_SAMPLES = 101;
static const ulong EEVDF_ONE_MEDIAN_NS = 500 * consts::NANO_SECS_IN_USEC;
static const ulong EEVDF_TWO_MEDIAN_NS = consts::NANO_SECS_IN_MS;
static const ulong EEVDF_ANSWER_NS = consts::NANO_SECS_IN_SEC;
/* Between signals: EEVDF_GAP_NS, and up to a default slice more drawn from a
   fixed seed */
static const ulong EEVDF_GAP_NS = consts::NANO_SECS_IN_MS;
static const u64 EEVDF_SEED = 0x9E3779B97F4A7C15UL;
static const ulong EEVDF_SHARE_NS = 400 * consts::NANO_SECS_IN_MS;
static const long EEVDF_LIGHT_NICE = 5;
/* nice 0 against nice 5 is 1024 against 335: 3.06, held to [2.4, 3.9] in
   tenths */
static const ulong EEVDF_SHARE_MIN_TENTHS = 24;
static const ulong EEVDF_SHARE_MAX_TENTHS = 39;

struct EevdfProbe
{
    Event ev;
    Atomic stop;
    Atomic sent_at;
    Atomic answered;
    /* The waiter's, read after it has exited */
    ulong took[EEVDF_SAMPLES];
    ulong count;
};

static void eevdf_spin(void* ctx)
{
    auto* p = static_cast<EevdfProbe*>(ctx);
    while (p->stop.get() == 0)
        cpu_relax();
}

static void eevdf_waiter(void* ctx)
{
    auto* p = static_cast<EevdfProbe*>(ctx);
    while (p->stop.get() == 0)
    {
        if (!p->ev.wait_for(100 * consts::NANO_SECS_IN_MS))
            continue;

        ulong now = get_boot_time().get_value();
        ulong sent = (ulong)p->sent_at.get();
        if (sent != 0 && now >= sent && p->count < EEVDF_SAMPLES)
            p->took[p->count++] = now - sent;
        p->sent_at.set(0);
        p->answered.set(1);
    }
}

static Task* eevdf_start(const char* name, ulong cpu, Task::Func func, void* ctx, long nice)
{
    Task* task = mm::talloc<Task, TAG>(name);
    if (task == nullptr)
        return nullptr;

    task->set_cpu_affinity(1UL << cpu);
    set_task_nice(task, nice);
    if (!task->start(func, ctx))
    {
        task->put();
        return nullptr;
    }
    return task;
}

static ulong eevdf_runtime(Task* task)
{
    stdlib::AutoLock lock(task->lock_);
    return task->runtime_.get_value();
}

/* The median wait of a waiter woken beside spinners on cpu, or ~0UL */
static ulong eevdf_median_wait(ulong cpu, ulong spinners)
{
    auto* p = mm::talloc<EevdfProbe, TAG>();
    if (p == nullptr)
        return ~0UL;
    p->count = 0;

    Task* tasks[3] = {};
    bool ok = (spinners + 1 <= stdlib::array_size(tasks));
    for (ulong i = 0; ok && i <= spinners; i++)
    {
        tasks[i] = eevdf_start((i == 0) ? "eevdf-wait" : "eevdf-spin", cpu,
            (i == 0) ? eevdf_waiter : eevdf_spin, p, 0);
        ok = (tasks[i] != nullptr);
    }

    u64 x = EEVDF_SEED;
    for (ulong sample = 0; ok && sample < EEVDF_SAMPLES; sample++)
    {
        p->answered.set(0);
        p->sent_at.set((long)get_boot_time().get_value());
        p->ev.signal();

        ulong give_up = get_boot_time().get_value() + EEVDF_ANSWER_NS;
        while (p->answered.get() == 0 && get_boot_time().get_value() < give_up)
            sleep(consts::NANO_SECS_IN_MS);

        /* xorshift64 */
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        sleep(EEVDF_GAP_NS + (ulong)(x % SchedEntity::DEFAULT_SLICE_NS));
    }

    p->stop.set(1);
    p->ev.signal();
    for (ulong i = 0; i <= spinners && i < stdlib::array_size(tasks); i++)
    {
        if (tasks[i] == nullptr)
            continue;
        tasks[i]->wait();
        tasks[i]->put();
    }

    ulong median = ~0UL;
    if (ok && p->count == EEVDF_SAMPLES)
    {
        for (ulong i = 1; i < p->count; i++)
        {
            ulong v = p->took[i];
            ulong j = i;
            for (; j > 0 && p->took[j - 1] > v; j--)
                p->took[j] = p->took[j - 1];
            p->took[j] = v;
        }
        median = p->took[p->count / 2];
    }

    p->~EevdfProbe();
    mm::free(p);
    return median;
}

/* What two spinners of nice 0 and EEVDF_LIGHT_NICE ran on cpu over a window:
   the heavy's runtime over the light's, in tenths, or ~0UL */
static ulong eevdf_share_tenths(ulong cpu)
{
    auto* p = mm::talloc<EevdfProbe, TAG>();
    if (p == nullptr)
        return ~0UL;

    Task* heavy = eevdf_start("eevdf-heavy", cpu, eevdf_spin, p, 0);
    Task* light = eevdf_start("eevdf-light", cpu, eevdf_spin, p, EEVDF_LIGHT_NICE);
    ulong tenths = ~0UL;
    if (heavy != nullptr && light != nullptr)
    {
        /* Both running before the window opens */
        sleep(20 * consts::NANO_SECS_IN_MS);
        ulong h0 = eevdf_runtime(heavy), l0 = eevdf_runtime(light);
        sleep(EEVDF_SHARE_NS);
        ulong h1 = eevdf_runtime(heavy), l1 = eevdf_runtime(light);
        ulong light_ran = l1 - l0;
        if (light_ran != 0)
            tenths = ((h1 - h0) * 10) / light_ran;
    }

    p->stop.set(1);
    Task* both[2] = { heavy, light };
    for (auto* task : both)
    {
        if (task == nullptr)
            continue;
        task->wait();
        task->put();
    }
    p->~EevdfProbe();
    mm::free(p);
    return tenths;
}

bool test_eevdf()
{
    /* The highest CPU that is not this one: the boot's own runs this */
    auto& cpus = CpuTable::get_instance();
    ulong self = cpus.get_current_cpu_id();
    ulong mask = cpus.get_running_cpus();
    ulong cpu = ~0UL;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((mask & (1UL << i)) && i != self)
            cpu = i;
    }
    if (cpu == ~0UL)
        return true;

    ulong one = eevdf_median_wait(cpu, 1);
    ulong two = eevdf_median_wait(cpu, 2);
    ulong share = eevdf_share_tenths(cpu);

    bool ok = (one <= EEVDF_ONE_MEDIAN_NS && two <= EEVDF_TWO_MEDIAN_NS &&
               share >= EEVDF_SHARE_MIN_TENTHS && share <= EEVDF_SHARE_MAX_TENTHS);
    klog(0, "test_eevdf: on cpu %lu a wakeup waits %lu us beside one spinner, %lu us beside two, "
        "at the median; nice 0 has %lu.%lu times nice %ld's CPU",
        cpu, (one == ~0UL) ? 0 : one / consts::NANO_SECS_IN_USEC, (two == ~0UL) ? 0 : two / consts::NANO_SECS_IN_USEC,
        (share == ~0UL) ? 0 : share / 10, (share == ~0UL) ? 0 : share % 10, EEVDF_LIGHT_NICE);
    return ok;
}

/* Wakeups that cross CPUs and land in every window a task's run state has
   (Task::run_state_): a ring of tasks, two a CPU, each pinned to its own,
   passing one token round -- each waits on an event of its own and signals
   the next's, which is always on another CPU. Half wait with a deadline,
   half without; every STORM_NAP_EVERY passes the holder sleeps before it
   passes the token on, so a sleeper's deadline races the signals that come
   for it; and a yielder on every CPU keeps a run list there to wait behind.
   One lost wakeup stops the token for good -- the untimed waits have no
   deadline to rescue them -- and the test fails after STORM_STALL_NS without
   a pass, naming the task the token was sent to and where it is. It does
   not clean up then: a task that cannot be woken cannot be joined, and the
   boot panics on the failure. */
static const ulong STORM_MAX_TASKS = 32;
static const ulong STORM_HOPS = 3000;
static const ulong STORM_NAP_EVERY = 64;
static const ulong STORM_NAP_NS = consts::NANO_SECS_IN_MS;
static const ulong STORM_WAIT_NS = 50 * consts::NANO_SECS_IN_MS;
static const ulong STORM_STALL_NS = 2 * consts::NANO_SECS_IN_SEC;
static const ulong STORM_MAX_NS = 60 * consts::NANO_SECS_IN_SEC;
/* How long a yielder spins between its yields */
static const ulong STORM_YIELD_SPINS = 200;

struct StormProbe;

struct StormSlot
{
    Event ev;
    StormProbe* probe;
    ulong index;
    Task* worker;
};

struct StormProbe
{
    StormSlot slots[STORM_MAX_TASKS];
    Task* yielders[MAX_CPUS];
    ulong count;
    Atomic hops;
    /* The slot the token was sent to last */
    Atomic holder;
    Atomic stop;
};

static void storm_task_func(void* ctx)
{
    auto* slot = static_cast<StormSlot*>(ctx);
    StormProbe* p = slot->probe;
    bool timed = (slot->index % 2) == 0;

    while (p->stop.get() == 0)
    {
        if (timed)
        {
            if (!slot->ev.wait_for(STORM_WAIT_NS))
                continue;
        }
        else
        {
            slot->ev.wait();
        }

        if (p->stop.get() != 0)
            break;

        /* The token is this task's alone until it signals the next */
        long hop = p->hops.get() + 1;
        p->hops.set(hop);
        if ((ulong)hop % STORM_NAP_EVERY == 0)
            sleep(STORM_NAP_NS);

        ulong next = (slot->index + 1) % p->count;
        p->holder.set((long)next);
        p->slots[next].ev.signal();
    }
}

static void storm_yield_func(void* ctx)
{
    auto* p = static_cast<StormProbe*>(ctx);
    while (p->stop.get() == 0)
    {
        for (ulong i = 0; i < STORM_YIELD_SPINS; i++)
            cpu_relax();
        yield_to_runnable();
    }
}

static Task* storm_start(const char* name, ulong cpu, Task::Func func, void* ctx)
{
    Task* task = mm::talloc<Task, TAG>(name);
    if (task == nullptr)
        return nullptr;

    task->set_cpu_affinity(1UL << cpu);
    if (!task->start(func, ctx))
    {
        task->put();
        return nullptr;
    }
    return task;
}

bool test_wake_storm()
{
    auto& cpus = CpuTable::get_instance();
    ulong mask = cpus.get_running_cpus();
    ulong cpu_list[MAX_CPUS];
    ulong cpu_count = 0;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (mask & (1UL << i))
            cpu_list[cpu_count++] = i;
    }
    if (cpu_count == 0)
        return false;

    auto* p = mm::talloc<StormProbe, TAG>();
    if (p == nullptr)
    {
        klog(0, "test_wake_storm: no memory");
        return false;
    }

    p->count = stdlib::min(2 * cpu_count, STORM_MAX_TASKS);
    for (ulong i = 0; i < MAX_CPUS; i++)
        p->yielders[i] = nullptr;

    bool ok = true;
    for (ulong i = 0; i < p->count; i++)
    {
        StormSlot& slot = p->slots[i];
        slot.probe = p;
        slot.index = i;
        char name[TASK_NAME_LEN];
        stdlib::sn_printf(name, sizeof(name), "storm%lu", i);
        slot.worker = ok ? storm_start(name, cpu_list[i % cpu_count], storm_task_func, &slot) : nullptr;
        ok = ok && (slot.worker != nullptr);
    }
    /* Not on this CPU: this runs in its idle task, the last resort, which a
       task that can always run would keep off it for as long as it ran */
    ulong self = cpus.get_current_cpu_id();
    for (ulong i = 0; ok && i < cpu_count; i++)
    {
        if (cpu_list[i] == self)
            continue;

        char name[TASK_NAME_LEN];
        stdlib::sn_printf(name, sizeof(name), "storm-yield%lu", cpu_list[i]);
        p->yielders[i] = storm_start(name, cpu_list[i], storm_yield_func, p);
        ok = (p->yielders[i] != nullptr);
    }

    if (ok)
    {
        ulong start = get_boot_time().get_value();
        ulong last_hops = 0;
        ulong last_at = start;

        p->holder.set(0);
        p->slots[0].ev.signal();

        while ((ulong)p->hops.get() < STORM_HOPS)
        {
            sleep(consts::NANO_SECS_IN_MS);
            ulong now = get_boot_time().get_value();
            ulong hops = (ulong)p->hops.get();
            if (hops != last_hops)
            {
                last_hops = hops;
                last_at = now;
                continue;
            }

            if (now - last_at < STORM_STALL_NS && now - start < STORM_MAX_NS)
                continue;

            ulong holder = (ulong)p->holder.get();
            Task::Where where;
            p->slots[holder].worker->locate(where);
            char what[2 * SymbolTable::DESCRIBE_MAX];
            where.describe(what, sizeof(what));
            klog(0, "test_wake_storm: the token stopped after %lu of %lu passes, %lu ms without one: sent to %s, which is %s",
                hops, STORM_HOPS, (now - last_at) / consts::NANO_SECS_IN_MS,
                p->slots[holder].worker->get_name(), what);
            return false;
        }

        klog(0, "test_wake_storm: %lu passes among %lu tasks on %lu cpus in %lu ms",
            (ulong)p->hops.get(), p->count, cpu_count,
            (get_boot_time().get_value() - start) / consts::NANO_SECS_IN_MS);
    }
    else
    {
        klog(0, "test_wake_storm: could not start its tasks");
    }

    p->stop.set(1);
    for (ulong i = 0; i < p->count; i++)
    {
        if (p->slots[i].worker == nullptr)
            continue;
        p->slots[i].ev.signal();
        p->slots[i].worker->wait();
        p->slots[i].worker->put();
    }
    for (ulong i = 0; i < cpu_count; i++)
    {
        if (p->yielders[i] == nullptr)
            continue;
        p->yielders[i]->wait();
        p->yielders[i]->put();
    }

    p->~StormProbe();
    mm::free(p);
    return ok;
}

/* ---- a wait()'s task let go of while signal()s are at its event ---- */

/* Waiters on one event, each freed the moment it returns, while tasks on
   other CPUs signal it -- each signal() holding the waiter it found for
   LET_GO_HOLD_NS inside its window (Event::signal_holding). A waiter is
   asleep on the event before the signals are let go, so they find it; on
   even rounds its deadline comes inside a hold, so that its way out of
   wait_for meets a signal() still in the window, and on odd ones a hold's
   unblock ends its wait. Every wait() must let go of its task only once no
   signal() is in the window (Event::let_go): one still holding the task
   after it was freed would find its magic dead (TASK_DEAD_MAGIC) -- nothing
   allocates a task again for two holds after the free -- and fail the boot
   there. The waiters run on a CPU of their own, not this one: an exited
   task's last reference is its queue's, let go of by its CPU's idle loop
   (TaskQueue::reap_exited), and this CPU's idle task is the one running
   the test. */
struct LetGoProbe
{
    Event ev;
    Atomic stop;
    /* The signallers signal while it is 1 */
    Atomic armed;
    /* The round under way: its waiter's deadline is chosen by it */
    Atomic round;
    Atomic woken;       /* waits a signal ended */
    Atomic timed_out;   /* waits their deadline ended */
};

static const ulong LET_GO_ROUNDS = 16;
static const ulong LET_GO_HOLD_NS = 2 * consts::NANO_SECS_IN_MS;
/* A waiter's deadline on even rounds -- inside a hold, with room for the
   signals to be let go after it fell asleep -- and on odd ones */
static const ulong LET_GO_SHORT_NS = 500 * consts::NANO_SECS_IN_USEC;
static const ulong LET_GO_LONG_NS = 50 * consts::NANO_SECS_IN_MS;
static const ulong LET_GO_SIGNALLERS_MAX = 3;

static void let_go_signaller(void* ctx)
{
    auto* p = static_cast<LetGoProbe*>(ctx);
    while (p->stop.get() == 0)
    {
        if (p->armed.get() == 0)
        {
            cpu_relax();
            continue;
        }
        p->ev.signal_holding(LET_GO_HOLD_NS);
    }
}

static void let_go_waiter(void* ctx)
{
    auto* p = static_cast<LetGoProbe*>(ctx);
    /* A signal left from the round before taken first, so that the wait
       sleeps until this round's */
    p->ev.poll_for(0);
    ulong deadline = (p->round.get() % 2 == 0) ? LET_GO_SHORT_NS : LET_GO_LONG_NS;
    if (p->ev.wait_for(deadline))
        p->woken.inc();
    else
        p->timed_out.inc();
}

bool test_event_let_go()
{
    auto& cpus = CpuTable::get_instance();
    ulong self = cpus.get_current_cpu_id();
    ulong others = cpus.get_running_cpus() & ~(1UL << self);
    ulong home = MAX_CPUS;
    for (ulong cpu = 0; cpu < MAX_CPUS && home == MAX_CPUS; cpu++)
    {
        if (others & (1UL << cpu))
            home = cpu;
    }
    if (home != MAX_CPUS)
        others &= ~(1UL << home);
    if (others == 0)
    {
        klog(0, "test_event_let_go: fewer than three CPUs -- this one, the waiters' and a signaller's: not run");
        return true;
    }

    auto* p = mm::talloc<LetGoProbe, TAG>();
    if (p == nullptr)
    {
        klog(0, "test_event_let_go: no memory");
        return false;
    }

    Task* signallers[LET_GO_SIGNALLERS_MAX] = {};
    ulong count = 0;
    bool ok = true;
    for (ulong cpu = 0; ok && cpu < MAX_CPUS && count < LET_GO_SIGNALLERS_MAX; cpu++)
    {
        if (!(others & (1UL << cpu)))
            continue;

        char name[TASK_NAME_LEN];
        stdlib::sn_printf(name, sizeof(name), "letgo-sig%lu", cpu);
        signallers[count] = storm_start(name, cpu, let_go_signaller, p);
        ok = (signallers[count] != nullptr);
        if (ok)
            count++;
    }

    ulong met_before = Event::let_go_waits();
    ulong rounds = 0;
    for (; ok && rounds < LET_GO_ROUNDS; rounds++)
    {
        p->round.set((long)rounds);
        Task* waiter = storm_start("letgo-wait", home, let_go_waiter, p);
        if (waiter == nullptr)
        {
            ok = false;
            break;
        }
        /* Asleep on the event -- or gone, a stray signal having ended its
           wait at once -- before the signals are let go */
        while (!waiter->is_blocked() && waiter->state_.get() != Task::STATE_EXITED)
            schedule();
        p->armed.set(1);
        while (waiter->state_.get() != Task::STATE_EXITED)
            schedule();
        /* Let go of the moment it is gone, the signallers still at it: freed
           with its queue's reference, which its CPU's idle loop lets go of */
        waiter->put();
        p->armed.set(0);
        /* Two holds: whatever signal() is still in its window is done before
           the next task is allocated -- in the memory of this one, perhaps */
        sleep(2 * LET_GO_HOLD_NS);
    }

    p->stop.set(1);
    for (ulong i = 0; i < count; i++)
    {
        signallers[i]->wait();
        signallers[i]->put();
    }

    ulong met = Event::let_go_waits() - met_before;
    if (!ok)
    {
        klog(0, "test_event_let_go: could not start its tasks");
    }
    else if (met < rounds / 4)
    {
        /* The even rounds' waits each end inside a hold: half of them at the
           least, or the window was not what was tested */
        klog(0, "test_event_let_go: %lu of %lu waits met a signal() in its window, too few to have tested it",
             met, rounds);
        ok = false;
    }
    else
    {
        klog(0, "test_event_let_go: %lu waiters freed as they returned under %lu signallers -- %lu woken, %lu timed "
             "out -- %lu of them let go only once a signal() left its window", rounds, count,
             (ulong)p->woken.get(), (ulong)p->timed_out.get(), met);
    }

    p->~LetGoProbe();
    mm::free(p);
    return ok;
}

/* ---- affinity changed under a task (set_task_affinity) ---- */

/* How long a move may take before it is a failure -- a spinner's is the
   IPI and the decision it ends in, a sleeper's its next wakeup, a
   millisecond -- and how long the task is watched staying where it went */
static const ulong AFFINITY_MOVE_NS = 100 * consts::NANO_SECS_IN_MS;
static const ulong AFFINITY_STAY_NS = 20 * consts::NANO_SECS_IN_MS;

struct AffinityProbe
{
    /* The CPU the task last found itself on, plus one: 0 before it ran */
    Atomic cpu;
    Atomic stop;
    /* A CPU watched, plus one -- 0 for none -- and how many of the task's
       looks found it there: what a sleeper did on the CPU it was taken off */
    Atomic watched;
    Atomic looks_there;
    /* Sleeps a millisecond between its looks, where the other spins */
    bool sleeps;
};

static void affinity_task_func(void* ctx)
{
    auto* p = static_cast<AffinityProbe*>(ctx);
    while (p->stop.get() == 0)
    {
        long cpu = (long)CpuTable::get_instance().get_current_cpu_id();
        p->cpu.set(cpu + 1);
        if (p->watched.get() == cpu + 1)
            p->looks_there.inc();
        if (p->sleeps)
            sleep(consts::NANO_SECS_IN_MS);
        else
            cpu_relax();
    }
}

/* Until the task has been seen on `cpu` and is on its queue, or `ns`: when,
   in ns from `from`, or ~0 */
static ulong affinity_wait_on(Task* task, AffinityProbe* p, ulong cpu, ulong from, ulong ns)
{
    for (;;)
    {
        ulong now = get_boot_time().get_value();
        ulong home = MAX_CPUS;
        if ((ulong)p->cpu.get() == cpu + 1 && task_home_cpu(task, home) && home == cpu)
            return now - from;
        if (now - from > ns)
            return ~0UL;
        sleep(consts::NANO_SECS_IN_MS / 10);
    }
}

/* A spinning task and a sleeping one, each started on CPU a and given CPU b
   for its affinity: each found on b, and staying there -- the spinner taken
   off a by the decision its CPU is interrupted for, the sleeper at its next
   wakeup, which used to be it running a moment there between two sleeps and
   never moving -- and refused for an idle task, a task bound to its CPU and
   a mask of no CPU that runs, nothing changed of any. Three CPUs or more --
   neither a nor b the BSP's, which runs this in its idle task -- else said
   and skipped. */
bool test_affinity()
{
    auto& cpus = CpuTable::get_instance();
    ulong running = cpus.get_running_cpus();
    ulong self = cpus.get_current_cpu_id();
    ulong a = MAX_CPUS;
    ulong b = MAX_CPUS;
    for (ulong i = 0; i < MAX_CPUS && b == MAX_CPUS; i++)
    {
        if (!(running & (1UL << i)) || i == self)
            continue;
        if (a == MAX_CPUS)
            a = i;
        else
            b = i;
    }
    if (b == MAX_CPUS)
    {
        klog(0, "test_affinity: fewer than three cpus, skipped");
        return true;
    }

    /* An idle task: refused, as it was */
    Task* idle = cpus.get_cpu(a).get_idle_task();
    ulong idle_was = idle->get_cpu_affinity();
    if (set_task_affinity(idle, 1UL << b) != AffinityResult::Pinned || idle->get_cpu_affinity() != idle_was)
    {
        klog(0, "test_affinity: cpu %lu's idle task taken another affinity", a);
        return false;
    }

    bool ok = true;
    for (ulong sleeps = 0; ok && sleeps < 2; sleeps++)
    {
        auto* p = mm::talloc<AffinityProbe, TAG>();
        if (p == nullptr)
        {
            klog(0, "test_affinity: no memory");
            return false;
        }
        p->cpu.set(0);
        p->stop.set(0);
        p->watched.set(0);
        p->looks_there.set(0);
        p->sleeps = (sleeps != 0);
        const char* what = p->sleeps ? "sleeping" : "spinning";
        Task* task = mm::talloc<Task, TAG>(p->sleeps ? "affinity/sleep" : "affinity/spin");
        if (task != nullptr)
        {
            task->set_cpu_affinity(1UL << a);
            if (!task->start(affinity_task_func, p))
            {
                task->put();
                task = nullptr;
            }
        }
        if (task == nullptr)
        {
            klog(0, "test_affinity: could not start a task");
            p->~AffinityProbe();
            mm::free(p);
            return false;
        }

        ulong start = get_boot_time().get_value();
        if (affinity_wait_on(task, p, a, start, AFFINITY_MOVE_NS) == ~0UL)
        {
            klog(0, "test_affinity: the %s task never ran on cpu %lu", what, a);
            ok = false;
        }

        /* A mask of no CPU that runs, where there is one: refused, as it was */
        if (ok && ~running != 0)
        {
            ulong none = ~running & (0UL - ~running);
            if (set_task_affinity(task, none) != AffinityResult::NoCpu || task->get_cpu_affinity() != (1UL << a))
            {
                klog(0, "test_affinity: the %s task taken a mask of no cpu that runs, 0x%lX", what, none);
                ok = false;
            }
        }

        ulong moved = 0;
        if (ok)
        {
            p->looks_there.set(0);
            p->watched.set((long)a + 1);
            ulong from = get_boot_time().get_value();
            if (set_task_affinity(task, 1UL << b) != AffinityResult::Set)
            {
                klog(0, "test_affinity: the %s task refused cpu %lu", what, b);
                ok = false;
            }
            else
            {
                moved = affinity_wait_on(task, p, b, from, AFFINITY_MOVE_NS);
                if (moved == ~0UL)
                {
                    ulong home = MAX_CPUS;
                    bool on = task_home_cpu(task, home);
                    klog(0, "test_affinity: the %s task not on cpu %lu %lu ms after it was given it: on cpu %lu, "
                        "last seen on %ld", what, b, AFFINITY_MOVE_NS / consts::NANO_SECS_IN_MS, on ? home : MAX_CPUS,
                        p->cpu.get() - 1);
                    ok = false;
                }
            }
        }

        /* And stays there: never seen on another CPU while watched */
        for (ulong from = get_boot_time().get_value(); ok && get_boot_time().get_value() - from < AFFINITY_STAY_NS;)
        {
            if ((ulong)p->cpu.get() != b + 1)
            {
                klog(0, "test_affinity: the %s task back on cpu %ld after it moved to cpu %lu", what,
                    p->cpu.get() - 1, b);
                ok = false;
            }
            sleep(consts::NANO_SECS_IN_MS / 10);
        }
        /* A sleeper moves at its next wakeup (TaskQueue::leave_if_outbound):
           seen on the old CPU after the watch began twice at most -- a look
           just before the change, and that of a wakeup under way there as it
           came. Without that move it woke there again and again until a
           decision happened to find it running: 30 looks there in a control
           run, and 1.4 s before it moved in another. */
        if (ok && p->sleeps && p->looks_there.get() > 2)
        {
            klog(0, "test_affinity: the sleeping task looked %ld times on cpu %lu after it was given cpu %lu: woken "
                "there, not moved", p->looks_there.get(), a, b);
            ok = false;
        }
        if (ok)
            klog(0, "test_affinity: the %s task moved from cpu %lu to cpu %lu in %lu us", what, a, b,
                moved / consts::NANO_SECS_IN_USEC);

        p->stop.set(1);
        task->wait();
        task->put();
        p->~AffinityProbe();
        mm::free(p);
    }

    /* A task bound to its CPU, as the softirq tasks are: refused, as it was */
    if (ok)
    {
        auto* p = mm::talloc<AffinityProbe, TAG>();
        Task* task = (p != nullptr) ? mm::talloc<Task, TAG>("affinity/bound") : nullptr;
        if (task == nullptr)
        {
            klog(0, "test_affinity: could not make a bound task");
            ok = false;
        }
        else
        {
            p->cpu.set(0);
            p->stop.set(1);
            p->sleeps = true;
            task->pin_to_cpu(a);
            if (set_task_affinity(task, 1UL << b) != AffinityResult::Pinned || task->get_cpu_affinity() != (1UL << a))
            {
                klog(0, "test_affinity: a task bound to cpu %lu taken another affinity", a);
                ok = false;
            }
            task->put();
        }
        if (p != nullptr)
        {
            p->~AffinityProbe();
            mm::free(p);
        }
    }
    return ok;
}

/* ---- tracepoints (kernel/tracepoint.h, kernel/trace_ring.h) ---- */

/* What a test record of index `index` carries: anything else read back
   under that index is a record torn, or another's. */
/* Rust's sites (kcore::trace), for the self-test below: `n` test records,
   the `i`th of sequence number base + i and value (base + i) ^
   RUST_TRACE_SALT -- src/crates/kernel/src/trace.rs says the same. */
extern "C" u64 rust_trace_emit_test(u64 base, u64 n);
static const u64 RUST_TRACE_SALT = 0xA5A5A5A5A5A5A5A5ULL;

static u64 trace_test_value(u64 index)
{
    return (index * 0x9E3779B97F4A7C15ULL) ^ 0xA5A5A5A5A5A5A5A5ULL;
}

static bool trace_ring_write(TraceRing& ring, u64 index)
{
    u64 payload[2] = { index, trace_test_value(index) };
    ulong flags = hal::irq_save();
    bool ok = ring.write((u16)tp::Id::Test, hal::read_cycle_counter(), payload, sizeof(payload));
    hal::irq_restore(flags);
    return ok;
}

static bool trace_ring_holds(const TraceRing& ring, u64 index)
{
    TraceRing::Record r;
    return ring.read(index, r) && r.index == index && r.id == (u16)tp::Id::Test && r.size == 2 * sizeof(u64)
        && r.payload[0] == index && r.payload[1] == trace_test_value(index);
}

struct TraceWriter
{
    TraceRing* ring;
    Atomic stop;
    Atomic written;
};

/* The other CPU's: records into the ring as fast as it can until told. */
static void trace_writer_task(void* ctx)
{
    auto* w = static_cast<TraceWriter*>(ctx);
    u64 index = 0;
    while (w->stop.get() == 0)
    {
        if (trace_ring_write(*w->ring, index))
            index++;
    }
    w->written.set((long)index);
}

/* A ring of its own: records read back in order, the ring wrapping with the
   oldest written over and no longer read, a payload too large refused; then
   a writer on another CPU and this one reading as it writes -- every record
   kept must be whole, the one its index says; then the kernel's own rings
   through emit(), a freeze, and every ring freed once no CPU can be writing
   (CpuTable::synchronize, an IPI to each). */
static bool test_tracepoints()
{
    TraceRing ring;
    if (!ring.init(1, TAG))
    {
        klog(0, "Tracepoints: no memory for a test ring");
        return false;
    }
    bool ok = true;
    const u64 cap = ring.capacity();

    for (u64 i = 0; i < 10 && ok; i++)
        ok = trace_ring_write(ring, i);
    for (u64 i = 0; i < 10 && ok; i++)
        ok = trace_ring_holds(ring, i);
    if (ok && (ring.head() != 10 || trace_ring_holds(ring, 10)))
        ok = false;
    if (!ok)
        klog(0, "Tracepoints: ten records not read back as written");

    u8 big[TraceRing::PAYLOAD_BYTES + 1] = {};
    if (ok && ring.write((u16)tp::Id::Test, 0, big, sizeof(big)))
    {
        klog(0, "Tracepoints: a payload of %lu bytes taken", (ulong)sizeof(big));
        ok = false;
    }

    /* Three times round: the newest `cap` kept, the one before them gone. */
    const u64 last = 3 * cap;
    for (u64 i = 10; i < last && ok; i++)
        ok = trace_ring_write(ring, i);
    for (u64 i = last - cap; i < last && ok; i++)
    {
        if (!trace_ring_holds(ring, i))
        {
            klog(0, "Tracepoints: record %lu of %lu not kept after the ring wrapped", (ulong)i, (ulong)last);
            ok = false;
        }
    }
    if (ok && trace_ring_holds(ring, last - cap - 1))
    {
        klog(0, "Tracepoints: record %lu read back after it was written over", (ulong)(last - cap - 1));
        ok = false;
    }
    ring.release();
    if (!ok)
        return false;

    /* A writer on another CPU, and this one reading what it writes as it
       writes it. */
    auto& cpus = CpuTable::get_instance();
    ulong self = cpus.get_current_cpu_id();
    ulong running = cpus.get_running_cpus();
    ulong other = MAX_CPUS;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((running & (1UL << i)) && i != self)
            other = i;
    }
    if (other != MAX_CPUS)
    {
        TraceRing shared;
        TraceWriter w = { &shared, Atomic(0), Atomic(0) };
        Task* writer = shared.init(1, TAG) ? mm::talloc<Task, TAG>() : nullptr;
        if (writer == nullptr)
        {
            shared.release();
            klog(0, "Tracepoints: no memory for the two-cpu test");
            return false;
        }
        /* The writer there; this task -- the boot's, its CPU's idle task,
           which never moves -- reads here. */
        writer->set_cpu_affinity(1UL << other);
        if (!writer->start(trace_writer_task, &w))
        {
            writer->put();
            shared.release();
            klog(0, "Tracepoints: the writer task did not start");
            return false;
        }

        static const ulong READ_NS = 20 * consts::NANO_SECS_IN_MS;
        ulong kept = 0, torn = 0, gone = 0;
        stdlib::Time until = get_boot_time() + stdlib::Time(READ_NS);
        while (get_boot_time() < until && torn == 0)
        {
            u64 head = shared.head();
            u64 from = (head > cap) ? head - cap : 0;
            for (u64 i = from; i < head; i++)
            {
                TraceRing::Record r;
                if (!shared.read(i, r))
                {
                    gone++;
                    continue;
                }
                if (r.payload[0] != i || r.payload[1] != trace_test_value(i) || r.index != i)
                    torn++;
                else
                    kept++;
            }
        }
        w.stop.set(1);
        writer->wait();
        writer->put();
        ulong written = (ulong)w.written.get();
        shared.release();

        if (torn != 0 || kept == 0 || written < cap)
        {
            klog(0, "Tracepoints: cpu %lu writing, cpu %lu reading: %lu records written, %lu read whole, %lu "
                 "not the record their index says, %lu written over as they were read",
                 other, self, written, kept, torn, gone);
            return false;
        }
        klog(TEST_LL, "Tracepoints: cpu %lu wrote %lu records while cpu %lu read %lu of them whole, %lu written "
             "over as they were read, none torn", other, written, self, kept, gone);
    }

    /* The kernel's own rings: the smallest, the test event on, a hundred
       records on this CPU read back; frozen, none taken; freed. */
    char out[256];
    stdlib::BufferPrinter con(out, sizeof(out));
    const ulong size_was = tp::ring_size_kib();
    if (!tp::set_ring_kib(64, con) || tp::set("test", true, con) != 1)
    {
        klog(0, "Tracepoints: the test event not turned on: %s", out);
        tp::free_rings();
        tp::set_ring_kib(size_was, con);
        return false;
    }

    /* A hundred from here, and a hundred more from Rust's sites through
       kcore::trace -- the flags' address it asks once, the kernel's emit()
       -- the path a module's tracepoints take into the rings. Interrupts off
       for both, so that every one is this CPU's. */
    static const ulong RECORDS = 100;
    static const ulong ALL = 2 * RECORDS;
    ulong flags = hal::irq_save();
    ulong cpu = cpus.get_current_cpu_id();
    for (ulong i = 0; i < RECORDS; i++)
    {
        tp::Test t = { i, trace_test_value(i) };
        tp::emit(tp::Id::Test, t);
    }
    u64 from_rust = rust_trace_emit_test(RECORDS, RECORDS);
    hal::irq_restore(flags);

    auto records = static_cast<TraceRing::Record*>(mm::alloc((ALL + 1) * sizeof(TraceRing::Record), TAG));
    if (records == nullptr)
    {
        tp::free_rings();
        tp::set_ring_kib(size_was, con);
        return false;
    }
    ulong got = tp::read_cpu(cpu, tp::Id::Test, records, ALL + 1);
    for (ulong i = 0; i < got && ok; i++)
    {
        tp::Test t;
        stdlib::mem_cpy(&t, records[i].payload, sizeof(t));
        u64 value = (i < RECORDS) ? trace_test_value(i) : (i ^ RUST_TRACE_SALT);
        ok = (t.seq == i && t.value == value && records[i].cpu == cpu);
        if (ok && i != 0 && records[i].cycles < records[i - 1].cycles)
            ok = false;
    }
    if (!ok || got != ALL || from_rust != RECORDS)
    {
        char stat[512];
        stdlib::BufferPrinter sp(stat, sizeof(stat));
        tp::command("stat", sp);
        klog(0, "Tracepoints: %lu records emitted on cpu %lu, %lu of them by Rust, %lu read back%s; the rings: %s",
             ALL, cpu, (ulong)from_rust, got, ok ? "" : ", not as they were emitted", stat);
        ok = false;
    }

    /* Frozen: nothing more is taken. */
    tp::command("freeze", con);
    if (ok)
    {
        tp::Test t = { ALL, 0 };
        tp::emit(tp::Id::Test, t);
        if (rust_trace_emit_test(ALL, 1) != 1 || tp::read_cpu(cpu, tp::Id::Test, records, ALL + 1) != ALL)
        {
            klog(0, "Tracepoints: a record taken while recording was frozen");
            ok = false;
        }
    }
    tp::command("thaw", con);
    mm::free(records);

    tp::free_rings();
    /* The size the boot found, for the rings anyone makes after. */
    if (!tp::set_ring_kib(size_was, con))
    {
        klog(0, "Tracepoints: the ring size not put back to %lu KiB", size_was);
        ok = false;
    }
    TraceRing::Record after;
    if (ok && (tp::on(tp::Id::Test) || tp::read_cpu(cpu, tp::Id::Test, &after, 1) != 0))
    {
        klog(0, "Tracepoints: the test event on, or a record read, after every ring was freed");
        ok = false;
    }
    if (ok)
        klog(TEST_LL, "Tracepoints: %lu records through emit() on cpu %lu read back, %lu of them Rust's, none "
             "while frozen, the rings freed", ALL, cpu, RECORDS);
    return ok;
}

/* test_heap_usage_cpus: each CPU's task puts this many blocks in and takes
   them out again, this many times -- past a slot's worth of bytes and of
   blocks each time, so that every CPU folds into the totals the others
   fold into */
static const ulong HEAP_USAGE_ROUNDS = 64;
static const long HEAP_USAGE_BLOCKS = 256;
static const ulong HEAP_USAGE_ASKED = 3000;
static const ulong HEAP_USAGE_TAKEN = 4096;

static void heap_usage_task(void* ctx)
{
    auto* usage = static_cast<mm::HeapUsage*>(ctx);
    for (ulong round = 0; round < HEAP_USAGE_ROUNDS; round++)
    {
        for (long i = 0; i < HEAP_USAGE_BLOCKS; i++)
            usage->add(HEAP_USAGE_ASKED, HEAP_USAGE_TAKEN);
        for (long i = 0; i < HEAP_USAGE_BLOCKS; i++)
            usage->sub(HEAP_USAGE_ASKED, HEAP_USAGE_TAKEN);
    }
}

/* The heap's counters changed on every CPU at once: nothing lost between
   the slots and the totals -- all of it back to zero, every allocation and
   free counted -- and the peak of what was taken between one task's blocks
   and every task's, give or take a slot's worth a CPU */
static bool test_heap_usage_cpus()
{
    /* Counters of the test's own: the Rust heap's go on counting under it */
    static mm::HeapUsage usage;

    auto& cpus = CpuTable::get_instance();
    const ulong mask = cpus.get_running_cpus();
    Task* tasks[MAX_CPUS] = {};
    ulong started = 0;
    bool ok = true;
    for (ulong cpu = 0; cpu < MAX_CPUS && ok; cpu++)
    {
        if ((mask & (1UL << cpu)) == 0)
            continue;
        Task* task = mm::talloc<Task, TAG>("heapusage");
        ok = (task != nullptr);
        if (ok)
            task->set_cpu_affinity(1UL << cpu);
        if (ok && !task->start(heap_usage_task, &usage))
        {
            task->put();
            ok = false;
        }
        if (ok)
            tasks[started++] = task;
    }
    for (ulong i = 0; i < started; i++)
    {
        tasks[i]->wait();
        tasks[i]->put();
    }
    if (!ok || started == 0)
    {
        klog(0, "test_heap_usage_cpus: a task not started, %lu were", started);
        return false;
    }

    mm::HeapUsage::Totals t;
    usage.read(t);
    const ulong changes = started * HEAP_USAGE_ROUNDS * static_cast<ulong>(HEAP_USAGE_BLOCKS);
    const long one = HEAP_USAGE_BLOCKS * static_cast<long>(HEAP_USAGE_TAKEN);
    const long slack = static_cast<long>(started) * mm::HeapUsage::FOLD_BYTES;
    ok = t.bytes == 0 && t.taken == 0 && t.blocks == 0 && t.allocs == changes && t.frees == changes &&
        t.refused == 0 && t.taken_peak >= one - slack && t.taken_peak <= static_cast<long>(started) * one + slack;
    if (!ok)
        klog(0, "test_heap_usage_cpus: on %lu cpus: asked/taken/blocks %ld/%ld/%ld, allocs/frees %lu/%lu of %lu, "
             "taken at most %ld (want %ld to %ld)", started, t.bytes, t.taken, t.blocks, t.allocs, t.frees, changes,
             t.taken_peak, one - slack, static_cast<long>(started) * one + slack);
    else
        klog(0, "test_heap_usage_cpus: %lu changes on %lu cpus, all counted; taken at most %ld KiB",
             2 * changes, started, t.taken_peak / static_cast<long>(consts::KB));
    return ok;
}

/* A driver's line interrupt, registered and taken off again, over and over.
   Until it was fixed, taking a handler off gave back its slot and nothing
   else: the line's entry kept the handler and its pin stayed unmasked, so
   each registration after that chained onto the line once more -- refused
   on x86 once the entry held eight, a BUG_ON on arm64 -- and the slot's own
   vector was never free again. Here: two handlers share the line, the first
   leaves and a third takes its place on the same vector, all leave, and the
   vector is nobody's -- in all more registrations than an entry holds
   handlers, and no more than that: each is a line in the log. */
extern "C" unsigned long kernel_interrupt_register_level(unsigned char irq_line, void (*handler)(void*),
    void* ctx, unsigned char* out_vector);
extern "C" void kernel_interrupt_unregister(unsigned long handle);

static void test_irq_line_handler(void* ctx)
{
    static_cast<Atomic*>(ctx)->inc();
}

static bool test_irq_line_unregister()
{
    static const ulong ROUNDS = 4;

    u8 line = 0;
    if (!hal::spare_level_line(line))
    {
        klog(0, "test_irq_line_unregister: no line here to try it on: not run");
        return true;
    }

    /* Static: a handler may still be called as its line is unregistered,
       and is handed this */
    static Atomic calls;

    for (ulong pass = 0; pass < ROUNDS; pass++)
    {
        unsigned char vector[3] = {0, 0, 0};
        unsigned long first = kernel_interrupt_register_level(line, test_irq_line_handler, &calls, &vector[0]);
        unsigned long second = first ? kernel_interrupt_register_level(line, test_irq_line_handler, &calls,
            &vector[1]) : 0;
        if (first == 0 || second == 0)
        {
            if (first != 0)
                kernel_interrupt_unregister(first);

            /* The first time round it is the drivers that have the slots --
               a machine of eight virtio-mmio devices has them all -- and
               there is nothing to try this with. Later it is what this
               looks for: slots and a line that were free a pass ago. */
            if (pass == 0)
            {
                klog(0, "test_irq_line_unregister: no two of the drivers' slots are free: not run");
                return true;
            }

            klog(0, "test_irq_line_unregister: pass %lu, line %lu refused (handles %lu %lu)", pass, (ulong)line,
                first, second);
            return false;
        }

        /* The line's first handler leaves; the one left has it, and its
           vector, and a third joins it there */
        kernel_interrupt_unregister(first);
        unsigned long third = kernel_interrupt_register_level(line, test_irq_line_handler, &calls, &vector[2]);
        bool ok = third != 0 && vector[1] == vector[0] && vector[2] == vector[0];

        kernel_interrupt_unregister(second);
        if (third != 0)
            kernel_interrupt_unregister(third);

        /* The line's last handler gone, its vector is nobody's */
        if (!ok || !Interrupt::is_vector_free(vector[0]))
        {
            klog(0, "test_irq_line_unregister: pass %lu, line %lu: handle %lu, vectors 0x%lX 0x%lX 0x%lX, the "
                "first %s", pass, (ulong)line, third, (ulong)vector[0], (ulong)vector[1], (ulong)vector[2],
                Interrupt::is_vector_free(vector[0]) ? "free" : "still taken");
            return false;
        }
    }

    klog(0, "test_irq_line_unregister: line %lu registered and let go of %lu times, %lu interrupts", (ulong)line,
        3 * ROUNDS, (ulong)calls.get());
    return true;
}

bool test_multi_tasking()
{
    if (!test_spin_lock_preempt())
        return false;

    if (!test_preempt_deferred())
        return false;

    if (!test_event())
        return false;

    if (!test_sleep_blocks())
        return false;

    if (!test_sleep_prompt())
        return false;

    if (!test_event_wait_for())
        return false;

    /* Once, on the boot CPU, which comes here after every other CPU has
       started: every CPU runs this function at once, and the rings and the
       events are the kernel's, one set of them -- one CPU's free_rings()
       under another's emit() and read back made the read find nothing. */
    auto& cpus = CpuTable::get_instance();
    if (cpus.get_current_cpu_id() == cpus.get_bsp_index() && !test_tracepoints())
        return false;

    /* Once too, from the boot CPU, a task on every CPU */
    if (cpus.get_current_cpu_id() == cpus.get_bsp_index() && !test_heap_usage_cpus())
        return false;

    /* And once: the line is one, and so are the drivers' slots */
    if (cpus.get_current_cpu_id() == cpus.get_bsp_index() && !test_irq_line_unregister())
        return false;

    /* The block table again, now that the drivers have put their disks in
       it: at its first run (test()) there was none, and all it checked was
       that a name nothing carries finds nothing */
    if (cpus.get_current_cpu_id() == cpus.get_bsp_index() && !test_block_device_table().ok())
        return false;

    Task *task[2] = {0};
    for (size_t i = 0; i < stdlib::array_size(task); i++)
    {
        task[i] = kernel::mm::talloc<Task, TAG>();
        if (task[i] == nullptr)
        {
            for (size_t j = 0; j < i; j++)
            {
                task[j]->put();
            }
            return false;
        }
    }

    bool result;

    for (size_t i = 0; i < stdlib::array_size(task); i++)
    {
        if (!task[i]->start(test_multi_tasking_task_func, nullptr))
        {
            for (size_t j = 0; j < i; j++)
            {
                task[j]->wait();
            }
            result = false;
            goto delTasks;
        }
    }

    for (size_t i = 0; i < stdlib::array_size(task); i++)
    {
        task[i]->wait();
    }

    result = true;

delTasks:
    for (size_t i = 0; i < stdlib::array_size(task); i++)
    {
        task[i]->put();
    }

    return result;
}

}

}
