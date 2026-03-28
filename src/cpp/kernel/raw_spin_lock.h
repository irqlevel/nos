#pragma once

#include <lib/stdlib.h>
#include "atomic.h"

namespace Kernel
{

class RawSpinLock final
{
public:
    RawSpinLock();
    ~RawSpinLock();

    void Lock();
    void Unlock();

	ulong LockIrqSave();
	void UnlockIrqRestore(ulong flags);

private:
    RawSpinLock(const RawSpinLock& other) = delete;
    RawSpinLock(RawSpinLock&& other) = delete;
    RawSpinLock& operator=(const RawSpinLock& other) = delete;
    RawSpinLock& operator=(RawSpinLock&& other) = delete;

    Atomic Value;
};
}