# Open Issues — `nos` kernel code review

Review of each component, focusing on correctness bugs (medium depth). Each finding has been re-verified against the actual source. Severity reflects both impact and how readily it is triggered.

**Verification pass (2026-07-08)**: every issue was independently re-verified against the source by adversarial review. Outcomes are recorded per issue as a **Verified** line. Of the original 8 Critical/High: 6 confirmed, #3 downgraded to Medium (UAF not reachable with current callers), #46 refuted and moved to Retracted. Of the 77 Medium/Low: 76 remain open (a few with corrections noted inline — notably #16/#17, whose stated failure consequences were revised; #76, whose race is unreachable at current call sites; and #85, where one of two sub-claims was refuted); #29 was refuted and moved to Retracted.

- **Critical/High**: real, independently-triggerable bugs — fix first.
- **Medium**: real correctness deviations / latent bugs under specific conditions.
- **Low**: defense-in-depth gaps, fairness issues, or bugs that require caller misuse / crafted input.
- **Retracted**: one finding was a false positive and is documented as such at the end.

**Status convention**: each issue heading has a checkbox. Mark it `- [x]` when fixed (optionally add a commit/PR ref on the same line). A summary count of open vs. fixed is at the bottom of the file.

---

## Critical / High

### 1. [x] virtio_blk + virtio_scsi: TOCTOU race on `SlotByHead` — CRITICAL (fixed in 11c608e)
- **Files**: `src/cpp/drivers/virtio_blk.cpp:357-383` (submit), `:399-419` (complete); same pattern in `src/cpp/drivers/virtio_scsi.cpp`.
- **Summary**: `SlotByHead[head]` bookkeeping is performed *outside* `VirtQueueLock`, while the descriptor free/reuse it depends on happens *inside* the lock. `CompleteIO` (IRQ context) frees the descriptor chain via `GetUsed` under the lock (`virtio_blk.cpp:399-401`), releases the lock, then reads/nulls `SlotByHead[usedId]` (`:412-419`). `DrainQueue` (softirq, another CPU) can take the lock, `AddBufs`-reuse the just-freed head index (`virtqueue.cpp:171-184` returns freed descriptors to `FreeHead`), and write `SlotByHead[head] = &new_slot` for a *new* request (`virtio_blk.cpp:382-383`).
- **Failure scenario**: On SMP under I/O load, a completion calls `req->Completion.Done()` on the wrong request (premature unblock → buffer reused while the device is still DMA-ing it → data corruption), or the real completion is lost (`SlotByHead` nulled by the wrong CPU → permanent `WaitForCompletion` hang).
- **Fix**: do the `SlotByHead` lookup/null/`FreeSlot`/`req` extraction under the same critical section as `GetUsed`; move the `DrainQueue` `SlotByHead[head]=` write inside its lock.
- **Verified (2026-07-08)**: CONFIRMED. `CompleteIO` reads/nulls `SlotByHead[usedId]` at `virtio_blk.cpp:412/:419` after releasing the lock taken for `GetUsed` (`:399-401`); `DrainQueue` writes `SlotByHead[head]` at `:383` outside the lock (`AddBufs` under lock at `:357-359`). Same in `virtio_scsi.cpp` (`:533-535/:546/:553`, `:459-461/:485`). Nuance: the free chain is LIFO and blk/scsi chains are ≥2 descriptors, so the just-freed head index is not handed out by the very next `AddBufs` (the chain tail resurfaces first) — same-index reuse needs an intervening allocation within the race window, and QEMU's ioeventfd kick ordering masks the completion-before-publish variant. The unsynchronized access and both failure modes remain real, but it is harder to trigger than the write-up implies.
- **Fixed (2026-07-09)**: `SlotByHead` publish (`DrainQueue`) and claim/null (`CompleteIO`) now run inside the same `VirtQueueLock` critical sections as `AddBufs`/`GetUsed`, in both virtio_blk and virtio_scsi.

### 2. [x] virtio_net RX: descriptor/buffer index swap after recycling — HIGH (fixed in 11c608e)
- **Files**: `src/cpp/drivers/virtio_net.cpp:269-277, 431-469`; `src/cpp/drivers/virtqueue.cpp:93-131, 160-187`.
- **Summary**: RX assumes `usedId == buffer index`. This holds only for the initial linear `PostAllRxBufs()` posting. `GetUsed` returns freed descriptors to `FreeHead` LIFO (`virtqueue.cpp:178-179`); `AddBufs` allocates from `FreeHead` (`:98`). When two buffers are reaped and recycled in one softirq pass, the free chain reverses (0→1 becomes 1→0), so `PostRxBuf(0)` ends up in descriptor 1 and vice-versa. Next completion of descriptor 0 reads `RxBufs[0]`/`RxFrames[0]` — the wrong buffer (`virtio_net.cpp:455-457`).
- **Failure scenario**: Corrupts received packets on every multi-packet RX batch. Easily triggered.
- **Fix**: store the buffer index in the descriptor's `id`/opaque field (carried through the used ring) rather than assuming descriptor id == buffer index.
- **Verified (2026-07-08)**: CONFIRMED. Swap traced exactly: reaping descriptors 0 then 1 in one pass leaves `FreeHead = 1→0` (LIFO push at `virtqueue.cpp:178-179`); frames are reposted in FIFO order, so `PostRxBuf(0)` lands in descriptor 1 and vice versa — the desc↔buffer mapping inverts permanently. `PostRxBuf` (`virtio_net.cpp:269-277`) uses the index only for the DMA address; nothing carries it through the used ring, and no re-sync mechanism exists. Single-packet passes stay consistent by luck; any ≥2-buffer reap before reposting permutes the mapping.
- **Fixed (2026-07-09)**: added an `RxBufByDesc[]` descriptor→buffer map, written when `PostRxBuf` posts a buffer (from the head `AddBufs` returns) and consulted by `ReapRx` instead of assuming `usedId == buffer index`.

### 4. [x] TCP: SYN-RECEIVED half-open connection pool leak — HIGH (fixed in 11c608e)
- **Files**: `src/cpp/net/tcp.cpp:1149-1155` (retransmit), `:1207-1214` (deadline reset), `:1228` (cleanup).
- **Summary**: A passive connection whose final ACK is lost retransmits SYN-ACK forever. `retransmitted=true` (`:1154`) doubles `RtoMs` capped at `TcpMaxRtoMs` (`:1210-1212`) and **always** resets `RetransmitDeadlineMs = now + RtoMs` (`:1214`) — no retry counter, no transition out of SYN-RECEIVED. Cleanup at `:1228` only reclaims `State == Closed && !OwnedByApp`; SYN-RECEIVED never reaches Closed, and `OwnedByApp`/`Accepted` are false so `Accept` never returns it.
- **Failure scenario**: ~64 half-open SYNs permanently exhaust `TcpMaxConnections`. Trivial DoS from a single unresponsive peer.
- **Fix**: bounded SYN-ACK retry count → transition to `Closed` → cleanup reclaims the slot.
- **Verified (2026-07-08)**: CONFIRMED. `TcpConn` has no retransmit-count field at all (`tcp.h:140-188`); the only timeout transition in `ProcessRetransmits` is TimeWait → Closed. Only an in-window RST rescues the slot — a silent peer leaks it forever. `TcpMaxConnections = 64` (`tcp.h:31`).
- **Fixed (2026-07-09)**: added `TcpConn::RetransmitCount` + `TcpMaxRetransmits` (8). After 8 consecutive retransmits with no ACK progress the connection is aborted to `Closed` (`NeedCleanup` set, waiters woken via `ConnReady`/`DataReady`) and the slot reclaimed by the cleanup pass. The counter resets on any ACK progress. Applies to every retransmit path (SYN, SYN-ACK, FIN states, Established data) — also fixes #45.

