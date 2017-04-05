#pragma once

#include "types.h"
#include "const.h"

namespace Shared
{

struct Time
{
    Time()
        : Secs(0)
        , NanoSecs(0)
    {
    }

    void Add(const Time& other)
    {
        Secs += other.Secs;

        u64 nanoSecs = NanoSecs + other.NanoSecs;
        while (nanoSecs > 1000000000)
        {
            Secs+= 1;
            nanoSecs -= 1000000000;
        }
        NanoSecs = nanoSecs;
    }

    int Compare(const Time& other)
    {
        if (Secs < other.Secs)
            return -1;
        if (Secs > other.Secs)
            return 1;

        if (NanoSecs < other.NanoSecs)
            return -1;
        if (NanoSecs > other.NanoSecs)
            return 1;

        return 0;
    }

    ulong Secs;
    ulong NanoSecs;
};

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

template <typename T>
int TestBit(const T& value, u8 bit)
{
    return (value & (static_cast<T>(1) << bit)) ? 1 : 0;
}

template <typename T>
void SetBit(T& value, u8 bit)
{
    value |= (static_cast<T>(1) << bit);
}

template <typename T>
void ClearBit(T& value, u8 bit)
{
    value &= ~(static_cast<T>(1) << bit);
}

static inline u32 HighPart(u64 value)
{
    return ((u32)(((value) >> 32ULL) & 0x00000000FFFFFFFFULL));
}

static inline u32 LowPart(u64 value)
{
    return ((u32)((value) & 0x00000000FFFFFFFFULL));
}

static inline u16 HighPart(u32 value)
{
    return ((u16)(((value) >> 16) & 0x0000FFFF));
}

static inline u16 LowPart(u32 value)
{
    return ((u16)((value) & 0x0000FFFF));
}

static inline u8 HighPart(u16 value)
{
    return ((u8)(((value) >> 8) & 0x00FF));
}

static inline u8 LowPart(u16 value)
{
    return ((u8)((value) & 0x00FF));
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

int StrCmp(const char *s1, const char *s2);

int StrnCmp(const char *s1, const char *s2, size_t size);

}