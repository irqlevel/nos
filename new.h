#pragma once

#include "allocator.h"

void* operator new(unsigned int size) noexcept;
void* operator new[](unsigned int size) noexcept;

void* operator new(unsigned int size, Shared::Allocator& allocator) noexcept;
void* operator new[](unsigned int size, Shared::Allocator& allocator) noexcept;

void operator delete(void* ptr) noexcept;
void operator delete[](void* ptr) noexcept;
