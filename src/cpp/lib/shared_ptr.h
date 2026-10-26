#pragma once

#include "deleter.h"

#include <kernel/trace.h>
#include <kernel/atomic.h>
#include <kernel/panic.h>
#include <mm/new.h>

namespace stdlib
{

const int SHARED_PTR_LL = 5;

template<typename T, typename Deleter = DefaultDeleter<T>>
class ObjectReference final
{
public:
    ObjectReference(T* object)
        : object_(nullptr)
    {
        counter_.set(1);
        object_ = object;

        trace(SHARED_PTR_LL, "objref 0x%p obj 0x%p ctor", this, object_);
    }

    ~ObjectReference()
    {
        trace(SHARED_PTR_LL, "objref 0x%p dtor", this);

        BUG_ON(object_ != nullptr);
        BUG_ON(counter_.get() != 0);
    }

    void inc_counter()
    {
        counter_.inc();
        trace(SHARED_PTR_LL, "objref 0x%p obj 0x%p inc counter %ld", this, object_, counter_.get());
    }

    int get_counter()
    {
        return counter_.get();
    }

    void set_object(T *object)
    {
        if (BUG_ON(object_ != nullptr))
            return;

        object_ = object;
    }

    T* get_object()
    {
        return object_;
    }

    bool dec_counter()
    {
        if (counter_.dec_and_test())
        {
            trace(SHARED_PTR_LL, "objref 0x%p obj 0x%p dec counter %ld", this, object_, counter_.get());

            Deleter()(object_);
            object_ = nullptr;
            return true;
        }
        trace(SHARED_PTR_LL, "objref 0x%p obj 0x%p dec counter %ld", this, object_, counter_.get());

        return false;
    }

private:
    kernel::Atomic counter_;
    T* object_;

    ObjectReference() = delete;
    ObjectReference(const ObjectReference& other) = delete;
    ObjectReference(ObjectReference&& other) = delete;
    ObjectReference& operator=(const ObjectReference& other) = delete;
    ObjectReference& operator=(ObjectReference&& other) = delete;
};

template<typename T, typename Deleter = DefaultDeleter<T>>
class SharedPtr final
{
public:
    SharedPtr()
    {
        object_ref_ = nullptr;

        trace(SHARED_PTR_LL, "ptr 0x%p ctor obj 0x%p", this, get());
    }

    SharedPtr(ObjectReference<T, Deleter> *object_ref)
    {
        object_ref_ = object_ref;

        trace(SHARED_PTR_LL, "ptr 0x%p ctor obj 0x%p", this, get());
    }

    SharedPtr(T *object)
        : SharedPtr()
    {
        reset(object);

        trace(SHARED_PTR_LL, "ptr 0x%p ctor obj 0x%p", this, get());
    }

    SharedPtr(const SharedPtr& other)
        : SharedPtr()
    {
        object_ref_ = other.object_ref_;
        if (object_ref_ != nullptr)
        {
            object_ref_->inc_counter();
        }

        trace(SHARED_PTR_LL, "ptr 0x%p ctor obj 0x%p", this, get());
    }

    SharedPtr(SharedPtr&& other)
        : SharedPtr()
    {
        object_ref_ = other.object_ref_;
        other.object_ref_ = nullptr;

        trace(SHARED_PTR_LL, "ptr 0x%p ctor obj 0x%p", this, get());
    }

    SharedPtr& operator=(const SharedPtr& other)
    {
        if (this != &other)
        {
            reset(nullptr);
            object_ref_ = other.object_ref_;
            if (object_ref_ != nullptr)
            {
                object_ref_->inc_counter();
            }
        }

        trace(SHARED_PTR_LL, "ptr 0x%p op= obj 0x%p", this, get());

        return *this;
    }

    SharedPtr& operator=(SharedPtr&& other)
    {
        if (this != &other)
        {
            reset(nullptr);
            object_ref_ = other.object_ref_;
            other.object_ref_ = nullptr;
        }

        trace(SHARED_PTR_LL, "ptr 0x%p op= obj 0x%p", this, get());

        return *this;
    }

    T* get() const
    {
        return (object_ref_ != nullptr) ? object_ref_->get_object() : nullptr;
    }

    T& operator*() const
    {
        return *get();
    }

    T* operator->() const
    {
        return get();
    }

    int get_counter()
    {
        return (object_ref_ != nullptr) ? object_ref_->get_counter() : 0;
    }

    ~SharedPtr()
    {
        reset(nullptr);
    }

    void reset(T* object)
    {
        trace(SHARED_PTR_LL, "ptr 0x%p reset obj 0x%p new 0x%p", this, get(), object);

        BUG_ON(get() != nullptr && get() == object);

        if (object_ref_ != nullptr)
        {
            if (object_ref_->dec_counter())
            {
                delete object_ref_;
            }
        }

        object_ref_ = nullptr;

        if (object != nullptr)
        {
            object_ref_ = kernel::mm::talloc<ObjectReference<T, Deleter>, 0>(object);
            if (object_ref_ == nullptr)
            {
                Deleter()(object);
                return;
            }
        }
    }

    void reset()
    {
        reset(nullptr);
    }

private:
    ObjectReference<T, Deleter>* object_ref_;
};

template<typename T, class... Args>
SharedPtr<T> make_shared(Args&&... args)
{
    T* object = kernel::mm::talloc<T, 0>(stdlib::forward<Args>(args)...);
    if (object == nullptr)
        return SharedPtr<T>();

    /* Construct the ObjectReference around the live object so a failed
       allocation here never destroys a refcounted-but-armed control block
       (its dtor asserts Counter == 0). */
    ObjectReference<T>* obj_ref = kernel::mm::talloc<ObjectReference<T>, 0>(object);
    if (obj_ref == nullptr)
    {
        delete object;
        return SharedPtr<T>();
    }

    return SharedPtr<T>(obj_ref);
}

template<typename T, typename Deleter, class... Args>
SharedPtr<T, Deleter> make_shared_with(Args&&... args)
{
    T* object = kernel::mm::talloc<T, 0>(stdlib::forward<Args>(args)...);
    if (object == nullptr)
        return SharedPtr<T, Deleter>();

    /* Construct the ObjectReference around the live object so a failed
       allocation here never destroys a refcounted-but-armed control block
       (its dtor asserts Counter == 0). */
    ObjectReference<T, Deleter>* obj_ref = kernel::mm::talloc<ObjectReference<T, Deleter>, 0>(object);
    if (obj_ref == nullptr)
    {
        delete object;
        return SharedPtr<T, Deleter>();
    }

    return SharedPtr<T, Deleter>(obj_ref);
}

}