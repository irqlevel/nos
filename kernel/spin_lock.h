#pragma once

#include "atomic.h"
#include <lib/lock.h>

namespace Kernel
{

class SpinLock
	: public Shared::LockInterface
	, public Shared::SharedLockInterface
{
public:
	SpinLock();

	void Lock();

	void Unlock();

	virtual void Lock(ulong& flags) override;

	virtual void Unlock(ulong flags) override;

	virtual void SharedLock(ulong& flags) override;

	virtual void SharedUnlock(ulong flags) override;

	virtual ~SpinLock();

private:
	SpinLock(const SpinLock& other) = delete;
	SpinLock(SpinLock&& other) = delete;
	SpinLock& operator=(const SpinLock& other) = delete;
	SpinLock& operator=(SpinLock&& other) = delete;

	Atomic RawLock;
	volatile void* Owner;
};

}