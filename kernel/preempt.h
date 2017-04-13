#pragma once

#include "stdlib.h"

namespace Kernel
{

namespace Core
{

void PreemptOn();

void PreemptOff();

bool PreemptIsOn();

void PreemptDisable();

void PreemptEnable();

}
}