// The kernel's printf (lib/format.cpp): every line anything traces, every
// panic's report and every command's output goes through Stdlib::VsnPrintf,
// and PRINTF_FORMAT has the compiler hold each call to C's printf rules --
// which is sound only while VsnPrintf reads each argument as C's printf
// does. So it is held to the host's own vsnprintf, format for format:
// random formats of what it implements -- d u x X c s and %%, the flags 0
// and -, widths, and the length modifiers hh h l ll z -- among literal
// text, over two fixed lists of twelve arguments with ints, longs and
// strings side by side (a C call's types are fixed when it is compiled),
// each conversion matching the type at its place, so that some are read
// from registers and some from the stack. What fits must be the host's
// output byte for byte and its length; what does not, the host's output cut
// where VsnPrintf cuts and marked with "..." as it marks it.
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

const size_t Places = 12;

const Slot Shapes[][Places] = {
    {SlotInt, SlotLong, SlotInt, SlotLong, SlotInt, SlotLong, SlotString, SlotInt, SlotLong, SlotInt, SlotLong,
     SlotInt},
    {SlotLong, SlotString, SlotInt, SlotInt, SlotLong, SlotLong, SlotInt, SlotString, SlotLong, SlotInt, SlotInt,
     SlotLong},
};

const char* const Strings[] = {"", "a", "hello", "with space", "%not a conversion%",
                               "a string longer than any width this fuzzer picks, by some way"};

/* An int place is filled with a long cut to an int at the call: the
   compiler owes nothing to the upper half of the register or the stack slot
   it passes it in, so a reader that took an int as a long would read that
   half too, and the host's own snprintf, which does not, would disagree. */
struct Args
{
    long I[Places];
    long L[Places];
    const char* S[Places];
};

int KernelCall(int shape, char* buf, size_t size, const char* fmt, const Args& a)
{
    if (shape == 0)
        return Stdlib::SnPrintf(buf, size, fmt, (int)a.I[0], a.L[1], (int)a.I[2], a.L[3], (int)a.I[4], a.L[5],
                                a.S[6], (int)a.I[7], a.L[8], (int)a.I[9], a.L[10], (int)a.I[11]);
    return Stdlib::SnPrintf(buf, size, fmt, a.L[0], a.S[1], (int)a.I[2], (int)a.I[3], a.L[4], a.L[5], (int)a.I[6],
                            a.S[7], a.L[8], (int)a.I[9], (int)a.I[10], a.L[11]);
}

int HostCall(int shape, char* buf, size_t size, const char* fmt, const Args& a)
{
    if (shape == 0)
        return snprintf(buf, size, fmt, (int)a.I[0], a.L[1], (int)a.I[2], a.L[3], (int)a.I[4], a.L[5], a.S[6],
                        (int)a.I[7], a.L[8], (int)a.I[9], a.L[10], (int)a.I[11]);
    return snprintf(buf, size, fmt, a.L[0], a.S[1], (int)a.I[2], (int)a.I[3], a.L[4], a.L[5], (int)a.I[6], a.S[7],
                    a.L[8], (int)a.I[9], (int)a.I[10], a.L[11]);
}

/* A conversion for what is at the place: its flags, width and length
   modifier among what VsnPrintf implements, and nothing C leaves undefined
   (0 with s or c, a width on c, which VsnPrintf does not pad) */
std::string Conversion(Fuzz::Input& in, Slot slot)
{
    std::string c = "%";
    char conv;
    static const char Integers[] = {'d', 'u', 'x', 'X'};
    if (slot == SlotString)
        conv = 's';
    else if (slot == SlotInt && in.Chance(24))
        conv = 'c';
    else
        conv = in.Pick(Integers);

    if (conv != 'c')
    {
        if (in.Chance(48))
            c += '-';
        if (conv != 's' && in.Chance(64))
            c += '0';
        if (in.Chance(96))
            c += std::to_string(in.Range(1, 30));
    }

    if (slot == SlotInt && conv != 'c')
    {
        static const char* const Short[] = {"", "", "", "h", "hh"};
        c += in.Pick(Short);
    }
    else if (slot == SlotLong)
    {
        static const char* const Long[] = {"l", "l", "ll", "z"};
        c += in.Pick(Long);
    }
    c += conv;
    return c;
}

