#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

/* A source of raw entropy: a device (virtio-rng), a cpu instruction (RDRAND,
   RNDR) or a measurement (timing jitter). Sources register with the table
   below and Kernel::Random (kernel/random.h) mixes all of them into its pool;
   nothing else reads a source directly. GetRandom may be slow -- virtio-rng
   polls its device -- and may fail, which is why there is a pool in front. */
class EntropySource
{
public:
    virtual ~EntropySource() {}
    virtual const char* GetName() = 0;
    virtual bool GetRandom(u8* buf, ulong len) = 0;
};

class EntropySourceTable
{
public:
    static EntropySourceTable& GetInstance()
    {
        static EntropySourceTable instance;
        return instance;
    }

    bool Register(EntropySource* src);
    EntropySource* Find(const char* name);
    /* Sources are enumerated, not picked: a reseed takes from all of them.
       Returns nullptr past the end. */
    EntropySource* Get(ulong index);
    ulong GetCount();
    void Dump(Stdlib::Printer& printer);

    /* Four virtio-rng devices, the cpu instruction and the jitter collector,
       with room to spare. */
    static const ulong MaxSources = 8;

private:
    EntropySourceTable();
    ~EntropySourceTable();
    EntropySourceTable(const EntropySourceTable& other) = delete;
    EntropySourceTable(EntropySourceTable&& other) = delete;
    EntropySourceTable& operator=(const EntropySourceTable& other) = delete;
    EntropySourceTable& operator=(EntropySourceTable&& other) = delete;

    EntropySource* Sources[MaxSources];
    ulong Count;
};

}
