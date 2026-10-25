// The console shell's line editor (kernel/line_editor.cpp): what is typed at
// the screen's keyboard or the serial port, made a command line, with a
// history and bash's ^R search through it. What reaches it is whatever a
// terminal on the serial port sends -- any byte, escape sequences cut short,
// lines longer than it takes -- so the keys are anything, weighted to the
// ones it acts on and to a handful of letters, so that lines repeat and a
// search finds them. Every key is held to a model written from
// line_editor.h, in std::string: the line Enter hands the shell, and after
// every key what the screen shows -- built from the editor's own output,
// text and Backspace and nothing else, as the console's is -- must be the
// prompt and the line, or the search's prompt, its query and what it found,
// exactly; a Backspace on an empty row erases the line above it and is a
// finding too.
#include "host.h"

#include <kernel/line_editor.h>

#include "fuzz.h"

#include <string>
#include <vector>

namespace
{

using Kernel::LineEditor;

const char CtrlC = 0x03;
const char CtrlG = 0x07;
const char Bs = 0x08;
const char Lf = 0x0A;
const char Cr = 0x0D;
const char CtrlN = 0x0E;
const char CtrlP = 0x10;
const char CtrlR = 0x12;
const char CtrlU = 0x15;
const char Esc = 0x1B;
const char Del = 0x7F;

bool Printable(char c)
{
    return c >= ' ' && c <= '~';
}

/* The one row of the screen the editor draws on: its text and Backspace */
class Screen final : public Stdlib::Printer
{
public:
    std::string Row;

    virtual void Printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        VPrintf(fmt, args);
        va_end(args);
    }

    virtual void VPrintf(const char *fmt, va_list args) override
    {
        char text[512];
        vsnprintf(text, sizeof(text), fmt, args);
        PrintString(text);
    }

    virtual void PrintString(const char *s) override
    {
        for (; *s != '\0'; s++)
        {
            INVARIANT(*s == '\n' || Printable(*s), "the editor printed byte 0x%02X", (unsigned)(unsigned char)*s);
            if (*s == '\n')
                Row.clear();
            else
                Row += *s;
        }
    }

    virtual void Backspace() override
    {
        INVARIANT(!Row.empty(), "a Backspace on an empty row: the line above is erased");
        Row.pop_back();
    }
};

/* line_editor.h, read in std::string */
struct Model
{
    enum { EscNone, EscStart, EscSequence } EscState = EscNone;
    std::string Line;
    std::vector<std::string> History;
    size_t Browse = 0;
    std::string Draft;
    bool Searching = false;
    bool Failed = false;
    std::string Query;
    size_t Match = 0;
    std::string Saved;
    /* What Enter handed the shell, when the last key ended a line */
    bool Ended = false;
    std::string Done;

    size_t Count() const { return History.size(); }

    void Older()
    {
        if (Browse == 0)
            return;
        if (Browse == Count())
            Draft = Line;
        Browse--;
        Line = History[Browse];
    }

    void Newer()
    {
        if (Browse >= Count())
            return;
        Browse++;
        Line = (Browse == Count()) ? Draft : History[Browse];
    }

    /* The newest line older than below that holds the query */
    void Find(size_t below)
    {
        for (size_t i = below; i > 0; i--)
        {
            if (History[i - 1].find(Query) != std::string::npos)
            {
                Match = i - 1;
                Failed = false;
                Line = History[Match];
                return;
            }
        }
        Failed = true;
    }

    void Enter()
    {
        Ended = true;
        Done = Line;
        bool blank = Line.find_first_not_of(' ') == std::string::npos;
        if (!blank && (History.empty() || History.back() != Line))
        {
            if (History.size() == LineEditor::HistoryMax)
                History.erase(History.begin());
            History.push_back(Line);
        }
        Line.clear();
        Draft.clear();
        Browse = Count();
    }

    void Cancel()
    {
        Searching = false;
        Line.clear();
        Draft.clear();
        Browse = Count();
    }

