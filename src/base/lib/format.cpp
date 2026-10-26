#include "stdlib.h"

namespace stdlib
{

bool put_char(char c, char *dst, size_t dst_size, size_t pos)
{
    if (pos >= dst_size)
        return false;
    dst[pos] = c;
    return true;
}

char get_dec_digit(u8 val)
{
    if (val >= 10)
        return '\0';
    return '0' + val;
}

char get_hex_digit(u8 val)
{
    if (val >= 16)
        return '\0';

    if (val < 10)
        return get_dec_digit(val);
    return 'A' + (val - 10);
}

char get_hex_digit_lower(u8 val)
{
    if (val >= 16)
        return '\0';

    if (val < 10)
        return get_dec_digit(val);
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
        if (!put_char('0', dst, dst_size, pos++))
            return -1;
        goto put_prefix;
    }
    while (src) {
        val = src % dst_base;
        src = src/dst_base;
        switch (dst_base) {
        case 10:
            digit = get_dec_digit(val);
            break;
        case 16:
            digit = lowercase ? get_hex_digit_lower(val) : get_hex_digit(val);
            break;
        default:
            return -1;
        }
        if (digit == '\0')
            return -1;
        if (!put_char(digit, dst, dst_size, pos++))
            return -1;
    }
put_prefix:
    mem_reverse(dst, pos);
    return pos;
}

int ulong_to_string(ulong src, u8 dst_base, char *dst, size_t dst_size, bool lowercase)
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

/* Standard vsnprintf always NUL-terminates when size > 0. Terminate at the
   last written position on every error/truncation path, so callers that
   ignore a -1 can never read past the buffer through an unterminated
   string. */
static void terminate_on_error(char *s, size_t size, size_t pos)
{
    if (size == 0)
        return;
    if (pos >= size)
        pos = size - 1;
    s[pos] = '\0';
}

/* The buffer filled up before the format string ran out: keep what fits,
   mark the cut with "..." so a reader can see the line is not all of it, and
   report the length written. The alternative -- what this did until a Rust
   panic came out as "RUST PANIC: " and nothing else -- was to return -1 and
   leave the caller to decide, which cost the whole line: the %s conversion
   dropped its entire argument rather than part of it, and Tracer::output
   dropped every line that reported -1. The reports that overflow a buffer
   are the long ones, which is to say the ones worth reading. */
static int truncate_at(char *s, size_t size, size_t pos)
{
    if (size == 0)
        return 0;
    if (pos >= size)
        pos = size - 1;

    static const size_t MARK_LEN = 3;    /* "..." */
    /* Where the mark has to start for both it and the NUL to fit. */
    size_t mark = (size - 1 > MARK_LEN) ? size - 1 - MARK_LEN : 0;
    if (pos > mark)
        pos = mark;
    while (pos < size - 1 && pos < mark + MARK_LEN)
        s[pos++] = '.';
    s[pos] = '\0';
    return (int)pos;
}

namespace
{

/* A length modifier, as C's printf reads one: the type an integer argument
   was passed as. -Wformat holds every call to it (PRINTF_FORMAT), so an
   argument is read as what it is -- never as a ulong where an int was
   passed, which left half the value to whatever the register or the stack
   slot held before. */
enum LengthModifier
{
    LengthNone,     /* int */
    LengthChar,     /* hh: a char, passed as int */
    LengthShort,    /* h: a short, passed as int */
    LengthLong,     /* l */
    LengthLongLong, /* ll */
    LengthSize,     /* z: a size_t */
};

/* The next argument of an unsigned conversion (u x X) */
ulong read_unsigned(va_list* ap, LengthModifier length)
{
    switch (length)
    {
    /* A char or a short, of either signedness, arrives promoted to int */
    case LengthChar:
        return (unsigned char)va_arg(*ap, int);
    case LengthShort:
        return (unsigned short)va_arg(*ap, int);
    case LengthLong:
        return va_arg(*ap, unsigned long);
    case LengthLongLong:
        return va_arg(*ap, unsigned long long);
    case LengthSize:
        return va_arg(*ap, size_t);
    case LengthNone:
        break;
    }
    return va_arg(*ap, unsigned int);
}

/* The next argument of a signed conversion (d) */
long read_signed(va_list* ap, LengthModifier length)
{
    switch (length)
    {
    case LengthChar:
        return (signed char)va_arg(*ap, int);
    case LengthShort:
        return (short)va_arg(*ap, int);
    case LengthLong:
        return va_arg(*ap, long);
    case LengthLongLong:
        return va_arg(*ap, long long);
    case LengthSize:
        return va_arg(*ap, ssize_t);
    case LengthNone:
        break;
    }
    return va_arg(*ap, int);
}

int format(char *s, size_t size, const char *fmt, va_list* ap)
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
                { terminate_on_error(s, size, (size_t)pos); return -1; }

            /* Literal %% */
            if (tp == '%') {
                if (!put_char('%', s, size, pos++))
                    return truncate_at(s, size, (size_t)pos);
                i++;
                continue;
            }

            /* Parse flags */
            bool zero_pad = false;
            bool left_align = false;
            while (tp == '0' || tp == '-') {
                if (tp == '0')
                    zero_pad = true;
                else
                    left_align = true;
                i++;
                tp = fmt[i];
                if (tp == '\0')
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
            }
            if (left_align)
                zero_pad = false;

            /* Parse optional width */
            int width = 0;
            while (tp >= '0' && tp <= '9') {
                width = width * 10 + (tp - '0');
                i++;
                tp = fmt[i];
                if (tp == '\0')
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
            }

