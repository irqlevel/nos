#include "trace.h"
#include "debug.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "stack_trace.h"
#include "asm.h"
#include <block/block_device.h>

#include <lib/btree.h>
#include <lib/error.h>
#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/vector.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <mm/new.h>

namespace Kernel
{

namespace Test
{

static const ulong Tag = 'Test';

Stdlib::Error TestBtree()
{
    Stdlib::Error err;

    Trace(TestLL, "TestBtree: started");

    size_t keyCount = 431;

    Stdlib::Vector<size_t> pos;
    if (!pos.ReserveAndUse(keyCount))
        return MakeError(Stdlib::Error::NoMemory);

    for (size_t i = 0; i < keyCount; i++)
    {
        pos[i] = i;
    }

    Stdlib::Vector<u32> key;
    if (!key.ReserveAndUse(keyCount))
        return MakeError(Stdlib::Error::NoMemory);
    for (size_t i = 0; i < keyCount; i++)
        key[i] = i;

    Stdlib::Vector<u32> value;
    if (!value.ReserveAndUse(keyCount))
        return MakeError(Stdlib::Error::NoMemory);

    for (size_t i = 0; i < keyCount; i++)
        value[i] = i;

    Stdlib::Btree<u32, u32, 4> tree;

    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(TestLL, "TestBtree: cant insert key %llu", key[pos[i]]);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(TestLL, "TestBtree: cant find key");
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(TestLL, "TestBtree: unexpected found value");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount / 2; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(TestLL, "TestBtree: cant delete key[%lu][%lu]=%llu", i, pos[i], key[pos[i]]);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(TestLL, "TestBtree: cant find key");
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(TestLL, "TestBtree: unexpected found value");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(TestLL, "TestBtree: cant delete key");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        tree.Lookup(key[pos[i]], exist);
        if (exist)
        {
            Trace(TestLL, "TestBtree: key still exist");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(TestLL, "TestBtree: can't insert key'");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    Trace(TestLL, "TestBtree: min depth %d max depth %d", tree.MinDepth(), tree.MaxDepth());

    tree.Clear();
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    Trace(TestLL, "TestBtree: complete");

    return MakeError(Stdlib::Error::Success);
}

Stdlib::Error TestAllocator()
{
    Stdlib::Error err;

    for (size_t size = 1; size <= 8 * Const::PageSize; size++)
    {
        u8 *block = new u8[size];
        if (block == nullptr)
        {
            return Stdlib::Error::NoMemory;
        }

        block[0] = 1;
        block[size / 2] = 1;
        block[size - 1] = 1;
        delete [] block;
    }

    return MakeError(Stdlib::Error::Success);
}

Stdlib::Error TestRingBuffer()
{
    Stdlib::RingBuffer<u8, 3> rb;

    if (!rb.Put(0x1))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (!rb.Put(0x2))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (!rb.Put(0x3))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (rb.Put(0x4))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (!rb.IsFull())
        return MakeError(Stdlib::Error::Unsuccessful);

    if (rb.IsEmpty())
        return MakeError(Stdlib::Error::Unsuccessful);

    if (rb.Get() != 0x1)
        return MakeError(Stdlib::Error::Unsuccessful);

    if (rb.Get() != 0x2)
        return MakeError(Stdlib::Error::Unsuccessful);

    if (rb.Get() != 0x3)
        return MakeError(Stdlib::Error::Unsuccessful);

    if (!rb.IsEmpty())
        return MakeError(Stdlib::Error::Unsuccessful);
   
    return MakeError(Stdlib::Error::Success);
}

/* noinline helpers to create a known call-depth difference */
static __attribute__((noinline)) size_t CaptureAtDepth0()
{
    ulong frames[20];
    return StackTrace::Capture(frames, Stdlib::ArraySize(frames));
}

static __attribute__((noinline)) size_t CaptureAtDepth1()
{
    return CaptureAtDepth0();
}

static __attribute__((noinline)) size_t CaptureAtDepth2()
{
    return CaptureAtDepth1();
}

static __attribute__((noinline)) size_t CaptureAtDepth3()
{
    return CaptureAtDepth2();
}

Stdlib::Error TestStackTrace()
{
    Trace(0, "TestStackTrace: started");

    ulong frames[20];
    size_t count;

    /* Auto-detect: must capture at least 1 frame */
    count = StackTrace::Capture(frames, Stdlib::ArraySize(frames));
    if (count == 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* All return addresses must be in kernel space */
    for (size_t i = 0; i < count; i++)
    {
        if (frames[i] < 0xFFFF800001000000UL)
            return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* maxFrames = 0 must return 0 */
    count = StackTrace::Capture(frames, 0);
    if (count != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* maxFrames = 1 must return exactly 1 */
    count = StackTrace::Capture(frames, 1);
    if (count != 1)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Explicit bounds: empty range (base == limit) must return 0 */
    count = StackTrace::Capture(0, 0, frames, Stdlib::ArraySize(frames));
    if (count != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Explicit bounds: range not containing our stack must return 0 */
    count = StackTrace::Capture(0x1000, 0x2000, frames, Stdlib::ArraySize(frames));
    if (count != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Explicit bounds with correct task stack range */
    ulong rsp = GetRsp();
    ulong base = rsp & (~(Task::StackSize - 1));
    Task::Stack* stackPtr = reinterpret_cast<Task::Stack*>(base);
    if (stackPtr->Magic1 == Task::StackMagic1 &&
        stackPtr->Magic2 == Task::StackMagic2)
    {
        count = StackTrace::Capture(base, base + Task::StackSize,
                                    frames, Stdlib::ArraySize(frames));
        if (count == 0)
            return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Deeper nesting must capture more frames:
       CaptureAtDepth0 adds 1 extra function on the chain,
       CaptureAtDepth3 adds 4.  Difference must be exactly 3. */
    size_t shallow = CaptureAtDepth0();
    size_t deep = CaptureAtDepth3();
    if (deep < shallow + 3)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Consecutive captures at the same depth must match */
    size_t count1 = StackTrace::Capture(frames, Stdlib::ArraySize(frames));
    ulong frames2[20];
    size_t count2 = StackTrace::Capture(frames2, Stdlib::ArraySize(frames2));
    if (count1 != count2)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Return addresses from consecutive captures must be identical
       (same call chain, same function, same callers above) */
    for (size_t i = 0; i < count1; i++)
    {
        /* frame[0] is the return site inside this function; it
           differs between the two calls.  Compare from frame[1] up. */
        if (i == 0)
            continue;
        if (frames[i] != frames2[i])
            return MakeError(Stdlib::Error::Unsuccessful);
    }

    Trace(0, "TestStackTrace: complete");
    return MakeSuccess();
}

Stdlib::Error TestParseUlong()
{
    Trace(0, "TestParseUlong: started");

    ulong val;

    /* Basic numbers */
    if (!Stdlib::ParseUlong("0", val) || val != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    if (!Stdlib::ParseUlong("1", val) || val != 1)
        return MakeError(Stdlib::Error::Unsuccessful);

    if (!Stdlib::ParseUlong("12345", val) || val != 12345)
        return MakeError(Stdlib::Error::Unsuccessful);

    if (!Stdlib::ParseUlong("999999", val) || val != 999999)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Invalid inputs must fail */
    if (Stdlib::ParseUlong("", val))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (Stdlib::ParseUlong(nullptr, val))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (Stdlib::ParseUlong("abc", val))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (Stdlib::ParseUlong("12x4", val))
        return MakeError(Stdlib::Error::Unsuccessful);

    if (Stdlib::ParseUlong(" 5", val))
        return MakeError(Stdlib::Error::Unsuccessful);

    Trace(0, "TestParseUlong: complete");
    return MakeSuccess();
}

Stdlib::Error TestHexCharToNibble()
{
    Trace(0, "TestHexCharToNibble: started");

    /* Decimal digits */
    for (char c = '0'; c <= '9'; c++)
    {
        if (Stdlib::HexCharToNibble(c) != (u8)(c - '0'))
            return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Lowercase hex */
    for (char c = 'a'; c <= 'f'; c++)
    {
        if (Stdlib::HexCharToNibble(c) != (u8)(c - 'a' + 10))
            return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Uppercase hex */
    for (char c = 'A'; c <= 'F'; c++)
    {
        if (Stdlib::HexCharToNibble(c) != (u8)(c - 'A' + 10))
            return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Invalid chars */
    if (Stdlib::HexCharToNibble('g') != 0xFF)
        return MakeError(Stdlib::Error::Unsuccessful);
    if (Stdlib::HexCharToNibble(' ') != 0xFF)
        return MakeError(Stdlib::Error::Unsuccessful);
    if (Stdlib::HexCharToNibble('\0') != 0xFF)
        return MakeError(Stdlib::Error::Unsuccessful);

    Trace(0, "TestHexCharToNibble: complete");
    return MakeSuccess();
}

Stdlib::Error TestHexDecode()
{
    Trace(0, "TestHexDecode: started");

    u8 out[16];
    ulong written;

    /* Simple decode */
    if (!Stdlib::HexDecode("48656C6C6F", 10, out, sizeof(out), written))
        return MakeError(Stdlib::Error::Unsuccessful);
    if (written != 5)
        return MakeError(Stdlib::Error::Unsuccessful);
    if (out[0] != 0x48 || out[1] != 0x65 || out[2] != 0x6C ||
        out[3] != 0x6C || out[4] != 0x6F)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* All zeros */
    if (!Stdlib::HexDecode("0000", 4, out, sizeof(out), written))
        return MakeError(Stdlib::Error::Unsuccessful);
    if (written != 2 || out[0] != 0 || out[1] != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* All FF */
    if (!Stdlib::HexDecode("FFFF", 4, out, sizeof(out), written))
        return MakeError(Stdlib::Error::Unsuccessful);
    if (written != 2 || out[0] != 0xFF || out[1] != 0xFF)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Lowercase */
    if (!Stdlib::HexDecode("deadbeef", 8, out, sizeof(out), written))
        return MakeError(Stdlib::Error::Unsuccessful);
    if (written != 4 || out[0] != 0xDE || out[1] != 0xAD ||
        out[2] != 0xBE || out[3] != 0xEF)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Invalid hex must fail */
    if (Stdlib::HexDecode("ZZZZ", 4, out, sizeof(out), written))
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Empty string */
    if (!Stdlib::HexDecode("", 0, out, sizeof(out), written))
        return MakeError(Stdlib::Error::Unsuccessful);
    if (written != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Odd length: only complete pairs decoded */
    if (!Stdlib::HexDecode("ABC", 3, out, sizeof(out), written))
        return MakeError(Stdlib::Error::Unsuccessful);
    if (written != 1 || out[0] != 0xAB)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Output buffer limit */
    if (!Stdlib::HexDecode("0102030405", 10, out, 2, written))
        return MakeError(Stdlib::Error::Unsuccessful);
    if (written != 2)
        return MakeError(Stdlib::Error::Unsuccessful);

    Trace(0, "TestHexDecode: complete");
    return MakeSuccess();
}

Stdlib::Error TestNextToken()
{
    Trace(0, "TestNextToken: started");

    const char* end;
    const char* tok;

    /* Single token */
    tok = Stdlib::NextToken("hello", end);
    if (!tok || Stdlib::StrnCmp(tok, "hello", 5) != 0 || (end - tok) != 5)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Leading spaces */
    tok = Stdlib::NextToken("   abc", end);
    if (!tok || Stdlib::StrnCmp(tok, "abc", 3) != 0 || (end - tok) != 3)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Two tokens */
    tok = Stdlib::NextToken("one two", end);
    if (!tok || Stdlib::StrnCmp(tok, "one", 3) != 0 || (end - tok) != 3)
        return MakeError(Stdlib::Error::Unsuccessful);
    tok = Stdlib::NextToken(end, end);
    if (!tok || Stdlib::StrnCmp(tok, "two", 3) != 0 || (end - tok) != 3)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* No more tokens */
    tok = Stdlib::NextToken(end, end);
    if (tok != nullptr)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Empty string */
    tok = Stdlib::NextToken("", end);
    if (tok != nullptr)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Only spaces */
    tok = Stdlib::NextToken("   ", end);
    if (tok != nullptr)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* TokenCopy */
    const char* input = "diskread vda 42";
    tok = Stdlib::NextToken(input, end);
    char buf[16];
    Stdlib::TokenCopy(tok, end, buf, sizeof(buf));
    if (Stdlib::StrCmp(buf, "diskread") != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    tok = Stdlib::NextToken(end, end);
    Stdlib::TokenCopy(tok, end, buf, sizeof(buf));
    if (Stdlib::StrCmp(buf, "vda") != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    tok = Stdlib::NextToken(end, end);
    Stdlib::TokenCopy(tok, end, buf, sizeof(buf));
    if (Stdlib::StrCmp(buf, "42") != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* TokenCopy truncation */
    char tiny[4];
    const char* longTok = "longtoken";
    Stdlib::TokenCopy(longTok, longTok + 9, tiny, sizeof(tiny));
    if (Stdlib::StrCmp(tiny, "lon") != 0)
        return MakeError(Stdlib::Error::Unsuccessful);

    Trace(0, "TestNextToken: complete");
    return MakeSuccess();
}

Stdlib::Error TestBlockDeviceTable()
{
    Trace(0, "TestBlockDeviceTable: started");

    /* BlockDeviceTable is a singleton used by the real system.
       We test Find with a name that doesn't exist, and Dump
       (which just exercises the code path without crashing). */

    auto& tbl = BlockDeviceTable::GetInstance();

    /* Find non-existent disk */
    if (tbl.Find("nonexistent") != nullptr)
        return MakeError(Stdlib::Error::Unsuccessful);

    if (tbl.Find("") != nullptr)
        return MakeError(Stdlib::Error::Unsuccessful);

    /* Dump should not crash (output goes to trace, not checked) */
    Trace(0, "TestBlockDeviceTable: count = %u", tbl.GetCount());

    Trace(0, "TestBlockDeviceTable: complete");
    return MakeSuccess();
}

Stdlib::Error TestContiguousPages()
{
    auto& pt = Mm::PageTable::GetInstance();

    Trace(0, "TestContiguousPages: started");

    /* count=0 must fail */
    if (pt.AllocContiguousPages(0) != nullptr)
    {
        Trace(0, "TestContiguousPages: count=0 should fail");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Alloc 1 page */
    ulong freeBefore = pt.GetFreePagesCount();
    Trace(0, "TestContiguousPages: free pages before %u", freeBefore);

    Mm::Page* p1 = pt.AllocContiguousPages(1);
    if (!p1)
    {
        Trace(0, "TestContiguousPages: alloc 1 page failed");
        return MakeError(Stdlib::Error::NoMemory);
    }

    if (pt.GetFreePagesCount() != freeBefore - 1)
    {
        Trace(0, "TestContiguousPages: free count mismatch after alloc 1");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Write and read back via TmpMap (pages are not identity-mapped) */
    ulong va1 = pt.TmpMapPage(p1->GetPhyAddress());
    if (!va1)
    {
        Trace(0, "TestContiguousPages: TmpMapPage failed");
        pt.FreePage(p1);
        return MakeError(Stdlib::Error::Unsuccessful);
    }
    Stdlib::MemSet((void*)va1, 0xAB, Const::PageSize);
    u8* ptr1 = (u8*)va1;
    for (ulong i = 0; i < Const::PageSize; i++)
    {
        if (ptr1[i] != 0xAB)
        {
            Trace(0, "TestContiguousPages: pattern mismatch at %u", i);
            pt.TmpUnmapPage(va1);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }
    pt.TmpUnmapPage(va1);

    pt.FreePage(p1);
    if (pt.GetFreePagesCount() != freeBefore)
    {
        Trace(0, "TestContiguousPages: free count mismatch after free 1");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Alloc 4 contiguous pages */
    ulong freeBefore4 = pt.GetFreePagesCount();
    Mm::Page* p4 = pt.AllocContiguousPages(4);
    if (!p4)
    {
        Trace(0, "TestContiguousPages: alloc 4 pages failed");
        return MakeError(Stdlib::Error::NoMemory);
    }

    if (pt.GetFreePagesCount() != freeBefore4 - 4)
    {
        Trace(0, "TestContiguousPages: free count mismatch after alloc 4");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Verify physical contiguity */
    ulong basePhys = p4->GetPhyAddress();
    Trace(0, "TestContiguousPages: 4 pages at phys 0x%p", basePhys);
    Mm::Page* pageArray = p4;
    for (ulong j = 0; j < 4; j++)
    {
        if (pageArray[j].GetPhyAddress() != basePhys + j * Const::PageSize)
        {
            Trace(0, "TestContiguousPages: page %u phys 0x%p expected 0x%p",
                j, pageArray[j].GetPhyAddress(), basePhys + j * Const::PageSize);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Write unique patterns to each page and verify via TmpMap */
    for (ulong j = 0; j < 4; j++)
    {
        ulong va = pt.TmpMapPage(pageArray[j].GetPhyAddress());
        BugOn(!va);
        Stdlib::MemSet((void*)va, (u8)(0xC0 + j), Const::PageSize);
        pt.TmpUnmapPage(va);
    }

    for (ulong j = 0; j < 4; j++)
    {
        ulong va = pt.TmpMapPage(pageArray[j].GetPhyAddress());
        BugOn(!va);
        u8* ptr = (u8*)va;
        u8 expected = (u8)(0xC0 + j);
        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (ptr[i] != expected)
            {
                Trace(0, "TestContiguousPages: page %u byte %u: 0x%p expected 0x%p",
                    j, i, (ulong)ptr[i], (ulong)expected);
                pt.TmpUnmapPage(va);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
        pt.TmpUnmapPage(va);
    }

    /* Free all 4 pages */
    for (ulong j = 0; j < 4; j++)
    {
        pt.FreePage(&pageArray[j]);
    }

    if (pt.GetFreePagesCount() != freeBefore4)
    {
        Trace(0, "TestContiguousPages: free count mismatch after free 4: %u expected %u",
            pt.GetFreePagesCount(), freeBefore4);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Alloc and free repeatedly to check stability */
    for (ulong round = 0; round < 3; round++)
    {
        ulong freeNow = pt.GetFreePagesCount();
        Mm::Page* pages = pt.AllocContiguousPages(2);
        if (!pages)
        {
            Trace(0, "TestContiguousPages: round %u alloc failed", round);
            return MakeError(Stdlib::Error::NoMemory);
        }

        if (pages[1].GetPhyAddress() != pages[0].GetPhyAddress() + Const::PageSize)
        {
            Trace(0, "TestContiguousPages: round %u not contiguous", round);
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        pt.FreePage(&pages[0]);
        pt.FreePage(&pages[1]);

        if (pt.GetFreePagesCount() != freeNow)
        {
            Trace(0, "TestContiguousPages: round %u free count mismatch", round);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    Trace(0, "TestContiguousPages: complete");
    return MakeSuccess();
}

Stdlib::Error TestPageAllocator()
{
    Trace(0, "TestPageAllocator: started");

    auto& pt = Mm::PageTable::GetInstance();

    /* Test AllocMapPages / UnmapFreePages with 1 page */
    {
        ulong physAddr = 0;
        void* ptr = Mm::AllocMapPages(1, &physAddr);
        if (!ptr)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(1) failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
        if (physAddr == 0)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(1) physAddr is 0");
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        Stdlib::MemSet(ptr, 0xAA, Const::PageSize);
        u8* p = (u8*)ptr;
        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (p[i] != 0xAA)
            {
                Trace(0, "TestPageAllocator: AllocMapPages(1) mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }

        Mm::UnmapFreePages(ptr);
    }

    /* Test AllocMapPages / UnmapFreePages with 2 pages */
    {
        ulong physAddr = 0;
        void* ptr = Mm::AllocMapPages(2, &physAddr);
        if (!ptr)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(2) failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
        if (physAddr == 0)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(2) physAddr is 0");
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        u8* p = (u8*)ptr;
        Stdlib::MemSet(p, 0xBB, Const::PageSize);
        Stdlib::MemSet(p + Const::PageSize, 0xCC, Const::PageSize);

        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (p[i] != 0xBB)
            {
                Trace(0, "TestPageAllocator: AllocMapPages(2) page0 mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (p[Const::PageSize + i] != 0xCC)
            {
                Trace(0, "TestPageAllocator: AllocMapPages(2) page1 mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }

        Mm::UnmapFreePages(ptr);
    }

    /* Test MapPages / UnmapPages with pre-allocated physical pages */
    {
        ulong freePagesBefore = pt.GetFreePagesCount();

        Mm::Page* pages = pt.AllocContiguousPages(2);
        if (!pages)
        {
            Trace(0, "TestPageAllocator: AllocContiguousPages(2) failed");
            return MakeError(Stdlib::Error::NoMemory);
        }

        ulong physAddrs[2];
        physAddrs[0] = pages[0].GetPhyAddress();
        physAddrs[1] = pages[1].GetPhyAddress();

        void* ptr = Mm::MapPages(2, physAddrs);
        if (!ptr)
        {
            Trace(0, "TestPageAllocator: MapPages(2) failed");
            pt.FreePage(&pages[0]);
            pt.FreePage(&pages[1]);
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        /* Write via mapped VA */
        u8* p = (u8*)ptr;
        Stdlib::MemSet(p, 0xDD, Const::PageSize);
        Stdlib::MemSet(p + Const::PageSize, 0xEE, Const::PageSize);

        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (p[i] != 0xDD)
            {
                Trace(0, "TestPageAllocator: MapPages page0 mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (p[Const::PageSize + i] != 0xEE)
            {
                Trace(0, "TestPageAllocator: MapPages page1 mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }

        /* UnmapPages - releases VA but does NOT free physical pages */
        Mm::UnmapPages(ptr, 2);

        /* Verify physical pages still contain written data via TmpMap */
        ulong va0 = pt.TmpMapPage(physAddrs[0]);
        if (!va0)
        {
            Trace(0, "TestPageAllocator: TmpMapPage page0 after UnmapPages failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
        u8* t0 = (u8*)va0;
        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (t0[i] != 0xDD)
            {
                Trace(0, "TestPageAllocator: post-unmap page0 mismatch at %u", i);
                pt.TmpUnmapPage(va0);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
        pt.TmpUnmapPage(va0);

        ulong va1 = pt.TmpMapPage(physAddrs[1]);
        if (!va1)
        {
            Trace(0, "TestPageAllocator: TmpMapPage page1 after UnmapPages failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
        u8* t1 = (u8*)va1;
        for (ulong i = 0; i < Const::PageSize; i++)
        {
            if (t1[i] != 0xEE)
            {
                Trace(0, "TestPageAllocator: post-unmap page1 mismatch at %u", i);
                pt.TmpUnmapPage(va1);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
        pt.TmpUnmapPage(va1);

        /* Now free the physical pages */
        pt.FreePage(&pages[0]);
        pt.FreePage(&pages[1]);

        if (pt.GetFreePagesCount() != freePagesBefore)
        {
            Trace(0, "TestPageAllocator: free count mismatch: %u expected %u",
                pt.GetFreePagesCount(), freePagesBefore);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Repeated alloc/map/unmap/free cycles for stability */
    {
        for (ulong round = 0; round < 5; round++)
        {
            ulong physAddr = 0;
            void* ptr = Mm::AllocMapPages(1, &physAddr);
            if (!ptr)
            {
                Trace(0, "TestPageAllocator: round %u AllocMapPages failed", round);
                return MakeError(Stdlib::Error::Unsuccessful);
            }

            u8* p = (u8*)ptr;
            Stdlib::MemSet(p, (u8)(0x10 + round), Const::PageSize);
            if (p[0] != (u8)(0x10 + round))
            {
                Trace(0, "TestPageAllocator: round %u pattern mismatch", round);
                return MakeError(Stdlib::Error::Unsuccessful);
            }

            Mm::UnmapFreePages(ptr);
        }
    }

    /* Repeated MapPages / UnmapPages cycles */
    {
        for (ulong round = 0; round < 5; round++)
        {
            Mm::Page* page = pt.AllocContiguousPages(1);
            if (!page)
            {
                Trace(0, "TestPageAllocator: MapPages round %u alloc failed", round);
                return MakeError(Stdlib::Error::NoMemory);
            }

            ulong pa = page->GetPhyAddress();
            void* ptr = Mm::MapPages(1, &pa);
            if (!ptr)
            {
                Trace(0, "TestPageAllocator: MapPages round %u map failed", round);
                pt.FreePage(page);
                return MakeError(Stdlib::Error::Unsuccessful);
            }

            u8* p = (u8*)ptr;
            Stdlib::MemSet(p, (u8)(0x50 + round), Const::PageSize);
            if (p[0] != (u8)(0x50 + round))
            {
                Trace(0, "TestPageAllocator: MapPages round %u pattern mismatch", round);
                return MakeError(Stdlib::Error::Unsuccessful);
            }

            Mm::UnmapPages(ptr, 1);
            pt.FreePage(page);
        }
    }

    /* Test AllocMapPages / UnmapFreePages with 32 pages (large DMA buffer) */
    {
        ulong freePagesBefore = pt.GetFreePagesCount();
        ulong physAddr = 0;
        void* ptr = Mm::AllocMapPages(32, &physAddr);
        if (!ptr)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(32) failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
        if (physAddr == 0)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(32) physAddr is 0");
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        u8* p = (u8*)ptr;
        for (ulong pg = 0; pg < 32; pg++)
        {
            Stdlib::MemSet(p + pg * Const::PageSize, (u8)(0xA0 + pg), Const::PageSize);
        }
        for (ulong pg = 0; pg < 32; pg++)
        {
            u8 expected = (u8)(0xA0 + pg);
            u8* base = p + pg * Const::PageSize;
            if (base[0] != expected || base[Const::PageSize - 1] != expected)
            {
                Trace(0, "TestPageAllocator: AllocMapPages(32) page %u mismatch", pg);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }

        Mm::UnmapFreePages(ptr);

        if (pt.GetFreePagesCount() != freePagesBefore)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(32) free count mismatch: %u expected %u",
                pt.GetFreePagesCount(), freePagesBefore);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Test AllocMapPages / UnmapFreePages with 128 pages (large VirtQueue) */
    {
        /* Pre-warm: the first mapping in this VA range may allocate
           intermediate page table pages (L1/L2/L3) that are never
           reclaimed on unmap.  Do a throwaway cycle so that the page
           table structure is already in place before we snapshot. */
        {
            ulong warmPhys = 0;
            void* warmPtr = Mm::AllocMapPages(128, &warmPhys);
            if (warmPtr)
                Mm::UnmapFreePages(warmPtr);
        }

        ulong freePagesBefore = pt.GetFreePagesCount();
        ulong physAddr = 0;
        void* ptr = Mm::AllocMapPages(128, &physAddr);
        if (!ptr)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(128) failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
        if (physAddr == 0)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(128) physAddr is 0");
            return MakeError(Stdlib::Error::Unsuccessful);
        }

        u8* p = (u8*)ptr;
        for (ulong pg = 0; pg < 128; pg++)
        {
            Stdlib::MemSet(p + pg * Const::PageSize, (u8)(pg & 0xFF), Const::PageSize);
        }
        for (ulong pg = 0; pg < 128; pg++)
        {
            u8 expected = (u8)(pg & 0xFF);
            u8* base = p + pg * Const::PageSize;
            if (base[0] != expected || base[Const::PageSize - 1] != expected)
            {
                Trace(0, "TestPageAllocator: AllocMapPages(128) page %u mismatch", pg);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }

        Mm::UnmapFreePages(ptr);

        if (pt.GetFreePagesCount() != freePagesBefore)
        {
            Trace(0, "TestPageAllocator: AllocMapPages(128) free count mismatch: %u expected %u",
                pt.GetFreePagesCount(), freePagesBefore);
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    Trace(0, "TestPageAllocator: complete");
    return MakeSuccess();
}

Stdlib::Error TestMemSet()
{
    Trace(0, "TestMemSet: started");

    /* Basic fill */
    {
        u8 buf[128];
        Stdlib::MemSet(buf, 0xAA, sizeof(buf));
        for (ulong i = 0; i < sizeof(buf); i++)
        {
            if (buf[i] != 0xAA)
            {
                Trace(0, "TestMemSet: basic fill mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
    }

    /* Zero fill */
    {
        u8 buf[64];
        Stdlib::MemSet(buf, 0xFF, sizeof(buf));
        Stdlib::MemSet(buf, 0, sizeof(buf));
        for (ulong i = 0; i < sizeof(buf); i++)
        {
            if (buf[i] != 0)
            {
                Trace(0, "TestMemSet: zero fill mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
    }

    /* Odd sizes (not multiple of 8) */
    {
        for (ulong sz = 0; sz <= 17; sz++)
        {
            u8 buf[32];
            Stdlib::MemSet(buf, 0, sizeof(buf));
            Stdlib::MemSet(buf, 0xCD, sz);
            for (ulong i = 0; i < sz; i++)
            {
                if (buf[i] != 0xCD)
                {
                    Trace(0, "TestMemSet: odd size %u mismatch at %u", sz, i);
                    return MakeError(Stdlib::Error::Unsuccessful);
                }
            }
            /* Bytes beyond sz must be untouched */
            for (ulong i = sz; i < sizeof(buf); i++)
            {
                if (buf[i] != 0)
                {
                    Trace(0, "TestMemSet: odd size %u overflow at %u", sz, i);
                    return MakeError(Stdlib::Error::Unsuccessful);
                }
            }
        }
    }

    Trace(0, "TestMemSet: complete");
    return MakeSuccess();
}

Stdlib::Error TestMemCpy()
{
    Trace(0, "TestMemCpy: started");

    /* Basic copy */
    {
        u8 src[128], dst[128];
        for (ulong i = 0; i < sizeof(src); i++)
            src[i] = (u8)(i & 0xFF);
        Stdlib::MemSet(dst, 0, sizeof(dst));
        Stdlib::MemCpy(dst, src, sizeof(src));
        for (ulong i = 0; i < sizeof(src); i++)
        {
            if (dst[i] != src[i])
            {
                Trace(0, "TestMemCpy: basic copy mismatch at %u", i);
                return MakeError(Stdlib::Error::Unsuccessful);
            }
        }
    }

    /* Odd sizes (not multiple of 8) */
    {
        for (ulong sz = 0; sz <= 17; sz++)
        {
            u8 src[32], dst[32];
            for (ulong i = 0; i < sizeof(src); i++)
                src[i] = (u8)((i + 0x30) & 0xFF);
            Stdlib::MemSet(dst, 0, sizeof(dst));
            Stdlib::MemCpy(dst, src, sz);
            for (ulong i = 0; i < sz; i++)
            {
                if (dst[i] != src[i])
                {
                    Trace(0, "TestMemCpy: odd size %u mismatch at %u", sz, i);
                    return MakeError(Stdlib::Error::Unsuccessful);
                }
            }
            /* Bytes beyond sz must be untouched */
            for (ulong i = sz; i < sizeof(dst); i++)
            {
                if (dst[i] != 0)
                {
                    Trace(0, "TestMemCpy: odd size %u overflow at %u", sz, i);
                    return MakeError(Stdlib::Error::Unsuccessful);
                }
            }
        }
    }

    Trace(0, "TestMemCpy: complete");
    return MakeSuccess();
}

Stdlib::Error TestMemCmp()
{
    Trace(0, "TestMemCmp: started");

    /* Equal buffers */
    {
        u8 a[64], b[64];
        Stdlib::MemSet(a, 0xAB, sizeof(a));
        Stdlib::MemSet(b, 0xAB, sizeof(b));
        if (Stdlib::MemCmp(a, b, sizeof(a)) != 0)
        {
            Trace(0, "TestMemCmp: equal buffers returned non-zero");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* First < second */
    {
        u8 a[] = {1, 2, 3};
        u8 b[] = {1, 2, 4};
        if (Stdlib::MemCmp(a, b, 3) >= 0)
        {
            Trace(0, "TestMemCmp: a < b not detected");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* First > second */
    {
        u8 a[] = {1, 2, 5};
        u8 b[] = {1, 2, 4};
        if (Stdlib::MemCmp(a, b, 3) <= 0)
        {
            Trace(0, "TestMemCmp: a > b not detected");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Difference at first byte */
    {
        u8 a[] = {0x00, 0xFF};
        u8 b[] = {0xFF, 0x00};
        if (Stdlib::MemCmp(a, b, 2) >= 0)
        {
            Trace(0, "TestMemCmp: first byte a < b not detected");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Zero length */
    {
        u8 a[] = {1};
        u8 b[] = {2};
        if (Stdlib::MemCmp(a, b, 0) != 0)
        {
            Trace(0, "TestMemCmp: zero length not equal");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Odd sizes */
    {
        u8 a[17], b[17];
        for (ulong i = 0; i < sizeof(a); i++)
        {
            a[i] = (u8)i;
            b[i] = (u8)i;
        }
        if (Stdlib::MemCmp(a, b, sizeof(a)) != 0)
        {
            Trace(0, "TestMemCmp: odd size equal mismatch");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
        b[16] = 0xFF;
        if (Stdlib::MemCmp(a, b, sizeof(a)) >= 0)
        {
            Trace(0, "TestMemCmp: odd size diff at end not detected");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    Trace(0, "TestMemCmp: complete");
    return MakeSuccess();
}

Stdlib::Error TestStrLen()
{
    Trace(0, "TestStrLen: started");

    /* Empty string */
    if (Stdlib::StrLen("") != 0)
    {
        Trace(0, "TestStrLen: empty string failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Single char */
    if (Stdlib::StrLen("A") != 1)
    {
        Trace(0, "TestStrLen: single char failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Known string */
    if (Stdlib::StrLen("hello") != 5)
    {
        Trace(0, "TestStrLen: 'hello' failed, got %u", Stdlib::StrLen("hello"));
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* String with embedded high bytes */
    {
        char buf[10] = {'\xFF', '\xFE', '\x01', '\0', 'X', '\0'};
        if (Stdlib::StrLen(buf) != 3)
        {
            Trace(0, "TestStrLen: high bytes failed, got %u", Stdlib::StrLen(buf));
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Longer string (> 8 bytes, tests qword boundary) */
    if (Stdlib::StrLen("abcdefghijklmnop") != 16)
    {
        Trace(0, "TestStrLen: 16-char string failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    Trace(0, "TestStrLen: complete");
    return MakeSuccess();
}

Stdlib::Error TestStrCmp()
{
    Trace(0, "TestStrCmp: started");

    /* Equal strings */
    if (Stdlib::StrCmp("abc", "abc") != 0)
    {
        Trace(0, "TestStrCmp: equal strings failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Empty strings */
    if (Stdlib::StrCmp("", "") != 0)
    {
        Trace(0, "TestStrCmp: empty strings failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* First < second */
    if (Stdlib::StrCmp("abc", "abd") >= 0)
    {
        Trace(0, "TestStrCmp: abc < abd failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* First > second */
    if (Stdlib::StrCmp("abd", "abc") <= 0)
    {
        Trace(0, "TestStrCmp: abd > abc failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Prefix: shorter < longer */
    if (Stdlib::StrCmp("ab", "abc") >= 0)
    {
        Trace(0, "TestStrCmp: ab < abc failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Prefix: longer > shorter */
    if (Stdlib::StrCmp("abc", "ab") <= 0)
    {
        Trace(0, "TestStrCmp: abc > ab failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Single char difference */
    if (Stdlib::StrCmp("a", "b") >= 0)
    {
        Trace(0, "TestStrCmp: a < b failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Empty vs non-empty */
    if (Stdlib::StrCmp("", "a") >= 0)
    {
        Trace(0, "TestStrCmp: '' < 'a' failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }
    if (Stdlib::StrCmp("a", "") <= 0)
    {
        Trace(0, "TestStrCmp: 'a' > '' failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Longer strings (cross qword boundary) */
    if (Stdlib::StrCmp("abcdefghij", "abcdefghij") != 0)
    {
        Trace(0, "TestStrCmp: long equal failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }
    if (Stdlib::StrCmp("abcdefghij", "abcdefghik") >= 0)
    {
        Trace(0, "TestStrCmp: long diff at end failed");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    Trace(0, "TestStrCmp: complete");
    return MakeSuccess();
}

Stdlib::Error TestStrStr()
{
    Trace(0, "TestStrStr: started");

    /* Empty needle always matches at start */
    {
        const char* h = "hello";
        if (Stdlib::StrStr(h, "") != h)
        {
            Trace(0, "TestStrStr: empty needle failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Match at beginning */
    {
        const char* h = "abcdef";
        const char* r = Stdlib::StrStr(h, "abc");
        if (r != h)
        {
            Trace(0, "TestStrStr: match at start failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Match in middle */
    {
        const char* h = "hello world";
        const char* r = Stdlib::StrStr(h, "world");
        if (r != h + 6)
        {
            Trace(0, "TestStrStr: match in middle failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Match at end */
    {
        const char* h = "abcxyz";
        const char* r = Stdlib::StrStr(h, "xyz");
        if (r != h + 3)
        {
            Trace(0, "TestStrStr: match at end failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* No match */
    {
        if (Stdlib::StrStr("abcdef", "xyz") != nullptr)
        {
            Trace(0, "TestStrStr: no match returned non-null");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Partial match then fail */
    {
        const char* h = "abcabd";
        const char* r = Stdlib::StrStr(h, "abd");
        if (r != h + 3)
        {
            Trace(0, "TestStrStr: partial match failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Needle longer than haystack */
    {
        if (Stdlib::StrStr("ab", "abcdef") != nullptr)
        {
            Trace(0, "TestStrStr: needle longer returned non-null");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Both empty */
    {
        const char* h = "";
        if (Stdlib::StrStr(h, "") != h)
        {
            Trace(0, "TestStrStr: both empty failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Single char match */
    {
        const char* h = "abcdef";
        const char* r = Stdlib::StrStr(h, "d");
        if (r != h + 3)
        {
            Trace(0, "TestStrStr: single char match failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    /* Exact match */
    {
        const char* h = "exact";
        if (Stdlib::StrStr(h, "exact") != h)
        {
            Trace(0, "TestStrStr: exact match failed");
            return MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    Trace(0, "TestStrStr: complete");
    return MakeSuccess();
}

Stdlib::Error TestSnPrintf()
{
    Trace(0, "TestSnPrintf: started");

    char buf[128];
    int rc;

    /* Existing specifiers still work */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "hello %s", "world");
    if (rc < 0 || Stdlib::StrCmp(buf, "hello world") != 0)
    {
        Trace(0, "TestSnPrintf: %%s failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%u", (ulong)42);
    if (rc < 0 || Stdlib::StrCmp(buf, "42") != 0)
    {
        Trace(0, "TestSnPrintf: %%u failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%c", (int)'A');
    if (rc < 0 || Stdlib::StrCmp(buf, "A") != 0)
    {
        Trace(0, "TestSnPrintf: %%c failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%u", (ulong)0);
    if (rc < 0 || Stdlib::StrCmp(buf, "0") != 0)
    {
        Trace(0, "TestSnPrintf: %%u zero failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* %% literal percent */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "100%%");
    if (rc < 0 || Stdlib::StrCmp(buf, "100%") != 0)
    {
        Trace(0, "TestSnPrintf: %%%% failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%%start");
    if (rc < 0 || Stdlib::StrCmp(buf, "%start") != 0)
    {
        Trace(0, "TestSnPrintf: %%%% at start failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* %d signed decimal */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%d", (long)42);
    if (rc < 0 || Stdlib::StrCmp(buf, "42") != 0)
    {
        Trace(0, "TestSnPrintf: %%d positive failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%d", (long)-1);
    if (rc < 0 || Stdlib::StrCmp(buf, "-1") != 0)
    {
        Trace(0, "TestSnPrintf: %%d -1 failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%d", (long)-12345);
    if (rc < 0 || Stdlib::StrCmp(buf, "-12345") != 0)
    {
        Trace(0, "TestSnPrintf: %%d negative failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%d", (long)0);
    if (rc < 0 || Stdlib::StrCmp(buf, "0") != 0)
    {
        Trace(0, "TestSnPrintf: %%d zero failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* %x lowercase hex */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%x", (ulong)0);
    if (rc < 0 || Stdlib::StrCmp(buf, "0") != 0)
    {
        Trace(0, "TestSnPrintf: %%x zero failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%x", (ulong)0xDEAD);
    if (rc < 0 || Stdlib::StrCmp(buf, "dead") != 0)
    {
        Trace(0, "TestSnPrintf: %%x deadbeef failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%x", (ulong)255);
    if (rc < 0 || Stdlib::StrCmp(buf, "ff") != 0)
    {
        Trace(0, "TestSnPrintf: %%x 255 failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* %X uppercase hex */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%X", (ulong)0xDEAD);
    if (rc < 0 || Stdlib::StrCmp(buf, "DEAD") != 0)
    {
        Trace(0, "TestSnPrintf: %%X failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%X", (ulong)255);
    if (rc < 0 || Stdlib::StrCmp(buf, "FF") != 0)
    {
        Trace(0, "TestSnPrintf: %%X 255 failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Zero-padded width: %08x */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%08x", (ulong)0xFF);
    if (rc < 0 || Stdlib::StrCmp(buf, "000000ff") != 0)
    {
        Trace(0, "TestSnPrintf: %%08x failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%016X", (ulong)0xABCD1234);
    if (rc < 0 || Stdlib::StrCmp(buf, "00000000ABCD1234") != 0)
    {
        Trace(0, "TestSnPrintf: %%016X failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Zero-padded decimal: %05u */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%05u", (ulong)42);
    if (rc < 0 || Stdlib::StrCmp(buf, "00042") != 0)
    {
        Trace(0, "TestSnPrintf: %%05u failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Zero-padded signed: %05d with negative */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%05d", (long)-42);
    if (rc < 0 || Stdlib::StrCmp(buf, "-0042") != 0)
    {
        Trace(0, "TestSnPrintf: %%05d negative failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Zero-padded signed: value fills width */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%03d", (long)12345);
    if (rc < 0 || Stdlib::StrCmp(buf, "12345") != 0)
    {
        Trace(0, "TestSnPrintf: %%03d overflow failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Width with no zero-pad (space padding) */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%5u", (ulong)42);
    if (rc < 0 || Stdlib::StrCmp(buf, "   42") != 0)
    {
        Trace(0, "TestSnPrintf: %%5u space pad failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%8x", (ulong)0xFF);
    if (rc < 0 || Stdlib::StrCmp(buf, "      ff") != 0)
    {
        Trace(0, "TestSnPrintf: %%8x space pad failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* String with width */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%10s", "hi");
    if (rc < 0 || Stdlib::StrCmp(buf, "        hi") != 0)
    {
        Trace(0, "TestSnPrintf: %%10s failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Mixed specifiers in one format string */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%d %u %x %X %s %%",
                          (long)-7, (ulong)100, (ulong)0xAB, (ulong)0xCD, "ok");
    if (rc < 0 || Stdlib::StrCmp(buf, "-7 100 ab CD ok %") != 0)
    {
        Trace(0, "TestSnPrintf: mixed failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* Null string */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%s", (const char*)nullptr);
    if (rc < 0 || Stdlib::StrCmp(buf, "(null)") != 0)
    {
        Trace(0, "TestSnPrintf: null string failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    /* %p still works (uppercase hex) */
    rc = Stdlib::SnPrintf(buf, sizeof(buf), "%p", (void*)(ulong)0xABCDEF);
    if (rc < 0 || Stdlib::StrCmp(buf, "ABCDEF") != 0)
    {
        Trace(0, "TestSnPrintf: %%p failed: '%s'", buf);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    Trace(0, "TestSnPrintf: complete");
    return MakeSuccess();
}

Stdlib::Error Test()
{
    Stdlib::Error err;

    Trace(0, "Self-test in progress, please wait...");

    err = TestAllocator();
    if (!err.Ok())
        return err;

    err = TestBtree();
    if (!err.Ok())
        return err;

    err = TestRingBuffer();
    if (!err.Ok())
        return err;

    err = TestStackTrace();
    if (!err.Ok())
        return err;

    err = TestParseUlong();
    if (!err.Ok())
        return err;

    err = TestHexCharToNibble();
    if (!err.Ok())
        return err;

    err = TestHexDecode();
    if (!err.Ok())
        return err;

    err = TestNextToken();
    if (!err.Ok())
        return err;

    err = TestBlockDeviceTable();
    if (!err.Ok())
        return err;

    err = TestContiguousPages();
    if (!err.Ok())
        return err;

    err = TestPageAllocator();
    if (!err.Ok())
        return err;

    err = TestMemSet();
    if (!err.Ok())
        return err;

    err = TestMemCpy();
    if (!err.Ok())
        return err;

    err = TestMemCmp();
    if (!err.Ok())
        return err;

    err = TestStrLen();
    if (!err.Ok())
        return err;

    err = TestStrCmp();
    if (!err.Ok())
        return err;

    err = TestStrStr();
    if (!err.Ok())
        return err;

    err = TestSnPrintf();
    if (!err.Ok())
        return err;

    return err;
}

void TestMultiTaskingTaskFunc(void *ctx)
{
    (void)ctx;

    for (size_t i = 0; i < 2; i++)
    {
        auto& cpu = GetCpu();
        auto task = Task::GetCurrentTask();
        Trace(0, "Hello from task 0x%p pid %u cpu %u", task, task->Pid, cpu.GetIndex());
        Sleep(100 * Const::NanoSecsInMs);
    }
}

bool TestMultiTasking()
{
    Task *task[2] = {0};
    for (size_t i = 0; i < Stdlib::ArraySize(task); i++)
    {
        task[i] = Kernel::Mm::TAlloc<Task, Tag>();
        if (task[i] == nullptr)
        {
            for (size_t j = 0; j < i; j++)
            {
                task[j]->Put();
            }
            return false;
        }
    }

    bool result;

    for (size_t i = 0; i < Stdlib::ArraySize(task); i++)
    {
        if (!task[i]->Start(TestMultiTaskingTaskFunc, nullptr))
        {
            for (size_t j = 0; j < i; j++)
            {
                task[j]->Wait();
            }
            result = false;
            goto delTasks;
        }
    }

    for (size_t i = 0; i < Stdlib::ArraySize(task); i++)
    {
        task[i]->Wait();
    }

    result = true;

delTasks:
    for (size_t i = 0; i < Stdlib::ArraySize(task); i++)
    {
        task[i]->Put();
    }

    return result;
}

}

}
