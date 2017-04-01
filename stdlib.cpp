#include "stdlib.h"

namespace Shared
{

void *MemAdd(void *ptr, unsigned long len)
{
    return reinterpret_cast<void *>(reinterpret_cast<unsigned long>(ptr) + len);
}

const void *MemAdd(const void *ptr, unsigned long len)
{
    return reinterpret_cast<const void *>(reinterpret_cast<unsigned long>(ptr) + len);
}

void MemSet(void* ptr, unsigned char c, size_t size)
{
    unsigned char *p = static_cast<unsigned char *>(ptr);

    for (size_t i = 0; i < size; i++)
    {
        *p = c;
        p++;
    }
}

int MemCmp(const void* ptr1, const void* ptr2, size_t size)
{
    const unsigned char *p1 = static_cast<const unsigned char *>(ptr1);
    const unsigned char *p2 = static_cast<const unsigned char *>(ptr2);

    for (size_t i = 0; i < size; i++)
    {
        if (*p1 > *p2)
            return 1;
        else if (*p1 < *p2)
            return -1;

        p1++;
        p2++;
    }

    return 0;
}

void MemCpy(void* dst, const void* src, size_t size)
{
    unsigned char *pdst = static_cast<unsigned char *>(dst);
    const unsigned char *psrc = static_cast<const unsigned char *>(src);

    for (size_t i = 0; i < size; i++)
    {
        *pdst = *psrc;
        pdst++;
        psrc++;
    }
}

size_t StrLen(const char* s)
{
    size_t i = 0;
    while (s[i] != 0)
    {
        i++;
    }
    return i;
}

const char *TruncateFileName(const char *fileName)
{
    const char *base, *lastSep = nullptr;

    base = fileName;
    for (;;)
    {
        if (*base == '\0')
            break;

        if (*base == '/')
        {
            lastSep = base;
        }
        base++;
    }

    if (lastSep)
        return lastSep + 1;
    else
        return fileName;
}

void ByteSwap(u8 *b1, u8 *b2)
{
    u8 tmp = *b1;
    *b1 = *b2;
    *b2 = tmp;
}

void MemReverse(void *mem, size_t size)
{
    u8 *p_mem = (u8 *)mem;
    size_t i;

    for (i = 0; i < size/2; i++)
        ByteSwap(&p_mem[i], &p_mem[size - i - 1]);
}

bool PutChar(char c, char *dst, size_t dst_size, size_t pos)
{
    if (pos >= dst_size)
        return false;
    dst[pos] = c;
    return true;
}

char GetDecDigit(u8 val)
{
    if (val >= 10)
        return '\0';
    return '0' + val;
}

char GetHexDigit(u8 val)
{
    if (val >= 16)
        return '\0';

    if (val < 10)
        return GetDecDigit(val);
    return 'A' + (val - 10);
}

int __UlongToString(ulong src, u8 dst_base, char *dst, size_t dst_size)
{
    size_t pos;
    u8 val;
    char digit;

    if (dst_base != 10 && dst_base != 16)
        return -1;

    pos = 0;
    if (src == 0) {
        if (!PutChar('0', dst, dst_size, pos++))
            return -1;
        goto put_prefix;
    }
    while (src) {
        val = src % dst_base;
        src = src/dst_base;
        switch (dst_base) {
        case 10:
            digit = GetDecDigit(val);
            break;
        case 16:
            digit = GetHexDigit(val);
            break;
        default:
            return -1;
        }
        if (digit == '\0')
            return -1;
        if (!PutChar(digit, dst, dst_size, pos++))
            return -1;
    }
put_prefix:
    MemReverse(dst, pos);
    return pos;
}

int UlongToString(ulong src, u8 dst_base, char *dst, size_t dst_size)
{
    int pos;

    pos = __UlongToString(src, dst_base, dst, dst_size);
    if (pos < 0)
        return -1;
    if ((size_t)pos >= dst_size)
        return -1;
    dst[pos] = '\0';
    return 0;
}

int VsnPrintf(char *s, size_t size, const char *fmt, va_list arg)
{
    size_t i;
    char t;
    int pos;
    int rc;

    i = 0;
    pos = 0;
    while (true) {
        t = fmt[i++];
        if (t == '\0')
            break;
        if (t == '%') {
            char tp = fmt[i++];

            if (tp == '\0')
                return -1;
            switch (tp) {
            case 'u': {
                ulong val;

                val = va_arg(arg, ulong);
                rc = __UlongToString(val, 10, &s[pos],
                            size - pos);
                if (rc < 0)
                    return -1;
                pos += rc;
                break;
            }
            case 'c': {
                int val;
                val = va_arg(arg, int);
                s[pos++] = val & 0xFF;
                break;
            }
            case 'p': {
                void *val;
                ulong uval;

                if (sizeof(void *) != sizeof(ulong))
                    return -1;

                val = va_arg(arg, void *);
                uval = (ulong)val;
                rc = __UlongToString(uval, 16, &s[pos],
                            size - pos);
                if (rc < 0)
                    return -1;
                pos += rc;
                break;
            }
            case 's': {
                char *val;
                size_t val_len;

                val = va_arg(arg, char *);
                val_len = StrLen(val);
                if (val_len > (size - pos))
                    return -1;
                MemCpy(&s[pos], val, val_len);
                pos += val_len;
                break;
            }
            default:
                return -1;
            }
        } else
            if (!PutChar(t, s, size, pos++))
                return -1;
    }

    if (!PutChar('\0', s, size, pos++))
        return -1;

    return pos;
}

int SnPrintf(char* buf, size_t size, const char* fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    int len = VsnPrintf(buf, size, fmt, args);
    va_end(args);

    return len;
}

}
