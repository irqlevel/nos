---
title: "Constructors that never ran, and the linker check that stops them"
description: "nos never runs .init_array, so a global with a real constructor stays all zeroes. Two locks slipped past the watchdog that way. Now the link refuses them."
date: 2026-09-19
tags: [safety, boot, debugging, testing]
---

In an ordinary C++ program, some code runs before `main`. The C runtime walks the `.init_array` section and calls the constructor of every global object that needs one. A kernel has no C runtime. nos is started by GRUB through Multiboot2 or through the Linux `Image` protocol. Its assembly entry point jumps straight into C++ (`Main` on x86-64, `MainArm64` on arm64), and nothing on that path walks `.init_array`.

That does no harm as long as no global needs it. A global whose type has a `constexpr` constructor and a trivial destructor is constant-initialised: the compiler computes its bytes, and they are already in the image the bootloader loads. A global whose constructor is not a constant expression is dynamically initialised. That happens when the constructor is defined in another file, say, or does real work. The compiler then emits a small function that runs the constructor and puts a pointer to it in `.init_array`.

In nos nobody calls that function. The object keeps what the image gave it, which is zeroes in `.bss`. There is no fault, no warning and no log line. And very often the zeroes are exactly what the constructor would have produced.

## Right by coincidence

The project already had a rule against this. Since February 2026 its conventions had said "Static constructors are unreliable in freestanding" and told you to use a singleton built explicitly at boot instead. Nothing enforced the rule, and the kernel had already been bitten once.

Each secondary CPU (an AP) spins on a counter until the boot CPU (the BSP) turns preemption on. Since 2018 that counter had been declared as `Atomic PreemptOnWaiting(1);` ([9bf8fcb](https://github.com/irqlevel/nos/commit/9bf8fcb)). `Atomic`'s constructor was defined out of line, so the counter started at 0 rather than 1, and the barrier never worked as written. The July 2026 review rebuilt that barrier on a counter that starts at zero anyway ([fc5b154](https://github.com/irqlevel/nos/commit/fc5b154)). It also added a comment saying why: "this kernel runs no .init_array, so a dynamically initialized `Atomic x(1)` would in fact be left at 0".

The other cases turned up by accident. On September 19 the C++ got a build with UBSan, clang's undefined-behaviour sanitizer ([e66e67c](https://github.com/irqlevel/nos/commit/e66e67c)). Its section list showed `.init_array` entries in six linked files on x86-64 and seven on arm64: `pci`, `msix` (`msix_its` on arm64), `interrupt_stats`, `sched`, `preempt`, `rust_ffi` and, on arm64 only, `pl011`. Most of them were right by coincidence. An `Atomic` or a `RawRwSpinLock` that is never constructed is all zeroes, and that is what their constructors would have made anyway.

## Silently wrong

Two of them were not. `Pci`'s config-space lock and `Pl011::OutLock`, the lock on the arm64 console's output, are both `RawSpinLock`s. A `RawSpinLock`'s constructor does real work: it registers the lock with the lock watchdog ([raw_spin_lock.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/raw_spin_lock.cpp)).

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

If the object is left as zeroes, `Watched` is `false`. The lock still works perfectly well as a lock, because a `Value` of 0 means free. But it is on no watchdog list, and taking it records no time. The watchdog reports any watched lock held for more than 25 ms and panics at ten seconds, and it could see neither of these two.

One of them guards every access to PCI config space, including the Rust drivers' accesses after SMP bring-up. The other is the lock that every line printed to the arm64 serial console goes through. Those are exactly the kind of locks that wedge a machine, and they were the two the watchdog was blind to.

The worst part is how they went wrong. Both globals were harmless when they were written, in July 2026. `RawSpinLock`'s constructor was empty then, so zeroes were all it would have produced. They broke on September 4, when the watchdog's instrumentation moved into that constructor after the deadlock on the EX44 ([that story](/blog/no-allocation-under-spinlock)). That change was in another file, so no review of `pci.cpp` or `pl011.cpp` could have seen it coming. A global with a dynamic initialiser is a bet that its type's constructor will stay trivial forever.

## Three forms that need no startup code

The fix ([6f83be8](https://github.com/irqlevel/nos/commit/6f83be8)) moves every global into a form that needs no code at startup. There are three such forms.

**A `constexpr` constructor and a trivial destructor.** This is for types whose construction only sets values. `Atomic` and `RawRwSpinLock` got both, and the compiler now initialises their static instances ([atomic.h](https://github.com/irqlevel/nos/blob/master/src/base/kernel/atomic.h)):

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

**Every member initialised.** `RustIrqSlot` and `RustMsixSlot` make up the tables where the Rust side's interrupt handlers are registered. Some of their members had no initialiser. A default constructor that leaves a member indeterminate is not a constant expression. So the compiler turned it into a constructor that runs at startup, for a table that would have been zeroes either way:

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

**A function-local static.** This is for an object whose constructor has real work to do. Both locks are now built the first time they are used ([pci.cpp](https://github.com/irqlevel/nos/blob/master/src/base/drivers/pci.cpp), [pl011.cpp](https://github.com/irqlevel/nos/blob/master/src/base/arch/arm64/pl011.cpp)):

```cpp
Kernel::RawSpinLock& ConfigLock()
{
    static Kernel::RawSpinLock lock;
    return lock;
}
```

The compiler guards that first use with `__cxa_guard_acquire`. nos implements it by disabling preemption and interrupts around an atomic bit ([icxxabi.cpp](https://github.com/irqlevel/nos/blob/master/src/base/kernel/icxxabi.cpp)). So if two CPUs race to the first PCI access, the lock is still built only once.

We also reviewed the change for what the new form could break. Building the console lock prints nothing, because neither the watchdog's constructor nor `RegisterSpinLock` prints. So the first line printed cannot recurse into building its own lock. And `Panic` writes through a path that takes no lock at all. After the fix, the `watchdog` shell command on arm64 counted 816 watched locks, where before it had counted 814 at the same point of the same boot.

We turned down one shortcut. The flag `-fno-c++-static-destructors` would have removed the destructor registrations from `.init_array` in one go. But nos does run `__cxa_finalize` at shutdown, and the singletons' destructors check things there. `BlockAllocatorImpl`'s destructor, for one, is a leak check. So the rule got stricter instead: a global needs a trivial destructor as well as a `constexpr` constructor.

## A link that refuses them

None of this would have stayed fixed on its own. The rule had been written down since February, and both locks were written after it. So both linker scripts now collect every constructor and destructor section into an `INFO` section, which is never loaded, and assert that it is empty ([linker-arm64.ld](https://github.com/irqlevel/nos/blob/master/build/linker-arm64.ld)):

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

Now a global that needs code at startup fails the build instead of quietly staying zero. We checked this directly by adding a global `RawSpinLock`. Both links failed with that message: GNU ld for x86-64 and ld.lld for arm64. The check sees every object file that goes into the image, whatever language it came from, and it costs nothing at run time.

The rule in CLAUDE.md, the file that holds the project's conventions, changed too. "Static constructors are unreliable" became "Static constructors do not run, and the link refuses one" ([Build](/docs/build#four-things-the-makefile-will-not-tell-you)). The difference between those two sentences is the whole point. The first is advice, and whoever writes the next global has to remember it. The second is a property of the build, and the build remembers for them. When a bug is possible only because someone forgot a rule, we look for a place where forgetting it stops the build. That can be a type, a test gate or, as here, a few lines of linker script ([the goal](/about)).
