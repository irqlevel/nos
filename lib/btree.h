#pragma once

#include "lock.h"
#include "shared_ptr.h"

#include <kernel/trace.h>
#include <mm/new.h>
#include <kernel/panic.h>

namespace Stdlib
{

const int BtreeLL = 6;

template<typename K, typename V, size_t T, typename LockType = Stdlib::NopLock>
class Btree
{
public:
    Btree()
    {
        Trace(BtreeLL, "tree 0x%p ctor", this);
    }

    virtual ~Btree()
    {
        Trace(BtreeLL, "tree 0x%p dtor, root 0x%p", this, Root.Get());
        Clear();
        Trace(BtreeLL, "tree 0x%p dtor complete", this);
    }

    bool Insert(const K& key, const V& value)
    {
        Stdlib::AutoLock lock(Lock);

        bool exist;
        LookupLocked(key, exist);
        if (exist)
        {
            return false;
        }

        if (Root.Get() == nullptr)
        {
            Root = MakeShared<BtreeNode>(true);
            if (Root.Get() == nullptr)
            {
                return false;
            }
        }

        if (Root->IsFull())
        {
            auto newNode = MakeShared<BtreeNode>();
            if (newNode.Get() == nullptr)
            {
                return false;
            }
            auto newNode2 = MakeShared<BtreeNode>();
            if (newNode2.Get() == nullptr)
            {
                return false;
            }
            newNode->PutChild(0, Root);
            newNode->SplitChild(0, newNode2);
            Root = newNode;
        }

        return Root->InsertNonFull(Root, key, value);
    }

    bool Delete(const K& key)
    {
        Stdlib::AutoLock lock(Lock);

        Trace(BtreeLL, "root 0x%p", Root.Get());

        BtreeNodePtr node = Root;
        if (node.Get() == nullptr)
        {
            return false;
        }

        KeyToDelete = key;

restart:
        size_t i = -1;
        bool exist = node->GetKeyIndex(KeyToDelete, i);
        Trace(BtreeLL, "node 0x%p get key index %lu", node.Get(), (exist) ? i : -1);
        if (exist)
        {
            if (node->IsLeaf())
            {
                Trace(BtreeLL, "node 0x%p is leaf", node.Get());
                node->DeleteKey(i);
                node->DecKeyCount();
                return true;
            }
            else
            {
                BtreeNodePtr preChild = node->GetChild(i);
                BtreeNodePtr sucChild = node->GetChild(i + 1);
                if (BugOn(preChild.Get() == nullptr))
                    return false;

                if (BugOn(sucChild.Get() == nullptr))
                    return false;

                if (preChild->GetKeyCount() >= T)
                {
                    size_t preIndex;
                    auto preNode = preChild->FindLeftMost(preChild, preIndex);

                    if (BugOn(preNode.Get() == nullptr))
                        return false;

                    node->CopyKey(i, preNode, preIndex);
                    node = preNode;
                    KeyToDelete = preNode->GetKey(preIndex);
                    Trace(BtreeLL, "node 0x%p new key %lu", node.Get(), preIndex);
                    goto restart;
                }
                else if (sucChild->GetKeyCount() >= T)
                {
                    size_t sucIndex;
                    auto sucNode = sucChild->FindRightMost(sucChild, sucIndex);

                    if (BugOn(sucNode.Get() == nullptr))
                        return false;

                    node->CopyKey(i, sucNode, sucIndex);
                    node = sucNode;
                    KeyToDelete = sucNode->GetKey(sucIndex);
                    Trace(BtreeLL, "node 0x%p new key %lu", node.Get(), sucIndex);
                    goto restart;
                }
                else
                {
                    /* merge key and all of sucChild into preChild
                    * node loses key and pointer to sucChild
                    */
                    size_t keyIndex = preChild->GetKeyCount();

                    Trace(BtreeLL, "node 0x%p pre child 0x%p merge keyIndex %lu",
                        node.Get(), preChild.Get(), keyIndex);

                    preChild->Merge(sucChild,
                                    Stdlib::Move(node->GetKey(i)),
                                    Stdlib::Move(node->GetValue(i)));
                    node->DeleteKey(i);
                    node->DeleteChild(i + 1);
                    node->DecKeyCount();
                    /* delete sucChild */
                    sucChild.Reset();
                    if (node->GetKeyCount() == 0)
                    {
                        node->Copy(preChild);
                        /* delete preChild */
                        preChild.Reset();
                    } else {
                        node = preChild;
                    }

                    KeyToDelete = node->GetKey(keyIndex);
                    Trace(BtreeLL, "node 0x%p new key %lu", node.Get(), keyIndex);
                    goto restart;
                }
            }
        }
        else
        {
            if (node->IsLeaf())
            {
                Trace(BtreeLL, "node 0x%p is leaf", node.Get());
                return false;
            }
            else
            {
                i = node->FindKeyIndex(KeyToDelete);
                Trace(BtreeLL, "node 0x%p find key index %lu", node.Get(), i);
                node = node->ChildBalance(node, i);

                if (BugOn(node.Get() == nullptr))
                    return false;

                goto restart;
            }
        }
        return false;
    }

