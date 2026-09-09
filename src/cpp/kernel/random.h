#pragma once

#include <include/types.h>
#include <lib/chacha20.h>
#include <lib/printer.h>

#include "entropy.h"
#include "spin_lock.h"

namespace Kernel
{

/* The CPU's own random instruction (hal/random.h) as an entropy source. */
class HwRandomSource : public EntropySource
{
public:
    HwRandomSource() {}
    virtual ~HwRandomSource() {}

    /* Draws a few values and refuses the instruction if they all come back
       the same. That is what a broken RDRAND looks like -- some AMD parts
       return all-ones forever after a resume -- and what a hypervisor that
       stubs the instruction out looks like too, and either way the CPUID bit
       says the instruction is there. */
    bool SelfTest();

    virtual const char* GetName() override;
    virtual bool GetRandom(u8* buf, ulong len) override;

private:
    HwRandomSource(const HwRandomSource& other) = delete;
    HwRandomSource(HwRandomSource&& other) = delete;
    HwRandomSource& operator=(const HwRandomSource& other) = delete;
    HwRandomSource& operator=(HwRandomSource&& other) = delete;
};

/* Timing jitter: what is left on a machine with no random instruction and no
   virtio-rng, which is the only reason it exists. Each bit is the low bit of
   how long a fixed piece of work took, measured with the cycle counter and
   debiased by von Neumann extraction (keep a differing pair, discard an equal
   one); what varies between two measurements is the cache and store-buffer
   state, the branch predictor, an SMT sibling, and any interrupt that lands
   in the middle.

   This is the weakest source here by a wide margin and the hardest to put a
   number on -- see docs/random.md. GetRandom fails rather than returning
   biased bytes when the counter is too coarse to show any variance at all. */
class JitterSource : public EntropySource
{
public:
    JitterSource();
    virtual ~JitterSource() {}

    virtual const char* GetName() override;
    virtual bool GetRandom(u8* buf, ulong len) override;

private:
    JitterSource(const JitterSource& other) = delete;
    JitterSource(JitterSource&& other) = delete;
    JitterSource& operator=(const JitterSource& other) = delete;
    JitterSource& operator=(JitterSource&& other) = delete;

    bool CollectByte(u8& out);
    u32 Sample();

    /* The walked buffer: bigger than a cache line by enough that a pass over
       it touches many, small enough to be a rounding error in .bss. */
    static const ulong ScratchWords = 1024;
    static const ulong ScratchMask = ScratchWords - 1;
    /* 64 bytes -- one cache line per step on both arches */
    static const ulong StrideWords = 16;
    static const ulong WalkSteps = 64;
    static const u32 WalkStepsMask = 63;
    /* A byte needs eight surviving pairs; give up rather than spin forever on
       a counter whose deltas never differ. */
    static const ulong MaxPairsPerByte = 256;

    u32 Scratch[ScratchWords];
    u32 Acc;
};

/* The kernel's random number generator: one ChaCha20 pool, seeded from every
   entropy source the machine turns out to have, and the only place the rest of
   the kernel -- the Rust TLS client included -- takes randomness from.
   Sources register with EntropySourceTable and contribute; the pool answers.
   Reading a source directly is what used to happen, and on a bare-metal box
   with no virtio-rng there was no source to read: the first TLS handshake on
   real hardware failed with FailedToGetRandomBytes.

   Both halves of the construction are Linux's, in miniature:

     output   fast key erasure. A request runs the ChaCha20 block function
              over the 32-byte key, keeps the first half of the result as the
              next key and hands the second half out, so the state that
              produced a value is gone by the time the caller has it. There is
              no way back from an output to an earlier one.
     seeding  absorb. Seed material is XORed into the key 32 bytes at a time
              and the block function run over the result. ChaCha20's
              feed-forward addition is what stops that being walked backwards,
              and XOR never destroys entropy already in the key, so a source
              that turns out to be worthless cannot make the pool worse. */
class Random
{
public:
    static Random& GetInstance()
    {
        static Random Instance;
        return Instance;
    }

    /* Probes the CPU's random instruction, registers the sources that do not
       need a device (the instruction, timing jitter) and seeds the pool from
       them. Touches no heap and no device, so it can run as early in boot as
       the trace log does -- it has to, because the self-tests want randomness
       and so does anything that runs before the PCI scan. */
    bool Setup();

    /* Fold in a fresh draw from every registered source, virtio-rng included.
       Called once the devices are up, and by `entropy reseed`. Never with a
       spinlock held: a source may poll its device for milliseconds. */
    void Reseed();

    /* Cannot fail and cannot block. Before Setup() it returns ChaCha20 output
       of an unseeded pool, which is why callers who must not do that (the TLS
       client) check IsSeeded first. */
    void GetBytes(void* buf, ulong len);
    u64 GetU64();

    /* Mix len bytes of anything into the pool. Cheap, IRQ-safe, and worth
       calling with material that is only partly unpredictable: the absorb
       cannot reduce what the pool already has. */
    void AddEntropy(const void* data, ulong len);

    bool IsSeeded();

    /* Whether anything better than timing jitter has ever contributed. */
    bool HasHardwareEntropy();

    void Dump(Stdlib::Printer& printer);

private:
    Random();
    ~Random();
    Random(const Random& other) = delete;
    Random(Random&& other) = delete;
    Random& operator=(const Random& other) = delete;
    Random& operator=(Random&& other) = delete;

    /* Both expect Lock held. */
    void AbsorbLocked(const void* data, ulong len);
    /* Fills a ChaCha20BlockSize block from the key and installs its first
       half as the next key -- the key erasure both the output and the absorb
       path are built on. */
    void GenerateLocked(u8* block);

    /* How much each source is asked for per reseed. A ChaCha20 key is 32
       bytes and there is nothing to be gained by seeding it with more at
       once; the absorb takes any length. */
    static const ulong SeedBytes = Stdlib::ChaCha20KeySize;

    SpinLock Lock;

    u8 Key[Stdlib::ChaCha20KeySize];
    /* Block-function nonce: monotonic, so no key is ever used with a repeated
       one even before the key erasure makes that moot. */
    u64 NonceCounter;

    HwRandomSource HwSource;
    JitterSource Jitter;

    volatile bool Seeded;
    bool HwSeeded;
    ulong ReseedCount;
    ulong BytesGenerated;
};

}
