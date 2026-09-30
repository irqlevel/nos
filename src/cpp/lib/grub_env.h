#pragma once

#include <include/types.h>

namespace Stdlib
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
    static const ulong SignatureLen = 25;

    /* GRUB refuses a block shorter than its signature and a terminator */
    static const ulong MinSize = SignatureLen + 1;

    /* What grub-editenv create makes; a larger block is as legal */
    static const ulong DefaultSize = 1024;

    /* Names are [A-Za-z0-9_]; a value longer than this is cut on the way out */
    static const ulong MaxNameLen = 64;
    static const ulong MaxValueLen = 255;

    GrubEnvBlock(char* buf, ulong size);

    /* Does buf start with the signature? Nothing else is trusted before this. */
    bool IsValid();

    /* Make buf an empty block: the signature, then '#' to the end. size is
       at least MinSize. */
    static void Format(char* buf, ulong size);

    /* Look up a variable, false if it is not set. The value comes back
       unescaped, cut to valueSize - 1. */
    bool Get(const char* name, char* value, ulong valueSize);

    /* Set a variable, replacing its value if it has one. False when the name
       is not one GRUB takes, the block is malformed or there is no room; the
       block is unchanged then. */
    bool Set(const char* name, const char* value);

    /* Remove a variable; false when it was not set, or the block is
       malformed, and the block is unchanged then. */
    bool Unset(const char* name);

    /* Visit every variable in order; the visitor returns false to stop.
       Returns false when the block is malformed and stops there: a line
       that is not a comment and has no '=' before its end, a name longer
       than MaxNameLen or holding a backslash or a NUL, a value without a
       newline before the end of the block. A block is malformed by the same
       rules for Set and Unset, which find a variable's line on their own:
       over those lines the two ways of reading the block could not agree on
       where a line starts -- a line without '=' would lend its text to the
       next one's name -- so a Set could say yes to what Get and GRUB then
       read as something else. */
    typedef bool (*Visitor)(const char* name, const char* value, void* ctx);
    bool ForEach(Visitor visitor, void* ctx);

private:
    /* Index of the line starting with name=, searching lines that start
       before limit; -1 if there is none */
    long FindLine(const char* name, ulong nameLen, ulong limit);

    /* Index just past the newline that ends the line at pos, or Size */
    ulong NextLine(ulong pos);

    /* Bytes of the value starting at pos, escapes included, up to its
       newline; NoNewline when the block ends first */
    ulong ValueLen(ulong pos);

    /* Where the '#' padding at the end begins; false when what precedes it
       is not a newline, which is a block GRUB will not write to either */
    bool FreeSpace(ulong& space);

    /* The variable line at pos, as ForEach reads it: where its name ends
       and where the line after it begins, before limit; false when it is not
       one (ForEach says what that is) */
    bool ParseLine(ulong pos, ulong limit, ulong& nameEnd, ulong& next);

    /* Every line before limit a comment, a blank or a variable line, the
       last of them ending exactly there */
    bool WellFormed(ulong limit);

    static bool ValidName(const char* name);
    static ulong EscapedLen(const char* value);
    static bool GetVisitor(const char* name, const char* value, void* ctx);

    static const ulong NoNewline = (ulong)-1;

    char* Buf;
    ulong Size;
};

}
