#include "stdlib.h"

namespace Stdlib
{

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

char GetHexDigitLower(u8 val)
{
    if (val >= 16)
        return '\0';

    if (val < 10)
        return GetDecDigit(val);
    return 'a' + (val - 10);
}

int __UlongToString(ulong src, u8 dst_base, char *dst, size_t dst_size, bool lowercase)
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
            digit = lowercase ? GetHexDigitLower(val) : GetHexDigit(val);
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

int UlongToString(ulong src, u8 dst_base, char *dst, size_t dst_size, bool lowercase)
{
    int pos;

    pos = __UlongToString(src, dst_base, dst, dst_size, lowercase);
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
            char tp = fmt[i];

            if (tp == '\0')
                return -1;

            /* Literal %% */
            if (tp == '%') {
                if (!PutChar('%', s, size, pos++))
                    return -1;
                i++;
                continue;
            }

            /* Parse flags */
            bool zeroPad = false;
            bool leftAlign = false;
            while (tp == '0' || tp == '-') {
                if (tp == '0')
                    zeroPad = true;
                else
                    leftAlign = true;
                i++;
                tp = fmt[i];
                if (tp == '\0')
                    return -1;
            }
            if (leftAlign)
                zeroPad = false;

            /* Parse optional width */
            int width = 0;
            while (tp >= '0' && tp <= '9') {
                width = width * 10 + (tp - '0');
                i++;
                tp = fmt[i];
                if (tp == '\0')
                    return -1;
            }

            /* Skip optional 'l' length modifier */
            if (tp == 'l') {
                i++;
                tp = fmt[i];
                if (tp == '\0')
                    return -1;
            }

            i++; /* consume the specifier */

            switch (tp) {
            case 'u': {
                ulong val = va_arg(arg, ulong);
                char tmp[24];
                rc = __UlongToString(val, 10, tmp, sizeof(tmp));
                if (rc < 0)
                    return -1;
                if (!leftAlign) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!PutChar(zeroPad ? '0' : ' ', s, size, pos++))
                            return -1;
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!PutChar(tmp[j], s, size, pos++))
                        return -1;
                }
                if (leftAlign) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!PutChar(' ', s, size, pos++))
                            return -1;
                    }
                }
                break;
            }
            case 'd': {
                long val = va_arg(arg, long);
                bool negative = (val < 0);
                ulong uval = negative ? (ulong)(-(val + 1)) + 1 : (ulong)val;
                char tmp[24];
                rc = __UlongToString(uval, 10, tmp, sizeof(tmp));
                if (rc < 0)
                    return -1;
                int totalLen = rc + (negative ? 1 : 0);
                if (!leftAlign) {
                    if (negative && zeroPad) {
                        /* Sign before zero padding: -00042 */
                        if (!PutChar('-', s, size, pos++))
                            return -1;
                        for (int p = 0; p < width - totalLen; p++) {
                            if (!PutChar('0', s, size, pos++))
                                return -1;
                        }
                    } else {
                        for (int p = 0; p < width - totalLen; p++) {
                            if (!PutChar(zeroPad ? '0' : ' ', s, size, pos++))
                                return -1;
                        }
                        if (negative) {
                            if (!PutChar('-', s, size, pos++))
                                return -1;
                        }
                    }
                } else {
                    if (negative) {
                        if (!PutChar('-', s, size, pos++))
                            return -1;
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!PutChar(tmp[j], s, size, pos++))
                        return -1;
                }
                if (leftAlign) {
                    for (int p = 0; p < width - totalLen; p++) {
                        if (!PutChar(' ', s, size, pos++))
                            return -1;
                    }
                }
                break;
            }
            case 'x':
            case 'X': {
                ulong val = va_arg(arg, ulong);
                char tmp[24];
                rc = __UlongToString(val, 16, tmp, sizeof(tmp), tp == 'x');
                if (rc < 0)
                    return -1;
                if (!leftAlign) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!PutChar(zeroPad ? '0' : ' ', s, size, pos++))
                            return -1;
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!PutChar(tmp[j], s, size, pos++))
                        return -1;
                }
                if (leftAlign) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!PutChar(' ', s, size, pos++))
                            return -1;
                    }
                }
                break;
            }
            case 'p': {
                if (sizeof(void *) != sizeof(ulong))
                    return -1;
                void *val = va_arg(arg, void *);
                ulong uval = (ulong)val;
                char tmp[24];
                rc = __UlongToString(uval, 16, tmp, sizeof(tmp));
                if (rc < 0)
                    return -1;
                if (!leftAlign) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!PutChar(zeroPad ? '0' : ' ', s, size, pos++))
                            return -1;
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!PutChar(tmp[j], s, size, pos++))
                        return -1;
                }
                if (leftAlign) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!PutChar(' ', s, size, pos++))
                            return -1;
                    }
                }
                break;
            }
            case 'c': {
                int val = va_arg(arg, int);
                if (!PutChar(val & 0xFF, s, size, pos++))
                    return -1;
                break;
            }
            case 's': {
                const char *val = va_arg(arg, char *);
                if (val == nullptr)
                    val = "(null)";
                size_t val_len = StrLen(val);
                if (!leftAlign) {
                    for (int p = 0; p < width - (int)val_len; p++) {
                        if (!PutChar(' ', s, size, pos++))
                            return -1;
                    }
                }
                if (val_len > (size - pos))
                    return -1;
                MemCpy(&s[pos], val, val_len);
                pos += val_len;
                if (leftAlign) {
                    for (int p = 0; p < width - (int)val_len; p++) {
                        if (!PutChar(' ', s, size, pos++))
                            return -1;
                    }
                }
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
