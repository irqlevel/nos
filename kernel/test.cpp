#include "trace.h"
#include "debug.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "stack_trace.h"
#include "block_device.h"

#include <lib/btree.h>
#include <lib/error.h>
#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/vector.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>

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

Stdlib::Error TestStackTrace3()
{
    ulong frames[20];
    size_t framesCount;

    framesCount = StackTrace::Capture(4096, frames, Stdlib::ArraySize(frames));
    for (size_t i = 0; i < framesCount; i++)
        Trace(0, "frame[%u]=0x%p", i, frames[i]);

    return MakeSuccess();
}

Stdlib::Error TestStackTrace2()
{
    return TestStackTrace3();
}

Stdlib::Error TestStackTrace()
{
    return TestStackTrace2();
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

    /* count > 16 must fail */
    if (pt.AllocContiguousPages(17) != nullptr)
    {
        Trace(0, "TestContiguousPages: count=17 should fail");
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

Stdlib::Error Test()
{
    Stdlib::Error err;

    Trace(0, "Test in progress, please wait...");

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
