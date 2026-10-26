#pragma once

#include <include/types.h>
#include <include/const.h>

namespace stdlib
{

struct Time
{
    Time()
        : nano_secs(0)
    {
    }

    Time(ulong nano_secs)
        : nano_secs(nano_secs)
    {
    }

    Time(const Time& other)
    {
        nano_secs = other.nano_secs;
    }

    Time& operator=(const Time& other)
    {
        if (this != &other)
        {
            nano_secs = other.nano_secs;
        }
        return *this;
    }

    void reset()
    {
        nano_secs = 0;
    }

    Time operator+(const Time& other) const
    {
        return nano_secs + other.nano_secs;
    }

    Time operator-(const Time& other) const
    {
        if (nano_secs <= other.nano_secs)
            return Time(0);
        else
            return nano_secs - other.nano_secs;
    }

    Time& operator+=(const Time& other)
    {
        nano_secs += other.nano_secs;;
        return *this;
    }

    Time& operator-=(const Time& other)
    {
        if (nano_secs <= other.nano_secs)
           nano_secs = 0;
        else
            nano_secs -= other.nano_secs;
        return *this;
    }

    ulong get_value()
    {
        return nano_secs;
    }

    void clear()
    {
        nano_secs = 0;
    }

    bool operator==(const Time& other) const
    {
        return (nano_secs == other.nano_secs) ? true : false;
    }

    bool operator<(const Time& other) const
    {
        return (nano_secs < other.nano_secs) ? true : false;
    }

    bool operator>=(const Time& other) const
    {
        return (nano_secs >= other.nano_secs) ? true : false;
    }

    bool operator>(const Time& other) const
    {
        return (nano_secs > other.nano_secs) ? true : false;
    }

    ulong get_secs()
    {
        return nano_secs / consts::NANO_SECS_IN_SEC;
    }

    ulong get_usecs()
    {
        return (nano_secs % consts::NANO_SECS_IN_SEC) / consts::NANO_SECS_IN_USEC;
    }

