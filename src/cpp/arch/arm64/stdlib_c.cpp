#include <lib/stdlib.h>

/* Portable C++ implementations of the asm_* string/memory routines that
   x86 provides in arch/x86_64/stdlib_asm.asm. -fno-builtin prevents the
   compiler from turning the loops back into libcalls. Optimize later. */

extern "C"
{

void asm_memset(void* ptr, unsigned char c, size_t size)
{
    unsigned char* p = static_cast<unsigned char*>(ptr);
    for (size_t i = 0; i < size; i++)
        p[i] = c;
}

void asm_memcpy(void* dst, const void* src, size_t size)
{
    unsigned char* d = static_cast<unsigned char*>(dst);
    const unsigned char* s = static_cast<const unsigned char*>(src);
    for (size_t i = 0; i < size; i++)
        d[i] = s[i];
}

void asm_memmove(void* dst, const void* src, size_t size)
{
    unsigned char* d = static_cast<unsigned char*>(dst);
    const unsigned char* s = static_cast<const unsigned char*>(src);
    if (d == s || size == 0)
        return;
    if (d < s)
    {
        for (size_t i = 0; i < size; i++)
            d[i] = s[i];
    }
    else
    {
        for (size_t i = size; i != 0; i--)
            d[i - 1] = s[i - 1];
    }
}

int asm_memcmp(const void* p1, const void* p2, size_t size)
{
    const unsigned char* a = static_cast<const unsigned char*>(p1);
    const unsigned char* b = static_cast<const unsigned char*>(p2);
    for (size_t i = 0; i < size; i++)
    {
        if (a[i] != b[i])
            return (a[i] < b[i]) ? -1 : 1;
    }
    return 0;
}

size_t asm_strlen(const char* s)
{
    size_t len = 0;
    while (s[len] != '\0')
        len++;
    return len;
}

int asm_strcmp(const char* s1, const char* s2)
{
    size_t i = 0;
    for (;;)
    {
        unsigned char a = (unsigned char)s1[i];
        unsigned char b = (unsigned char)s2[i];
        if (a != b)
            return (a < b) ? -1 : 1;
        if (a == '\0')
            return 0;
        i++;
    }
}

const char* asm_strstr(const char* haystack, const char* needle)
{
    if (needle[0] == '\0')
        return haystack;

    for (size_t i = 0; haystack[i] != '\0'; i++)
    {
        size_t j = 0;
        while (needle[j] != '\0' && haystack[i + j] == needle[j])
            j++;
        if (needle[j] == '\0')
            return &haystack[i];
    }
    return nullptr;
}

}
