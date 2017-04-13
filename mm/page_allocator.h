#pragma once

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Core
{

class BlockAllocatorImpl
{
public:
    BlockAllocatorImpl();
    ~BlockAllocatorImpl();

    bool Setup(ulong startAddress, ulong endAddress, ulong blockSize);

    void* Alloc();

    bool IsOwner(void *block);

    void Free(void *block);

private:
    BlockAllocatorImpl(const BlockAllocatorImpl& other) = delete;
    BlockAllocatorImpl(BlockAllocatorImpl&& other) = delete;
    BlockAllocatorImpl& operator=(const BlockAllocatorImpl& other) = delete;
    BlockAllocatorImpl& operator=(BlockAllocatorImpl&& other) = delete;

    using ListEntry = Shared::ListEntry;

    ListEntry BlockList;
    ulong Usage;
    ulong Total;
    ulong StartAddress;
    ulong EndAddress;
    ulong BlockSize;
    SpinLock Lock;
};

class PageAllocator
{
public:
    virtual void* Alloc(size_t numPages) = 0;
    virtual void Free(void* ptr) = 0;
};

class PageAllocatorImpl : public PageAllocator
{
public:
	static PageAllocatorImpl& GetInstance()
	{
		static PageAllocatorImpl Instance;
		return Instance;
	}

    bool Setup(ulong startAddress, ulong endAddress);

    virtual void* Alloc(size_t numPages) override;
    virtual void Free(void* pages) override;

private:
    PageAllocatorImpl();
    virtual ~PageAllocatorImpl();

    PageAllocatorImpl(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl(PageAllocatorImpl&& other) = delete;
    PageAllocatorImpl& operator=(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl& operator=(PageAllocatorImpl&& other) = delete;

    static const size_t PageLogLimit = 4;

    BlockAllocatorImpl Balloc[PageLogLimit];

};

}

}