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

namespace kernel
{

namespace
{

/* What the compiler describes a site with: the layouts of compiler-rt's
   ubsan_handlers.h, clang's side of the ABI. */
struct SourceLocation
{
    const char* file;
    u32 line;
    u32 column;
};

/* Followed in memory by the type's name, NUL-terminated. */
struct TypeDescriptor
{
    u16 kind;
    u16 info;
};

struct TypeMismatchData
{
    SourceLocation loc;
    const TypeDescriptor* type;
    u8 log_alignment;
    u8 check_kind;
};

struct AlignmentAssumptionData
{
    SourceLocation loc;
    SourceLocation assumption_loc;
    const TypeDescriptor* type;
};

struct OverflowData
{
    SourceLocation loc;
    const TypeDescriptor* type;
};

struct ShiftOutOfBoundsData
{
    SourceLocation loc;
    const TypeDescriptor* lhs_type;
    const TypeDescriptor* rhs_type;
};

struct OutOfBoundsData
{
    SourceLocation loc;
    const TypeDescriptor* array_type;
    const TypeDescriptor* index_type;
};

struct UnreachableData
{
    SourceLocation loc;
};

struct VlaBoundData
{
    SourceLocation loc;
    const TypeDescriptor* type;
};

struct InvalidValueData
{
    SourceLocation loc;
    const TypeDescriptor* type;
};

struct InvalidBuiltinData
{
    SourceLocation loc;
    u8 kind;
};

struct NonNullReturnData
{
    SourceLocation attr_loc;
};

struct NonNullArgData
{
    SourceLocation loc;
    SourceLocation attr_loc;
    int arg_index;
};

struct PointerOverflowData
{
    SourceLocation loc;
};

struct FloatCastOverflowData
{
    SourceLocation loc;
    const TypeDescriptor* from_type;
    const TypeDescriptor* to_type;
};

const u16 TYPE_KIND_INTEGER = 0;
const u16 TYPE_INFO_SIGNED = 1;
const u32 REPORTED_BIT = 0x80000000;
const unsigned MAX_INLINE_BITS = 64;
const size_t LINE_LEN_MAX = 256;
const size_t VALUE_MAX = 32;
const size_t BACKTRACE_FRAMES = 16;

/* TypeMismatchData::check_kind, in clang's order. */
const char* const CHECK_KIND_NAMES[] = {
    "load of", "store to", "reference binding to", "member access within",
    "member call on", "constructor call on", "downcast of", "downcast of",
    "upcast of", "cast to virtual base of", "_Nonnull binding to",
    "dynamic operation on",
};

/* InvalidBuiltinData::kind */
const u8 BUILTIN_CTZ = 0;
const u8 BUILTIN_CLZ = 1;

/* Constant-initialised, so nothing here needs a constructor to have run. */
int busy;
bool warn_only;
unsigned long report_count;
unsigned long unprinted_count;

const char* type_name(const TypeDescriptor* type)
{
    return reinterpret_cast<const char*>(type) + sizeof(TypeDescriptor);
}

bool is_integer(const TypeDescriptor* type)
{
    return type->kind == TYPE_KIND_INTEGER;
}

bool is_signed(const TypeDescriptor* type)
{
    return (type->info & TYPE_INFO_SIGNED) != 0;
}

unsigned bit_width(const TypeDescriptor* type)
{
    return 1u << (type->info >> 1);
}

/* An integer of up to 64 bits arrives in the handle itself; a wider one as a
   pointer to it, which is only ever printed as a width. */
bool is_inline(const TypeDescriptor* type)
{
    return is_integer(type) && bit_width(type) <= MAX_INLINE_BITS;
}

long signed_value(const TypeDescriptor* type, ulong value)
{
    unsigned width = bit_width(type);
    if (width >= MAX_INLINE_BITS)
        return (long)value;

    unsigned shift = MAX_INLINE_BITS - width;
    return (long)(value << shift) >> shift;
}

bool is_negative(const TypeDescriptor* type, ulong value)
{
    return is_inline(type) && is_signed(type) && signed_value(type, value) < 0;
}

void format_value(char* buf, size_t size, const TypeDescriptor* type, ulong value)
{
    if (!is_integer(type))
    {
        stdlib::sn_printf(buf, size, "<a value of type %s>", type_name(type));
        return;
    }
    if (!is_inline(type))
    {
        stdlib::sn_printf(buf, size, "<a %lu-bit value>", (ulong)bit_width(type));
        return;
    }
    if (is_signed(type))
    {
        stdlib::sn_printf(buf, size, "%ld", signed_value(type, value));
        return;
    }

    unsigned width = bit_width(type);
    ulong masked = (width >= MAX_INLINE_BITS) ? value : (value & ((1UL << width) - 1));
    stdlib::sn_printf(buf, size, "%lu", masked);
}

void emit(const char* text)
{
    hal::console_panic_write(text);
    rust_disklog_log(text);
}

/* Whether this failure is to be printed: nobody else is reporting, and its
   site has not reported yet. On true the caller holds Busy. */
bool begin(SourceLocation* loc)
{
    if (__atomic_exchange_n(&busy, 1, __ATOMIC_ACQUIRE) != 0)
    {
        __atomic_add_fetch(&unprinted_count, 1, __ATOMIC_RELAXED);
        return false;
    }

    u32 before = __atomic_fetch_or(&loc->column, REPORTED_BIT, __ATOMIC_RELAXED);
    if (before & REPORTED_BIT)
    {
        __atomic_store_n(&busy, 0, __ATOMIC_RELEASE);
        return false;
    }
    return true;
}

/* Print the report and, unless ubsan=warn, panic with it. Called holding
   Busy, which a warning gives back. */
void report(const SourceLocation* loc, const char* what)
{
    char line[LINE_LEN_MAX];
    ulong count = __atomic_add_fetch(&report_count, 1, __ATOMIC_RELAXED);
    stdlib::sn_printf(line, sizeof(line), "UBSAN: %s at %s:%lu:%lu",
        what, loc->file, (ulong)loc->line, (ulong)(loc->column & ~REPORTED_BIT));

    if (!__atomic_load_n(&warn_only, __ATOMIC_RELAXED))
        PANIC("%s", line);

    ulong flags = hal::irq_save();

    emit(line);
    emit("\n");

    ulong frames[BACKTRACE_FRAMES];
    size_t frame_count = StackTrace::capture(frames, stdlib::array_size(frames));
    char where[SymbolTable::DESCRIBE_MAX];
    char buf[SymbolTable::DESCRIBE_MAX + VALUE_MAX];
    for (size_t i = 0; i < frame_count; i++)
    {
        if (SymbolTable::get_instance().describe_return(frames[i], where, sizeof(where)))
            stdlib::sn_printf(buf, sizeof(buf), "  [%lu] 0x%lX %s\n", (ulong)i, frames[i], where);
        else
            stdlib::sn_printf(buf, sizeof(buf), "  [%lu] 0x%lX\n", (ulong)i, frames[i]);
        emit(buf);
    }

    stdlib::sn_printf(buf, sizeof(buf), "UBSAN: %lu report(s) so far\n", count);
    emit(buf);

    hal::irq_restore(flags);
    __atomic_store_n(&busy, 0, __ATOMIC_RELEASE);
}

/* The two checks there is no going on from. */
[[noreturn]] void fatal(SourceLocation* loc, const char* what)
{
    if (begin(loc))
        report(loc, what);

    PANIC("UBSAN: %s at %s:%lu", what, loc->file, (ulong)loc->line);
    __builtin_unreachable();
}

void overflow(void* data, ulong lhs, ulong rhs, char op)
{
    auto* d = static_cast<OverflowData*>(data);
    if (!begin(&d->loc))
        return;

    char left[VALUE_MAX], right[VALUE_MAX], what[LINE_LEN_MAX];
    format_value(left, sizeof(left), d->type, lhs);
    format_value(right, sizeof(right), d->type, rhs);
    stdlib::sn_printf(what, sizeof(what), "%s integer overflow: %s %c %s cannot be represented in type %s",
        is_signed(d->type) ? "signed" : "unsigned", left, op, right, type_name(d->type));
    report(&d->loc, what);
}

}

namespace ubsan
{

void set_warn_only(bool on)
{
    __atomic_store_n(&warn_only, on, __ATOMIC_RELAXED);
}

bool enabled()
{
#ifdef NOS_UBSAN
    return true;
#else
    return false;
#endif
}

void announce()
{
    if (!enabled())
        return;

    trace(0, "UBSan: on -- %s", __atomic_load_n(&warn_only, __ATOMIC_RELAXED)
        ? "ubsan=warn: each site reports once, to the serial port and the disk log, and the kernel goes on"
        : "the first report is a panic");
}

unsigned long reports()
{
    return __atomic_load_n(&report_count, __ATOMIC_RELAXED);
}

unsigned long unprinted()
{
    return __atomic_load_n(&unprinted_count, __ATOMIC_RELAXED);
}

}

}

