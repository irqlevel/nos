#pragma once

#include "helpers32.h"
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
		spin_lock_lock_32(&RawLock);
	}

	virtual void Unlock() override
	{
		spin_lock_unlock_32(&RawLock);
	}

	virtual void SharedLock() override
	{
		spin_lock_lock_32(&RawLock);
	}

	virtual void SharedUnlock() override
	{
		spin_lock_unlock_32(&RawLock);
	}

	virtual ~SpinLock()
	{
	}
private:
	ulong RawLock;
};

}

}