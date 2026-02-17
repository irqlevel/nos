#pragma once

#include "stdlib.h"
#include "deleter.h"

#include <kernel/panic.h>
#include <mm/new.h>

namespace Stdlib
{

template<typename T, typename Deleter = DefaultDeleter<T>>
class UniquePtr final
{
public:
    UniquePtr(UniquePtr&& other)
        : UniquePtr()
    {
        Reset(other.Release());
    }

    UniquePtr& operator=(UniquePtr&& other)
    {
        if (this != &other)
        {
            Reset(other.Release());
        }
        return *this;
    }

    UniquePtr()
        : Object(nullptr)
    {
    }

    UniquePtr(T* object)
        : UniquePtr()
    {
        Reset(object);
    }

    UniquePtr(const UniquePtr& other) = delete;

    UniquePtr& operator=(const UniquePtr& other) = delete;

    void Reset(T* object)
    {
        BugOn(Object != nullptr && Object == object);

        if (Object != nullptr)
        {
            Deleter()(Object);
            Object = nullptr;
        }

        Object = object;
    }

    void Reset()
    {
        Reset(nullptr);
    }

    T* Release()
    {
        T* object = Object;
        Object = nullptr;
        return object;
    }

    ~UniquePtr()
    {
        Reset(nullptr);
    }

    T* Get() const
    {
        return Object;
    }

    T& operator*() const
    {
        return *Get();
    }

    T* operator->() const
    {
        return Get();
    }

private:
    T* Object;
};

template<typename T, class... Args>
UniquePtr<T> MakeUnique(Args&&... args)
{
    return UniquePtr<T>(Kernel::Mm::TAlloc<T, 0>(Stdlib::Forward<Args>(args)...));
}

}