#pragma once

#include "stdlib.h"

namespace Kernel
{

void PreemptOn();

void PreemptOff();

bool PreemptIsOn();

void PreemptDisable();

void PreemptEnable();

}