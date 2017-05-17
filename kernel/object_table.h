#pragma once

#include <lib/stdlib.h>
#include "spin_lock.h"

namespace Kernel
{

class Object
{
public:
    virtual void Get() = 0;
    virtual void Put() = 0;
};

using ObjectId = ulong;

const ObjectId InvalidObjectId = ~((ObjectId)0);

class ObjectTable final
{
public:
    ObjectTable();
    ~ObjectTable();

    ulong Insert(Object *object);

    void Remove(ulong objectId);

    Object* Lookup(ulong objectId);

private:
    ObjectTable(const ObjectTable& other) = delete;
    ObjectTable(ObjectTable&& other) = delete;
    ObjectTable& operator=(const ObjectTable& other) = delete;
    ObjectTable& operator=(ObjectTable&& other) = delete;

    static const ulong MaxObjectId = 256;

    Object* ObjectArray[MaxObjectId];

    SpinLock Lock;
};

}