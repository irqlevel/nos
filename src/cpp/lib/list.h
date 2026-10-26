#pragma once

#include "list_entry.h"
#include <include/types.h>
#include <kernel/panic.h>

namespace stdlib
{

template <typename T>
class LinkedList final
{
public:

    class Iterator final
    {
    public:
        Iterator()
            : curr_list_entry_(nullptr)
            , end_list_(nullptr)
        {
        }

        Iterator(const Iterator& other)
        {
            curr_list_entry_ = other.curr_list_entry_;
            end_list_ = other.end_list_;
        }

        Iterator(LinkedList& list)
            : Iterator()
        {
            curr_list_entry_ = list.list_head_.flink;
            end_list_ = &list.list_head_;
        }

        Iterator& operator=(const Iterator& other)
        {
            if (this != &other)
            {
                curr_list_entry_ = other.curr_list_entry_;
                end_list_ = other.end_list_;
            }
            return *this;
        }

        Iterator& operator=(Iterator&& other)
        {
            if (this != &other)
            {
                curr_list_entry_ = other.curr_list_entry_;
                end_list_ = other.end_list_;
                other.curr_list_entry_ = nullptr;
                other.end_list_ = nullptr; 
            }
            return *this;
        }

        T& get()
        {
            BUG_ON(curr_list_entry_ == end_list_);
            LinkedListNode* node = CONTAINING_RECORD(curr_list_entry_,
                                                     LinkedListNode,
                                                     list_link_);
            return node->value_;
        }

        bool is_valid()
        {
            if (curr_list_entry_ != nullptr && end_list_ != nullptr)
            {
                return (curr_list_entry_ != end_list_) ? true : false;
            }
            else
            {
                return false;
            }
        }

        void next()
        {
            if (is_valid())
            {
                curr_list_entry_ = curr_list_entry_->flink;
            }
        }

        void erase()
        {
            BUG_ON(!is_valid());

            ListEntry* next = curr_list_entry_->flink;

            LinkedListNode* node = CONTAINING_RECORD(curr_list_entry_,
                                                     LinkedListNode,
                                                     list_link_);
            node->list_link_.remove_init();
            delete node;
            curr_list_entry_ = next;
        }

        ~Iterator()
        {

        }
    private:
        ListEntry* curr_list_entry_;
        ListEntry* end_list_;
    };

    LinkedList()
    {
        list_head_.init();
    }

    bool add_head(const T& value)
    {
        LinkedListNode* node = kernel::mm::talloc<LinkedListNode, 0>(value);

        if (!node)
        {
            return false;
        }
        list_head_.insert_head(&node->list_link_);
        return true;
    }

    bool add_tail(const T& value)
    {
        LinkedListNode* node = kernel::mm::talloc<LinkedListNode, 0>(value);

        if (!node)
        {
            return false;
        }
        list_head_.insert_tail(&node->list_link_);
        return true;
    }

    bool add_tail(T&& value)
    {
        LinkedListNode* node = kernel::mm::talloc<LinkedListNode, 0>(stdlib::move(value));
        if (!node)
        {
            return false;
        }
        list_head_.insert_tail(&node->list_link_);
        return true;
    }

    void add_tail(LinkedList&& other)
    {
        if (other.list_head_.is_empty())
            return;

        ListEntry* entry = other.list_head_.flink;
        other.list_head_.remove_init();
        list_head_.append_tail(entry);
        return;
    }

    T& head()
    {
        LinkedListNode* node;

        BUG_ON(list_head_.is_empty());

        node = CONTAINING_RECORD(list_head_.flink, LinkedListNode, list_link_);
        return node->value_;
    }

    T& tail()
    {
        LinkedListNode* node;

        BUG_ON(list_head_.is_empty());

        node = CONTAINING_RECORD(list_head_.blink, LinkedListNode, list_link_);
        return node->value_;
    }

    void pop_head()
    {
        LinkedListNode* node;

        BUG_ON(list_head_.is_empty());

        node = CONTAINING_RECORD(list_head_.remove_head(),
                                 LinkedListNode, list_link_);
        delete node;
    }

    void pop_tail()
    {
        LinkedListNode* node;

        BUG_ON(list_head_.is_empty());

        node = CONTAINING_RECORD(list_head_.remove_tail(),
                                 LinkedListNode, list_link_);
        delete node;
    }

    bool is_empty()
    {
        return list_head_.is_empty();
    }

    ~LinkedList()
    {
        release();
    }

    LinkedList(LinkedList&& other)
    {
        list_head_.init();
        add_tail(stdlib::move(other));
    }

    LinkedList& operator=(LinkedList&& other)
    {
        if (this != &other)
        {
            release();

            list_head_.init();
            add_tail(stdlib::move(other));
        }
        return *this;
    }

    Iterator get_iterator()
    {
        return Iterator(*this);
    }

    size_t count()
    {
        size_t count = 0;
        auto it = get_iterator();
        for (; it.is_valid();it.next())
        {
            count++;
        }
        return count;
    }

    void clear()
    {
        release();
    }

private:
    LinkedList(const LinkedList& other) = delete;
    LinkedList& operator=(const LinkedList& other) = delete;

    void release()
    {
        LinkedListNode* node;
        while (!list_head_.is_empty())
        {
            node = CONTAINING_RECORD(list_head_.remove_head(),
                                     LinkedListNode, list_link_);
            delete node;
        }
    }

    class LinkedListNode final
    {
    public:
        LinkedListNode(const T& value)
        {
            list_link_.init();
            value_ = value;
        }
        LinkedListNode(T&& value)
        {
            list_link_.init();
            value_ = stdlib::move(value);
        }
        ListEntry list_link_;
        T value_;
    private:
        LinkedListNode() = delete;
        LinkedListNode(const LinkedListNode& other) = delete;
        LinkedListNode& operator=(const LinkedListNode& other) = delete;
        LinkedListNode& operator=(LinkedListNode&& other) = delete;
    };
    ListEntry list_head_;
};

}