#include "stdlib.h"

extern "C"
{
void asm_memset(void* ptr, unsigned char c, size_t size);
void asm_memcpy(void* dst, const void* src, size_t size);
void asm_memmove(void* dst, const void* src, size_t size);
int asm_memcmp(const void* p1, const void* p2, size_t size);
size_t asm_strlen(const char* s);
int asm_strcmp(const char* s1, const char* s2);
const char* asm_strstr(const char* haystack, const char* needle);
}

namespace stdlib
{

void *mem_add(void *ptr, unsigned long len)
{
    return reinterpret_cast<void *>(reinterpret_cast<unsigned long>(ptr) + len);
}

const void *mem_add(const void *ptr, unsigned long len)
{
    return reinterpret_cast<const void *>(reinterpret_cast<unsigned long>(ptr) + len);
}

void mem_set(void* ptr, unsigned char c, size_t size)
{
    asm_memset(ptr, c, size);
}

int mem_cmp(const void* ptr1, const void* ptr2, size_t size)
{
    return asm_memcmp(ptr1, ptr2, size);
}

void mem_cpy(void* dst, const void* src, size_t size)
{
    asm_memcpy(dst, src, size);
}

void mem_move(void* dst, const void* src, size_t size)
{
    asm_memmove(dst, src, size);
}

size_t str_len(const char* s)
{
    return asm_strlen(s);
}

const char *truncate_file_name(const char *file_name)
{
    const char *base, *last_sep = nullptr;

    base = file_name;
    for (;;)
    {
        if (*base == '\0')
            break;

        if (*base == '/')
        {
            last_sep = base;
        }
        base++;
    }

    if (last_sep)
        return last_sep + 1;
    else
        return file_name;
}

void byte_swap(u8 *b1, u8 *b2)
{
    u8 tmp = *b1;
    *b1 = *b2;
    *b2 = tmp;
}

void mem_reverse(void *mem, size_t size)
{
    u8 *p_mem = (u8 *)mem;
    size_t i;

    for (i = 0; i < size/2; i++)
        byte_swap(&p_mem[i], &p_mem[size - i - 1]);
}

int str_cmp(const char *s1, const char *s2)
{
    return asm_strcmp(s1, s2);
}

int strn_cmp(const char *s1, const char *s2, size_t size)
{
    size_t i;

    for (i = 0; i < size; i++)
    {
        const unsigned char c1 = (unsigned char)*s1, c2 = (unsigned char)*s2;

        if (c1 < c2)
            return -1;
        if (c1 > c2)
            return 1;
        else
            if (c1 == '\0')
                break;

        s1++;
        s2++;
    }

    return 0;
}

void strn_cpy(char *dst, const char *src, size_t size)
{
    if (size == 0)
        return;

    size_t i;
    for (i = 0; i + 1 < size; i++)
    {
        const char c = src[i];
        dst[i] = c;
        if (c == '\0')
            return;
    }
    dst[i] = '\0';
}

// find n : 2^n >= size
size_t log2(size_t size)
{
    if (size < 2) {
        return 0;
    }

	size_t log = 0;
    size_t rest_size = size;
    while (rest_size != 0)
    {
        rest_size >>= 1;
        log++;
    }

    return (size & (size - 1)) ? log : (log - 1);
}

size_t hash_ptr(void *ptr)
{
    ulong val = (ulong)ptr;
    size_t hash, i, c;

    hash = 5381;
    val = val >> 3;
    for (i = 0; i < sizeof(val); i++) {
        c = (unsigned char)val & 0xFF;
        hash = ((hash << 5) + hash) + c;
        val = val >> 8;
    }

    return hash;
}

const char* str_chr(const char* s, char c)
{
    for (const char* curr = s; *curr != '\0'; curr++)
    {
        if (*curr == c)
            return curr;
    }
    return nullptr;
}

const char* str_chr_once(const char* s, char sep)
{
    const char* curr = s;
    const char* res = nullptr;

    for (;;)
    {
        if (*curr == '\0')
        {
            return res;
        }

        if (*curr == sep)
        {
            if (res == nullptr)
            {
                res = curr;
            }
            else
            {
                return nullptr;
            }
        }
        curr++;
    }
}

bool is_value_in_range(ulong value, ulong base, ulong limit)
{
    if (value >= base && value < limit)
        return true;

    return false;
}

bool parse_ulong(const char* s, ulong& result)
{
    result = 0;
    if (!s || *s == '\0')
        return false;

    const ulong max_value = (ulong)-1;
    while (*s)
    {
        if (*s < '0' || *s > '9')
            return false;
        ulong digit = (ulong)(*s - '0');
        if (result > (max_value - digit) / 10)
            return false;
        result = result * 10 + digit;
        s++;
    }
    return true;
}

u8 hex_char_to_nibble(char c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return 0xFF;
}

bool hex_decode(const char* hex, ulong hex_len, u8* out, ulong out_size, ulong& bytes_written)
{
    bytes_written = 0;
    for (ulong i = 0; i + 1 < hex_len && bytes_written < out_size; i += 2)
    {
        u8 hi = hex_char_to_nibble(hex[i]);
        u8 lo = hex_char_to_nibble(hex[i + 1]);
        if (hi == 0xFF || lo == 0xFF)
            return false;
        out[bytes_written++] = (hi << 4) | lo;
    }
    return true;
}

const char* next_token(const char* s, const char*& end)
{
    while (*s == ' ') s++;
    if (*s == '\0') { end = s; return nullptr; }
    const char* start = s;
    while (*s && *s != ' ') s++;
    end = s;
    return start;
}

ulong token_copy(const char* start, const char* end, char* dst, ulong dst_size)
{
    if (dst_size == 0)
        return 0;

    ulong len = (ulong)(end - start);
    if (len >= dst_size)
        len = dst_size - 1;
    mem_cpy(dst, start, len);
    dst[len] = '\0';
    return len;
}

const char* str_str(const char* haystack, const char* needle)
{
    return asm_strstr(haystack, needle);
}

}

extern "C"
{

void *memcpy(void *dst, const void *src, size_t size)
{
    stdlib::mem_cpy(dst, src, size);
    return dst;
}

void *memset(void *ptr, int c, size_t size)
{
    stdlib::mem_set(ptr, static_cast<unsigned char>(c), size);
    return ptr;
}

int memcmp(const void *s1, const void *s2, size_t n)
{
    return stdlib::mem_cmp(s1, s2, n);
}

void *memmove(void *dst, const void *src, size_t size)
{
    stdlib::mem_move(dst, src, size);
    return dst;
}

}
