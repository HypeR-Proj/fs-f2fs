// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// HypeR platform adapter for the pinned media core.
#pragma once
#include "src/storage/f2fs/common.h"
namespace f2fs {
class VmoManager {
  struct Buffer {
    std::unique_ptr<uint8_t[]> data = std::make_unique<uint8_t[]>(kBlockSize);
  };
  std::mutex mutex_;
  std::map<pgoff_t, Buffer> pages_;
  uint64_t size_, checkpointed_;

 public:
  explicit VmoManager(uint64_t n) : size_(n), checkpointed_(n) {}
  bool IsPaged() const { return false; }
  zx::result<bool> CreateAndLockVmo(pgoff_t n, void** out = nullptr) {
    std::lock_guard l(mutex_);
    auto [it, inserted] = pages_.try_emplace(n);
    if (out) *out = it->second.data.get();
    return zx::ok(!inserted);
  }
  zx_status_t UnlockVmo(pgoff_t n) {
    std::lock_guard l(mutex_);
    pages_.erase(n);
    return ZX_OK;
  }
  void ZeroBlocks(PlatformVfs&, pgoff_t a, pgoff_t b) {
    std::lock_guard l(mutex_);
    for (auto i = pages_.lower_bound(a); i != pages_.end() && i->first < b; ++i)
      memset(i->second.data.get(), 0, kBlockSize);
  }
  zx::result<> WritebackBegin(PlatformVfs&, size_t, size_t) { return zx::ok(); }
  zx_status_t WritebackEnd(PlatformVfs&, size_t, size_t) { return ZX_OK; }
  zx::result<> DirtyPages(PlatformVfs&, size_t, size_t) { return zx::error(ZX_ERR_NOT_SUPPORTED); }
  void AllowEviction(PlatformVfs&) {}
  void Reset(bool = false) {
    std::lock_guard l(mutex_);
    pages_.clear();
  }
  void SetContentSize(size_t n) {
    std::lock_guard l(mutex_);
    size_ = n;
  }
  uint64_t GetContentSize(bool checkpoint = false, bool round = false) {
    std::lock_guard l(mutex_);
    if (checkpoint) checkpointed_ = size_;
    return round ? fbl::round_up(size_, uint64_t(kBlockSize)) : size_;
  }
  bool SizeIsChanged() {
    std::lock_guard l(mutex_);
    return size_ != checkpointed_;
  }
  zx_status_t Read(void*, uint64_t, size_t) { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t Write(const void*, uint64_t, size_t) { return ZX_ERR_NOT_SUPPORTED; }
};
}  // namespace f2fs
