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

/* The boot, run in the idle task, takes its turns beside a task that never
   blocks: from the BSP */
bool test_idle_takes_turns();

/* EEVDF: a wakeup beside spinners runs at once, and spinners share the CPU
   by their weights -- on a CPU other than the BSP's, from the BSP */
bool test_eevdf();

/* The management bus's module side: modtest's provider called, a job of it
   stopped and waited out by the module's unload. Needs tasks: run after
   rust_test. */
bool test_module_bus();

}

}