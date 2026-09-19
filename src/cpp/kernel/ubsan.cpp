#include "ubsan.h"
#include "panic.h"
#include "trace.h"
#include "stack_trace.h"
#include "symtab.h"

#include <hal/console.h>
#include <hal/cpu.h>
#include <lib/stdlib.h>

/*
 * The runtime of clang's undefined-behaviour sanitizer (-fsanitize=undefined),
 * for a kernel built with UBSAN=1. The compiler puts a check in front of
 * every operation whose behaviour the language leaves undefined -- a shift
 * past the width, signed overflow, an index past a bound it knows, a null or
 * misaligned access, a bool or an enum holding no valid value, falling off
 * the end of a function -- and calls one of the handlers below when one
 * fails, with a description of the site the compiler made for it. This file
 * is built without the sanitizer.
 *
 * A report goes out through writers that take no lock -- the serial port,
 * polled, and the disk log -- because a check can fail anywhere: inside an
 * NMI handler, or on a CPU that holds the dmesg or netconsole lock, and a
 * report that deadlocks is worse than none. So a report is not in `dmesg`
 * and, under ubsan=warn, not on the netconsole either; the default, a panic,
 * goes everywhere a panic goes.
 *
 * Each site reports once: the first report sets the top bit of its column,
 * as the Linux runtime does. One report at a time: a check that fails inside
 * a report -- what the report calls is instrumented -- or on another CPU in
 * the meantime is counted and not printed, and its site stays unmarked, to
 * report the next time it fails.
 */

extern "C" void rust_disklog_log(const char* line);

