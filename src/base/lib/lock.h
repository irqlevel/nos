#pragma once

#include <include/types.h>

namespace stdlib
{

class LockInterface
{
public:
    virtual void lock(ulong& flags) = 0;
    virtual void unlock(ulong flags) = 0;
};

class SharedLockInterface
{
public:
    virtual void shared_lock(ulong& flags) = 0;
    virtual void shared_unlock(ulong flags) = 0;
};

class AutoLock
{
public:
    AutoLock(LockInterface& lock)
        : lock_(lock)
        , flags_(0)
    {
        lock_.lock(flags_);
    }

    virtual ~AutoLock()
    {
        lock_.unlock(flags_);
    }
private:
    AutoLock() = delete;
    AutoLock(const AutoLock& other) = delete;
    AutoLock(AutoLock&& other) = delete;
    AutoLock& operator=(const AutoLock& other) = delete;
    AutoLock& operator=(AutoLock&& other) = delete;

    LockInterface& lock_;
    ulong flags_;
};

class SharedAutoLock
{
public:
    SharedAutoLock(SharedLockInterface& lock)
        : lock_(lock)
        , flags_(0)
    {
        lock_.shared_lock(flags_);
    }

    virtual ~SharedAutoLock()
    {
        lock_.shared_unlock(flags_);
    }
private:
    SharedAutoLock() = delete;
    SharedAutoLock(const SharedAutoLock& other) = delete;
    SharedAutoLock(SharedAutoLock&& other) = delete;
    SharedAutoLock& operator=(const SharedAutoLock& other) = delete;
    SharedAutoLock& operator=(SharedAutoLock&& other) = delete;

    SharedLockInterface& lock_;
    ulong flags_;
};

class NopLock
	: public stdlib::LockInterface
	, public stdlib::SharedLockInterface
{
public:
	NopLock()
	{
	}

	virtual void lock(ulong& flags) override
	{
        flags = 0;
	}

	virtual void unlock(ulong flags) override
	{
        (void)flags;
	}

	virtual void shared_lock(ulong& flags) override
	{
        flags = 0;
	}

	virtual void shared_unlock(ulong flags) override
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