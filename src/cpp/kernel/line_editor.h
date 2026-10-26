#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace kernel
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
   HISTORY_MAX lines run, and forward again to the one being typed. ^R searches
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
    static const ulong LINE_LEN_MAX = 80;
    /* Lines Up, Down and ^R go back through */
    static const ulong HISTORY_MAX = 32;

    static constexpr char PROMPT_TEXT[] = "$";
    static constexpr char SEARCH_TEXT[] = "(reverse-i-search)'";
    static constexpr char FAILED_TEXT[] = "(failed reverse-i-search)'";
    static constexpr char QUERY_END_TEXT[] = "': ";

    LineEditor();

    /* A line begins: the prompt, and the line so far -- empty once a line
       has ended -- on out, at the start of a line of its own */
    void prompt(stdlib::Printer& out);

    /* One byte typed, what it changes shown on out. True when it ended a
       line, which get_line() then is until the next call. */
    bool key(char c, stdlib::Printer& out);

    const char* get_line() const { return done_; }

private:
    LineEditor(const LineEditor& other) = delete;
    LineEditor(LineEditor&& other) = delete;
    LineEditor& operator=(const LineEditor& other) = delete;
    LineEditor& operator=(LineEditor&& other) = delete;

    /* The keys the editor has a use for, as a terminal sends them */
    static const char KEY_CTRL_C = 0x03;
    static const char KEY_CTRL_G = 0x07;
    static const char KEY_BACKSPACE = 0x08;
    static const char KEY_LF = 0x0A;
    static const char KEY_CR = 0x0D;
    static const char KEY_CTRL_N = 0x0E;
    static const char KEY_CTRL_P = 0x10;
    static const char KEY_CTRL_R = 0x12;
    static const char KEY_CTRL_U = 0x15;
    static const char KEY_ESC = 0x1B;
    static const char KEY_DEL = 0x7F;

    /* ECMA-48's control sequence: ESC [, parameter and intermediate bytes,
       and a final byte that says which key -- A up, B down */
    static const char CSI_MIDDLE_FIRST = 0x20;
    static const char CSI_MIDDLE_LAST = 0x3F;
    static const char CSI_FINAL_FIRST = 0x40;
    static const char CSI_FINAL_LAST = 0x7E;

    /* The most the editor ever shows: a failed search's prompt, what is
       searched for, and the line */
    static const ulong SHOWN_MAX = (sizeof(FAILED_TEXT) - 1) + LINE_LEN_MAX + (sizeof(QUERY_END_TEXT) - 1) + LINE_LEN_MAX;

    enum EscState
    {
        EscNone,
        EscStart,       /* ESC */
        EscSequence,    /* ESC [ or ESC O, until the final byte */
    };

    static bool is_printable(char c) { return c >= ' ' && c <= '~'; }

    bool enter(stdlib::Printer& out);
    void cancel(stdlib::Printer& out);
    void remember();
    void older();
    void newer();

    /* ^R: the search begins, from the newest line */
    void start_search();
    /* The newest line older than below that holds the query: Line and
       Match are it, or Failed says there is none and they stay */
    void find(ulong below);
    void accept_search();

    const char* history_at(ulong i) const;
    bool holds(ulong i) const;

    /* Makes the screen show the editor's state, from where the two differ */
    void show(stdlib::Printer& out);

    char line_[LINE_LEN_MAX + 1];
    ulong len_;

    /* A ring of the lines run, Count of them from First, oldest first */
    char history_[HISTORY_MAX][LINE_LEN_MAX + 1];
    ulong first_;
    ulong count_;

    /* Where Up and Down have got to in the history: Count when not in it */
    ulong browse_;
    /* The line being typed when Up took it away, for Down to give back */
    char draft_[LINE_LEN_MAX + 1];
    ulong draft_len_;

    EscState esc_;

    bool searching_;
    /* Nothing older holds the query: the line is the last that did */
    bool failed_;
    char query_[LINE_LEN_MAX + 1];
    ulong query_len_;
    /* The history line the search found, Count before it found one */
    ulong match_;
    /* The line from before the search, for ^G */
    char saved_[LINE_LEN_MAX + 1];
    ulong saved_len_;

    /* What the screen shows since the prompt began */
    char shown_[SHOWN_MAX + 1];
    ulong shown_len_;

    char done_[LINE_LEN_MAX + 1];
};

}