namespace Kernel
{

namespace
{

/* What the compiler describes a site with: the layouts of compiler-rt's
   ubsan_handlers.h, clang's side of the ABI. */
struct SourceLocation
{
    const char* File;
    u32 Line;
    u32 Column;
};

/* Followed in memory by the type's name, NUL-terminated. */
struct TypeDescriptor
{
    u16 Kind;
    u16 Info;
};

struct TypeMismatchData
{
    SourceLocation Loc;
    const TypeDescriptor* Type;
    u8 LogAlignment;
    u8 CheckKind;
};

struct AlignmentAssumptionData
{
    SourceLocation Loc;
    SourceLocation AssumptionLoc;
    const TypeDescriptor* Type;
};

struct OverflowData
{
    SourceLocation Loc;
    const TypeDescriptor* Type;
};

struct ShiftOutOfBoundsData
{
    SourceLocation Loc;
    const TypeDescriptor* LhsType;
    const TypeDescriptor* RhsType;
};

struct OutOfBoundsData
{
    SourceLocation Loc;
    const TypeDescriptor* ArrayType;
    const TypeDescriptor* IndexType;
};

struct UnreachableData
{
    SourceLocation Loc;
};

struct VlaBoundData
{
    SourceLocation Loc;
    const TypeDescriptor* Type;
};

struct InvalidValueData
{
    SourceLocation Loc;
    const TypeDescriptor* Type;
};

struct InvalidBuiltinData
{
    SourceLocation Loc;
    u8 Kind;
};

struct NonNullReturnData
{
    SourceLocation AttrLoc;
};

struct NonNullArgData
{
    SourceLocation Loc;
    SourceLocation AttrLoc;
    int ArgIndex;
};

struct PointerOverflowData
{
    SourceLocation Loc;
};

struct FloatCastOverflowData
{
    SourceLocation Loc;
    const TypeDescriptor* FromType;
    const TypeDescriptor* ToType;
};

const u16 TypeKindInteger = 0;
const u16 TypeInfoSigned = 1;
const u32 ReportedBit = 0x80000000;
const unsigned MaxInlineBits = 64;
const size_t LineMax = 256;
const size_t ValueMax = 32;
const size_t BacktraceFrames = 16;

/* TypeMismatchData::CheckKind, in clang's order. */
const char* const CheckKindNames[] = {
    "load of", "store to", "reference binding to", "member access within",
    "member call on", "constructor call on", "downcast of", "downcast of",
    "upcast of", "cast to virtual base of", "_Nonnull binding to",
    "dynamic operation on",
};

/* InvalidBuiltinData::Kind */
const u8 BuiltinCtz = 0;
const u8 BuiltinClz = 1;

/* Constant-initialised, so nothing here needs a constructor to have run. */
int Busy;
bool WarnOnly;
unsigned long ReportCount;
unsigned long UnprintedCount;

const char* TypeName(const TypeDescriptor* type)
{
    return reinterpret_cast<const char*>(type) + sizeof(TypeDescriptor);
}

bool IsInteger(const TypeDescriptor* type)
{
    return type->Kind == TypeKindInteger;
}

bool IsSigned(const TypeDescriptor* type)
{
    return (type->Info & TypeInfoSigned) != 0;
}

unsigned BitWidth(const TypeDescriptor* type)
{
    return 1u << (type->Info >> 1);
}

/* An integer of up to 64 bits arrives in the handle itself; a wider one as a
   pointer to it, which is only ever printed as a width. */
bool IsInline(const TypeDescriptor* type)
{
    return IsInteger(type) && BitWidth(type) <= MaxInlineBits;
}

long SignedValue(const TypeDescriptor* type, ulong value)
{
    unsigned width = BitWidth(type);
    if (width >= MaxInlineBits)
        return (long)value;

    unsigned shift = MaxInlineBits - width;
    return (long)(value << shift) >> shift;
}

bool IsNegative(const TypeDescriptor* type, ulong value)
{
    return IsInline(type) && IsSigned(type) && SignedValue(type, value) < 0;
}

void FormatValue(char* buf, size_t size, const TypeDescriptor* type, ulong value)
{
    if (!IsInteger(type))
    {
        Stdlib::SnPrintf(buf, size, "<a value of type %s>", TypeName(type));
        return;
    }
    if (!IsInline(type))
    {
        Stdlib::SnPrintf(buf, size, "<a %u-bit value>", (ulong)BitWidth(type));
        return;
    }
    if (IsSigned(type))
    {
        Stdlib::SnPrintf(buf, size, "%d", SignedValue(type, value));
        return;
    }

    unsigned width = BitWidth(type);
    ulong masked = (width >= MaxInlineBits) ? value : (value & ((1UL << width) - 1));
    Stdlib::SnPrintf(buf, size, "%u", masked);
}

void Emit(const char* text)
{
    Hal::ConsolePanicWrite(text);
    rust_disklog_log(text);
}

/* Whether this failure is to be printed: nobody else is reporting, and its
   site has not reported yet. On true the caller holds Busy. */
bool Begin(SourceLocation* loc)
{
    if (__atomic_exchange_n(&Busy, 1, __ATOMIC_ACQUIRE) != 0)
    {
        __atomic_add_fetch(&UnprintedCount, 1, __ATOMIC_RELAXED);
        return false;
    }

    u32 before = __atomic_fetch_or(&loc->Column, ReportedBit, __ATOMIC_RELAXED);
    if (before & ReportedBit)
    {
        __atomic_store_n(&Busy, 0, __ATOMIC_RELEASE);
        return false;
    }
    return true;
}

/* Print the report and, unless ubsan=warn, panic with it. Called holding
   Busy, which a warning gives back. */
void Report(const SourceLocation* loc, const char* what)
{
    char line[LineMax];
    ulong count = __atomic_add_fetch(&ReportCount, 1, __ATOMIC_RELAXED);
    Stdlib::SnPrintf(line, sizeof(line), "UBSAN: %s at %s:%u:%u",
        what, loc->File, (ulong)loc->Line, (ulong)(loc->Column & ~ReportedBit));

    if (!__atomic_load_n(&WarnOnly, __ATOMIC_RELAXED))
        Panic("%s", line);

    ulong flags = Hal::IrqSave();

    Emit(line);
    Emit("\n");

    ulong frames[BacktraceFrames];
    size_t frameCount = StackTrace::Capture(frames, Stdlib::ArraySize(frames));
    char where[SymbolTable::DescribeMax];
    char buf[SymbolTable::DescribeMax + ValueMax];
    for (size_t i = 0; i < frameCount; i++)
    {
        if (SymbolTable::GetInstance().Describe(frames[i], where, sizeof(where)))
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%u] 0x%p %s\n", (ulong)i, frames[i], where);
        else
            Stdlib::SnPrintf(buf, sizeof(buf), "  [%u] 0x%p\n", (ulong)i, frames[i]);
        Emit(buf);
    }

