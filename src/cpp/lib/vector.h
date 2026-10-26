#pragma once

#include "stdlib.h"
#include "error.h"

#include <kernel/panic.h>
#include <mm/new.h>

namespace stdlib
{

template<class T>
class Vector
{
public:
    Vector()
        : arr_(nullptr), size_(0), capacity_(0)
    {
    }

    size_t get_size() const
    {
        return size_;
    }

    size_t get_capacity() const
    {
        return capacity_;
    }

    T& operator[](size_t index)
    {
        BUG_ON(index >= size_);
        return arr_[index];
    }

    bool reserve(size_t capacity)
    {
        if (capacity <= capacity_)
            return true;

        T* new_arr = new T[capacity];
        if (!new_arr)
            return false;

        if (arr_)
        {
            for (size_t i = 0; i < size_; i++)
            {
                new_arr[i] = stdlib::move(arr_[i]);
            }
            delete[] arr_;
        }
        arr_ = new_arr;
        capacity_ = capacity;
        return true;
    }

    bool reserve_and_use(size_t capacity)
    {
        if (!reserve(capacity))
            return false;
        /* Reserve() keeps a larger existing Capacity; using it here would
           expose stale elements at [capacity..Capacity). */
        size_ = capacity;
        return true;
    }

    bool truncate(size_t size)
    {
        if (size > size_)
            return false;

        size_ = size;
        return true;
    }

    bool push_back(T&& e)
    {
        if (size_ == capacity_)
        {
            if (!reserve(2*size_ + 1))
                return false;
        }
        arr_[size_++] = stdlib::move(e);
        return true;
    }

    bool push_back(const T& e)
    {
        if (size_ == capacity_)
        {
            if (!reserve(2*size_ + 1))
                return false;
        }
        arr_[size_++] = e;
        return true;
    }

    const T* get_const_buf() const
    {
        return arr_;
    }

    T* get_buf()
    {
        return arr_;
    }

    virtual ~Vector()
    {
        release();
    }

    void clear()
    {
        release();
    }

    Vector(Vector&& other)
        : Vector()
    {
        arr_ = other.arr_;
        size_ = other.size_;
        capacity_ = other.capacity_;
        other.arr_ = nullptr;
        other.size_ = 0;
        other.capacity_ = 0;
    }

    Vector& operator=(Vector&& other)
    {
        release();
        arr_ = other.arr_;
        size_ = other.size_;
        capacity_ = other.capacity_;
        other.arr_ = nullptr;
        other.size_ = 0;
        other.capacity_ = 0;
        return *this;
    }

    Vector(const Vector& other, stdlib::Error& err)
        : Vector()
    {
        if (!err.ok())
            return;

        arr_ = new (kernel::mm::NO_THROW) T[other.capacity_];
        if (!arr_)
        {
            err = make_error(stdlib::Error::NO_MEMORY);
            return;
        }

        for (size_t i = 0; i < other.size_; i++)
        {
            arr_[i] = other.arr_[i];
        }
        size_ = other.size_;
        capacity_ = other.capacity_;
    }

    void shuffle()
    {
        PANIC("Not implemented yet!");
    }

    Vector(const T* arr, size_t size, stdlib::Error& err)
        : Vector()
    {
        if (!err.ok())
            return;

        arr_ = new (kernel::mm::NO_THROW) T[size];
        if (!arr_)
        {
            err = make_error(stdlib::Error::NO_MEMORY);
            return;
        }

        for (size_t i = 0; i < size; i++)
        {
            arr_[i] = arr[i];
        }

        size_ = size;
        capacity_ = size_;
    }

private:
    Vector(const Vector& other) = delete;
    Vector& operator=(const Vector& other) = delete;

    void release()
    {
        if (arr_)
        {
            delete[] arr_;
            arr_ = nullptr;
        }
        size_ = 0;
        capacity_ = 0;
    }

    T* arr_;
    size_t size_;
    size_t capacity_;
};

}