    ulong nano_secs;
};

template< class T > struct RemoveReference      {typedef T type;};
template< class T > struct RemoveReference<T&>  {typedef T type;};
template< class T > struct RemoveReference<T&&> {typedef T type;};

template <typename T>
typename RemoveReference<T>::type&& move(T&& arg)
{
    return static_cast<typename RemoveReference<T>::type&&>(arg);
}

template <class T>
T&& forward(typename RemoveReference<T>::type& arg)
{
    return static_cast<T&&>(arg);
}

template <class T>
T&& forward(typename RemoveReference<T>::type&& arg)
{
    return static_cast<T&&>(arg);
}

template <class T> void swap(T& a, T& b)
{
    T c(move(a)); a=move(b); b=move(c);
}

template <typename T,unsigned S> unsigned array_size(const T (&v)[S])
{
    (void)v;
    return S;
}

template <typename T,unsigned S> bool array_equal(const T (&s1)[S], const T (&s2)[S])
{
    for (size_t i = 0; i < S; i++)
    {
        if (s1[i] != s2[i])
            return false;
    }

    return true;
}

template <typename T>
T min(const T& a, const T& b)
{
    if (a < b)
        return a;

    return b;
}

template <typename T>
T max(const T& a, const T& b)
{
    if (a > b)
        return a;

    return b;
}

template <typename T>
bool check_range(const T& start, const T& end)
{
    if (end <= start)
        return false;
    return true;
}

template <typename T>
bool check_intersection(const T& start1, const T& end1, const T& start2, const T& end2)
{
    if (!check_range(start1, end1))
        return false;
    if (!check_range(start2, end2))
        return false;
    if (start2 >= end1)
        return false;
    if (end2 <= start1)
        return false;
    return true;
}

template <typename T>
size_t size_of_in_bits()
{
    return 8 * sizeof(T);
}

template <typename T>
int test_bit(const T& value, u8 bit)
{
    return (value & (static_cast<T>(1) << bit)) ? 1 : 0;
}

template <typename T>
void set_bit(T& value, u8 bit)
{
    value |= (static_cast<T>(1) << bit);
}

template <typename T>
void clear_bit(T& value, u8 bit)
{
    value &= ~(static_cast<T>(1) << bit);
}

static inline u32 high_part(u64 value)
{
    return ((u32)(((value) >> 32ULL) & 0x00000000FFFFFFFFULL));
}

static inline u32 low_part(u64 value)
{
    return ((u32)((value) & 0x00000000FFFFFFFFULL));
}

static inline u16 high_part(u32 value)
{
    return ((u16)(((value) >> 16) & 0x0000FFFF));
}

static inline u16 low_part(u32 value)
{
    return ((u16)((value) & 0x0000FFFF));
}

static inline u8 high_part(u16 value)
{
    return ((u8)(((value) >> 8) & 0x00FF));
}

static inline u8 low_part(u16 value)
{
    return ((u8)((value) & 0x00FF));
}

static inline size_t round_down(size_t value, size_t align)
{
    return (value / align) * align;
}

static inline size_t round_up(size_t value, size_t align)
{
    size_t i = value / align;
    return ((value % align) == 0) ? value : ((i + 1) * align);
}

static inline size_t size_in_pages(size_t size)
{
    return round_up(size, consts::PAGE_SIZE) / consts::PAGE_SIZE;
}

void *mem_add(void *ptr, unsigned long len);

const void *mem_add(const void *ptr, unsigned long len);

void mem_set(void* ptr, unsigned char c, size_t size);

int mem_cmp(const void* ptr1, const void* ptr2, size_t size);

void mem_cpy(void* dst, const void* src, size_t size);

void mem_move(void* dst, const void* src, size_t size);

size_t str_len(const char* s);

const char *truncate_file_name(const char *file_name);

void byte_swap(u8 *b1, u8 *b2);

void mem_reverse(void *mem, size_t size);

bool put_char(char c, char *dst, size_t dst_size, size_t pos);

char get_dec_digit(u8 val);

char get_hex_digit(u8 val);

char get_hex_digit_lower(u8 val);

int __UlongToString(ulong src, u8 dst_base, char *dst, size_t dst_size, bool lowercase = false);

int ulong_to_string(ulong src, u8 dst_base, char *dst, size_t dst_size, bool lowercase = false);

/* C's vsnprintf and snprintf, for the conversions d u x X p c s, the flags
   0 and -, a width, and the length modifiers hh h l ll z (PRINTF_FORMAT).
   What does not fit is cut, marked with "...", and the length written
   returned. */
PRINTF_FORMAT(3, 0) int vsn_printf(char *s, size_t size, const char *fmt, va_list arg);

PRINTF_FORMAT(3, 4) int sn_printf(char* buf, size_t size, const char* fmt, ...);

int str_cmp(const char *s1, const char *s2);

int strn_cmp(const char *s1, const char *s2, size_t size);

/* Copies at most size-1 chars; dst is always null-terminated (if size > 0). */
void strn_cpy(char *dst, const char *s, size_t size);

/* The first occurrence of c in s, or nullptr */
const char* str_chr(const char* s, char c);

const char* str_chr_once(const char* s, char sep);

size_t log2(size_t size);

constexpr size_t const_log2(size_t v)
{
    size_t r = 0;
    while ((1UL << r) < v)
        r++;
    return r;
}

size_t hash_ptr(void *ptr);

bool is_value_in_range(ulong value, ulong base, ulong limit);

bool parse_ulong(const char* s, ulong& result);

u8 hex_char_to_nibble(char c);

bool hex_decode(const char* hex, ulong hex_len, u8* out, ulong out_size, ulong& bytes_written);

const char* next_token(const char* s, const char*& end);

ulong token_copy(const char* start, const char* end, char* dst, ulong dst_size);

const char* str_str(const char* haystack, const char* needle);

}