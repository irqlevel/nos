#pragma once

#include "stdlib.h"
#include "spin_lock.h"

namespace Kernel
{

namespace Core
{

class Cpu final
{
public:
    Cpu();
    ~Cpu();

    void Init(ulong index);

    void SetRunning();

    ulong GetIndex();

    void Idle();

    ulong GetState();

    void IPI(Context* ctx);

    static const ulong StateInited = 0x1;
    static const ulong StateRunning = 0x2;

private:
    Cpu(const Cpu& other) = delete;
    Cpu(Cpu&& other) = delete;
    Cpu& operator=(const Cpu& other) = delete;
    Cpu& operator=(Cpu&& other) = delete;

    ulong Index;
    ulong State;
    SpinLock Lock;
};

class CpuTable final
{
public:
    static CpuTable& GetInstance()
    {
        static CpuTable instance;
        return instance;
    }

    bool InsertCpu(ulong index);

    Cpu& GetCpu(ulong index);

    bool StartAll();

    ulong GetBspIndex();
    bool SetBspIndex(ulong index);

    ulong GetCurrentCpuId();

    Cpu& GetCurrentCpu();

    static const u8 IPIVector = 0xFE;

    void SendIPI(ulong index);

    ulong GetRunningCpus();

private:
    CpuTable();
    ~CpuTable();
    CpuTable(const CpuTable& other) = delete;
    CpuTable(CpuTable&& other) = delete;
    CpuTable& operator=(const CpuTable& other) = delete;
    CpuTable& operator=(CpuTable&& other) = delete;

    ulong GetBspIndexLockHeld();

    static const ulong MaxCpu = 16;

    SpinLock Lock;
    Cpu CpuArray[MaxCpu];

    ulong BspIndex;
};

}

}