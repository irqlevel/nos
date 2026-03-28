#pragma once
 
#include <include/types.h>
#include "atomic.h"

#define ATEXIT_MAX_FUNCS	128
 
#ifdef __cplusplus
extern "C" {
#endif

/* guard variables */

/* The ABI requires a 64-bit type.  */
typedef Kernel::Atomic __guard;

static_assert(sizeof(__guard) == sizeof(u64), "invalid size");

int __cxa_guard_acquire(__guard *g);

void __cxa_guard_release(__guard *g);

void __cxa_guard_abort(__guard *);

typedef unsigned long uarch_t;
 
struct atexit_func_entry_t
{
	/*
	* Each member is at least 4 bytes large. Such that each entry is 12bytes.
	* 128 * 12 = 1.5KB exact.
	**/
	void (*destructor_func)(void *);
	void *obj_ptr;
	void *dso_handle;
};
 
int __cxa_atexit(void (*f)(void *), void *objptr, void *dso);
void __cxa_finalize(void *f);

void __cxa_pure_virtual(void);

#ifdef __cplusplus
};
#endif