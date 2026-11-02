// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// HypeR platform adapter for the pinned media core.
#pragma once
namespace f2fs {
enum class MemoryPressure { kUnknown, kLow, kMedium, kHigh };
enum class MemoryStatus { kNeedReclaim, kNeedCheckpoint, kNeedWriteback };
class MemoryPressureWatcher {};
}  // namespace f2fs
