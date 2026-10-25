#pragma once

#include "error.h"

namespace Kernel
{

namespace Test {

Stdlib::Error Test();

bool TestMultiTasking();

/* Task::Locate and Wait's report: once, from the BSP, with every CPU up */
bool TestTaskLocate();

/* Wakeups across every CPU, into every window of a task's run state, and
   none lost: once, from the BSP, with every CPU up */
bool TestWakeStorm();

/* The boot, run in the idle task, takes its turns beside a task that never
   blocks: from the BSP */
bool TestIdleTakesTurns();

/* EEVDF: a wakeup beside spinners runs at once, and spinners share the CPU
   by their weights -- on a CPU other than the BSP's, from the BSP */
bool TestEevdf();

}

}