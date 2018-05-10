#pragma once

#include <lib/stdlib.h>

namespace Kernel
{

void PreemptOn();

void PreemptOnWait();

void PreemptOff();

bool PreemptIsOn();

void PreemptDisable();

void PreemptEnable();

}
