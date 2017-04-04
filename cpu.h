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

    ulong GetIndex();

    void Idle();

    bool Activate(ulong index);
    bool IsActive();

private:
    Cpu(const Cpu& other) = delete;
    Cpu(Cpu&& other) = delete;
    Cpu& operator=(const Cpu& other) = delete;
    Cpu& operator=(Cpu&& other) = delete;

    ulong Index;
    bool Active;
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

    bool RegisterCpu(ulong index);

    Cpu& GetCpu(ulong index);

private:
    CpuTable();
    ~CpuTable();
    CpuTable(const CpuTable& other) = delete;
    CpuTable(CpuTable&& other) = delete;
    CpuTable& operator=(const CpuTable& other) = delete;
    CpuTable& operator=(CpuTable&& other) = delete;

    static const ulong MaxCpu = 16;
    SpinLock Lock;
    Cpu CpuArray[MaxCpu];
};

}

}