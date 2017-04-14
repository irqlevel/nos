#pragma once

namespace Shared
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

private:
    ListEntry(const ListEntry& other) = delete;
    ListEntry(ListEntry&& other) = delete;
    ListEntry& operator=(const ListEntry& other) = delete;
    ListEntry& operator=(ListEntry&& other) = delete;
};

}