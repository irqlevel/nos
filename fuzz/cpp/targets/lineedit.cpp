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

using kernel::LineEditor;

const char CTRL_C = 0x03;
const char CTRL_G = 0x07;
const char BS = 0x08;
const char LF = 0x0A;
const char CR = 0x0D;
const char CTRL_N = 0x0E;
const char CTRL_P = 0x10;
const char CTRL_R = 0x12;
const char CTRL_U = 0x15;
const char ESC = 0x1B;
const char DEL = 0x7F;

bool printable(char c)
{
    return c >= ' ' && c <= '~';
}

/* The one row of the screen the editor draws on: its text and Backspace */
class Screen final : public stdlib::Printer
{
public:
    std::string row_;

    virtual void printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        v_printf(fmt, args);
        va_end(args);
    }

    virtual void v_printf(const char *fmt, va_list args) override
    {
        char text[512];
        vsnprintf(text, sizeof(text), fmt, args);
        print_string(text);
    }

    virtual void print_string(const char *s) override
    {
        for (; *s != '\0'; s++)
        {
            INVARIANT(*s == '\n' || printable(*s), "the editor printed byte 0x%02X", (unsigned)(unsigned char)*s);
            if (*s == '\n')
                row_.clear();
            else
                row_ += *s;
        }
    }

    virtual void backspace() override
    {
        INVARIANT(!row_.empty(), "a Backspace on an empty row: the line above is erased");
        row_.pop_back();
    }
};

/* line_editor.h, read in std::string */
struct Model
{
    enum { EscNone, EscStart, EscSequence } esc_state = EscNone;
    std::string line;
    std::vector<std::string> history;
    size_t browse = 0;
    std::string draft;
    bool searching = false;
    bool failed = false;
    std::string query;
    size_t match = 0;
    std::string saved;
    /* What Enter handed the shell, when the last key ended a line */
    bool ended = false;
    std::string done;

    size_t count() const { return history.size(); }

    void older()
    {
        if (browse == 0)
            return;
        if (browse == count())
            draft = line;
        browse--;
        line = history[browse];
    }

    void newer()
    {
        if (browse >= count())
            return;
        browse++;
        line = (browse == count()) ? draft : history[browse];
    }

    /* The newest line older than below that holds the query */
    void find(size_t below)
    {
        for (size_t i = below; i > 0; i--)
        {
            if (history[i - 1].find(query) != std::string::npos)
            {
                match = i - 1;
                failed = false;
                line = history[match];
                return;
            }
        }
        failed = true;
    }

    void enter()
    {
        ended = true;
        done = line;
        bool blank = line.find_first_not_of(' ') == std::string::npos;
        if (!blank && (history.empty() || history.back() != line))
        {
            if (history.size() == LineEditor::HISTORY_MAX)
                history.erase(history.begin());
            history.push_back(line);
        }
        line.clear();
        draft.clear();
        browse = count();
    }

    void cancel()
    {
        searching = false;
        line.clear();
        draft.clear();
        browse = count();
    }

    void key(char c)
    {
        ended = false;

        if (esc_state == EscStart)
        {
            esc_state = EscNone;
            if (c == '[' || c == 'O')
            {
                esc_state = EscSequence;
                return;
            }
            if (printable(c))
                return;
        }
        else if (esc_state == EscSequence)
        {
            if (c >= 0x20 && c <= 0x3F)
                return;
            esc_state = EscNone;
            if (c >= 0x40 && c <= 0x7E)
            {
                if (c == 'A')
                    older();
                else if (c == 'B')
                    newer();
                return;
            }
        }

        if (searching)
        {
            if (printable(c))
            {
                if (query.size() < LineEditor::LINE_LEN_MAX)
                {
                    query += c;
                    find(match < count() ? match + 1 : count());
                }
                return;
            }
            if (c == CTRL_R)
            {
                find(match);
                return;
            }
            if (c == BS || c == DEL)
            {
                if (!query.empty())
                {
                    query.pop_back();
                    line = saved;
                    match = count();
                    failed = false;
                    if (!query.empty())
                        find(count());
                }
                return;
            }
            if (c == CTRL_G)
            {
                line = saved;
                searching = false;
                return;
            }
            if (c == CTRL_C)
            {
                cancel();
                return;
            }
            searching = false;
            if (match < count())
            {
                if (browse == count())
                    draft = saved;
                browse = match;
            }
        }

        switch (c)
        {
        case CR:
        case LF:
            enter();
            break;
        case BS:
        case DEL:
            if (!line.empty())
                line.pop_back();
            break;
        case CTRL_C:
            cancel();
            break;
        case CTRL_U:
            line.clear();
            break;
        case CTRL_R:
            searching = true;
            failed = false;
            query.clear();
            match = count();
            saved = line;
            break;
        case CTRL_P:
            older();
            break;
        case CTRL_N:
            newer();
            break;
        case ESC:
            esc_state = EscStart;
            break;
        default:
            if (printable(c) && line.size() < LineEditor::LINE_LEN_MAX)
                line += c;
            break;
        }
    }

