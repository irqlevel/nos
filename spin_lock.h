#pragma once

#include "asm.h"
#include "lock.h"

namespace Kernel
{

namespace Core
{

class SpinLock
	: public Shared::LockInterface
	, public Shared::SharedLockInterface
{
public:
	SpinLock()
		: RawLock(0)
	{
	}

	virtual void Lock() override
	{
		SpinLockLock(&RawLock);
	}

	virtual void Unlock() override
	{
		SpinLockUnlock(&RawLock);
	}

	virtual void SharedLock() override
	{
		SpinLockLock(&RawLock);
	}

	virtual void SharedUnlock() override
	{
		SpinLockUnlock(&RawLock);
	}

	virtual ~SpinLock()
	{
	}
private:
	ulong RawLock;
};

}

}