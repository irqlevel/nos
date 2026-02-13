#include "stdlib.h"

extern "C"
{
void asm_memset(void* ptr, unsigned char c, size_t size);
void asm_memcpy(void* dst, const void* src, size_t size);
int asm_memcmp(const void* p1, const void* p2, size_t size);
size_t asm_strlen(const char* s);
int asm_strcmp(const char* s1, const char* s2);
}

namespace Stdlib
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
    asm_memset(ptr, c, size);
}

int MemCmp(const void* ptr1, const void* ptr2, size_t size)
{
    return asm_memcmp(ptr1, ptr2, size);
}

void MemCpy(void* dst, const void* src, size_t size)
{
    asm_memcpy(dst, src, size);
}

size_t StrLen(const char* s)
{
    return asm_strlen(s);
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
                if (!PutChar(val & 0xFF, s, size, pos++))
                    return -1;
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
                const char *val;
                size_t val_len;

                val = va_arg(arg, char *);
                if (val == nullptr)
                    val = "(null)";
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

int StrCmp(const char *s1, const char *s2)
{
    return asm_strcmp(s1, s2);
}

int StrnCmp(const char *s1, const char *s2, size_t size)
{
    size_t i;

    for (i = 0; i < size; i++)
    {
        const char c1 = *s1, c2 = *s2;

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

void StrnCpy(char *dst, const char *src, size_t size)
{
    for (size_t i = 0; i < size; i++)
    {
        const char c = src[i];
        dst[i] = c;
        if (c == '\0')
            break;
    }
}

// find n : 2^n >= size
size_t Log2(size_t size)
{
    if (size < 2) {
        return 0;
    }

	size_t log = 0;
    size_t restSize = size;
    while (restSize != 0)
    {
        restSize >>= 1;
        log++;
    }

    return (size & (size - 1)) ? log : (log - 1);
}

size_t HashPtr(void *ptr)
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

const char* StrChrOnce(const char* s, char sep)
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

bool IsValueInRange(ulong value, ulong base, ulong limit)
{
    if (value >= base && value < limit)
        return true;

    return false;
}

bool ParseUlong(const char* s, ulong& result)
{
    result = 0;
    if (!s || *s == '\0')
        return false;

    while (*s)
    {
        if (*s < '0' || *s > '9')
            return false;
        result = result * 10 + (*s - '0');
        s++;
    }
    return true;
}

u8 HexCharToNibble(char c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return 0xFF;
}

bool HexDecode(const char* hex, ulong hexLen, u8* out, ulong outSize, ulong& bytesWritten)
{
    bytesWritten = 0;
    for (ulong i = 0; i + 1 < hexLen && bytesWritten < outSize; i += 2)
    {
        u8 hi = HexCharToNibble(hex[i]);
        u8 lo = HexCharToNibble(hex[i + 1]);
        if (hi == 0xFF || lo == 0xFF)
            return false;
        out[bytesWritten++] = (hi << 4) | lo;
    }
    return true;
}

const char* NextToken(const char* s, const char*& end)
{
    while (*s == ' ') s++;
    if (*s == '\0') { end = s; return nullptr; }
    const char* start = s;
    while (*s && *s != ' ') s++;
    end = s;
    return start;
}

ulong TokenCopy(const char* start, const char* end, char* dst, ulong dstSize)
{
    ulong len = (ulong)(end - start);
    if (len >= dstSize)
        len = dstSize - 1;
    MemCpy(dst, start, len);
    dst[len] = '\0';
    return len;
}

}

extern "C"
{

void *memcpy(void *dst, const void *src, size_t size)
{
    Stdlib::MemCpy(dst, src, size);
    return dst;
}

void *memset(void *ptr, int c, size_t size)
{
    Stdlib::MemSet(ptr, static_cast<unsigned char>(c), size);
    return ptr;
}

}