/* Output as a finding can print it: a NUL, or any byte not printable, as \xNN */
std::string Show(const std::string& b)
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

/* What VsnPrintf makes of output that does not fit size bytes: what fits
   before the mark, the mark as much as fits, NUL-terminated */
std::string Cut(const std::string& full, size_t size)
{
    const size_t markLen = 3;
    size_t room = size - 1;
    size_t kept = (room > markLen) ? room - markLen : 0;
    return full.substr(0, kept) + std::string(room - kept, '.');
}

void Run(Fuzz::Input& in)
{
    const int shape = static_cast<int>(in.Below(2));
    const Slot* slots = Shapes[shape];

    Args a;
    for (size_t k = 0; k < Places; k++)
    {
        a.I[k] = static_cast<long>(static_cast<uint64_t>(in.U32()) << 32 | in.Value32());
        a.L[k] = static_cast<long>(in.Value64());
        a.S[k] = in.Pick(Strings);
    }

    /* The format: literal text and conversions, the places in order */
    std::string fmt;
    const size_t conversions = in.Below(Places + 1);
    bool narrow = false, wide = false;
    for (size_t k = 0; k <= conversions; k++)
    {
        for (size_t n = in.Below(6); n > 0; n--)
        {
            char c = static_cast<char>(in.Range(' ', '~'));
            if (in.Chance(16))
                fmt += "%%";
            else
                fmt += (c == '%') ? '_' : c;
        }
        if (k == conversions)
            break;
        std::string c = Conversion(in, slots[k]);
        narrow |= c.find('h') != std::string::npos;
        wide |= c.find("ll") != std::string::npos || c.find('z') != std::string::npos;
        fmt += c;
    }

    char host[1024];
    const int hostLen = HostCall(shape, host, sizeof(host), fmt.c_str(), a);
    INVARIANT(hostLen >= 0 && static_cast<size_t>(hostLen) < sizeof(host), "the host's own snprintf failed on '%s'",
              fmt.c_str());

    /* A buffer that holds it, or one that does not, down to a byte or none */
    size_t size = sizeof(host);
    if (in.Chance(96))
        size = in.Below(hostLen + 3);

    std::vector<char> buf(size + 1, '\x7F');
    const int len = KernelCall(shape, buf.data(), size, fmt.c_str(), a);
    INVARIANT(buf[size] == '\x7F', "VsnPrintf wrote past the %zu bytes it was given, on '%s'", size, fmt.c_str());

    if (size == 0)
    {
        INVARIANT(len == 0, "VsnPrintf said %d for a buffer of nothing, on '%s'", len, fmt.c_str());
        Fuzz::Reached("a buffer of nothing");
        return;
    }

    /* Bytes, not strings: a %c of a value whose low byte is 0 puts a NUL in
       the middle of both */
    INVARIANT(len >= 0 && static_cast<size_t>(len) < size && buf[len] == '\0',
              "VsnPrintf said %d into %zu bytes and did not end there, on '%s'", len, size, fmt.c_str());
    const std::string got(buf.data(), len);
    const std::string full(host, hostLen);
    if (static_cast<size_t>(hostLen) < size)
    {
        INVARIANT(got == full, "'%s' made '%s' (%d) where the host makes '%s' (%d)", fmt.c_str(),
                  Show(got).c_str(), len, Show(full).c_str(), hostLen);
        Fuzz::Reached(conversions > 3 ? "arguments from the stack" : "arguments from registers");
    }
    else
    {
        const std::string want = Cut(full, size);
        INVARIANT(got == want, "'%s' into %zu bytes made '%s' (%d) where the host's cut and marked is '%s'",
                  fmt.c_str(), size, Show(got).c_str(), len, Show(want).c_str());
        Fuzz::Reached("output cut short");
    }
    if (narrow)
        Fuzz::Reached("h or hh");
    if (wide)
        Fuzz::Reached("ll or z");
}

void Reset()
{
}

}

const Fuzz::Target Fuzz::TheTarget = {"format", Reset, Run, 512, 200000};
