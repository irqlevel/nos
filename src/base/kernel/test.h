#pragma once

#include "error.h"

namespace kernel
{

namespace test {

stdlib::Error test();

bool test_multi_tasking();

/* Task::locate and Wait's report: once, from the BSP, with every CPU up */
bool test_task_locate();

/* Wakeups across every CPU, into every window of a task's run state, and
   none lost: once, from the BSP, with every CPU up */
bool test_wake_storm();

/* A wait()'s task let go of only once no signal() of its event is still at
   it: short-lived waiters freed as they return, under signal()s that hold
   what they found -- once, from the BSP, with every CPU up */
bool test_event_let_go();

/* The boot, run in the idle task, takes its turns beside a task that never
   blocks: from the BSP */
bool test_idle_takes_turns();

/* EEVDF: a wakeup beside spinners runs at once, and spinners share the CPU
   by their weights -- on a CPU other than the BSP's, from the BSP */
bool test_eevdf();

/* A task's affinity changed under it (set_task_affinity): a spinning task
   and a sleeping one moved off the CPU they were on, and kept off it; an
   idle task, a task bound to its CPU and a mask of no CPU that runs
   refused. From the BSP, with every CPU up. */
bool test_affinity();

/* The management bus's module side: modtest's provider called, a job of it
   stopped and waited out by the module's unload. Needs tasks: run after
   rust_test. */
bool test_module_bus();

/* The message channel's module side (src/crates/ipc): modtest talking to
   kernel.echo, then unloaded under the echo's traffic -- its mailbox closed
   and the echo's monitor told, the one it left open closed by the sweep
   after its exit. Needs tasks: run after rust_test. */
bool test_module_ipc();

}

}