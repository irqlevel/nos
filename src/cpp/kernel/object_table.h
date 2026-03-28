#pragma once

#include <lib/stdlib.h>
#include "spin_lock.h"

namespace Kernel
{

class Object
{
public:
    virtual void Get() = 0;
    virtual void Put() = 0;
};

template<typename T>
class ObjectPtr final
{
public:
    ObjectPtr()
        : Ptr(nullptr)
    {
    }

    explicit ObjectPtr(T* ptr)
        : Ptr(ptr)
    {
    }

    ~ObjectPtr()
    {
        if (Ptr != nullptr)
            Ptr->Put();
    }

    ObjectPtr(ObjectPtr&& other)
        : Ptr(other.Ptr)
    {
        other.Ptr = nullptr;
    }

    ObjectPtr& operator=(ObjectPtr&& other)
    {
        if (this != &other)
        {
            if (Ptr != nullptr)
                Ptr->Put();
            Ptr = other.Ptr;
            other.Ptr = nullptr;
        }
        return *this;
    }

    ObjectPtr(const ObjectPtr& other) = delete;
    ObjectPtr& operator=(const ObjectPtr& other) = delete;

    T* Get() const { return Ptr; }
    T* operator->() const { return Ptr; }
    T& operator*() const { return *Ptr; }

    explicit operator bool() const { return Ptr != nullptr; }

    T* Release()
    {
        T* p = Ptr;
        Ptr = nullptr;
        return p;
    }

private:
    T* Ptr;
};

using ObjectId = ulong;

const ObjectId InvalidObjectId = ~((ObjectId)0);

class ObjectTable final
{
public:
    ObjectTable();
    ~ObjectTable();

    ulong Insert(Object *object);

    void Remove(ulong objectId);

    Object* Lookup(ulong objectId);

private:
    ObjectTable(const ObjectTable& other) = delete;
    ObjectTable(ObjectTable&& other) = delete;
    ObjectTable& operator=(const ObjectTable& other) = delete;
    ObjectTable& operator=(ObjectTable&& other) = delete;

    static const ulong MaxObjectId = 256;

    Object* ObjectArray[MaxObjectId];

    SpinLock Lock;
};

}