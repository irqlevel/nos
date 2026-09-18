#pragma once

#include <include/types.h>

/* The block layer is Rust (src/rust/block): the table every disk and every
   partition of one is registered in, the claims that keep two writers apart,
   and the MBR and GPT readers that put the partitions there.

   There is no C++ view of a device any more. A device is the handle the
   table knows it by -- 0 names none -- and these are the calls the C++ that
   is left makes on one: the shell, the disk log, and mounting a filesystem.
   A driver registers with the table from the Rust side (kcore::block);
   nothing in C++ does. */

extern "C" {

/* The handle of the device called name (nameLen bytes, no NUL needed), or 0
   when the table has no such device. */
ulong kernel_blockdev_find(const u8* name, ulong nameLen);

/* Walking the table: how many devices it holds, and the index'th one. The
   table only grows, so an index once valid stays valid and names the same
   device; a slot a registration has not finished reads as 0. */
u32 kernel_blockdev_count();
ulong kernel_blockdev_at(u32 index);

/* NUL-terminated and the driver's to keep, so it outlives the kernel's use
   of it. Null for a handle that names nothing. */
const char* kernel_blockdev_name_ptr(ulong handle);

u64 kernel_blockdev_capacity(ulong handle);     /* sectors */
u64 kernel_blockdev_sector_size(ulong handle);  /* bytes */

/* Synchronous I/O, count in sectors: 0 once it is done, the driver's error
   otherwise. The device may DMA in and out of buf, so buf has to be memory
   the page allocator tracks -- a static array has no physical address the
   driver can find, and a read into one comes back reporting success with the
   buffer untouched. */
int kernel_blockdev_read(ulong handle, u64 sector, void* buf, u32 count);
int kernel_blockdev_write(ulong handle, u64 sector, const void* buf, u32 count, int fua);

/* Exclusive users of a device: a mounted filesystem, the disk log, code
   writing to it around both. A claim is refused while another overlaps it --
   the same device, the disk a partition is on, or a partition of that disk
   -- and *heldBy then names the holder. holder has to outlive the claim.
   Reads need no claim. Answers with what Release takes, 0 when refused. */
ulong kernel_blockdev_claim_as(ulong handle, const char* holder, const char** heldBy);
void kernel_blockdev_release(ulong claim);

/* Set once interrupts and the scheduler are running. Before that a
   synchronous I/O has to poll its device: there is nothing yet to wake a
   waiter. */
void kernel_blockdev_set_interrupts_started();

}
