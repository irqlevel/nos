#include "list_entry.h"

namespace Shared
{

void ListEntry::Init()
{
    Flink = Blink = this;
    return;
}

bool ListEntry::IsEmpty()
{
    return (Flink == this) ? true : false;
}

bool ListEntry::Remove()
{
    ListEntry* blink;
    ListEntry* flink;

    flink = Flink;
    blink = Blink;
    blink->Flink = flink;
    flink->Blink = blink;
    return (flink == blink) ? true : false;
}

bool ListEntry::RemoveInit()
{
    bool result = Remove();
    Init();
    return result;
}

ListEntry* ListEntry::RemoveHead()
{
    ListEntry* flink;
    ListEntry* entry;

    entry = Flink;
    flink = entry->Flink;
    Flink = flink;
    flink->Blink = this;
    return entry;
}

ListEntry* ListEntry::RemoveTail()
{
    ListEntry* blink;
    ListEntry* entry;

    entry = Blink;
    blink = entry->Blink;
    Blink = blink;
    blink->Flink = this;
    return entry;
}

void ListEntry::InsertTail(ListEntry* entry)
{

    ListEntry* blink;

    blink = Blink;
    entry->Flink = this;
    entry->Blink = blink;
    blink->Flink = entry;
    Blink = entry;
    return;
}

void ListEntry::AppendTail(ListEntry* listToAppend)
{
    ListEntry* listEnd = Blink;

    Blink->Flink = listToAppend;
    Blink = listToAppend->Blink;
    listToAppend->Blink->Flink = this;
    listToAppend->Blink = listEnd;
}

void ListEntry::InsertHead(ListEntry* entry)
{

    ListEntry* flink;

    flink = Flink;
    entry->Flink = flink;
    entry->Blink = this;
    flink->Blink = entry;
    Flink = entry;
    return;
}

}