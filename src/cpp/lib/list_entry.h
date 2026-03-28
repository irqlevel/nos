#pragma once

#include <include/types.h>

namespace Stdlib
{

struct ListEntry final
{
    struct ListEntry *Flink;
    struct ListEntry *Blink;

    ListEntry();

    void Init();

    bool IsEmpty();

    bool Remove();

    bool RemoveInit();

    ListEntry* RemoveHead();

    ListEntry* RemoveTail();

    void InsertTail(ListEntry* entry);

    void AppendTail(ListEntry* listToAppend);

    void InsertHead(ListEntry* Entry);

    void MoveTailList(ListEntry* list);

    ListEntry(ListEntry&& other);
    ListEntry& operator=(ListEntry&& other);

    size_t CountEntries();

private:
    ListEntry(const ListEntry& other) = delete;
    ListEntry& operator=(const ListEntry& other) = delete;
};

}