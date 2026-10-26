#include "list_entry.h"
#include <kernel/panic.h>

namespace stdlib
{

ListEntry::ListEntry()
{
    init();
}

void ListEntry::init()
{
    flink = blink = this;
    return;
}

bool ListEntry::is_empty()
{
    return (flink == this) ? true : false;
}

bool ListEntry::remove()
{
    ListEntry* prev;
    ListEntry* next;

    next = flink;
    prev = blink;
    prev->flink = next;
    next->blink = prev;
    return (next == prev) ? true : false;
}

bool ListEntry::remove_init()
{
    bool result = remove();
    init();
    return result;
}

ListEntry* ListEntry::remove_head()
{
    ListEntry* next;
    ListEntry* entry;

    entry = flink;
    next = entry->flink;
    flink = next;
    next->blink = this;
    return entry;
}

ListEntry* ListEntry::remove_tail()
{
    ListEntry* prev;
    ListEntry* entry;

    entry = blink;
    prev = entry->blink;
    blink = prev;
    prev->flink = this;
    return entry;
}

void ListEntry::insert_tail(ListEntry* entry)
{
    ListEntry* prev;

    prev = blink;
    BUG_ON(!prev);
    entry->flink = this;
    entry->blink = prev;
    prev->flink = entry;
    blink = entry;
    return;
}

void ListEntry::append_tail(ListEntry* list_to_append)
{
    ListEntry* list_end = blink;

    blink->flink = list_to_append;
    blink = list_to_append->blink;
    list_to_append->blink->flink = this;
    list_to_append->blink = list_end;
}

void ListEntry::insert_head(ListEntry* entry)
{
    ListEntry* next;

    next = flink;
    entry->flink = next;
    entry->blink = this;
    next->blink = entry;
    flink = entry;
    return;
}

void ListEntry::move_tail_list(ListEntry *list)
{
    if (list->is_empty())
        return;

    ListEntry* entry = list->flink;
    list->remove_init();
    append_tail(entry);
}

ListEntry::ListEntry(ListEntry&& other)
{
    if (other.is_empty()) {
        init();
        return;
    }
    other.flink->blink = this;
    other.blink->flink = this;
    flink = other.flink;
    blink = other.blink;
    other.init();
}

ListEntry& ListEntry::operator=(ListEntry&& other)
{
    if (this != &other) {
        if (other.is_empty()) {
            init();
            return *this;
        }
        other.flink->blink = this;
        other.blink->flink = this;
        flink = other.flink;
        blink = other.blink;
        other.init();
    }
    return *this;
}

size_t ListEntry::count_entries()
{
    size_t count = 0;

    for (ListEntry* e = flink; e != this; e = e->flink)
    {
        count++;
    }

    return count;
}

}