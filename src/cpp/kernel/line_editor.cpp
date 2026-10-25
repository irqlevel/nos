#include "line_editor.h"

#include <kernel/panic.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace
{

/* Appends n bytes of s to buf at len, which the caller has made room for */
void Put(char* buf, ulong size, ulong& len, const char* s, ulong n)
{
    BugOn(n > size - len);
    Stdlib::MemCpy(buf + len, s, n);
    len += n;
}

}

LineEditor::LineEditor()
    : Len(0)
    , First(0)
    , Count(0)
    , Browse(0)
    , DraftLen(0)
    , Esc(EscNone)
    , Searching(false)
    , Failed(false)
    , QueryLen(0)
    , Match(0)
    , SavedLen(0)
    , ShownLen(0)
{
    Stdlib::MemSet(Line, 0, sizeof(Line));
    Stdlib::MemSet(History, 0, sizeof(History));
    Stdlib::MemSet(Draft, 0, sizeof(Draft));
    Stdlib::MemSet(Query, 0, sizeof(Query));
    Stdlib::MemSet(Saved, 0, sizeof(Saved));
    Stdlib::MemSet(Shown, 0, sizeof(Shown));
    Stdlib::MemSet(Done, 0, sizeof(Done));
}

void LineEditor::Prompt(Stdlib::Printer& out)
{
    ShownLen = 0;
    Show(out);
}

bool LineEditor::Key(char c, Stdlib::Printer& out)
{
    /* An escape sequence is the terminal's name for a key, and only its
       final byte says which */
    switch (Esc)
    {
    case EscStart:
        Esc = EscNone;
        if (c == '[' || c == 'O')
        {
            Esc = EscSequence;
            return false;
        }
        /* ESC and a character is Alt and it, which nothing here has a use
           for; a control key is itself */
        if (IsPrintable(c))
            return false;
        break;
    case EscSequence:
        if (c >= CsiMiddleFirst && c <= CsiMiddleLast)
            return false;
        Esc = EscNone;
        if (c >= CsiFinalFirst && c <= CsiFinalLast)
        {
            if (c == 'A')
                Older();
            else if (c == 'B')
                Newer();
            Show(out);
            return false;
        }
        /* A control key cut the sequence short: it is a key of its own */
        break;
    case EscNone:
        break;
    }

    if (Searching)
    {
        if (IsPrintable(c))
        {
            if (QueryLen < LineMax)
            {
                Query[QueryLen++] = c;
                /* The line found so far may hold the longer query too */
                Find((Match < Count) ? Match + 1 : Count);
            }
            Show(out);
            return false;
        }

        switch (c)
        {
        case KeyCtrlR:
            Find(Match);
            Show(out);
            return false;
        case KeyBackspace:
        case KeyDel:
            /* A shorter query is searched for again from the newest line */
            if (QueryLen > 0)
            {
                QueryLen--;
                Stdlib::MemCpy(Line, Saved, SavedLen);
                Len = SavedLen;
                Match = Count;
                Failed = false;
                if (QueryLen > 0)
                    Find(Count);
            }
            Show(out);
            return false;
        case KeyCtrlG:
            Stdlib::MemCpy(Line, Saved, SavedLen);
            Len = SavedLen;
            Searching = false;
            Show(out);
            return false;
        case KeyCtrlC:
            Cancel(out);
            return false;
        default:
            /* Any other key keeps what was found, and is a key of the line's */
            AcceptSearch();
            break;
        }
    }

    switch (c)
    {
    case KeyCr:
    case KeyLf:
        return Enter(out);
    case KeyBackspace:
    case KeyDel:
        if (Len > 0)
            Len--;
        break;
    case KeyCtrlC:
        Cancel(out);
        return false;
    case KeyCtrlU:
        Len = 0;
        break;
    case KeyCtrlR:
        StartSearch();
        break;
    case KeyCtrlP:
        Older();
        break;
    case KeyCtrlN:
        Newer();
        break;
    case KeyEsc:
        Esc = EscStart;
        break;
    default:
        /* Tab, the other control keys and whatever is past ASCII: no
           command has a use for them */
        if (IsPrintable(c) && Len < LineMax)
            Line[Len++] = c;
        break;
    }

    Show(out);
    return false;
}

bool LineEditor::Enter(Stdlib::Printer& out)
{
    /* What runs is shown after the prompt: a search's find too */
    Show(out);
    out.PrintString("\n");
    ShownLen = 0;

    Stdlib::MemCpy(Done, Line, Len);
    Done[Len] = '\0';
    Remember();

    Len = 0;
    DraftLen = 0;
    Browse = Count;
    return true;
}