### 5. [x] NanoFs: old data blocks freed before inode commit — HIGH (fixed in 11c608e)
- **File**: `src/cpp/fs/nanofs.cpp:1050-1062` (`Write`, success path).
- **Summary**: On overwrite, `FreeDataBlock(inode->Blocks[i])` (`:1051-1052`) flushes the bitmap **before** `WriteInode(inodeIdx, inode)` (`:1062`) commits the new inode. `FreeDataBlock`→`FlushSuper` persists the bitmap (FUA).
- **Failure scenario**: Crash in that window leaves the on-disk inode referencing blocks marked free in the bitmap; a later `AllocDataBlock` hands one to a different file → cross-file corruption. The new data is also orphaned (allocated in the bitmap but unreferenced).
- **Fix**: write the new inode (pointing at new blocks) before freeing old blocks, or use a journal / ordering discipline.
- **Verified (2026-07-08)**: CONFIRMED. Order traced: `FreeDataBlock` (`:1052`) → `FlushSuper` FUA-commits the bitmap (`:292/:343`, FUA propagates through `BlockIo::WriteBlock` → `WriteSectors` → device flush) strictly before the non-FUA `WriteInode` (`:1062`). Both failure modes (cross-file aliasing of still-referenced blocks + orphaned new blocks) are real.
- **Fixed (2026-07-09)**: `Write` (overwrite and truncate paths) saves the old block list, commits the new inode via `WriteInode` first, and only then frees the old blocks; on inode-commit failure the new blocks are rolled back instead. A crash in the window now leaks blocks (bitmap-used, unreferenced) rather than cross-linking files. Also clamped `oldBlockCount` to `NanoMaxBlocks` (partial hardening for #13).

### 44. [x] r8168 RX is never dispatched — `process_rx` is dead code — HIGH (fixed in 11c608e)
- **Files**: `src/rust/drivers/r8168/src/lib.rs:400` (ISR raises `TYPE_NET_RX`), `src/cpp/drivers/virtio_net.cpp:736-744, 792-796` (`NetRxSoftIrqHandler` + registration), `src/cpp/kernel/rust_ffi.cpp:1126-1129, 1145-1170` (`RustNetDevice::ProcessRx` + `kernel_netdev_register`).
- **Summary**: The r8168 ISR raises `softirq::raise(TYPE_NET_RX)` on RX, and `TYPE_NET_RX == SoftIrq::TypeNetRx == 0` (`softirq.rs:1`, `softirq.h:31`). But the `TypeNetRx` softirq handler is `NetRxSoftIrqHandler` (`virtio_net.cpp:736`), registered only `if (VirtioNet::InstanceCount > 0)` (`:794`), and it iterates **only** `VirtioNet::Instances` (`:740-744`) — it never touches `NetDeviceTable`. `RustNetDevice` is registered into `NetDeviceTable` (`rust_ffi.cpp:1166`) with a `ProcessRx()` override (`:1126`) that calls `r8168_process_rx`, but nothing ever invokes it. The driver's own header comment (`lib.rs:11-13, 399`) describes a dispatch mechanism that does not exist in the C++ layer.
- **Failure scenario**: On any system with an r8168 NIC (Hetzner EX44/AX41 per the driver header), the hardware receives packets into the RX descriptor ring but the host never harvests them. `r8168_process_rx` is dead code; the RX ring fills up and all inbound traffic is silently dropped. TX still works (called directly via `SubmitTx`→`FlushTx`). On an r8168-only system `TypeNetRx` is never registered at all, so `softirq::raise` is a no-op.
- **Fix**: `NetRxSoftIrqHandler` (or a new unified handler) must iterate `NetDeviceTable` and call `ProcessRx()` on every registered device, not just `VirtioNet::Instances`. Since `SoftIrq::Handlers[]` allows one handler per type, the virtio-net and Rust-net RX paths must share a single handler.
- **Caveat**: Root cause is in `virtio_net.cpp`, but the bug manifests entirely through the r8168 driver. Does not affect virtio-net RX.
- **Verified (2026-07-08)**: CONFIRMED. Full-tree grep: the only `ProcessRx()` call site is the `VirtioNet::Instances` loop (`virtio_net.cpp:743`); `NetDeviceTable::Devices[]` is never iterated for RX (its only users are `Register/Find/Dump`). `SoftIrq::Handlers[]` holds one handler per type, and registration is conditional on `VirtioNet::InstanceCount > 0`, so on an r8168-only system `raise(TYPE_NET_RX)` is a no-op exactly as claimed. TX works via the synchronous `SubmitTx` → `FlushTx` path, confirming the asymmetry.
- **Fixed (2026-07-09)**: the `TypeNetRx`/`TypeNetTx` softirq handlers moved to `NetDeviceTable` (registered when the first device registers) and dispatch to every registered device via new `NetDevice::ReapRx()`/`DrainTx()` virtuals (defaults: no-op reap, flush-TxQueue-under-lock drain). `RustNetDevice::ProcessRx` → `r8168_process_rx` is now reachable, and handler registration no longer depends on virtio-net being present.

### 45. [x] TCP: FIN retransmit has no retry limit — FIN-WAIT-1 / LAST-ACK / CLOSING permanent slot leak — HIGH (fixed in 11c608e)
- **File**: `src/cpp/net/tcp.cpp:1157-1214`.
- **Summary**: The FIN retransmit path for `FinWait1`, `LastAck`, and `Closing` is the exact analogue of #4 (SYN-RECEIVED): `ProcessRetransmits` retransmits the FIN, doubles `RtoMs` capped at `TcpMaxRtoMs` (`:1210-1212`), and **always** resets `RetransmitDeadlineMs = now + RtoMs` (`:1214`) with no retry counter and no transition to `Closed`. The slot is never reclaimed (cleanup at `:1228` only handles `State == Closed`).
- **Failure scenario**: Local side calls `Close()` (sends FIN); the peer crashes or the network partitions before ACKing. The FIN is retransmitted every 8 s forever. ~8 such teardowns exhaust `TcpMaxConnections` (64). Unlike the Established data-retransmit case (`:1187-1205`), the app has already called `Close()` and cannot escape.
- **Fix**: bounded FIN retry count → transition to `Closed` → cleanup reclaims the slot (mirror the fix for #4).
- **Caveat**: Distinct from #4 (SYN-RECEIVED only). Related to the FIN-WAIT-2 / persist gaps (#53, #54).
- **Verified (2026-07-08)**: CONFIRMED, and broader than filed. One correction: the contrast drawn with the Established data-retransmit path (`:1187-1205`) is wrong — that path has the **identical uncapped structure** (no retry limit, no abort, no `State = Closed` anywhere in `ProcessRetransmits`), so Established/CloseWait connections with unacked in-flight data to a dead peer leak slots the same way. `Close()` sets `OwnedByApp = false` for every state (`:1090`), so the app has genuinely released the conn and cannot escape.
- **Fixed (2026-07-09)**: same mechanism as #4 — the `RetransmitCount` cap covers the FIN states and the Established/CloseWait data path, aborting to `Closed` after `TcpMaxRetransmits` consecutive no-progress retransmits.

---

## Medium

### 3. [x] TimerTable: cross-CPU data race (UAF latent, not currently reachable) — MEDIUM (downgraded from HIGH) (fixed in d57f74e)
- **Files**: `src/cpp/kernel/timer.cpp` (whole file); call site `src/cpp/kernel/cpu.cpp:486-488`; callers `src/cpp/net/tcp.cpp:79`, `src/cpp/drivers/8042.cpp:40`, `src/cpp/kernel/rust_ffi.cpp:842/865`.
- **Summary**: `StartTimer`/`StopTimer`/`ProcessTimers` have **zero synchronization** (no lock, no IRQ disable, no atomics). `ProcessTimers()` runs in hard-IPI context on CPU 0 (`Cpu::IPI` → `cpu.cpp:488`), while `StartTimer`/`StopTimer` run in task context on any CPU. Plain data race on every field of `Timer[i]` (e.g. the `Expired += Period` RMW at `timer.cpp:61` races concurrent `StartTimer`/`StopTimer` writes); a handler can fire after `StopTimer` returns.
- **Verified (2026-07-08)**: downgraded from HIGH. The data race is real, but the originally-claimed use-after-free is **not reachable with current callers**: all three `TimerCallback` implementers are never freed (`Tcp` and `IO8042` are never-stopped singletons; `RustTimerAdapter` lives in a static buffer and re-checks its `Handler` under `RustTimerLock` with a null guard, `rust_ffi.cpp:795-804`). Any future heap-allocated callback freed after `StopTimer` becomes a genuine UAF, since `StopTimer` does not synchronize with an in-flight `ProcessTimers`.
- **Fix**: a lock (or IRQ-disable + seqlock) around timer array mutation; ensure `StopTimer` synchronizes with any in-flight `ProcessTimers`.
- **Fixed (2026-07-09)**: the timer array is now guarded by a `RawSpinLock` (IRQ-save) in `StartTimer`/`StopTimer`/`ProcessTimers`; callbacks run outside the lock with a per-slot `Running`/`RunningCpu` marker, and `StopTimer` spin-waits for an in-flight callback on another CPU before returning (skipping the wait when called from inside the callback itself).

### 6. [x] TCP FIN-WAIT-1 / FIN-WAIT-2 drop all incoming data (fixed in 5e37ea5)
- **File**: `src/cpp/net/tcp.cpp:529-583`.
- **Summary**: Neither state has a payload-processing branch; only ACK/FIN are handled. Any data the peer sends after our local `Close()` is silently discarded and never ACKed (RFC 793 half-close violation).
- **Scenario**: Local side closes before the peer finishes sending; peer's data segments are dropped, peer retransmits until RST. (Does not affect the in-tree HTTP client, which recvs to EOF before close.)
- **Fix**: add a `payloadLen > 0 && seq == RcvNxt` branch that writes to `RecvBuf` and ACKs, as in the `Established` case.
- **Verified (2026-07-08)**: CONFIRMED. Neither state has a `payloadLen > 0` branch; a pure-data segment isn't even ACKed (only a FIN triggers `SendSegment` at `:546/:576`).
- **Fixed (2026-07-09)**: FIN-WAIT-1/2 deliver in-order payload to `RecvBuf` and ACK it via the new shared `ProcessPayload` helper (also used by Established).

### 7. [x] TCP FIN-WAIT-1 / FIN-WAIT-2 / CLOSING compute `RcvNxt` wrong on FIN+data (fixed in 5e37ea5)
- **File**: `src/cpp/net/tcp.cpp:543, 573`.
- **Summary**: When a segment carries both data and FIN, `RcvNxt = seq + 1` rather than `seq + payloadLen + 1`.
- **Scenario**: The ACK we send back mis-states next-expected sequence number, causing retransmit loops / desync. Compounds #6.
- **Fix**: `conn->RcvNxt = seq + (u32)payloadLen + 1`.
- **Verified (2026-07-08)**: CONFIRMED at `:543/:573`. Nit: the CLOSING case (`:584-596`) never assigns `RcvNxt` itself — it is reached via FIN-WAIT-1's already-buggy `:543`, so the two cited lines are the actual defects.
- **Fixed (2026-07-09)**: the FIN branches in FIN-WAIT-1/2 now require the FIN in-order (`seq + payloadLen == RcvNxt`) and set `RcvNxt = seq + payloadLen + 1`; CLOSING inherits the fix via FIN-WAIT-1.

### 8. [x] TCP: no receive-window update ACK after app drains `RecvBuf` (fixed in 5e37ea5)
- **Files**: `src/cpp/net/tcp.cpp:1009-1045` (`Recv`), `:489-527` (`Established`).
- **Summary**: `RcvWnd` is refreshed inside `Recv` and advertised only on data-path ACKs; there is no path that sends an unsolicited window-update ACK when the buffer is drained.
- **Scenario**: We advertise a near-zero window, the app reads everything out, but the peer is never told the window reopened. Throughput collapses to one byte per zero-window probe interval.
- **Fix**: send a window-update ACK when the window grows by ≥1 MSS or to half the buffer.
- **Verified (2026-07-08)**: CONFIRMED. `Recv` refreshes `RcvWnd` at `:1024` but never sends; the window is only advertised on reactive segments (`SendSegment`, `:258`), and `ProcessRetransmits` sends no updates either.
- **Fixed (2026-07-09)**: `SendSegment` records `AdvertisedWnd`; `Recv` sends an unsolicited window-update ACK when the last advertised window was < 1 MSS and draining reopened it to >= 1 MSS (Established/FIN-WAIT-1/FIN-WAIT-2).

### 9. [x] kvmclock enabled only on the BSP; APs read the BSP's pvclock page (fixed in d57f74e)
- **Files**: `src/cpp/kernel/tsc.cpp:149-213` (`SetupKvmClock`, `KvmClockTime`), `src/cpp/kernel/tsc.h:26-28` (singleton), `src/cpp/kernel/main.cpp:504` (`TimeInit` only in `BpStartup`).
- **Summary**: `Tsc` is a singleton; `SetupKvmClock` writes `MSR_KVM_SYSTEM_TIME` once on the BSP and stores one shared `PvClock` page. APs (`ApMain2`) never write the MSR, yet `Tsc::GetTime()` returns `KvmClockTime()` on every CPU whenever `KvmClockAvail` is set. APs compute `AP_ReadTsc() − BSP_TscTimestamp` against a page KVM updates only for the BSP vcpu.
- **Scenario**: AP time queries (`GetBootTime` via `Sleep`, runtime accounting) can drift or jump; correctness relies on TSCs being perfectly synchronized across vcpus (not guaranteed by KVM).
- **Fix**: each AP needs its own pvclock page + MSR write (per-vcpu pvclock).
- **Verified (2026-07-08)**: CONFIRMED. `TimeInit` is called only from `BpStartup` (`main.cpp:504`); `ApMain2`/`ApStartup` never write the MSR, yet `GetTime()` (`tsc.cpp:215-224`) uses the shared pvclock page on every CPU.
- **Fixed (2026-07-09)**: the pvclock page now holds one 32-byte entry per APIC id; `EnableKvmClockSelf()` writes `MSR_KVM_SYSTEM_TIME` for the calling CPU (BSP in `SetupKvmClock`, APs in `ApMain2` right after `Lapic::Enable`), and `KvmClockTime` reads the current CPU's entry. A never-armed entry yields 0, which `GetBootTime` treats as fall-back-to-HPET/PIT.

### 10. [x] lib: `TokenCopy` underflow when `dstSize == 0` (fixed in 3dd96d7)
- **File**: `src/cpp/lib/stdlib.cpp:252-260`.
- **Summary**: No guard for `dstSize == 0`. `len = dstSize - 1` underflows to `ULONG_MAX`, then `MemCpy` overrun + OOB write at `dst[ULONG_MAX]`.
- **Scenario**: Any caller that computes buffer size dynamically and passes 0.
- **Fix**: early `return 0;` when `dstSize == 0`.
- **Verified (2026-07-08)**: CONFIRMED, latent: every current caller (cmd.cpp, test.cpp) passes `sizeof(fixedArray)` — none can pass 0 today.
- **Fixed (2026-07-09)**: early `return 0` when `dstSize == 0`.

### 11. [x] lib: `Vector::ReserveAndUse` sets `Size = Capacity` instead of `Size = capacity` (fixed in 3dd96d7)
- **File**: `src/cpp/lib/vector.h:59-65`.
- **Summary**: `Reserve(capacity)` only allocates when `capacity > Capacity`; if the vector already has `Capacity > capacity`, `Reserve` returns true without changing `Capacity`, and `Size = Capacity` exposes stale/uninitialized elements at `[capacity..Capacity)`.
- **Scenario**: `ReserveAndUse(10)` on a vector with `Capacity == 20` sets `Size = 20`, exposing 10 stale elements.
- **Fix**: `Size = capacity`.
- **Verified (2026-07-08)**: CONFIRMED, latent: all three callers (test.cpp) invoke it on freshly-constructed vectors (`Capacity == 0`), so the stale-exposure path is currently unreachable.
- **Fixed (2026-07-09)**: `Size = capacity`.

### 12. [x] NVMe `Drop`: hardcoded 100ms disable timeout (fixed in 3dd96d7)
- **File**: `src/rust/drivers/nvme/src/lib.rs:97`.
- **Summary**: Shutdown uses `wait_csts_clear(&self.regs, CSTS_RDY, 100)` instead of the controller-reported `to_ms` (up to ~127s; `MIN_TIMEOUT_MS = 5000`) used during init.
- **Scenario**: If the controller takes >100ms to clear CSTS.RDY, the driver frees queue DMA buffers while the controller may still be fetching/posting entries → DMA into freed memory.
- **Fix**: use `to_ms` (capped at `MIN_TIMEOUT_MS`) as in init.
- **Verified (2026-07-08)**: CONFIRMED. Correction: `MIN_TIMEOUT_MS` is a floor applied when `CAP.TO == 0`, not a cap — init's effective timeout can be up to ~127 s, making the 100 ms Drop value even more of an outlier. Drop disarms MSI-X then implicitly frees queue DMA on scope exit.
- **Fixed (2026-07-09)**: `NvmeDevice` stores the CAP.TO-derived init timeout (`disable_timeout_ms`) and `Drop` uses it instead of the hardcoded 100 ms.

### 13. [x] NanoFs: `Write`/`RemoveRecursive` trust on-disk `Size` without clamp (fixed in 315329f)
- **Files**: `src/cpp/fs/nanofs.cpp:960-966` (truncate path), `:1050-1052` (overwrite path), `:1210-1214` (`RemoveRecursive`).
- **Summary**: `blockCount = (inode->Size + NanoBlockSize - 1) / NanoBlockSize` is computed from the just-read on-disk inode and `Blocks[i]` iterated up to `blockCount` with no clamp to `NanoMaxBlocks` (256). A corrupted/crafted inode with `Size > NanoMaxFileSize` makes `blockCount > 256`, walking past `Blocks[256]` into `Padding` and calling `FreeDataBlock` on garbage → arbitrary data-bitmap corruption.
- **Caveat**: the missing clamp is confirmed; whether `ReadInode` validates the checksum first was not verified. If it does, the exploit requires a forged-but-valid checksum. Either way the clamp is a real defense-in-depth gap.
- **Fix**: clamp `blockCount` to `NanoMaxBlocks` and verify the inode checksum before trusting `Size`/`Blocks`.
- **Verified (2026-07-08)**: CONFIRMED, and the open caveat is resolved: `ReadInode` does **not** verify the checksum, and neither `Write` nor `RemoveRecursive` calls `VerifyInodeChecksum` afterward — no forged CRC is needed. `FreeDataBlock` does clamp `idx >= NanoDataBlockCount`, so only in-range garbage corrupts the bitmap, but the OOB `Blocks[]`/`Padding` walk stands.
- **Fixed (2026-07-09)**: `Write` and `RemoveRecursive` verify the inode checksum before trusting `Size`/`Blocks` (`Write` fails; `RemoveRecursive` skips block freeing, leaking rather than corrupting), and `RemoveRecursive` clamps `blockCount` to `NanoMaxBlocks` (the `Write` clamp landed with #5).

### 14. [x] NanoFs: `Write` never flushes (fixed in 315329f)
- **File**: `src/cpp/fs/nanofs.cpp:931-1072` (`Write`).
- **Summary**: `Write` never calls `Io.Flush()`; data block writes (`:1026`) and the final `WriteInode` (`:1062`) are issued without FUA/flush, even though the bitmap (via `FlushSuper`) is persisted.
- **Scenario**: Crash without clean unmount can lose file data + inode update despite the bitmap showing them allocated.
- **Caveat**: no `Io.Flush()` is visible in the success path; whether `WriteInode`/`WriteBlock` internally flush/FUA was not verified.
- **Fix**: issue a device flush (or FUA) on the data and inode writes, or at the end of `Write`.
- **Verified (2026-07-08)**: CONFIRMED, caveat resolved: `BlockIo::WriteBlock` just forwards `fua` (default false) to `Dev->WriteSectors` with no internal flush, so data and inode writes are genuinely unflushed while the bitmap is FUA-committed.
- **Fixed (2026-07-09)**: `Write` flushes the device write cache after the data-block writes and commits the inode with FUA (both truncate and overwrite paths), matching the already-FUA'd bitmap.

### 15. [x] ext2: division by zero on crafted superblock (fixed in 315329f)
- **Files**: `src/cpp/fs/ext2.cpp:137` (`Mount`), `:259-260` (`ReadInode`).
- **Summary**: `GroupCount = (BlockCount + BlocksPerGroup - 1) / BlocksPerGroup` and `group = (inodeNum - 1) / InodesPerGroup` are computed without checking that `BlocksPerGroup` / `InodesPerGroup` are non-zero.
- **Scenario**: Mount a crafted/corrupt ext2 image with either field set to 0 → kernel division exception.
- **Fix**: reject the image (or `goto fail`) if `BlocksPerGroup == 0` or `InodesPerGroup == 0`.
- **Verified (2026-07-08)**: CONFIRMED. Both sites are independently triggerable (`InodesPerGroup == 0` crashes in `ReadInode(2)` during mount even if `BlocksPerGroup` is valid).
- **Fixed (2026-07-09)**: `Mount` rejects images with `BlocksPerGroup == 0` or `InodesPerGroup == 0`.

### 16. [x] virtio-blk shared-INTx stub unconditionally EOIs (fixed in dc9d947)
- **File**: `src/cpp/drivers/virtio_blk.cpp:606-616`.
- **Summary**: The global stub loops over all instances calling `Interrupt()`, then unconditionally calls `Lapic::EOI()`. Each instance's `Interrupt()` returns early if its ISR reads 0. If the IRQ was shared with another device and no virtio-blk instance claimed it, the stub still EOIs the LAPIC, stealing the interrupt.
- **Scenario**: On legacy INTx (supported at `:186-189`) with a genuinely shared IRQ, drops another device's interrupt. Harmless under MSI-X.
- **Fix**: only EOI if some instance claimed the interrupt; otherwise let the shared-dispatch framework handle it.
- **Verified (2026-07-08)**: PARTIAL. The code claims are true, but the stated consequence is largely refuted: `Interrupt::RegisterLevel` chains handlers by GSI and swaps the IDT entry to `SharedInterruptStub` once a GSI has ≥2 handlers (`interrupt.cpp:91`), so the EOI-ing blk stub is only installed while virtio-blk solely owns its vector — where the EOI is correct. Genuine sharing (e.g. blk+scsi on one GSI) dispatches via `SharedDispatch`, which EOIs itself (`:137`). Theft would require a co-located device registering via the non-GSI-tracking `Interrupt::Register` path — a narrow edge case.
- **Reviewed (2026-07-09), no code change**: the suggested fix (skip the EOI) is unsafe -- whenever the blk stub is installed it is the vector's sole IDT owner, and a LAPIC EOI is mandatory for any vector taken in service; skipping it would leave the in-service bit set and block further interrupts at that priority. Genuine sharing dispatches via `SharedDispatch` (which EOIs itself), per the verification note. Left open as a latent design wart only.
- **Fixed (2026-07-09)**: closed by enforcing the invariant the EOI depends on, rather than touching the EOI. The only remaining theft path was a device registered via the non-GSI-tracking `Interrupt::Register` edge path silently coexisting on the blk stub's GSI/vector. `Register` now records its GSI + vector in the shared `Vectors[]` table and refuses an already-owned GSI or vector; `RegisterLevel` refuses to chain onto an edge-owned GSI and refuses an occupied fresh vector instead of silently overwriting the IDT entry. The blk stub's unconditional EOI is thereby always correct: whenever it is installed, it is provably the sole owner of its vector and GSI, and any future conflict surfaces as a loud trace at driver init instead of dropped interrupts at runtime.

### 17. [x] virtio `SlotByHead` arrays sized to `MaxDescriptors=256`, queue size unclamped (fixed in 3dd96d7)
- **Files**: `src/cpp/drivers/virtqueue.h:84` (`MaxDescriptors = 256`), `src/cpp/drivers/virtqueue.cpp:29-31` (no clamp on `queueSize`), `src/cpp/drivers/virtio_blk.h:111`, `src/cpp/drivers/virtio_net.h:141` (`SlotByHead[MaxDescriptors]`), `src/cpp/drivers/virtio_blk.cpp:373` (post-hoc bounds check), `src/cpp/drivers/virtio_scsi.cpp:475`.
- **Summary**: Virtio queues can be up to 32768. `VirtQueue::Setup` does not clamp `queueSize` to 256. If the device reports a queue size >256, `AddBufs` returns a head ≥256; the bounds check at `virtio_blk.cpp:373` rejects it **after** the descriptors have already been consumed from the free chain and published in the avail ring. The completion path drops it too, so those descriptors are never returned → permanent descriptor leak.
- **Scenario**: Latent under QEMU defaults (queue size 256); real for larger queue sizes.
- **Fix**: clamp `queueSize` to `MaxDescriptors` in `Setup`, or size `SlotByHead` to the actual queue size.
- **Verified (2026-07-08)**: PARTIAL. Structure confirmed (no clamp; bounds check after the descriptors are published), but the "permanent descriptor leak" is refuted: `GetUsed` unconditionally returns the whole chain to the free list before `CompleteIO` drops the unknown id, so descriptors are reclaimed on completion. The real bug is worse in kind: a head ≥256 request is failed and its slot freed at submit time while it is already published in the avail ring — the device will still DMA into the freed/reused buffer. Still latent under QEMU (queue size 256).
- **Fixed (2026-07-09)**: `VirtQueue::Setup` fails for `queueSize == 0` or `> MaxDescriptors` (256), refusing the device instead of mis-driving it -- in legacy mode the size cannot be negotiated down, so clamping would desynchronize the ring layout from the device.

### 47. [x] mm: `ExcludeFreePages` permanently leaks ~1-2% of low physical RAM — MEDIUM (fixed in d57f74e)
- **File**: `src/cpp/mm/page_table.cpp:414-433` (function), `:448` (call), `:507-524` (`SetupFreePagesList`).
- **Summary**: `ExcludeFreePages(VirtToPhys(pageArrayLimit))` (`:448`) unlinks **every** free page whose physical address is below `pageArrayLimit` (the end of the `PageArray`) from the early singly-linked free list. Only the `PageArray`'s own backing range needs excluding (to avoid `PhysToVirt` aliasing during `Setup`), but the function excludes everything below the limit — including usable RAM between the kernel image and the `PageArray` and the TmpMap range. `SetupFreePagesList` (`:507-524`) draws from the same pruned `FreePages` list via `GetFreePageByTmpMap`, so those pages are never recovered.
- **Failure scenario**: For a 2 GB QEMU VM with a ~2 MB kernel, `pageArrayLimit ≈ 22 MB`, leaking ~20 MB of low RAM; for 512 MB, ~8 MB. Permanently unavailable to the page allocator for the kernel's lifetime.
- **Fix**: exclude only `[VirtToPhys(PageArray), VirtToPhys(pageArrayLimit))` (a range, not "everything below"), and after `SetCr3` re-scan e820 and add the previously-excluded usable pages back to `FreePagesList`.
- **Verified (2026-07-08)**: CONFIRMED. The unlink test is `if (curr < phyLimit)` — everything below, not a range; `FreePagesList` is populated only from the already-pruned `FreePages` (`:518`) and runtime `FreePage` (`:623`), with no e820 re-scan. Permanent leak stands.
- **Fixed (2026-07-09)**: `ExcludeFreePages` parks below-limit pages on a side list (`ExcludedPages`) instead of dropping them; `SetupFreePagesList` drains that list into `FreePagesList` after the main list. Safe because all runtime page access goes through TmpMap; `Setup`'s own identity-accessed allocations still come exclusively from above the limit.

### 48. [x] NanoFs: `RemoveDirEntry` trusts on-disk dir `Size` without clamp or checksum — MEDIUM (fixed in 315329f)
- **File**: `src/cpp/fs/nanofs.cpp:617-704`.
- **Summary**: `RemoveDirEntry` reads the dir inode fresh via `ReadInode` (`:626`) with **no** `VerifyInodeChecksum` (unlike `LoadVNode` at `:442`), then iterates `for (u32 i = 0; i < dirInode->Size; i++)` (`:668`) and writes `entries[dirInode->Size - 1]` (`:677`) with no clamp to `NanoMaxDirEntries` (256) or the 4096-byte `dirBuf` capacity (512 entries). `AddDirEntry` (`:562`) and `LoadVNode` (`:490`) both clamp; only `RemoveDirEntry` is missing the guard.
- **Failure scenario**: A crafted NanoFs image with a dir inode (valid CRC32 — CRC32 is not cryptographically secure, so an attacker can forge it) with `Size = 1000` → `RemoveDirEntry` reads/writes `entries[0..999]` past the 4096-byte heap buffer → heap corruption or crash.
- **Fix**: clamp `dirInode->Size` to `NanoMaxDirEntries` and call `VerifyInodeChecksum(dirInode)` before trusting `Size`/`Blocks[0]`.
- **Caveat**: Sibling of #13 (file-block-count in `Write`/`RemoveRecursive`); this is dir-entry-count in `RemoveDirEntry`.
- **Verified (2026-07-08)**: CONFIRMED. The asymmetry is real: `AddDirEntry` (`:562`) and `LoadVNode` (`:490`) clamp; only `RemoveDirEntry` lacks both the clamp and the checksum verify. `Size > 512` produces OOB heap read/write past the 4096-byte `dirBuf`.
- **Fixed (2026-07-09)**: `RemoveDirEntry` verifies the dir inode checksum and rejects `Size > NanoMaxDirEntries` before iterating.

### 49. [x] NanoFs: `LoadVNode` has no recursion depth limit → stack overflow at mount — MEDIUM (fixed in 315329f)
- **File**: `src/cpp/fs/nanofs.cpp:410-508` (recursive at `:492`).
- **Summary**: `LoadVNode` recurses for every directory entry. The `VNodes[inodeIdx] != nullptr` guard (`:418`) bounds total work to `NanoInodeCount` (1024) but not recursion **depth**. The dir buffer is heap-allocated (`:473`), so each frame is small (~150-250 bytes), but a chain of 1024 nested single-entry directories still recurses ~1024 deep → overflows the 32 KB kernel stack (≈ frame 160).
- **Failure scenario**: Crafted NanoFs image with a deeply nested directory chain. `Mount` → `LoadVNode(0)` → stack overflow → panic at mount.
- **Fix**: convert the directory-tree load to an iterative worklist, or cap recursion depth and return an error.
- **Verified (2026-07-08)**: CONFIRMED. Kernel stack is 32 KB (`Task::StackSize = 8 * PageSize`, task.h:20; boot stacks likewise); the `VNodes[]` guard bounds work to 1024 inodes but not depth.
- **Fixed (2026-07-09)**: `LoadVNode` takes a depth parameter capped at `NanoMaxDirDepth` (32).

### 50. [x] NanoFs: directory cycle in `LoadVNode` → `RemoveRecursive` infinite recursion — MEDIUM (fixed in 315329f)
- **Files**: `src/cpp/fs/nanofs.cpp:418, 489-501` (cycle forms), `:1174-1230` (`RemoveRecursive`).
- **Summary**: `LoadVNode` sets `VNodes[inodeIdx] = vnode` (`:467`) before loading children. If dir A references B and B references A, `LoadVNode(B)` returns the partially-constructed `vnodeA` (its `SiblingLink` is still `IsEmpty()` because A has not yet been linked into a parent). The guard at `:496` (`child != vnode && child->SiblingLink.IsEmpty()`) does **not** prevent the cross-link: both links are empty at the moment of the mutual reference, so A becomes a child of B and B becomes a child of A → a Parent/Children cycle. `RemoveRecursive` does not remove the child from `node->Children` before recursing (removal happens only in `FreeVNode` after the while-loop, `:1227`), so it re-picks the same child and recurses forever.
- **Failure scenario**: Crafted NanoFs image with two directories mutually referencing each other. `Vfs::Remove` on either → infinite recursion → stack overflow → panic.
- **Fix**: remove the child from `node->Children` before recursing (so the loop can't re-pick it), and/or detect cycles in `LoadVNode` (check if `child` is an ancestor of `vnode`).
- **Caveat**: Distinct from #23 (inode-0 root reparenting); #23's "skip inode 0" fix does not prevent general A↔B cycles.
- **Verified (2026-07-08)**: CONFIRMED. Cycle formation traced (`VNodes[inodeIdx]` set at `:467` before children load; both `SiblingLink`s empty at cross-link time); `RemoveRecursive`'s only unlink is in `FreeVNode` (`:526`), reached only after the `while (!Children.IsEmpty())` loop returns — with a cycle it never does.
- **Fixed (2026-07-09)**: fixed at cycle formation -- `LoadVNode` marks inodes load-in-progress and refuses to link a child whose own `LoadVNode` is still on the recursion stack, so A<->B cycles (and any ancestor link) can never enter the VFS tree; `RemoveRecursive` therefore always terminates. Also fixes #23: inode 0 is in-progress for the entire mount, so root can never be reparented.

### 51. [x] ext2: `LoadDir` has no recursion depth limit → stack overflow at mount — MEDIUM (fixed in 315329f)
- **File**: `src/cpp/fs/ext2.cpp:437-584` (recursive at `:529`).
- **Summary**: `LoadDir` recurses for each subdirectory entry. `MaxCachedVNodes = 512` (`:473`) bounds total VNodes but not depth. Each frame has `Ext2Inode inode` on the stack (128 bytes, `:443`) plus call overhead (~200-350 bytes/frame). A chain of 512 nested directories recurses ~512 deep → overflows the 32 KB stack (≈ frame 90-160).
- **Failure scenario**: Crafted ext2 image with deeply nested directories. `Mount` → `LoadDir(2)` → stack overflow → panic at mount.
- **Fix**: iterative worklist or depth cap (same as #49).
- **Verified (2026-07-08)**: CONFIRMED, if anything worse than estimated: two 128-byte `Ext2Inode` stack locals per frame (`:443`, `:548`) → ~250-400 B/frame; the 32 KB stack (`Task::StackSize`, `task.h:20`) overflows near frame ~130, well before the 512-VNode cap.
- **Fixed (2026-07-09)**: `LoadDir` takes a depth parameter capped at `Ext2MaxDirDepth` (32).

### 52. [x] lib: `VsnPrintf` does not NUL-terminate on exact-fill / truncation — MEDIUM (fixed in 3dd96d7)
- **File**: `src/cpp/lib/format.cpp:295-296` (`%s` bounds check), `:315-316` (final terminator).
- **Summary**: The `%s` check is `val_len > (size - pos)` (`:295`), which allows `val_len == size - pos` (exact fit). After `MemCpy` + `pos += val_len`, `pos == size`. The final `PutChar('\0', s, size, pos++)` (`:315`) then sees `pos >= size` → returns false → `VsnPrintf` returns -1 **without** writing `'\0'`. Standard `vsnprintf` always NUL-terminates when `size > 0`. The same happens whenever regular chars or padding fill the buffer exactly.
- **Failure scenario**: `SnPrintf(buf, 5, "ab%s", "cde")` writes `"abcde"` (5 bytes) and returns -1 with no terminator. Several callers don't check the return value (e.g. `procfs.cpp` `SnPrintf(buf, sizeof(buf), "nos %s", KERNEL_VERSION)`, `ext2.cpp:42` `SnPrintf(buf, bufSize, "%s", Dev->GetName())`) → the buffer is used as a non-terminated string → unbounded read past the buffer.
- **Fix**: before returning -1, always write `'\0'` at `s[min(pos, size-1)]` when `size > 0`; or tighten the `%s` check to `val_len + 1 > (size - pos)` and NUL-terminate on every early-return path.
- **Verified (2026-07-08)**: CONFIRMED (boundary arithmetic checked; both cited callers ignore the return value). Exploitability note: `KERNEL_VERSION` defaults to `"dev"` so the procfs case never fills its 128-byte buffer in practice; the ext2 case depends on device-name length. The NUL-termination defect itself is real regardless.
- **Fixed (2026-07-09)**: every error/truncation return in `VsnPrintf` now NUL-terminates at `s[min(pos, size-1)]` via a `TerminateOnError` helper (33 return sites).

### 53. [x] TCP: no FIN-WAIT-2 timeout — permanent stall if peer never sends FIN — MEDIUM (fixed in 5e37ea5)
- **Files**: `src/cpp/net/tcp.cpp:569-583` (FIN-WAIT-2 handler), `:1097-1218` (`ProcessRetransmits` has no `TcpStateFinWait2` case).
- **Summary**: In FIN-WAIT-2 we sent our FIN, it was ACKed, `RetransmitDeadlineMs` is cleared on entry (`:565`), and there is no timer for the state. If the peer ACKs our FIN then crashes without sending its own FIN, the connection sits in FIN-WAIT-2 forever; `Recv()` never returns EOF and the slot is leaked.
- **Failure scenario**: Local closes, peer ACKs the FIN (→ FIN-WAIT-2), then the peer dies. Slot leaked permanently; `Recv` blocks forever.
- **Fix**: implement a FIN-WAIT-2 timer (RFC 1122 §4.2.2.20 requires one) → transition to `Closed` after a timeout.
- **Verified (2026-07-08)**: CONFIRMED. Entry to FIN-WAIT-2 zeroes `RetransmitDeadlineMs` (`:565`), so even the generic deadline guard (`:1136`) is skipped; `Recv`'s EOF state set (`:1032-1036`) omits FinWait2, so `Recv` sleeps forever.
- **Fixed (2026-07-09)**: entering FIN-WAIT-2 arms `TimeWaitDeadlineMs` with `TcpFinWait2TimeoutMs` (60 s); `ProcessRetransmits` transitions an expired FIN-WAIT-2 to Closed, wakes waiters, and reclaims the slot.

### 54. [x] TCP: no persist timer — permanent deadlock on zero window — MEDIUM (fixed in 5e37ea5)
- **Files**: `src/cpp/net/tcp.cpp:929-1007` (`Send`), `:371-397` (`ProcessAck`), `:1097-1218` (`ProcessRetransmits`).
- **Summary**: When the peer advertises a zero window and all outstanding data is ACKed, `ProcessAck` clears `RetransmitDeadlineMs` (`:393-394`, because `SndUna == SndNxt`). No timer runs, nothing is in flight, and `ProcessRetransmits` has no persist case. The only zero-window-probe path is in `Send` (`:964-969`), and only when the app actively calls `Send`. If the app is blocked in `Recv` (not sending), no probe is ever sent.
- **Failure scenario**: App sends a request; peer ACKs all data with window=0 (its buffer is full), drains its buffer and sends a window-update ACK that is lost. App is in `Recv`, not `Send`. We wait for a window update; the peer waits for data. Permanent deadlock.
- **Fix**: add a persist timer in `ProcessRetransmits` that sends a zero-window probe when `SndWnd == 0`, `SndUna == SndNxt`, and no timer is running, with exponential backoff (RFC 9293).
- **Verified (2026-07-08)**: CONFIRMED. `ProcessAck` clears the deadline at `:393-394` when fully ACKed; the only `SndWnd == 0` probe is inside `Send` (`:964-969`); `ProcessRetransmits` never inspects `SndWnd`.
- **Fixed (2026-07-09)**: `ProcessRetransmits` implements a persist timer -- when `SndWnd == 0`, everything is ACKed and no retransmit is pending, it sends a probe with `seq = SndUna - 1` (below the window, forcing a duplicate ACK that carries the peer's current window) with exponential backoff via `PersistDeadlineMs`.

### 55. [x] HTTP client: no `Transfer-Encoding: chunked` support — MEDIUM (fixed in 5e37ea5)
- **File**: `src/cpp/net/http.cpp:128-262` (`RecvResponse`).
- **Summary**: The response parser finds the `\r\n\r\n` boundary (`:189-196`) and treats everything after it as the body, honoring `Content-Length` but never checking for `Transfer-Encoding: chunked`. If the server uses chunked encoding (common for dynamic HTTP/1.1 responses with no Content-Length), the chunk-size markers and trailing CRLF are included verbatim in `resp.Body`.
- **Failure scenario**: `GET` with `Connection: close`; server responds `Transfer-Encoding: chunked`. Body contains lines like `1a\r\n...data...\r\n0\r\n\r\n` → corrupt parsed content.
- **Fix**: detect `Transfer-Encoding: chunked` in the headers and dechunk the body by parsing chunk sizes.
- **Verified (2026-07-08)**: CONFIRMED. No `chunk`/`Transfer-Encoding` handling anywhere in http.cpp/.h; the body is copied verbatim from `buf + headerEnd` (`:241-251`).
- **Fixed (2026-07-09)**: `RecvResponse` detects `Transfer-Encoding: chunked` and dechunks the body (chunk extensions skipped, trailers ignored, a truncated final chunk keeps the bytes that arrived); `ContentLength` reports the decoded length.

### 56. [x] `kernel_cpu_run_on` hangs forever if the target CPU has exited — MEDIUM (fixed in d57f74e)
- **Files**: `src/cpp/kernel/rust_ffi.cpp:317-326`, `src/cpp/kernel/cpu.cpp:505-509` (`QueueIPITask`), `:541-552` (`SendIPISelf`).
- **Summary**: `kernel_cpu_run_on` calls `GetCpu(cpu).QueueIPITask(task)` (which queues + `SendIPISelf`) then `task.Completion.Wait()`. If the target has `StateExited`, `SendIPISelf` returns at `cpu.cpp:548` without sending the IPI; the task sits in `IPITaskList` forever and `Completion.Wait()` blocks forever.
- **Failure scenario**: During shutdown, `ExitAllExceptSelf` sets remote CPUs to `StateExited`. If Rust code (a driver finalizer) calls `kernel_cpu_run_on(cpu, ...)` on an already-exited CPU, the call hangs permanently.
- **Fix**: check `!(GetState() & StateRunning)` / `StateExited` before queuing and return an error instead of blocking.
- **Caveat**: Shutdown-path issue; requires the caller to target an exited CPU.
- **Verified (2026-07-08)**: CONFIRMED. `SendIPISelf` returns at `cpu.cpp:548-549` on `StateExited` without sending; nothing drains `IPITaskList`, so `Completion.Wait()` blocks forever. Bonus hazard: the `RustIPIAdapter` is stack-local (`rust_ffi.cpp:323`), so a late completion would also be a stale-pointer access.
- **Fixed (2026-07-09)**: `kernel_cpu_run_on` checks the target CPU state (`StateRunning` and not `StateExiting`/`StateExited`) and returns without queueing instead of blocking forever.

### 57. [x] Rust FFI interrupt/MSI-X dispatch: unregister-while-in-flight use-after-free — MEDIUM (fixed in d57f74e)
- **Files**: `src/cpp/kernel/rust_ffi.cpp:709-724` (`RustInterruptDispatch`), `:955-970` (`RustMsixDispatch`), `:761-772` (`kernel_interrupt_unregister`), `:1012-1022` (`kernel_msix_unregister_handler`).
- **Summary**: Both dispatch functions read `Handler`/`Ctx` under a read lock, release the lock, then call `handler(ctx)` outside the lock. The unregister functions null `Handler`/`Ctx` under the write lock but do not synchronize with an in-flight ISR. If unregister runs on one CPU between an ISR's read and call on another CPU, and the caller then frees the `ctx` memory, the ISR calls `handler(freed_ctx)`.
- **Failure scenario**: CPU 0 ISR reads `handler`/`ctx`, releases lock; CPU 1 unregisters (nulls fields), returns to caller, caller frees `ctx`; CPU 0 calls `handler(freed_ctx)` → UAF.
- **Fix**: reference count or generation counter / RCU-style grace period so unregister does not return while an ISR is mid-call.
- **Caveat**: Standard "unregister while in-flight" problem; requires precise cross-CPU timing and the caller freeing `ctx` immediately after unregister. The timer variant of this pattern is already #3.
- **Verified (2026-07-08)**: CONFIRMED for both the interrupt path (`ReadUnlock` at `:720`, call at `:722`) and the MSI-X path (`:966/:968`); neither unregister has a barrier/refcount against an in-flight ISR.
- **Fixed (2026-07-09)**: the dispatch paths raise a per-slot `InFlight` counter inside the read-locked section and drop it after the handler returns; unregister nulls the slot under the write lock, then spins until `InFlight == 0`, so it cannot return while an ISR is mid-call on another CPU. Covers the vector, legacy-INTx (`RustLegacyHandler::OnInterrupt`) and MSI-X paths.

---

## Low

### 18. [x] TCP RST acceptance window is too strict (fixed in d037195)
- **File**: `src/cpp/net/tcp.cpp:422`.
- **Summary**: For all states except SYN-SENT a RST is only accepted when `SEG.SEQ == RcvNxt` exactly, rather than the RFC 793 in-window check `RCV.NXT <= SEG.SEQ < RCV.NXT + RCV.WND`. A valid RST elsewhere in the window is dropped. (Deliberate per the comment at `:412-414`; helps against blind RSTs but is a correctness deviation.)
- **Fix**: use the in-window check.
- **Verified (2026-07-08)**: CONFIRMED (`:415-422`). The exact-match rule aligns with RFC 5961's spirit but without the paired challenge-ACK, so it remains an RFC 793 deviation as filed.
- **Fixed (2026-07-09)**: RST acceptance now uses the RFC 793 in-window check (`seq == RcvNxt` is kept so a zero-window RST at the expected sequence still lands).

### 19. [x] TCP RST-ACK computation ignores the FIN flag (fixed in d037195)
- **File**: `src/cpp/net/tcp.cpp:765-767`.
- **Summary**: When generating an RST in response to an unknown segment, `rstAck = Ntohl(tcp->SeqNum) + payloadLen` adds 1 for SYN but not for FIN.
- **Scenario**: A stray FIN to a closed port produces an RST whose ack is one byte low; some stacks treat it as malformed.
- **Fix**: `if (tcp->Flags & TcpFlagFin) rstAck++;`.
- **Verified (2026-07-08)**: CONFIRMED (`:764-767`); SYN is incremented, FIN is not.
- **Fixed (2026-07-09)**: `rstAck` now also counts a FIN.

### 20. [x] TCP `Connect` with caller-supplied `srcPort` skips collision check (fixed in d037195)
- **File**: `src/cpp/net/tcp.cpp:798-808`.
- **Summary**: Only the `srcPort == 0` branch calls `AllocEphemeralPort`. A non-zero `srcPort` is inserted via `InsertHash` with no 4-tuple uniqueness check; `LookupLocked` returns whichever entry it hits first.
- **Scenario**: Two conns with the same 4-tuple; incoming segments routed to the wrong connection. (Only triggers on caller misuse, but the API permits it.)
- **Fix**: check for an existing conn with the same 4-tuple before inserting.
- **Verified (2026-07-08)**: CONFIRMED (`:798-808`, insert at `:831`); only the `srcPort == 0` branch allocates/checks.
- **Fixed (2026-07-09)**: a caller-supplied `srcPort` is rejected when a connection with the same 4-tuple already exists (checked under `PoolLock` via `LookupLocked`).

### 21. [x] TCP `Listen` allows duplicate listeners on the same port (fixed in d037195)
- **File**: `src/cpp/net/tcp.cpp:869-890`.
- **Summary**: `Listen` doesn't scan for an existing listener on `port`; a second `Listen` silently succeeds, wasting a pool slot, and `FindListenerLocked` always returns the first.
- **Fix**: reject a duplicate listener.
- **Verified (2026-07-08)**: CONFIRMED (`:869-890`); no duplicate scan, `FindListenerLocked` returns the first match.
- **Fixed (2026-07-09)**: `Listen` scans the pool and rejects a duplicate listener on the same port.

### 22. [x] NanoFs: `AllocInode`/`AllocDataBlock` commit bitmap before inode/block is initialized (fixed in d037195)
- **File**: `src/cpp/fs/nanofs.cpp:302-313, 324-335`.
- **Summary**: `AllocInode`/`AllocDataBlock` call `FlushSuper` (FUA write of the bitmap) immediately after marking the bit, before the caller has written the inode/block contents.
- **Scenario**: Crash between the bitmap flush and the subsequent `WriteInode` → inode permanently marked allocated but zeroed/`Type=Free` → leaked forever.
- **Fix**: commit the bitmap after the inode/block contents are written, or use a journal.
- **Verified (2026-07-08)**: CONFIRMED. `FindSetZeroBit` sets the bit in-place and `FlushSuper` (FUA) runs at `:311/:333` before callers write contents (`CreateFile` `WriteInode` at `:775`, `CreateDir` at `:887`).
- **Fixed (2026-07-09)**: `AllocInode`/`AllocDataBlock` now only set the bit in memory; callers commit with one `FlushSuper` *after* the inode/block contents are written (CreateFile/CreateDir after `WriteInode`, `Write` after the data flush and before the inode commit). A crash in between leaves the slot free instead of leaked, and the Write path issues one FUA bitmap write instead of up to 256.

### 23. [x] NanoFs: `LoadVNode` can link root into a subdirectory via back-reference (fixed via #50 in 315329f)
- **File**: `src/cpp/fs/nanofs.cpp:490-501`.
- **Summary**: When loading a directory, each dir entry's target inode is loaded and, if `child != vnode && child->SiblingLink.IsEmpty()`, linked into the current dir. For inode 0 (root), `SiblingLink` is always empty, so any directory entry referencing inode 0 reparents root.
- **Scenario**: A crafted/corrupted image where a subdirectory's dir block contains an entry with `InodeIndex = 0` → root reparented, VFS tree broken.
- **Fix**: skip inode 0 in the reparenting logic.
- **Verified (2026-07-08)**: CONFIRMED. Root is never inserted into any parent's `Children`, so its `SiblingLink` stays empty and the `:496` guard passes; no `inodeIdx == 0` skip exists.
- **Fixed (2026-07-09)**: by the #50 fix -- inode 0 is marked load-in-progress for the entire mount, so the `LoadVNode` linking guard skips any dir entry referencing it.

### 24. [x] ext2: triple-indirect blocks silently return zeros (fixed in d037195)
- **File**: `src/cpp/fs/ext2.cpp:337-339`.
- **Summary**: Files using triple-indirect blocks are not supported; `GetBlockNum` returns 0, which `ReadInodeData` interprets as a sparse hole and fills with zeros, silently returning wrong data instead of an error.
- **Fix**: return an error / `Trace` for triple-indirect blocks rather than 0.
- **Verified (2026-07-08)**: CONFIRMED (a `Trace` is already emitted at `:337`, but 0 is still returned and treated as a hole). Reachable: with 1 KB block size, double-indirect covers only ~64 MB, so files above that hit triple-indirect and silently read zeros.
- **Fixed (2026-07-09)**: `GetBlockNum` returns bool with a `physBlock` out-param; triple-indirect blocks (and indirect-block read failures) now fail the read instead of silently returning zeros.

### 25. [x] Vfs: path components longer than 63 characters are silently truncated (fixed in d037195)
- **File**: `src/cpp/fs/vfs.cpp:204-216`.
- **Summary**: The `component` buffer is 64 bytes; the copy loop stops at 63 chars. When truncation occurs, `*p` is not `/` or `\0`, so the remaining characters are reinterpreted as the next path component(s).
- **Scenario**: A path with a >63-char component is mis-parsed (wrong lookup or spurious failure) instead of yielding "name too long".
- **Fix**: return an ENAMETOOLONG-style error when a component exceeds the buffer.
- **Verified (2026-07-08)**: CONFIRMED. On truncation `*p` is not `/`, so `if (*p == '/') p++` doesn't advance and the outer loop re-parses the remainder as new components, exactly as filed.
- **Fixed (2026-07-09)**: a component that does not fit the 64-byte buffer fails resolution with a trace instead of being re-parsed as extra components.

### 26. [x] mm: `SetupFreePagesList` leaks a refcount on every free page (fixed in 49debda)
- **File**: `src/cpp/mm/page_table.cpp:516`.
- **Summary**: `GetPage(phyAddr)` increments every free page's refcount (+1), but that reference is never balanced by a `Put`. Free-list pages sit at refcount 2 instead of 1, violating the "free page == refcount 1" invariant.
- **Scenario**: Functionally harmless (refcount is not consulted for free-list decisions; `FreePage` just pushes onto the list), but the invariant is violated and masks the (retracted) race in mm #2-below.
- **Fix**: drop the extra `GetPage`/`Put` pair, or document the invariant.
- **Verified (2026-07-08)**: CONFIRMED. `Page::Init` sets refcount 1, `GetPage` at `:516` makes it 2, never balanced; "functionally harmless" verified — no free-list path consults `RefCount`.
- **Fixed (2026-07-09)**: `SetupFreePagesList` drops `GetPage`'s lookup reference with `Put()`, restoring the free-page == refcount-1 invariant.

### 27. [x] mm: `MapMmioRegion` maps at `physAddr + KernelSpaceBase`, bypassing `VaAllocator`, no cross-CPU TLB shootdown (documented in 49debda)
- **File**: `src/cpp/mm/page_table.cpp:889-973` (returns `physAddr + KernelSpaceBase` at `:972`; local `Invlpg` at `:969`).
- **Summary**: Violates the CLAUDE.md rule that device memory must go through `Mm::MapPages`/`AllocMapPages` so the `VaAllocator` tracks the VA. Only local `Invlpg`, no global shootdown. `kernel_unmap_phys` is a no-op (mappings are permanent).
- **Scenario**: Safe under current boot ordering (all `MapMmioRegion` callers run in `Main2`/`BpStartup` before `cpus.StartAll()`). Becomes a real bug if any driver is initialized after APs start or if MMIO regions need to be unmapped/remapped.
- **Fix**: route through `VaAllocator` and issue a cross-CPU shootdown (or document the boot-ordering constraint).
- **Verified (2026-07-08)**: CONFIRMED, including the mitigation: every `MapMmioRegion`/`kernel_map_phys` caller (HPET at `main.cpp:724`, virtio + `rust_init` at `:466-474`) runs strictly before `cpus.StartAll()` (`:510`), so it is safe under current boot ordering as filed.
- **Fixed (2026-07-09)**: resolved by documenting the constraint in `page_table.h`: `MapMmioRegion` mappings are permanent, live at `physAddr + KernelSpaceBase` outside the VaAllocator, and only invalidate the local TLB, so callers must run on the BSP before APs start -- which every current caller does.

### 28. [x] drivers: `Pic::EOI` only EOIs the master PIC (fixed in 7a88822)
- **File**: `src/cpp/drivers/pic.cpp:53-56`.
- **Summary**: `EOI()` sends `PIC_EOI` to `PIC1` (0x20) only. For interrupts originating from the slave PIC (IRQ 8–15), the slave at 0xA0 also requires an EOI; without it the slave stops delivering. The standard cascaded-8259 EOI is: if IRQ > 7, EOI slave then master.
- **Scenario**: Effectively dead code (kernel switches to LAPIC/IOAPIC and calls `Pic::Disable()`), but broken if ever used for slave IRQs.
- **Fix**: EOI slave then master for IRQ > 7.
- **Verified (2026-07-08)**: CONFIRMED, and fully dead code: grep finds zero callers of `Pic::EOI`; the only Pic use is `Remap()` + `Disable()` in main.cpp.
- **Fixed (2026-07-09)**: `EOI(int irq)` EOIs the slave PIC first for IRQ >= 8.

### 30. [x] drivers: MSI-X vector allocation is non-atomic (fixed in 7a88822)
- **File**: `src/cpp/drivers/msix.cpp:187-192`.
- **Summary**: `MsixTable::NextVector` is a `static u8` incremented by `AllocVector()` with no atomicity or lock.
- **Scenario**: Safe only because device initialization is serialized on the BSP; if two devices were ever initialized concurrently this would double-allocate vectors.
- **Fix**: make `NextVector` atomic or guard allocation with a lock.
- **Verified (2026-07-08)**: CONFIRMED (`return NextVector++` on a plain static u8, `msix.cpp:187-192`).
- **Fixed (2026-07-09)**: vector allocation is a cmpxchg loop over an `Atomic` offset from `MsixVectorBase` (BSS zero-init, so no static-constructor hazard).

### 31. [x] kernel: `TaskQueue::Schedule` panics if an exiting task finds no runnable next (fixed in 7a88822)
- **File**: `src/cpp/kernel/sched.cpp:144-153`.
- **Summary**: When the current task is `StateExited` (removed at `:122-124`) and `SelectNext` returns `nullptr`, the no-switch branch ends with `BugOn(curr->State.Get() == Task::StateExited)` → panic. Currently avoided only because every CPU queue always contains a non-preempt-disabled idle task.
- **Scenario**: Panics if that invariant is ever broken (e.g. an idle task that exits or a stuck preempt-disable count).
- **Fix**: handle the no-runnable-next-exiting case gracefully instead of `BugOn`.
- **Verified (2026-07-08)**: CONFIRMED (removal at `sched.cpp:118-124`, `BugOn` at `:151`; `SelectNext` returns nullptr when all other candidates are preempt-disabled).
- **Fixed (2026-07-09)**: an exited task that finds no runnable candidate drops the locks and retries (bounded by `MaxExitedRetries`, then a diagnosable Panic) instead of tripping the BugOn.

### 32. [x] kernel: `RawRwSpinLock` / `RwMutex` reader can sneak past a waiting writer (fixed in 7a88822)
- **Files**: `src/cpp/kernel/raw_rw_spin_lock.cpp:16-32` (same pattern in `src/cpp/kernel/rw_mutex.cpp:15-31`).
- **Summary**: `ReadLock` checks `WriterWaiting` then does a cmpxchg on `Value`. A writer may set `WriterWaiting=1` (and acquire `Value=-1`) in the window between the reader's check and its cmpxchg; the reader's cmpxchg still succeeds against a stale `Value==0` and enters with the writer waiting. Violates the "no new readers once a writer is waiting" intent; can starve writers under contention. Not a safety violation.
- **Fix**: re-check `WriterWaiting` after acquiring, or use a single atomic that encodes both writer-waiting and reader count.
- **Verified (2026-07-08)**: CONFIRMED (fairness only — the `v < 0` cmpxchg guard preserves safety once the writer holds `Value = -1`).
- **Fixed (2026-07-09)**: `ReadLock` re-checks `WriterWaiting` after the cmpxchg and backs out if a writer started waiting (both `RawRwSpinLock` and `RwMutex`).

### 33. [x] kernel: `Interrupt::SharedDispatch` fires every shared handler on every shared IRQ (fixed in 7a88822)
- **File**: `src/cpp/kernel/interrupt.cpp:119-138`.
- **Summary**: When any shared vector fires, `SharedDispatch` iterates **all** `MaxVectors` entries and invokes every handler of every vector with `HandlerCount > 1`, regardless of which vector actually asserted.
- **Scenario**: Handlers on an unrelated GSI get called spuriously. Only safe if every shared handler no-ops when its device's interrupt-pending bit is clear (virtio ISR reads clear-on-read, so a spurious call reads 0 and returns); any handler that doesn't check will process phantom interrupts.
- **Fix**: dispatch only the handlers registered for the vector that fired.
- **Verified (2026-07-08)**: CONFIRMED. The firing vector isn't even a parameter of `SharedDispatch`; it loops all `MaxVectors` and calls every handler where `HandlerCount > 1`.
- **Fixed (2026-07-09)**: `SharedDispatch` dispatches only vectors that are actually in service per `Lapic::CheckIsr` (made public), so handlers of unrelated shared vectors no longer see phantom interrupts.

### 34. [x] lib: `StrnCmp` uses signed `char` comparison (fixed in 49debda)
- **File**: `src/cpp/lib/stdlib.cpp:96-117`.
- **Summary**: `char` is signed on x86-64, so bytes with the high bit set (0x80–0xFF) compare as negative and sort before ASCII. The C standard for `strncmp` requires comparison as `unsigned char`.
- **Scenario**: Comparing strings containing non-ASCII/binary bytes yields incorrect ordering.
- **Fix**: cast to `unsigned char` before comparing.
- **Verified (2026-07-08)**: CONFIRMED (`stdlib.cpp:102`, plain signed `char` compares).
- **Fixed (2026-07-09)**: comparison is done on `unsigned char`.

### 35. [x] lib: `ParseUlong` has no overflow check (fixed in 49debda)
- **File**: `src/cpp/lib/stdlib.cpp:204-218`.
- **Summary**: `result = result * 10 + (*s - '0')` silently wraps on unsigned overflow for long numeric input, returning a bogus value with no error.
- **Fix**: check for overflow before the multiply/add and return false.
- **Verified (2026-07-08)**: CONFIRMED (`stdlib.cpp:214`, silent unsigned wrap, returns true).
- **Fixed (2026-07-09)**: overflow is detected before the multiply/add and returns false.

### 36. [x] rust: NVMe `1 << lbaf.lbads` shift overflow from device-supplied data (fixed in 78d0960)
- **File**: `src/rust/drivers/nvme/src/lib.rs:268`.
- **Summary**: `let sector_size: u32 = 1 << lbaf.lbads;` where `lbaf.lbads: u8` is read directly from the device's Identify Namespace response with no range check. If `lbads >= 32`, the shift overflows: debug builds panic (aborting the kernel via the panic handler); release builds yield `sector_size = 1`, which the subsequent `< 512` check rejects.
- **Fix**: guard `lbads` (e.g. `> 12` or `> 15`) before the shift.
- **Verified (2026-07-08)**: CONFIRMED. Correction: in release builds the shift is masked (`lbads & 31`), so it doesn't always yield 1 — `lbads % 32 ∈ {9..12}` produces a plausible-but-wrong sector size that passes the range check. Debug builds panic as filed.
- **Fixed (2026-07-09)**: `lbads >= 32` is rejected before the shift; values 13..31 were already caught by the existing `> PAGE_SIZE` check.

### 37. [x] rust: `from_utf8_unchecked` on possibly-truncated trace buffers is UB (fixed in 78d0960)
- **File**: `src/rust/ffi/src/trace.rs:25` (and indirectly `ffi/src/panic.rs:9`).
- **Summary**: `TraceBuf::as_str` returns `core::str::from_utf8_unchecked(&self.buf[..self.pos])`. The buffer can be truncated mid-multibyte-UTF-8 sequence (or the panic formatting can produce non-UTF8), so producing a `&str` that is not valid UTF-8 is undefined behavior. Practically safe (bytes are only ever copied byte-wise), but technically UB.
- **Fix**: validate UTF-8 before constructing `&str`, or pass `&[u8]`.
- **Verified (2026-07-08)**: CONFIRMED. `write_str` truncates byte-wise at the 512-byte boundary; both consumers only take `.as_ptr()`/`.len()`, so practically safe, technically UB — as filed.
- **Fixed (2026-07-09)**: `as_str` returns the valid UTF-8 prefix (`from_utf8` + `valid_up_to`) instead of an unchecked conversion of a possibly-split sequence.

### 38. [x] rust: r8168 ISR forms `&mut` that can alias `&mut` held by `flush_tx`/`process_rx` (fixed in 78d0960)
- **File**: `src/rust/drivers/r8168/src/lib.rs:377`.
- **Summary**: `r8168_isr` does `let dev = unsafe { &mut *(ctx as *mut R8168Device) }`; `r8168_flush_tx` (`:408`) and `r8168_process_rx` (`:441`) do the same. If the ISR fires on one CPU while `flush_tx` runs on another, two `&mut` references to the same `R8168Device` exist simultaneously — aliasing UB. In practice the ISR only touches `INTR_STATUS` MMIO and raises softirq, so no actual data race.
- **Fix**: use raw `(*dev).field` dereferences and shared `&MmioRegion` references as the NVMe ISR does.
- **Verified (2026-07-08)**: CONFIRMED. `flush_tx` runs under the C++ `TxQueueLock` but the ISR takes no lock (per-CPU IRQ-disable only, comment at `:393-395`); the NVMe ISR contrast holds (raw `(*dev)` derefs at nvme `lib.rs:507`).
- **Fixed (2026-07-09)**: the ISR takes a raw `*mut R8168Device` and a shared `&regs` reference, matching the NVMe ISR pattern.

### 39. [x] rust: NVMe `inflight` handle stored with `Relaxed` ordering before doorbell write (fixed in 78d0960)
- **File**: `src/rust/drivers/nvme/src/lib.rs:636-637` (submit_io) and `:566-567` (flush).
- **Summary**: The submitter stores `inflight[cid].store(handle, Ordering::Relaxed)` then writes the SQE + rings the SQ doorbell (volatile MMIO). The ISR loads `inflight[cid].load(Ordering::Relaxed)`. Relaxed atomics provide no ordering with respect to the volatile doorbell write in Rust's abstract machine; if the ISR fired before the store became visible, it would see `inflight[cid] == 0`, skip `wg_done`, and deadlock the submitter. On x86 (TSO) with current LLVM codegen this does not occur in practice.
- **Fix**: use `Ordering::Release` for the store and `Ordering::Acquire` for the ISR load.
- **Verified (2026-07-08)**: CONFIRMED for the submit→ISR handle-visibility path. Note the completion path (`inflight_status`) already uses Release/Acquire correctly; only the handle store/load pair is Relaxed.
- **Fixed (2026-07-09)**: the inflight handle store is `Release` (both `submit_io` and `nvme_flush`) and the ISR load is `Acquire`.

### 40. [x] block: MBR accepts `LbaStart == 0` (aliases boot sector) (fixed in d037195)
- **File**: `src/cpp/block/partition.cpp:101-106`.
- **Summary**: A partition entry with `LbaStart == 0` is accepted; only `endSector > capacity` is rejected, not `LbaStart == 0`. It would alias the MBR boot sector.
- **Scenario**: Real partition tables never do this, so latent.
- **Fix**: reject `LbaStart == 0`.
- **Verified (2026-07-08)**: CONFIRMED (`partition.cpp:98-106`; only `Type == 0 || LbaSize == 0` and `endSector > capacity` are rejected).
- **Fixed (2026-07-09)**: entries with `LbaStart == 0` are skipped with a trace.

### 41. [x] block: `PartitionDevice::ProbeAll` is non-idempotent (fixed in d037195)
- **File**: `src/cpp/block/partition.cpp:138-161`.
- **Summary**: Calling `ProbeAll` more than once resets `InstanceCount = 0` and placement-news over `Instances[]` that are still registered in `BlockDeviceTable`, producing stale pointers and duplicate registrations with no way to unregister. Documented in a comment; called once at boot (`main.cpp:468`).
- **Fix**: add an `Unregister` path or assert/guard against double-probing.
- **Verified (2026-07-08)**: CONFIRMED; exactly one call site (`main.cpp:468`), so latent as documented.
- **Fixed (2026-07-09)**: a second `ProbeAll` call is rejected by a `BugOn`'d static flag.

### 42. [x] boot: no bounds check on `tag->Size` before advancing (fixed in 49debda)
- **File**: `src/cpp/boot/grub.cpp:64-66`.
- **Summary**: The tag-advancement expression `(tag->Size + 7) & ~7` is read from the Multiboot info. A malformed tag with `Size == 0` would not advance and spin forever. GRUB always produces well-formed tags, so theoretical.
- **Fix**: break the loop if the advanced pointer does not move past `tag`.
- **Verified (2026-07-08)**: CONFIRMED (`(0 + 7) & ~7 == 0` → `MemAdd(tag, 0) == tag`, no advance).
- **Fixed (2026-07-09)**: a tag with `Size < sizeof(MultiBootTag)` (or extending past `TotalSize`) stops parsing instead of spinning; the advance always moves forward.

### 43. [x] boot: mmap loop trusts `mmap->EntrySize` is non-zero (fixed in 49debda)
- **File**: `src/cpp/boot/grub.cpp:83-85`.
- **Summary**: If `EntrySize == 0`, the loop never advances and spins. The Multiboot2 spec guarantees `EntrySize >= 16`, so not triggerable in practice.
- **Fix**: break if `EntrySize == 0`.
- **Verified (2026-07-08)**: CONFIRMED (same non-advance shape as #42).
- **Fixed (2026-07-09)**: `EntrySize < sizeof(MultiBootMmapEntry)` (including 0) skips the mmap tag.

### 58. [x] TCP: CLOSING / CLOSE-WAIT do not re-ACK a retransmitted FIN — LOW (fixed in d037195)
- **File**: `src/cpp/net/tcp.cpp:584-616`.
- **Summary**: Neither state has a `flags & TcpFlagFin` branch (only ACK is handled), unlike TIME-WAIT (`:617-628`) which re-ACKs. If our ACK of the peer's FIN is lost, the peer retransmits the FIN; we silently drop it and the peer retransmits until its RTO expires.
- **Fix**: add a `TcpFlagFin` branch that sends a duplicate ACK.
- **Verified (2026-07-08)**: CONFIRMED. CLOSING (`:584-595`) and CLOSE-WAIT (`:610-615`) handle only ACK; TIME-WAIT (`:617-628`) re-ACKs as claimed.
- **Fixed (2026-07-09)**: both states re-ACK a retransmitted FIN, mirroring TIME-WAIT.

### 59. [x] TCP: TIME-WAIT duration is 2 s (RFC recommends 2·MSL ≈ 60-120 s) — LOW (fixed in d037195)
- **File**: `src/cpp/net/tcp.h:38` (`TcpTimeWaitMs = 2000`).
- **Summary**: A delayed duplicate segment from the old incarnation could arrive after TIME-WAIT expires and be matched by a new connection reusing the same 4-tuple.
- **Fix**: raise toward 2·MSL (or accept the deviation for a hobby OS).
- **Verified (2026-07-08)**: CONFIRMED (`tcp.h:38`).
- **Fixed (2026-07-09)**: raised to 60 s (Linux-style stand-in for 2*MSL).

### 60. [x] TCP: SYN in ESTABLISHED is silently ignored (RFC 793 requires RST) — LOW (fixed in d037195)
- **File**: `src/cpp/net/tcp.cpp:489-527`.
- **Summary**: The Established handler processes ACK/data/FIN but never checks `TcpFlagSyn`. RFC 793 §3.9 requires a RST.
- **Fix**: send a RST for a segment with SYN set in ESTABLISHED.
- **Verified (2026-07-08)**: CONFIRMED. The Established handler (`:489-527`) never tests `TcpFlagSyn`.
- **Fixed (2026-07-09)**: an in-window SYN in ESTABLISHED sends a RST and closes the connection; a below-window retransmitted handshake SYN stays ignored.

### 61. [x] TCP: no ICMP unreachable handling for connections — LOW (fixed in d037195)
- **File**: `src/cpp/net/icmp.cpp:154-157` (type 3 counted as `RxOther`, dropped).
- **Summary**: ICMP Destination Unreachable is silently dropped; TCP is never notified. Dead-peer / PMTU-black-hole detection relies solely on the retransmit timeout (which, per #45, may never expire).
- **Fix**: notify TCP on ICMP type 3 (abort the connection / reduce path MTU).
- **Verified (2026-07-08)**: CONFIRMED. Only echo request/reply are handled; every other type hits the `RxOther` else-branch; no TCP notification path exists.
- **Fixed (2026-07-09)**: `Icmp::Process` parses type 3; codes 2/3 (protocol/port unreachable, the RFC 1122 hard errors) abort the quoted connection via the new `Tcp::OnIcmpUnreachable`, whose slot the cleanup pass then reclaims.

### 62. [x] ARP and DNS cache entries never expire (no TTL) — LOW (fixed in d037195)
- **Files**: `src/cpp/net/arp.cpp:46-76` (`ArpEntry` has no timestamp); `src/cpp/net/dns.cpp:84-114` (`CacheEntry` has no TTL).
- **Summary**: Neither cache honors a TTL. If a host's MAC or a hostname's IP changes, the stale entry is served until evicted by cache pressure.
- **Fix**: store insertion time + TTL; treat expired entries as invalid in `Lookup`.
- **Verified (2026-07-08)**: CONFIRMED. `ArpEntry` (`arp.h:43-48`) and DNS `CacheEntry` (`dns.h:68-72`) carry no timestamp/TTL; the DNS response parser explicitly skips the TTL field (`dns.cpp:330`).
- **Fixed (2026-07-09)**: ARP entries carry `CreatedMs` and expire after 5 minutes (expiry forces re-resolution); the DNS cache stores a per-record expiry from the response TTL, which is now parsed instead of skipped (TTL 0 is not cached per RFC 1035, TTLs are capped at 1 day).

### 63. [x] DHCP: renewal REQUEST is broadcast instead of unicast to the server — LOW (fixed in d037195)
- **File**: `src/cpp/net/dhcp.cpp:383` (broadcast), `:420-437` (renewing path omits server-id).
- **Summary**: RFC 2131 §4.1.4 requires the RENEWING (T1) `DHCPREQUEST` to be **unicast** to the granting server. The code broadcasts it (correct for REBINDING/T2, wrong for RENEWING).
- **Failure scenario**: Some servers ignore broadcast renewals, or a different server NAKs → lease renewal fails and the client restarts from DISCOVER (`:182-184`), causing brief connectivity loss.
- **Fix**: in the renewing path, unicast to `Result.ServerIp` (ARP-resolve its MAC).
- **Verified (2026-07-08)**: CONFIRMED, with a correction: there is no separate REBINDING/T2 path — a single `renewing` bool drives one T1 renewal that always broadcasts (`:370/:383`), so the "correct for REBINDING" framing doesn't match the code. The `if (!renewing)` server-id omission is actually RFC-correct for renewal; the core broadcast-instead-of-unicast claim holds (`Result.ServerIp` is stored at `:564/:569`).
- **Fixed (2026-07-09)**: the T1 renewal is unicast to `Result.ServerIp` (ARP-resolved, gateway-aware via `RouteIp`; broadcast fallback if resolution fails), and the broadcast flag is cleared while renewing.

### 64. [x] Vfs: `ResolvePath` does not handle `.` and `..` components — LOW (fixed in d037195)
- **File**: `src/cpp/fs/vfs.cpp:200-246`.
- **Summary**: `Lookup(cur, ".")` / `Lookup(cur, "..")` always return nullptr (neither NanoFs nor ext2 stores them as children). Any path with an intermediate `.`/`..` fails resolution; a trailing `..` can mislead `CreateFile`/`CreateDir` into targeting a literal `..`.
- **Fix**: special-case `.` (skip) and `..` (walk to `cur->Parent`, or the mount root).
- **Caveat**: Not triggered by the current shell (absolute paths, no `cd`).
- **Verified (2026-07-08)**: CONFIRMED. ext2 `LoadDir` skips `.`/`..` when building children and NanoFs never creates them, so `Lookup` returns nullptr for both. The trailing-`..` create-mislead only bites NanoFs (ext2's `Create*` are stubs); the resolution failure affects both.
- **Fixed (2026-07-09)**: `.` is skipped and `..` walks to `Parent` (the mount root stays put); both are also handled as the last component.

### 65. [x] NanoFs: superblock layout fields trusted without validation — LOW (fixed in d037195)
- **File**: `src/cpp/fs/nanofs.cpp:53-106` (`Mount`); used at `:265`, `:281`, `:479/585/599/692`.
- **Summary**: `Mount` validates `Magic`/`Version`/CRC32 but not `InodeStartBlock`/`DataStartBlock`/`BlockSize`/`InodeCount`/`DataBlockCount` against the compile-time constants. All I/O uses the on-disk values directly. A forged-but-valid-CRC image (e.g. `InodeStartBlock = 0`) makes `ReadInode` read the superblock as an inode, or data writes overwrite inodes.
- **Fix**: validate each layout field equals its `Nano*` constant in `Mount`.
- **Verified (2026-07-08)**: CONFIRMED. `Mount` checks Magic/Version/CRC only; all I/O uses on-disk `InodeStartBlock`/`DataStartBlock` directly.
- **Fixed (2026-07-09)**: `Mount` validates `BlockSize`/`InodeCount`/`DataBlockCount`/`InodeStartBlock`/`DataStartBlock` against the compiled-in geometry.

### 66. [x] ext2: directory entries with `NameLen > 63` are silently truncated — LOW (fixed in d037195)
- **File**: `src/cpp/fs/ext2.cpp:566-570`.
- **Summary**: ext2 allows 255-byte names; `VNode::Name` is 64 bytes. `LoadDir` clamps and truncates without error. Two files sharing a 63-byte prefix become unreachable-by-name collisions in `Lookup`.
- **Fix**: reject `NameLen >= sizeof(VNode::Name)` during `LoadDir`, or widen `VNode::Name`.
- **Verified (2026-07-08)**: CONFIRMED (`ext2.cpp:566-570`; `VNode::Name` is `char[64]`, ext2 `NameLen` is u8 up to 255; clamp-and-continue with no error).
- **Fixed (2026-07-09)**: entries with `NameLen >= sizeof(VNode::Name)` are skipped with a trace instead of truncated into colliding names.

### 67. [x] NanoFs: `Read` silently short-reads when `Size > NanoMaxFileSize` — LOW (fixed in d037195)
- **File**: `src/cpp/fs/nanofs.cpp:1130-1167`.
- **Summary**: The read loop is bounded by `blockOff < NanoMaxBlocks` (256). A crafted inode (valid CRC) with `Size > NanoMaxFileSize` (1 MB) makes `toRead` exceed 1 MB, but the loop exits at 256 blocks with `bytesRead < toRead` and `ok` still true; `Read` returns true. `ComputeDataChecksum` (`:1161`) also covers only the first 256 blocks, so the checksum check passes. The caller uses the full `toRead` length → reads uninitialized heap (info leak / wrong data).
- **Fix**: return false when `bytesRead < toRead`, or clamp `inode->Size` to `NanoMaxFileSize` after verifying the checksum.
- **Caveat**: Read-side sibling of #13.
- **Verified (2026-07-08)**: CONFIRMED. Note `Read` does verify the inode checksum (`:1097`) — unlike `Write`/`RemoveRecursive` (#13) — so this one does require a forged-but-valid CRC32 (feasible; CRC32 is not cryptographic). The data-checksum check is also skipped entirely when `DataChecksum == 0`.
- **Fixed (2026-07-09)**: `Read` rejects `Size > NanoMaxFileSize` right after the checksum verify, and a short read now fails instead of returning success with garbage in the tail.

### 68. [x] virtio_blk/scsi: `DrainQueue` re-enqueue reorders remaining requests — LOW (fixed in 7a88822)
- **Files**: `src/cpp/drivers/virtio_blk.cpp:280-286`, `:364-370`; same in `src/cpp/drivers/virtio_scsi.cpp:337-342, 464-472`.
- **Summary**: On `AllocSlot` failure or ring-full mid-batch, the code does `InsertHead(&batch[i]->Link)` then `InsertTail(&batch[j]->Link)` for j=i+1.., putting `batch[i]` at the head but the rest at the tail (behind pre-existing items). Original order `batch[i..], existing` becomes `batch[i], existing, batch[i+1..]` — a reordering that can let a later-enqueued flush leapfrog earlier writes.
- **Fix**: re-enqueue all remaining batch items at the head in reverse order to preserve the original order.
- **Verified (2026-07-08)**: CONFIRMED in both drivers and both failure paths (blk `:280-286`/`:362-371`, scsi `:335-342`/`:464-472`).
- **Caveat**: Limited impact under the current synchronous I/O model (one in-flight per caller); matters under concurrent I/O.
- **Fixed (2026-07-09)**: failed batches are re-inserted at the queue head in reverse order, preserving the original request order (both drivers, both failure paths).

### 69. [x] virtio_rng: never suppresses interrupts, registers no handler, ISR never read — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/virtio_rng.cpp` (whole file); `src/cpp/drivers/virtqueue.cpp:77` (`Avail->Flags = 0`).
- **Summary**: The driver is polling-based (`GetRandom` polls `HasUsed`) but never sets `VIRTQ_AVAIL_F_NO_INTERRUPT`, never enables MSI-X, and never registers an INTx handler. On completion the device asserts INTx (clear-on-read ISR); since the ISR is never read, the level line stays asserted.
- **Failure scenario**: If virtio-rng's GSI is shared with another INTx device whose handler unmasks the shared line, the asserted level is re-delivered in a tight loop (handler reads its own ISR=0, returns, line still asserted) → interrupt storm / hang. If the GSI stays masked (no handler registered), it is latent.
- **Fix**: set `Avail->Flags = VIRTQ_AVAIL_F_NO_INTERRUPT` after `Queue.Setup`.
- **Verified (2026-07-08)**: CONFIRMED end-to-end (no ISR read, no MSI-X, no INTx handler; `Avail->Flags` stays 0). Supporting evidence: virtio-scsi explicitly `Reset()`s an empty HBA to avoid exactly this shared-line storm — a mitigation the RNG driver lacks.
- **Caveat**: Latent under QEMU default PCI slot assignment (GSI stays masked). Same shape as #16.
- **Fixed (2026-07-09)**: the queue sets `VIRTQ_AVAIL_F_NO_INTERRUPT` via the new `VirtQueue::DisableDeviceInterrupts()` right after setup.

### 70. [x] mm: `Pool::Free` and `VaAllocator::Free` have no double-free detection — LOW (fixed in 49debda)
- **Files**: `src/cpp/mm/pool.cpp:107-137` (`BlockCount++` with no check); `src/cpp/mm/va_allocator.cpp:94-103` (`ClearBit` unconditional).
- **Summary**: `Pool::Free` increments `BlockCount` without checking the block was already freed → `BlockCount` inflates past `MaxBlockCount`, the page is never seen as full-free, and a later free can prematurely return the page to the page allocator while blocks are still live → UAF. `VaAllocator::Free` clears an already-clear bit silently → the next `Alloc` can return the same VA to two callers → overlapping allocations.
- **Fix**: `BugOn` on double-free (Pool: a self-pointing `Link` / allocated flag; VaAllocator: bit already clear).
- **Verified (2026-07-08)**: CONFIRMED. Nuance: the Pool release trigger is an equality check (`BlockCount == MaxBlockCount`), so a double-free that lands the count exactly on the max while live blocks remain releases the page prematurely (UAF); overshooting past it instead skips the release. VaAllocator double-VA-handout confirmed.
- **Fixed (2026-07-09)**: `Pool::Free` stamps freed blocks with a `FreedTag` sentinel and BugOns on a re-free (plus a `BlockCount >= MaxBlockCount` bound); `VaAllocator::Free` BugOns when the bit is already clear (new `Bitmap::TestBit`).

### 71. [x] mm: `AllocatorImpl::Alloc` integer overflow in `size + sizeof(Header)` — LOW (fixed in 49debda)
- **File**: `src/cpp/mm/allocator.cpp:51-54`.
- **Summary**: `reqSize = size + sizeof(*header)` with no overflow check. A near-`SIZE_MAX` `size` wraps to a small value, bypassing the `reqSize >= PageSize/2` page path; `Log2(reqSize)` returns a small log and a tiny block is returned for a huge `size` → caller overwrites adjacent pool memory.
- **Fix**: reject `size > SIZE_MAX - sizeof(Header)`.
- **Verified (2026-07-08)**: CONFIRMED. The internal `BugOn((1 << log) < size)` compares against the already-wrapped `reqSize`, so it does not catch the overflow.
- **Fixed (2026-07-09)**: sizes that would overflow `size + sizeof(Header)` return nullptr.

### 72. [x] lib: `ListEntry` move ctor/assignment corrupts state when the source is empty — LOW (fixed in 49debda)
- **File**: `src/cpp/lib/list_entry.cpp:111-130`.
- **Summary**: Move-constructing from an empty source (`Flink == Blink == &other`) leaves the destination's `Flink` pointing at the source's sentinel rather than itself → `IsEmpty()` returns false and traversal loops forever treating the sentinel as a data node. The move-assignment operator has the same bug.
- **Fix**: `Init()` first and bail if `other.IsEmpty()` before splicing.
- **Caveat**: Latent — no current code move-constructs a `ListEntry` from an empty source (containing structs `delete` their move ctors).
- **Verified (2026-07-08)**: CONFIRMED. Splice traced: empty source leaves `this->Flink == &other`, `IsEmpty()` false, traversal loops via the source sentinel. Move-assignment has the identical defect. No invocation exists anywhere in src/cpp — latent as filed.
- **Fixed (2026-07-09)**: move ctor/assignment `Init()` the destination and leave the source alone when the source is empty.

### 73. [x] TimerTable: `StartTimer` does not reject `Period == 0` (infinite re-fire) — LOW (fixed in 7a88822)
- **File**: `src/cpp/kernel/timer.cpp:20-35` (`StartTimer`), `:49-64` (`ProcessTimers`).
- **Summary**: `ProcessTimers` always does `Expired += Period` (`:61`). With `Period == 0`, `Expired` never advances and the callback fires on every tick (100 Hz) until `StopTimer`. The Rust FFI bridge rejects `period_ns == 0` (`rust_ffi.cpp:826`), but C++ callers (tcp.cpp, 8042.cpp) are unguarded.
- **Fix**: reject `period == 0` in `StartTimer`, or make 0 mean one-shot.
- **Verified (2026-07-08)**: CONFIRMED, latent: the Rust bridge rejects `period_ns == 0` (`rust_ffi.cpp:826`); both C++ callers currently pass nonzero constants, so it's defense-in-depth as filed.
- **Fixed (2026-07-09)**: `StartTimer` rejects a zero period.

### 74. [x] TimerTable: `ProcessTimers` catch-up storm after a delayed tick — LOW (fixed in 7a88822)
- **File**: `src/cpp/kernel/timer.cpp:49-64`.
- **Summary**: After a callback fires, `Expired += Period` (`:61`). If `ProcessTimers` was delayed (long IRQ handler / long `ProcessIPITasks`), `now` can be far ahead of `Expired`; after `+= Period`, `Expired` may still be `<= now`, so the callback fires again next tick — a burst of rapid callbacks (e.g. a 100 ms timer firing 10× in 100 ms after a 1 s delay).
- **Fix**: set `Expired = now + Period` (skip missed ticks) instead of `Expired += Period`.
- **Verified (2026-07-08)**: CONFIRMED. The loop fires at most once per `ProcessTimers` call, so the burst is one fire per 10 ms tick until `Expired` catches up — the "10× in 100 ms" example is accurate.
- **Fixed (2026-07-09)**: `Expired = now + Period` -- missed ticks are skipped instead of replayed.

### 75. [x] `Cpu::IPI` uses `Index == 0` instead of `BspIndex` to gate `ProcessTimers` — LOW (fixed in 7a88822)
- **File**: `src/cpp/kernel/cpu.cpp:486-488`.
- **Summary**: The `if (Index == 0)` check assumes the BSP's index is always 0, but `BspIndex` is the LAPIC APIC ID (`main.cpp:771`). On hardware where the BSP's APIC ID is non-zero, `ProcessTimers` never runs on any CPU → TCP retransmits, keyboard autorepeat, and Rust periodic timers silently stop firing.
- **Fix**: compare against `GetBspIndexLockHeld()` (or cache it in a lock-free variable).
- **Caveat**: On QEMU/KVM the BSP APIC ID is always 0, so latent.
- **Verified (2026-07-08)**: CONFIRMED. `Index` is genuinely the LAPIC APIC ID (`InsertCpu(lapicEntry->ApicId)`, `acpi.cpp:304`; `SendIPISelf` uses `Index` as the IPI destination). Nuance: if some AP happens to have APIC ID 0, timers run on that AP instead of the BSP (works, wrong core); the hard failure is when no CPU has APIC ID 0.
- **Fixed (2026-07-09)**: the check compares against a lock-free cached BSP index (`BspIndexCached`, mirrored by `SetBspIndex`).

### 76. [x] LAPIC `SendInit`/`SendStartup` race with the tick IPI on ICR — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/lapic.cpp:90-110`.
- **Summary**: `SendInit`/`SendStartup` write ICR-high then ICR-low **without disabling interrupts**, unlike `SendIPI` (`:112-118`) which does `InterruptDisable()` first (with a comment at `:114-116` explicitly warning that two senders must not interleave or the second clobbers IcrHigh). The PIT/HPET handler's `SendIPI` (via `SendIPIAll`) can fire between `SendInit`'s two writes and clobber ICR-high → INIT/SIPI sent to the wrong CPU → the AP never starts → `StartAll` times out → panic.
- **Fix**: `InterruptDisable()` around the two ICR writes, as in `SendIPI`.
- **Caveat**: Extremely small window (~20 ns vs 10 ms tick); effectively never on QEMU. Related to the (since-retracted) #46 but a distinct mechanism.
- **Verified (2026-07-08)**: PARTIAL — the missing `InterruptDisable` in `SendInit`/`SendStartup` is real, but the failure scenario is **not currently reachable**: both are called only inside `StartAll`'s `AutoLock(Lock)` scopes (`cpu.cpp:178-187`, `:200-210`), and `AutoLock` disables IRQs via `PreemptIrqSave`, so the tick cannot interrupt between the two ICR writes (the ICR is per-CPU; other CPUs write their own). Latent fragility only — any future call site outside a lock would be exposed.
- **Fixed (2026-07-09)**: both functions disable IRQs around the ICR write pair, like `SendIPI`.

### 77. [x] MSI-X: 64-bit BAR size probe writes only the low 32 bits — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/msix.cpp:94-96`.
- **Summary**: BAR size probing writes `0xFFFFFFFF` to only the low register. For a 64-bit BAR the PCI spec requires writing both halves; with only the low half probed, `barSize` computes as 1 for any BAR ≥ 4 GB and the table mapping is rejected.
- **Fix**: write `0xFFFFFFFF` to both `bar` and `bar+1`, compute `barSize` from the combined 64-bit mask.
- **Caveat**: MSI-X table BARs are small; never triggered in practice.
- **Verified (2026-07-08)**: CONFIRMED, one correction: for a ≥4 GB 64-bit BAR the computed `barSize` is 16, not 1 — same outcome (table mapping wrongly rejected).
- **Fixed (2026-07-09)**: both halves of a 64-bit BAR are size-probed (and the high half restored); the size comes from the combined 64-bit mask.

### 78. [x] IOAPIC: `SetEntry` writes the low redirection register before the high — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/ioapic.cpp:64-68`.
- **Summary**: Low (vector/mask/trigger) is written first, then high (destination). When unmasking an already-asserted level interrupt, there is a brief window where the entry is unmasked with the old (default BSP) destination → a spurious interrupt is delivered to APIC ID 0.
- **Fix**: write the high register first, then the low; or mask before modifying and unmask after.
- **Verified (2026-07-08)**: CONFIRMED (`ioapic.cpp:64-68`; the stale destination is the masking-pass default from `Enable`).
- **Fixed (2026-07-09)**: the high (destination) register is written before the low (vector/mask) one.

### 79. [x] HPET: `Setup` doesn't check timer 0 periodic-mode capability — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/hpet.cpp:96-99`.
- **Summary**: Timer 0 is programmed with `TimerTypePer` (periodic) without checking `TN_PER_INT_CAP` (bit 4). If the timer lacks periodic support, the bit is ignored, the timer fires once, and the system tick stops.
- **Fix**: read the timer config; if periodic is unsupported, fall back to the PIT or one-shot with re-arming.
- **Caveat**: QEMU's timer 0 supports periodic mode.
- **Verified (2026-07-08)**: CONFIRMED (`TimerTypePer` written unconditionally; the timer config register is never read back).
- **Fixed (2026-07-09)**: `Setup` checks `TN_PER_INT_CAP` and falls back to the PIT when timer 0 lacks periodic mode.

### 80. [x] MSI-X: `EnableVector` doesn't clear the Function Mask bit — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/msix.cpp:246-248`.
- **Summary**: Enabling MSI-X sets `MsixControlEnable` (bit 15) but never clears Function Mask (bit 14). If bit 14 was left set by the BIOS/previous driver, all vectors stay masked after enable → no interrupts delivered.
- **Fix**: `mc = (mc | MsixControlEnable) & ~MsixControlFuncMask`.
- **Caveat**: Bit 14 is 0 after PCI reset.
- **Verified (2026-07-08)**: CONFIRMED (no Function-Mask constant even exists in msix.cpp; bit 14 is never touched).
- **Fixed (2026-07-09)**: the enable write also clears Function Mask (bit 14).

### 81. [x] Serial `Wait` and 8042 `ReadData` have no timeout — LOW (fixed in 7a88822)
- **Files**: `src/cpp/drivers/serial.cpp:58-72`; `src/cpp/drivers/8042.cpp:52-58` (also the ctor at `:25` and ISR at `:66`).
- **Summary**: Both poll a status bit with backoff but no maximum iteration count. A stuck transmitter (serial) or stuck 8042 output buffer hangs the kernel indefinitely (at boot for the 8042 ctor, or in the ISR).
- **Fix**: bounded iteration count; drop the char / break out when exceeded.
- **Caveat**: QEMU's serial and 8042 are always ready.
- **Verified (2026-07-08)**: CONFIRMED. Nuance: the 8042 `ReadData` loop is a drain loop (`while (status & 1)`), not wait-for-ready, but the unbounded-on-status-bit hang is real for both.
- **Fixed (2026-07-09)**: `Serial::Wait` gives up after 20 doubling rounds (~1M pauses total); `IO8042::ReadData` drains at most `MaxDrainIterations` (4096) bytes per call.

### 82. [x] ACPI: `ParseRsdp` doesn't validate the extended checksum for ACPI 2.0+ — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/acpi.cpp:48-64`.
- **Summary**: Only the first-part checksum is validated. For Revision ≥ 2 the `ExtendedChecksum` (covering the whole `RSDPDescriptor20`) is not checked, nor is `Revision` consulted. A corrupted ACPI 2.0 RSDP with a valid first-part checksum is accepted.
- **Fix**: for `Revision >= 2`, validate `ComputeSum(rsdp, sizeof(RSDPDescriptor20)) == 0`.
- **Verified (2026-07-08)**: CONFIRMED (`Revision` is read only for tracing; `ExtendedChecksum` never verified).
- **Fixed (2026-07-09)**: for `Revision >= 2` the extended checksum over the whole descriptor is validated.

### 83. [x] 8042: shift + number/symbol keys produce wrong characters — LOW (fixed in 7a88822)
- **File**: `src/cpp/drivers/8042.cpp:108-120`.
- **Summary**: `char c = map[code] ^ Mod;` with `Mod = 0x20` for shift only toggles case for letters (`'a'^0x20='A'`). For numbers/symbols it produces control chars (e.g. `'1'`(0x31)`^0x20 = 0x11` instead of `'!'`).
- **Failure scenario**: Shifted number/symbol keys produce garbage; only letters shift correctly.
- **Fix**: use a separate shifted scancode map (or special-case non-letter keys).
- **Verified (2026-07-08)**: CONFIRMED (`'1' ^ 0x20 = 0x11` (DC1); only letters toggle correctly).
- **Fixed (2026-07-09)**: a dedicated shifted scancode map replaces the XOR-0x20 hack; shifted digits/symbols now produce the right characters (`:`, `!`, `_`, `?` verified typeable in QEMU).

### 84. [x] boot: Multiboot2 tag parsing lacks several bounds checks — LOW (fixed in 49debda)
- **File**: `src/cpp/boot/grub.cpp:64-129`.
- **Summary**: Three defense-in-depth gaps in the same tag loop: (a) the loop terminates only on `Type == End` with no check that `tag` stays within `MbInfo->TotalSize` — a missing/clobbered End tag walks past the buffer; (b) the framebuffer (`:120-129`) and bootdev (`:71-77`) handlers cast and access fields without verifying `tag->Size >= sizeof(...)`; (c) the cmdline (`:96-107`) is passed to unbounded `StrLen`/`SnPrintf` without verifying a NUL within the tag. (The ACPI handler at `:46` does check `tag->Size` — the pattern is just inconsistent.)
- **Fix**: bound the loop by `TotalSize`; check `tag->Size >= sizeof(struct)` before field access; use bounded string copy for cmdline.
- **Caveat**: GRUB always produces well-formed tags; defense-in-depth (siblings of #42/#43).
- **Verified (2026-07-08)**: CONFIRMED, all three sub-claims plus the inconsistency (the ACPI handler does check `tag->Size` at `grub.cpp:46`).
- **Fixed (2026-07-09)**: the tag loop is bounded by `TotalSize`; bootdev/mmap/framebuffer handlers check `tag->Size >= sizeof(struct)`; the cmdline must be NUL-terminated within the tag or it is ignored.

### 85. [x] rust: `kernel_alloc` / `kernel_alloc_dma_pages` bridge gaps — LOW (fixed in 78d0960)
- **Files**: `src/cpp/kernel/rust_ffi.cpp:46-51` (`kernel_alloc`), `:344` (`kernel_alloc_dma_pages`).
- **Summary**: (a) `kernel_alloc(size, align)` validates `align` is a power of 2 and `<= 8` but then calls `Mm::Alloc(size, RustAllocTag)` **without passing `align`**; any future Rust type needing alignment > 8 (e.g. `#[repr(align(16))]`, SIMD) hits the `Panic` for `align > 8`. (b) `*actual_pages_out = 1UL << Log2(count)` ceiling-rounds to the next power of 2; if `AllocMapPages` allocates exactly `count` (non-power-of-2) pages, `DmaBuffer::len()` overestimates by up to a page → potential access to unmapped memory.
- **Fix**: pass `align` through (or over-allocate + align); set `*actual_pages_out = count` (or verify `AllocMapPages` also ceiling-rounds).
- **Caveat**: Latent — all current Rust allocations use alignment ≤ 8 and 1-page DMA buffers.
- **Verified (2026-07-08)**: PARTIAL — (a) `align` dropped: CONFIRMED (`Mm::Alloc` has no align parameter). (b) `actual_pages_out` overestimate: REFUTED — `Mm::AllocMapPages` (`page_allocator.cpp:247-258`) ceiling-rounds with the same `1UL << Log2(count)`, so the reported count exactly matches what was mapped; no unmapped-access risk. Only sub-claim (a) remains open.
- **Fixed (2026-07-09)**: sub-claim (a): `kernel_alloc` over-allocates for `align > 8` and stashes the original pointer just below the aligned address; `kernel_free` now receives `(ptr, size, align)` (Rust's `GlobalAlloc` passes the layout) and unwraps the stash. Sub-claim (b) was refuted on verification.

---

## Retracted

### 46 (retracted, not a bug): "PIT/HPET tick handler deadlocks on `CpuTable::Lock` / `Cpu::Lock`"
- **Files**: `src/cpp/drivers/pit.cpp:72`, `src/cpp/drivers/hpet.cpp:166`, `src/cpp/kernel/cpu.cpp`, `src/cpp/kernel/spin_lock.cpp:48-52`, `src/cpp/lib/lock.h:11`, `src/cpp/kernel/preempt.cpp:63-73`.
- **Status**: **Not a bug** — refuted on verification (2026-07-08). The claim rested on `Stdlib::AutoLock` invoking the no-flags `SpinLock::Lock()` (which skips IRQ-disable). It cannot: `LockInterface` (`lib/lock.h:11`) exposes only `Lock(ulong& flags)`, so every `AutoLock` resolves to `SpinLock::Lock(ulong&)` (`spin_lock.cpp:48-52`) → `PreemptIrqSave()` → `InterruptDisable()` (`preempt.cpp:71`). Every task-context holder of `CpuTable::Lock`/`Cpu::Lock` therefore runs with IRQs disabled (these locks are private and only acquired via `AutoLock`; the one direct-lock use in cpu.cpp, `IPITaskLock.LockIrqSave()` at `:498/:516`, also disables IRQs). The tick handler can never interrupt a same-CPU holder, so the claimed self-deadlock cannot occur; cross-CPU contention just spins briefly. The IRQ-context `SendIPIAll` calls are real but safe.

### 29 (retracted, not a bug): "LAPIC DFR written with reserved bits set"
- **File**: `src/cpp/drivers/lapic.cpp:51`.
- **Status**: **Not a bug** — refuted on verification (2026-07-08). `0xFFFFFFFF` is the standard flat-mode DFR value: the DFR's bits [27:0] reset to all-1s and writing 1s there is the universal idiom (Linux defines `APIC_DFR_FLAT = 0xFFFFFFFFul`); no #GP results. Worse, the originally proposed fix `0x0FFFFFFF` sets model bits [31:28] to 0 — that selects *cluster* mode (`APIC_DFR_CLUSTER`) and would break the intended flat-mode IPI delivery. The current code is the correct idiom.

### mm (retracted, not a bug): "FreePage before Put" race
- **File**: `src/cpp/mm/page_allocator.cpp:68-70`.
- **Status**: **Not a bug.** The `UnmapPage → FreePage → Put` order at these lines is **exactly the order CLAUDE.md documents** as the convention ("To free a mapped page: UnmapPage + FreePage + Put"). It is not a deviation. The subagent's race scenario was speculative and self-admittedly "masked by mm #26"; tracing the refcounts, the `Put` only releases `MapPage`'s +1 and never drives the count to 0. Retracted after direct verification.

---

## Summary

- **Boot** and **mm**/**lib** are largely clean — boot had zero real bugs; mm and lib only low-severity items.
- The serious, fix-first cluster is **SMP/interrupt-safety in the I/O paths**: #1 (virtio_blk/scsi `SlotByHead`) and #2 (virtio_net RX swap) — both the same shape: bookkeeping done outside the lock that protects the underlying ring/array. (#3, the TimerTable race, was downgraded to Medium on verification: the race is real but the UAF is unreachable with current callers.)
- The other two high-impact, independently-triggerable bugs are #4 (TCP SYN-RECEIVED leak) and #5 (NanoFs overwrite ordering).
- Of ~31 first-pass findings, 29 are confirmed real; 2 are retracted as false positives (mm "FreePage before Put", #29 LAPIC DFR — see Retracted).

**Deep second pass (#44-#85).** A second, deeper review across every subsystem surfaced 42 more findings (41 confirmed on verification; #46 refuted). The new fix-first items: **#44** (r8168 RX is never dispatched — `r8168_process_rx` is dead code because the `TypeNetRx` softirq only iterates `VirtioNet::Instances`, not `NetDeviceTable`) and **#45** (TCP FIN retransmit has no retry limit — the FIN-state analogue of #4). (**#46**, the claimed PIT/HPET tick-handler deadlock, was refuted on verification and moved to Retracted — `AutoLock` always disables IRQs via `PreemptIrqSave`.) The pass also found a recurring **missing-recursion-depth-limit** pattern in filesystem mount paths (#49, #50, #51 — NanoFs `LoadVNode` / `RemoveRecursive` and ext2 `LoadDir` → stack overflow on a crafted image), the **NanoFs `RemoveDirEntry` OOB** sibling of #13 (#48), a **`VsnPrintf` no-NUL-termination** deviation (#52), and several missing TCP timers (#53 FIN-WAIT-2, #54 persist). Two areas came back clean on re-review: the scheduler/context-switch/lock-internals paths, and the boot/AP-trampoline assembly.

## Status

- **Total open issues**: 0
- **Fixed**: 83 — Critical/High: #1, #2, #4, #5, #44, #45 (11c608e); Medium: #3, #6, #7, #8, #9, #10, #11, #12, #13, #14, #15, #17, #47, #48, #49, #50, #51, #52, #53, #54, #55, #56, #57, #16 (dc9d947); Low: #23 (via #50), #26/#27/#34/#35/#42/#43/#70/#71/#72/#84 (49debda), #28/#30–#33/#68/#69/#73–#83 (7a88822), #18–#22/#24/#25/#40/#41/#58–#67 (d037195), #36–#39/#85 (78d0960)
- **Retracted (not bugs)**: 3 (mm "FreePage before Put", #46 PIT/HPET tick-handler deadlock, #29 LAPIC DFR flat-mode value)

**Fix pass (2026-07-09, commit 11c608e)**: all six Critical/High issues fixed. Validated by a full Docker build (`make nocheck` + `make check`/cppcheck clean) and a headless QEMU boot: self-tests pass, DHCP acquires a lease (RX through the new `NetDeviceTable` softirq dispatch + descriptor map), `wget example.com/` completes an HTTP GET over TCP (`tcpstat` shows the connection slot reclaimed), and nanofs format/mount/write/cat/overwrite round-trips on a virtio-blk disk.

**Medium fix pass (2026-07-09)**: 23 of the 24 Medium issues fixed (all but #16, which was resolved as reviewed-no-change), prioritized by real-world impact: TCP protocol correctness first (#8 window updates, #54 persist timer, #53 FIN-WAIT-2 timer, #6/#7 half-close data handling), then SMP/data-integrity (#3 timer locking, #14 NanoFs durability, #12 NVMe shutdown, #57 ISR unregister sync, #56 run-on-exited-CPU hang), then crafted-image/robustness hardening (#13, #15, #48, #49, #50, #51, plus lib fixes #10, #11, #52 and #17 virtqueue size), and finally #9 (per-CPU kvmclock), #47 (low-RAM reclaim, ~8-20 MB back to the allocator), #55 (HTTP chunked decoding).

**Final pass (2026-07-09, commit dc9d947)**: #16, previously resolved as reviewed-no-change, is now closed for real — not by touching the (mandatory) EOI but by enforcing exclusive GSI/vector ownership across `Interrupt::Register`/`RegisterLevel`, which eliminates the only path by which another device could silently share the blk stub's line. Validated by Docker build + cppcheck + a QEMU boot with virtio-blk (modern) and virtio-scsi (legacy INTx, GSI 0xB → vector 0x35): all registrations clean, no refusals, keyboard/disk/network all functional. **Every issue in this file is now closed.**

**Low fix pass (2026-07-09, commits 49debda/7a88822/d037195/78d0960)**: all 52 remaining Low issues fixed (#27 by documenting the boot-ordering constraint, per the issue's own fix suggestion). Notable behavior changes: TIME-WAIT is now 60 s (#59), NanoFs commits allocation bitmaps after content writes with a single FUA instead of one per AllocDataBlock (#22), the Rust `kernel_free` FFI now takes `(ptr, size, align)` (#85), and shifted digits/symbols finally type correctly in the shell (#83). Validated by a full Docker build (`make rust` + `make nocheck` + `make check`/cppcheck clean) and headless QEMU boots on both the plain-ISO and blk/scsi/nvme/rng-attached configurations: all self-tests pass, DHCP + DNS + `wget example.com/` work (`tcpstat` conns=0 after), `HI: Test_1!?` types correctly (#83), NanoFs format/mkdir/write/cat/overwrite round-trips, the file survives a guest reboot (#22 ordering), `ls /mnt/dir1/..` resolves (#64), and `random 8` returns entropy from the polled virtio-rng (#69).

To mark an issue fixed, change its `- [ ]` to `- [x]` and optionally append a commit/PR reference on the same line, e.g.:

```
### 4. [x] TCP: SYN-RECEIVED half-open connection pool leak — HIGH  (fixed in abc1234)
```

When you do, also bump the **Fixed** count above and decrement **Total open issues**.
