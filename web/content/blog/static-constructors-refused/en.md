---
title: "The constructor that never ran, and the link that now refuses one"
description: "nos runs no .init_array, so a global with a real constructor stays zeroes. Two locks went unwatched that way, until the link was made to refuse one."
date: 2026-09-19
tags: [safety, boot, debugging, testing]
---

In a hosted C++ program something runs before `main`: the C runtime walks `.init_array` and calls the constructor of every global that needs one. A kernel has no C runtime. nos is entered by GRUB over Multiboot2 or through the Linux `Image` protocol, goes from its assembly entry straight into C++ (`Main` on x86-64, `MainArm64` on arm64), and nothing on that path walks `.init_array`.

That is harmless as long as no global needs it. A global whose type has a `constexpr` constructor and a trivial destructor is constant-initialised: the compiler computes its bytes, and they are already in the image the bootloader loads. A global whose constructor is not a constant expression, because it is defined in another file, say, or does real work, is dynamically initialised: the compiler emits a small function that runs the constructor and puts a pointer to it in `.init_array`. In nos nobody calls that function. The object stays as the image left it, zeroes in `.bss`. No fault, no warning, no log line, and very often the zeroes are exactly what the constructor would have produced.

## Right by coincidence

The rule against it was not new. Since February 2026 the project's conventions had said "Static constructors are unreliable in freestanding" and prescribed a singleton built explicitly at boot instead. Nothing enforced it, and the kernel had already met the bug once. The counter each AP spins on until the BSP turns preemption on had been declared `Atomic PreemptOnWaiting(1);` since 2018 ([9bf8fcb](https://github.com/irqlevel/nos/commit/9bf8fcb)), with `Atomic`'s constructor defined out of line, so it started at 0 rather than 1, and the barrier never worked as written. The July 2026 review re-based that barrier on a counter that starts at zero anyway ([fc5b154](https://github.com/irqlevel/nos/commit/fc5b154)), with a comment saying why: "this kernel runs no .init_array, so a dynamically initialized `Atomic x(1)` would in fact be left at 0".

The rest were found by accident. On September 19 the C++ got a UBSan build ([e66e67c](https://github.com/irqlevel/nos/commit/e66e67c)), and its section list showed `.init_array` entries in six linked files on x86-64 and seven on arm64: `pci`, `msix` (`msix_its` on arm64), `interrupt_stats`, `sched`, `preempt`, `rust_ffi` and, on arm64, `pl011`. Most were right by coincidence. An `Atomic` or a `RawRwSpinLock` that is never constructed is all zeroes, and that is what their constructors would have made.

## Wrong in silence

Two were not. `Pci`'s config-space lock and `Pl011::OutLock`, the arm64 console's output lock, are `RawSpinLock`s, and a `RawSpinLock`'s constructor does real work: it registers the lock with the lock watchdog ([raw_spin_lock.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/raw_spin_lock.cpp)).

```cpp
RawSpinLock::RawSpinLock(bool watched)
    : Watched(watched)
    , PreemptTask(nullptr)
    , WatchdogReported(0)
    , WatchdogLockTime(0)
{
    if (Watched)
        Watchdog::GetInstance().RegisterSpinLock(*this);
}
```

Left as zeroes, `Watched` is `false`. Such a lock works perfectly well as a lock, since a `Value` of 0 means free, but it is on no watchdog list, and taking it records no time. The watchdog reports any watched lock held for more than 25 ms and panics at ten seconds, and it could see neither of these two. One is the lock behind every PCI config-space access, the Rust drivers' after SMP bring-up included; the other is the lock every line printed to the arm64 serial console goes through. Those are exactly the kind of locks that wedge a machine, and they were the two the watchdog was blind to.

The worst part is how they became wrong. Both globals were harmless the day they were written, in July 2026: `RawSpinLock`'s constructor was empty then, and zeroes were all it would have made. They broke on September 4, when the watchdog's instrumentation moved into that constructor after the EX44 deadlock ([that story](/blog/no-allocation-under-spinlock)). The change was in another file, and no review of `pci.cpp` or `pl011.cpp` could have seen it coming. A global with a dynamic initialiser is a bet that its type's constructor stays trivial forever.

## Three shapes that need no startup code

The fix ([6f83be8](https://github.com/irqlevel/nos/commit/6f83be8)) puts every global into a shape that needs no code at startup.

**A `constexpr` constructor and a trivial destructor**, for types whose construction is nothing but values. `Atomic` and `RawRwSpinLock` got them, and the compiler now initialises their static instances ([atomic.h](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/atomic.h)):

```cpp
    constexpr Atomic()
        : Value(0)
    {
    }

    constexpr Atomic(long value)
        : Value(value)
    {
    }
    // ...
    ~Atomic() = default;
```

**Every member initialised.** `RustIrqSlot` and `RustMsixSlot`, the tables the Rust side's interrupt handlers are registered in, had members with no initialiser. A default constructor that leaves a member indeterminate is not a constant expression, so the compiler made a runtime constructor of it, for a table that would have been zeroes either way:

```cpp
struct RustIrqSlot
{
    void (*Handler)(void*) = nullptr;
    void* Ctx = nullptr;
    u8 Vector = 0;
    bool Used = false;
    Kernel::Atomic InFlight; /* ISRs currently executing Handler */
};
```

**A function-local static**, for an object whose constructor has to do something. Both locks are built on first use now ([pci.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/drivers/pci.cpp), [pl011.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/arm64/pl011.cpp)):

```cpp
Kernel::RawSpinLock& ConfigLock()
{
    static Kernel::RawSpinLock lock;
    return lock;
}
```

The compiler guards that first use with `__cxa_guard_acquire`, which nos implements by disabling preemption and interrupts around an atomic bit ([icxxabi.cpp](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/icxxabi.cpp)), so two CPUs racing to the first PCI access build the lock once. The change was also read for what the new shape could break. Building the console lock prints nothing, since neither the watchdog's constructor nor `RegisterSpinLock` does, so the first line printed cannot recurse into the construction of its own lock; and `Panic` writes through a path that takes no lock at all. Afterwards `watchdog` on arm64 counted 816 locks watched where it had counted 814, at the same point of the same boot.

One shortcut was declined. `-fno-c++-static-destructors` would have emptied `.init_array` of destructor registrations with a single flag, but nos does run `__cxa_finalize` at shutdown, and the singletons' destructors check things there; `BlockAllocatorImpl`'s, for one, is a leak check. So the requirement got stricter instead: a global needs a trivial destructor as well as a `constexpr` constructor.

## The link that refuses one

None of that would have stayed fixed on its own. The rule had been written down since February, and both locks were written after it. So both linker scripts now collect every constructor and destructor section into an `INFO` section, one the image never loads, and assert that it is empty ([linker-arm64.ld](https://github.com/irqlevel/nos/blob/master/build/linker-arm64.ld)):

```diff
+    .static_init (INFO) : {
+        *(.preinit_array)
+        *(.init_array) *(.init_array.*)
+        *(.ctors) *(.ctors.*)
+        *(.fini_array) *(.fini_array.*)
+        *(.dtors) *(.dtors.*)
+    }
+    ASSERT(SIZEOF(.static_init) == 0, "a static constructor or destructor: this kernel runs none (CLAUDE.md)")
```

A global that needs code at startup is now a failed build, not a quiet zero. It was checked the direct way, by adding a global `RawSpinLock`: both links, GNU ld for x86-64 and ld.lld for arm64, fail with that message. The check sees every object file that reaches the image, whatever language it came from, and costs nothing at run time.

CLAUDE.md's rule changed with it, from "Static constructors are unreliable" to "Static constructors do not run, and the link refuses one" ([Build](/docs/build#four-things-the-makefile-will-not-tell-you)). The difference between those two sentences is the point of the exercise. The first is advice, which whoever writes the next global has to remember. The second is a property of the build, which remembers for them. When a bug is possible only because someone forgot a rule, this project looks for the place where forgetting it stops the build: a type, a gate, or, here, a few lines of linker script ([the goal](/about)).
