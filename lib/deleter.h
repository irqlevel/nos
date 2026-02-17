#pragma once

namespace Stdlib
{

template<typename T>
struct DefaultDeleter
{
    void operator()(T* ptr) const { delete ptr; }
};

}
