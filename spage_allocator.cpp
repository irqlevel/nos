#include "spage_allocator.h"
#include "const.h"
#include "list_entry.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

SPageAllocator::SPageAllocator()
    : Usage(0)
{
    PageList.Init();

    for (size_t i = 0; i < MaxPages; i++)
    {
        ListEntry* pageLink = reinterpret_cast<ListEntry*>(Shared::MemAdd(Page, i * PAGE_SIZE));
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

	if (pageAddr < reinterpret_cast<ulong>(Page)) {
		Panic("pageAddr < Page");
		return;
	}

	if (pageAddr >= (reinterpret_cast<ulong>(Page) + sizeof(Page))) {
		Panic("pageAddr >= Page");
		return;
	}

    ListEntry* pageLink = reinterpret_cast<ListEntry*>(page);
    PageList.InsertTail(pageLink);
    Usage--;
}

}

}