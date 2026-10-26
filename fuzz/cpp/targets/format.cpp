// The kernel's printf (lib/format.cpp): every line anything traces, every
// panic's report and every command's output goes through stdlib::vsn_printf,
// and PRINTF_FORMAT has the compiler hold each call to C's printf rules --
// which is sound only while vsn_printf reads each argument as C's printf
// does. So it is held to the host's own vsnprintf, format for format:
// random formats of what it implements -- d u x X c s and %%, the flags 0
// and -, widths, and the length modifiers hh h l ll z -- among literal
// text, over two fixed lists of twelve arguments with ints, longs and
// strings side by side (a C call's types are fixed when it is compiled),
// each conversion matching the type at its place, so that some are read
// from registers and some from the stack. What fits must be the host's
// output byte for byte and its length; what does not, the host's output cut
// where vsn_printf cuts and marked with "..." as it marks it.
#include "host.h"

#include <lib/stdlib.h>

#include "fuzz.h"

#include <stdio.h>
#include <string.h>

#include <string>
#include <vector>

namespace
{

/* What each argument place holds */
enum Slot
{
    SlotInt,
    SlotLong,
    SlotString,
};

const size_t PLACES = 12;

const Slot SHAPES[][PLACES] = {
    {SlotInt, SlotLong, SlotInt, SlotLong, SlotInt, SlotLong, SlotString, SlotInt, SlotLong, SlotInt, SlotLong,
     SlotInt},
    {SlotLong, SlotString, SlotInt, SlotInt, SlotLong, SlotLong, SlotInt, SlotString, SlotLong, SlotInt, SlotInt,
     SlotLong},
};

const char* const STRINGS[] = {"", "a", "hello", "with space", "%not a conversion%",
                               "a string longer than any width this fuzzer picks, by some way"};

/* An int place is filled with a long cut to an int at the call: the
   compiler owes nothing to the upper half of the register or the stack slot
   it passes it in, so a reader that took an int as a long would read that
   half too, and the host's own snprintf, which does not, would disagree. */
struct Args
{
    long i[PLACES];
    long l[PLACES];
    const char* s[PLACES];
};

int kernel_call(int shape, char* buf, size_t size, const char* fmt, const Args& a)
{
    if (shape == 0)
        return stdlib::sn_printf(buf, size, fmt, (int)a.i[0], a.l[1], (int)a.i[2], a.l[3], (int)a.i[4], a.l[5],
                                a.s[6], (int)a.i[7], a.l[8], (int)a.i[9], a.l[10], (int)a.i[11]);
    return stdlib::sn_printf(buf, size, fmt, a.l[0], a.s[1], (int)a.i[2], (int)a.i[3], a.l[4], a.l[5], (int)a.i[6],
                            a.s[7], a.l[8], (int)a.i[9], (int)a.i[10], a.l[11]);
}

int host_call(int shape, char* buf, size_t size, const char* fmt, const Args& a)
{
    if (shape == 0)
        return snprintf(buf, size, fmt, (int)a.i[0], a.l[1], (int)a.i[2], a.l[3], (int)a.i[4], a.l[5], a.s[6],
                        (int)a.i[7], a.l[8], (int)a.i[9], a.l[10], (int)a.i[11]);
    return snprintf(buf, size, fmt, a.l[0], a.s[1], (int)a.i[2], (int)a.i[3], a.l[4], a.l[5], (int)a.i[6], a.s[7],
                    a.l[8], (int)a.i[9], (int)a.i[10], a.l[11]);
}

/* A conversion for what is at the place: its flags, width and length
   modifier among what vsn_printf implements, and nothing C leaves undefined
   (0 with s or c, a width on c, which vsn_printf does not pad) */
std::string conversion(fuzz::Input& in, Slot slot)
{
    std::string c = "%";
    char conv;
    static const char INTEGERS[] = {'d', 'u', 'x', 'X'};
    if (slot == SlotString)
        conv = 's';
    else if (slot == SlotInt && in.chance(24))
        conv = 'c';
    else
        conv = in.pick(INTEGERS);

    if (conv != 'c')
    {
        if (in.chance(48))
            c += '-';
        if (conv != 's' && in.chance(64))
            c += '0';
        if (in.chance(96))
            c += std::to_string(in.range(1, 30));
    }

    if (slot == SlotInt && conv != 'c')
    {
        static const char* const SHORT[] = {"", "", "", "h", "hh"};
        c += in.pick(SHORT);
    }
    else if (slot == SlotLong)
    {
        static const char* const LONG[] = {"l", "l", "ll", "z"};
        c += in.pick(LONG);
    }
    c += conv;
    return c;
}

/* Output as a finding can print it: a NUL, or any byte not printable, as \xNN */
std::string show(const std::string& b)
{
    std::string out;
    for (unsigned char c : b)
    {
        char hex[5];
        if (c >= ' ' && c <= '~')
            out += static_cast<char>(c);
        else
        {
            snprintf(hex, sizeof(hex), "\\x%02x", c);
            out += hex;
        }
    }
    return out;
}

/* What vsn_printf makes of output that does not fit size bytes: what fits
   before the mark, the mark as much as fits, NUL-terminated */
std::string cut(const std::string& full, size_t size)
{
    const size_t mark_len = 3;
    size_t room = size - 1;
    size_t kept = (room > mark_len) ? room - mark_len : 0;
    return full.substr(0, kept) + std::string(room - kept, '.');
}

void run(fuzz::Input& in)
{
    const int shape = static_cast<int>(in.below(2));
    const Slot* slots = SHAPES[shape];

    Args a;
    for (size_t k = 0; k < PLACES; k++)
    {
        a.i[k] = static_cast<long>(static_cast<uint64_t>(in.u32()) << 32 | in.value32());
        a.l[k] = static_cast<long>(in.value64());
        a.s[k] = in.pick(STRINGS);
    }

    /* The format: literal text and conversions, the places in order */
    std::string fmt;
    const size_t conversions = in.below(PLACES + 1);
    bool narrow = false, wide = false;
    for (size_t k = 0; k <= conversions; k++)
    {
        for (size_t n = in.below(6); n > 0; n--)
        {
            char c = static_cast<char>(in.range(' ', '~'));
            if (in.chance(16))
                fmt += "%%";
            else
                fmt += (c == '%') ? '_' : c;
        }
        if (k == conversions)
            break;
        std::string c = conversion(in, slots[k]);
        narrow |= c.find('h') != std::string::npos;
        wide |= c.find("ll") != std::string::npos || c.find('z') != std::string::npos;
        fmt += c;
    }

    char host[1024];
    const int host_len = host_call(shape, host, sizeof(host), fmt.c_str(), a);
    INVARIANT(host_len >= 0 && static_cast<size_t>(host_len) < sizeof(host), "the host's own snprintf failed on '%s'",
              fmt.c_str());

    /* A buffer that holds it, or one that does not, down to a byte or none */
    size_t size = sizeof(host);
    if (in.chance(96))
        size = in.below(host_len + 3);

    std::vector<char> buf(size + 1, '\x7F');
    const int len = kernel_call(shape, buf.data(), size, fmt.c_str(), a);
    INVARIANT(buf[size] == '\x7F', "vsn_printf wrote past the %zu bytes it was given, on '%s'", size, fmt.c_str());

    if (size == 0)
    {
        INVARIANT(len == 0, "vsn_printf said %d for a buffer of nothing, on '%s'", len, fmt.c_str());
        fuzz::reached("a buffer of nothing");
        return;
    }

    /* Bytes, not strings: a %c of a value whose low byte is 0 puts a NUL in
       the middle of both */
    INVARIANT(len >= 0 && static_cast<size_t>(len) < size && buf[len] == '\0',
              "vsn_printf said %d into %zu bytes and did not end there, on '%s'", len, size, fmt.c_str());
    const std::string got(buf.data(), len);
    const std::string full(host, host_len);
    if (static_cast<size_t>(host_len) < size)
    {
        INVARIANT(got == full, "'%s' made '%s' (%d) where the host makes '%s' (%d)", fmt.c_str(),
                  show(got).c_str(), len, show(full).c_str(), host_len);
        fuzz::reached(conversions > 3 ? "arguments from the stack" : "arguments from registers");
    }
    else
    {
        const std::string want = cut(full, size);
        INVARIANT(got == want, "'%s' into %zu bytes made '%s' (%d) where the host's cut and marked is '%s'",
                  fmt.c_str(), size, show(got).c_str(), len, show(want).c_str());
        fuzz::reached("output cut short");
    }
    if (narrow)
        fuzz::reached("h or hh");
    if (wide)
        fuzz::reached("ll or z");
}

void reset()
{
}

}

const fuzz::Target fuzz::THE_TARGET = {"format", reset, run, 512, 200000};
