#pragma once

#include "stdlib.h"
#include "deleter.h"

#include <kernel/panic.h>
#include <mm/new.h>

namespace stdlib
{

template<typename T, typename Deleter = DefaultDeleter<T>>
class UniquePtr final
{
public:
    UniquePtr(UniquePtr&& other)
        : UniquePtr()
    {
        reset(other.release());
    }

    UniquePtr& operator=(UniquePtr&& other)
    {
        if (this != &other)
        {
            reset(other.release());
        }
        return *this;
    }

    UniquePtr()
        : object_(nullptr)
    {
    }

    UniquePtr(T* object)
        : UniquePtr()
    {
        reset(object);
    }

    UniquePtr(const UniquePtr& other) = delete;

    UniquePtr& operator=(const UniquePtr& other) = delete;

    void reset(T* object)
    {
        BUG_ON(object_ != nullptr && object_ == object);

        if (object_ != nullptr)
        {
            Deleter()(object_);
            object_ = nullptr;
        }

        object_ = object;
    }

    void reset()
    {
        reset(nullptr);
    }

    T* release()
    {
        T* object = object_;
        object_ = nullptr;
        return object;
    }

    ~UniquePtr()
    {
        reset(nullptr);
    }

    T* get() const
    {
        return object_;
    }

    T& operator*() const
    {
        return *get();
    }

    T* operator->() const
    {
        return get();
    }

private:
    T* object_;
};

template<typename T, class... Args>
UniquePtr<T> make_unique(Args&&... args)
{
    return UniquePtr<T>(kernel::mm::talloc<T, 0>(stdlib::forward<Args>(args)...));
}

}