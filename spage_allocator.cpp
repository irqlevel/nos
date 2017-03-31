#include "spage_allocator.h"
#include "const.h"
#include "list_entry.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

SPageAllocator* SPageAllocator::InstancePtr = nullptr;

SPageAllocator::SPageAllocator(ulong pageStart, ulong pageEnd)
    : Usage(0)
    , PageStart(pageStart)
    , PageEnd(pageEnd)
{
    PageList.Init();

    if (PageStart == 0 || PageEnd <= PageStart ||
        (PageStart & (PAGE_SIZE - 1)) ||
        (PageEnd & (PAGE_SIZE - 1)))
    {
        Panic("Invalid page start/end");
        return;
    }

    for (ulong page = PageStart; page < PageEnd; page+= PAGE_SIZE)
    {
        ListEntry* pageLink = reinterpret_cast<ListEntry*>(page);
        PageList.InsertTail(pageLink);
    }
}

SPageAllocator::~SPageAllocator()
{
    BugOn(Usage != 0);
}

void* SPageAllocator::Alloc()
{
    Shared::AutoLock lock(Lock);

    if (PageList.IsEmpty())
    {
        return nullptr;
    }

    Usage++;
    return PageList.RemoveHead();
}

void SPageAllocator::Free(void* page)
{
    Shared::AutoLock lock(Lock);

    BugOn(page == nullptr);

	ulong pageAddr = reinterpret_cast<ulong>(page);

	if (pageAddr & (PAGE_SIZE - 1)) {
		Panic("pageAddr unaligned");
		return;
	}

	if (pageAddr < PageStart) {
		Panic("pageAddr < PageStart");
		return;
	}

	if (pageAddr >= PageEnd) {
		Panic("pageAddr >= PageEnd");
		return;
	}

    ListEntry* pageLink = reinterpret_cast<ListEntry*>(page);
    PageList.InsertTail(pageLink);
    Usage--;
}

}

}