#pragma once

#include "raw_spin_lock.h"
#include <lib/lock.h>
#include <lib/list_entry.h>

namespace kernel
{

class SpinLock final
	: public stdlib::LockInterface
	, public stdlib::SharedLockInterface
{
public:
	SpinLock();

	void lock();

	void unlock();

	virtual void lock(ulong& flags) override;

	virtual void unlock(ulong flags) override;

	virtual void shared_lock(ulong& flags) override;

	virtual void shared_unlock(ulong flags) override;

	/* Take this lock out of the watchdog's supervision. The console locks
	   do it: the watchdog reports through the console, so watching the
	   console's own lock would have it report about itself. */
	void unwatch();

	virtual ~SpinLock();

private:
	SpinLock(const SpinLock& other) = delete;
	SpinLock(SpinLock&& other) = delete;
	SpinLock& operator=(const SpinLock& other) = delete;
	SpinLock& operator=(SpinLock&& other) = delete;

	/* The embedded raw lock carries the watchdog timestamp and registration
	   for both of them; SpinLock adds only the owner. */
	RawSpinLock raw_lock_;
	volatile void* owner_;
};

}