    void Key(char c)
    {
        Ended = false;

        if (EscState == EscStart)
        {
            EscState = EscNone;
            if (c == '[' || c == 'O')
            {
                EscState = EscSequence;
                return;
            }
            if (Printable(c))
                return;
        }
        else if (EscState == EscSequence)
        {
            if (c >= 0x20 && c <= 0x3F)
                return;
            EscState = EscNone;
            if (c >= 0x40 && c <= 0x7E)
            {
                if (c == 'A')
                    Older();
                else if (c == 'B')
                    Newer();
                return;
            }
        }

        if (Searching)
        {
            if (Printable(c))
            {
                if (Query.size() < LineEditor::LineMax)
                {
                    Query += c;
                    Find(Match < Count() ? Match + 1 : Count());
                }
                return;
            }
            if (c == CtrlR)
            {
                Find(Match);
                return;
            }
            if (c == Bs || c == Del)
            {
                if (!Query.empty())
                {
                    Query.pop_back();
                    Line = Saved;
                    Match = Count();
                    Failed = false;
                    if (!Query.empty())
                        Find(Count());
                }
                return;
            }
            if (c == CtrlG)
            {
                Line = Saved;
                Searching = false;
                return;
            }
            if (c == CtrlC)
            {
                Cancel();
                return;
            }
            Searching = false;
            if (Match < Count())
            {
                if (Browse == Count())
                    Draft = Saved;
                Browse = Match;
            }
        }

        switch (c)
        {
        case Cr:
        case Lf:
            Enter();
            break;
        case Bs:
        case Del:
            if (!Line.empty())
                Line.pop_back();
            break;
        case CtrlC:
            Cancel();
            break;
        case CtrlU:
            Line.clear();
            break;
        case CtrlR:
            Searching = true;
            Failed = false;
            Query.clear();
            Match = Count();
            Saved = Line;
            break;
        case CtrlP:
            Older();
            break;
        case CtrlN:
            Newer();
            break;
        case Esc:
            EscState = EscStart;
            break;
        default:
            if (Printable(c) && Line.size() < LineEditor::LineMax)
                Line += c;
            break;
        }
    }

    /* What the screen shows after the key */
    std::string Shown() const
    {
        if (Searching)
            return std::string(Failed ? LineEditor::FailedText : LineEditor::SearchText) + Query +
                   LineEditor::QueryEndText + Line;
        return std::string(LineEditor::PromptText) + Line;
    }
};

/* The keys the editor acts on, and letters few enough to repeat */
const char Keys[] = {Cr, Lf, Bs, Del, CtrlC, CtrlG, CtrlN, CtrlP, CtrlR, CtrlR, CtrlU, Esc, Esc, '[', 'O',
                     'A', 'B', 'C', 'D', '1', ';', '\t', 0};
const char Letters[] = "ab c";

char NextKey(Fuzz::Input& in)
{
    switch (in.Below(8))
    {
    case 0:
    case 1:
        return in.Pick(Keys);
    case 2:
        return static_cast<char>(in.U8());
    default:
        return Letters[in.Below(sizeof(Letters) - 1)];
    }
}

void Run(Fuzz::Input& in)
{
    LineEditor* ed = new LineEditor();
    Model m;
    Screen screen;

    ed->Prompt(screen);
    INVARIANT(screen.Row == m.Shown(), "the first prompt shows '%s'", screen.Row.c_str());

    while (in.More())
    {
        /* Now and then a run of one key: a line past LineMax, a history
           past HistoryMax, Up past its oldest line */
        size_t times = in.Chance(16) ? in.Range(1, 2 * LineEditor::LineMax) : 1;
        char c = NextKey(in);
        if (times > 1)
            Fuzz::Reached("a run of one key");

        for (size_t i = 0; i < times; i++)
        {
            bool ended = ed->Key(c, screen);
            m.Key(c);
            Fuzz::Say("key 0x%02X: '%s'", (unsigned)(unsigned char)c, screen.Row.c_str());

            INVARIANT(ended == m.Ended, "key 0x%02X %s a line, the model says it %s",
                      (unsigned)(unsigned char)c, ended ? "ended" : "did not end", m.Ended ? "did" : "did not");
            if (ended)
            {
                INVARIANT(ed->GetLine() == m.Done, "Enter ran '%s', the model '%s'", ed->GetLine(), m.Done.c_str());
                INVARIANT(screen.Row.empty(), "Enter left '%s' on the line it ended", screen.Row.c_str());
                Fuzz::Reached("a line run");
                if (m.Count() == LineEditor::HistoryMax)
                    Fuzz::Reached("a full history");
                ed->Prompt(screen);
            }

            INVARIANT(screen.Row == m.Shown(), "after key 0x%02X the screen shows '%s', the model '%s'",
                      (unsigned)(unsigned char)c, screen.Row.c_str(), m.Shown().c_str());

            if (m.Searching && m.Failed)
                Fuzz::Reached("a search that failed");
            if (m.Searching && m.Match < m.Count())
                Fuzz::Reached("a search that found a line");
            if (!m.Searching && m.Browse < m.Count())
                Fuzz::Reached("a line from the history");
            if (m.Line.size() == LineEditor::LineMax)
                Fuzz::Reached("a line at LineMax");
        }
    }
    delete ed;
}

void Reset()
{
}

}

const Fuzz::Target Fuzz::TheTarget = {"lineedit", Reset, Run, 4096, 20000};
