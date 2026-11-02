// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// HypeR platform adapter for the pinned media core.
#pragma once
#include "portability.h"
#include "src/storage/lib/buffer/block_buffer.h"
namespace storage {
class VmoBuffer : public BlockBuffer {
  std::unique_ptr<uint8_t[]> bytes_;
  size_t blocks_ = 0;
  uint32_t size_ = 0;

 public:
  template <class D>
  zx_status_t Initialize(D*, size_t n, uint32_t size, const char*) {
    blocks_ = n;
    size_ = size;
    bytes_ = std::make_unique<uint8_t[]>(n * size);
    return ZX_OK;
  }
  void* Data(size_t i) override { return bytes_.get() + i * size_; }
  const void* Data(size_t i) const override { return bytes_.get() + i * size_; }
  size_t capacity() const override { return blocks_; }
  uint32_t BlockSize() const override { return size_; }
};
}  // namespace storage
