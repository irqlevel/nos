#pragma once

#include "error.h"
#include "stdlib.h"

namespace stdlib
{

template<typename T>
class Result
{
public:
    Result(const T& value)
        : value_(value)
        , err_(Error::SUCCESS)
    {
    }

    Result(T&& value)
        : value_(move(value))
        , err_(Error::SUCCESS)
    {
    }

    Result(const Error& err)
        : value_()
        , err_(err)
    {
    }

    Result(Error&& err)
        : value_()
        , err_(move(err))
    {
    }

    ~Result()
    {
    }

    Result(const Result& other)
        : value_(other.value_)
        , err_(other.err_)
    {
    }

    Result& operator=(const Result& other)
    {
        if (this != &other)
        {
            value_ = other.value_;
            err_ = other.err_;
        }
        return *this;
    }

    Result(Result&& other)
        : value_(move(other.value_))
        , err_(move(other.err_))
    {
    }

    Result& operator=(Result&& other)
    {
        if (this != &other)
        {
            value_ = move(other.value_);
            err_ = move(other.err_);
        }
        return *this;
    }

    bool ok() const
    {
        return err_.ok();
    }

    const T& get_value() const
    {
        return value_;
    }

    T& get_value()
    {
        return value_;
    }

    const Error& get_error() const
    {
        return err_;
    }

    Error& get_error()
    {
        return err_;
    }

private:
    T value_;
    Error err_;
};

}