    /* What the screen shows after the key */
    std::string shown() const
    {
        if (searching)
            return std::string(failed ? LineEditor::FAILED_TEXT : LineEditor::SEARCH_TEXT) + query +
                   LineEditor::QUERY_END_TEXT + line;
        return std::string(LineEditor::PROMPT_TEXT) + line;
    }
};

/* The keys the editor acts on, and letters few enough to repeat */
const char KEYS[] = {CR, LF, BS, DEL, CTRL_C, CTRL_G, CTRL_N, CTRL_P, CTRL_R, CTRL_R, CTRL_U, ESC, ESC, '[', 'O',
                     'A', 'B', 'C', 'D', '1', ';', '\t', 0};
const char LETTERS[] = "ab c";

char next_key(fuzz::Input& in)
{
    switch (in.below(8))
    {
    case 0:
    case 1:
        return in.pick(KEYS);
    case 2:
        return static_cast<char>(in.u8());
    default:
        return LETTERS[in.below(sizeof(LETTERS) - 1)];
    }
}

void run(fuzz::Input& in)
{
    LineEditor* ed = new LineEditor();
    Model m;
    Screen screen;

    ed->prompt(screen);
    INVARIANT(screen.row_ == m.shown(), "the first prompt shows '%s'", screen.row_.c_str());

    while (in.more())
    {
        /* Now and then a run of one key: a line past LineMax, a history
           past HistoryMax, Up past its oldest line */
        size_t times = in.chance(16) ? in.range(1, 2 * LineEditor::LINE_LEN_MAX) : 1;
        char c = next_key(in);
        if (times > 1)
            fuzz::reached("a run of one key");

        for (size_t i = 0; i < times; i++)
        {
            bool ended = ed->key(c, screen);
            m.key(c);
            fuzz::say("key 0x%02X: '%s'", (unsigned)(unsigned char)c, screen.row_.c_str());

            INVARIANT(ended == m.ended, "key 0x%02X %s a line, the model says it %s",
                      (unsigned)(unsigned char)c, ended ? "ended" : "did not end", m.ended ? "did" : "did not");
            if (ended)
            {
                INVARIANT(ed->get_line() == m.done, "Enter ran '%s', the model '%s'", ed->get_line(), m.done.c_str());
                INVARIANT(screen.row_.empty(), "Enter left '%s' on the line it ended", screen.row_.c_str());
                fuzz::reached("a line run");
                if (m.count() == LineEditor::HISTORY_MAX)
                    fuzz::reached("a full history");
                ed->prompt(screen);
            }

            INVARIANT(screen.row_ == m.shown(), "after key 0x%02X the screen shows '%s', the model '%s'",
                      (unsigned)(unsigned char)c, screen.row_.c_str(), m.shown().c_str());

            if (m.searching && m.failed)
                fuzz::reached("a search that failed");
            if (m.searching && m.match < m.count())
                fuzz::reached("a search that found a line");
            if (!m.searching && m.browse < m.count())
                fuzz::reached("a line from the history");
            if (m.line.size() == LineEditor::LINE_LEN_MAX)
                fuzz::reached("a line at LineMax");
        }
    }
    delete ed;
}

void reset()
{
}

}

const fuzz::Target fuzz::THE_TARGET = {"lineedit", reset, run, 4096, 20000};
