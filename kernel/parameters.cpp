#include "parameters.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

Parameters::Parameters()
    : TraceVga(false)
    , PanicVga(false)
    , SmpOff(false)
{
}

Parameters::~Parameters()
{
}

bool Parameters::IsTraceVga()
{
    return TraceVga;
}

bool Parameters::IsPanicVga()
{
    return PanicVga;
}

bool Parameters::IsSmpOff()
{
    return SmpOff;
}

bool Parameters::ParseParameter(const char *cmdline, size_t start, size_t end)
{
    if (BugOn(start >= end))
        return false;

    const size_t maxLen = 20;
    char param[maxLen + 1];
    size_t len = end - start;
    if (len > maxLen)
        return false;

    if (len < 3)
        return false;

    Stdlib::StrnCpy(param, &cmdline[start], len);
    param[len] = '\0';
    
    const char* sep = Stdlib::StrChrOnce(param, '=');
    if (sep == nullptr)
        return false;

    if ((sep == param) || (sep == &param[len - 1]))
        return false;

    size_t keyLen = sep - param;
    const char *key = &param[0];
    param[keyLen] = '\0';
    const char *value = &param[keyLen + 1];

    Trace(0, "Key %s value %s", key, value);

    if (Stdlib::StrCmp(key, "trace") == 0)
    {
        if (Stdlib::StrCmp(value, "vga") == 0)
        {
            TraceVga = true;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    if (Stdlib::StrCmp(key, "panic") == 0)
    {
        if (Stdlib::StrCmp(value, "vga") == 0)
        {
            PanicVga = true;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }        
    }
    if (Stdlib::StrCmp(key, "smp") == 0)
    {
        if (Stdlib::StrCmp(value, "off") == 0)
        {
            SmpOff = true;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else
    {
        Trace(0, "Unknown key %s, skipping", key);
    }

    return true;
}

bool Parameters::Parse(const char *cmdline)
{
    if (Stdlib::SnPrintf(Cmdline, Stdlib::ArraySize(Cmdline), "%s", cmdline) < 0)
        return false;

    size_t start = 0, i = 0;
    for (; i < Stdlib::StrLen(Cmdline); i++)
    {
        if (Cmdline[i] == ' ')
        {
            if (start < i)
            {
                if (!ParseParameter(Cmdline, start, i))
                    return false;
            }
            start = i + 1;
        }
    }

    if (start < i)
    {
        if (!ParseParameter(Cmdline, start, i))
            return false;
    }

    return true;
}

}
