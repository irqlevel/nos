#pragma once

#include <kernel/trace.h>
#include <kernel/atomic.h>
#include <kernel/panic.h>

namespace Kernel
{

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

#if defined(__DEBUG__)
        if (Object != nullptr)
            BugOn(get_kapi()->unique_key_register(Object, this, get_kapi_pool_type(PoolType)) != 0);
#endif

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
        if (BugOn(Object != nullptr))
            return;

        Object = object;

#if defined(__DEBUG__)
        BugOn(get_kapi()->unique_key_register(Object, this, get_kapi_pool_type(PoolType)) != 0);
#endif

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

#if defined(__DEBUG__)
            if (BugOn(get_kapi()->unique_key_unregister(Object, this) != 0))
                return false;
#endif

            delete Object;
            Object = nullptr;
            return true;
        }
        Trace(SharedPtrLL, "objref 0x%p obj 0x%p dec counter %d", this, Object, Counter.Get());

        return false;
    }

private:
    Atomic Counter;
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
            ObjectRef = new ObjectReference<T>(object);
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
    ObjectReference<T>* objRef = new ObjectReference<T>(nullptr);
    if (objRef == nullptr)
        return SharedPtr<T>();

    T* object = new T(Shared::Forward<Args>(args)...);
    if (object == nullptr)
    {
        delete objRef;
        return SharedPtr<T>();
    }

    objRef->SetObject(object);
    return SharedPtr<T>(objRef);
}

}