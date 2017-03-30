#pragma once

#include "types.h"

namespace Shared
{

class Allocator
{
public:
	virtual void* Alloc(size_t size) = 0;
	virtual void Free(void* ptr) = 0;
};

}