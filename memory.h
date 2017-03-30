#pragma once

#include "types.h"

namespace Shared
{

template< class T > struct RemoveReference      {typedef T type;};
template< class T > struct RemoveReference<T&>  {typedef T type;};
template< class T > struct RemoveReference<T&&> {typedef T type;};

template <typename T>
typename RemoveReference<T>::type&& Move(T&& arg)
{
    return static_cast<typename RemoveReference<T>::type&&>(arg);
}

template <class T>
T&& Forward(typename RemoveReference<T>::type& arg)
{
    return static_cast<T&&>(arg);
}

template <class T>
T&& Forward(typename RemoveReference<T>::type&& arg)
{
    return static_cast<T&&>(arg);
}

template <class T> void Swap(T& a, T& b)
{
    T c(Move(a)); a=Move(b); b=Move(c);
}

template <typename T,unsigned S> unsigned ArraySize(const T (&v)[S])
{
    (void)v;
    return S;
}

template <typename T,unsigned S> bool ArrayEqual(const T (&s1)[S], const T (&s2)[S])
{
    for (size_t i = 0; i < S; i++)
    {
        if (s1[i] != s2[i])
            return false;
    }

    return true;
}

template <typename T>
T Min(const T& a, const T& b)
{
    if (a < b)
        return a;

    return b;
}

template <typename T>
T Max(const T& a, const T& b)
{
    if (a > b)
        return a;

    return b;
}

template <typename T>
bool CheckRange(const T& start, const T& end)
{
    if (end <= start)
        return false;
    return true;
}

template <typename T>
bool CheckIntersection(const T& start1, const T& end1, const T& start2, const T& end2)
{
    if (!CheckRange(start1, end1))
        return false;
    if (!CheckRange(start2, end2))
        return false;
    if (start2 >= end1)
        return false;
    if (end2 <= start1)
        return false;
    return true;
}

template <typename T>
size_t SizeOfInBits()
{
    return 8 * sizeof(T);
}

void *MemAdd(void *ptr, unsigned long len);

const void *MemAdd(const void *ptr, unsigned long len);

void MemSet(void* ptr, unsigned char c, size_t size);

int MemCmp(const void* ptr1, const void* ptr2, size_t size);

void MemCpy(void* dst, const void* src, size_t size);

size_t StrLen(const char* s);

const char *TruncateFileName(const char *fileName);

void ByteSwap(u8 *b1, u8 *b2);

void MemReverse(void *mem, size_t size);

bool PutChar(char c, char *dst, size_t dst_size, size_t pos);

char GetDecDigit(u8 val);

char GetHexDigit(u8 val);

int __UlongToString(ulong src, u8 dst_base, char *dst, size_t dst_size);

int UlongToString(ulong src, u8 dst_base, char *dst, size_t dst_size);

int VsnPrintf(char *s, size_t size, const char *fmt, va_list arg);

int SnPrintf(char* buf, size_t size, const char* fmt, ...);

}