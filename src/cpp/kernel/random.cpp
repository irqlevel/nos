#include "random.h"
#include "time.h"
#include "trace.h"

#include <hal/barrier.h>
#include <hal/cpu.h>
#include <hal/random.h>
#include <lib/stdlib.h>

namespace Kernel
{

/* ---- the CPU's random instruction ---- */

bool HwRandomSource::SelfTest()
{
    /* Eight draws: enough to tell a working source from one that answers the
       same thing every time, cheap enough to do on every boot. */
    const ulong Draws = 8;

    u64 first = 0;
    bool haveFirst = false;
    bool differs = false;
    ulong ok = 0;

    for (ulong i = 0; i < Draws; i++)
    {
        u64 value;
        if (!Hal::HwRandom(value))
            continue;

        ok++;

        if (!haveFirst)
        {
            first = value;
            haveFirst = true;
            continue;
        }

        if (value != first)
            differs = true;
    }

    /* Half the draws failing is a source not worth having either: the retry
       loops inside the HAL have already been patient. */
    if (ok < Draws / 2)
    {
        Trace(0, "HwRandom: only %u of %u draws succeeded", ok, Draws);
        return false;
    }

    if (!differs)
    {
        Trace(0, "HwRandom: every draw returned 0x%p", (ulong)first);
        return false;
    }

    return true;
}

const char* HwRandomSource::GetName()
{
    return Hal::HwRandomName();
}

bool HwRandomSource::GetRandom(u8* buf, ulong len)
{
    if (buf == nullptr || len == 0)
        return false;

    ulong off = 0;

    while (off < len)
    {
        u64 value;
        if (!Hal::HwRandomSeed(value))
            return false;

        ulong n = len - off;
        if (n > sizeof(value))
            n = sizeof(value);

        Stdlib::MemCpy(&buf[off], &value, n);
        off += n;
    }

    return true;
}

/* ---- timing jitter ---- */

JitterSource::JitterSource()
    : Acc(0)
{
    Stdlib::MemSet(Scratch, 0, sizeof(Scratch));
}

const char* JitterSource::GetName()
{
    return "jitter";
}

u32 JitterSource::Sample()
{
    u64 t0 = Hal::ReadCycleCounter();

    /* The compiler barriers keep the walk inside the measurement: on arm64
       ReadCycleCounter is inline asm with no memory clobber, so without them
       the loads and stores below could be hoisted out of the window being
       timed and the delta would measure nothing. */
    Hal::CompilerBarrier();

    /* A dependent read-modify-write walk, one cache line per step, with a
       trip count that depends on the state left by the previous measurement.
       What differs between two measurements is the cache and store-buffer
       state, the branch predictor, an SMT sibling, and any interrupt that
       lands in the middle of one. */
    ulong steps = WalkSteps + (Acc & WalkStepsMask);
    ulong idx = Acc & ScratchMask;

    for (ulong i = 0; i < steps; i++)
    {
        Acc = Acc + Scratch[idx] + (u32)i;
        Scratch[idx] = Acc;
        idx = (idx + StrideWords) & ScratchMask;
    }

    Hal::CompilerBarrier();

    u64 t1 = Hal::ReadCycleCounter();

    /* Fold the measurement back in, so the next walk takes a different path
       and a different amount of time. */
    Acc = Acc ^ (u32)(t1 - t0) ^ (u32)t1;

    return (u32)((t1 - t0) & 1);
}

bool JitterSource::CollectByte(u8& out)
{
    u8 value = 0;
    ulong bits = 0;

    for (ulong pairs = 0; pairs < MaxPairsPerByte && bits < 8; pairs++)
    {
        u32 a = Sample();
        u32 b = Sample();

        /* Von Neumann: a pair that agrees says nothing about the bias behind
           it, a pair that differs is an unbiased bit whichever way the source
           leans. */
        if (a == b)
            continue;

        value = (u8)((value << 1) | (a & 1));
        bits++;
    }

    if (bits < 8)
        return false;

    out = value;
    return true;
}

bool JitterSource::GetRandom(u8* buf, ulong len)
{
    if (buf == nullptr || len == 0)
        return false;

    for (ulong i = 0; i < len; i++)
    {
        u8 value;
        if (!CollectByte(value))
        {
            /* Every measurement took exactly as long as the one before it:
               either the counter is too coarse to see this much work, or it
               is not running. Biased bytes would be worse than none. */
            Trace(0, "Jitter: no variance in %u cycle-counter measurements",
                (ulong)(2 * MaxPairsPerByte));
            return false;
        }

        buf[i] = value;
    }

    return true;
}

/* ---- the pool ---- */

Random::Random()
    : NonceCounter(0)
    , Seeded(false)
    , HwSeeded(false)
    , ReseedCount(0)
    , BytesGenerated(0)
{
    Stdlib::MemSet(Key, 0, sizeof(Key));
}

Random::~Random()
{
    Stdlib::MemSet(Key, 0, sizeof(Key));
}

void Random::GenerateLocked(u8* block)
{
    u8 nonce[Stdlib::ChaCha20NonceSize];

    NonceCounter = NonceCounter + 1;

    /* The counter in the low eight bytes is what keeps the nonce unique; the
       cycle counter in the other four is free freshness. */
    for (ulong i = 0; i < sizeof(NonceCounter); i++)
        nonce[i] = (u8)((NonceCounter >> (8 * i)) & 0xFF);

    u32 stamp = (u32)Hal::ReadCycleCounter();
    for (ulong i = 0; i < sizeof(stamp); i++)
        nonce[sizeof(NonceCounter) + i] = (u8)((stamp >> (8 * i)) & 0xFF);

    Stdlib::ChaCha20Block(Key, 0, nonce, block);

    /* Fast key erasure: the first half of the block becomes the key, so the
       state that produced this block is gone before the caller sees it. */
    Stdlib::MemCpy(Key, block, sizeof(Key));
}

void Random::AbsorbLocked(const void* data, ulong len)
{
    const u8* p = (const u8*)data;
    ulong off = 0;

    for (;;)
    {
        u8 chunk[Stdlib::ChaCha20KeySize];
        u8 block[Stdlib::ChaCha20BlockSize];

        Stdlib::MemSet(chunk, 0, sizeof(chunk));

        ulong n = len - off;
        if (n > sizeof(chunk))
            n = sizeof(chunk);
        if (n != 0)
            Stdlib::MemCpy(chunk, &p[off], n);

        /* XOR in, then run the block function over the result. The XOR cannot
           take entropy out of the key, and the feed-forward addition inside
           ChaCha20 is what stops the new key leading back to the old one. */
        for (ulong i = 0; i < sizeof(chunk); i++)
            Key[i] = Key[i] ^ chunk[i];

        GenerateLocked(block);

        Stdlib::MemSet(block, 0, sizeof(block));
        Stdlib::MemSet(chunk, 0, sizeof(chunk));

        off += n;
        if (off >= len)
            break;
    }
}

void Random::AddEntropy(const void* data, ulong len)
{
    u64 stamp = Hal::ReadCycleCounter();

    Stdlib::AutoLock lock(Lock);

    /* When the caller has nothing but the fact that it called, the timestamp
       is the material. */
    AbsorbLocked(&stamp, sizeof(stamp));

    if (data != nullptr && len != 0)
        AbsorbLocked(data, len);
}

void Random::GetBytes(void* buf, ulong len)
{
    if (buf == nullptr || len == 0)
        return;

    u8* out = (u8*)buf;

    /* One draw from the CPU's instruction per request where there is one: a
       few hundred cycles, no lock and no device, and it means every value the
       pool hands out on such a machine carries entropy the pool never had to
       store. */
    u64 fresh = 0;
    bool haveFresh = Hal::HasHwRandom() && Hal::HwRandom(fresh);
    u64 stamp = Hal::ReadCycleCounter();

    Stdlib::AutoLock lock(Lock);

    if (haveFresh)
    {
        for (ulong i = 0; i < sizeof(fresh); i++)
            Key[i] = Key[i] ^ (u8)((fresh >> (8 * i)) & 0xFF);
    }

    for (ulong i = 0; i < sizeof(stamp); i++)
        Key[sizeof(fresh) + i] = Key[sizeof(fresh) + i] ^
            (u8)((stamp >> (8 * i)) & 0xFF);

    ulong off = 0;

    while (off < len)
    {
        u8 block[Stdlib::ChaCha20BlockSize];

        GenerateLocked(block);

        /* The half of the block the key was not taken from is the output. */
        ulong n = len - off;
        if (n > Stdlib::ChaCha20KeySize)
            n = Stdlib::ChaCha20KeySize;

        Stdlib::MemCpy(&out[off], &block[Stdlib::ChaCha20KeySize], n);
        Stdlib::MemSet(block, 0, sizeof(block));

        off += n;
    }

    BytesGenerated = BytesGenerated + len;
}

u64 Random::GetU64()
{
    u64 value = 0;

    GetBytes(&value, sizeof(value));

    return value;
}

bool Random::Setup()
{
    Hal::ProbeHwRandom();

    auto& table = EntropySourceTable::GetInstance();

    /* What this early in the boot can be told apart by at all. Not secret,
       and on a machine that boots the same image twice not even different --
       which is why it is the first thing mixed and not the only one. */
    ulong marks[3];
    marks[0] = (ulong)Hal::ReadCycleCounter();
    marks[1] = Hal::GetSp();
    marks[2] = (ulong)&marks[0];
    AddEntropy(marks, sizeof(marks));

    u8 seed[SeedBytes];
    bool hwOk = false;

    if (Hal::HasHwRandom())
    {
        if (HwSource.SelfTest())
        {
            table.Register(&HwSource);
            if (HwSource.GetRandom(seed, sizeof(seed)))
            {
                AddEntropy(seed, sizeof(seed));
                hwOk = true;
            }
        }
        else
        {
            Trace(0, "Random: %s failed its self test, not using it",
                Hal::HwRandomName());
        }
    }

    bool jitterOk = Jitter.GetRandom(seed, sizeof(seed));
    if (jitterOk)
    {
        table.Register(&Jitter);
        AddEntropy(seed, sizeof(seed));
    }

    Stdlib::MemSet(seed, 0, sizeof(seed));

    {
        Stdlib::AutoLock lock(Lock);
        HwSeeded = hwOk;
        Seeded = hwOk || jitterOk;
    }

    if (hwOk)
        Trace(0, "Random: seeded from %s%s", Hal::HwRandomName(),
            jitterOk ? " and timing jitter" : "");
    else if (jitterOk)
        Trace(0, "Random: seeded from timing jitter only -- this cpu has no "
            "random instruction, see docs/random.md");
    else
        Trace(0, "Random: nothing seeded the pool, https will not work");

    return Seeded;
}

void Random::Reseed()
{
    auto& table = EntropySourceTable::GetInstance();

    ulong contributed = 0;
    bool hw = false;

    /* Sources are read with no lock of ours held: virtio-rng polls its device
       and can sit there for milliseconds. */
    for (ulong i = 0; i < table.GetCount(); i++)
    {
        EntropySource* src = table.Get(i);
        if (src == nullptr)
            continue;

        u8 seed[SeedBytes];
        if (!src->GetRandom(seed, sizeof(seed)))
        {
            Trace(RandomLL, "Random: source %s gave nothing", src->GetName());
            continue;
        }

        AddEntropy(seed, sizeof(seed));
        Stdlib::MemSet(seed, 0, sizeof(seed));

        contributed++;
        if (src != static_cast<EntropySource*>(&Jitter))
            hw = true;
    }

    /* Neither of these is secret; both differ between two boots. */
    ulong marks[2];
    marks[0] = GetWallTimeSecs();
    marks[1] = (ulong)Hal::ReadCycleCounter();
    AddEntropy(marks, sizeof(marks));

    {
        Stdlib::AutoLock lock(Lock);
        ReseedCount = ReseedCount + 1;
        if (hw)
            HwSeeded = true;
        if (contributed != 0)
            Seeded = true;
    }

    Trace(0, "Random: reseeded from %u of %u sources", contributed,
        table.GetCount());
}

bool Random::IsSeeded()
{
    return Seeded;
}

bool Random::HasHardwareEntropy()
{
    Stdlib::AutoLock lock(Lock);

    return HwSeeded;
}

void Random::Dump(Stdlib::Printer& printer)
{
    bool seeded;
    bool hw;
    ulong reseeds;
    ulong bytes;

    {
        Stdlib::AutoLock lock(Lock);
        seeded = Seeded;
        hw = HwSeeded;
        reseeds = ReseedCount;
        bytes = BytesGenerated;
    }

    printer.Printf("pool: chacha20 %s, hardware entropy %s, reseeds %u, "
        "bytes out %u\n", seeded ? "seeded" : "UNSEEDED", hw ? "yes" : "no",
        reseeds, bytes);
    printer.Printf("sources:\n");

    EntropySourceTable::GetInstance().Dump(printer);
}

}
