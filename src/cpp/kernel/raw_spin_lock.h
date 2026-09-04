#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include "atomic.h"

namespace Kernel
{

class RawSpinLock final
{
public:
    /* watched = false only for the watchdog's own locks: registering those
       would recurse through Watchdog::GetInstance() while it is being built.
       Everything else is watched, which is the point -- the locks that
       actually wedge a machine are the hot raw ones in the driver and net
       paths, and they used to be invisible because only SpinLock was
       instrumented. */
    explicit RawSpinLock(bool watched = true);
    ~RawSpinLock();

    void Lock();
    void Unlock();

    /* One attempt, no spin. For paths that must not block on a lock whose
       holder may never release it -- the panic path, above all. */
    bool TryLock();

	ulong LockIrqSave();
	void UnlockIrqRestore(ulong flags);

	/* Disables interrupts either way; acquired says whether the lock was
	   actually taken, and the caller must decide what to do if it was not. */
	ulong TryLockIrqSave(bool& acquired);

private:
    RawSpinLock(const RawSpinLock& other) = delete;
    RawSpinLock(RawSpinLock&& other) = delete;
    RawSpinLock& operator=(const RawSpinLock& other) = delete;
    RawSpinLock& operator=(RawSpinLock&& other) = delete;

    void Stamp();

    Atomic Value;
    bool Watched;

public:
    /* Set once the watchdog has complained about this lock, cleared on
       release: one line per stuck episode instead of one per check pass,
       and the watchdog checks over a thousand times a second per CPU. */
    Atomic WatchdogReported;

private:

public:
    Stdlib::ListEntry WatchdogListEntry;
    Atomic WatchdogLockTime;
};
}