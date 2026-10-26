#pragma once

#include "atomic.h"
#include <lib/lock.h>

namespace kernel
{

class Mutex final
	: public stdlib::LockInterface
{
public:
	Mutex();

	void lock();

	void unlock();

	virtual void lock(ulong& flags) override;

	virtual void unlock(ulong flags) override;

	virtual ~Mutex();

private:
	Mutex(const Mutex& other) = delete;
	Mutex(Mutex&& other) = delete;
	Mutex& operator=(const Mutex& other) = delete;
	Mutex& operator=(Mutex&& other) = delete;

	Atomic value_; // 0 = unlocked, 1 = locked
};

}
