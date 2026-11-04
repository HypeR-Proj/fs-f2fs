// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

// Host-only access to the real core for checkpoint and roll-forward fixtures.
// These entry points are absent from the Native worker and its FFI header.
#include "src/storage/f2fs/segment.h"
#include "src/storage/f2fs/node.h"
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
    default: return 0;
  }
}

extern "C" int32_t hyper_f2fs_test_node(void *context, uint32_t nid, bool batch) {
  auto &nodes = static_cast<Mounted *>(context)->fs->GetNodeManager();
  if (batch) return nodes.GetNodePages({nid}).status_value();
  f2fs::LockedPage page;
  return nodes.GetNodePage(nid, &page);
}

extern "C" int32_t hyper_f2fs_test_fsync(void *context, const char *path, size_t length) {
  auto &mount = *static_cast<Mounted *>(context);
  auto vnode = Lookup(mount, std::string_view(path, length));
  if (vnode.is_error()) return vnode.error_value();
  auto status = (*vnode)->SyncFile(false);
  mount.fs->GetWriter().Sync();
  if (mount.fs->GetSuperblockInfo().TestCpFlags(f2fs::CpFlag::kCpErrorFlag)) return ZX_ERR_IO;
  return status;
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