    Stdlib::SnPrintf(buf, sizeof(buf), "UBSAN: %u report(s) so far\n", count);
    Emit(buf);

    Hal::IrqRestore(flags);
    __atomic_store_n(&Busy, 0, __ATOMIC_RELEASE);
}

/* The two checks there is no going on from. */
[[noreturn]] void Fatal(SourceLocation* loc, const char* what)
{
    if (Begin(loc))
        Report(loc, what);

    Panic("UBSAN: %s at %s:%u", what, loc->File, (ulong)loc->Line);
    __builtin_unreachable();
}

void Overflow(void* data, ulong lhs, ulong rhs, char op)
{
    auto* d = static_cast<OverflowData*>(data);
    if (!Begin(&d->Loc))
        return;

    char left[ValueMax], right[ValueMax], what[LineMax];
    FormatValue(left, sizeof(left), d->Type, lhs);
    FormatValue(right, sizeof(right), d->Type, rhs);
    Stdlib::SnPrintf(what, sizeof(what), "%s integer overflow: %s %c %s cannot be represented in type %s",
        IsSigned(d->Type) ? "signed" : "unsigned", left, op, right, TypeName(d->Type));
    Report(&d->Loc, what);
}

}

namespace Ubsan
{

void SetWarnOnly(bool warnOnly)
{
    __atomic_store_n(&WarnOnly, warnOnly, __ATOMIC_RELAXED);
}

bool Enabled()
{
#ifdef NOS_UBSAN
    return true;
#else
    return false;
#endif
}

void Announce()
{
    if (!Enabled())
        return;

    Trace(0, "UBSan: on -- %s", __atomic_load_n(&WarnOnly, __ATOMIC_RELAXED)
        ? "ubsan=warn: each site reports once, to the serial port and the disk log, and the kernel goes on"
        : "the first report is a panic");
}

unsigned long Reports()
{
    return __atomic_load_n(&ReportCount, __ATOMIC_RELAXED);
}

unsigned long Unprinted()
{
    return __atomic_load_n(&UnprintedCount, __ATOMIC_RELAXED);
}

}

}

using namespace Kernel;

/* The handlers clang calls. Recoverable ones only: the build never asks for
   -fno-sanitize-recover, so no _abort variant is referenced, and a build
   that did would fail to link rather than go without. */
