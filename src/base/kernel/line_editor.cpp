#include "line_editor.h"

#include <kernel/panic.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace
{

/* Appends n bytes of s to buf at len, which the caller has made room for */
void put(char* buf, ulong size, ulong& len, const char* s, ulong n)
{
    BUG_ON(n > size - len);
    stdlib::mem_cpy(buf + len, s, n);
    len += n;
}

}

LineEditor::LineEditor()
    : len_(0)
    , first_(0)
    , count_(0)
    , browse_(0)
    , draft_len_(0)
    , esc_(EscNone)
    , searching_(false)
    , failed_(false)
    , query_len_(0)
    , match_(0)
    , saved_len_(0)
    , shown_len_(0)
{
    stdlib::mem_set(line_, 0, sizeof(line_));
    stdlib::mem_set(history_, 0, sizeof(history_));
    stdlib::mem_set(draft_, 0, sizeof(draft_));
    stdlib::mem_set(query_, 0, sizeof(query_));
    stdlib::mem_set(saved_, 0, sizeof(saved_));
    stdlib::mem_set(shown_, 0, sizeof(shown_));
    stdlib::mem_set(done_, 0, sizeof(done_));
}

void LineEditor::prompt(stdlib::Printer& out)
{
    shown_len_ = 0;
    show(out);
}

bool LineEditor::key(char c, stdlib::Printer& out)
{
    /* An escape sequence is the terminal's name for a key, and only its
       final byte says which */
    switch (esc_)
    {
    case EscStart:
        esc_ = EscNone;
        if (c == '[' || c == 'O')
        {
            esc_ = EscSequence;
            return false;
        }
        /* ESC and a character is Alt and it, which nothing here has a use
           for; a control key is itself */
        if (is_printable(c))
            return false;
        break;
    case EscSequence:
        if (c >= CSI_MIDDLE_FIRST && c <= CSI_MIDDLE_LAST)
            return false;
        esc_ = EscNone;
        if (c >= CSI_FINAL_FIRST && c <= CSI_FINAL_LAST)
        {
            if (c == 'A')
                older();
            else if (c == 'B')
                newer();
            show(out);
            return false;
        }
        /* A control key cut the sequence short: it is a key of its own */
        break;
    case EscNone:
        break;
    }

    if (searching_)
    {
        if (is_printable(c))
        {
            if (query_len_ < LINE_LEN_MAX)
            {
                query_[query_len_++] = c;
                /* The line found so far may hold the longer query too */
                find((match_ < count_) ? match_ + 1 : count_);
            }
            show(out);
            return false;
        }

        switch (c)
        {
        case KEY_CTRL_R:
            find(match_);
            show(out);
            return false;
        case KEY_BACKSPACE:
        case KEY_DEL:
            /* A shorter query is searched for again from the newest line */
            if (query_len_ > 0)
            {
                query_len_--;
                stdlib::mem_cpy(line_, saved_, saved_len_);
                len_ = saved_len_;
                match_ = count_;
                failed_ = false;
                if (query_len_ > 0)
                    find(count_);
            }
            show(out);
            return false;
        case KEY_CTRL_G:
            stdlib::mem_cpy(line_, saved_, saved_len_);
            len_ = saved_len_;
            searching_ = false;
            show(out);
            return false;
        case KEY_CTRL_C:
            cancel(out);
            return false;
        default:
            /* Any other key keeps what was found, and is a key of the line's */
            accept_search();
            break;
        }
    }

    switch (c)
    {
    case KEY_CR:
    case KEY_LF:
        return enter(out);
    case KEY_BACKSPACE:
    case KEY_DEL:
        if (len_ > 0)
            len_--;
        break;
    case KEY_CTRL_C:
        cancel(out);
        return false;
    case KEY_CTRL_U:
        len_ = 0;
        break;
    case KEY_CTRL_R:
        start_search();
        break;
    case KEY_CTRL_P:
        older();
        break;
    case KEY_CTRL_N:
        newer();
        break;
    case KEY_ESC:
        esc_ = EscStart;
        break;
    default:
        /* Tab, the other control keys and whatever is past ASCII: no
           command has a use for them */
        if (is_printable(c) && len_ < LINE_LEN_MAX)
            line_[len_++] = c;
        break;
    }

    show(out);
    return false;
}

bool LineEditor::enter(stdlib::Printer& out)
{
    /* What runs is shown after the prompt: a search's find too */
    show(out);
    out.print_string("\n");
    shown_len_ = 0;

    stdlib::mem_cpy(done_, line_, len_);
    done_[len_] = '\0';
    remember();

    len_ = 0;
    draft_len_ = 0;
    browse_ = count_;
    return true;
}

