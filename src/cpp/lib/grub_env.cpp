#include "grub_env.h"
#include "stdlib.h"

namespace Stdlib
{

static const char GrubEnvSignature[] = "# GRUB Environment Block\n";
static_assert(sizeof(GrubEnvSignature) - 1 == GrubEnvBlock::SignatureLen,
    "GRUB environment block signature length");

GrubEnvBlock::GrubEnvBlock(char* buf, ulong size)
    : Buf(buf)
    , Size(size)
{
}

bool GrubEnvBlock::IsValid()
{
    return Buf != nullptr && Size >= MinSize &&
        MemCmp(Buf, GrubEnvSignature, SignatureLen) == 0;
}

void GrubEnvBlock::Format(char* buf, ulong size)
{
    if (buf == nullptr || size < MinSize)
        return;

    MemSet(buf, '#', size);
    MemCpy(buf, GrubEnvSignature, SignatureLen);
}

bool GrubEnvBlock::ValidName(const char* name)
{
    if (name == nullptr || name[0] == '\0')
        return false;

    ulong len = 0;
    for (const char* p = name; *p != '\0'; p++, len++)
    {
        char c = *p;
        bool ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                  (c >= '0' && c <= '9') || c == '_';
        if (!ok || len >= MaxNameLen)
            return false;
    }
    return true;
}

ulong GrubEnvBlock::EscapedLen(const char* value)
{
    ulong len = 0;
    for (const char* p = value; *p != '\0'; p++)
        len += (*p == '\\' || *p == '\n') ? 2 : 1;
    return len;
}

ulong GrubEnvBlock::NextLine(ulong pos)
{
    while (pos < Size)
    {
        if (Buf[pos] == '\\')
            pos += 2;
        else if (Buf[pos] == '\n')
            return pos + 1;
        else
            pos++;
    }
    return Size;
}

ulong GrubEnvBlock::ValueLen(ulong pos)
{
    ulong len = 0;
    while (pos + len < Size && Buf[pos + len] != '\n')
        len += (Buf[pos + len] == '\\') ? 2 : 1;

    if (pos + len >= Size)
        return NoNewline;
    return len;
}

long GrubEnvBlock::FindLine(const char* name, ulong nameLen, ulong limit)
{
    ulong pos = SignatureLen;
    while (pos + nameLen + 1 < limit)
    {
        if (MemCmp(Buf + pos, name, nameLen) == 0 && Buf[pos + nameLen] == '=')
            return (long)pos;
        pos = NextLine(pos);
    }
    return -1;
}

bool GrubEnvBlock::FreeSpace(ulong& space)
{
    ulong pos = Size;
    while (pos > SignatureLen && Buf[pos - 1] == '#')
        pos--;

    if (Buf[pos - 1] != '\n')
        return false;

    space = pos;
    return true;
}

bool GrubEnvBlock::Set(const char* name, const char* value)
{
    if (!IsValid() || !ValidName(name) || value == nullptr)
        return false;

    ulong nameLen = StrLen(name);
    ulong newLen = EscapedLen(value);

    ulong space;
    if (!FreeSpace(space))
        return false;
    ulong room = Size - space;

    ulong valuePos;
    long line = FindLine(name, nameLen, space);
    if (line >= 0)
    {
        valuePos = (ulong)line + nameLen + 1;
        ulong oldLen = ValueLen(valuePos);
        if (oldLen == NoNewline)
            return false;

        /* The lines after this one and the padding move as one; growing
           eats the tail of the padding, which the room check says is
           there, shrinking leaves stale bytes at the very end to cover. */
        if (newLen > oldLen)
        {
            ulong grow = newLen - oldLen;
            if (room < grow)
                return false;
            MemMove(Buf + valuePos + newLen, Buf + valuePos + oldLen, Size - (valuePos + newLen));
        }
        else if (newLen < oldLen)
        {
            ulong shrink = oldLen - newLen;
            MemMove(Buf + valuePos + newLen, Buf + valuePos + oldLen, Size - (valuePos + oldLen));
            MemSet(Buf + Size - shrink, '#', shrink);
        }
    }
    else
    {
        if (room < nameLen + 1 + newLen + 1)
            return false;
        MemCpy(Buf + space, name, nameLen);
        Buf[space + nameLen] = '=';
        valuePos = space + nameLen + 1;
    }

    ulong pos = valuePos;
    for (const char* p = value; *p != '\0'; p++)
    {
        if (*p == '\\' || *p == '\n')
            Buf[pos++] = '\\';
        Buf[pos++] = *p;
    }
    Buf[pos] = '\n';
    return true;
}

bool GrubEnvBlock::Unset(const char* name)
{
    if (!IsValid() || !ValidName(name))
        return false;

    ulong nameLen = StrLen(name);
    long line = FindLine(name, nameLen, Size);
    if (line < 0)
        return false;

    ulong valueLen = ValueLen((ulong)line + nameLen + 1);
    if (valueLen == NoNewline)
        return false;

    ulong lineLen = nameLen + 1 + valueLen + 1;
    MemMove(Buf + line, Buf + line + lineLen, Size - ((ulong)line + lineLen));
    MemSet(Buf + Size - lineLen, '#', lineLen);
    return true;
}

bool GrubEnvBlock::ForEach(Visitor visitor, void* ctx)
{
    if (!IsValid() || visitor == nullptr)
        return false;

    ulong pos = SignatureLen;
    while (pos < Size)
    {
        char c = Buf[pos];
        if (c != '#' && c != '\n' && c != '\r')
        {
            ulong nameStart = pos;
            while (pos < Size && Buf[pos] != '=')
                pos++;
            if (pos == Size)
                return false;
            ulong nameEnd = pos;

            ulong valueStart = ++pos;
            while (pos < Size && Buf[pos] != '\n')
                pos += (Buf[pos] == '\\') ? 2 : 1;
            if (pos >= Size)
                return false;

            char name[MaxNameLen + 1];
            ulong nameLen = nameEnd - nameStart;
            if (nameLen > MaxNameLen)
                nameLen = MaxNameLen;
            MemCpy(name, Buf + nameStart, nameLen);
            name[nameLen] = '\0';

            /* The scan above stopped on this line's newline and stepped
               over escapes the same way, so the walk cannot run past it */
            char value[MaxValueLen + 1];
            ulong valueLen = 0;
            for (ulong i = valueStart; Buf[i] != '\n'; i++)
            {
                if (Buf[i] == '\\')
                    i++;
                if (valueLen < MaxValueLen)
                    value[valueLen++] = Buf[i];
            }
            value[valueLen] = '\0';

            if (!visitor(name, value, ctx))
                return true;
        }

        pos = NextLine(pos);
    }

    return true;
}

struct GrubEnvGetCtx
{
    const char* Name;
    char* Value;
    ulong ValueSize;
    bool Found;
};

bool GrubEnvBlock::GetVisitor(const char* name, const char* value, void* ctx)
{
    auto* get = static_cast<GrubEnvGetCtx*>(ctx);
    if (StrCmp(name, get->Name) != 0)
        return true;

    StrnCpy(get->Value, value, get->ValueSize);
    get->Found = true;
    return false;
}

bool GrubEnvBlock::Get(const char* name, char* value, ulong valueSize)
{
    if (!ValidName(name) || value == nullptr || valueSize == 0)
        return false;

    GrubEnvGetCtx ctx;
    ctx.Name = name;
    ctx.Value = value;
    ctx.ValueSize = valueSize;
    ctx.Found = false;
    ForEach(GetVisitor, &ctx);
    return ctx.Found;
}

}
