// The GRUB environment block (lib/grub_env.cpp): the file `grubenv` reads
// off /boot and writes back for GRUB's next boot. What is on a disk is
// whoever wrote it's to choose, so the block is anything: what grub-editenv
// writes, that damaged, bytes. Over a block the model can read -- comments,
// blank lines and name=value lines, then '#' to the end -- every call is held
// to what grub_env.h says it does: Set and Unset change exactly the line the
// model says, or say no and change nothing, room counted to the byte; Get
// and for_each give back what the model holds. Over any block, a Set that
// says yes is what Get gives back, one that says no leaves every byte as it
// was, and a block Set or Unset wrote is one for_each reads to its end, with
// the newline and the '#' padding GRUB's own writer looks for.
#include "host.h"

#include <lib/grub_env.h>

#include "fuzz.h"

#include <string.h>

#include <algorithm>
#include <string>
#include <vector>

namespace
{

using stdlib::GrubEnvBlock;

struct Var
{
    std::string name;
    std::string value;
};

const char* const NAMES[] = {"a", "b", "saved_entry", "next_entry", "nos_next", "x_1", "A", "aa", "boot_once"};

/* Names GRUB would not take. */
const char* const BAD_NAMES[] = {"", "a b", "a=b", "a\\b", "a\nb", "-x", "a.b"};

/* Names a block may have all the same. */
const char* const ODD_NAMES[] = {"a-b", "x.y", "with space",
                                "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"};

std::string escape(const std::string& v)
{
    std::string e;
    for (char c : v)
    {
        if (c == '\\' || c == '\n')
            e += '\\';
        e += c;
    }
    return e;
}

bool valid_name(const std::string& n)
{
    if (n.empty() || n.size() > GrubEnvBlock::MAX_NAME_LEN)
        return false;
    for (char c : n)
    {
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_'))
            return false;
    }
    return true;
}

/* A name to ask about: one GRUB takes, one it does not, one the block has. */
std::string pick_name(fuzz::Input& in, const std::vector<Var>& vars)
{
    switch (in.u8() % 8)
    {
    case 0:
        return std::string(in.range(60, 70), 'a');
    case 1:
        return in.pick(BAD_NAMES);
    case 2:
    case 3:
        if (!vars.empty())
            return vars[in.below(vars.size())].name;
        [[fallthrough]];
    default:
        return NAMES[in.below(sizeof(NAMES) / sizeof(NAMES[0]))];
    }
}

/* A name a line of a block may have: one GRUB takes, mostly. */
std::string line_name(fuzz::Input& in)
{
    if (in.chance(20))
        return in.pick(ODD_NAMES);
    return NAMES[in.below(sizeof(NAMES) / sizeof(NAMES[0]))];
}

std::string pick_value(fuzz::Input& in)
{
    size_t n = in.chance(16) ? in.range(250, 600) : in.below(40);
    std::string v;
    for (size_t i = 0; i < n; i++)
    {
        uint8_t c = in.u8();
        v += (c < 48) ? "\\\n=#\r "[c % 6] : static_cast<char>('a' + c % 26);
    }
    return v;
}

std::string cut(const std::string& v, size_t n)
{
    return v.substr(0, std::min(n, v.size()));
}

/* for_each's visit, kept. */
struct Visits
{
    std::vector<Var> seen;
    size_t stop_after;
};

bool visit(const char* name, const char* value, void* ctx)
{
    auto* v = static_cast<Visits*>(ctx);
    v->seen.push_back({name, value});
    return v->seen.size() < v->stop_after;
}

bool tail(const char* b, size_t size)
{
    size_t pos = size;
    while (pos > GrubEnvBlock::SIGNATURE_LEN && b[pos - 1] == '#')
        pos--;
    return b[pos - 1] == '\n';
}

void check_reads_as_model(GrubEnvBlock& env, const std::vector<Var>& vars, const char* after)
{
    Visits v{{}, ~0UL};
    INVARIANT(env.for_each(visit, &v), "for_each calls a block the model reads malformed, after %s", after);
    INVARIANT(v.seen.size() == vars.size(), "for_each visits %zu variables, not %zu, after %s", v.seen.size(),
        vars.size(), after);
    for (size_t i = 0; i < vars.size(); i++)
    {
        INVARIANT(v.seen[i].name == vars[i].name && v.seen[i].value == cut(vars[i].value, GrubEnvBlock::MAX_VALUE_LEN),
            "for_each's variable %zu is '%s', not '%s', after %s", i, v.seen[i].name.c_str(), vars[i].name.c_str(),
            after);
    }
}

void run(fuzz::Input& in)
{
    size_t size;
    switch (in.u8() % 4)
    {
    case 0:
        size = GrubEnvBlock::MIN_SIZE + in.below(64);
        break;
    case 1:
        size = GrubEnvBlock::MIN_SIZE + in.below(4096);
        break;
    default:
        size = GrubEnvBlock::DEFAULT_SIZE;
        break;
    }
    /* The block alone in its allocation: a step past either end is ASan's. */
    char* b = static_cast<char*>(malloc(size));
    GrubEnvBlock::format(b, size);
    GrubEnvBlock env(b, size);

    /* The model holds while the block is one it can read. */
    std::vector<Var> vars;
    size_t used = GrubEnvBlock::SIGNATURE_LEN;
    bool modelled = true;
    switch (in.u8() % 8)
    {
    case 0:
        /* Bytes: whatever a disk holds past the signature. */
        for (size_t i = GrubEnvBlock::SIGNATURE_LEN; i < size && in.more(); i++)
            b[i] = static_cast<char>(in.u8());
        modelled = false;
        fuzz::reached("a block of bytes");
        break;
    default:
    {
        /* Lines grub-editenv could have written. */
        for (size_t lines = in.below(10); lines > 0; lines--)
        {
            std::string line;
            Var var;
            bool is_var = false;
            switch (in.u8() % 8)
            {
            case 0:
                line = "# a comment, a\\b in it\n";
                break;
            case 1:
                line = in.chance(128) ? "\n" : "\r\n";
                break;
            default:
                var = {line_name(in), pick_value(in)};
                line = var.name + "=" + escape(var.value) + "\n";
                is_var = true;
                break;
            }
            if (used + line.size() > size)
                break;
            memcpy(b + used, line.data(), line.size());
            used += line.size();
            if (is_var)
                vars.push_back(var);
        }
        if (in.chance(40))
        {
            /* And then damaged: a line with no '=', a byte of anything, an
               escape where the last newline was. */
            size_t at = GrubEnvBlock::SIGNATURE_LEN + in.below(size - GrubEnvBlock::SIGNATURE_LEN);
            switch (in.u8() % 3)
            {
            case 0:
                if (used + 2 <= size)
                    memcpy(b + GrubEnvBlock::SIGNATURE_LEN, "b\n", 2);
                break;
            case 1:
                b[at] = static_cast<char>(in.u8());
                break;
            default:
                if (used > GrubEnvBlock::SIGNATURE_LEN + 1)
                    b[used - 2] = '\\';
                break;
            }
            modelled = false;
            fuzz::reached("a block damaged");
        }
        break;
    }
    }
    if (modelled)
    {
        check_reads_as_model(env, vars, "the block was written");
        fuzz::reached("a block the model reads");
    }

    std::vector<char> before(size);
    for (int ops = 0; ops < 24 && in.more(); ops++)
    {
        std::string name = pick_name(in, vars);
        memcpy(before.data(), b, size);
        switch (in.u8() % 4)
        {
        case 0:
        {
            std::string value = pick_value(in);
            bool said = env.set(name.c_str(), value.c_str());
            fuzz::say("set('%s', '%s') = %d", name.c_str(), value.c_str(), said);
            if (modelled)
            {
                /* The model's answer, room counted to the byte. */
                bool want = false;
                auto it = std::find_if(vars.begin(), vars.end(), [&](const Var& v) { return v.name == name; });
                if (valid_name(name))
                {
                    size_t new_len = escape(value).size();
                    if (it != vars.end())
                    {
                        size_t old_len = escape(it->value).size();
                        want = new_len <= old_len || new_len - old_len <= size - used;
                        if (want)
                        {
                            used = used + new_len - old_len;
                            it->value = value;
                        }
                    }
                    else
                    {
                        want = name.size() + 1 + new_len + 1 <= size - used;
                        if (want)
                        {
                            used += name.size() + 1 + new_len + 1;
                            vars.push_back({name, value});
                        }
                    }
                }
                INVARIANT(said == want, "set('%s') of %zu bytes said %d where the model says %d", name.c_str(),
                    value.size(), said, want);
                if (said)
                    check_reads_as_model(env, vars, "a Set");
            }
            if (said)
            {
                char got[GrubEnvBlock::MAX_VALUE_LEN + 1];
                INVARIANT(env.get(name.c_str(), got, sizeof(got)), "a Set of '%s' that said yes is not there to Get",
                    name.c_str());
                INVARIANT(cut(value, GrubEnvBlock::MAX_VALUE_LEN) == got, "get('%s') gives back other than Set put",
                    name.c_str());
                Visits v{{}, ~0UL};
                INVARIANT(env.for_each(visit, &v), "a block Set wrote is one for_each calls malformed");
                INVARIANT(tail(b, size), "a Set left no newline before the padding");
                fuzz::reached("a Set");
            }
            else
            {
                INVARIANT(memcmp(before.data(), b, size) == 0, "a Set of '%s' that said no changed the block",
                    name.c_str());
            }
            break;
        }
        case 1:
        {
            bool said = env.unset(name.c_str());
            fuzz::say("unset('%s') = %d", name.c_str(), said);
            if (modelled)
            {
                auto it = std::find_if(vars.begin(), vars.end(), [&](const Var& v) { return v.name == name; });
                bool want = valid_name(name) && it != vars.end();
                INVARIANT(said == want, "unset('%s') said %d where the model says %d", name.c_str(), said, want);
                if (want)
                {
                    used -= name.size() + 1 + escape(it->value).size() + 1;
                    vars.erase(it);
                    check_reads_as_model(env, vars, "an Unset");
                }
            }
            if (said)
            {
                Visits v{{}, ~0UL};
                INVARIANT(env.for_each(visit, &v), "a block Unset wrote is one for_each calls malformed");
                INVARIANT(tail(b, size), "an Unset left no newline before the padding");
                fuzz::reached("an Unset");
            }
            else
            {
                INVARIANT(memcmp(before.data(), b, size) == 0, "an Unset of '%s' that said no changed the block",
                    name.c_str());
            }
            break;
        }
        case 2:
        {
            size_t cap = 1 + in.below(GrubEnvBlock::MAX_VALUE_LEN + 1);
            std::vector<char> got(cap);
            bool found = env.get(name.c_str(), got.data(), cap);
            if (modelled)
            {
                auto it = std::find_if(vars.begin(), vars.end(), [&](const Var& v) { return v.name == name; });
                bool want = valid_name(name) && it != vars.end();
                INVARIANT(found == want, "get('%s') found %d where the model says %d", name.c_str(), found, want);
                if (found)
                    INVARIANT(cut(cut(it->value, GrubEnvBlock::MAX_VALUE_LEN), cap - 1) == got.data(),
                        "get('%s') into %zu bytes gives back other than the model's value", name.c_str(), cap);
            }
            if (found)
                INVARIANT(strlen(got.data()) < cap, "Get wrote past what it was given");
            break;
        }
        default:
        {
            Visits v{{}, 1 + in.below(8)};
            bool whole = env.for_each(visit, &v);
            if (modelled)
            {
                INVARIANT(whole, "for_each calls a block the model reads malformed");
                for (size_t i = 0; i < v.seen.size(); i++)
                    INVARIANT(i < vars.size() && v.seen[i].name == vars[i].name, "for_each visits past the model");
            }
            break;
        }
        }
        memcpy(before.data(), b, size);
    }
    free(b);
}

void reset()
{
}

}

const fuzz::Target fuzz::THE_TARGET = {"grubenv", reset, run, 2048, 200000};
