#pragma once

namespace Shared
{

class LockInterface
{
public:
    virtual void Lock() = 0;
    virtual void Unlock() = 0;
};

class SharedLockInterface
{
public:
    virtual void SharedLock() = 0;
    virtual void SharedUnlock() = 0;
};

class AutoLock
{
public:
    AutoLock(LockInterface& lock)
        : Lock(lock)
    {
        Lock.Lock();
    }

    virtual ~AutoLock()
    {
        Lock.Unlock();
    }
private:
    AutoLock() = delete;
    AutoLock(const AutoLock& other) = delete;
    AutoLock(AutoLock&& other) = delete;
    AutoLock& operator=(const AutoLock& other) = delete;
    AutoLock& operator=(AutoLock&& other) = delete;

    LockInterface& Lock;
};

class SharedAutoLock
{
public:
    SharedAutoLock(SharedLockInterface& lock)
        : Lock(lock)
    {
        Lock.SharedLock();
    }

    virtual ~SharedAutoLock()
    {
        Lock.SharedUnlock();
    }
private:
    SharedAutoLock() = delete;
    SharedAutoLock(const SharedAutoLock& other) = delete;
    SharedAutoLock(SharedAutoLock&& other) = delete;
    SharedAutoLock& operator=(const SharedAutoLock& other) = delete;
    SharedAutoLock& operator=(SharedAutoLock&& other) = delete;

    SharedLockInterface& Lock;
};

class NopLock
	: public Shared::LockInterface
	, public Shared::SharedLockInterface
{
public:
	NopLock()
	{
	}

	virtual void Lock() override
	{
	}

	virtual void Unlock() override
	{
	}

	virtual void SharedLock() override
	{
	}

	virtual void SharedUnlock() override
	{
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