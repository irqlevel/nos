#pragma once

namespace Kernel
{

namespace Core
{

class PageAllocator
{
public:
    virtual void* Alloc() = 0;
    virtual void Free(void* ptr) = 0;
};

}
}