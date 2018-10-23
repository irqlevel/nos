#pragma once

#include <kernel/trace.h>
#include <kernel/atomic.h>
#include <kernel/panic.h>
#include <mm/new.h>

namespace Stdlib
{

const int SharedPtrLL = 5;

template<typename T>
class ObjectReference final
{
public:
    ObjectReference(T* object)
        : Object(nullptr)
    {
        Counter.Set(1);
        Object = object;

        Trace(SharedPtrLL, "objref 0x%p obj 0x%p ctor", this, Object);
    }

    ~ObjectReference()
    {
        Trace(SharedPtrLL, "objref 0x%p dtor", this);

        BugOn(Object != nullptr);
        BugOn(Counter.Get() != 0);
    }

    void IncCounter()
    {
        Counter.Inc();
        Trace(SharedPtrLL, "objref 0x%p obj 0x%p inc counter %d", this, Object, Counter.Get());
    }

    int GetCounter()
    {
        return Counter.Get();
    }

    void SetObject(T *object)
    {
        BugOn(Object != nullptr);

        Object = object;

    }

    T* GetObject()
    {
        return Object;
    }

    bool DecCounter()
    {
        if (Counter.DecAndTest())
        {
            Trace(SharedPtrLL, "objref 0x%p obj 0x%p dec counter %d", this, Object, Counter.Get());
            delete Object;
            Object = nullptr;
            return true;
        }
        Trace(SharedPtrLL, "objref 0x%p obj 0x%p dec counter %d", this, Object, Counter.Get());

        return false;
    }

private:
    Kernel::Atomic Counter;
    T* Object;

    ObjectReference() = delete;
    ObjectReference(const ObjectReference& other) = delete;
    ObjectReference(ObjectReference&& other) = delete;
    ObjectReference& operator=(const ObjectReference& other) = delete;
    ObjectReference& operator=(ObjectReference&& other) = delete;
};

template<typename T>
class SharedPtr final
{
public:
    SharedPtr()
    {
        ObjectRef = nullptr;

        Trace(SharedPtrLL, "ptr 0x%p ctor obj 0x%p", this, Get());
    }

    SharedPtr(ObjectReference<T> *objectRef)
    {
        ObjectRef = objectRef;

        Trace(SharedPtrLL, "ptr 0x%p ctor obj 0x%p", this, Get());
    }

    SharedPtr(T *object)
        : SharedPtr()
    {
        Reset(object);

        Trace(SharedPtrLL, "ptr 0x%p ctor obj 0x%p", this, Get());
    }

    SharedPtr(const SharedPtr<T>& other)
        : SharedPtr()
    {
        ObjectRef = other.ObjectRef;
        if (ObjectRef != nullptr)
        {
            ObjectRef->IncCounter();
        }

        Trace(SharedPtrLL, "ptr 0x%p ctor obj 0x%p", this, Get());
    }

    SharedPtr(SharedPtr<T>&& other)
        : SharedPtr()
    {
        ObjectRef = other.ObjectRef;
        other.ObjectRef = nullptr;

        Trace(SharedPtrLL, "ptr 0x%p ctor obj 0x%p", this, Get());
    }

    SharedPtr<T>& operator=(const SharedPtr<T>& other)
    {
        if (this != &other)
        {
            Reset(nullptr);
            ObjectRef = other.ObjectRef;
            if (ObjectRef != nullptr)
            {
                ObjectRef->IncCounter();
            }
        }

        Trace(SharedPtrLL, "ptr 0x%p op= obj 0x%p", this, Get());

        return *this;
    }

    SharedPtr<T>& operator=(SharedPtr<T>&& other)
    {
        if (this != &other)
        {
            Reset(nullptr);
            ObjectRef = other.ObjectRef;
            other.ObjectRef = nullptr;
        }

        Trace(SharedPtrLL, "ptr 0x%p op= obj 0x%p", this, Get());

        return *this;
    }

    T* Get() const
    {
        return (ObjectRef != nullptr) ? ObjectRef->GetObject() : nullptr;
    }

    T& operator*() const
    {
        return *Get();
    }

    T* operator->() const
    {
        return Get();
    }

    int GetCounter()
    {
        return (ObjectRef != nullptr) ? ObjectRef->GetCounter() : 0;
    }

    ~SharedPtr()
    {
        Reset(nullptr);
    }

    void Reset(T* object)
    {
        Trace(SharedPtrLL, "ptr 0x%p reset obj 0x%p new 0x%p", this, Get(), object);

        BugOn(Get() != nullptr && Get() == object);

        if (ObjectRef != nullptr)
        {
            if (ObjectRef->DecCounter())
            {
                delete ObjectRef;
            }
        }

        ObjectRef = nullptr;

        if (object != nullptr)
        {
            ObjectRef = Kernel::Mm::TAlloc<ObjectReference<T>, 0>(object);
            if (ObjectRef == nullptr)
            {
                return;
            }
        }
    }

    void Reset()
    {
        Reset(nullptr);
    }

private:
    ObjectReference<T>* ObjectRef;
};

template<typename T, class... Args>
SharedPtr<T> MakeShared(Args&&... args)
{
    ObjectReference<T>* objRef = Kernel::Mm::TAlloc<ObjectReference<T>, 0>(nullptr);
    if (objRef == nullptr)
        return SharedPtr<T>();

    T* object = Kernel::Mm::TAlloc<T, 0>(Stdlib::Forward<Args>(args)...);
    if (object == nullptr)
    {
        delete objRef;
        return SharedPtr<T>();
    }

    objRef->SetObject(object);
    return SharedPtr<T>(objRef);
}

}