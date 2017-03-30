#pragma once

namespace Shared
{

struct ListEntry
{
    struct ListEntry *Flink;
    struct ListEntry *Blink;

    void Init();

    bool IsEmpty();

    bool Remove();

    bool RemoveInit();

    ListEntry* RemoveHead();

    ListEntry* RemoveTail();

    void InsertTail(ListEntry* entry);

    void AppendTail(ListEntry* listToAppend);

    void InsertHead(ListEntry* Entry);

};

}