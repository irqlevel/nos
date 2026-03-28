#pragma once

#include "atomic.h"
#include <lib/lock.h>

namespace Kernel
{

class Mutex final
	: public Stdlib::LockInterface
{
public:
	Mutex();

	void Lock();

	void Unlock();

	virtual void Lock(ulong& flags) override;

	virtual void Unlock(ulong flags) override;

	virtual ~Mutex();

private:
	Mutex(const Mutex& other) = delete;
	Mutex(Mutex&& other) = delete;
	Mutex& operator=(const Mutex& other) = delete;
	Mutex& operator=(Mutex&& other) = delete;

	Atomic Value; // 0 = unlocked, 1 = locked
};

}
