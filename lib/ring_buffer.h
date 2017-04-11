#pragma once

#include "stdlib.h"
#include "error.h"
#include "lock.h"

#include <kernel/panic.h>

namespace Shared
{

template <typename T, size_t Capacity = Shared::PageSize, typename LockType = Shared::NopLock>
class RingBuffer final
{
public:
    class Dumper
    {
    public:
        virtual void OnElement(const T& element) = 0;
    };

    RingBuffer()
    {
        Shared::AutoLock lock(Lock);

        Size = 0;
        StartIndex = 0;
        EndIndex = 0;
    }

    ~RingBuffer()
    {
        Shared::AutoLock lock(Lock);
    }

    bool Put(const T& value)
    {
        Shared::AutoLock lock(Lock);

        if (Size == Capacity)
        {
            return false;
        }

        size_t position = EndIndex;
        Buf[position] = value;
        EndIndex = (EndIndex + 1) % Capacity;
        Size++;
        return true;
    }

    bool Put(T&& value)
    {
        Shared::AutoLock lock(Lock);

        if (Size == Capacity)
        {
            return false;
        }

        size_t position = EndIndex;
        Buf[position] = Shared::Move(value);
        EndIndex = (EndIndex + 1) % Capacity;
        Size++;
        return true;
    }

    bool IsEmpty()
    {
        Shared::AutoLock lock(Lock);

        return (Size == 0) ? true : false;
    }

    bool IsFull()
    {
        Shared::AutoLock lock(Lock);

        return (Size == Capacity) ? true : false;
    }

    size_t GetSize()
    {
        Shared::AutoLock lock(Lock);

        return Size;
    }

    size_t GetCapacity()
    {
        Shared::AutoLock lock(Lock);

        return Capacity;
    }

    T Get()
    {
        Shared::AutoLock lock(Lock);

        if (Size == 0)
        {
            return T();
        }

        size_t position = StartIndex;
        StartIndex = (StartIndex + 1) % Capacity;
        Size--;
        return Buf[position];
    }

    void PopHead()
    {
        Shared::AutoLock lock(Lock);

        if (Size == 0)
            return;

        StartIndex = (StartIndex + 1) % Capacity;
        Size--;
    }

    void Clear()
    {
        Shared::AutoLock lock(Lock);

        Size = 0;
        StartIndex = 0;
        EndIndex = 0;
    }

    void Dump(Dumper& dumper)
    {
        Shared::AutoLock lock(Lock);

        if (Size == 0)
            return;

        size_t index = StartIndex;
        for (size_t i = 0; i < Size; i++)
        {
            dumper.OnElement(Buf[index]);
            index = (index + 1) % Capacity;
        }
    }

private:
    RingBuffer(const RingBuffer& other) = delete;
    RingBuffer(RingBuffer&& other) = delete;
    RingBuffer& operator=(const RingBuffer& other) = delete;
    RingBuffer& operator=(RingBuffer&& other) = delete;

    size_t StartIndex;
    size_t EndIndex;
    size_t Size;
    T Buf[Capacity];

    LockType Lock;
};

}
