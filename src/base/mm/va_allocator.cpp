#include "va_allocator.h"
#include "page_table.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/bitmap.h>

namespace kernel
{

namespace mm
{

VaAllocator::VaAllocator()
    : bitmap_ptr_(nullptr)
    , bitmap_size_(0)
    , va_start_(0)
    , va_end_(0)
    , block_size_(0)
    , block_count_(0)
{
}

VaAllocator::~VaAllocator()
{
    trace(0, "0x%p dtor", this);
}

bool VaAllocator::setup(ulong start, ulong end, ulong block_size)
{
    block_count_ = (end - start) / block_size;
    if (!block_count_)
        return false;

    va_start_ = start;
    va_end_ = end;
    block_size_ = block_size;
    bitmap_size_ = (block_count_ + 7) / 8;

    ulong bitmap_pages = stdlib::round_up(bitmap_size_, consts::PAGE_SIZE) / consts::PAGE_SIZE;

    auto& pt = PageTable::get_instance();
    for (ulong i = 0; i < bitmap_pages; i++)
    {
        Page* page = pt.alloc_page();
        if (!page)
        {
            for (ulong j = 0; j < i; j++)
            {
                Page* p = pt.unmap_page(start + j * consts::PAGE_SIZE);
                pt.free_page(p);
                p->put();
            }
            return false;
        }
        if (!pt.map_page(start + i * consts::PAGE_SIZE, page))
        {
            pt.free_page(page);
            for (ulong j = 0; j < i; j++)
            {
                Page* p = pt.unmap_page(start + j * consts::PAGE_SIZE);
                pt.free_page(p);
                p->put();
            }
            return false;
        }
    }

    bitmap_ptr_ = (u8*)start;
    stdlib::mem_set(bitmap_ptr_, 0, bitmap_pages * consts::PAGE_SIZE);

    /* Reserve the blocks occupied by the bitmap itself */
    ulong bitmap_blocks = stdlib::round_up(bitmap_pages * consts::PAGE_SIZE, block_size) / block_size;
    for (ulong i = 0; i < bitmap_blocks; i++)
        stdlib::Bitmap(bitmap_ptr_, block_count_).set_bit(i);

    trace(0, "0x%p start 0x%lX end 0x%lX bsize 0x%lX bcount %lu bmpages %lu",
        this, va_start_, va_end_, block_size_, block_count_, bitmap_pages);
    return true;
}

ulong VaAllocator::alloc()
{
    stdlib::AutoLock lock(lock_);

    long block_index = stdlib::Bitmap(bitmap_ptr_, block_count_).find_set_zero_bit();
    if (block_index < 0)
        return 0;

    return va_start_ + block_index * block_size_;
}

void VaAllocator::free(ulong va)
{
    stdlib::AutoLock lock(lock_);

    BUG_ON(va < va_start_ || va >= va_end_);
    BUG_ON((va - va_start_) % block_size_ != 0);

    ulong block_index = (va - va_start_) / block_size_;
    stdlib::Bitmap bitmap(bitmap_ptr_, block_count_);
    /* Double-free: the same VA would be handed out to two callers */
    BUG_ON(!bitmap.test_bit(block_index));
    bitmap.clear_bit(block_index);
}

bool VaAllocator::contains(ulong va)
{
    return va >= va_start_ && va < va_end_ && ((va - va_start_) % block_size_) == 0;
}

}
}