    V Lookup(const K& key, bool& exist)
    {
        Stdlib::SharedAutoLock lock(Lock);
        return LookupLocked(key, exist);
    }

    bool Check()
    {
        Stdlib::SharedAutoLock lock(Lock);
        if (Root.Get() == nullptr)
            return true;

        return Root->Check(true);
    }

    void Clear()
    {
        Stdlib::AutoLock lock(Lock);

        if (Root.Get() == nullptr)
            return;

        BtreeNodePtr curr, parent;
        size_t childIndex;

restart:
        curr = Root;
        parent.Reset(nullptr);
        childIndex = -1;

        for (;;)
        {
            if (curr->IsLeaf())
            {
                if (parent.Get() != nullptr)
                {
                    if (childIndex > 0)
                    {
                        parent->DeleteKey(childIndex - 1);
                        parent->DecKeyCount();
                    }
                    parent->DeleteChild(childIndex);
                    if (parent->GetChildCount() == 0)
                    {
                        parent->SetLeaf(true);
                    }
                    goto restart;
                }
                else
                {
                    if (BugOn(curr.Get() != Root.Get()))
                        return;

                    goto finish;
                }
            }
            else
            {
                for (size_t i = 0; i < (curr->GetKeyCount() + 1); i++)
                {
                    parent = curr;
                    curr = curr->GetChild(i);
                    childIndex = i;
                    break;
                }
            }
        }

finish:
        if (BugOn(Root->GetChildCount() != 0))
            return;

        Root.Reset();
    }

    size_t MinDepth()
    {
        Stdlib::SharedAutoLock lock(Lock);
        if (Root.Get() == nullptr)
        {
            return 0;
        }
        return MinDepth(Root);
    }

    size_t MaxDepth()
    {
        Stdlib::SharedAutoLock lock(Lock);
        if (Root.Get() == nullptr)
        {
            return 0;
        }
        return MaxDepth(Root);
    }

private:
    Btree(const Btree& other) = delete;
    Btree(Btree&& other) = delete;
    Btree& operator=(const Btree& other) = delete;
    Btree& operator=(Btree&& other) = delete;

    class BtreeNode;
    using BtreeNodePtr = SharedPtr<BtreeNode>;

    size_t MinDepth(const BtreeNodePtr& node)
    {
        if (BugOn(node.Get() == nullptr))
            return 0;

        if (node->IsLeaf())
        {
            return 1;
        }

        size_t depth = MinDepth(node->GetChild(0));

        for (size_t i = 1; i < (node->GetKeyCount() + 1); i++)
        {
            depth = Stdlib::Min<size_t>(depth, MinDepth(node->GetChild(i)));
        }

        return 1 + depth;
    }

    size_t MaxDepth(const BtreeNodePtr& node)
    {
        if (BugOn(node.Get() == nullptr))
            return 0;

        if (node->IsLeaf())
        {
            return 1;
        }

        size_t depth = MaxDepth(node->GetChild(0));

        for (size_t i = 1; i < (node->GetKeyCount() + 1); i++)
        {
            depth = Stdlib::Max<size_t>(depth, MaxDepth(node->GetChild(i)));
        }

        return 1 + depth;
    }

    V LookupLocked(const K& key, bool& exist)
    {
        exist = false;
        if (Root.Get() == nullptr)
        {
            return EmptyValue;
        }

        BtreeNodePtr node = Root;
        for (;;)
        {
            if (node->GetKeyCount() == 0)
                return EmptyValue;

            size_t i = node->FindKeyIndex(key);
            if (i < node->GetKeyCount() && key == node->GetKey(i))
            {
                exist = true;
                return node->GetValue(i);
            }
            else if (node->IsLeaf())
            {
                return EmptyValue;
            }
            else
            {
                node = node->GetChild(i);
                if (BugOn(node.Get() == nullptr))
                    return EmptyValue;
            }
        }
    }

