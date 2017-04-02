#include "new.h"
#include "const.h"
#include "panic.h"
#include "spage_allocator.h"
#include "sallocator.h"

namespace Kernel
{

namespace Core
{

struct NewEntry {
	ulong Magic;
	Shared::Allocator* Allocator;
};

const ulong Magic = 0xCBEFCBEF;

void* New(size_t size, Shared::Allocator* allocator) noexcept
{
	NewEntry* e;

	if (allocator == nullptr)
	{
		e = static_cast<NewEntry*>
				(SAllocator::GetInstance(SPageAllocator::GetInstance()).Alloc(size + sizeof(*e)));
	}
	else
	{
		e = static_cast<NewEntry*>(allocator->Alloc(size + sizeof(*e)));
	}

	if (e == nullptr)
		return nullptr;

	e->Allocator = allocator;
	e->Magic = Magic;

	return e + 1;
}

void Delete(void* ptr) noexcept
{
	auto e = static_cast<NewEntry*>(ptr) - 1;

	if (e->Allocator == nullptr)
	{
		SAllocator::GetInstance(SPageAllocator::GetInstance()).Free(e);
	}
	else
	{
		BugOn(e->Magic != Magic);
		e->Allocator->Free(e);
	}
}

}
}

void* operator new(size_t size, Shared::Allocator& allocator) noexcept
{
	return Kernel::Core::New(size, &allocator);
}

void* operator new[](size_t size, Shared::Allocator& allocator) noexcept
{
	return Kernel::Core::New(size, &allocator);
}

void* operator new(size_t size) noexcept
{
	return Kernel::Core::New(size, nullptr);
}

void* operator new[](size_t size) noexcept
{
	return Kernel::Core::New(size, nullptr);
}

void operator delete(void* ptr) noexcept
{
	Kernel::Core::Delete(ptr);
}

void operator delete[](void* ptr) noexcept
{
	Kernel::Core::Delete(ptr);
}
