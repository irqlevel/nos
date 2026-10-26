#pragma once

#include "stdlib.h"
#include "error.h"
#include "lock.h"
#include "printer.h"

#include <kernel/panic.h>

namespace stdlib
{

template <typename T, size_t CAPACITY = consts::PAGE_SIZE, typename LockType = stdlib::NopLock>
class RingBuffer final
{
public:
    class Dumper
    {
    public:
        virtual void on_element(const T& element) = 0;
    };

    RingBuffer()
    {
        stdlib::AutoLock lock(lock_);

        size_ = 0;
        start_index_ = 0;
        end_index_ = 0;
    }

    ~RingBuffer()
    {
        stdlib::AutoLock lock(lock_);
    }

    bool put(const T& value)
    {
        stdlib::AutoLock lock(lock_);

        if (size_ == CAPACITY)
        {
            return false;
        }

        size_t position = end_index_;
        buf_[position] = value;
        end_index_ = (end_index_ + 1) % CAPACITY;
        size_++;
        return true;
    }

    bool put(T&& value)
    {
        stdlib::AutoLock lock(lock_);

        if (size_ == CAPACITY)
        {
            return false;
        }

        size_t position = end_index_;
        buf_[position] = stdlib::move(value);
        end_index_ = (end_index_ + 1) % CAPACITY;
        size_++;
        return true;
    }

    bool is_empty()
    {
        stdlib::AutoLock lock(lock_);

        return (size_ == 0) ? true : false;
    }

    bool is_full()
    {
        stdlib::AutoLock lock(lock_);

        return (size_ == CAPACITY) ? true : false;
    }

    size_t get_size()
    {
        stdlib::AutoLock lock(lock_);

        return size_;
    }

    size_t get_capacity()
    {
        stdlib::AutoLock lock(lock_);

        return CAPACITY;
    }

    T get()
    {
        stdlib::AutoLock lock(lock_);

        if (size_ == 0)
        {
            return T();
        }

        size_t position = start_index_;
        start_index_ = (start_index_ + 1) % CAPACITY;
        size_--;
        return buf_[position];
    }

    void pop_head()
    {
        stdlib::AutoLock lock(lock_);

        if (size_ == 0)
            return;

        start_index_ = (start_index_ + 1) % CAPACITY;
        size_--;
    }

    void clear()
    {
        stdlib::AutoLock lock(lock_);

        size_ = 0;
        start_index_ = 0;
        end_index_ = 0;
    }

    void print(TypePrinter<T>& printer)
    {
        stdlib::AutoLock lock(lock_);

        if (size_ == 0)
            return;

        size_t index = start_index_;
        for (size_t i = 0; i < size_; i++)
        {
            printer.print_element(buf_[index]);
            index = (index + 1) % CAPACITY;
        }
    }

private:
    RingBuffer(const RingBuffer& other) = delete;
    RingBuffer(RingBuffer&& other) = delete;
    RingBuffer& operator=(const RingBuffer& other) = delete;
    RingBuffer& operator=(RingBuffer&& other) = delete;

    size_t start_index_;
    size_t end_index_;
    size_t size_;
    T buf_[CAPACITY];

    LockType lock_;
};

}
