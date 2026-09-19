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

/* __builtin_offsetof, what offsetof is: the old &((type*)0)->field made a
   member access through a null pointer, which is undefined behaviour -- the
   first report the UBSan build made (docs/build.md). Some of the types an
   intrusive list runs through are not standard-layout (Task has a vtable),
   where offsetof is conditionally supported rather than undefined: clang
   defines it for every type without a virtual base, and refuses one with.
   So -Winvalid-offsetof is quiet here and nowhere else. */
#define OFFSET_OF(type, field)  \
            _Pragma("clang diagnostic push") \
            _Pragma("clang diagnostic ignored \"-Winvalid-offsetof\"") \
            ((unsigned long)__builtin_offsetof(type, field)) \
            _Pragma("clang diagnostic pop")

#define CONTAINING_RECORD(addr, type, field)    \
            ((type*)((unsigned long)(addr) - OFFSET_OF(type, field)))


