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
        (PageStart & (Shared::PageSize - 1)) ||
        (PageEnd & (Shared::PageSize - 1)))
    {
        Panic("Invalid page start/end");
        return;
    }

    for (ulong page = PageStart; page < PageEnd; page+= Shared::PageSize)
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
    void* page = PageList.RemoveHead();
    Trace(0, "Alloc page %p", page);
    return page;
}

void SPageAllocator::Free(void* page)
{
    Shared::AutoLock lock(Lock);

    Trace(0, "Free page %p", page);

    BugOn(page == nullptr);

	ulong pageAddr = reinterpret_cast<ulong>(page);

	if (pageAddr & (Shared::PageSize - 1)) {
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