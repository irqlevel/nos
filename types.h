#ifndef __TYPES_H__
#define __TYPES_H__

#include <stdint.h>
#include <stdarg.h>

typedef unsigned long size_t;
typedef long ssize_t;

#if !defined(__cplusplus)
typedef unsigned char bool;
#endif

typedef unsigned long ulong;

typedef int8_t s8;
typedef int16_t s16;
typedef int32_t s32;
typedef int64_t s64;

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;

typedef u8 byte;

#define true	((bool)1)
#define false	((bool)0)

#define NULL ((void *)((ulong)0))

#define ARRAY_SIZE(x) (sizeof(x) / sizeof((x)[0]))
#define FIELD_SIZEOF(t, f) (sizeof(((t *)0)->f))

#define U64_HIGH(value)	\
		((u32)(((value) >> 32ULL) & 0x00000000FFFFFFFFULL))

#define U64_LOW(value)	\
		((u32)((value) & 0x00000000FFFFFFFFULL))

#define U32_HIGH(value)	\
		((u16)(((value) >> 16) & 0x0000FFFF))

#define U32_LOW(value)	\
		((u16)((value) & 0x0000FFFF))

#define likely(x)       __builtin_expect((x),1)
#define unlikely(x)     __builtin_expect((x),0)

#endif