extern "C"
{

void __ubsan_handle_type_mismatch_v1(void* data, ulong pointer)
{
    auto* d = static_cast<TypeMismatchData*>(data);
    if (!Begin(&d->Loc))
        return;

    const char* check = (d->CheckKind < Stdlib::ArraySize(CheckKindNames))
        ? CheckKindNames[d->CheckKind] : "access to";
    ulong alignment = 1UL << d->LogAlignment;
    char what[LineMax];

    if (pointer == 0)
        Stdlib::SnPrintf(what, sizeof(what), "%s null pointer of type %s", check, TypeName(d->Type));
    else if (pointer & (alignment - 1))
        Stdlib::SnPrintf(what, sizeof(what), "%s misaligned address 0x%p for type %s, which requires %u byte alignment",
            check, pointer, TypeName(d->Type), alignment);
    else
        Stdlib::SnPrintf(what, sizeof(what), "%s address 0x%p with insufficient space for an object of type %s",
            check, pointer, TypeName(d->Type));
    Report(&d->Loc, what);
}

void __ubsan_handle_alignment_assumption(void* data, ulong pointer, ulong alignment, ulong offset)
{
    auto* d = static_cast<AlignmentAssumptionData*>(data);
    if (!Begin(&d->Loc))
        return;

    char what[LineMax];
    Stdlib::SnPrintf(what, sizeof(what), "assumption of %u byte alignment (offset %u) for pointer 0x%p of type %s failed",
        alignment, offset, pointer, TypeName(d->Type));
    Report(&d->Loc, what);
}

void __ubsan_handle_add_overflow(void* data, ulong lhs, ulong rhs)
{
    Overflow(data, lhs, rhs, '+');
}

void __ubsan_handle_sub_overflow(void* data, ulong lhs, ulong rhs)
{
    Overflow(data, lhs, rhs, '-');
}

void __ubsan_handle_mul_overflow(void* data, ulong lhs, ulong rhs)
{
    Overflow(data, lhs, rhs, '*');
}

void __ubsan_handle_negate_overflow(void* data, ulong old)
{
    auto* d = static_cast<OverflowData*>(data);
    if (!Begin(&d->Loc))
        return;

    char value[ValueMax], what[LineMax];
    FormatValue(value, sizeof(value), d->Type, old);
    Stdlib::SnPrintf(what, sizeof(what), "negation of %s cannot be represented in type %s", value, TypeName(d->Type));
    Report(&d->Loc, what);
}

void __ubsan_handle_divrem_overflow(void* data, ulong lhs, ulong rhs)
{
    auto* d = static_cast<OverflowData*>(data);
    if (!Begin(&d->Loc))
        return;

    char left[ValueMax], what[LineMax];
    FormatValue(left, sizeof(left), d->Type, lhs);
    if (IsInline(d->Type) && (IsSigned(d->Type) ? SignedValue(d->Type, rhs) == 0 : rhs == 0))
        Stdlib::SnPrintf(what, sizeof(what), "division of %s by zero", left);
    else
        Stdlib::SnPrintf(what, sizeof(what), "division of %s by -1 cannot be represented in type %s",
            left, TypeName(d->Type));
    Report(&d->Loc, what);
}

void __ubsan_handle_shift_out_of_bounds(void* data, ulong lhs, ulong rhs)
{
    auto* d = static_cast<ShiftOutOfBoundsData*>(data);
    if (!Begin(&d->Loc))
        return;

    char left[ValueMax], right[ValueMax], what[LineMax];
    FormatValue(left, sizeof(left), d->LhsType, lhs);
    FormatValue(right, sizeof(right), d->RhsType, rhs);

    if (IsNegative(d->RhsType, rhs))
        Stdlib::SnPrintf(what, sizeof(what), "shift exponent %s is negative", right);
    else if (IsInline(d->RhsType) && rhs >= BitWidth(d->LhsType))
        Stdlib::SnPrintf(what, sizeof(what), "shift exponent %s is too large for %u-bit type %s",
            right, (ulong)BitWidth(d->LhsType), TypeName(d->LhsType));
    else if (IsNegative(d->LhsType, lhs))
        Stdlib::SnPrintf(what, sizeof(what), "left shift of negative value %s", left);
    else
        Stdlib::SnPrintf(what, sizeof(what), "left shift of %s by %s places cannot be represented in type %s",
            left, right, TypeName(d->LhsType));
    Report(&d->Loc, what);
}

void __ubsan_handle_out_of_bounds(void* data, ulong index)
{
    auto* d = static_cast<OutOfBoundsData*>(data);
    if (!Begin(&d->Loc))
        return;

    char value[ValueMax], what[LineMax];
    FormatValue(value, sizeof(value), d->IndexType, index);
    Stdlib::SnPrintf(what, sizeof(what), "index %s out of bounds for type %s", value, TypeName(d->ArrayType));
    Report(&d->Loc, what);
}

void __ubsan_handle_builtin_unreachable(void* data)
{
    Fatal(&static_cast<UnreachableData*>(data)->Loc, "execution reached an unreachable program point");
}

void __ubsan_handle_missing_return(void* data)
{
    Fatal(&static_cast<UnreachableData*>(data)->Loc,
        "execution reached the end of a value-returning function without returning a value");
}

void __ubsan_handle_vla_bound_not_positive(void* data, ulong bound)
{
    auto* d = static_cast<VlaBoundData*>(data);
    if (!Begin(&d->Loc))
        return;

    char value[ValueMax], what[LineMax];
    FormatValue(value, sizeof(value), d->Type, bound);
    Stdlib::SnPrintf(what, sizeof(what), "variable length array bound evaluates to non-positive value %s", value);
    Report(&d->Loc, what);
}

void __ubsan_handle_float_cast_overflow(void* data, ulong)
{
    auto* d = static_cast<FloatCastOverflowData*>(data);
    if (!Begin(&d->Loc))
        return;

    char what[LineMax];
    Stdlib::SnPrintf(what, sizeof(what), "a value of type %s is outside the range of type %s",
        TypeName(d->FromType), TypeName(d->ToType));
    Report(&d->Loc, what);
}

void __ubsan_handle_load_invalid_value(void* data, ulong value)
{
    auto* d = static_cast<InvalidValueData*>(data);
    if (!Begin(&d->Loc))
        return;

    char text[ValueMax], what[LineMax];
    FormatValue(text, sizeof(text), d->Type, value);
    Stdlib::SnPrintf(what, sizeof(what), "load of value %s, which is not a valid value for type %s",
        text, TypeName(d->Type));
    Report(&d->Loc, what);
}

void __ubsan_handle_invalid_builtin(void* data)
{
    auto* d = static_cast<InvalidBuiltinData*>(data);
    if (!Begin(&d->Loc))
        return;

    char what[LineMax];
    if (d->Kind == BuiltinCtz || d->Kind == BuiltinClz)
        Stdlib::SnPrintf(what, sizeof(what), "passing zero to %s(), which is not a valid argument",
            d->Kind == BuiltinCtz ? "ctz" : "clz");
    else
        Stdlib::SnPrintf(what, sizeof(what), "an assumption the code made does not hold");
    Report(&d->Loc, what);
}

void __ubsan_handle_nonnull_return_v1(void*, void* location)
{
    auto* loc = static_cast<SourceLocation*>(location);
    if (!Begin(loc))
        return;

    Report(loc, "null pointer returned from a function declared never to return null");
}

void __ubsan_handle_nonnull_arg(void* data)
{
    auto* d = static_cast<NonNullArgData*>(data);
    if (!Begin(&d->Loc))
        return;

    char what[LineMax];
    Stdlib::SnPrintf(what, sizeof(what), "null pointer passed as argument %d, which is declared never to be null",
        (long)d->ArgIndex);
    Report(&d->Loc, what);
}

void __ubsan_handle_pointer_overflow(void* data, ulong base, ulong result)
{
    auto* d = static_cast<PointerOverflowData*>(data);
    if (!Begin(&d->Loc))
        return;

    char what[LineMax];
    if (base == 0 && result == 0)
        Stdlib::SnPrintf(what, sizeof(what), "applying zero offset to null pointer");
    else if (base == 0)
        Stdlib::SnPrintf(what, sizeof(what), "applying non-zero offset 0x%p to null pointer", result);
    else if (result == 0)
        Stdlib::SnPrintf(what, sizeof(what), "applying non-zero offset to non-null pointer 0x%p produced null pointer", base);
    else
        Stdlib::SnPrintf(what, sizeof(what), "pointer arithmetic on 0x%p overflowed to 0x%p", base, result);
    Report(&d->Loc, what);
}

}
