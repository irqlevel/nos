#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

/* The console shell's line editor: what is typed at the screen's keyboard or
   at the serial port, made a command line. What it reads is a terminal's
   bytes -- a serial terminal's as they come, and the keyboards' made the same
   (an arrow ESC [ A..D, Ctrl and a letter the letter's control byte) -- and
   what it shows goes to a Printer of text and Backspace alone, which is all
   the screen understands: what changes is erased back to where it differs
   from what is shown, and printed again from there.

   Enter ends the line; Backspace (BS or DEL) takes back a character; ^C drops
   the line, ^U clears it. Up and Down (^P and ^N) go back through the last
   HistoryMax lines run, and forward again to the one being typed. ^R searches
   back through them for a line holding what is typed after it -- bash's
   reverse-i-search: ^R again finds the next older one, Backspace takes a
   character off what is searched for, Enter runs what was found, ^G puts the
   line from before the search back, and any other key keeps what was found,
   to be edited, and is then the key it is. The SSH session's editor
   (src/rust/ssh/src/terminal.rs) does the same on a terminal that knows ANSI.

   Not thread-safe: one task types at it. */
class LineEditor final
{
public:
    /* The longest line taken: what is typed past it is dropped */
    static const ulong LineMax = 80;
    /* Lines Up, Down and ^R go back through */
    static const ulong HistoryMax = 32;

    static constexpr char PromptText[] = "$";
    static constexpr char SearchText[] = "(reverse-i-search)'";
    static constexpr char FailedText[] = "(failed reverse-i-search)'";
    static constexpr char QueryEndText[] = "': ";

    LineEditor();

    /* A line begins: the prompt, and the line so far -- empty once a line
       has ended -- on out, at the start of a line of its own */
    void Prompt(Stdlib::Printer& out);

    /* One byte typed, what it changes shown on out. True when it ended a
       line, which GetLine() then is until the next call. */
    bool Key(char c, Stdlib::Printer& out);

    const char* GetLine() const { return Done; }

private:
    LineEditor(const LineEditor& other) = delete;
    LineEditor(LineEditor&& other) = delete;
    LineEditor& operator=(const LineEditor& other) = delete;
    LineEditor& operator=(LineEditor&& other) = delete;

    /* The keys the editor has a use for, as a terminal sends them */
    static const char KeyCtrlC = 0x03;
    static const char KeyCtrlG = 0x07;
    static const char KeyBackspace = 0x08;
    static const char KeyLf = 0x0A;
    static const char KeyCr = 0x0D;
    static const char KeyCtrlN = 0x0E;
    static const char KeyCtrlP = 0x10;
    static const char KeyCtrlR = 0x12;
    static const char KeyCtrlU = 0x15;
    static const char KeyEsc = 0x1B;
    static const char KeyDel = 0x7F;

    /* ECMA-48's control sequence: ESC [, parameter and intermediate bytes,
       and a final byte that says which key -- A up, B down */
    static const char CsiMiddleFirst = 0x20;
    static const char CsiMiddleLast = 0x3F;
    static const char CsiFinalFirst = 0x40;
    static const char CsiFinalLast = 0x7E;

    /* The most the editor ever shows: a failed search's prompt, what is
       searched for, and the line */
    static const ulong ShownMax = (sizeof(FailedText) - 1) + LineMax + (sizeof(QueryEndText) - 1) + LineMax;

    enum EscState
    {
        EscNone,
        EscStart,       /* ESC */
        EscSequence,    /* ESC [ or ESC O, until the final byte */
    };

    static bool IsPrintable(char c) { return c >= ' ' && c <= '~'; }

    bool Enter(Stdlib::Printer& out);
    void Cancel(Stdlib::Printer& out);
    void Remember();
    void Older();
    void Newer();

    /* ^R: the search begins, from the newest line */
    void StartSearch();
    /* The newest line older than below that holds the query: Line and
       Match are it, or Failed says there is none and they stay */
    void Find(ulong below);
    void AcceptSearch();

    const char* HistoryAt(ulong i) const;
    bool Holds(ulong i) const;

    /* Makes the screen show the editor's state, from where the two differ */
    void Show(Stdlib::Printer& out);

    char Line[LineMax + 1];
    ulong Len;

    /* A ring of the lines run, Count of them from First, oldest first */
    char History[HistoryMax][LineMax + 1];
    ulong First;
    ulong Count;

    /* Where Up and Down have got to in the history: Count when not in it */
    ulong Browse;
    /* The line being typed when Up took it away, for Down to give back */
    char Draft[LineMax + 1];
    ulong DraftLen;

    EscState Esc;

    bool Searching;
    /* Nothing older holds the query: the line is the last that did */
    bool Failed;
    char Query[LineMax + 1];
    ulong QueryLen;
    /* The history line the search found, Count before it found one */
    ulong Match;
    /* The line from before the search, for ^G */
    char Saved[LineMax + 1];
    ulong SavedLen;

    /* What the screen shows since the prompt began */
    char Shown[ShownMax + 1];
    ulong ShownLen;

    char Done[LineMax + 1];
};

}
