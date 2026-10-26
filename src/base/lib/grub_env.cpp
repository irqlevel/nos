#include "grub_env.h"
#include "stdlib.h"

namespace stdlib
{

static const char GRUB_ENV_SIGNATURE[] = "# GRUB Environment Block\n";
static_assert(sizeof(GRUB_ENV_SIGNATURE) - 1 == GrubEnvBlock::SIGNATURE_LEN,
    "GRUB environment block signature length");

GrubEnvBlock::GrubEnvBlock(char* buf, ulong size)
    : buf_(buf)
    , size_(size)
{
}

bool GrubEnvBlock::is_valid()
{
    return buf_ != nullptr && size_ >= MIN_SIZE &&
        mem_cmp(buf_, GRUB_ENV_SIGNATURE, SIGNATURE_LEN) == 0;
}

void GrubEnvBlock::format(char* buf, ulong size)
{
    if (buf == nullptr || size < MIN_SIZE)
        return;

    mem_set(buf, '#', size);
    mem_cpy(buf, GRUB_ENV_SIGNATURE, SIGNATURE_LEN);
}

bool GrubEnvBlock::valid_name(const char* name)
{
    if (name == nullptr || name[0] == '\0')
        return false;

    ulong len = 0;
    for (const char* p = name; *p != '\0'; p++, len++)
    {
        char c = *p;
        bool ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                  (c >= '0' && c <= '9') || c == '_';
        if (!ok || len >= MAX_NAME_LEN)
            return false;
    }
    return true;
}

ulong GrubEnvBlock::escaped_len(const char* value)
{
    ulong len = 0;
    for (const char* p = value; *p != '\0'; p++)
        len += (*p == '\\' || *p == '\n') ? 2 : 1;
    return len;
}

ulong GrubEnvBlock::next_line(ulong pos)
{
    while (pos < size_)
    {
        if (buf_[pos] == '\\')
            pos += 2;
        else if (buf_[pos] == '\n')
            return pos + 1;
        else
            pos++;
    }
    return size_;
}

ulong GrubEnvBlock::value_len(ulong pos)
{
    ulong len = 0;
    while (pos + len < size_ && buf_[pos + len] != '\n')
        len += (buf_[pos + len] == '\\') ? 2 : 1;

    if (pos + len >= size_)
        return NO_NEWLINE;
    return len;
}

long GrubEnvBlock::find_line(const char* name, ulong name_len, ulong limit)
{
    ulong pos = SIGNATURE_LEN;
    while (pos + name_len + 1 < limit)
    {
        if (mem_cmp(buf_ + pos, name, name_len) == 0 && buf_[pos + name_len] == '=')
            return (long)pos;
        pos = next_line(pos);
    }
    return -1;
}

bool GrubEnvBlock::free_space(ulong& space)
{
    ulong pos = size_;
    while (pos > SIGNATURE_LEN && buf_[pos - 1] == '#')
        pos--;

    if (buf_[pos - 1] != '\n')
        return false;

    space = pos;
    return true;
}

bool GrubEnvBlock::parse_line(ulong pos, ulong limit, ulong& name_end, ulong& next)
{
    /* A backslash would make next_line and find_line step over what follows it,
       and a NUL cut the name for_each hands out short of the one find_line
       compares */
    ulong p = pos;
    while (p < limit && buf_[p] != '=')
    {
        char c = buf_[p];
        if (c == '\n' || c == '\\' || c == '\0' || p - pos >= MAX_NAME_LEN)
            return false;
        p++;
    }
    if (p >= limit)
        return false;
    name_end = p;

    p++;
    while (p < limit && buf_[p] != '\n')
        p += (buf_[p] == '\\') ? 2 : 1;
    if (p >= limit)
        return false;

    next = p + 1;
    return true;
}

bool GrubEnvBlock::well_formed(ulong limit)
{
    ulong pos = SIGNATURE_LEN;
    while (pos < limit)
    {
        char c = buf_[pos];
        ulong next;
        if (c == '#' || c == '\n' || c == '\r')
        {
            /* A comment whose last newline is escaped runs on into whatever
               comes after it */
            next = next_line(pos);
            if (next > limit)
                return false;
        }
        else
        {
            ulong name_end;
            if (!parse_line(pos, limit, name_end, next))
                return false;
        }
        pos = next;
    }
    return true;
}

bool GrubEnvBlock::set(const char* name, const char* value)
{
    if (!is_valid() || !valid_name(name) || value == nullptr)
        return false;

    ulong name_len = str_len(name);
    ulong new_len = escaped_len(value);

    ulong space;
    if (!free_space(space) || !well_formed(space))
        return false;
    ulong room = size_ - space;

    ulong value_pos;
    long line = find_line(name, name_len, space);
    if (line >= 0)
    {
        value_pos = (ulong)line + name_len + 1;
        ulong old_len = value_len(value_pos);
        if (old_len == NO_NEWLINE)
            return false;

        /* The lines after this one and the padding move as one; growing
           eats the tail of the padding, which the room check says is
           there, shrinking leaves stale bytes at the very end to cover. */
        if (new_len > old_len)
        {
            ulong grow = new_len - old_len;
            if (room < grow)
                return false;
            mem_move(buf_ + value_pos + new_len, buf_ + value_pos + old_len, size_ - (value_pos + new_len));
        }
        else if (new_len < old_len)
        {
            ulong shrink = old_len - new_len;
            mem_move(buf_ + value_pos + new_len, buf_ + value_pos + old_len, size_ - (value_pos + old_len));
            mem_set(buf_ + size_ - shrink, '#', shrink);
        }
    }
    else
    {
        if (room < name_len + 1 + new_len + 1)
            return false;
        mem_cpy(buf_ + space, name, name_len);
        buf_[space + name_len] = '=';
        value_pos = space + name_len + 1;
    }

    ulong pos = value_pos;
    for (const char* p = value; *p != '\0'; p++)
    {
        if (*p == '\\' || *p == '\n')
            buf_[pos++] = '\\';
        buf_[pos++] = *p;
    }
    buf_[pos] = '\n';
    return true;
}

bool GrubEnvBlock::unset(const char* name)
{
    if (!is_valid() || !valid_name(name))
        return false;

    ulong space;
    if (!free_space(space) || !well_formed(space))
        return false;

    ulong name_len = str_len(name);
    long line = find_line(name, name_len, space);
    if (line < 0)
        return false;

    ulong vlen = value_len((ulong)line + name_len + 1);
    if (vlen == NO_NEWLINE)
        return false;

    ulong line_len = name_len + 1 + vlen + 1;
    mem_move(buf_ + line, buf_ + line + line_len, size_ - ((ulong)line + line_len));
    mem_set(buf_ + size_ - line_len, '#', line_len);
    return true;
}

bool GrubEnvBlock::for_each(Visitor visitor, void* ctx)
{
    if (!is_valid() || visitor == nullptr)
        return false;

    ulong pos = SIGNATURE_LEN;
    while (pos < size_)
    {
        char c = buf_[pos];
        if (c == '#' || c == '\n' || c == '\r')
        {
            pos = next_line(pos);
            continue;
        }

        ulong name_end, next;
        if (!parse_line(pos, size_, name_end, next))
            return false;

        /* parse_line bounds the name by MAX_NAME_LEN */
        char name[MAX_NAME_LEN + 1];
        ulong name_len = name_end - pos;
        mem_cpy(name, buf_ + pos, name_len);
        name[name_len] = '\0';

        /* parse_line stopped on this line's newline and stepped over escapes
           the same way, so the walk cannot run past it */
        char value[MAX_VALUE_LEN + 1];
        ulong value_len = 0;
        for (ulong i = name_end + 1; buf_[i] != '\n'; i++)
        {
            if (buf_[i] == '\\')
                i++;
            if (value_len < MAX_VALUE_LEN)
                value[value_len++] = buf_[i];
        }
        value[value_len] = '\0';

        if (!visitor(name, value, ctx))
            return true;

        pos = next;
    }

    return true;
}

struct GrubEnvGetCtx
{
    const char* name;
    char* value;
    ulong value_size;
    bool found;
};

bool GrubEnvBlock::get_visitor(const char* name, const char* value, void* ctx)
{
    auto* get = static_cast<GrubEnvGetCtx*>(ctx);
    if (str_cmp(name, get->name) != 0)
        return true;

    strn_cpy(get->value, value, get->value_size);
    get->found = true;
    return false;
}

bool GrubEnvBlock::get(const char* name, char* value, ulong value_size)
{
    if (!valid_name(name) || value == nullptr || value_size == 0)
        return false;

    GrubEnvGetCtx ctx;
    ctx.name = name;
    ctx.value = value;
    ctx.value_size = value_size;
    ctx.found = false;
    for_each(get_visitor, &ctx);
    return ctx.found;
}

}
