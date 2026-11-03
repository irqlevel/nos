// The tracepoints' ring (kernel/trace_ring.cpp): one CPU's record of events,
// written by that CPU alone with its interrupts off, and read by anyone at
// any time while it writes. Every call held to a model of the ring: init
// takes a power of two of chunks and nothing else, and on an allocation that
// fails keeps nothing; a write takes a payload of at most PAYLOAD_BYTES and
// nothing larger; the records it holds are the newest capacity() of those
// written, each read back as written -- its index, cycles, event, size, the
// payload's bytes and zeros after them -- and an index not written yet,
// written over, or past a release, is no record. Now and then a writer on a
// thread of its own and the input's thread reading as it writes: every
// record a read keeps is the one its index says, whatever the interleaving
// (on an arm64 host, a weakly ordered one, the ring's own dmbs between).
#include "host.h"

#include <kernel/trace_ring.h>

#include "fuzz.h"
#include "kernel.h"

#include <string.h>

#include <atomic>
#include <thread>
#include <vector>

namespace
{

using kernel::TraceRing;

const ulong TAG = 0x46547A74;

struct Written
{
    u16 id;
    u16 size;
    u64 cycles;
    uint8_t payload[TraceRing::PAYLOAD_BYTES];
};

bool power_of_two(ulong n)
{
    return n != 0 && (n & (n - 1)) == 0;
}

/* A record of index `index` read back, held to what the model wrote. */
void check_read(const TraceRing& ring, const std::vector<Written>& written, u64 index, const char* after)
{
    const u64 head = written.size();
    const u64 cap = ring.capacity();
    const bool held = ring.ready() && index < head && index + cap >= head;

    TraceRing::Record r;
    memset(&r, 0xEE, sizeof(r));
    bool got = ring.read(index, r);
    INVARIANT(got == held, "read(%lu) of a ring of %lu records, %lu written, said %d after %s", (ulong)index,
              (ulong)cap, (ulong)head, got, after);
    if (!got)
        return;

    const Written& w = written[index];
    INVARIANT(r.index == index && r.id == w.id && r.size == w.size && r.cycles == w.cycles && r.cpu == 0,
              "record %lu read back as index %lu, event %u of %u bytes at %lu -- written event %u of %u at %lu",
              (ulong)index, (ulong)r.index, r.id, r.size, (ulong)r.cycles, w.id, w.size, (ulong)w.cycles);
    uint8_t bytes[TraceRing::PAYLOAD_BYTES];
    memcpy(bytes, r.payload, sizeof(bytes));
    for (size_t i = 0; i < sizeof(bytes); i++)
    {
        uint8_t want = (i < w.size) ? w.payload[i] : 0;
        INVARIANT(bytes[i] == want, "record %lu's payload byte %zu is 0x%02x, not 0x%02x", (ulong)index, i, bytes[i],
                  want);
    }
}

void run_model(fuzz::Input& in)
{
    static const ulong CHUNKS[] = {1, 1, 2, 4, 0, 3, 6, TraceRing::MAX_CHUNKS * 2};
    const size_t blocks = fuzz::heap_blocks();

    TraceRing ring;
    std::vector<Written> written;
    ulong chunks = in.pick(CHUNKS);
    if (in.chance(32))
        fuzz::heap_fail_after(in.below(chunks + 2));
    bool made = ring.init(chunks, TAG);
    fuzz::heap_fail_after(-1);
    if (!power_of_two(chunks) || chunks > TraceRing::MAX_CHUNKS)
        INVARIANT(!made, "init took %lu chunks", chunks);
    if (!made)
    {
        INVARIANT(!ring.ready() && ring.capacity() == 0, "a ring init refused is ready");
        INVARIANT(fuzz::heap_blocks() == blocks, "init refused and kept %zu blocks", fuzz::heap_blocks() - blocks);
        fuzz::reached(power_of_two(chunks) ? "init failed for memory" : "init refused a count");
        return;
    }
    INVARIANT(ring.capacity() == chunks * TraceRing::SLOTS_PER_CHUNK, "a ring of %lu chunks holds %lu", chunks,
              (ulong)ring.capacity());
    fuzz::reached("a ring made");

    for (int op = 0; op < 4000 && in.more(); op++)
    {
        switch (in.u8() % 6)
        {
        case 0:
        case 1:
        {
            Written w = {};
            w.id = in.u16();
            w.size = static_cast<u16>(in.chance(24) ? in.range(TraceRing::PAYLOAD_BYTES + 1, 255)
                                                    : in.below(TraceRing::PAYLOAD_BYTES + 1));
            w.cycles = in.u64();
            uint8_t src[256];
            for (size_t i = 0; i < w.size; i++)
                src[i] = in.u8();
            bool took = ring.write(w.id, w.cycles, src, w.size);
            INVARIANT(took == (w.size <= TraceRing::PAYLOAD_BYTES), "write of %u bytes said %d", w.size, took);
            if (took)
            {
                memcpy(w.payload, src, w.size);
                written.push_back(w);
            }
            else
            {
                fuzz::reached("a payload too large refused");
            }
            break;
        }
        case 2:
        {
            /* A burst: round the ring, or close to it. */
            ulong n = in.range(1, ring.capacity() * 3);
            for (ulong i = 0; i < n; i++)
            {
                Written w = {};
                w.id = static_cast<u16>(i);
                w.size = sizeof(u64);
                w.cycles = written.size();
                u64 v = written.size() * 0x9E3779B97F4A7C15ULL;
                memcpy(w.payload, &v, sizeof(v));
                INVARIANT(ring.write(w.id, w.cycles, &v, sizeof(v)), "a write of 8 bytes refused");
                written.push_back(w);
            }
            if (written.size() > ring.capacity())
                fuzz::reached("the ring wrapped");
            break;
        }
        case 3:
        case 4:
        {
            /* Around what the ring holds: before its oldest, in it, past it. */
            u64 head = written.size();
            u64 cap = ring.capacity();
            u64 lo = (head > cap + 3) ? head - cap - 3 : 0;
            check_read(ring, written, in.range(lo, head + 3), "a read");
            break;
        }
        default:
            INVARIANT(ring.head() == written.size(), "head %lu, %lu written", (ulong)ring.head(),
                      (ulong)written.size());
            INVARIANT(ring.dropped() == 0, "%lu dropped, with nothing nested", (ulong)ring.dropped());
            break;
        }
    }

    /* Every index the ring may hold, at the end. */
    u64 head = written.size();
    u64 cap = ring.capacity();
    for (u64 i = (head > cap + 2) ? head - cap - 2 : 0; i < head + 2; i++)
        check_read(ring, written, i, "the input's end");

    ring.release();
    INVARIANT(!ring.ready() && ring.capacity() == 0, "a released ring is ready");
    INVARIANT(fuzz::heap_blocks() == blocks, "a released ring kept %zu blocks", fuzz::heap_blocks() - blocks);
    TraceRing::Record r;
    INVARIANT(!ring.read(0, r) && !ring.write(0, 0, &r, 1), "a released ring read or written");
}

u64 value_of(u64 index, u64 salt)
{
    return (index * 0x9E3779B97F4A7C15ULL) ^ salt;
}

/* A writer thread and this one reading as it writes. */
void run_threads(fuzz::Input& in)
{
    const size_t blocks = fuzz::heap_blocks();
    TraceRing ring;
    fuzz::reached("a writer thread");
    if (!ring.init(in.boolean() ? 1 : 2, TAG))
    {
        fuzz::reached("a writer thread's ring not made");
        return;
    }
    const u64 cap = ring.capacity();
    const u64 salt = in.u64();
    const u64 limit = in.range(cap, cap * 64);

    std::atomic<bool> stop{false};
    std::atomic<u64> wrote{0};
    std::thread writer([&] {
        u64 i = 0;
        while (i < limit && !stop.load(std::memory_order_relaxed))
        {
            u64 p[2] = {i, value_of(i, salt)};
            if (ring.write(7, i, p, sizeof(p)))
                i++;
        }
        wrote.store(i);
    });

    /* Reading from the writer's first record until it is done: rounds that
       find the ring empty -- the thread not started yet -- read nothing. */
    u64 kept = 0, gone = 0;
    for (u64 round = 0; round < 200000 && wrote.load() == 0; round++)
    {
        u64 head = ring.head();
        for (u64 i = (head > cap) ? head - cap : 0; i < head; i++)
        {
            TraceRing::Record r;
            if (!ring.read(i, r))
            {
                gone++;
                continue;
            }
            INVARIANT(r.index == i && r.id == 7 && r.size == 16 && r.cycles == i && r.payload[0] == i &&
                          r.payload[1] == value_of(i, salt) && r.payload[2] == 0,
                      "record %lu read as the writer wrote it: index %lu, cycles %lu, payload %lu %lx -- torn",
                      (ulong)i, (ulong)r.index, (ulong)r.cycles, (ulong)r.payload[0], (ulong)r.payload[1]);
            kept++;
        }
    }
    stop.store(true);
    writer.join();
    if (kept != 0)
        fuzz::reached("records read whole as they were written");
    if (gone != 0)
        fuzz::reached("records written over as they were read");

    /* The writer stopped: every record the ring holds, whole. */
    u64 head = ring.head();
    INVARIANT(head == wrote.load(), "head %lu, the writer wrote %lu", (ulong)head, (ulong)wrote.load());
    for (u64 i = (head > cap) ? head - cap : 0; i < head; i++)
    {
        TraceRing::Record r;
        INVARIANT(ring.read(i, r) && r.payload[0] == i && r.payload[1] == value_of(i, salt),
                  "record %lu not read back after the writer stopped", (ulong)i);
    }
    ring.release();
    INVARIANT(fuzz::heap_blocks() == blocks, "a released ring kept %zu blocks", fuzz::heap_blocks() - blocks);
}

void reset()
{
    fuzz::heap_fail_after(-1);
}

void run(fuzz::Input& in)
{
    if (in.chance(12))
        run_threads(in);
    else
        run_model(in);
}

}

const fuzz::Target fuzz::THE_TARGET = {"trace", reset, run, 4096, 20000};
