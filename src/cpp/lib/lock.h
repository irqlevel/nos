#pragma once

#include <include/types.h>

namespace Stdlib
{

class LockInterface
{
public:
    virtual void Lock(ulong& flags) = 0;
    virtual void Unlock(ulong flags) = 0;
};

class SharedLockInterface
{
public:
    virtual void SharedLock(ulong& flags) = 0;
    virtual void SharedUnlock(ulong flags) = 0;
};

class AutoLock
{
public:
    AutoLock(LockInterface& lock)
        : Lock(lock)
        , Flags(0)
    {
        Lock.Lock(Flags);
    }

    virtual ~AutoLock()
    {
        Lock.Unlock(Flags);
    }
private:
    AutoLock() = delete;
    AutoLock(const AutoLock& other) = delete;
    AutoLock(AutoLock&& other) = delete;
    AutoLock& operator=(const AutoLock& other) = delete;
    AutoLock& operator=(AutoLock&& other) = delete;

    LockInterface& Lock;
    ulong Flags;
};

class SharedAutoLock
{
public:
    SharedAutoLock(SharedLockInterface& lock)
        : Lock(lock)
        , Flags(0)
    {
        Lock.SharedLock(Flags);
    }

    virtual ~SharedAutoLock()
    {
        Lock.SharedUnlock(Flags);
    }
private:
    SharedAutoLock() = delete;
    SharedAutoLock(const SharedAutoLock& other) = delete;
    SharedAutoLock(SharedAutoLock&& other) = delete;
    SharedAutoLock& operator=(const SharedAutoLock& other) = delete;
    SharedAutoLock& operator=(SharedAutoLock&& other) = delete;

    SharedLockInterface& Lock;
    ulong Flags;
};

class NopLock
	: public Stdlib::LockInterface
	, public Stdlib::SharedLockInterface
{
public:
	NopLock()
	{
	}

	virtual void Lock(ulong& flags) override
	{
        flags = 0;
	}

	virtual void Unlock(ulong flags) override
	{
        (void)flags;
	}

	virtual void SharedLock(ulong& flags) override
	{
        flags = 0;
	}

	virtual void SharedUnlock(ulong flags) override
	{
        (void)flags;
	}

	virtual ~NopLock()
	{
	}
private:
    NopLock(const NopLock& other) = delete;
    NopLock(NopLock&& other) = delete;
    NopLock& operator=(const NopLock& other) = delete;
    NopLock& operator=(NopLock&& other) = delete;
};

}