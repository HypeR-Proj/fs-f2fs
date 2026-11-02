// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// HypeR platform adapter for the pinned media core.
#pragma once
#include "bridge.h"
#include "src/storage/f2fs/common.h"
#include "src/storage/lib/operation/operation.h"
namespace fuchsia_storage_block::wire {
struct BlockInfo {
  uint32_t block_size;
  uint64_t block_count;
};
}  // namespace fuchsia_storage_block::wire
namespace f2fs {
using DeviceCallbacks = HyperF2fsDevice;
class BcacheMapper {
 public:
  explicit BcacheMapper(DeviceCallbacks callbacks) : cb_(callbacks) {}
  bool IsWritable() const { return true; }
  uint64_t Maxblk() const { return cb_.blocks; }
  uint32_t BlockSize() const { return 4096; }
  const uint8_t* Uuid() const { return cb_.uuid; }
  zx_status_t Readblk(block_t n, void* data) {
    return n >= Maxblk() ? ZX_ERR_OUT_OF_RANGE
                         : cb_.read(cb_.context, uint64_t(n) * 4096, data, 4096);
  }
  zx_status_t Writeblk(block_t n, const void* data) {
    return n >= Maxblk() ? ZX_ERR_OUT_OF_RANGE
                         : cb_.write(cb_.context, uint64_t(n) * 4096, data, 4096);
  }
  zx_status_t Flush() { return cb_.flush(cb_.context); }
  zx_status_t Trim(size_t, size_t) { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t BlockGetInfo(fuchsia_storage_block::wire::BlockInfo* out) const {
    out->block_size = 4096;
    out->block_count = Maxblk();
    return ZX_OK;
  }

  zx_status_t RunRequests(const std::vector<storage::BufferedOperation>& ops) {
    std::vector<HyperF2fsIo> requests;
    requests.reserve(ops.size());
    for (const auto& b : ops) {
      const auto& op = b.op;
      if (op.dev_offset > Maxblk() || op.length > Maxblk() - op.dev_offset)
        return ZX_ERR_OUT_OF_RANGE;
      uint32_t kind;
      switch (op.type) {
        case storage::OperationType::kRead:
          kind = 0;
          break;
        case storage::OperationType::kWrite:
          kind = 1;
          break;
        case storage::OperationType::kWriteFua:
          kind = 2;
          break;
        default:
          return ZX_ERR_NOT_SUPPORTED;
      }
      requests.push_back({op.dev_offset * 4096,
                          static_cast<uint8_t*>(b.data) + op.vmo_offset * 4096, op.length * 4096,
                          kind});
    }
    return cb_.run(cb_.context, requests.data(), requests.size());
  }

 private:
  DeviceCallbacks cb_;
};
}  // namespace f2fs
