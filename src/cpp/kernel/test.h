#pragma once

#include "error.h"

namespace Kernel
{

namespace Test {

Stdlib::Error Test();

bool TestMultiTasking();

/* Task::Locate and Wait's report: once, from the BSP, with every CPU up */
bool TestTaskLocate();

}

}