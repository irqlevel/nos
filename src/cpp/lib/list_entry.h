#pragma once

#include <include/types.h>

namespace stdlib
{

struct ListEntry final
{
    struct ListEntry *flink;
    struct ListEntry *blink;

    ListEntry();

    void init();

    bool is_empty();

    bool remove();

    bool remove_init();

    ListEntry* remove_head();

    ListEntry* remove_tail();

    void insert_tail(ListEntry* entry);

    void append_tail(ListEntry* list_to_append);

    void insert_head(ListEntry* entry);

    void move_tail_list(ListEntry* list);

    ListEntry(ListEntry&& other);
    ListEntry& operator=(ListEntry&& other);

    size_t count_entries();

private:
    ListEntry(const ListEntry& other) = delete;
    ListEntry& operator=(const ListEntry& other) = delete;
};

}