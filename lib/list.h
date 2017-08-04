#pragma once

#include "list_entry.h"
#include <include/types.h>
#include <include/panic.h>

namespace Stdlib
{

template <typename T>
class LinkedList final
{
public:

    class Iterator final
    {
    public:
        Iterator()
            : CurrListEntry(nullptr)
            , EndList(nullptr)
        {
        }

        Iterator(const Iterator& other)
        {
            CurrListEntry = other.CurrListEntry;
            EndList = other.EndList;
        }

        Iterator(LinkedList& List)
            : Iterator()
        {
            CurrListEntry = List.ListHead.Flink;
            EndList = &List.ListHead;
        }

        Iterator& operator=(const Iterator& other)
        {
            if (this != &other)
            {
                CurrListEntry = other.CurrListEntry;
                EndList = other.EndList;
            }
            return *this;
        }

        Iterator& operator=(Iterator&& other)
        {
            if (this != &other)
            {
                CurrListEntry = other.CurrListEntry;
                EndList = other.EndList;
                other.CurrListEntry = nullptr;
                other.EndList = nullptr; 
            }
            return *this;
        }

        T& Get()
        {
            panic(CurrListEntry == EndList);
            LinkedListNode* node = CONTAINING_RECORD(CurrListEntry,
                                                     LinkedListNode,
                                                     ListLink);
            return node->Value;
        }

        bool IsValid()
        {
            if (CurrListEntry != nullptr && EndList != nullptr)
            {
                return (CurrListEntry != EndList) ? true : false;
            }
            else
            {
                return false;
            }
        }

        void Next()
        {
            if (IsValid())
            {
                CurrListEntry = CurrListEntry->Flink;
            }
        }

        void Erase()
        {
            panic(!IsValid());

            ListEntry* next = CurrListEntry->Flink;

            LinkedListNode* node = CONTAINING_RECORD(CurrListEntry,
                                                     LinkedListNode,
                                                     ListLink);
            node->ListLink.RemoveInit();
            delete node;
            CurrListEntry = next;
        }

        ~Iterator()
        {

        }
    private:
        ListEntry* CurrListEntry;
        ListEntry* EndList;
    };

    LinkedList()
    {
        ListHead.Init();
    }

    bool AddHead(const T& value)
    {
        LinkedListNode* node = new LinkedListNode(value);

        if (!node)
        {
            return false;
        }
        ListHead.InsertHead(&node->ListLink);
        return true;
    }

    bool AddTail(const T& value)
    {
        LinkedListNode* node = new LinkedListNode(value);

        if (!node)
        {
            return false;
        }
        ListHead.InsertTail(&node->ListLink);
        return true;
    }

    bool AddTail(T&& value)
    {
        LinkedListNode* node = new LinkedListNode(Stdlib::Move(value));
        if (!node)
        {
            return false;
        }
        ListHead.InsertTail(&node->ListLink);
        return true;
    }

    void AddTail(LinkedList&& other)
    {
        if (other.ListHead.IsEmpty())
            return;

        ListEntry* entry = other.ListHead.Flink;
        other.ListHead.RemoveInit();
        ListHead.AppendTail(entry);
        return;
    }

    T& Head()
    {
        LinkedListNode* node;

        BugOn(ListHead.IsEmpty());

        node = CONTAINING_RECORD(ListHead.Flink, LinkedListNode, ListLink);
        return node->Value;
    }

    T& Tail()
    {
        LinkedListNode* node;

        BugOn(ListHead.IsEmpty());

        node = CONTAINING_RECORD(ListHead.Blink, LinkedListNode, ListLink);
        return node->Value;
    }

    void PopHead()
    {
        LinkedListNode* node;

        BugOn(ListHead.IsEmpty());

        node = CONTAINING_RECORD(ListHead.RemoveHead(),
                                 LinkedListNode, ListLink);
        delete node;
    }

    void PopTail()
    {
        LinkedListNode* node;

        BugOn(ListHead.IsEmpty());

        node = CONTAINING_RECORD(ListHead.RemoveTail(),
                                 LinkedListNode, ListLink);
        delete node;
    }

    bool IsEmpty()
    {
        return ListHead.IsEmpty();
    }

    ~LinkedList()
    {
        Release();
    }

    LinkedList(LinkedList&& other)
    {
        ListHead.Init();
        AddTail(Stdlib::Move(other));
    }

    LinkedList& operator=(LinkedList&& other)
    {
        if (this != &other)
        {
            Release();

            ListHead.Init();
            AddTail(Stdlib::Move(other));
        }
        return *this;
    }

    Iterator GetIterator()
    {
        return Iterator(*this);
    }

    size_t Count()
    {
        size_t count = 0;
        auto it = GetIterator();
        for (; it.IsValid();it.Next())
        {
            count++;
        }
        return count;
    }

    void Clear()
    {
        Release();
    }

private:
    LinkedList(const LinkedList& other) = delete;
    LinkedList& operator=(const LinkedList& other) = delete;

    void Release()
    {
        LinkedListNode* node;
        while (!ListHead.IsEmpty())
        {
            node = CONTAINING_RECORD(ListHead.RemoveHead(),
                                     LinkedListNode, ListLink);
            delete node;
        }
    }

    class LinkedListNode final
    {
    public:
        LinkedListNode(const T& value)
        {
            ListLink.Init();
            Value = value;
        }
        LinkedListNode(T&& value)
        {
            ListLink.Init();
            Value = Stdlib::Move(value);
        }
        ListEntry ListLink;
        T Value;
    private:
        LinkedListNode() = delete;
        LinkedListNode(const LinkedListNode& other) = delete;
        LinkedListNode& operator=(const LinkedListNode& other) = delete;
        LinkedListNode& operator=(LinkedListNode&& other) = delete;
    };
    ListEntry ListHead;
};

}