void LineEditor::Cancel(Stdlib::Printer& out)
{
    out.PrintString("^C\n");
    ShownLen = 0;

    Searching = false;
    Len = 0;
    DraftLen = 0;
    Browse = Count;
    Show(out);
}

void LineEditor::Remember()
{
    ulong len = Stdlib::StrLen(Done);
    bool blank = true;
    for (ulong i = 0; i < len; i++)
    {
        if (Done[i] != ' ')
        {
            blank = false;
            break;
        }
    }
    if (blank)
        return;

    /* The same line twice running is kept once */
    if (Count != 0 && Stdlib::StrCmp(HistoryAt(Count - 1), Done) == 0)
        return;

    ulong slot;
    if (Count < HistoryMax)
    {
        slot = (First + Count) % HistoryMax;
        Count++;
    }
    else
    {
        slot = First;
        First = (First + 1) % HistoryMax;
    }
    Stdlib::MemCpy(History[slot], Done, len + 1);
}

const char* LineEditor::HistoryAt(ulong i) const
{
    BugOn(i >= Count);
    return History[(First + i) % HistoryMax];
}

void LineEditor::Older()
{
    if (Browse == 0)
        return;

    if (Browse == Count)
    {
        Stdlib::MemCpy(Draft, Line, Len);
        DraftLen = Len;
    }
    Browse--;

    const char* line = HistoryAt(Browse);
    Len = Stdlib::StrLen(line);
    Stdlib::MemCpy(Line, line, Len);
}

void LineEditor::Newer()
{
    if (Browse >= Count)
        return;

    Browse++;
    if (Browse == Count)
    {
        Stdlib::MemCpy(Line, Draft, DraftLen);
        Len = DraftLen;
    }
    else
    {
        const char* line = HistoryAt(Browse);
        Len = Stdlib::StrLen(line);
        Stdlib::MemCpy(Line, line, Len);
    }
}

void LineEditor::StartSearch()
{
    Searching = true;
    Failed = false;
    QueryLen = 0;
    Match = Count;
    Stdlib::MemCpy(Saved, Line, Len);
    SavedLen = Len;
}

bool LineEditor::Holds(ulong i) const
{
    const char* line = HistoryAt(i);
    ulong len = Stdlib::StrLen(line);
    if (QueryLen > len)
        return false;

    for (ulong at = 0; at + QueryLen <= len; at++)
    {
        if (Stdlib::MemCmp(line + at, Query, QueryLen) == 0)
            return true;
    }
    return false;
}

void LineEditor::Find(ulong below)
{
    BugOn(below > Count);

    for (ulong i = below; i > 0; i--)
    {
        if (Holds(i - 1))
        {
            Match = i - 1;
            Failed = false;
            const char* line = HistoryAt(Match);
            Len = Stdlib::StrLen(line);
            Stdlib::MemCpy(Line, line, Len);
            return;
        }
    }
    Failed = true;
}

void LineEditor::AcceptSearch()
{
    Searching = false;
    if (Match < Count)
    {
        /* Up and Down go on from what was found, and Down past the newest
           line still gives back the one being typed before it all */
        if (Browse == Count)
        {
            Stdlib::MemCpy(Draft, Saved, SavedLen);
            DraftLen = SavedLen;
        }
        Browse = Match;
    }
}

void LineEditor::Show(Stdlib::Printer& out)
{
    char text[ShownMax];
    ulong len = 0;
    if (Searching)
    {
        if (Failed)
            Put(text, sizeof(text), len, FailedText, sizeof(FailedText) - 1);
        else
            Put(text, sizeof(text), len, SearchText, sizeof(SearchText) - 1);
        Put(text, sizeof(text), len, Query, QueryLen);
        Put(text, sizeof(text), len, QueryEndText, sizeof(QueryEndText) - 1);
    }
    else
    {
        Put(text, sizeof(text), len, PromptText, sizeof(PromptText) - 1);
    }
    Put(text, sizeof(text), len, Line, Len);

    ulong same = 0;
    while (same < ShownLen && same < len && Shown[same] == text[same])
        same++;

    for (ulong i = same; i < ShownLen; i++)
        out.Backspace();

    if (same < len)
    {
        char tail[ShownMax + 1];
        Stdlib::MemCpy(tail, text + same, len - same);
        tail[len - same] = '\0';
        out.PrintString(tail);
    }

    Stdlib::MemCpy(Shown, text, len);
    ShownLen = len;
}

}