void LineEditor::cancel(stdlib::Printer& out)
{
    out.print_string("^C\n");
    shown_len_ = 0;

    searching_ = false;
    len_ = 0;
    draft_len_ = 0;
    browse_ = count_;
    show(out);
}

void LineEditor::remember()
{
    ulong len = stdlib::str_len(done_);
    bool blank = true;
    for (ulong i = 0; i < len; i++)
    {
        if (done_[i] != ' ')
        {
            blank = false;
            break;
        }
    }
    if (blank)
        return;

    /* The same line twice running is kept once */
    if (count_ != 0 && stdlib::str_cmp(history_at(count_ - 1), done_) == 0)
        return;

    ulong slot;
    if (count_ < HISTORY_MAX)
    {
        slot = (first_ + count_) % HISTORY_MAX;
        count_++;
    }
    else
    {
        slot = first_;
        first_ = (first_ + 1) % HISTORY_MAX;
    }
    stdlib::mem_cpy(history_[slot], done_, len + 1);
}

const char* LineEditor::history_at(ulong i) const
{
    BUG_ON(i >= count_);
    return history_[(first_ + i) % HISTORY_MAX];
}

void LineEditor::older()
{
    if (browse_ == 0)
        return;

    if (browse_ == count_)
    {
        stdlib::mem_cpy(draft_, line_, len_);
        draft_len_ = len_;
    }
    browse_--;

    const char* line = history_at(browse_);
    len_ = stdlib::str_len(line);
    stdlib::mem_cpy(line_, line, len_);
}

void LineEditor::newer()
{
    if (browse_ >= count_)
        return;

    browse_++;
    if (browse_ == count_)
    {
        stdlib::mem_cpy(line_, draft_, draft_len_);
        len_ = draft_len_;
    }
    else
    {
        const char* line = history_at(browse_);
        len_ = stdlib::str_len(line);
        stdlib::mem_cpy(line_, line, len_);
    }
}

void LineEditor::start_search()
{
    searching_ = true;
    failed_ = false;
    query_len_ = 0;
    match_ = count_;
    stdlib::mem_cpy(saved_, line_, len_);
    saved_len_ = len_;
}

bool LineEditor::holds(ulong i) const
{
    const char* line = history_at(i);
    ulong len = stdlib::str_len(line);
    if (query_len_ > len)
        return false;

    for (ulong at = 0; at + query_len_ <= len; at++)
    {
        if (stdlib::mem_cmp(line + at, query_, query_len_) == 0)
            return true;
    }
    return false;
}

void LineEditor::find(ulong below)
{
    BUG_ON(below > count_);

    for (ulong i = below; i > 0; i--)
    {
        if (holds(i - 1))
        {
            match_ = i - 1;
            failed_ = false;
            const char* line = history_at(match_);
            len_ = stdlib::str_len(line);
            stdlib::mem_cpy(line_, line, len_);
            return;
        }
    }
    failed_ = true;
}

void LineEditor::accept_search()
{
    searching_ = false;
    if (match_ < count_)
    {
        /* Up and Down go on from what was found, and Down past the newest
           line still gives back the one being typed before it all */
        if (browse_ == count_)
        {
            stdlib::mem_cpy(draft_, saved_, saved_len_);
            draft_len_ = saved_len_;
        }
        browse_ = match_;
    }
}

void LineEditor::show(stdlib::Printer& out)
{
    char text[SHOWN_MAX];
    ulong len = 0;
    if (searching_)
    {
        if (failed_)
            put(text, sizeof(text), len, FAILED_TEXT, sizeof(FAILED_TEXT) - 1);
        else
            put(text, sizeof(text), len, SEARCH_TEXT, sizeof(SEARCH_TEXT) - 1);
        put(text, sizeof(text), len, query_, query_len_);
        put(text, sizeof(text), len, QUERY_END_TEXT, sizeof(QUERY_END_TEXT) - 1);
    }
    else
    {
        put(text, sizeof(text), len, PROMPT_TEXT, sizeof(PROMPT_TEXT) - 1);
    }
    put(text, sizeof(text), len, line_, len_);

    ulong same = 0;
    while (same < shown_len_ && same < len && shown_[same] == text[same])
        same++;

    for (ulong i = same; i < shown_len_; i++)
        out.backspace();

    if (same < len)
    {
        char tail[SHOWN_MAX + 1];
        stdlib::mem_cpy(tail, text + same, len - same);
        tail[len - same] = '\0';
        out.print_string(tail);
    }

    stdlib::mem_cpy(shown_, text, len);
    shown_len_ = len;
}

}
