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

}

}