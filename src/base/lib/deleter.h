#pragma once

namespace stdlib
{

template<typename T>
struct DefaultDeleter
{
    void operator()(T* ptr) const { delete ptr; }
};

}