            /* Parse an optional length modifier: hh, h, l, ll or z */
            LengthModifier length = LengthNone;
            if (tp == 'h' || tp == 'l' || tp == 'z') {
                char first = tp;
                i++;
                tp = fmt[i];
                if (first == 'z')
                    length = LengthSize;
                else if (tp == first) {
                    length = (first == 'h') ? LengthChar : LengthLongLong;
                    i++;
                    tp = fmt[i];
                } else
                    length = (first == 'h') ? LengthShort : LengthLong;
                if (tp == '\0')
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
            }

            /* A length modifier is for an integer conversion only */
            if (length != LengthNone && tp != 'u' && tp != 'd' && tp != 'x' && tp != 'X')
                { terminate_on_error(s, size, (size_t)pos); return -1; }

            i++; /* consume the specifier */

            switch (tp) {
            case 'u': {
                ulong val = read_unsigned(ap, length);
                char tmp[24];
                rc = __UlongToString(val, 10, tmp, sizeof(tmp));
                if (rc < 0)
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
                if (!left_align) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!put_char(zero_pad ? '0' : ' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!put_char(tmp[j], s, size, pos++))
                        return truncate_at(s, size, (size_t)pos);
                }
                if (left_align) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!put_char(' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                break;
            }
            case 'd': {
                long val = read_signed(ap, length);
                bool negative = (val < 0);
                ulong uval = negative ? (ulong)(-(val + 1)) + 1 : (ulong)val;
                char tmp[24];
                rc = __UlongToString(uval, 10, tmp, sizeof(tmp));
                if (rc < 0)
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
                int total_len = rc + (negative ? 1 : 0);
                if (!left_align) {
                    if (negative && zero_pad) {
                        /* Sign before zero padding: -00042 */
                        if (!put_char('-', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                        for (int p = 0; p < width - total_len; p++) {
                            if (!put_char('0', s, size, pos++))
                                return truncate_at(s, size, (size_t)pos);
                        }
                    } else {
                        for (int p = 0; p < width - total_len; p++) {
                            if (!put_char(zero_pad ? '0' : ' ', s, size, pos++))
                                return truncate_at(s, size, (size_t)pos);
                        }
                        if (negative) {
                            if (!put_char('-', s, size, pos++))
                                return truncate_at(s, size, (size_t)pos);
                        }
                    }
                } else {
                    if (negative) {
                        if (!put_char('-', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!put_char(tmp[j], s, size, pos++))
                        return truncate_at(s, size, (size_t)pos);
                }
                if (left_align) {
                    for (int p = 0; p < width - total_len; p++) {
                        if (!put_char(' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                break;
            }
            case 'x':
            case 'X': {
                ulong val = read_unsigned(ap, length);
                char tmp[24];
                rc = __UlongToString(val, 16, tmp, sizeof(tmp), tp == 'x');
                if (rc < 0)
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
                if (!left_align) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!put_char(zero_pad ? '0' : ' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!put_char(tmp[j], s, size, pos++))
                        return truncate_at(s, size, (size_t)pos);
                }
                if (left_align) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!put_char(' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                break;
            }
            case 'p': {
                if (sizeof(void *) != sizeof(ulong))
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
                void *val = va_arg(*ap, void *);
                ulong uval = (ulong)val;
                char tmp[24];
                rc = __UlongToString(uval, 16, tmp, sizeof(tmp));
                if (rc < 0)
                    { terminate_on_error(s, size, (size_t)pos); return -1; }
                if (!left_align) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!put_char(zero_pad ? '0' : ' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                for (int j = 0; j < rc; j++) {
                    if (!put_char(tmp[j], s, size, pos++))
                        return truncate_at(s, size, (size_t)pos);
                }
                if (left_align) {
                    for (int p = 0; p < width - rc; p++) {
                        if (!put_char(' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                break;
            }
            case 'c': {
                int val = va_arg(*ap, int);
                if (!put_char(val & 0xFF, s, size, pos++))
                    return truncate_at(s, size, (size_t)pos);
                break;
            }
            case 's': {
                const char *val = va_arg(*ap, char *);
                if (val == nullptr)
                    val = "(null)";
                size_t val_len = str_len(val);
                if (!left_align) {
                    for (int p = 0; p < width - (int)val_len; p++) {
                        if (!put_char(' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                /* Room for the string and the NUL after it; what does not
                   fit is truncated, never dropped (truncate_at). */
                size_t room = ((size_t)pos + 1 < size) ? size - (size_t)pos - 1 : 0;
                if (val_len > room) {
                    mem_cpy(&s[pos], val, room);
                    return truncate_at(s, size, (size_t)pos + room);
                }
                mem_cpy(&s[pos], val, val_len);
                pos += val_len;
                if (left_align) {
                    for (int p = 0; p < width - (int)val_len; p++) {
                        if (!put_char(' ', s, size, pos++))
                            return truncate_at(s, size, (size_t)pos);
                    }
                }
                break;
            }
            default:
                { terminate_on_error(s, size, (size_t)pos); return -1; }
            }
        } else
            if (!put_char(t, s, size, pos++))
                return truncate_at(s, size, (size_t)pos);
    }

    /* NUL-terminate but do not count the terminator: the return value is the
       length of the formatted string (standard vsnprintf semantics). */
    if (!put_char('\0', s, size, pos))
        return truncate_at(s, size, (size_t)pos);

    return pos;
}

}

int vsn_printf(char *s, size_t size, const char *fmt, va_list arg)
{
    /* The readers above take the list by pointer, and only a va_list
       object's address is the same thing on both arches: a va_list
       parameter is an array decayed to a pointer on x86-64, a struct on
       arm64. */
    va_list ap;
    va_copy(ap, arg);
    int len = format(s, size, fmt, &ap);
    va_end(ap);
    return len;
}

int sn_printf(char* buf, size_t size, const char* fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    int len = vsn_printf(buf, size, fmt, args);
    va_end(args);

    return len;
}

}
