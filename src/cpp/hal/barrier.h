#pragma once

// Memory-ordering primitives, classified by what they order:
//
//   CompilerBarrier - compiler-only; never emits an instruction.
//   SmpWmb/SmpRmb   - CPU<->CPU publish/consume (e.g. seqlock).
//   SmpMb           - CPU<->CPU full fence.
//   DmaWmb/DmaRmb   - CPU<->device DMA descriptor/ring ordering
//                     (virtio rings, doorbells, OWN bits).
//   WcFlush         - drain buffered write-combining stores so what was
//                     written to WC memory (a framebuffer) actually reaches
//                     the bus. The variants above do NOT do this: on x86
//                     they are compiler barriers, while WC stores sit in the
//                     fill buffers until a fence or an eviction.
//
// On x86 (TSO) everything except SmpMb compiles to a compiler barrier, so
// classifying a site never changes x86 codegen. On arm64 these become dmb
// instructions, which is why call sites must pick the semantic variant.

namespace hal
{

static inline __attribute__((always_inline)) void compiler_barrier()
{
    __asm__ __volatile__("" : : : "memory");
}

#if defined(__x86_64__)

static inline __attribute__((always_inline)) void smp_wmb() { compiler_barrier(); }
static inline __attribute__((always_inline)) void smp_rmb() { compiler_barrier(); }
static inline __attribute__((always_inline)) void smp_mb() { __asm__ __volatile__("mfence" : : : "memory"); }
static inline __attribute__((always_inline)) void dma_wmb() { compiler_barrier(); }
static inline __attribute__((always_inline)) void dma_rmb() { compiler_barrier(); }
static inline __attribute__((always_inline)) void wc_flush() { __asm__ __volatile__("sfence" : : : "memory"); }

#elif defined(__aarch64__)

static inline __attribute__((always_inline)) void smp_wmb() { __asm__ __volatile__("dmb ishst" : : : "memory"); }
static inline __attribute__((always_inline)) void smp_rmb() { __asm__ __volatile__("dmb ishld" : : : "memory"); }
static inline __attribute__((always_inline)) void smp_mb() { __asm__ __volatile__("dmb ish" : : : "memory"); }
static inline __attribute__((always_inline)) void dma_wmb() { __asm__ __volatile__("dmb oshst" : : : "memory"); }
static inline __attribute__((always_inline)) void dma_rmb() { __asm__ __volatile__("dmb oshld" : : : "memory"); }
static inline __attribute__((always_inline)) void wc_flush() { __asm__ __volatile__("dsb oshst" : : : "memory"); }

#else
#error "unsupported architecture"
#endif

}