    class BtreeNode {
    public:
        BtreeNode(bool leaf = false)
        {
            Trace(BtreeLL, "node 0x%p ctor", this);
            SetLeaf(leaf);
            SetKeyCount(0);
        }

        virtual ~BtreeNode()
        {
            Trace(BtreeLL, "node 0x%p dtor", this);

            for (size_t i = 0; i < 2 * T; i++)
            {
                if (BugOn(Child[i].Get() != nullptr))
                    return;

                Child[i].Reset();
            }

            Trace(BtreeLL, "node 0x%p dtor complete", this);
        }

        bool IsFull()
        {
            bool result = ((2 * T - 1) == KeyCount) ? true : false;
            Trace(BtreeLL, "node 0x%p full %d keyCount %lu", this, result, KeyCount);
            return result;
        }

        void CopyKey(size_t index, const BtreeNodePtr& src, size_t srcIndex)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;

            if (BugOn(srcIndex < 0 || srcIndex >= (2 * T - 1)))
                return;

            Trace(BtreeLL, "node 0x%p copy key %lu", this, index);

            SetKey(index, Stdlib::Move(src->GetKey(srcIndex)));
            SetValue(index, Stdlib::Move(src->GetValue(srcIndex)));
        }

        void CopyChild(size_t index, const BtreeNodePtr& src, size_t srcIndex)
        {
            if (BugOn(index < 0 || index >= 2 * T))
                return;

            if (BugOn(srcIndex < 0 || srcIndex >= 2 * T))
                return;

            Trace(BtreeLL, "node 0x%p copy child %lu", this, index);

            SetChild(index, Stdlib::Move(src->Child[srcIndex]));
        }

        void PutKey(size_t index, const K& key, const V& value)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;

            Trace(BtreeLL, "node 0x%p put key %lu", this, index);

            /* free space */
            for (ssize_t i = KeyCount - 1; i >= static_cast<ssize_t>(index); i--)
            {
                SetKey(i + 1, Stdlib::Move(GetKey(i)));
                SetValue(i + 1, Stdlib::Move(GetValue(i)));
            }

            SetKey(index, key);
            SetValue(index, value);
        }

        void PutKey(size_t index, const BtreeNodePtr& src, size_t srcIndex)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;

            if (BugOn(srcIndex < 0 || srcIndex >= (2 * T - 1)))
                return;

            Trace(BtreeLL, "node 0x%p put key %lu", this, index);

            /* free space */
            for (ssize_t i = KeyCount - 1; i >= static_cast<ssize_t>(index); i--)
            {
                SetKey(i + 1, Stdlib::Move(GetKey(i)));
                SetValue(i + 1, Stdlib::Move(GetValue(i)));
            }

