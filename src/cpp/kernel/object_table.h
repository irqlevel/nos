#pragma once

#include <lib/stdlib.h>
#include "spin_lock.h"

namespace kernel
{

class Object
{
public:
    virtual void get() = 0;
    virtual void put() = 0;
};

template<typename T>
class ObjectPtr final
{
public:
    ObjectPtr()
        : ptr_(nullptr)
    {
    }

    explicit ObjectPtr(T* ptr)
        : ptr_(ptr)
    {
    }

    ~ObjectPtr()
    {
        if (ptr_ != nullptr)
            ptr_->put();
    }

    ObjectPtr(ObjectPtr&& other)
        : ptr_(other.ptr_)
    {
        other.ptr_ = nullptr;
    }

    ObjectPtr& operator=(ObjectPtr&& other)
    {
        if (this != &other)
        {
            if (ptr_ != nullptr)
                ptr_->put();
            ptr_ = other.ptr_;
            other.ptr_ = nullptr;
        }
        return *this;
    }

    ObjectPtr(const ObjectPtr& other) = delete;
    ObjectPtr& operator=(const ObjectPtr& other) = delete;

    T* get() const { return ptr_; }
    T* operator->() const { return ptr_; }
    T& operator*() const { return *ptr_; }

    explicit operator bool() const { return ptr_ != nullptr; }

    T* release()
    {
        T* p = ptr_;
        ptr_ = nullptr;
        return p;
    }

private:
    T* ptr_;
};

using ObjectId = ulong;

const ObjectId INVALID_OBJECT_ID = ~((ObjectId)0);

class ObjectTable final
{
public:
    ObjectTable();
    ~ObjectTable();

    ulong insert(Object *object);

    void remove(ulong object_id);

    Object* lookup(ulong object_id);

private:
    ObjectTable(const ObjectTable& other) = delete;
    ObjectTable(ObjectTable&& other) = delete;
    ObjectTable& operator=(const ObjectTable& other) = delete;
    ObjectTable& operator=(ObjectTable&& other) = delete;

    static const ulong MAX_OBJECT_ID = 256;

    Object* object_array_[MAX_OBJECT_ID];

    SpinLock lock_;
};

}