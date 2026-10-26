#pragma once

// This CPU's own timer, one-shot. Provides namespace Hal {
// local_timer_arm(delta_ns) }: fire the local timer's interrupt once, delta_ns
// from now -- the last arming wins, and an armed timer is moved. On this CPU,
// with interrupts off, once the arch has set the timer up one-shot (the
// local APIC's timer on x86, the generic timer's CNTV on arm64). What it is
// armed for is kernel::Cpu's (Cpu::LocalTimer*).
#if defined(__x86_64__)
#include <arch/x86_64/hal_timer_inline.h>
#elif defined(__aarch64__)
#include <arch/arm64/hal_timer_inline.h>
#else
#error "unsupported architecture"
#endif
