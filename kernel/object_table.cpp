#include "object_table.h"

namespace Kernel
{

ObjectTable::ObjectTable()
{
    for (ObjectId objectId = 0; objectId < Shared::ArraySize(ObjectArray); objectId++)
    {
        ObjectArray[objectId] = nullptr;
    }
}

ulong ObjectTable::Insert(Object *object)
{
    object->Get();
    {
        Shared::AutoLock lock(Lock);
        for (ObjectId objectId = 0; objectId < Shared::ArraySize(ObjectArray); objectId++)
        {
            if (ObjectArray[objectId] == nullptr)
            {
                ObjectArray[objectId] = object;
                return objectId;
            }
        }
    }
    object->Put();
    return InvalidObjectId;
}

void ObjectTable::Remove(ulong objectId)
{
    if (objectId >= Shared::ArraySize(ObjectArray))
        return;

    Object* object = nullptr;
    {
        Shared::AutoLock lock(Lock);
        object = ObjectArray[objectId];
        ObjectArray[objectId] = nullptr;
    }

    if (object != nullptr)
    {
        object->Put();
    }
}

Object* ObjectTable::Lookup(ulong objectId)
{
    if (objectId >= Shared::ArraySize(ObjectArray))
        return nullptr;

    Object* object = nullptr;
    {
        Shared::AutoLock lock(Lock);
        object = ObjectArray[objectId];
        if (object != nullptr)
        {
            object->Get();
            return object;
        }
    }

    return nullptr;
}

ObjectTable::~ObjectTable()
{
    for (size_t objectId = 0; objectId < Shared::ArraySize(ObjectArray); objectId++)
    {
        Object* object = ObjectArray[objectId];
        ObjectArray[objectId] = nullptr;
        if (object != nullptr)
        {
            object->Put();
        }
    }
}

}