using namespace kernel;

/* The handlers clang calls. Recoverable ones only: the build never asks for
   -fno-sanitize-recover, so no _abort variant is referenced, and a build
   that did would fail to link rather than go without. */
extern "C"
{

void __ubsan_handle_type_mismatch_v1(void* data, ulong pointer)
{
    auto* d = static_cast<TypeMismatchData*>(data);
    if (!begin(&d->loc))
        return;

    const char* check = (d->check_kind < stdlib::array_size(CHECK_KIND_NAMES))
        ? CHECK_KIND_NAMES[d->check_kind] : "access to";
    ulong alignment = 1UL << d->log_alignment;
    char what[LINE_LEN_MAX];

    if (pointer == 0)
        stdlib::sn_printf(what, sizeof(what), "%s null pointer of type %s", check, type_name(d->type));
    else if (pointer & (alignment - 1))
        stdlib::sn_printf(what, sizeof(what), "%s misaligned address 0x%lX for type %s, which requires %lu byte alignment",
            check, pointer, type_name(d->type), alignment);
    else
        stdlib::sn_printf(what, sizeof(what), "%s address 0x%lX with insufficient space for an object of type %s",
            check, pointer, type_name(d->type));
    report(&d->loc, what);
}

void __ubsan_handle_alignment_assumption(void* data, ulong pointer, ulong alignment, ulong offset)
{
    auto* d = static_cast<AlignmentAssumptionData*>(data);
    if (!begin(&d->loc))
        return;

    char what[LINE_LEN_MAX];
    stdlib::sn_printf(what, sizeof(what), "assumption of %lu byte alignment (offset %lu) for pointer 0x%lX of type %s failed",
        alignment, offset, pointer, type_name(d->type));
    report(&d->loc, what);
}

void __ubsan_handle_add_overflow(void* data, ulong lhs, ulong rhs)
{
    overflow(data, lhs, rhs, '+');
}

void __ubsan_handle_sub_overflow(void* data, ulong lhs, ulong rhs)
{
    overflow(data, lhs, rhs, '-');
}

void __ubsan_handle_mul_overflow(void* data, ulong lhs, ulong rhs)
{
    overflow(data, lhs, rhs, '*');
}

void __ubsan_handle_negate_overflow(void* data, ulong old)
{
    auto* d = static_cast<OverflowData*>(data);
    if (!begin(&d->loc))
        return;

    char value[VALUE_MAX], what[LINE_LEN_MAX];
    format_value(value, sizeof(value), d->type, old);
    stdlib::sn_printf(what, sizeof(what), "negation of %s cannot be represented in type %s", value, type_name(d->type));
    report(&d->loc, what);
}

void __ubsan_handle_divrem_overflow(void* data, ulong lhs, ulong rhs)
{
    auto* d = static_cast<OverflowData*>(data);
    if (!begin(&d->loc))
        return;

    char left[VALUE_MAX], what[LINE_LEN_MAX];
    format_value(left, sizeof(left), d->type, lhs);
    if (is_inline(d->type) && (is_signed(d->type) ? signed_value(d->type, rhs) == 0 : rhs == 0))
        stdlib::sn_printf(what, sizeof(what), "division of %s by zero", left);
    else
        stdlib::sn_printf(what, sizeof(what), "division of %s by -1 cannot be represented in type %s",
            left, type_name(d->type));
    report(&d->loc, what);
}

void __ubsan_handle_shift_out_of_bounds(void* data, ulong lhs, ulong rhs)
{
    auto* d = static_cast<ShiftOutOfBoundsData*>(data);
    if (!begin(&d->loc))
        return;

    char left[VALUE_MAX], right[VALUE_MAX], what[LINE_LEN_MAX];
    format_value(left, sizeof(left), d->lhs_type, lhs);
    format_value(right, sizeof(right), d->rhs_type, rhs);

    if (is_negative(d->rhs_type, rhs))
        stdlib::sn_printf(what, sizeof(what), "shift exponent %s is negative", right);
    else if (is_inline(d->rhs_type) && rhs >= bit_width(d->lhs_type))
        stdlib::sn_printf(what, sizeof(what), "shift exponent %s is too large for %lu-bit type %s",
            right, (ulong)bit_width(d->lhs_type), type_name(d->lhs_type));
    else if (is_negative(d->lhs_type, lhs))
        stdlib::sn_printf(what, sizeof(what), "left shift of negative value %s", left);
    else
        stdlib::sn_printf(what, sizeof(what), "left shift of %s by %s places cannot be represented in type %s",
            left, right, type_name(d->lhs_type));
    report(&d->loc, what);
}

void __ubsan_handle_out_of_bounds(void* data, ulong index)
{
    auto* d = static_cast<OutOfBoundsData*>(data);
    if (!begin(&d->loc))
        return;

    char value[VALUE_MAX], what[LINE_LEN_MAX];
    format_value(value, sizeof(value), d->index_type, index);
    stdlib::sn_printf(what, sizeof(what), "index %s out of bounds for type %s", value, type_name(d->array_type));
    report(&d->loc, what);
}

void __ubsan_handle_builtin_unreachable(void* data)
{
    fatal(&static_cast<UnreachableData*>(data)->loc, "execution reached an unreachable program point");
}

void __ubsan_handle_missing_return(void* data)
{
    fatal(&static_cast<UnreachableData*>(data)->loc,
        "execution reached the end of a value-returning function without returning a value");
}

void __ubsan_handle_vla_bound_not_positive(void* data, ulong bound)
{
    auto* d = static_cast<VlaBoundData*>(data);
    if (!begin(&d->loc))
        return;

    char value[VALUE_MAX], what[LINE_LEN_MAX];
    format_value(value, sizeof(value), d->type, bound);
    stdlib::sn_printf(what, sizeof(what), "variable length array bound evaluates to non-positive value %s", value);
    report(&d->loc, what);
}

void __ubsan_handle_float_cast_overflow(void* data, ulong)
{
    auto* d = static_cast<FloatCastOverflowData*>(data);
    if (!begin(&d->loc))
        return;

    char what[LINE_LEN_MAX];
    stdlib::sn_printf(what, sizeof(what), "a value of type %s is outside the range of type %s",
        type_name(d->from_type), type_name(d->to_type));
    report(&d->loc, what);
}

void __ubsan_handle_load_invalid_value(void* data, ulong value)
{
    auto* d = static_cast<InvalidValueData*>(data);
    if (!begin(&d->loc))
        return;

    char text[VALUE_MAX], what[LINE_LEN_MAX];
    format_value(text, sizeof(text), d->type, value);
    stdlib::sn_printf(what, sizeof(what), "load of value %s, which is not a valid value for type %s",
        text, type_name(d->type));
    report(&d->loc, what);
}

void __ubsan_handle_invalid_builtin(void* data)
{
    auto* d = static_cast<InvalidBuiltinData*>(data);
    if (!begin(&d->loc))
        return;

    char what[LINE_LEN_MAX];
    if (d->kind == BUILTIN_CTZ || d->kind == BUILTIN_CLZ)
        stdlib::sn_printf(what, sizeof(what), "passing zero to %s(), which is not a valid argument",
            d->kind == BUILTIN_CTZ ? "ctz" : "clz");
    else
        stdlib::sn_printf(what, sizeof(what), "an assumption the code made does not hold");
    report(&d->loc, what);
}

void __ubsan_handle_nonnull_return_v1(void*, void* location)
{
    auto* loc = static_cast<SourceLocation*>(location);
    if (!begin(loc))
        return;

    report(loc, "null pointer returned from a function declared never to return null");
}

void __ubsan_handle_nonnull_arg(void* data)
{
    auto* d = static_cast<NonNullArgData*>(data);
    if (!begin(&d->loc))
        return;

    char what[LINE_LEN_MAX];
    stdlib::sn_printf(what, sizeof(what), "null pointer passed as argument %ld, which is declared never to be null",
        (long)d->arg_index);
    report(&d->loc, what);
}

void __ubsan_handle_pointer_overflow(void* data, ulong base, ulong result)
{
    auto* d = static_cast<PointerOverflowData*>(data);
    if (!begin(&d->loc))
        return;

    char what[LINE_LEN_MAX];
    if (base == 0 && result == 0)
        stdlib::sn_printf(what, sizeof(what), "applying zero offset to null pointer");
    else if (base == 0)
        stdlib::sn_printf(what, sizeof(what), "applying non-zero offset 0x%lX to null pointer", result);
    else if (result == 0)
        stdlib::sn_printf(what, sizeof(what), "applying non-zero offset to non-null pointer 0x%lX produced null pointer", base);
    else
        stdlib::sn_printf(what, sizeof(what), "pointer arithmetic on 0x%lX overflowed to 0x%lX", base, result);
    report(&d->loc, what);
}

}
