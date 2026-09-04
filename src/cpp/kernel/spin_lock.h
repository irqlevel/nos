#pragma once

#include "raw_spin_lock.h"
#include <lib/lock.h>
#include <lib/list_entry.h>

namespace Kernel
{

class SpinLock final
	: public Stdlib::LockInterface
	, public Stdlib::SharedLockInterface
{
public:
	SpinLock();

	void Lock();

	void Unlock();

	virtual void Lock(ulong& flags) override;

	virtual void Unlock(ulong flags) override;

	virtual void SharedLock(ulong& flags) override;

	virtual void SharedUnlock(ulong flags) override;

	/* Take this lock out of the watchdog's supervision. The console locks
	   do it: the watchdog reports through the console, so watching the
	   console's own lock would have it report about itself. */
	void Unwatch();

	virtual ~SpinLock();

private:
	SpinLock(const SpinLock& other) = delete;
	SpinLock(SpinLock&& other) = delete;
	SpinLock& operator=(const SpinLock& other) = delete;
	SpinLock& operator=(SpinLock&& other) = delete;

	/* The embedded raw lock carries the watchdog timestamp and registration
	   for both of them; SpinLock adds only the owner. */
	RawSpinLock RawLock;
	volatile void* Owner;
};

}