            SetKey(index, Stdlib::Move(src->GetKey(srcIndex)));
            SetValue(index, Stdlib::Move(src->GetValue(srcIndex)));
        }

        void SetValue(size_t index, const V& value)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;

            Value[index] = value;
            Trace(BtreeLL, "node 0x%p set value %lu", this, index);
        }

        void SetValue(size_t index, V&& value)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;

            Value[index] = Stdlib::Move(value);
            Trace(BtreeLL, "node 0x%p set value %lu", this, index);
        }

        void SetKeyCheck(size_t index)
        {
            K* prevKey = nullptr;
            for (size_t i = 0; i <= index; i++)
            {
                if (prevKey != nullptr && *prevKey >= Key[i])
                {
                    Trace(BtreeLL, "node 0x%p prev key bigger %lu", this, i);
                }
                prevKey = &Key[i];
            }
        }

        void SetKey(size_t index, K&& key)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;

            Key[index] = Stdlib::Move(key);
            Trace(BtreeLL, "node 0x%p set key %lu", this, index);
            SetKeyCheck(index);
        }

        void SetKey(size_t index, const K& key)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;

            Key[index] = key;
            Trace(BtreeLL, "node 0x%p set key %lu", this, index);
            SetKeyCheck(index);
        }

        void SetChild(size_t index, const BtreeNodePtr& child)
        {
            if (BugOn(index < 0 || index >= 2 * T))
                return;

            Child[index] = child;
            Trace(BtreeLL, "node 0x%p set child %lu 0x%p", this, index, Child[index].Get());
        }

        void SetChild(size_t index, BtreeNodePtr&& child)
        {
            if (BugOn(index < 0 || index >= 2 * T))
                return;

            Child[index] = Stdlib::Move(child);
            Trace(BtreeLL, "node 0x%p set child %lu 0x%p", this, index, Child[index].Get());
        }

        void PutChild(size_t index, const BtreeNodePtr& child)
        {
            if (BugOn(index < 0 || index >= 2 * T))
                return;

            Trace(BtreeLL, "node 0x%p put child %lu", this, index);

            /* free space */
            for (ssize_t i = KeyCount; i >= static_cast<ssize_t>(index); i--)
            {
                SetChild(i + 1, Stdlib::Move(Child[i]));
            }

            SetChild(index, child);
        }

        void PutChild(size_t index, const BtreeNodePtr& src, size_t srcIndex)
        {
            if (BugOn(index < 0 || index >= 2 * T))
                return;
            if (BugOn(srcIndex < 0 || srcIndex >= 2 * T))
                return;

            PutChild(index, Stdlib::Move(src->Child[srcIndex]));
        }

        bool IsLeaf()
        {
            return Leaf;
        }

        void SetLeaf(bool leaf)
        {
            Leaf = leaf;
            Trace(BtreeLL, "node 0x%p set leaf %d", this, leaf);
        }

        void SetKeyCount(size_t keyCount)
        {
            if (BugOn(keyCount < 0 || keyCount > (2 * T - 1)))
                return;

            size_t oldKeyCount = KeyCount;
            KeyCount = keyCount;
            Trace(BtreeLL, "node 0x%p set keyCount %lu old %lu", this, keyCount, oldKeyCount);
        }

        void IncKeyCount(size_t delta = 1)
        {
            SetKeyCount(KeyCount + delta);
        }

        void DecKeyCount()
        {
            SetKeyCount(KeyCount - 1);
        }

        void DeleteChild(size_t index)
        {
            if (BugOn(index < 0 || index >= 2 * T))
                return;

            if (BugOn(index >= (KeyCount + 1)))
                return;

            Trace(BtreeLL, "node 0x%p delete child %lu 0x%p", this, index, Child[index].Get());

            Child[index].Reset();

            for (size_t i = (index + 1); i < (KeyCount + 1); i++)
            {
                SetChild(i - 1, Stdlib::Move(Child[i]));
            }
        }

        void DeleteKey(size_t index)
        {
            if (BugOn(index < 0 || index >= (2 * T - 1)))
                return;
            if (BugOn(index >= KeyCount))
                return;

            Trace(BtreeLL, "node 0x%p delete key %lu", this, index);

            SetKey(index, EmptyKey);
            SetValue(index, EmptyValue);

            for (size_t i = (index + 1); i < KeyCount; i++)
            {
                SetKey(i - 1, Stdlib::Move(GetKey(i)));
                SetValue(i - 1, Stdlib::Move(GetValue(i)));
            }
        }

        void SplitChild(size_t childIndex, const BtreeNodePtr& sibling)
        {
            Trace(BtreeLL, "node 0x%p split child %lu sibling 0x%p",
                this, childIndex, sibling.Get());

            auto child = GetChild(childIndex);

            if (BugOn(child.Get() == nullptr))
                return;

            if (BugOn(sibling.Get() == nullptr))
                return;

            sibling->SetLeaf(child->IsLeaf());
            /* copy T-1 keys from child to sibling */
            for (size_t i = 0; i < (T - 1); i++)
            {
                sibling->CopyKey(i,  child, i + T);
            }
            sibling->SetKeyCount(T - 1);
            /* copy T childs from child to new */
            if (!child->IsLeaf())
            {
                for (size_t i = 0; i < T; i++)
                {
                    sibling->CopyChild(i, child, i + T);
                }
            }
            /* setup node new child */
            PutChild(childIndex + 1, sibling);
            /* move mid key from child to node */
            PutKey(childIndex, child, T - 1);
            /* shift node childs to the right by one */
            child->SetKeyCount(T - 1);
            IncKeyCount();
        }

        size_t FindKeyIndex(const K& key)
        {
            if (KeyCount == 0)
                return 0;

            size_t start = 0;
            size_t end = KeyCount;
            if (key > Key[end - 1])
                return end;
            else if (key < Key[start])
                return 0;

            while (start < end)
            {
                size_t mid = (start + end) / 2;
                if (key == Key[mid])
                    return mid;
                else if (key < Key[mid])
                    end = mid;
                else
                    start = mid + 1;
            }

            return end;
        }

        bool IsKeyProbablyInside(const K& key)
        {
            if (KeyCount == 0)
                return false;
            else if ((key > Key[KeyCount-1]) || (key < Key[0]))
                return false;

            return true;
        }

        bool GetKeyIndex(const K& key, size_t& index)
        {
            if (!IsKeyProbablyInside(key))
            {
                Trace(BtreeLL, "node 0x%p key not inside", this);
                return false;
            }

            size_t start = 0;
            size_t end = KeyCount;
            while (start < end)
            {
                size_t mid = (start + end) / 2;
                if (key == Key[mid])
                {
                    Trace(BtreeLL, "node 0x%p get key at %lu", this, mid);
                    index = mid;
                    return true;
                }
                else if (key < Key[mid])
                    end = mid;
                else
                    start = mid + 1;
            }

            Trace(BtreeLL, "node 0x%p key not found", this);
            return false;
        }

        bool InsertNonFull(const BtreeNodePtr& self, const K& key, const V& value)
        {
            if (BugOn(self.Get() != this))
                return false;

            BtreeNodePtr node = self;

            for (;;)
            {
                size_t i;
                bool exist = node->GetKeyIndex(key, i);
                if (exist)
                {
                    return false;
                }

                i = node->FindKeyIndex(key);
                if (node->IsLeaf())
                {
                    node->PutKey(i, key, value);
                    node->IncKeyCount();
                    return true;
                }
                else
                {
                    auto child = node->GetChild(i);
                    if (BugOn(child.Get() == nullptr))
                        return false;

                    if (child->IsFull())
                    {
                        auto newNode = MakeShared<BtreeNode>();
                        if (newNode.Get() == nullptr)
                        {
                            return false;
                        }
                        node->SplitChild(i, newNode);
                        continue; /* restart */
                    }
                    node = child;
                }
            }
        }

        BtreeNodePtr FindLeftMost(const BtreeNodePtr& self, size_t& index)
        {
            if (BugOn(self.Get() != this))
                return BtreeNodePtr();

            Trace(BtreeLL, "node 0x%p find left most", this);

            BtreeNodePtr curr = self;

            for (;;)
            {
                BugOn(curr->KeyCount == 0);
                if (curr->Leaf)
                {
                    index = curr->KeyCount - 1;
                    return curr;
                }
                curr = curr->ChildBalance(curr, curr->KeyCount);
            }
        }

        BtreeNodePtr FindRightMost(const BtreeNodePtr& self, size_t& index)
        {
            if (BugOn(self.Get() != this))
                return BtreeNodePtr();

            Trace(BtreeLL, "node 0x%p find right most", this);

            BtreeNodePtr curr = self;

            for (;;)
            {
                if (BugOn(curr->KeyCount == 0))
                    return BtreeNodePtr();

                if (curr->Leaf)
                {
                    index = 0;
                    return curr;
                }
                curr = curr->ChildBalance(curr, 0);
            }
        }

        void Merge(const BtreeNodePtr& src, K&& key, V&& value)
        {
            SetKey(KeyCount, Stdlib::Move(key));
            SetValue(KeyCount, Stdlib::Move(value));

            size_t pos = KeyCount + 1, i;
            for (i = 0; i < src->KeyCount; i++, pos++)
            {
                SetKey(pos, Stdlib::Move(src->GetKey(i)));
                SetValue(pos, Stdlib::Move(src->GetValue(i)));
                SetChild(pos, Stdlib::Move(src->Child[i]));
            }

            SetChild(pos, Stdlib::Move(src->Child[i]));
            IncKeyCount(1 + src->KeyCount);
        }

        void Copy(const BtreeNodePtr& src)
        {
            size_t i;

            for (i = 0; i < src->GetKeyCount(); i++)
            {
                SetKey(i, Stdlib::Move(src->GetKey(i)));
                SetValue(i, Stdlib::Move(src->GetValue(i)));
                SetChild(i, Stdlib::Move(src->Child[i]));
            }
            SetChild(i, Stdlib::Move(src->Child[i]));
            for (i = i + 1; i < 2 * T; i++)
            {
                Child[i].Reset();
            }

            SetLeaf(src->IsLeaf());
            SetKeyCount(src->GetKeyCount());
        }

        void ChildGiveKey(const BtreeNodePtr& self, size_t childIndex, bool left)
        {
            /* give child an extra key by moving a key from node
             * down into child, moving a key from child's
             * immediate left or right sibling up into node,
             * and moving the appropriate child pointer from the
             * sibling into child
             */

            if (BugOn(self.Get() != this))
                return;

            if (BugOn(childIndex < 0 || childIndex >= 2*T))
                return;

            size_t sibIndex = (left) ? (childIndex - 1) : (childIndex + 1);

            if (BugOn(sibIndex < 0 || sibIndex >= 2 *T))
                return;

            auto child = GetChild(childIndex);
            auto sib = GetChild(sibIndex);

            if (BugOn(child.Get() == nullptr))
                return;

            if (BugOn(sib.Get() == nullptr))
                return;

            if (BugOn(child.Get() == sib.Get()))
                return;

            if (!left)
            {
                child->CopyKey(child->GetKeyCount(), self, childIndex);

                CopyKey(childIndex, sib, 0);
                sib->DeleteKey(0);

                child->CopyChild(child->GetKeyCount() + 1, sib, 0);
                sib->DeleteChild(0);
            }
            else
            {
                if (BugOn(childIndex == 0))
                    return;

                if (BugOn(sib->GetKeyCount() == 0))
                    return;

                child->PutKey(0, self, childIndex - 1);

                CopyKey(childIndex -1, sib, sib->GetKeyCount() - 1);
                sib->DeleteKey(sib->GetKeyCount() - 1);

                child->PutChild(0, sib, sib->KeyCount);
                sib->DeleteChild(sib->KeyCount);
            }
            child->IncKeyCount();
            sib->DecKeyCount();
        }

        BtreeNodePtr ChildMerge(const BtreeNodePtr& self, size_t childIndex, bool left)
        {
            if (BugOn(self.Get() != this))
                return BtreeNodePtr();

            if (BugOn(childIndex < 0 || childIndex >= 2*T))
                return BtreeNodePtr();

            size_t sibIndex = (left) ? (childIndex - 1) : (childIndex + 1);

            if (BugOn(sibIndex < 0 || sibIndex >= 2 *T))
                return BtreeNodePtr();

            auto child = GetChild(childIndex);
            auto sib = GetChild(sibIndex);

            if (BugOn(child.Get() == nullptr))
                return BtreeNodePtr();

            if (BugOn(sib.Get() == nullptr))
                return BtreeNodePtr();

            if (BugOn(child.Get() == sib.Get()))
                return BtreeNodePtr();

            Trace(BtreeLL, "node 0x%p child %lu 0x%p merge sib %lu 0x%p",
                this, childIndex, child.Get(), sibIndex, sib.Get());

            if (left)
            {
                if (BugOn(childIndex == 0))
                    return BtreeNodePtr();

                sib->Merge(child,
                        Stdlib::Move(GetKey(childIndex-1)),
                        Stdlib::Move(GetValue(childIndex - 1)));
                DeleteKey(childIndex - 1);
                DeleteChild(childIndex);
            }
            else
            {
                child->Merge(sib,
                        Stdlib::Move(GetKey(childIndex)),
                        Stdlib::Move(GetValue(childIndex)));
                DeleteKey(childIndex);
                DeleteChild(childIndex + 1);
            }
            DecKeyCount();

            if (KeyCount == 0)
            {
                if (left)
                {
                    Copy(sib);
                }
                else
                {
                    Copy(child);
                }

                return self;
            }
            else
            {
                return (left) ? sib : child;
            }
        }

        BtreeNodePtr ChildBalance(const BtreeNodePtr& self, size_t childIndex)
        {
            Trace(BtreeLL, "node 0x%p child %lu balance", this, childIndex);

            if (BugOn(self.Get() != this))
                return BtreeNodePtr();

            if (BugOn(Leaf))
                return BtreeNodePtr();

            if (BugOn(childIndex < 0 || childIndex >= 2*T))
                return BtreeNodePtr();

            auto child = GetChild(childIndex);
            if (BugOn(child.Get() == nullptr))
                return BtreeNodePtr();

            if (child->KeyCount < T)
            {
                auto left = (childIndex > 0) ?
                    GetChild(childIndex - 1) : BtreeNodePtr();
                auto right = (childIndex < KeyCount) ?
                    GetChild(childIndex + 1) : BtreeNodePtr();

                if (left.Get() != nullptr && left->KeyCount >= T)
                {
                    ChildGiveKey(self, childIndex, true);
                }
                else if (right.Get() != nullptr && right->KeyCount >= T)
                {
                    ChildGiveKey(self, childIndex, false);
                }
                else if (left.Get() != nullptr && left->KeyCount < T)
                {
                    return ChildMerge(self, childIndex, true);
                }
                else if (right.Get() != nullptr && right->KeyCount < T)
                {
                    return ChildMerge(self, childIndex, false);
                }
                else
                {
                    if (BugOn(true))
                        return BtreeNodePtr();
                }
            }

            return child;
        }

        size_t GetChildCount()
        {
            size_t i, childCount;

            for (i = 0; i < 2 * T; i++)
            {
                if (Child[i].Get() == nullptr)
                    break;
            }

            childCount = i;
            for (; i < 2 * T; i++)
            {
                if (BugOn(Child[i].Get() != nullptr))
                    return 0;
            }

            return childCount;
        }

        BtreeNodePtr GetChild(size_t childIndex)
        {
            Trace(BtreeLL, "node 0x%p get child %lu", this, childIndex);

            if (BugOn(childIndex < 0 || childIndex >= 2*T))
                return BtreeNodePtr();

            return Child[childIndex];
        }

        K& GetKey(size_t keyIndex)
        {
            Trace(BtreeLL, "node 0x%p get key %lu", this, keyIndex);

            if (BugOn(keyIndex < 0 || keyIndex >= (2*T - 1)))
                return EmptyKey;

            if (BugOn(keyIndex >= KeyCount))
                return EmptyKey;

            return Key[keyIndex];
        }

        V& GetValue(size_t keyIndex)
        {
            Trace(BtreeLL, "node 0x%p get value %lu", this, keyIndex);

            if (BugOn(keyIndex < 0 || keyIndex >= (2*T - 1)))
                return EmptyValue;

            if (BugOn(keyIndex >= KeyCount))
                return EmptyValue;

            return Value[keyIndex];
        }

        size_t GetKeyCount()
        {
            return KeyCount;
        }

        bool Check(bool root)
        {
            if (KeyCount < 0 || KeyCount > (2 * T - 1))
            {
                Trace(BtreeLL, "node 0x%p key count %lu", this, KeyCount);
                return false;
            }

            if (!root)
            {
                if (KeyCount < (T - 1))
                {
                    Trace(BtreeLL, "node 0x%p key count %lu", this, KeyCount);
                    return false;
                }
            }

            K* prevKey = nullptr;
            for (size_t i = 0; i < KeyCount; i++)
            {
                if (prevKey != nullptr && *prevKey >= Key[i])
                {
                    Trace(BtreeLL, "node 0x%p prev key bigger %lu", this, i);
                    return false;
                }
                prevKey = &Key[i];
            }

            if (!Leaf)
            {
                for (size_t i = 0; i < KeyCount + 1; i++)
                {
                    if (Child[i].Get() == nullptr)
                    {
                        Trace(BtreeLL, "node 0x%p child %lu is null", this, i);
                        return false;
                    }

                    auto result = Child[i]->Check(false);
                    if (!result)
                        return false;
                }

                for (size_t i = KeyCount + 1; i < 2 * T; i++)
                {
                    if (Child[i].Get() != nullptr)
                    {
                        Trace(BtreeLL, "node 0x%p child %lu is NOT null", this, i);
                        return false;
                    }
                }
            }
            else
            {
                for (size_t i = 0; i < 2 * T; i++)
                {
                    if (Child[i].Get() != nullptr)
                    {
                        Trace(BtreeLL, "node 0x%p child %lu is NOT null", this, i);
                        return false;
                    }
                }
            }

            return true;
        }

    private:
        BtreeNode(const BtreeNode& other) = delete;
        BtreeNode(BtreeNode&& other) = delete;
        BtreeNode& operator=(const BtreeNode& other) = delete;
        BtreeNode& operator=(BtreeNode&& other) = delete;

        K Key[2 * T - 1];
        V Value[2 * T - 1];
        BtreeNodePtr Child[2 * T];

        size_t KeyCount;
        bool Leaf;

        K EmptyKey;
        V EmptyValue;
    };

    BtreeNodePtr Root;
    LockType Lock;
    K KeyToDelete;
    K EmptyKey;
    V EmptyValue;
};

}