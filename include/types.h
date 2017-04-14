#pragma once

#include <stdint.h>
#include <stdarg.h>

static_assert(sizeof(char) == 1, "Invalid size");

typedef unsigned long size_t;
typedef long ssize_t;

typedef unsigned long ulong;

typedef int8_t s8;
typedef int16_t s16;
typedef int32_t s32;
typedef int64_t s64;

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;

#define likely(x)       __builtin_expect((x),1)
#define unlikely(x)     __builtin_expect((x),0)

#define OFFSET_OF(type, field)  \
            (unsigned long)&((type*)0)->field

#define CONTAINING_RECORD(addr, type, field)    \
            (type*)((unsigned long)(addr) - (unsigned long)&((type*)0)->field)


