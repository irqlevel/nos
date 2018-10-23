#pragma once

#include <lib/error.h>

namespace Kernel
{

namespace Test {

Stdlib::Error Test();

bool TestMultiTasking();

void TestStartSomeTasks();

void TestStopSomeTasks();

void TestPaging();

}

}
