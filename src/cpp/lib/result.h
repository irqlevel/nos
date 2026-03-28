#pragma once

#include "error.h"
#include "stdlib.h"

namespace Stdlib
{

template<typename T>
class Result
{
public:
    Result(const T& value)
        : Value(value)
        , Err(Error::Success)
    {
    }

    Result(T&& value)
        : Value(Move(value))
        , Err(Error::Success)
    {
    }

    Result(const Error& err)
        : Value()
        , Err(err)
    {
    }

    Result(Error&& err)
        : Value()
        , Err(Move(err))
    {
    }

    ~Result()
    {
    }

    Result(const Result& other)
        : Value(other.Value)
        , Err(other.Err)
    {
    }

    Result& operator=(const Result& other)
    {
        if (this != &other)
        {
            Value = other.Value;
            Err = other.Err;
        }
        return *this;
    }

    Result(Result&& other)
        : Value(Move(other.Value))
        , Err(Move(other.Err))
    {
    }

    Result& operator=(Result&& other)
    {
        if (this != &other)
        {
            Value = Move(other.Value);
            Err = Move(other.Err);
        }
        return *this;
    }

    bool Ok() const
    {
        return Err.Ok();
    }

    const T& GetValue() const
    {
        return Value;
    }

    T& GetValue()
    {
        return Value;
    }

    const Error& GetError() const
    {
        return Err;
    }

    Error& GetError()
    {
        return Err;
    }

private:
    T Value;
    Error Err;
};

}
