#pragma once

// Memory-ordering primitives, classified by what they order:
//
//   CompilerBarrier - compiler-only; never emits an instruction.
//   SmpWmb/SmpRmb   - CPU<->CPU publish/consume (e.g. seqlock).
//   SmpMb           - CPU<->CPU full fence.
//   DmaWmb/DmaRmb   - CPU<->device DMA descriptor/ring ordering
//                     (virtio rings, doorbells, OWN bits).
//
// On x86 (TSO) everything except SmpMb compiles to a compiler barrier, so
// classifying a site never changes x86 codegen. On arm64 these become dmb
// instructions, which is why call sites must pick the semantic variant.

namespace Hal
{

static inline __attribute__((always_inline)) void CompilerBarrier()
{
    __asm__ __volatile__("" : : : "memory");
}

#if defined(__x86_64__)

static inline __attribute__((always_inline)) void SmpWmb() { CompilerBarrier(); }
static inline __attribute__((always_inline)) void SmpRmb() { CompilerBarrier(); }
static inline __attribute__((always_inline)) void SmpMb() { __asm__ __volatile__("mfence" : : : "memory"); }
static inline __attribute__((always_inline)) void DmaWmb() { CompilerBarrier(); }
static inline __attribute__((always_inline)) void DmaRmb() { CompilerBarrier(); }

#elif defined(__aarch64__)

static inline __attribute__((always_inline)) void SmpWmb() { __asm__ __volatile__("dmb ishst" : : : "memory"); }
static inline __attribute__((always_inline)) void SmpRmb() { __asm__ __volatile__("dmb ishld" : : : "memory"); }
static inline __attribute__((always_inline)) void SmpMb() { __asm__ __volatile__("dmb ish" : : : "memory"); }
static inline __attribute__((always_inline)) void DmaWmb() { __asm__ __volatile__("dmb oshst" : : : "memory"); }
static inline __attribute__((always_inline)) void DmaRmb() { __asm__ __volatile__("dmb oshld" : : : "memory"); }

#else
#error "unsupported architecture"
#endif

}
