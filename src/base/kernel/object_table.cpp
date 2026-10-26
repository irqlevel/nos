#include "object_table.h"

namespace kernel
{

ObjectTable::ObjectTable()
{
    for (ObjectId object_id = 0; object_id < stdlib::array_size(object_array_); object_id++)
    {
        object_array_[object_id] = nullptr;
    }
}

ulong ObjectTable::insert(Object *object)
{
    object->get();
    {
        stdlib::AutoLock lock(lock_);
        for (ObjectId object_id = 0; object_id < stdlib::array_size(object_array_); object_id++)
        {
            if (object_array_[object_id] == nullptr)
            {
                object_array_[object_id] = object;
                return object_id;
            }
        }
    }
    object->put();
    return INVALID_OBJECT_ID;
}

void ObjectTable::remove(ulong object_id)
{
    if (object_id >= stdlib::array_size(object_array_))
        return;

    Object* object = nullptr;
    {
        stdlib::AutoLock lock(lock_);
        object = object_array_[object_id];
        object_array_[object_id] = nullptr;
    }

    if (object != nullptr)
    {
        object->put();
    }
}

Object* ObjectTable::lookup(ulong object_id)
{
    if (object_id >= stdlib::array_size(object_array_))
        return nullptr;

    Object* object = nullptr;
    {
        stdlib::AutoLock lock(lock_);
        object = object_array_[object_id];
        if (object != nullptr)
        {
            object->get();
            return object;
        }
    }

    return nullptr;
}

ObjectTable::~ObjectTable()
{
    for (size_t object_id = 0; object_id < stdlib::array_size(object_array_); object_id++)
    {
        Object* object = object_array_[object_id];
        object_array_[object_id] = nullptr;
        if (object != nullptr)
        {
            object->put();
        }
    }
}

}