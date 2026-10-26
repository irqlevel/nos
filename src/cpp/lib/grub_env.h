#pragma once

#include <include/types.h>

namespace stdlib
{

/* A GRUB environment block: the 1 KiB file grub-editenv makes, which GRUB's
   load_env reads and save_env writes back. It is how a boot choice gets from
   a running kernel to the next boot's GRUB, and the kernel has to leave it
   exactly as GRUB expects, because GRUB rewrites it afterwards: save_env
   goes straight to the file's disk blocks, so the file keeps its size, and
   GRUB looks for the free space by walking back from the end over '#' and
   insists on a newline right before it.

   The layout, after the signature line: lines of name=value, each ended by
   a newline, with a backslash before any backslash or newline in a value;
   lines starting with '#' are comments (grub-editenv writes one); and the
   rest of the block is '#' to the end. Edits happen in place, in the
   caller's buffer, the way GRUB's own do: a value that changes length
   shifts the lines after it, and the padding grows or shrinks by as much.

   No allocation: the block is wherever the caller read it to. */
class GrubEnvBlock
{
public:
    /* "# GRUB Environment Block\n" */
    static const ulong SIGNATURE_LEN = 25;

    /* GRUB refuses a block shorter than its signature and a terminator */
    static const ulong MIN_SIZE = SIGNATURE_LEN + 1;

    /* What grub-editenv create makes; a larger block is as legal */
    static const ulong DEFAULT_SIZE = 1024;

    /* Names are [A-Za-z0-9_]; a value longer than this is cut on the way out */
    static const ulong MAX_NAME_LEN = 64;
    static const ulong MAX_VALUE_LEN = 255;

    GrubEnvBlock(char* buf, ulong size);

    /* Does buf start with the signature? Nothing else is trusted before this. */
    bool is_valid();

    /* Make buf an empty block: the signature, then '#' to the end. size is
       at least MIN_SIZE. */
    static void format(char* buf, ulong size);

    /* Look up a variable, false if it is not set. The value comes back
       unescaped, cut to value_size - 1. */
    bool get(const char* name, char* value, ulong value_size);

    /* Set a variable, replacing its value if it has one. False when the name
       is not one GRUB takes, the block is malformed or there is no room; the
       block is unchanged then. */
    bool set(const char* name, const char* value);

    /* Remove a variable; false when it was not set, or the block is
       malformed, and the block is unchanged then. */
    bool unset(const char* name);

    /* Visit every variable in order; the visitor returns false to stop.
       Returns false when the block is malformed and stops there: a line
       that is not a comment and has no '=' before its end, a name longer
       than MAX_NAME_LEN or holding a backslash or a NUL, a value without a
       newline before the end of the block. A block is malformed by the same
       rules for Set and Unset, which find a variable's line on their own:
       over those lines the two ways of reading the block could not agree on
       where a line starts -- a line without '=' would lend its text to the
       next one's name -- so a Set could say yes to what Get and GRUB then
       read as something else. */
    typedef bool (*Visitor)(const char* name, const char* value, void* ctx);
    bool for_each(Visitor visitor, void* ctx);

private:
    /* Index of the line starting with name=, searching lines that start
       before limit; -1 if there is none */
    long find_line(const char* name, ulong name_len, ulong limit);

    /* Index just past the newline that ends the line at pos, or Size */
    ulong next_line(ulong pos);

    /* Bytes of the value starting at pos, escapes included, up to its
       newline; NO_NEWLINE when the block ends first */
    ulong value_len(ulong pos);

    /* Where the '#' padding at the end begins; false when what precedes it
       is not a newline, which is a block GRUB will not write to either */
    bool free_space(ulong& space);

    /* The variable line at pos, as for_each reads it: where its name ends
       and where the line after it begins, before limit; false when it is not
       one (for_each says what that is) */
    bool parse_line(ulong pos, ulong limit, ulong& name_end, ulong& next);

    /* Every line before limit a comment, a blank or a variable line, the
       last of them ending exactly there */
    bool well_formed(ulong limit);

    static bool valid_name(const char* name);
    static ulong escaped_len(const char* value);
    static bool get_visitor(const char* name, const char* value, void* ctx);

    static const ulong NO_NEWLINE = (ulong)-1;

    char* buf_;
    ulong size_;
};

}
