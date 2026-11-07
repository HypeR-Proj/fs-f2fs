// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

// Host-only access to the real core for checkpoint and roll-forward fixtures.
// These entry points are absent from the Native worker and its FFI header.
#include "src/storage/f2fs/segment.h"
#include "src/storage/f2fs/node.h"
#include "src/storage/f2fs/node_page.h"
#include "src/storage/f2fs/writeback.h"
#include <chrono>
#include <thread>

extern "C" uint64_t hyper_f2fs_test_state(void *context, unsigned field) {
  auto &fs = *static_cast<Mounted *>(context)->fs;
  auto &sb = fs.GetSuperblockInfo();
  switch (field) {
    case 0: return fs.GetSegmentManager().NextFreeBlkAddr(f2fs::CursegType::kCursegWarmNode);
    case 1: return sb.GetCheckpointVer();
    case 2: return f2fs::LeToCpu(sb.GetSuperblock().cp_blkaddr);
    case 3: return sb.GetBlocksPerSeg();
    case 4: return f2fs::LeToCpu(sb.GetSuperblock().nat_blkaddr);
    case 5: return sb.GetPageCount(f2fs::CountType::kDirtyData);
    case 6: return sb.GetPageCount(f2fs::CountType::kWriteback);
    default: return 0;
  }
}

extern "C" int32_t hyper_f2fs_test_node(void *context, uint32_t nid, bool batch) {
  auto &nodes = static_cast<Mounted *>(context)->fs->GetNodeManager();
  if (batch) return nodes.GetNodePages({nid}).status_value();
  f2fs::LockedPage page;
  return nodes.GetNodePage(nid, &page);
}

// Exercise address-allocation failure after the page has entered writeback.
extern "C" int32_t hyper_f2fs_test_writeback_nat(void *context, uint32_t nid) {
  auto &fs = *static_cast<Mounted *>(context)->fs;
  std::lock_guard lock(f2fs::GetGlobalLock());
  auto &nodes = fs.GetNodeVnode();
  f2fs::LockedPage page;
  if (auto status = nodes.GrabLockedPage(nid, &page); status != ZX_OK) return status;
  page.GetPage<f2fs::NodePage>().FillNodeFooter(nid, nid, 0);
  page.SetDirty();
  if (nodes.GetBlockAddr(page) != f2fs::kNullAddr || !page->IsDirty()) return ZX_ERR_BAD_STATE;
  return fs.GetSuperblockInfo().TestCpFlags(f2fs::CpFlag::kCpErrorFlag) ? ZX_ERR_IO : ZX_ERR_BAD_STATE;
}

extern "C" int32_t hyper_f2fs_test_fsync(void *context, const char *path, size_t length) {
  return hyper_f2fs_sync_file(context, path, length, 0);
}

extern "C" void hyper_f2fs_test_pending_writer(void *context) {
  auto &fs = *static_cast<Mounted *>(context)->fs;
  fs.GetWriter().ScheduleWriteback(fpromise::make_promise([&fs] {
    // Destruction must join this producer before withdrawing GetWriter().
    while (!fs.IsTearDown()) std::this_thread::yield();
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
    fs.GetWriter().Sync();
    return fpromise::ok();
  }));
}

extern "C" int64_t hyper_f2fs_test_cached_pages(void *context, const char *path, size_t length) {
  auto found = Lookup(*static_cast<Mounted *>(context), {path, length});
  if (found.is_error()) return found.error_value();
  return static_cast<int64_t>((*found)->GetFileCache().GetSize());
}

// Pause the exact dirty -> local writeback-list transition. These synchronization
// hooks and the deliberately split preparation helpers are absent from Native.
namespace {
std::mutex preparation_test_mutex;
std::condition_variable preparation_test_cv;
bool preparation_test_armed = false;
bool preparation_test_paused = false;
bool preparation_test_entered = false;
}
extern "C" void hyper_f2fs_test_pause_preparation(bool enabled) {
  {
    std::lock_guard lock(preparation_test_mutex);
    preparation_test_armed = enabled;
    preparation_test_paused = enabled;
    if (enabled) preparation_test_entered = false;
  }
  preparation_test_cv.notify_all();
}
extern "C" bool hyper_f2fs_test_wait_prepared() {
  std::unique_lock lock(preparation_test_mutex);
  return preparation_test_cv.wait_for(lock, std::chrono::seconds(5), [] {
    return preparation_test_entered;
  });
}
extern "C" void hyper_f2fs_test_prepared_writeback() {
  std::unique_lock lock(preparation_test_mutex);
  if (!preparation_test_armed) return;
  preparation_test_armed = false;
  preparation_test_entered = true;
  preparation_test_cv.notify_all();
  preparation_test_cv.wait(lock, [] { return !preparation_test_paused; });
}
extern "C" int32_t hyper_f2fs_test_dirty(void* context, const char* path, size_t length,
                                        const void* data, size_t bytes) {
  auto found = Lookup(*static_cast<Mounted*>(context), {path, length});
  if (found.is_error()) return found.error_value();
  if (!S_ISREG((*found)->GetMode())) return ZX_ERR_NOT_FILE;
  size_t actual = 0;
  auto status = static_cast<f2fs::File*>((*found).get())->WriteNative(data, bytes, 0, &actual);
  return status != ZX_OK ? status : actual == bytes ? ZX_OK : ZX_ERR_IO;
}
extern "C" int32_t hyper_f2fs_test_snapshot(void* context, const char* path, size_t length,
                                           void** output) {
  *output = nullptr;
  auto receipt = std::unique_ptr<NativeWriteReceipt>(new (std::nothrow) NativeWriteReceipt);
  if (!receipt) return ZX_ERR_NO_MEMORY;
  auto found = Lookup(*static_cast<Mounted*>(context), {path, length});
  if (found.is_error()) return found.error_value();
  fs::SharedLock global(f2fs::GetGlobalLock());
  f2fs::WritebackOperation operation;
  (*found)->Writeback(operation, &receipt->result);
  *output = receipt.release();
  return ZX_OK;
}
